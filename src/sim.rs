//! A simulated strap, for working without hardware.
//!
//! # Why it builds frames
//!
//! The obvious way to write this would be to push [`WhoopEvent`]s straight into
//! a channel. That would test nothing: it would exercise a second
//! implementation of the protocol living next to the real one, and the two
//! would drift apart exactly where the real one was wrong.
//!
//! So this builds **complete frames with correct checksums** and hands them to
//! [`crate::whoop_client::decode_frame`] — the same function the live dispatch
//! loop calls. Everything downstream of the radio is the real thing: framing,
//! verification, layout selection, field offsets, the unit conversions. A
//! decoder bug shows up in simulation, which is the entire point of having one.
//!
//! # It also checks the client
//!
//! The simulated offload will not advance its cursor unless the acknowledgement
//! echoes the **exact** eight-byte token it sent. A client that rebuilt the
//! token from its decoded trim cursor — the specific mistake
//! [`crate::history`] exists to prevent — gets its chunk resent instead of
//! advanced, and the reason is logged. So this is a conformance check on the
//! safe-trim path, not just a data source.
//!
//! ```no_run
//! use poop::prelude::*;
//! use poop::sim;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let (mut events, strap) = sim::connect(sim::SimConfig::default());
//! println!("battery: {:.0}%", strap.battery().await?);
//! while let Some(event) = events.recv().await {
//!     if let WhoopEvent::HeartRate(hr) = event {
//!         println!("{} bpm", hr.bpm);
//!     }
//! }
//! # Ok(()) }
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::{info, warn};
use tokio::sync::{mpsc, oneshot};

use crate::history::AckPayload;
use crate::platform;
use crate::protocol::{
    crc16_modbus, crc32, crc8, event, layout, packet, Command, DeviceFamily, Whoop5Variant,
};
use crate::types::WhoopEvent;
use crate::whoop_client::decode_frame;

// ── Frame construction ───────────────────────────────────────────────────────

/// Assembles one frame field by field, at **absolute frame offsets**.
///
/// Offsets are absolute because that is how every documented layout is written.
/// A builder taking payload-relative offsets would make every call site do the
/// arithmetic the decoders deliberately do not.
///
/// Public because a decoder test needs exactly this, and the test helpers that
/// grew up alongside it in `parse` were the same code with different names.
#[derive(Debug)]
pub struct FrameBuilder {
    bytes: Vec<u8>,
    family: DeviceFamily,
}

impl FrameBuilder {
    /// A zero-filled frame of `total` bytes with its envelope stamped.
    ///
    /// `b1` and `b2` are the two inner bytes after the packet type — sequence
    /// and command on a command frame, layout and marker on a WHOOP 5 record,
    /// version and subtype on a WHOOP 4 one.
    pub fn new(family: DeviceFamily, ptype: u8, b1: u8, b2: u8, total: usize) -> Self {
        let mut bytes = vec![0u8; total];
        bytes[0] = 0xAA;
        match family {
            DeviceFamily::Whoop4 => {
                let declared = (total - 4) as u16;
                bytes[1..3].copy_from_slice(&declared.to_le_bytes());
                bytes[3] = crc8(&declared.to_le_bytes());
                bytes[4] = ptype;
                bytes[5] = b1;
                bytes[6] = b2;
            }
            DeviceFamily::Whoop5 => {
                // The inner record is padded to four bytes, so a complete frame
                // is always a multiple of four. Asserting it here turns a
                // mis-sized simulated record into a panic in a test rather than
                // a rejected frame nobody can explain.
                debug_assert_eq!(total % 4, 0, "a WHOOP 5 frame is a multiple of four bytes");
                let declared = (total - 8) as u16;
                bytes[1] = 0x01;
                bytes[2..4].copy_from_slice(&declared.to_le_bytes());
                bytes[4..6].copy_from_slice(&[0x01, 0x00]);
                let header_crc = crc16_modbus(&bytes[0..6]).to_le_bytes();
                bytes[6..8].copy_from_slice(&header_crc);
                bytes[8] = ptype;
                bytes[9] = b1;
                bytes[10] = b2;
            }
        }
        Self { bytes, family }
    }

    /// Write one byte.
    pub fn u8(&mut self, off: usize, value: u8) -> &mut Self {
        self.bytes[off] = value;
        self
    }

    /// Write a little-endian `u16`.
    pub fn u16(&mut self, off: usize, value: u16) -> &mut Self {
        self.bytes[off..off + 2].copy_from_slice(&value.to_le_bytes());
        self
    }

    /// Write a little-endian `u32`.
    pub fn u32(&mut self, off: usize, value: u32) -> &mut Self {
        self.bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        self
    }

    /// Write a little-endian IEEE-754 `f32`.
    pub fn f32(&mut self, off: usize, value: f32) -> &mut Self {
        self.bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        self
    }

    /// Write a byte slice.
    pub fn bytes_at(&mut self, off: usize, value: &[u8]) -> &mut Self {
        self.bytes[off..off + value.len()].copy_from_slice(value);
        self
    }

    /// Seal the payload checksum and take the frame.
    ///
    /// The result passes [`crate::protocol::verify_frame`] by construction,
    /// which is what makes it usable as a decoder-test fixture: a fixture that
    /// did not verify would be testing the decoder against a frame no strap
    /// could send.
    pub fn finish(mut self) -> Vec<u8> {
        let start = self.family.inner_offset();
        let end = self.bytes.len() - 4;
        let crc = crc32(&self.bytes[start..end]).to_le_bytes();
        self.bytes[end..].copy_from_slice(&crc);
        self.bytes
    }
}

// ── Deterministic noise ──────────────────────────────────────────────────────

/// A tiny xorshift, so a run is reproducible and the crate needs no RNG.
///
/// Reproducibility is the point: a simulated session that behaves differently
/// every time cannot be used to chase a decoder bug.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `-range..=range`.
    fn spread(&mut self, range: i64) -> i64 {
        if range == 0 {
            return 0;
        }
        (self.next() % (range as u64 * 2 + 1)) as i64 - range
    }
}

// ── Configuration ────────────────────────────────────────────────────────────

/// How the simulated strap behaves.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Which wire profile to speak. Both are fully simulated, and they are
    /// genuinely different frames — not one shape with a flag on it.
    pub family: DeviceFamily,
    /// The advertised name.
    pub name: String,
    /// Battery percentage reported by command 26 and the pushed event.
    pub battery_percent: f32,
    /// Resting heart rate the live stream oscillates around.
    pub resting_hr: u8,
    /// How many records the simulated ring buffer holds.
    pub stored_records: u32,
    /// Records per chunk before an end marker.
    ///
    /// Defaults to 50, matching the roughly-every-50-records cadence a
    /// high-frequency offload uses, so the repeated-end-marker behaviour the
    /// real session has to cope with actually happens here.
    pub chunk_records: u32,
    /// Interval between live packets.
    pub tick: Duration,
    /// Seconds between consecutive historical records.
    pub record_interval_secs: u32,
    /// Seed for the deterministic noise.
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            family: DeviceFamily::Whoop5,
            name: "WHOOP SIM".into(),
            battery_percent: 62.0,
            resting_hr: 54,
            stored_records: 180,
            chunk_records: 50,
            tick: Duration::from_millis(1000),
            record_interval_secs: 6,
            seed: 0x5EED_1234_ABCD_0001,
        }
    }
}

// ── Handle ───────────────────────────────────────────────────────────────────

/// What the handle asks the simulated strap to do.
enum SimRequest {
    Battery(oneshot::Sender<f32>),
    BeginHistory,
    AckHistory(Vec<u8>),
    AbortHistory,
    ToggleRealtimeHr(bool),
    Buzz,
    Disconnect,
}

/// A handle to a simulated strap.
///
/// Mirrors the subset of [`crate::whoop_client::WhoopHandle`] a front end needs,
/// so [`crate::strap::Strap`] can present the two interchangeably.
pub struct SimHandle {
    requests: mpsc::Sender<SimRequest>,
    family: DeviceFamily,
    connected: Arc<AtomicBool>,
}

impl std::fmt::Debug for SimHandle {
    /// Identity without innards: the request channel has nothing printable in
    /// it, and a handle in someone's `#[derive(Debug)]` struct should still
    /// compile.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimHandle")
            .field("family", &self.family)
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl SimHandle {
    /// Which wire profile this simulated strap speaks.
    pub fn family(&self) -> DeviceFamily {
        self.family
    }

    /// Always [`Whoop5Variant::Unknown`].
    ///
    /// Simulating an MG would mean asserting ECG hardware this has no waveform
    /// for, and "unknown" is the honest answer for a strap with no Device
    /// Information Service at all.
    pub fn variant(&self) -> Whoop5Variant {
        Whoop5Variant::Unknown
    }

    /// Read the simulated battery.
    ///
    /// Goes through a real command-response frame and the real decoder, so the
    /// per-family body width is exercised rather than asserted.
    pub async fn battery(&self) -> Result<f32> {
        let (tx, rx) = oneshot::channel();
        self.requests
            .send(SimRequest::Battery(tx))
            .await
            .map_err(|_| anyhow!("the simulated strap has stopped"))?;
        rx.await
            .map_err(|_| anyhow!("the simulated strap dropped the request"))
    }

    /// Start the simulated offload.
    pub async fn begin_history(&self) -> Result<()> {
        self.send(SimRequest::BeginHistory).await
    }

    /// Acknowledge a committed chunk.
    ///
    /// The simulated strap compares these bytes against the token it sent and
    /// **will not advance** unless they match exactly.
    pub async fn ack_history(&self, ack: &AckPayload) -> Result<()> {
        self.send(SimRequest::AckHistory(ack.body().to_vec())).await
    }

    /// Stop the simulated offload.
    pub async fn abort_history(&self) -> Result<()> {
        self.send(SimRequest::AbortHistory).await
    }

    /// Start or stop the simulated live heart-rate stream.
    pub async fn toggle_realtime_hr(&self, on: bool) -> Result<()> {
        self.send(SimRequest::ToggleRealtimeHr(on)).await
    }

    /// Buzz — logged, since there is no motor.
    pub async fn buzz(&self, _loops: u8) -> Result<()> {
        self.send(SimRequest::Buzz).await
    }

    /// Whether the simulated link is still up.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Drop the simulated link.
    pub async fn disconnect(&self) -> Result<()> {
        self.send(SimRequest::Disconnect).await
    }

    async fn send(&self, request: SimRequest) -> Result<()> {
        self.requests
            .send(request)
            .await
            .map_err(|_| anyhow!("the simulated strap has stopped"))
    }
}

// ── The strap ────────────────────────────────────────────────────────────────

/// Where the simulated offload has got to.
enum Offload {
    Idle,
    /// Streaming records from `cursor`, with `in_chunk` emitted since the last
    /// end marker.
    Streaming {
        cursor: u32,
        in_chunk: u32,
    },
    /// An end marker is out and the strap is waiting to be told the chunk
    /// landed. `token` is what the client has to echo back.
    AwaitingAck {
        cursor: u32,
        token: [u8; 8],
        /// How many times this end marker has been resent.
        ///
        /// The real strap resends up to four times and abandons the attempt on
        /// the fifth, so a client that never acknowledges sees the transfer end
        /// rather than hanging.
        resends: u8,
    },
    Complete,
}

/// Connect to a simulated strap.
///
/// Returns the same pair [`crate::whoop_client::WhoopClient::connect`] does, so
/// a caller can swap one for the other.
pub fn connect(config: SimConfig) -> (mpsc::Receiver<WhoopEvent>, SimHandle) {
    let (events_tx, events_rx) = mpsc::channel::<WhoopEvent>(1024);
    let (requests_tx, requests_rx) = mpsc::channel::<SimRequest>(16);
    let connected = Arc::new(AtomicBool::new(true));

    let handle = SimHandle {
        requests: requests_tx,
        family: config.family,
        connected: Arc::clone(&connected),
    };

    platform::spawn(run(config, events_tx, requests_rx, connected));
    (events_rx, handle)
}

async fn run(
    config: SimConfig,
    events: mpsc::Sender<WhoopEvent>,
    mut requests: mpsc::Receiver<SimRequest>,
    connected: Arc<AtomicBool>,
) {
    let family = config.family;
    let mut noise = Noise(config.seed | 1);
    let mut offload = Offload::Idle;
    let mut realtime_hr = true;
    let mut phase = 0u32;
    // Simulated history runs backwards from "now" so the newest record is the
    // most recent, as a real ring buffer's would be.
    let base_time = 1_760_000_000u32;
    let oldest = base_time - config.stored_records * config.record_interval_secs;

    let _ = events
        .send(WhoopEvent::Connected {
            name: config.name.clone(),
            family,
        })
        .await;

    // One deliver closure, so every event in this module goes through the real
    // decoder. Nothing here constructs a `WhoopEvent` directly except the
    // connect and disconnect notices, which have no frame.
    macro_rules! deliver {
        ($frame:expr) => {{
            for decoded in decode_frame(&$frame, family) {
                if events.send(decoded).await.is_err() {
                    return;
                }
            }
        }};
    }

    // One wait, not a `select!`: a bounded wait on the request channel gives
    // both arms at once — a request arrives, or the deadline is the tick. That
    // keeps `tokio`'s `macros` feature out of the portable dependency set, and
    // `macros` is native-only here for the reason in `platform.rs`.
    loop {
        match platform::timeout(config.tick, requests.recv()).await {
            // ── A request ───────────────────────────────────────────────────
            Ok(Some(request)) => match request {
                SimRequest::Battery(reply) => {
                    let frame = battery_response(family, config.battery_percent);
                    deliver!(frame);
                    let _ = reply.send(config.battery_percent);
                }
                SimRequest::BeginHistory => {
                    info!(
                        "sim: offload requested, {} record(s) stored",
                        config.stored_records
                    );
                    offload = Offload::Streaming {
                        cursor: 0,
                        in_chunk: 0,
                    };
                    deliver!(metadata(family, 1, 0, [0u8; 8]));
                }
                SimRequest::AckHistory(body) => {
                    offload = acknowledge(offload, &body, &config);
                }
                SimRequest::AbortHistory => {
                    info!("sim: offload aborted");
                    offload = Offload::Idle;
                }
                SimRequest::ToggleRealtimeHr(on) => {
                    realtime_hr = on;
                    let number = if on {
                        event::BLE_REALTIME_HR_ON
                    } else {
                        event::BLE_REALTIME_HR_OFF
                    };
                    deliver!(strap_event(family, number, base_time));
                }
                SimRequest::Buzz => info!("sim: buzz (no motor attached)"),
                SimRequest::Disconnect => break,
            },

            // The handle was dropped: nothing can ask for anything again.
            Ok(None) => break,

            // ── A tick ──────────────────────────────────────────────────────
            Err(_) => {
                phase = phase.wrapping_add(1);

                if realtime_hr {
                    let bpm = heart_rate(&config, phase, &mut noise);
                    deliver!(realtime_hr_frame(
                        family,
                        base_time + phase,
                        bpm,
                        &mut noise
                    ));
                }
                // Battery once a minute, wrist-on once every two, so the event
                // path and the pushed-battery path both actually run.
                if phase.is_multiple_of(60) {
                    deliver!(battery_event(
                        family,
                        config.battery_percent,
                        base_time + phase
                    ));
                }
                if phase % 120 == 30 {
                    deliver!(strap_event(family, event::WRIST_ON, base_time + phase));
                }

                offload = match offload {
                    Offload::Streaming {
                        mut cursor,
                        mut in_chunk,
                    } => {
                        // A burst per tick, because a real offload arrives far
                        // faster than one record a second and a consumer has to
                        // cope with the burst rather than the average.
                        for _ in 0..10 {
                            if cursor >= config.stored_records || in_chunk >= config.chunk_records {
                                break;
                            }
                            let when = oldest + cursor * config.record_interval_secs;
                            let bpm = (i64::from(config.resting_hr) + noise.spread(6))
                                .clamp(35, 180) as u8;
                            deliver!(historical_record(family, when, bpm, cursor, &mut noise));
                            cursor += 1;
                            in_chunk += 1;
                        }

                        if in_chunk >= config.chunk_records || cursor >= config.stored_records {
                            let token = chunk_token(cursor);
                            let when = oldest + cursor * config.record_interval_secs;
                            deliver!(metadata(family, 2, when, token));
                            Offload::AwaitingAck {
                                cursor,
                                token,
                                resends: 0,
                            }
                        } else {
                            Offload::Streaming { cursor, in_chunk }
                        }
                    }
                    Offload::AwaitingAck {
                        cursor,
                        token,
                        resends,
                    } => {
                        // Resend up to four times, then abandon — the real
                        // strap's bound, so a client that never acknowledges
                        // sees the transfer end rather than hanging.
                        if resends >= 4 {
                            warn!(
                                "sim: no acknowledgement after five end markers, \
                                 ending the attempt"
                            );
                            Offload::Idle
                        } else {
                            let when = oldest + cursor * config.record_interval_secs;
                            deliver!(metadata(family, 2, when, token));
                            Offload::AwaitingAck {
                                cursor,
                                token,
                                resends: resends + 1,
                            }
                        }
                    }
                    Offload::Complete => {
                        deliver!(metadata(family, 3, base_time, [0u8; 8]));
                        Offload::Idle
                    }
                    Offload::Idle => Offload::Idle,
                };
            }
        }
    }

    connected.store(false, Ordering::Relaxed);
    let _ = events.send(WhoopEvent::Disconnected).await;
    info!("sim: disconnected");
}

/// Apply an acknowledgement, or refuse it.
///
/// The token has to match byte for byte. A client that rebuilt it from its
/// decoded trim cursor produces the right **first** word and a wrong second
/// one, which is precisely the bug this refuses to reward: the chunk is resent
/// instead of advanced.
fn acknowledge(offload: Offload, body: &[u8], config: &SimConfig) -> Offload {
    let Offload::AwaitingAck { cursor, token, .. } = offload else {
        warn!("sim: acknowledgement arrived with no end marker outstanding");
        return offload;
    };
    // `[0x01] + the eight-byte block`.
    let expected: Vec<u8> = std::iter::once(0x01).chain(token).collect();
    if body != expected.as_slice() {
        warn!(
            "sim: refusing an acknowledgement that does not echo the token sent — \
             expected {expected:02x?}, got {body:02x?}; the chunk will be resent"
        );
        return Offload::AwaitingAck {
            cursor,
            token,
            resends: 0,
        };
    }
    if cursor >= config.stored_records {
        info!("sim: offload complete, {cursor} record(s) acknowledged");
        Offload::Complete
    } else {
        Offload::Streaming {
            cursor,
            in_chunk: 0,
        }
    }
}

/// The eight-byte end token for a chunk: a trim cursor and a wrap word.
///
/// The second word is deliberately **not** derivable from the first. That is
/// what makes a rebuilt token detectable.
fn chunk_token(cursor: u32) -> [u8; 8] {
    let mut token = [0u8; 8];
    token[..4].copy_from_slice(&(100_000 + cursor).to_le_bytes());
    token[4..].copy_from_slice(&(0xA5A5_0000u32 | cursor).to_le_bytes());
    token
}

/// A slow oscillation around the resting rate, plus a little noise.
fn heart_rate(config: &SimConfig, phase: u32, noise: &mut Noise) -> u8 {
    let swing = (f64::from(phase) / 18.0).sin() * 6.0;
    (f64::from(config.resting_hr) + swing + noise.spread(2) as f64).clamp(35.0, 200.0) as u8
}

// ── Frame generators ─────────────────────────────────────────────────────────

/// A live heart-rate packet (type 40).
///
/// The two generations are genuinely different frames here: WHOOP 4 puts time
/// at 6 and intervals in **milliseconds** from 14, WHOOP 5 puts time at 10 and
/// intervals in **ticks of 1/1024 s** from 18. Generating both is how the
/// simulator catches a decoder that applied one family's units to the other.
fn realtime_hr_frame(family: DeviceFamily, unix: u32, bpm: u8, noise: &mut Noise) -> Vec<u8> {
    // Intervals consistent with the rate, so a consumer computing 60000/mean
    // lands near the reported bpm.
    let nominal_ms = 60_000u32 / u32::from(bpm.max(30));
    let intervals: Vec<u32> = (0..3)
        .map(|_| (nominal_ms as i64 + noise.spread(25)).clamp(300, 2000) as u32)
        .collect();

    match family {
        DeviceFamily::Whoop4 => {
            let mut builder = FrameBuilder::new(family, packet::REALTIME_DATA, 1, 0, 28);
            builder
                .u32(6, unix)
                .u8(12, bpm)
                .u8(13, intervals.len() as u8);
            for (i, ms) in intervals.iter().enumerate() {
                builder.u16(14 + i * 2, *ms as u16);
            }
            builder.finish()
        }
        DeviceFamily::Whoop5 => {
            let mut builder = FrameBuilder::new(family, packet::REALTIME_DATA, 0, 0, 32);
            builder
                .u32(10, unix)
                .u8(16, bpm)
                .u8(17, intervals.len() as u8);
            for (i, ms) in intervals.iter().enumerate() {
                // Back to ticks, so the decoder's conversion is what produces
                // the millisecond value a consumer sees.
                let ticks = (*ms * 1024 / 1000).min(u16::MAX as u32) as u16;
                builder.u16(18 + i * 2, ticks);
            }
            builder.finish()
        }
    }
}

/// A command response to `GET_BATTERY_LEVEL` (26).
///
/// WHOOP 4 carries `u16le` tenths, WHOOP 5 a four-byte whole percent — so the
/// per-family body width gets exercised rather than assumed.
fn battery_response(family: DeviceFamily, percent: f32) -> Vec<u8> {
    let opcode = Command::GetBatteryLevel.opcode();
    match family {
        DeviceFamily::Whoop4 => {
            let mut builder = FrameBuilder::new(family, packet::COMMAND_RESPONSE, 0x40, opcode, 16);
            builder
                .u8(7, 0) // originating sequence
                .u8(8, 1) // success
                .u16(9, (percent * 10.0) as u16);
            builder.finish()
        }
        DeviceFamily::Whoop5 => {
            let mut builder = FrameBuilder::new(family, packet::COMMAND_RESPONSE, 0x40, opcode, 24);
            builder.u8(11, 0).u8(12, 1).u32(13, percent as u32);
            builder.finish()
        }
    }
}

/// A pushed battery event (type 48, event 3).
///
/// The dense WHOOP 4 layout carries millivolts and a charging bit; the WHOOP 5
/// event carries neither, which is why only one of them produces a
/// [`crate::types::Battery`] with voltage.
fn battery_event(family: DeviceFamily, percent: f32, unix: u32) -> Vec<u8> {
    match family {
        DeviceFamily::Whoop4 => {
            let mut builder = FrameBuilder::new(family, packet::EVENT, 1, 0, 36);
            builder
                .u8(6, event::BATTERY_LEVEL)
                .u32(8, unix)
                .u16(17, (percent * 10.0) as u16)
                .u16(21, 3950)
                .u8(26, 0);
            builder.finish()
        }
        DeviceFamily::Whoop5 => strap_event(family, event::BATTERY_LEVEL, unix),
    }
}

/// A strap event (type 48).
fn strap_event(family: DeviceFamily, number: u8, unix: u32) -> Vec<u8> {
    match family {
        DeviceFamily::Whoop4 => {
            let mut builder = FrameBuilder::new(family, packet::EVENT, 1, 0, 20);
            builder.u8(6, number).u32(8, unix);
            builder.finish()
        }
        DeviceFamily::Whoop5 => {
            let mut builder = FrameBuilder::new(family, packet::EVENT, 0, 0, 24);
            builder.u8(10, number).u32(12, unix);
            builder.finish()
        }
    }
}

/// An offload control message (type 49).
///
/// The end token goes at 17 on WHOOP 4 and 21 on WHOOP 5 — the one offset pair
/// where getting it wrong means acknowledging the wrong chunk.
fn metadata(family: DeviceFamily, kind: u8, unix: u32, token: [u8; 8]) -> Vec<u8> {
    match family {
        DeviceFamily::Whoop4 => {
            let mut builder = FrameBuilder::new(family, packet::METADATA, 1, 0, 32);
            builder.u8(6, kind).u32(7, unix).bytes_at(17, &token);
            builder.finish()
        }
        DeviceFamily::Whoop5 => {
            let mut builder = FrameBuilder::new(family, packet::METADATA, 0, 0, 40);
            builder.u8(10, kind).u32(11, unix).bytes_at(21, &token);
            builder.finish()
        }
    }
}

/// One historical record: R18 on WHOOP 5, v24 on WHOOP 4.
///
/// Both are real layouts at their real sizes — 124 and 96 bytes — with the
/// fields a consumer actually reads. The WHOOP 4 one carries its sensor block
/// as raw ADC words, because that is what the layout has.
fn historical_record(
    family: DeviceFamily,
    unix: u32,
    bpm: u8,
    index: u32,
    noise: &mut Noise,
) -> Vec<u8> {
    let nominal_ms = 60_000u32 / u32::from(bpm.max(30));
    let intervals: Vec<u32> = (0..4)
        .map(|_| (nominal_ms as i64 + noise.spread(20)).clamp(300, 2000) as u32)
        .collect();
    // A gravity vector of roughly 1 g, tilted a little, so a consumer checking
    // the magnitude gets a physically plausible answer.
    let tilt = noise.spread(100) as f32 / 1000.0;

    match family {
        DeviceFamily::Whoop5 => {
            let mut builder = FrameBuilder::new(
                family,
                packet::HISTORICAL_DATA,
                layout::R18_BIOMETRIC,
                0x80,
                124,
            );
            builder
                .u32(11, index)
                .u32(15, unix)
                .u8(22, bpm)
                .u8(23, intervals.len() as u8);
            for (i, ms) in intervals.iter().enumerate() {
                builder.u16(24 + i * 2, (*ms * 1024 / 1000) as u16);
            }
            builder
                .u8(36, 0xA0)
                .f32(41, 0.03)
                .f32(45, tilt)
                .f32(49, tilt / 2.0)
                .f32(53, 1.0 - tilt.abs())
                .u16(57, (index % 10_000) as u16)
                .u8(63, u8::from(index.is_multiple_of(7)))
                // 33.50 °C in the raw/100 convention the decoder reverses.
                // Clamped in a signed intermediate: the noise is signed, and
                // casting it to u16 first would wrap a small negative wobble
                // into a 65,000-count temperature.
                .u16(73, (3350 + noise.spread(40)).clamp(500, 4500) as u16);
            builder.finish()
        }
        DeviceFamily::Whoop4 => {
            let mut builder = FrameBuilder::new(family, packet::HISTORICAL_DATA, 24, 0, 96);
            builder
                .u32(11, unix)
                .u8(21, bpm)
                .u8(22, intervals.len() as u8);
            for (i, ms) in intervals.iter().enumerate() {
                // Already milliseconds on this generation.
                builder.u16(23 + i * 2, *ms as u16);
            }
            builder
                .u16(33, 4100)
                .u16(35, 4200)
                .f32(40, tilt)
                .f32(44, tilt / 2.0)
                .f32(48, 1.0 - tilt.abs())
                .u8(55, 1)
                .u16(68, 5100)
                .u16(70, 5200)
                .u16(72, 3350)
                .u16(74, 90)
                .u16(80, 16)
                .u16(82, 7);
            builder.finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{verify_frame, MetadataType};
    use crate::types::Record;

    #[test]
    fn every_generated_frame_passes_the_real_verifier() {
        // If this fails, the simulator is producing frames no strap could send,
        // and nothing it reports afterwards means anything.
        for family in [DeviceFamily::Whoop4, DeviceFamily::Whoop5] {
            let mut noise = Noise(1);
            let frames = [
                realtime_hr_frame(family, 1_700_000_000, 58, &mut noise),
                battery_response(family, 62.0),
                battery_event(family, 62.0, 1_700_000_000),
                strap_event(family, event::WRIST_ON, 1_700_000_000),
                metadata(family, 2, 1_700_000_000, chunk_token(7)),
                historical_record(family, 1_700_000_000, 58, 3, &mut noise),
            ];
            for frame in frames {
                let check = verify_frame(&frame, family);
                assert!(check.ok, "{family:?}: {check:?} for {} bytes", frame.len());
            }
        }
    }

    #[test]
    fn generated_frames_decode_to_the_events_they_encode() {
        for family in [DeviceFamily::Whoop4, DeviceFamily::Whoop5] {
            let mut noise = Noise(2);

            let hr = decode_frame(
                &realtime_hr_frame(family, 1_700_000_000, 61, &mut noise),
                family,
            );
            let [WhoopEvent::HeartRate(hr)] = &hr[..] else {
                panic!("{family:?}: expected a heart rate, got {hr:?}");
            };
            assert_eq!(hr.bpm, 61);
            assert_eq!(hr.rr_ms.len(), 3);
            // The intervals are generated from the rate, so the usual
            // cross-check lands close. This is the arithmetic that catches a
            // tick/millisecond mix-up: with the wrong units it is out by ~2.4%.
            let mean = hr.rr_ms.iter().sum::<u32>() / hr.rr_ms.len() as u32;
            let implied = 60_000 / mean;
            assert!(
                implied.abs_diff(61) <= 3,
                "{family:?}: {mean} ms implies {implied} bpm, not 61"
            );

            let battery = decode_frame(&battery_response(family, 62.0), family);
            assert!(
                battery
                    .iter()
                    .any(|e| matches!(e, WhoopEvent::Battery(b) if (b.percent - 62.0).abs() < 0.2)),
                "{family:?}: {battery:?}"
            );

            let record = decode_frame(
                &historical_record(family, 1_700_000_000, 57, 1, &mut noise),
                family,
            );
            let [WhoopEvent::Record(Record::Biometric(r))] = &record[..] else {
                panic!("{family:?}: expected a biometric record, got {record:?}");
            };
            assert_eq!(r.heart_rate.bpm, 57);
            assert_eq!(r.unix_seconds, 1_700_000_000);
            // Gravity magnitude near 1 g, as a real record's would be.
            let g = r.gravity_g.expect("a gravity vector");
            let magnitude = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt();
            assert!(
                (0.9..=1.1).contains(&magnitude),
                "{family:?}: |g| = {magnitude}"
            );
        }
    }

    #[test]
    fn the_end_token_survives_the_round_trip_to_the_session() {
        // The token the simulator sends has to be the token a client gets back
        // out of the decoder — all eight bytes, both words.
        for family in [DeviceFamily::Whoop4, DeviceFamily::Whoop5] {
            let token = chunk_token(42);
            let events = decode_frame(&metadata(family, 2, 1_700_000_000, token), family);
            let [WhoopEvent::HistoryMeta(meta)] = &events[..] else {
                panic!("{family:?}: expected history metadata, got {events:?}");
            };
            assert_eq!(meta.kind, MetadataType::HistoryEnd);
            assert_eq!(meta.end_block, Some(token));
        }
    }

    #[test]
    fn a_rebuilt_token_is_refused_and_the_chunk_resent() {
        // The mistake this exists to catch: an acknowledgement built from the
        // decoded trim cursor gets the first word right and the second wrong.
        let config = SimConfig {
            stored_records: 100,
            ..SimConfig::default()
        };
        let token = chunk_token(50);
        let outstanding = Offload::AwaitingAck {
            cursor: 50,
            token,
            resends: 2,
        };

        let mut rebuilt = vec![0x01];
        rebuilt.extend_from_slice(&token[..4]);
        rebuilt.extend_from_slice(&[0, 0, 0, 0]);
        let after = acknowledge(outstanding, &rebuilt, &config);
        assert!(
            matches!(after, Offload::AwaitingAck { resends: 0, .. }),
            "a rebuilt token must not advance the cursor"
        );

        // The real thing advances it.
        let correct: Vec<u8> = std::iter::once(0x01).chain(token).collect();
        let after = acknowledge(
            Offload::AwaitingAck {
                cursor: 50,
                token,
                resends: 0,
            },
            &correct,
            &config,
        );
        assert!(matches!(
            after,
            Offload::Streaming {
                cursor: 50,
                in_chunk: 0
            }
        ));
    }

    #[test]
    fn the_last_acknowledged_chunk_completes_the_attempt() {
        let config = SimConfig {
            stored_records: 50,
            ..SimConfig::default()
        };
        let token = chunk_token(50);
        let correct: Vec<u8> = std::iter::once(0x01).chain(token).collect();
        let after = acknowledge(
            Offload::AwaitingAck {
                cursor: 50,
                token,
                resends: 0,
            },
            &correct,
            &config,
        );
        assert!(matches!(after, Offload::Complete));
    }

    #[test]
    fn noise_is_reproducible_and_bounded() {
        // A session that behaves differently every run cannot be used to chase
        // a decoder bug.
        let mut a = Noise(7);
        let mut b = Noise(7);
        for _ in 0..100 {
            let value = a.spread(5);
            assert_eq!(value, b.spread(5));
            assert!((-5..=5).contains(&value));
        }
        assert_eq!(Noise(9).spread(0), 0);
    }
}
