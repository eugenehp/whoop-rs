//! Command numbers, result codes and the send gate.

use super::DeviceFamily;

// ── Command results ──────────────────────────────────────────────────────────

/// The result byte of a command response.
///
/// [`Self::Success`] means the request was accepted **at that command's response
/// boundary**. It is not proof of completed asynchronous work, sensor
/// initialisation, delivered packets or durable storage, and the first byte of
/// the response body is command-specific — often a revision marker — rather than
/// a second success flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResult {
    /// Refused. Preserve it; do not infer a cause without command-specific
    /// information.
    Failure,
    /// Accepted at this command's response boundary.
    Success,
    /// Keep the request open for a later result, within a bounded client wait.
    Pending,
    /// Not supported in the applicable command context.
    Unsupported,
    /// Something else. Preserved numerically rather than coerced to success.
    Unknown(u8),
}

impl CommandResult {
    /// Classify a raw result byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::Failure,
            1 => Self::Success,
            2 => Self::Pending,
            3 => Self::Unsupported,
            other => Self::Unknown(other),
        }
    }

    /// Whether this result ends the request, as opposed to leaving it open.
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

// ── Metadata types ───────────────────────────────────────────────────────────

/// Historical-offload control values carried by a metadata frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataType {
    /// Opens the stream.
    HistoryStart,
    /// Closes a chunk and asks for an acknowledgement.
    HistoryEnd,
    /// Closes the overall attempt.
    HistoryComplete,
}

impl MetadataType {
    /// Classify a metadata type byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::HistoryStart),
            2 => Some(Self::HistoryEnd),
            3 => Some(Self::HistoryComplete),
            _ => None,
        }
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

/// The command subset this crate will send.
///
/// Deliberately smaller than the decode vocabulary. Commands that can wipe
/// data, write firmware or enter the bootloader are not here at all — see
/// [`is_destructive`] for the list and the reason — and [`super::admits_send`] gates
/// the raw-opcode path so an arbitrary number cannot reach a strap by accident.
///
/// The discriminant is the on-wire command byte: `[6]` on WHOOP 4, `[10]` on
/// WHOOP 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Command {
    /// WHOOP 5 only. Returns a fixed 13-byte acknowledgement; **not** an
    /// identity token.
    LinkValid = 1,
    /// Proprietary live HR stream toggle. The standard Heart Rate Service is a
    /// separate source and stays available either way.
    ToggleRealtimeHr = 3,
    /// WHOOP 4 firmware versions, from a 68-byte response body.
    ReportVersionInfo = 7,
    /// Deprecated clock pair. On WHOOP 4 one of two request forms has been seen
    /// to latch; read back to confirm.
    SetClockDeprecated = 10,
    /// Deprecated clock read.
    GetClockDeprecated = 11,
    /// Stop request for an in-progress offload. Not a trim, and local cleanup
    /// must not depend on a reply arriving.
    AbortHistoricalTransmits = 20,
    /// Begin the historical offload.
    SendHistoricalData = 22,
    /// Acknowledge one `HISTORY_END` chunk: `[0x01] + the verbatim 8-byte end
    /// block`. Sent only after the chunk is durably stored.
    HistoricalDataResult = 23,
    /// Battery percent. On WHOOP 4 the body is `u16le` tenths; on WHOOP 5 it is
    /// a four-byte whole percent.
    GetBatteryLevel = 26,
    /// Restart. User-initiated and confirmation-gated only.
    RebootStrap = 29,
    /// The strap's stored record range and ring cursors.
    GetDataRange = 34,
    /// WHOOP 4 identity hello; its body carries the strap serial beside
    /// sensitive key material.
    GetHelloHarvard = 35,
    /// The WHOOP 4 type-43 raw-stream output switch.
    SendR10R11Realtime = 63,
    /// Arm an alarm.
    SetAlarmTime = 66,
    /// Read the armed alarm.
    GetAlarmTime = 67,
    /// Fire the stored alarm now.
    RunAlarm = 68,
    /// Disarm.
    DisableAlarm = 69,
    /// WHOOP 4 advertised name.
    GetAdvertisingNameHarvard = 76,
    /// Buzz a preset haptic pattern (WHOOP 4 form).
    RunHapticsPattern = 79,
    /// Raw-data collection on.
    StartRawData = 81,
    /// Raw-data collection off.
    StopRawData = 82,
    /// Wrist / body-location status. Read-only.
    GetBodyLocationAndStatus = 84,
    /// Leave high-frequency sync. Used during watchdog recovery; this crate
    /// never enters it.
    ExitHighFreqSync = 97,
    /// WHOOP 4 extended battery record from cache (25 bytes when warm).
    GetExtendedBatteryInfo = 98,
    /// Requested live motion state. An accepted write is not proof the producer
    /// started.
    ToggleImuMode = 106,
    /// Read stored IMU stream state.
    GetImuDataStream = 107,
    /// Stop an in-progress haptic.
    StopHaptics = 122,
    /// MG-only in-memory wrist selection. Persistence across reboot is not
    /// established.
    SelectWrist = 123,
    /// MG ECG data generation (`[0x01, 1]` stop, `[0x01, 2]` start).
    ToggleLabradorDataGeneration = 124,
    /// MG raw ECG save toggle.
    ToggleLabradorRawSave = 125,
    /// MG filtered realtime ECG toggle.
    ToggleLabradorFiltered = 139,
    /// WHOOP 5 buzz — `RUN_HAPTIC_PATTERN_MAVERICK`, a 12-byte body.
    RunHapticPatternMaverick = 19,
    /// WHOOP 5 identity hello. Revisions 1 and 3; the first reply is PENDING.
    GetHello = 145,
    /// WHOOP 5 clock set — revision 1, `u32` seconds, `u16` ticks of 1/32768 s.
    SetClock = 146,
    /// WHOOP 5 clock read.
    GetClock = 147,
    /// WHOOP 5 battery-pack fuel-gauge record, from cache —
    /// `BATTERY_PACK_INFO`.
    GetBatteryPack = 151,

    // ── Configuration and service commands ──────────────────────────────────
    /// WHOOP 4 advertised-name write. Two reserved bytes, the name, then NUL.
    SetAdvertisingNameHarvard = 77,
    /// Legacy haptic-pattern read. Response vocabulary unresolved.
    GetAllHapticsPattern = 80,
    /// Count device-configuration keys. Resets the device enumeration cursor.
    CountDeviceKeys = 115,
    /// Next device-configuration key. Advances a **shared cursor**, not a
    /// caller-supplied index, so requests must be serialised.
    NextDeviceKey = 116,
    /// Count feature flags. Resets the feature enumeration cursor.
    StartFeatureFlagExchange = 117,
    /// Next feature flag. Advances the feature cursor.
    SendNextFeatureFlag = 118,
    /// Write a device-configuration value. Gated — see [`SendPolicy`].
    SetDeviceValue = 119,
    /// Write a feature-flag value. Gated — see [`SendPolicy`].
    SetFeatureValue = 120,
    /// Read a device-configuration value by name.
    ReadDeviceValue = 121,
    /// Read a feature-flag value by name.
    ReadFeatureValue = 128,
    /// WHOOP 5 advertised-name write, 0–15 bytes.
    SetAdvertisingName = 140,
    /// WHOOP 5 advertised-name read. Fixed 19-byte reply.
    GetAdvertisingName = 141,
    /// Force the worn state and disable normal wear detection. Gated.
    SetWearDetectionOverride = 148,
    /// Persistent LED accessibility option. Gated.
    SetLedAccessibility = 149,
}

impl Command {
    /// The on-wire command byte.
    pub const fn opcode(self) -> u8 {
        self as u8
    }

    /// Protocol name, as the command reference spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::LinkValid => "LINK_VALID",
            Self::ToggleRealtimeHr => "TOGGLE_REALTIME_HR",
            Self::ReportVersionInfo => "REPORT_VERSION_INFO",
            Self::SetClockDeprecated => "SET_CLOCK_DEPRECATED",
            Self::GetClockDeprecated => "GET_CLOCK_DEPRECATED",
            Self::AbortHistoricalTransmits => "ABORT_HISTORICAL_TRANSMITS",
            Self::SendHistoricalData => "SEND_HISTORICAL_DATA",
            Self::HistoricalDataResult => "HISTORICAL_DATA_RESULT",
            Self::GetBatteryLevel => "GET_BATTERY_LEVEL",
            Self::RebootStrap => "REBOOT_STRAP",
            Self::GetDataRange => "GET_DATA_RANGE",
            Self::GetHelloHarvard => "GET_HELLO_HARVARD",
            Self::SendR10R11Realtime => "SEND_R10_R11_REALTIME",
            Self::SetAlarmTime => "SET_ALARM_TIME",
            Self::GetAlarmTime => "GET_ALARM_TIME",
            Self::RunAlarm => "RUN_ALARM",
            Self::DisableAlarm => "DISABLE_ALARM",
            Self::GetAdvertisingNameHarvard => "GET_ADVERTISING_NAME_HARVARD",
            Self::RunHapticsPattern => "RUN_HAPTICS_PATTERN",
            Self::StartRawData => "START_RAW_DATA",
            Self::StopRawData => "STOP_RAW_DATA",
            Self::GetBodyLocationAndStatus => "GET_BODY_LOCATION_AND_STATUS",
            Self::ExitHighFreqSync => "EXIT_HIGH_FREQ_SYNC",
            Self::GetExtendedBatteryInfo => "GET_EXTENDED_BATTERY_INFO",
            Self::ToggleImuMode => "TOGGLE_IMU_MODE",
            Self::GetImuDataStream => "GET_IMU_DATA_STREAM",
            Self::StopHaptics => "STOP_HAPTICS",
            Self::SelectWrist => "SELECT_WRIST",
            Self::ToggleLabradorDataGeneration => "TOGGLE_LABRADOR_DATA_GENERATION",
            Self::ToggleLabradorRawSave => "TOGGLE_LABRADOR_RAW_SAVE",
            Self::ToggleLabradorFiltered => "TOGGLE_LABRADOR_FILTERED",
            Self::RunHapticPatternMaverick => "RUN_HAPTIC_PATTERN_MAVERICK",
            Self::GetHello => "GET_HELLO",
            Self::SetClock => "SET_CLOCK",
            Self::GetClock => "GET_CLOCK",
            Self::GetBatteryPack => "GET_BATTERY_PACK_INFO",
            Self::SetAdvertisingNameHarvard => "SET_ADVERTISING_NAME_HARVARD",
            Self::GetAllHapticsPattern => "GET_ALL_HAPTICS_PATTERN",
            Self::CountDeviceKeys => "COUNT_DEVICE_KEYS",
            Self::NextDeviceKey => "SEND_NEXT_DEVICE_KEY",
            Self::StartFeatureFlagExchange => "START_FF_KEY_EXCHANGE",
            Self::SendNextFeatureFlag => "SEND_NEXT_FF",
            Self::SetDeviceValue => "SET_DEVICE_CONFIG_VALUE",
            Self::SetFeatureValue => "SET_FF_VALUE",
            Self::ReadDeviceValue => "GET_DEVICE_CONFIG_VALUE",
            Self::ReadFeatureValue => "GET_FF_VALUE",
            Self::SetAdvertisingName => "SET_ADVERTISING_NAME",
            Self::GetAdvertisingName => "GET_ADVERTISING_NAME",
            Self::SetWearDetectionOverride => "SET_WEAR_DETECTION_OVERRIDE",
            Self::SetLedAccessibility => "SET_LED_ACCESSIBILITY",
        }
    }
}

/// Name for an arbitrary command byte, including ones outside [`Command`].
///
/// A number with no name is reported as `CMD_<n>` rather than dropped: an
/// unknown command in a reply is unresolved, not absent.
///
/// Where the two generations give one number different names, this table reads
/// it as WHOOP 5/MG — opcode 19 is `RUN_HAPTIC_PATTERN_MAVERICK` here and
/// `SET_R7_REALTIME_STREAM` on a WHOOP 4. The name is a label for a log, not a
/// selector: nothing in this crate dispatches on it, and the sending paths pick
/// their opcode per family. Consult the canonical command matrix when the
/// distinction matters.
pub fn command_name(opcode: u8) -> String {
    const KNOWN: [Command; 50] = [
        Command::LinkValid,
        Command::ToggleRealtimeHr,
        Command::ReportVersionInfo,
        Command::SetClockDeprecated,
        Command::GetClockDeprecated,
        Command::RunHapticPatternMaverick,
        Command::AbortHistoricalTransmits,
        Command::SendHistoricalData,
        Command::HistoricalDataResult,
        Command::GetBatteryLevel,
        Command::RebootStrap,
        Command::GetDataRange,
        Command::GetHelloHarvard,
        Command::SendR10R11Realtime,
        Command::SetAlarmTime,
        Command::GetAlarmTime,
        Command::RunAlarm,
        Command::DisableAlarm,
        Command::GetAdvertisingNameHarvard,
        Command::RunHapticsPattern,
        Command::StartRawData,
        Command::StopRawData,
        Command::GetBodyLocationAndStatus,
        Command::ExitHighFreqSync,
        Command::GetExtendedBatteryInfo,
        Command::ToggleImuMode,
        Command::GetImuDataStream,
        Command::StopHaptics,
        Command::SelectWrist,
        Command::ToggleLabradorDataGeneration,
        Command::ToggleLabradorRawSave,
        Command::ToggleLabradorFiltered,
        Command::GetHello,
        Command::SetClock,
        Command::GetClock,
        Command::GetBatteryPack,
        Command::SetAdvertisingNameHarvard,
        Command::GetAllHapticsPattern,
        Command::CountDeviceKeys,
        Command::NextDeviceKey,
        Command::StartFeatureFlagExchange,
        Command::SendNextFeatureFlag,
        Command::SetDeviceValue,
        Command::SetFeatureValue,
        Command::ReadDeviceValue,
        Command::ReadFeatureValue,
        Command::SetAdvertisingName,
        Command::GetAdvertisingName,
        Command::SetWearDetectionOverride,
        Command::SetLedAccessibility,
    ];
    if let Some(c) = KNOWN.into_iter().find(|c| c.opcode() == opcode) {
        return c.name().to_owned();
    }
    match opcode {
        15 => "FORGET_BONDS".into(),
        25 => "FORCE_TRIM".into(),
        32 => "POWER_CYCLE_STRAP".into(),
        33 => "SET_READ_POINTER".into(),
        36 => "START_FIRMWARE_LOAD".into(),
        37 => "LOAD_FIRMWARE_DATA".into(),
        38 => "PROCESS_FIRMWARE_IMAGE".into(),
        45 => "ENTER_BLE_DFU".into(),
        61 => "SET_AFE_PARAMETERS".into(),
        62 => "GET_AFE_PARAMETERS".into(),
        96 => "ENTER_HIGH_FREQ_SYNC".into(),
        // Named in the decode catalogue only: this crate has no sender for
        // either, because neither request contract has been validated.
        138 => "SELECT_PRESET".into(),
        153 => "TOGGLE_PERSISTENT_R20".into(),
        154 => "TOGGLE_PERSISTENT_R21".into(),
        99 => "RESET_FUEL_GAUGE".into(),
        142 => "START_FIRMWARE_LOAD_NEW".into(),
        143 => "LOAD_FIRMWARE_DATA_NEW".into(),
        144 => "PROCESS_FIRMWARE_IMAGE_NEW".into(),
        other => format!("CMD_{other}"),
    }
}

/// Commands this crate refuses to transmit.
///
/// They exist on the wire; sending one can discard unoffloaded history, write
/// firmware or leave the strap in the bootloader. The list is written down
/// rather than merely omitted from [`Command`], because something has to record
/// that it must stay that way.
///
/// | Code | Command | Hazard |
/// |-----:|---------|--------|
/// | 15 | `FORGET_BONDS` | drops the bond |
/// | 25 | `FORCE_TRIM` | invasive cursor/reclaim; unoffloaded data may go |
/// | 32 | `POWER_CYCLE_STRAP` | hard power cycle |
/// | 33 | `SET_READ_POINTER` | invasive cursor mutation |
/// | 36, 37, 38 | firmware load / data / process | firmware write |
/// | 45 | `ENTER_BLE_DFU` | enters the bootloader |
/// | 99 | `RESET_FUEL_GAUGE` | resets the battery gauge |
/// | 142, 143, 144 | the high-opcode firmware family | firmware write |
pub fn is_destructive(opcode: u8) -> bool {
    matches!(
        opcode,
        15 | 25 | 32 | 33 | 36 | 37 | 38 | 45 | 99 | 142 | 143 | 144
    )
}

/// Why a send was refused before it reached the radio.
///
/// Every variant carries the opcode and its name, because a refusal has to be
/// explicable from the log line alone: "refused" without the number is a report
/// nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendRefused {
    /// The opcode is on the destructive list, which no policy admits.
    Destructive {
        /// The command byte that was refused.
        opcode: u8,
        /// Its protocol name.
        name: String,
    },
    /// An MG-only ECG command aimed at a family that does not have it.
    WrongFamily {
        /// The command byte that was refused.
        opcode: u8,
        /// Its protocol name.
        name: String,
        /// The family this connection actually speaks.
        family: DeviceFamily,
    },
    /// An ECG command without [`SendPolicy::ecg`].
    EcgNotOptedIn {
        /// The command byte that was refused.
        opcode: u8,
        /// Its protocol name.
        name: String,
    },
    /// A configuration write without [`SendPolicy::config_write`].
    ConfigWriteNotOptedIn {
        /// The command byte that was refused.
        opcode: u8,
        /// Its protocol name.
        name: String,
    },
}

impl std::fmt::Display for SendRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Destructive { opcode, name } => {
                write!(f, "refusing to send {name} ({opcode}): destructive command")
            }
            Self::WrongFamily {
                opcode,
                name,
                family,
            } => write!(
                f,
                "refusing to send {name} ({opcode}) to a {}: command is WHOOP 5/MG only",
                family.label()
            ),
            Self::EcgNotOptedIn { opcode, name } => write!(
                f,
                "refusing to send {name} ({opcode}): ECG commands need an explicit opt-in"
            ),
            Self::ConfigWriteNotOptedIn { opcode, name } => write!(
                f,
                "refusing to send {name} ({opcode}): configuration writes need an explicit opt-in"
            ),
        }
    }
}

impl std::error::Error for SendRefused {}

/// The ECG command family — MG hardware, and gated separately from framing.
pub const ECG_COMMANDS: [u8; 4] = [123, 124, 125, 139];

/// Commands that write configuration or override a sensor policy.
///
/// Separated from the destructive list because these are recoverable — a wrong
/// value can be written back — but they are not reads, and a crate that sends
/// one by default would be changing a strap's stored settings on connect. The
/// named writes (119/120) additionally persist, and 148 disables wear detection
/// outright.
pub const CONFIG_WRITE_COMMANDS: [u8; 5] = [119, 120, 140, 148, 149];

/// What a connection is allowed to send beyond the ordinary read surface.
///
/// A struct rather than a growing list of `bool` arguments: each flag is a
/// separate decision with its own reason, and a positional pair of bools at a
/// call site is the kind of thing that gets swapped silently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendPolicy {
    /// Permit the MG ECG family. These change acquisition state, and repeated
    /// starts can lose the record a companion collection needs to clean up.
    pub ecg: bool,
    /// Permit configuration writes — see [`CONFIG_WRITE_COMMANDS`].
    pub config_write: bool,
}

impl SendPolicy {
    /// Reads only. The default, and what every front end in this crate uses
    /// unless the caller asked for more.
    pub const READ_ONLY: Self = Self {
        ecg: false,
        config_write: false,
    };

    /// Permit ECG commands.
    pub const fn with_ecg(mut self) -> Self {
        self.ecg = true;
        self
    }

    /// Permit configuration writes.
    pub const fn with_config_write(mut self) -> Self {
        self.config_write = true;
        self
    }
}

/// Decide whether an opcode may be written to this family.
///
/// Checked before a frame is built, so a refusal costs no radio time and the
/// reason survives into a log. Three separate decisions, in order of severity:
///
/// 1. Destructive commands are refused outright, whatever the policy says.
/// 2. The ECG family needs WHOOP 5 hardware **and** [`SendPolicy::ecg`].
/// 3. Configuration writes need [`SendPolicy::config_write`].
pub fn admits_send(
    opcode: u8,
    family: DeviceFamily,
    policy: SendPolicy,
) -> Result<(), SendRefused> {
    if is_destructive(opcode) {
        return Err(SendRefused::Destructive {
            opcode,
            name: command_name(opcode),
        });
    }
    if ECG_COMMANDS.contains(&opcode) {
        if family != DeviceFamily::Whoop5 {
            return Err(SendRefused::WrongFamily {
                opcode,
                name: command_name(opcode),
                family,
            });
        }
        if !policy.ecg {
            return Err(SendRefused::EcgNotOptedIn {
                opcode,
                name: command_name(opcode),
            });
        }
    }
    if CONFIG_WRITE_COMMANDS.contains(&opcode) && !policy.config_write {
        return Err(SendRefused::ConfigWriteNotOptedIn {
            opcode,
            name: command_name(opcode),
        });
    }
    Ok(())
}
