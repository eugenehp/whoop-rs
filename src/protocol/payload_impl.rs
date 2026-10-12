//! Request bodies for the commands in [`super::Command`].

/// `[0x00]` — the body most read commands take.
pub const ZERO: [u8; 1] = [0x00];
/// `[0x01]` — revision 1, the lead byte of every revisioned WHOOP 5 body.
pub const REVISION_1: [u8; 1] = [0x01];

/// A one-byte on/off argument.
pub fn toggle(on: bool) -> Vec<u8> {
    vec![if on { 0x01 } else { 0x00 }]
}

/// WHOOP 4 `SET_CLOCK` — `[secs u32le][0,0,0,0]`, eight bytes.
///
/// One of two forms observed in use outside the documented 41.17.6.0
/// command set. On some straps one of the two latches, so read the clock
/// back rather than trusting the acknowledgement.
pub fn set_clock_whoop4(unix_seconds: u32) -> Vec<u8> {
    let mut body = unix_seconds.to_le_bytes().to_vec();
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

/// WHOOP 4 `SET_CLOCK`, the nine-byte form — `[secs u32le][0,0,0,0,0]`.
pub fn set_clock_whoop4_legacy(unix_seconds: u32) -> Vec<u8> {
    let mut body = unix_seconds.to_le_bytes().to_vec();
    body.extend_from_slice(&[0, 0, 0, 0, 0]);
    body
}

/// WHOOP 5 `SET_CLOCK` (146) — revision 1, `u32` seconds, `u16` fractional
/// ticks in units of 1/32768 s.
///
/// Ticks are clamped to the canonical 0–32767; behaviour for non-canonical
/// values is not specified. The stored clock keeps hundredths, so a
/// set/get round trip loses precision by design.
pub fn set_clock_whoop5(unix_seconds: u32, ticks: u16) -> Vec<u8> {
    let mut body = vec![0x01];
    body.extend_from_slice(&unix_seconds.to_le_bytes());
    body.extend_from_slice(&ticks.min(32767).to_le_bytes());
    body
}

/// WHOOP 4 `SET_ALARM_TIME` — `[0x01] + epoch u32le + subseconds u16le +
/// [0,0]`, nine bytes.
///
/// The first seven bytes are the semantic body; the trailing two are not
/// evaluated. A seven-byte request has been acknowledged without producing
/// a vibration, so the subsecond field — not the body length — is what
/// carries meaning.
pub fn set_alarm_whoop4(epoch_seconds: u32) -> Vec<u8> {
    let mut body = vec![0x01];
    body.extend_from_slice(&epoch_seconds.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&[0x00, 0x00]);
    body
}

/// The canonical WHOOP waveform-effect pair, shared by the notification
/// buzz and the wake alarm.
const WAVEFORM_EFFECTS: [u8; 8] = [47, 152, 0, 0, 0, 0, 0, 0];

/// WHOOP 5 `RUN_HAPTIC_PATTERN_MAVERICK` (19) — a 12-byte body:
/// `[rev 1][waveform 8][loop control u16le = 0][overall loop]`.
///
/// `overall loop` counts the repeats that **follow** the first pulse, not
/// the total, so `loops` is written as `loops - 1`: writing 3 produces four
/// buzzes. Clamped to 1–8, eight being the largest value any captured body
/// evidences.
pub fn maverick_buzz(loops: u8) -> Vec<u8> {
    let mut body = vec![0x01];
    body.extend_from_slice(&WAVEFORM_EFFECTS);
    body.extend_from_slice(&[0x00, 0x00, loops.clamp(1, 8) - 1]);
    body
}

/// WHOOP 5 `SET_ALARM_TIME` (66) revision 4 — 20 bytes. The strap arms its
/// own RTC and fires the wake haptic itself.
///
/// `[0x04][alarm id][epoch u32le][subseconds u16le][8 effects][loop control
/// u16le][overall loop = 7][duration = 30]`, subseconds being
/// `(ms % 1000) * 32768 / 1000` in 1/32768-second fixed point.
pub fn set_alarm_whoop5(wake_epoch_ms: i64, alarm_id: u8) -> Vec<u8> {
    let seconds = (wake_epoch_ms / 1000).clamp(0, u32::MAX as i64) as u32;
    let subseconds =
        ((wake_epoch_ms.rem_euclid(1000)) * 32768 / 1000).clamp(0, u16::MAX as i64) as u16;
    let mut body = vec![0x04, alarm_id];
    body.extend_from_slice(&seconds.to_le_bytes());
    body.extend_from_slice(&subseconds.to_le_bytes());
    body.extend_from_slice(&WAVEFORM_EFFECTS);
    body.extend_from_slice(&[0x00, 0x00, 0x07, 30]);
    body
}

/// WHOOP 5 `DISABLE_ALARM` (69) revision 2 — `[0x02, 0xFF]`.
pub fn disable_alarm_whoop5() -> Vec<u8> {
    vec![0x02, 0xFF]
}

/// WHOOP 5 `RUN_ALARM` (68) revision 2 — `[0x02, alarm id]`.
pub fn run_alarm_whoop5(alarm_id: u8) -> Vec<u8> {
    vec![0x02, alarm_id]
}

/// WHOOP 4 `RUN_HAPTICS_PATTERN` (79) — `[pattern, loops, 0, 0, 0]`.
pub fn whoop4_haptics(pattern: u8, loops: u8) -> Vec<u8> {
    vec![pattern, loops, 0, 0, 0]
}

/// `HISTORICAL_DATA_RESULT` (23) — `[0x01] + the chunk's verbatim 8-byte end
/// block`.
///
/// The block is echoed, never reconstructed from decoded timestamps: its
/// first word is a read-page position modulo ring capacity and its second
/// reflects write-wrap state, and a replacement built from an independently
/// saved cursor is not the same token. Send this **only after** the chunk's
/// records and the progress cursor are durably stored — a successful
/// acknowledgement can let the strap reclaim that history.
pub fn history_ack(end_block: &[u8; 8]) -> Vec<u8> {
    let mut body = vec![0x01];
    body.extend_from_slice(end_block);
    body
}

/// An ECG-family body — `[revision 1, arg]`.
pub fn ecg(arg: u8) -> Vec<u8> {
    vec![0x01, arg]
}

// ── Named configuration ─────────────────────────────────────────────────

/// Width of the key and value fields in a named configuration record.
pub const NAMED_FIELD: usize = 32;

/// Pack a name into a 32-byte NUL-terminated field.
///
/// At most 31 meaningful bytes, because the 32nd has to be the terminator;
/// a longer name is truncated there rather than overflowing into the next
/// field. Unused bytes are zero-filled, which the contract requires —
/// leaving them uninitialised is how a key echo stops matching.
fn named_field(name: &str) -> [u8; NAMED_FIELD] {
    let mut field = [0u8; NAMED_FIELD];
    let bytes = name.as_bytes();
    let n = bytes.len().min(NAMED_FIELD - 1);
    field[..n].copy_from_slice(&bytes[..n]);
    field
}

/// A named read — `[revision 1] + key[32]`, 33 bytes.
///
/// Used by both command 121 (device values) and 128 (feature flags); the
/// namespace is the command, not the body.
pub fn named_read(key: &str) -> Vec<u8> {
    let mut body = vec![0x01];
    body.extend_from_slice(&named_field(key));
    body
}

/// A named write — `[revision 1] + key[32] + value[32]`, 65 bytes.
///
/// The value is ASCII text matching the key's declared type: there is no
/// binary form and no type tag. A shorter historical request with one value
/// character and a little padding does **not** establish the contract, and
/// handling of truncated bodies is unresolved — so this always sends the
/// full width.
pub fn named_write(key: &str, value: &str) -> Vec<u8> {
    let mut body = vec![0x01];
    body.extend_from_slice(&named_field(key));
    body.extend_from_slice(&named_field(value));
    body
}

// ── Service commands ────────────────────────────────────────────────────

/// WHOOP 5 `SET_ADVERTISING_NAME` (140) — `[revision 1, length, name…]`.
///
/// The strap stores 0–15 bytes. Longer input is truncated here rather than
/// sent with a length the body cannot satisfy. No character-set validation
/// is established, and radio visibility can lag the reply.
pub fn set_advertising_name_whoop5(name: &str) -> Vec<u8> {
    let bytes = name.as_bytes();
    let n = bytes.len().min(15);
    let mut body = vec![0x01, n as u8];
    body.extend_from_slice(&bytes[..n]);
    body
}

/// WHOOP 4 `SET_ADVERTISING_NAME_HARVARD` (77) — two reserved bytes, the
/// name, then NUL.
///
/// Up to 24 client bytes are accepted here; the documented device field
/// retains at most 15 and forces its last byte to NUL, so what comes back
/// from a read can be shorter than what was sent. The rename applies on
/// reboot, which is itself a side effect worth knowing about.
pub fn set_advertising_name_whoop4(name: &str) -> Vec<u8> {
    let bytes = name.as_bytes();
    let n = bytes.len().min(24);
    let mut body = vec![0x00, 0x00];
    body.extend_from_slice(&bytes[..n]);
    body.push(0x00);
    body
}

/// Command 148 — `[revision 1, override]`.
///
/// `true` forces the worn state and **disables normal wear detection**;
/// `false` restores it. Restoring does not promise an immediate off-body
/// report, so a reading taken right afterwards is not evidence either way.
pub fn wear_detection_override(force_worn: bool) -> Vec<u8> {
    vec![0x01, u8::from(force_worn)]
}

/// Command 149 — `[revision 1, enabled]`. Accepts only 0 and 1.
///
/// This updates one option inside a shared stored settings record, and if
/// reading that record fails the strap starts from defaults before writing.
/// Success therefore does not prove the other stored options survived.
pub fn led_accessibility(enabled: bool) -> Vec<u8> {
    vec![0x01, u8::from(enabled)]
}

/// A revision-1-only request body, for the reads that take nothing else:
/// command 84 (body location), 141 (advertising name), 80, 122 on WHOOP 5.
pub fn revision_only() -> Vec<u8> {
    vec![0x01]
}

/// `STOP_HAPTICS` (122) — the legacy WHOOP 4 request body is `[0x00]`,
/// the WHOOP 5 one is revision 1.
///
/// Stopping is asynchronous: the initial pending reply and the final result
/// have different success conditions, and stopping the current effect does
/// not disable stored alarms.
pub fn stop_haptics(family: super::DeviceFamily) -> Vec<u8> {
    match family {
        super::DeviceFamily::Whoop4 => vec![0x00],
        super::DeviceFamily::Whoop5 => vec![0x01],
    }
}

/// WHOOP 4 `RUN_HAPTICS_PATTERN` (79) with the one preset observed to work.
///
/// `02 03 00 00 00` — preset 2, three loops. The reliable one-shot path
/// pairs this with `RUN_ALARM` (68) revision 1: a bare preset write was
/// reported ignored in one run. That pairing is an observation, not proof
/// that firmware requires both.
pub fn whoop4_haptics_proven() -> Vec<u8> {
    vec![0x02, 0x03, 0x00, 0x00, 0x00]
}

// ── WHOOP 5 alarms ──────────────────────────────────────────────────────

/// A haptic pattern, as carried by both the alarm record and the buzz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HapticPattern {
    /// Eight waveform effect bytes. Each must be at most 251.
    pub effects: [u8; 8],
    /// Effect-loop control.
    pub loop_control: u16,
    /// Overall repeat count. Must be below 8, and counts the repeats
    /// **after** the first pulse rather than the total.
    pub repeats: u8,
    /// Duration. When `repeats` is 7 this must be 30–120 inclusive; that
    /// interval is not imposed for 0–6.
    pub duration: u8,
    /// 0 ordinary pattern, 1 staged crescendo.
    pub crescendo: u8,
}

impl Default for HapticPattern {
    /// The canonical WHOOP waveform pair with the long wake train.
    ///
    /// Effects `[47, 152, …]` are the pair both the notification buzz and
    /// the wake alarm use; repeats 7 with duration 30 is the only
    /// combination any captured alarm body evidences.
    fn default() -> Self {
        Self {
            effects: [47, 152, 0, 0, 0, 0, 0, 0],
            loop_control: 0,
            repeats: 7,
            duration: 30,
            crescendo: 0,
        }
    }
}

impl HapticPattern {
    /// Check the documented acceptance rules before sending.
    ///
    /// Worth doing locally: a validation failure returns outer failure with
    /// a detail byte, and a rejected request has still cost a round trip
    /// and possibly stored half of itself.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.effects.iter().any(|&e| e > 251) {
            return Err("every effect byte must be at most 251");
        }
        if self.repeats >= 8 {
            return Err("repeat count must be below 8");
        }
        if self.crescendo > 1 {
            return Err("crescendo must be 0 or 1");
        }
        if self.repeats == 7 && !(30..=120).contains(&self.duration) {
            return Err("with 7 repeats the duration must be 30–120");
        }
        Ok(())
    }
}

/// WHOOP 5 `SET_ALARM_TIME` (66) revision 4 — the 21-byte record.
///
/// | Offset | Field |
/// |---:|---|
/// | 0 | revision 4 |
/// | 1 | alarm ID, 1–6 |
/// | 2 | epoch seconds |
/// | 6 | fractional ticks, 1/32768 s |
/// | 8 | eight waveform effects |
/// | 16 | effect-loop control |
/// | 18 | overall repeat count |
/// | 19 | duration |
/// | 20 | crescendo |
///
/// Crescendo is supplied explicitly rather than left to framing padding.
/// The two produce the same bytes, but a body that relies on padding to
/// mean zero cannot distinguish "ordinary pattern" from "field absent",
/// and padding is not a semantic argument.
///
/// The strap requires seconds **strictly later** than its current seconds —
/// a larger fractional value inside the same second is not enough.
pub fn alarm_set_whoop5(id: u8, wake_epoch_ms: i64, pattern: HapticPattern) -> Vec<u8> {
    let seconds = (wake_epoch_ms / 1000).clamp(0, u32::MAX as i64) as u32;
    let ticks = ((wake_epoch_ms.rem_euclid(1000)) * 32768 / 1000).clamp(0, u16::MAX as i64) as u16;
    let mut body = vec![0x04, id];
    body.extend_from_slice(&seconds.to_le_bytes());
    body.extend_from_slice(&ticks.to_le_bytes());
    body.extend_from_slice(&pattern.effects);
    body.extend_from_slice(&pattern.loop_control.to_le_bytes());
    body.push(pattern.repeats);
    body.push(pattern.duration);
    body.push(pattern.crescendo);
    body
}

/// WHOOP 5 `GET_ALARM_TIME` (67) — `[revision 4, ID]`.
pub fn alarm_get_whoop5(id: u8) -> Vec<u8> {
    vec![0x04, id]
}

/// WHOOP 5 `RUN_ALARM` (68) — `[revision 2, ID]`.
///
/// Requires an existing non-zero stored time and does **not** apply the
/// future-time check. It also clears the selected record, so it is not a
/// non-destructive preview of a schedule.
pub fn alarm_run_whoop5(id: u8) -> Vec<u8> {
    vec![0x02, id]
}

/// WHOOP 5 `DISABLE_ALARM` (69) — `[revision 2, ID]`, or `0xFF` for all six.
///
/// An all-ID disable can clear some slots and still return failure when at
/// least one was not cleared, so read back individual slots when their
/// state matters.
pub fn alarm_disable_whoop5(id: u8) -> Vec<u8> {
    vec![0x02, id]
}

/// Every alarm slot — `[revision 2, 0xFF]`.
pub fn alarm_disable_all_whoop5() -> Vec<u8> {
    vec![0x02, 0xFF]
}
