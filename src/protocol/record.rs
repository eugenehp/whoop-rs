//! Record layout selectors, event names and physical conversions.

// ── Events ───────────────────────────────────────────────────────────────────

/// Selected strap event numbers from a type-48 frame.
pub mod event {
    /// Dense pushed battery observation.
    pub const BATTERY_LEVEL: u8 = 3;
    /// On the charger.
    pub const CHARGING_ON: u8 = 7;
    /// Off the charger.
    pub const CHARGING_OFF: u8 = 8;
    /// Strap detected on the wrist.
    pub const WRIST_ON: u8 = 9;
    /// Strap detected off the wrist.
    pub const WRIST_OFF: u8 = 10;
    /// Real-time clock lost — wall time is not trustworthy until set.
    pub const RTC_LOST: u8 = 13;
    /// Double tap on the strap.
    pub const DOUBLE_TAP: u8 = 14;
    /// Bond established.
    pub const BLE_BONDED: u8 = 23;
    /// Proprietary live HR stream started.
    pub const BLE_REALTIME_HR_ON: u8 = 33;
    /// Proprietary live HR stream stopped.
    pub const BLE_REALTIME_HR_OFF: u8 = 34;
    /// Extended battery information is available (WHOOP 4, command 98).
    pub const EXTENDED_BATTERY_INFORMATION: u8 = 63;
    /// High-frequency sync enabled.
    pub const HIGH_FREQ_SYNC_ENABLED: u8 = 97;
    /// High-frequency sync disabled.
    pub const HIGH_FREQ_SYNC_DISABLED: u8 = 98;
}

/// Name for an event number.
///
/// A strap-pushed event is WHOOP's "strap as clock" signal: whatever the number
/// means, its arrival says *I may have new data*, which is why the client treats
/// any event as a rate-limited sync trigger.
pub fn event_name(n: u8) -> &'static str {
    match n {
        3 => "BATTERY_LEVEL",
        7 => "CHARGING_ON",
        8 => "CHARGING_OFF",
        9 => "WRIST_ON",
        10 => "WRIST_OFF",
        13 => "RTC_LOST",
        14 => "DOUBLE_TAP",
        17 => "TEMPERATURE_LEVEL",
        23 => "BLE_BONDED",
        32 => "CAPTOUCH_AUTOTHRESHOLD_ACTION",
        33 => "BLE_REALTIME_HR_ON",
        34 => "BLE_REALTIME_HR_OFF",
        40 => "CH1_SATURATION_DETECTED",
        41 => "CH2_SATURATION_DETECTED",
        42 => "ACCELEROMETER_SATURATION_DETECTED",
        46 => "RAW_DATA_COLLECTION_ON",
        47 => "RAW_DATA_COLLECTION_OFF",
        56 => "STRAP_DRIVEN_ALARM_SET",
        57 => "STRAP_DRIVEN_ALARM_EXECUTED",
        58 => "APP_DRIVEN_ALARM_EXECUTED",
        59 => "STRAP_DRIVEN_ALARM_DISABLED",
        60 => "HAPTICS_FIRED",
        63 => "EXTENDED_BATTERY_INFORMATION",
        96 => "HIGH_FREQ_SYNC_PROMPT",
        97 => "HIGH_FREQ_SYNC_ENABLED",
        98 => "HIGH_FREQ_SYNC_DISABLED",
        100 => "HAPTICS_TERMINATED",
        _ => "UNKNOWN_EVENT",
    }
}

// ── Record layouts ───────────────────────────────────────────────────────────

/// WHOOP 5 record layout selectors, read at byte 9 of a type-43 or type-47
/// frame.
///
/// Packet type alone does not choose a decoder: type 47 carries R16 through R26,
/// and type 43 carries both live sensor shapes and ECG.
pub mod layout {
    /// Raw ECG waveform, 1,584 bytes. MG hardware.
    pub const R16_ECG_RAW: u8 = 16;
    /// Filtered ECG waveform, 240 bytes. MG hardware.
    pub const R17_ECG_FILTERED: u8 = 17;
    /// Biometric summary, 124 bytes.
    pub const R18_BIOMETRIC: u8 = 18;
    /// Optical blocks, 2,140 bytes.
    pub const R20_OPTICAL: u8 = 20;
    /// Six-axis IMU, 1,244 bytes.
    pub const R21_IMU: u8 = 21;
    /// Queued multi-channel record. No decoder here — the version-9 channel
    /// encoding is documented but unimplemented, so these stay raw.
    pub const R22_QUEUED: u8 = 22;
    /// Compact optical window, 88 bytes.
    pub const R26_COMPACT_OPTICAL: u8 = 26;
}

/// Expected total frame length for a WHOOP 5 record layout, where the layout
/// has a fixed one.
///
/// Fixed regions do not move when a declared count is smaller, so a length
/// check is a real gate rather than a formality: a decoder that located arrays
/// with `header + count × width` would read the wrong bytes on every
/// partly-populated record.
pub fn whoop5_layout_length(layout: u8) -> Option<usize> {
    match layout {
        self::layout::R16_ECG_RAW => Some(1584),
        self::layout::R17_ECG_FILTERED => Some(240),
        self::layout::R18_BIOMETRIC => Some(124),
        self::layout::R20_OPTICAL => Some(2140),
        self::layout::R21_IMU => Some(1244),
        self::layout::R26_COMPACT_OPTICAL => Some(88),
        _ => None,
    }
}

// ── Conversions ──────────────────────────────────────────────────────────────

/// R-R interval ticks (1/1024 s) to whole milliseconds.
///
/// `(ticks * 1000 + 512) / 1024`, computed in a wide enough intermediate. Used
/// by WHOOP 5 packet 40 and R18, and by the standard Heart Rate Service.
///
/// ```
/// # use poop::protocol::rr_ms_from_ticks;
/// assert_eq!(rr_ms_from_ticks(1024), 1000);
/// assert_eq!(rr_ms_from_ticks(1000), 977);
/// ```
pub fn rr_ms_from_ticks(ticks: u16) -> u32 {
    ((ticks as u32 * 1000) + 512) / 1024
}

/// Combine a WHOOP 5 inertial timestamp: `seconds + fraction / 32768`.
///
/// The fraction is a clock reading quantised to hundredths of a second; its
/// finer binary representation does not imply finer acquisition precision.
pub fn inertial_timestamp(seconds: u32, fraction: u16) -> f64 {
    seconds as f64 + fraction as f64 / 32768.0
}
