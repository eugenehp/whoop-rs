//! Checksums, frame envelopes, verification, reassembly and frame building.

use super::DeviceFamily;

// ── Checksums ────────────────────────────────────────────────────────────────

/// CRC8 lookup table, polynomial 0x07, init 0x00.
static CRC8_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Reflected CRC32 table, polynomial 0xEDB88320 (the zlib convention).
static CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC8 (table-driven, poly 0x07, init 0x00) — the WHOOP 4 header check.
///
/// Computed over the **two length bytes only**, never the whole frame.
///
/// ```
/// # use poop::protocol::crc8;
/// // The length field of an 11-byte minimal WHOOP 4 frame.
/// assert_eq!(crc8(&[0x07, 0x00]), 0x6b);
/// ```
pub fn crc8(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |crc, &b| CRC8_TABLE[(crc ^ b) as usize])
}

/// Standard zlib CRC32 — reflected, poly 0xEDB88320, init and final XOR
/// 0xFFFFFFFF. The payload check on both generations.
pub fn crc32(bytes: &[u8]) -> u32 {
    let crc = bytes.iter().fold(0xFFFF_FFFFu32, |crc, &b| {
        CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8)
    });
    crc ^ 0xFFFF_FFFF
}

/// CRC16-Modbus — poly 0xA001, init 0xFFFF, reflected. The WHOOP 5 header
/// check, computed over bytes `[0,6)` of the frame.
pub fn crc16_modbus(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0xFFFFu16, |mut crc, &b| {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
        crc
    })
}

// ── Little-endian readers ────────────────────────────────────────────────────

/// `u16` little-endian at `off`, or `None` if the slice is too short.
#[inline]
pub fn u16le(bytes: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*bytes.get(off)?, *bytes.get(off + 1)?]))
}

/// `u32` little-endian at `off`, or `None` if the slice is too short.
#[inline]
pub fn u32le(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *bytes.get(off)?,
        *bytes.get(off + 1)?,
        *bytes.get(off + 2)?,
        *bytes.get(off + 3)?,
    ]))
}

/// `i16` little-endian at `off`, or `None` if the slice is too short.
#[inline]
pub fn i16le(bytes: &[u8], off: usize) -> Option<i16> {
    u16le(bytes, off).map(|v| v as i16)
}

/// `i32` little-endian at `off`, or `None` if the slice is too short.
#[inline]
pub fn i32le(bytes: &[u8], off: usize) -> Option<i32> {
    u32le(bytes, off).map(|v| v as i32)
}

/// IEEE-754 `f32` little-endian at `off`, or `None` if the slice is too short.
#[inline]
pub fn f32le(bytes: &[u8], off: usize) -> Option<f32> {
    u32le(bytes, off).map(f32::from_bits)
}

// ── Frame integrity ──────────────────────────────────────────────────────────

/// Why a frame failed the envelope check.
///
/// [`Self::None`] is the only value that accompanies a positive verdict.
/// Structural failures are decided before the payload CRC is required, so every
/// frame that reaches the integrity decision has a computable CRC32 by
/// construction.
///
/// These are **local** failures. None of them is a command result that came off
/// the wire: an unsupported format, a wrong size or a bad checksum is something
/// this side concluded, and conflating the two is how a corrupt frame gets
/// reported as a strap refusing a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRejectReason {
    /// The frame is intact.
    None,
    /// First byte is not `0xAA`.
    NoStartOfFrame,
    /// Shorter than the family minimum, or declaring less than it.
    BelowMinimumLength,
    /// Declared size and supplied size disagree — truncated or with a tail.
    LengthMismatch,
    /// Header checksum over the length field does not match.
    HeaderChecksum,
    /// Payload CRC32 does not match.
    PayloadChecksum,
    /// Both checksums are wrong.
    HeaderAndPayloadChecksum,
}

impl FrameRejectReason {
    fn from_checksums(header_ok: bool, payload_ok: bool) -> Self {
        match (header_ok, payload_ok) {
            (true, true) => Self::None,
            (false, true) => Self::HeaderChecksum,
            (true, false) => Self::PayloadChecksum,
            (false, false) => Self::HeaderAndPayloadChecksum,
        }
    }
}

/// The verdict on one complete frame: structure and both checksums together.
///
/// `ok` means **intact**, not **parsed**. A frame with a broken header is still
/// worth decoding into a diagnostic view, which is why the individual checksum
/// outcomes stay on the result instead of collapsing into a bool. Code that
/// wants to know whether a decode produced anything must ask the decoder, not
/// read `ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FrameCheck {
    /// Structure and both checksums agree.
    pub ok: bool,
    /// The declared length field, when enough bytes existed to read it.
    pub length: Option<usize>,
    /// Header checksum outcome — CRC8 on WHOOP 4, CRC16 on WHOOP 5, so callers
    /// get one uniform "header ok?" signal either way.
    pub header_crc_ok: Option<bool>,
    /// Payload CRC32 outcome. `None` when truncation made it uncomputable.
    pub payload_crc_ok: Option<bool>,
    /// Which rule failed first.
    pub reason: FrameRejectReason,
}

impl FrameCheck {
    const fn rejected(reason: FrameRejectReason) -> Self {
        Self {
            ok: false,
            length: None,
            header_crc_ok: None,
            payload_crc_ok: None,
            reason,
        }
    }
}

/// Validate a complete frame envelope for `family`.
///
/// WHOOP 4: `AA | len u16le | crc8(len) | type | seq | cmd | payload | crc32`,
/// total `len + 4`, CRC32 over `[4, len)`.
///
/// WHOOP 5 format 1: `AA | 01 | declLen u16le | hdr[2] | crc16 u16le | type |
/// seq | cmd | body | pad | crc32`, total `declLen + 8`, CRC16 over `[0,6)`,
/// CRC32 over `[8, total-4)` **including the padding**.
///
/// A frame is accepted only when it is at least the family minimum, carries
/// exactly the declared size (so truncation and a surplus tail are both
/// rejected), and both checksums match.
pub fn verify_frame(frame: &[u8], family: DeviceFamily) -> FrameCheck {
    if frame.first() != Some(&0xAA) {
        return FrameCheck::rejected(FrameRejectReason::NoStartOfFrame);
    }
    let floor = family.minimum_frame_bytes();
    if frame.len() < floor {
        // Below the smallest real inner record: no field is read, and the length
        // word it may carry is not worth reporting as a length.
        return FrameCheck::rejected(FrameRejectReason::BelowMinimumLength);
    }

    // Where the declared length sits, what it counts, and what the header check
    // covers are the three things that differ. Everything after is shared.
    let (declared, total, header_crc_ok) = match family {
        DeviceFamily::Whoop4 => {
            let declared = u16::from_le_bytes([frame[1], frame[2]]) as usize;
            (declared, declared + 4, crc8(&frame[1..3]) == frame[3])
        }
        DeviceFamily::Whoop5 => {
            let declared = u16::from_le_bytes([frame[2], frame[3]]) as usize;
            let got = u16::from_le_bytes([frame[6], frame[7]]);
            (declared, declared + 8, crc16_modbus(&frame[0..6]) == got)
        }
    };
    let body_start = family.inner_offset();

    // A payload CRC is still worth reporting when only the envelope is wrong:
    // "payload CRC right, envelope wrong" is a distinct class, and the frames
    // in it are exactly the ones a capture exists to preserve. Truncation is
    // the one case that leaves it uncomputable.
    let payload_crc = |total: usize| -> Option<bool> {
        (total >= body_start + 4 && total <= frame.len()).then(|| {
            crc32(&frame[body_start..total - 4])
                == u32::from_le_bytes([
                    frame[total - 4],
                    frame[total - 3],
                    frame[total - 2],
                    frame[total - 1],
                ])
        })
    };

    if total < floor {
        return FrameCheck {
            ok: false,
            length: Some(declared),
            header_crc_ok: Some(header_crc_ok),
            payload_crc_ok: payload_crc(total),
            reason: FrameRejectReason::BelowMinimumLength,
        };
    }
    if total != frame.len() {
        return FrameCheck {
            ok: false,
            length: Some(declared),
            header_crc_ok: Some(header_crc_ok),
            payload_crc_ok: payload_crc(total),
            reason: FrameRejectReason::LengthMismatch,
        };
    }

    // The structural checks above prove a complete four-byte trailer is in
    // bounds, so this unwrap cannot fire.
    let payload_crc_ok = payload_crc(total).expect("structural checks bound the trailer");
    let reason = FrameRejectReason::from_checksums(header_crc_ok, payload_crc_ok);
    FrameCheck {
        ok: reason == FrameRejectReason::None,
        length: Some(declared),
        header_crc_ok: Some(header_crc_ok),
        payload_crc_ok: Some(payload_crc_ok),
        reason,
    }
}

/// Packet type byte of a frame — `[4]` on WHOOP 4, `[8]` on WHOOP 5.
pub fn packet_type(frame: &[u8], family: DeviceFamily) -> Option<u8> {
    frame.get(family.inner_offset()).copied()
}

/// Offset where the CRC32 trailer begins, i.e. one past the last payload byte.
///
/// Every field decoder bounds its reads with this, so no decoded value can ever
/// come out of the checksum trailer.
pub fn payload_end(frame: &[u8], family: DeviceFamily) -> Option<usize> {
    let declared = match family {
        DeviceFamily::Whoop4 => u16le(frame, 1)? as usize,
        DeviceFamily::Whoop5 => u16le(frame, 2)? as usize + 4,
    };
    let end = declared.min(frame.len().saturating_sub(4));
    (end > family.inner_offset()).then_some(end)
}

// ── Reassembly ───────────────────────────────────────────────────────────────

/// Accumulate BLE notification fragments into complete frames.
///
/// One per characteristic per connection, because each notify channel is its
/// own byte stream and interleaving two of them in a single buffer invents
/// frames that were never sent.
///
/// The resync discipline is the interesting part. A bare `0xAA` occurs inside
/// payload as readily as at a frame start, so a stream that desynced — a
/// subscription landing mid-frame, one earlier mis-parse — can put the cursor on
/// a payload byte whose next bytes read as a *plausible* length. Waiting for
/// that many bytes and emitting them as one frame swallows every valid frame
/// inside it, and nothing downstream can hand them back. So the header checksum,
/// which already covers the length field on both families, gates acceptance
/// before any bytes are consumed: a real frame always passes, a false start
/// passes with probability 1/256 on WHOOP 4 and 1/65536 on WHOOP 5.
#[derive(Debug)]
pub struct Reassembler {
    buf: Vec<u8>,
    /// Index of the first byte not yet consumed. Advancing a cursor rather than
    /// draining off the front keeps a multi-night offload — thousands of ~2 KB
    /// records — from paying an O(n) shift per frame.
    head: usize,
    family: DeviceFamily,
    below_minimum_length_drops: u64,
    header_checksum_drops: u64,
    oversize_drops: u64,
}

impl Reassembler {
    /// ~4× the largest real WHOOP frame (R20 optical is 2,140 bytes). A declared
    /// total beyond this is a corrupt or misaligned length — a bit flip, or a
    /// spurious `0xAA` mid-frame — so that start byte is dropped and the stream
    /// resyncs rather than stalling until a reconnect.
    pub const MAX_FRAME_BYTES: usize = 8192;

    /// A reassembler for one characteristic of one connection.
    pub fn new(family: DeviceFamily) -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
            family,
            below_minimum_length_drops: 0,
            header_checksum_drops: 0,
            oversize_drops: 0,
        }
    }

    /// Start bytes dropped for declaring less than the family minimum. Such a
    /// byte run never reaches a parser, so without a counter it would vanish
    /// without trace. Monotonic for the lifetime of the reassembler.
    pub fn below_minimum_length_drops(&self) -> u64 {
        self.below_minimum_length_drops
    }

    /// Start bytes dropped because the checksum over their own length field did
    /// not match — a `0xAA` that is not a frame start. Counted separately from
    /// the floor drops because the two say different things about a link: a
    /// floor drop is a malformed frame, this is a misplaced read cursor.
    pub fn header_checksum_drops(&self) -> u64 {
        self.header_checksum_drops
    }

    /// Start bytes dropped for declaring more than [`Self::MAX_FRAME_BYTES`].
    pub fn oversize_drops(&self) -> u64 {
        self.oversize_drops
    }

    /// Bytes currently held waiting for the rest of a frame.
    pub fn pending_bytes(&self) -> usize {
        self.buf.len() - self.head
    }

    /// Feed one notification; get back every complete frame it finished, in
    /// arrival order.
    pub fn feed(&mut self, fragment: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(fragment);
        let mut out = Vec::new();
        let floor = self.family.minimum_frame_bytes();

        loop {
            let Some(sof) = self.index_of_sof() else {
                // No start byte left in the window: nothing here is salvageable.
                self.buf.clear();
                self.head = 0;
                break;
            };
            // Skip bytes ahead of the start byte rather than removing them.
            self.head = sof;
            let avail = self.buf.len() - self.head;
            // Both families need four bytes to read their declared length.
            if avail < 4 {
                break;
            }

            let h = self.head;
            let total = match self.family {
                DeviceFamily::Whoop4 => {
                    u16::from_le_bytes([self.buf[h + 1], self.buf[h + 2]]) as usize + 4
                }
                DeviceFamily::Whoop5 => {
                    u16::from_le_bytes([self.buf[h + 2], self.buf[h + 3]]) as usize + 8
                }
            };

            if total < floor {
                // Emitting this would hand the parser a byte run whose "inner
                // fields" are its own checksum trailer.
                self.below_minimum_length_drops += 1;
                self.head += 1;
                continue;
            }
            if total > Self::MAX_FRAME_BYTES {
                self.oversize_drops += 1;
                self.head += 1;
                continue;
            }

            // An in-range length is not the same as a real one — see the type
            // docs. Ordered after the floor and ceiling so this gate only sees
            // lengths those two accepted.
            let header_bytes = match self.family {
                DeviceFamily::Whoop4 => 4,
                DeviceFamily::Whoop5 => 8,
            };
            if avail < header_bytes {
                break;
            }
            let header_ok = match self.family {
                DeviceFamily::Whoop4 => crc8(&self.buf[h + 1..h + 3]) == self.buf[h + 3],
                DeviceFamily::Whoop5 => {
                    let got = u16::from_le_bytes([self.buf[h + 6], self.buf[h + 7]]);
                    crc16_modbus(&self.buf[h..h + 6]) == got
                }
            };
            if !header_ok {
                self.header_checksum_drops += 1;
                self.head += 1;
                continue;
            }

            if avail < total {
                break;
            }
            out.push(self.buf[h..h + total].to_vec());
            self.head += total;
        }

        self.compact();
        out
    }

    /// Index of the first `0xAA` at or after the cursor.
    fn index_of_sof(&self) -> Option<usize> {
        self.buf[self.head..]
            .iter()
            .position(|&b| b == 0xAA)
            .map(|i| self.head + i)
    }

    /// Slide the unconsumed tail to the front so the cursor cannot drift
    /// forever. Runs at the end of every `feed`, so the cursor is always 0 when
    /// the next append lands, and the leftover is at most one in-progress frame.
    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        if self.head >= self.buf.len() {
            self.buf.clear();
        } else {
            self.buf.drain(..self.head);
        }
        self.head = 0;
    }
}

// ── Packet types ─────────────────────────────────────────────────────────────

/// Inner packet type values, read at `[4]` on WHOOP 4 and `[8]` on WHOOP 5.
///
/// This is a decoder vocabulary, not a promise that either generation produces
/// every one of them.
pub mod packet {
    /// Outbound command, app → strap.
    pub const COMMAND: u8 = 35;
    /// Reply to a command.
    pub const COMMAND_RESPONSE: u8 = 36;
    /// Older label; role on 50.42.1.0 unconfirmed.
    pub const WHOOP5_COMMAND_INNER: u8 = 37;
    /// Older label, decoded as [`COMMAND_RESPONSE`].
    pub const WHOOP5_COMMAND_RESPONSE: u8 = 38;
    /// Live heart rate and R-R.
    pub const REALTIME_DATA: u8 = 40;
    /// Live raw sensor data; on WHOOP 5 also carries ECG R16/R17.
    pub const REALTIME_RAW_DATA: u8 = 43;
    /// Offloaded historical records.
    pub const HISTORICAL_DATA: u8 = 47;
    /// Strap-pushed event.
    pub const EVENT: u8 = 48;
    /// Offload control metadata — START / END / COMPLETE.
    pub const METADATA: u8 = 49;
    /// Firmware log text.
    pub const CONSOLE_LOGS: u8 = 50;
    /// Dedicated realtime IMU stream.
    pub const REALTIME_IMU_DATA_STREAM: u8 = 51;
    /// Dedicated historical IMU stream.
    pub const HISTORICAL_IMU_DATA_STREAM: u8 = 52;
    /// WHOOP 5 relative events.
    pub const WHOOP5_RELATIVE_EVENTS: u8 = 53;
    /// WHOOP 5 strap events.
    pub const WHOOP5_EVENTS_FROM_STRAP: u8 = 54;
    /// Battery-pack console logs.
    pub const RELATIVE_BATTERY_PACK_CONSOLE_LOGS: u8 = 55;
    /// WHOOP 5 metadata, decoded as [`METADATA`].
    pub const WHOOP5_METADATA: u8 = 56;
}

/// Canonical name for a packet type.
///
/// The two WHOOP 5 aliases are folded onto the names they share decoding
/// semantics with — 38 → `COMMAND_RESPONSE`, 56 → `METADATA` — so neither ever
/// decodes as unknown.
pub fn packet_type_name(t: u8) -> &'static str {
    match t {
        packet::COMMAND => "COMMAND",
        packet::COMMAND_RESPONSE | packet::WHOOP5_COMMAND_RESPONSE => "COMMAND_RESPONSE",
        packet::WHOOP5_COMMAND_INNER => "WHOOP5_COMMAND_INNER",
        packet::REALTIME_DATA => "REALTIME_DATA",
        packet::REALTIME_RAW_DATA => "REALTIME_RAW_DATA",
        packet::HISTORICAL_DATA => "HISTORICAL_DATA",
        packet::EVENT => "EVENT",
        packet::METADATA | packet::WHOOP5_METADATA => "METADATA",
        packet::CONSOLE_LOGS => "CONSOLE_LOGS",
        packet::REALTIME_IMU_DATA_STREAM => "REALTIME_IMU_DATA_STREAM",
        packet::HISTORICAL_IMU_DATA_STREAM => "HISTORICAL_IMU_DATA_STREAM",
        packet::WHOOP5_RELATIVE_EVENTS => "WHOOP5_RELATIVE_EVENTS",
        packet::WHOOP5_EVENTS_FROM_STRAP => "WHOOP5_EVENTS_FROM_STRAP",
        packet::RELATIVE_BATTERY_PACK_CONSOLE_LOGS => "RELATIVE_BATTERY_PACK_CONSOLE_LOGS",
        _ => "UNKNOWN",
    }
}

/// Whether a packet type belongs to the historical offload.
///
/// The live streams (40, 43) are deliberately excluded: an offload idle
/// watchdog re-armed by a type-43 flood never fires, so a strap that has stopped
/// sending history would look busy forever.
pub fn is_offload_frame(t: u8) -> bool {
    matches!(
        t,
        packet::HISTORICAL_DATA
            | packet::EVENT
            | packet::METADATA
            | packet::CONSOLE_LOGS
            | packet::HISTORICAL_IMU_DATA_STREAM
            | packet::WHOOP5_METADATA
    )
}

// ── Frame construction ───────────────────────────────────────────────────────

/// Build a command frame for `family`.
///
/// The one entry point the client uses, so the envelope can never be chosen
/// independently of the connection it is written to.
pub fn command_frame(family: DeviceFamily, cmd: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    match family {
        DeviceFamily::Whoop4 => whoop4_command_frame(cmd, seq, payload),
        DeviceFamily::Whoop5 => whoop5_command_frame(cmd, seq, payload),
    }
}

/// Build a WHOOP 4 type-35 COMMAND frame.
///
/// `AA | len u16le | crc8(len) | 35 | seq | cmd | payload | crc32`, where `len`
/// is `inner.len() + 4` and the CRC32 covers the inner record.
///
/// ```
/// # use poop::protocol::{whoop4_command_frame, verify_frame, DeviceFamily};
/// let frame = whoop4_command_frame(26, 1, &[0x00]);
/// assert!(verify_frame(&frame, DeviceFamily::Whoop4).ok);
/// ```
pub fn whoop4_command_frame(cmd: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(3 + payload.len());
    inner.extend_from_slice(&[packet::COMMAND, seq, cmd]);
    inner.extend_from_slice(payload);

    let length = (inner.len() + 4) as u16;
    let len_bytes = length.to_le_bytes();

    let mut frame = Vec::with_capacity(4 + inner.len() + 4);
    frame.push(0xAA);
    frame.extend_from_slice(&len_bytes);
    frame.push(crc8(&len_bytes));
    frame.extend_from_slice(&inner);
    frame.extend_from_slice(&crc32(&inner).to_le_bytes());
    frame
}

/// Build a WHOOP 5/MG format-1 COMMAND frame with the default type 35 and
/// request header `00 01`.
///
/// The inner record is **zero-padded to a four-byte boundary before** the
/// declared length and the CRC32 are computed, exactly as the strap's framing
/// does. It is a no-op for the four-aligned commands but required for the
/// 12-byte haptics body (inner 15 → 16); without it the length and checksum
/// cover the wrong byte count and the strap rejects the frame.
///
/// Padding is not a semantic argument: an explicit request byte `00` and a
/// missing argument must not be treated as interchangeable just because padding
/// can make their frames look alike.
pub fn whoop5_command_frame(cmd: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    whoop5_frame_with(cmd, seq, payload, packet::COMMAND, [0x00, 0x01])
}

/// [`whoop5_command_frame`] with the inner type and header bytes exposed.
///
/// The baseline acceptance constraints do not require one constant value for
/// bytes 4–5, so they are a parameter rather than a literal.
pub fn whoop5_frame_with(cmd: u8, seq: u8, payload: &[u8], ptype: u8, header: [u8; 2]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + payload.len());
    inner.extend_from_slice(&[ptype, seq, cmd]);
    inner.extend_from_slice(payload);
    let pad = (4 - inner.len() % 4) % 4;
    inner.resize(inner.len() + pad, 0);

    let decl_len = (inner.len() + 4) as u16;

    let mut frame = Vec::with_capacity(8 + inner.len() + 4);
    frame.push(0xAA);
    frame.push(0x01);
    frame.extend_from_slice(&decl_len.to_le_bytes());
    frame.extend_from_slice(&header);
    // Computed into a local first: the checksum reads the six bytes already in
    // the frame, and extending it while borrowing them is not expressible.
    let header_crc = crc16_modbus(&frame[0..6]).to_le_bytes();
    frame.extend_from_slice(&header_crc);
    frame.extend_from_slice(&inner);
    frame.extend_from_slice(&crc32(&inner).to_le_bytes());
    frame
}

/// Rebuild a complete WHOOP 4 frame from a bare payload (`frame[7..]`).
///
/// Some captures store only the data portion. The header checksum is computed
/// rather than zero-filled, so a rebuilt frame is a frame a strap could have
/// sent and round-trips through [`verify_frame`] positively end to end.
pub fn frame_from_payload(data: &[u8], ptype: u8, seq: u8, cmd: u8) -> Vec<u8> {
    whoop4_command_frame_with_type(ptype, cmd, seq, data)
}

fn whoop4_command_frame_with_type(ptype: u8, cmd: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(3 + payload.len());
    inner.extend_from_slice(&[ptype, seq, cmd]);
    inner.extend_from_slice(payload);

    let length = (inner.len() + 4) as u16;
    let len_bytes = length.to_le_bytes();

    let mut frame = Vec::with_capacity(4 + inner.len() + 4);
    frame.push(0xAA);
    frame.extend_from_slice(&len_bytes);
    frame.push(crc8(&len_bytes));
    frame.extend_from_slice(&inner);
    frame.extend_from_slice(&crc32(&inner).to_le_bytes());
    frame
}
