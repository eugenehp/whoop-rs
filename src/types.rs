//! Every event and data type the client produces.
//!
//! The naming discipline is deliberate: a field whose physical meaning is
//! established is named for it, and a field that is only *structurally* known
//! keeps a raw name. Skin temperature is `skin_temp_c`
//! because the conversion is documented; the optical values are `raw` because
//! no calibrated unit is. Nothing is converted into a unit it has not earned.

use serde::Serialize;

use crate::protocol::{CommandResult, DeviceFamily, FrameRejectReason, MetadataType};

// ── Live heart rate ──────────────────────────────────────────────────────────

/// Where a heart-rate reading came from.
///
/// The two sources are independent, and a healthy reading from one says nothing
/// about the other: the standard service can be fine while proprietary type-40
/// output or historical storage is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HrSource {
    /// The standard Heart Rate Measurement characteristic (`2A37`). Works
    /// unbonded; carries no device timestamp.
    StandardService,
    /// The proprietary live packet (type 40), which carries the strap's own
    /// clock reading.
    RealtimePacket,
    /// A biometric summary record from the historical offload (R18, or WHOOP 4
    /// v24).
    Historical,
}

/// One heart-rate observation with any R-R intervals that came with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeartRate {
    /// Which stream produced this.
    pub source: HrSource,
    /// Beats per minute as reported. Not recomputed from the intervals: the two
    /// are separate fields on the wire and disagreement is information.
    pub bpm: u8,
    /// R-R intervals in whole milliseconds, zero placeholders removed.
    ///
    /// A zero on the wire is a placeholder rather than a measured interval, so
    /// it is dropped here; the count that remains can therefore be smaller than
    /// the declared count.
    pub rr_ms: Vec<u32>,
    /// The strap's clock reading in Unix seconds, where the record carries one.
    ///
    /// Correct wall time depends on the strap's clock state — an `RTC_LOST`
    /// event, or a clock never set, leaves this a plausible-looking number that
    /// is not a date.
    pub unix_seconds: Option<u32>,
}

// ── Battery ──────────────────────────────────────────────────────────────────

/// Where a battery reading came from. The sources disagree in practice, so they
/// are kept apart rather than averaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatterySource {
    /// Standard Battery Level (`2A19`). Some WHOOP 4 straps report a constant
    /// 100 here.
    StandardService,
    /// Command 26. Measured for the request rather than read from a cache.
    Command,
    /// A pushed `BATTERY_LEVEL` event (type 48, event 3), which also carries
    /// millivolts and a charging bit.
    Event,
    /// The identity hello's battery field.
    Hello,
}

/// A battery observation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Battery {
    /// Which source produced it.
    pub source: BatterySource,
    /// State of charge in percent.
    ///
    /// Zero can also be a conversion fallback, so it does not by itself prove a
    /// depleted battery.
    pub percent: f32,
    /// Pack voltage in millivolts, where the source carries it.
    pub millivolts: Option<u16>,
    /// Whether the strap reported itself charging, where the source says.
    pub charging: Option<bool>,
}

// ── Strap events ─────────────────────────────────────────────────────────────

/// A strap-pushed event (packet type 48).
///
/// Treat the arrival of any event as "there may be new data": that is how WHOOP
/// straps signal, and acting only on the numbers you recognise means missing
/// the ones you do not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrapEvent {
    /// Raw event number, preserved even when unnamed.
    pub number: u8,
    /// Name from the event table, or `UNKNOWN_EVENT`.
    pub name: &'static str,
    /// The event's own timestamp in Unix seconds.
    pub unix_seconds: u32,
    /// A battery reading, when this is a `BATTERY_LEVEL` event with the dense
    /// WHOOP 4 layout.
    pub battery: Option<Battery>,
}

// ── Command responses ────────────────────────────────────────────────────────

/// A decoded reply to a command (packet type 36).
///
/// Correlate on [`Self::command`] **and** [`Self::origin_sequence`] within a
/// connection: the response frame carries its own generated sequence, which is
/// not an echo of the request, and one request can produce several CRC-valid
/// replies.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandResponse {
    /// The command this answers.
    pub command: u8,
    /// Its protocol name, or `CMD_<n>`.
    pub name: String,
    /// The originating request's sequence byte — the correlation field.
    pub origin_sequence: u8,
    /// The response frame's own sequence. Not a request echo.
    pub response_sequence: u8,
    /// The result byte, classified.
    pub result: CommandResult,
    /// The response body, with framing, padding and CRC excluded.
    ///
    /// Kept raw because the first byte is command-specific: a revision marker
    /// for several revision-1 controls, the start of a four-byte percent for
    /// command 26, and never a universal success flag.
    #[serde(serialize_with = "hex")]
    pub body: Vec<u8>,
}

// ── Identity ─────────────────────────────────────────────────────────────────

/// The decoded fields of a WHOOP 5 `GET_HELLO` (145) reply.
///
/// Only the fields a client has a use for. The reply also contains identity
/// blocks and key material; a decoder that dumped the whole body would be
/// exporting personal device information for no reason, so the sensitive region
/// is never copied out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hello {
    /// Request revision echoed back — 1 or 3.
    pub revision: u8,
    /// Battery in percent, from a tenths-of-a-percent field.
    pub battery_percent: f32,
    /// The strap's clock reading in Unix seconds.
    pub unix_seconds: u32,
    /// Firmware version components, e.g. `50.42.1.0`.
    pub firmware: String,
    /// Preparation status bits, preserved verbatim.
    ///
    /// The reply reports success even when these are set: `0x10` means identity
    /// block A could not be filled and `0x20` the same for block B, so a
    /// success result is not a promise that every field is valid.
    pub preparation_bits: u32,
}

// ── History ──────────────────────────────────────────────────────────────────

/// A historical-offload control message (packet type 49 or 56).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryMeta {
    /// Start, end or complete.
    pub kind: MetadataType,
    /// The chunk's clock reading, on an end marker.
    pub unix_seconds: Option<u32>,
    /// The trim cursor — the first word of the end block.
    pub trim_cursor: Option<u32>,
    /// The verbatim eight-byte acknowledgement block, on an end marker.
    ///
    /// Echoed back unchanged and never rebuilt: the first word is a read-page
    /// position modulo ring capacity and the second reflects write-wrap state,
    /// and special values (`0xffffffff`, paired `0xfefefefe`, paired
    /// `0xfdfdfdfd`) select strap behaviour that a synthesised token would
    /// invoke by accident.
    pub end_block: Option<[u8; 8]>,
}

/// The strap's stored record range and ring cursors — command 34.
///
/// The positions are **page slots**, not timestamps, and the counts are
/// estimates that can fall back to fifteen records per page. Treat them as
/// distances, not durations.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DataRange {
    /// Ring boundary A. Its full oldest/erase role is unresolved.
    pub boundary: u32,
    /// Read-page cursor.
    pub read_page: u32,
    /// Write-page cursor.
    pub write_page: u32,
    /// Acknowledged/trim boundary.
    pub trim_boundary: u32,
    /// Write-wrap state or count.
    pub wrap_state: u32,
    /// Ring capacity in page slots.
    pub capacity: u32,
    /// Record-count estimate. Not an exact row count.
    pub record_estimate: u32,
    /// The four clock pairs, as `(seconds, companion)`.
    ///
    /// Selected from the boundary page, the trim-side page, the read-side page
    /// and the last written page. A failed page read shows as `0xffffffff` with
    /// a companion of 65535; an unsupported header shows as a zero pair. Keep
    /// those sentinels apart from usable times.
    pub clock_pairs: [(u32, u32); 4],
}

impl DataRange {
    /// How many ring pages separate the acknowledged boundary from the write
    /// cursor — the strap-side backlog, in pages.
    ///
    /// Diagnostic only: a page holds several records, so this is a distance and
    /// not a number of rows waiting.
    pub fn pages_behind(&self) -> u32 {
        if self.capacity == 0 {
            return 0;
        }
        self.write_page
            .wrapping_sub(self.trim_boundary)
            .rem_euclid(self.capacity)
    }

    /// The largest clock-pair second that is neither sentinel nor zero.
    ///
    /// `None` when every pair is a sentinel or unset — which is the honest
    /// answer, and distinctly better than reporting 1970 or the year 2106.
    pub fn newest_unix(&self) -> Option<u32> {
        self.usable_clocks().max()
    }

    /// The smallest usable clock-pair second.
    ///
    /// The oldest stored record's time is *approximately* this, and no closer
    /// than that: the pairs are selected from four particular pages rather than
    /// scanned across the ring, and boundary A's full oldest/erase role is
    /// unresolved. Treat it as a window hint, not a retention guarantee.
    pub fn oldest_unix(&self) -> Option<u32> {
        self.usable_clocks().min()
    }

    /// The clock-pair seconds that are neither a failed-read sentinel
    /// (`0xffffffff`) nor an unsupported-header zero.
    fn usable_clocks(&self) -> impl Iterator<Item = u32> + '_ {
        self.clock_pairs
            .iter()
            .map(|&(secs, _)| secs)
            .filter(|&s| s != 0 && s != u32::MAX)
    }
}

// ── Records ──────────────────────────────────────────────────────────────────

/// One record from the historical offload.
///
/// The [`Self::Unknown`] arm is the point of the enum: an unrecognised layout
/// keeps its bytes, its version and its length rather than being forced through
/// a decoder that was written for a different record. A future firmware's new
/// layout then shows up as a preserved unknown instead of plausible-looking
/// wrong numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// WHOOP 5 R18, or WHOOP 4 v24 — the per-record biometric summary.
    Biometric(BiometricRecord),
    /// WHOOP 5 R20 — optical blocks.
    Optical(OpticalRecord),
    /// WHOOP 5 R21 — six-axis IMU.
    Imu(ImuRecord),
    /// WHOOP 5 R26 — compact optical window, reconstructed from deltas.
    CompactOptical(CompactOpticalRecord),
    /// WHOOP 5 R16/R17, MG — an ECG waveform.
    Ecg(EcgRecord),
    /// WHOOP 4 v25 — the 84-byte configuration-dependent layout. No per-second
    /// HR field is mapped, and none is inferred from the waveform.
    Whoop4V25(Whoop4V25Record),
    /// WHOOP 4 live type-43, the 1,917-byte variant — a one-second six-axis
    /// buffer with heart rate and intervals in its header.
    Whoop4RawImu(Whoop4RawImuRecord),
    /// WHOOP 4 live type-43, the 1,921-byte variant — one AC-coupled optical
    /// waveform.
    Whoop4RawOptical(Whoop4RawOpticalRecord),
    /// Packet type 51 or 52 — the dedicated IMU stream's count/span layout.
    ImuStream(ImuStreamRecord),
    /// A layout this crate does not decode, preserved whole.
    Unknown {
        /// Packet type the record arrived on.
        packet_type: u8,
        /// Layout selector (WHOOP 5 byte 9) or version byte (WHOOP 4 byte 5).
        version: u8,
        /// Complete frame length as received.
        length: usize,
        /// Why it was not decoded.
        note: &'static str,
        /// The complete frame.
        #[serde(serialize_with = "hex")]
        raw: Vec<u8>,
    },
}

/// A biometric summary record: heart rate, motion, temperature.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BiometricRecord {
    /// Which generation's layout produced this.
    pub family: DeviceFamily,
    /// Layout selector (18) or WHOOP 4 version byte (24).
    pub version: u8,
    /// The record's own index. **Not** a timestamp: do not fill time gaps by
    /// counting records, and do not assume rollover behaviour.
    pub record_index: Option<u32>,
    /// Unix seconds from the record's payload. A buffered record keeps its own
    /// time, so receive time must not be substituted.
    pub unix_seconds: u32,
    /// Heart rate and intervals.
    pub heart_rate: HeartRate,
    /// Skin temperature in °C, where the layout's conversion is documented and
    /// the value falls in the plausibility range.
    pub skin_temp_c: Option<f32>,
    /// Gravity-removed dynamic acceleration in g (R18 only).
    pub dynamic_accel_g: Option<f32>,
    /// Gravity vector in g.
    pub gravity_g: Option<[f32; 3]>,
    /// Cumulative step/motion counter as selected by the record.
    pub steps: Option<u16>,
    /// Activity class: 0 still, 1 walk, 2 run. Other input classes arrive as
    /// `0xFF` and are reported as `None` rather than squeezed into those three.
    pub activity: Option<u8>,
    /// The HR/R-R quality byte, raw. Its vocabulary is unresolved, so it is
    /// surfaced rather than interpreted.
    pub quality_flags: Option<u8>,
    /// The WHOOP 4 v24 raw sensor block, when this is that layout.
    ///
    /// Optional rather than flattened into the fields above, because these are
    /// uncalibrated ADC words belonging to one generation's record — putting
    /// them beside R18's converted values would imply they are the same kind of
    /// thing.
    pub whoop4_sensors: Option<Whoop4SensorBlock>,
}

/// R20 — five optical blocks, each with a shared configuration header and two
/// sample slots.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpticalRecord {
    /// Record index.
    pub record_index: u32,
    /// Unix seconds.
    pub unix_seconds: u32,
    /// Marker byte at offset 10, often `0x81`. Not a validity flag.
    pub marker: u8,
    /// The five blocks.
    pub blocks: Vec<OpticalBlock>,
}

/// One R20 block. The two slots share one header and remain **A/B** — not red,
/// infrared or green: no wavelength identity is established.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpticalBlock {
    /// Valid sample count per slot, 0–50. Counts above 50 are rejected, and a
    /// count of zero means no populated readings rather than fifty measured
    /// zeros.
    pub count: u8,
    /// Source/emitter selector A. Enum unresolved.
    pub source_a: u8,
    /// Source/emitter selector B. Enum unresolved.
    pub source_b: u8,
    /// Drive/configuration A. Physical unit unresolved.
    pub drive_a: u16,
    /// Drive/configuration B. Physical unit unresolved.
    pub drive_b: u16,
    /// Slot A samples, raw optical counts in −524288..=524287.
    pub slot_a: Vec<i32>,
    /// Slot B samples, raw optical counts.
    pub slot_b: Vec<i32>,
}

/// R21 — a one-second six-axis buffer.
///
/// The two counts are independent: the record's capacity permits an
/// accelerometer count and a gyroscope count that differ, and requiring them to
/// match would drop real samples.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImuRecord {
    /// Record index.
    pub record_index: u32,
    /// Base time, `seconds + fraction / 32768`.
    pub timestamp: f64,
    /// Unix seconds component of that base time.
    pub unix_seconds: u32,
    /// Accelerometer samples in g, under the retained `raw / 4096` convention —
    /// an applied interpretation, not a confirmed device parameter.
    pub accel_g: Vec<[f32; 3]>,
    /// Gyroscope samples in degrees/second, under the retained
    /// `raw * 2000 / 32768` convention.
    pub gyro_dps: Vec<[f32; 3]>,
}

/// R26 — an optical base value plus 24 adjacent deltas.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompactOpticalRecord {
    /// Record index.
    pub record_index: u32,
    /// Unix seconds.
    pub unix_seconds: u32,
    /// Burst counter. Wraps modulo 65536, and is not the record index, a
    /// channel selector or a ring-slot index — zero remains a valid value.
    pub burst: u16,
    /// The 25 reconstructed samples, `s[0] = base`, `s[i+1] = s[i] + delta[i]`.
    ///
    /// Each transmitted delta was clipped to `i16`, so this reconstructs the
    /// transmitted approximation; a larger original adjacent jump cannot be
    /// recovered, and later deltas do not repair it.
    pub samples: Vec<i64>,
    /// Summary motion value. Physical conversion not established.
    pub motion: f32,
    /// On-wrist-related summary byte, raw.
    pub wrist_byte: u8,
}

/// R16 or R17 — an ECG waveform with its status header. MG hardware.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EcgRecord {
    /// 16 for raw (18-bit, 500 slots), 17 for filtered (i16, 100 slots).
    pub layout: u8,
    /// Shared record sequence. Raw and filtered outputs from one acquisition
    /// pass can share it, so it is not a deduplication key on its own.
    pub sequence: u32,
    /// Timestamp main word. Units unresolved — reported, not converted.
    pub timestamp_word: u32,
    /// Quality code at byte 21. Observed values are 0–3; thresholds and
    /// vocabulary are unresolved, and this is not a clinical scale.
    pub quality: u8,
    /// Declared waveform count, validated against the layout's capacity.
    pub declared_count: u16,
    /// Waveform samples, as many as the declared count. Signed, uncalibrated:
    /// no volts per count is established, so these are not millivolts.
    pub samples: Vec<i32>,
    /// Per-sample flag 6, for raw records.
    pub sample_flags: Vec<bool>,
    /// The packed status region at 21–33, kept whole.
    ///
    /// Thirteen bytes of classifier codes, presence bits and HR/HRV-adjacent
    /// values whose vocabulary is unresolved. Preserved rather than expanded
    /// into named booleans, because naming them would be inventing a contract.
    #[serde(serialize_with = "hex")]
    pub status: Vec<u8>,
    /// Lead-off diagnostics, for raw records only.
    pub lead_off: Option<EcgLeadOff>,
}

/// The R16 lead-off diagnostic arrays.
///
/// Fixed regions at 1,534–1,578 that do **not** move when the waveform count is
/// smaller — a decoder locating them with `header + count × width` would read
/// the wrong bytes on every partly-populated record.
///
/// Flag 7 of each waveform sample comes from this slower contact/lead stream,
/// so its timing must not be read as an independently sampled 500-entry
/// channel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EcgLeadOff {
    /// Declared diagnostic count. At most the eleven available slots.
    pub count: u8,
    /// Eleven fixed I-channel halfwords.
    pub i_channel: Vec<u16>,
    /// Eleven fixed Q-channel halfwords.
    pub q_channel: Vec<u16>,
}

impl Record {
    /// A one-line description, for a log or a terminal row.
    ///
    /// On the type rather than in each front end. There were three copies of
    /// this match — CLI, TUI and the browser example — and adding three
    /// variants broke all three builds at once. One implementation means the
    /// compiler points at exactly one place when the enum grows.
    ///
    /// An undecoded record says so out loud. A summary that skipped it would
    /// make a strap emitting something this build cannot read look like a strap
    /// emitting nothing.
    pub fn summary(&self) -> String {
        match self {
            Self::Biometric(r) => format!(
                "biometric v{} at {} — {} bpm, {} interval(s){}",
                r.version,
                r.unix_seconds,
                r.heart_rate.bpm,
                r.heart_rate.rr_ms.len(),
                r.skin_temp_c
                    .map(|c| format!(", {c:.2} °C"))
                    .unwrap_or_default()
            ),
            Self::Optical(r) => {
                let samples: usize = r
                    .blocks
                    .iter()
                    .map(|b| b.slot_a.len() + b.slot_b.len())
                    .sum();
                format!(
                    "optical at {} — {} block(s), {samples} sample(s)",
                    r.unix_seconds,
                    r.blocks.len()
                )
            }
            Self::Imu(r) => format!(
                "imu at {:.2} — {} accel, {} gyro",
                r.timestamp,
                r.accel_g.len(),
                r.gyro_dps.len()
            ),
            Self::CompactOptical(r) => format!(
                "compact optical at {} — burst {}, {} sample(s)",
                r.unix_seconds,
                r.burst,
                r.samples.len()
            ),
            Self::Ecg(r) => format!(
                "ecg R{} — {} sample(s), quality {}",
                r.layout, r.declared_count, r.quality
            ),
            Self::Whoop4V25(r) => {
                format!("whoop4 v25 at {} — movement only", r.unix_seconds)
            }
            Self::Whoop4RawImu(r) => format!(
                "whoop4 raw imu at {} — {} bpm, {} accel, {} gyro",
                r.unix_seconds,
                r.heart_rate.bpm,
                r.accel_g.len(),
                r.gyro_dps.len()
            ),
            Self::Whoop4RawOptical(r) => format!(
                "whoop4 raw optical at {} — {} sample(s)",
                r.unix_seconds,
                r.samples.len()
            ),
            Self::ImuStream(r) => format!(
                "imu stream type {} — {} accel, {} gyro (raw words)",
                r.packet_type, r.accel_count, r.gyro_count
            ),
            Self::Unknown {
                version,
                length,
                note,
                ..
            } => format!("UNDECODED layout {version}, {length} bytes — {note}"),
        }
    }

    /// Whether this record kept its bytes instead of being decoded.
    ///
    /// Worth counting at the end of a session: a run that found something the
    /// decoders do not cover is a different outcome from a clean one, and
    /// should not look the same.
    pub fn is_undecoded(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// The record's own timestamp in Unix seconds, where its layout has one.
    ///
    /// `None` for the IMU streams, whose timestamp fields are unspecified, and
    /// for undecoded records — rather than a zero, which would sort as 1970.
    pub fn unix_seconds(&self) -> Option<u32> {
        match self {
            Self::Biometric(r) => Some(r.unix_seconds),
            Self::Optical(r) => Some(r.unix_seconds),
            Self::Imu(r) => Some(r.unix_seconds),
            Self::CompactOptical(r) => Some(r.unix_seconds),
            Self::Whoop4V25(r) => Some(r.unix_seconds),
            Self::Whoop4RawImu(r) => Some(r.unix_seconds),
            Self::Whoop4RawOptical(r) => Some(r.unix_seconds),
            // ECG carries a timestamp word whose units are unresolved, so it is
            // not presented as seconds.
            Self::Ecg(_) | Self::ImuStream(_) | Self::Unknown { .. } => None,
        }
    }
}

/// The WHOOP 4 v24 raw sensor block.
///
/// Every field is a **raw ADC word**. WHOOP computes SpO₂, skin temperature and
/// respiratory rate from these server-side, so nothing here is converted: there
/// is no documented client-side conversion to apply, and borrowing the WHOOP 5
/// R18 scales would produce numbers in the right ballpark for the wrong reason.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Whoop4SensorBlock {
    /// Green-LED optical scalar. The label comes from the legacy layout and is
    /// not calibrated wavelength proof.
    pub ppg_green: u16,
    /// Red/IR-labelled optical scalar, same caveat.
    pub ppg_red_ir: u16,
    /// Skin-contact state. Zero is the retained off-wrist interpretation.
    pub skin_contact: u8,
    /// A second gravity triplet in g. Its physical distinction from the first
    /// is unresolved, so both are kept.
    pub gravity2_g: [f32; 3],
    /// Red channel of the SpO₂-adjacent pair. **Not** a percentage.
    pub spo2_red: u16,
    /// IR channel of the same pair.
    pub spo2_ir: u16,
    /// Skin-temperature raw ADC. Not centidegrees.
    pub skin_temp_raw: u16,
    /// Ambient-light raw ADC, uncalibrated.
    pub ambient: u16,
    /// The two LED-drive words. Units and channel assignment unresolved.
    pub led_drive: [u16; 2],
    /// Respiration-adjacent raw word. Not breaths per minute.
    pub resp_raw: u16,
    /// Signal-quality word. Scale and polarity unresolved.
    pub signal_quality: u16,
}

/// WHOOP 4 live type-43, the 1,917-byte variant.
///
/// A capture-backed layout: 100 signed `i16le` samples per axis at roughly one
/// packet per second, with heart rate and up to four intervals in the header.
/// The scales are commonly applied interpretations — `1/4096` g matched a
/// gravity-sphere fit and `2000/32768` °/s matched bounded 720° rotations — not
/// confirmed device parameters.
///
/// Byte ranges 31–88, 689–691 and the tail from 1,292 are unmapped and are
/// deliberately not surfaced as measurements.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Whoop4RawImuRecord {
    /// Device-clock seconds from the record header.
    pub unix_seconds: u32,
    /// Heart rate and intervals carried alongside the motion buffer.
    pub heart_rate: HeartRate,
    /// Accelerometer samples in g.
    pub accel_g: Vec<[f32; 3]>,
    /// Gyroscope samples in degrees/second.
    pub gyro_dps: Vec<[f32; 3]>,
}

/// WHOOP 4 live type-43, the 1,921-byte variant.
///
/// One AC-coupled, DC-removed optical waveform — **not** four interleaved
/// wavelength channels. On-finger it showed a clean ~1 Hz pulsatile signal and
/// flattened in air, which supports an optical role without assigning red, IR
/// or ambient identity to it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Whoop4RawOpticalRecord {
    /// Device-clock seconds from the record header.
    pub unix_seconds: u32,
    /// Up to 419 signed 24-bit samples at roughly 437 Hz.
    pub samples: Vec<i32>,
    /// The fourth byte of each four-byte stride — status or exposure, unknown.
    ///
    /// Retained rather than skipped: the contract says to keep it, and a
    /// waveform decoder that discarded it would make the unknown unrecoverable.
    pub aux: Vec<u8>,
    /// The optical configuration header at 15–41, kept raw.
    #[serde(serialize_with = "hex")]
    pub config: Vec<u8>,
}

/// Packet type 51 or 52 — the dedicated IMU stream.
///
/// Counts at offsets 24 and 26 decide where every array starts, so the layout
/// is **computed, not fixed**: accelerometer X at 28, then Y and Z at
/// `28 + 2A` and `28 + 4A`, then the gyroscope triplet from `28 + 6A` strided
/// by `G`. The whole calculated span is validated against the payload boundary
/// before anything is read.
///
/// Samples are kept as raw 16-bit words. Signedness, physical scale, cadence
/// and timestamp fields are all unspecified for these types — and this layout
/// is explicitly **not** R21, so R21's `raw/4096` g is not applied to it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImuStreamRecord {
    /// 51 for the realtime stream, 52 for the historical one.
    pub packet_type: u8,
    /// Declared accelerometer sample count, `A`.
    pub accel_count: u16,
    /// Declared gyroscope sample count, `G`.
    pub gyro_count: u16,
    /// Accelerometer X, Y, Z as raw words.
    pub accel_raw: [Vec<u16>; 3],
    /// Gyroscope X, Y, Z as raw words.
    pub gyro_raw: [Vec<u16>; 3],
}

/// WHOOP 4 v25 — the 84-byte layout 41.17.6.0 emits depending on configuration.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Whoop4V25Record {
    /// Unix seconds.
    pub unix_seconds: u32,
    /// The three signed movement values at 73/75/77, raw.
    pub movement_raw: [i16; 3],
    /// Those values divided by 16384, the observation-based gravity reading.
    /// Not a confirmed device parameter.
    pub movement_g: [f32; 3],
}

/// The WHOOP 5 battery-pack record — command 151, `BATTERY_PACK_INFO`.
///
/// Cached, not a live query: a successful reply is not a freshness guarantee,
/// and it reports success even when no pack is present.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatteryPack {
    /// Whether a pack is reported present.
    pub present: bool,
    /// The six-byte address/identifier block, raw.
    #[serde(serialize_with = "hex")]
    pub address: Vec<u8>,
    /// The serial/text block, handled as bounded bytes and trimmed of NULs.
    pub serial: String,
    /// The raw charge value.
    ///
    /// **Not** converted. The strap-side message contract does not establish a
    /// physical percentage scale for this field, so dividing it by ten — as
    /// some clients do for display — is a convention, not a reading.
    pub charge_raw: u16,
}

/// The WHOOP 4 extended battery record — command 98, from cache.
///
/// Announced by event 63. An empty cache returns failure rather than zeros, so
/// a decoded record means the strap had something stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExtendedBattery {
    /// Pack voltage in millivolts — the third `u16le`, at body offset 5.
    pub millivolts: u16,
    /// The complete 25-byte record, because only one field is mapped.
    ///
    /// Keeping the rest raw is the point: `u8`, seven `u16le`, two `u32le` and
    /// a trailing `u16le` are structurally known and semantically not, and none
    /// of them establishes cell health, remaining runtime or temperature
    /// compensation.
    #[serde(serialize_with = "hex")]
    pub raw: Vec<u8>,
}

/// The advertised name, from WHOOP 5 command 141's fixed 19-byte reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdvertisingName {
    /// The name, with its terminator excluded.
    pub name: String,
    /// Source/status byte. 1 identifies a custom name; 2 covers fallback, empty
    /// storage **and** a storage-read failure, so it does not diagnose which.
    pub status: u8,
    /// Whether the status byte reports a custom name.
    pub custom: bool,
}

/// A body-location read — command 84.
///
/// Diagnostic only. It never feeds wear detection, sleep gating or scoring, on
/// either generation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BodyLocationStatus {
    /// Decoded location, for the WHOOP 4 four-byte record.
    ///
    /// `None` on WHOOP 5, whose reply is `[1, 0, 255, on_body]`: the two middle
    /// bytes are fixed placeholders rather than a measured location and
    /// confidence, so there is nothing to decode.
    pub location: Option<crate::protocol::BodyLocation>,
    /// Cached on-body status, where the reply carries one.
    pub on_body: Option<bool>,
    /// The complete record, since most of it is unresolved on both families.
    #[serde(serialize_with = "hex")]
    pub raw: Vec<u8>,
}

/// One named configuration value — the 65-byte reply to command 121 or 128.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigValue {
    /// The key the strap echoed back, normalised and NUL-trimmed.
    ///
    /// Check this against the key you asked for before using the value: a
    /// non-NUL 32-byte key is normalised and will not echo exactly.
    pub key: String,
    /// The formatted value text — `0`, `1`, `2`, an unsigned decimal, or a
    /// decimal with one fractional digit, depending on the key's type.
    pub value: String,
    /// Whether the value field was entirely zero.
    ///
    /// Worth distinguishing: an unreadable stored value is returned as zeros
    /// **with result 1**, so a zero readback can be a fallback rather than a
    /// saved value or a factory default.
    pub zero_filled: bool,
}

/// One entry from a configuration enumeration walk — command 116 or 118.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigEntry {
    /// Cursor index. 255 is the one terminal value a strap has served
    /// unambiguously.
    pub index: u8,
    /// The strap's validity byte.
    ///
    /// False **alone** is not a terminator: it could equally mark an empty or
    /// retired slot with the list continuing past it, and the two readings have
    /// never been separated on the wire. A walk records it, steps over it, and
    /// asks again.
    pub valid: bool,
    /// The bounded name, when one decoded as printable ASCII.
    pub key: Option<String>,
}

/// Why an enumeration walk stopped.
///
/// Named rather than collapsed into "finished", because only the first is the
/// strap saying so; the rest are this side's limits, and a report that did not
/// distinguish them would present a client cap as a complete key list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkStop {
    /// The strap served index 255 — its unambiguous end marker.
    EndMarker,
    /// Eight consecutive invalid entries, with the index still advancing.
    EmptyRun,
    /// An invalid entry repeated the previous index: the cursor is parked.
    ParkedCursor,
    /// The announced count plus four replies.
    AnnouncedCount,
    /// The hard client cap of 128 replies.
    HardCap,
    /// A reply failed to arrive, failed a checksum, or was too short.
    ProtocolError,
}

/// The result of one enumeration walk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigWalk {
    /// The count the strap announced, from command 115 or 117.
    pub announced: u8,
    /// Every entry served, in order, including invalid slots.
    pub entries: Vec<ConfigEntry>,
    /// Why the walk ended.
    pub stop: WalkStop,
}

/// A stored alarm, from WHOOP 5 command 67's 21-byte record.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AlarmRecord {
    /// Alarm ID, 1–6.
    pub id: u8,
    /// Epoch seconds. An all-zero time can mean cleared or unset state **or** a
    /// read fallback, because GET can return zeros after a read failure while
    /// still reporting success.
    pub unix_seconds: u32,
    /// Fractional ticks in the clock's 1/32768-second convention.
    pub ticks: u16,
    /// The stored haptic pattern.
    pub effects: [u8; 8],
    /// Effect-loop control.
    pub loop_control: u16,
    /// Overall repeat count.
    pub repeats: u8,
    /// Duration.
    pub duration: u8,
    /// 0 ordinary pattern, 1 staged crescendo.
    pub crescendo: u8,
}

impl AlarmRecord {
    /// Whether this slot holds a time at all.
    ///
    /// A convenience, not a verdict: see the caveat on [`Self::unix_seconds`].
    pub fn is_set(&self) -> bool {
        self.unix_seconds != 0
    }
}

// ── Rejected frames ──────────────────────────────────────────────────────────

/// A frame that failed the envelope check, kept rather than dropped.
///
/// A capture exists to hold the frames nothing else can read, and a link
/// problem is diagnosable only from the bytes that showed it. The counters on
/// [`crate::protocol::Reassembler`] cover the byte runs that never became
/// frames; this covers the frames that did and then failed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RejectedFrame {
    /// Which rule failed first.
    pub reason: FrameRejectReason,
    /// The packet type, if the frame was long enough to carry one.
    pub packet_type: Option<u8>,
    /// The frame as received.
    #[serde(serialize_with = "hex")]
    pub raw: Vec<u8>,
}

// ── The event enum ───────────────────────────────────────────────────────────

/// Everything [`crate::whoop_client::WhoopClient`] emits.
///
/// Consumers receive these through the `mpsc::Receiver` returned by
/// [`crate::whoop_client::WhoopClient::connect`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WhoopEvent {
    /// The BLE link is up, GATT is discovered and the family's handshake has
    /// been written.
    Connected {
        /// Advertised name.
        name: String,
        /// Which wire profile this connection speaks.
        family: DeviceFamily,
    },
    /// The link was lost. The channel closes after this; no further events.
    Disconnected,
    /// A live or historical heart-rate observation.
    HeartRate(HeartRate),
    /// A battery observation.
    Battery(Battery),
    /// A strap-pushed event. Any of them means "I may have new data".
    Event(StrapEvent),
    /// A reply to a command. Also delivered to whoever awaited that request —
    /// this copy is for logging and for replies nobody asked for.
    CommandResponse(CommandResponse),
    /// A decoded identity hello.
    Hello(Hello),
    /// The strap's stored range and ring cursors.
    DataRange(DataRange),
    /// An offload control message.
    HistoryMeta(HistoryMeta),
    /// One record from the offload.
    Record(Record),
    /// Firmware log text.
    ConsoleLog(String),
    /// A live raw-sensor frame whose layout this crate does not decode, kept
    /// whole. Live ECG and the WHOOP 4 1,917/1,921-byte shapes arrive here when
    /// they are not separately decoded.
    RawSensor {
        /// Packet type.
        packet_type: u8,
        /// Layout or subtype byte.
        version: u8,
        /// The complete frame.
        #[serde(serialize_with = "hex")]
        raw: Vec<u8>,
    },
    /// A frame that did not pass the envelope check.
    Rejected(RejectedFrame),
}

// ── Serialisation helpers ────────────────────────────────────────────────────

/// Serialise raw bytes as a lowercase hex string.
///
/// Shares [`crate::store::hex`] so a field written into a capture and the same
/// field printed by a tool cannot disagree about case or padding.
fn hex<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&crate::store::hex(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_behind_wraps_around_the_ring() {
        let mut range = DataRange {
            boundary: 0,
            read_page: 0,
            write_page: 10,
            trim_boundary: 4,
            wrap_state: 0,
            capacity: 1024,
            record_estimate: 0,
            clock_pairs: [(0, 0); 4],
        };
        assert_eq!(range.pages_behind(), 6);
        // Across a wrap the distance stays inside the capacity.
        range.write_page = 3;
        range.trim_boundary = 1020;
        assert_eq!(range.pages_behind(), 7);
        // A zero capacity is a strap that reported nothing, not a divide by it.
        range.capacity = 0;
        assert_eq!(range.pages_behind(), 0);
    }

    #[test]
    fn newest_unix_ignores_sentinels_and_zeros() {
        let range = DataRange {
            boundary: 0,
            read_page: 0,
            write_page: 0,
            trim_boundary: 0,
            wrap_state: 0,
            capacity: 1,
            record_estimate: 0,
            // A failed page read, an unsupported header, and two real times.
            clock_pairs: [
                (u32::MAX, 65535),
                (0, 0),
                (1_700_000_000, 100),
                (1_700_000_500, 200),
            ],
        };
        assert_eq!(range.newest_unix(), Some(1_700_000_500));

        // Both ends come from the same filtered set.
        assert_eq!(range.oldest_unix(), Some(1_700_000_000));

        let all_sentinel = DataRange {
            clock_pairs: [(u32::MAX, 65535), (0, 0), (0, 0), (0, 0)],
            ..range
        };
        // Better no answer than 1970 or 2106.
        assert_eq!(all_sentinel.newest_unix(), None);
        assert_eq!(all_sentinel.oldest_unix(), None);
    }

    #[test]
    fn raw_bytes_serialise_as_hex() {
        let rejected = WhoopEvent::Rejected(RejectedFrame {
            reason: FrameRejectReason::PayloadChecksum,
            packet_type: Some(47),
            raw: vec![0xAA, 0x01, 0xFF],
        });
        let json = serde_json::to_string(&rejected).unwrap();
        assert!(json.contains(r#""raw":"aa01ff""#), "{json}");
        assert!(json.contains(r#""reason":"payload_checksum""#), "{json}");
    }
}
