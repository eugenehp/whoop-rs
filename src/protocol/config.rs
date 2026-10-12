//! Named configuration inventories, alarm detail codes and probe vocabularies.

// ── Named configuration ──────────────────────────────────────────────────────

/// How a named configuration value is represented on the wire.
///
/// Every value is **ASCII text** in a 32-byte field — there is no binary form
/// and no type tag in the reply, so the type has to come from the key's schema
/// rather than from the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigValueType {
    /// Exact NUL-terminated ASCII `0`, `1` or `2`.
    ///
    /// Numeric policy values, **not** one universal on/off encoding: the same
    /// digit means different things per key, and for one key `2` reads as false
    /// where `0` and `1` both read as true.
    TriState,
    /// A decimal integer bounded above by 255. The accepted range is not a
    /// per-key safe operating range.
    UnsignedByte,
    /// A non-negative decimal scaled by ten with `+0.5` rounding into bounded
    /// integer storage; readback decodes a 16-bit quantity.
    Tenths,
}

/// One device-configuration key.
#[derive(Debug, Clone, Copy)]
pub struct DeviceKey {
    /// The name sent in the 32-byte key field.
    pub name: &'static str,
    /// How to format a value for it.
    pub value_type: ConfigValueType,
    /// What is established about it, and what is not.
    pub note: &'static str,
}

/// The eight device-configuration keys eligible for named lookup.
///
/// Eligibility is not a complete behavioural contract, and it does not
/// establish support on every device. In particular **writing zero is not a
/// universal reset-to-off**: `enable_raw_data_w_ecg` treats `0` and `1` alike
/// as true, and an unavailable readback also acts as true.
pub const DEVICE_KEYS: [DeviceKey; 8] = [
    DeviceKey {
        name: "sigproc_wear_detect",
        value_type: ConfigValueType::TriState,
        note: "wear-detection setting; per-value effect and safe change conditions unresolved",
    },
    DeviceKey {
        name: "enable_rfid",
        value_type: ConfigValueType::TriState,
        note: "RFID setting; effect, polarity and default unresolved",
    },
    DeviceKey {
        name: "max_collection_backlog",
        value_type: ConfigValueType::Tenths,
        note: "capacity percentage in tenths; exceeding it attempts to clear continuous mode",
    },
    DeviceKey {
        name: "cont_collection_mode",
        value_type: ConfigValueType::UnsignedByte,
        note:
            "0 removes continuous collection, 1 requests optical and IMU; other bytes unestablished",
    },
    DeviceKey {
        name: "whoop_live_hr_in_adv_ind_pkt",
        value_type: ConfigValueType::TriState,
        note: "stored 1 selects this advertising preference; 0 and 2 do not",
    },
    DeviceKey {
        name: "whoop_live_2_hrm_devices",
        value_type: ConfigValueType::TriState,
        note: "stored 1 selects the named preference; connection capacity unresolved",
    },
    DeviceKey {
        name: "enable_raw_data_w_ecg",
        value_type: ConfigValueType::TriState,
        note: "0 and 1 act as true, 2 as false; NOT the ECG master gate",
    },
    DeviceKey {
        name: "dorset_detection_period_min",
        value_type: ConfigValueType::UnsignedByte,
        note: "zero or a failed read selects fallback 5; the name does not establish time units",
    },
];

/// One feature flag. All 25 use the tri-state representation.
#[derive(Debug, Clone, Copy)]
pub struct FeatureFlag {
    /// The name sent in the 32-byte key field.
    pub name: &'static str,
    /// Whether a named SET/GET is established for it.
    ///
    /// Five of the 25 are **ineligible**. They are listed anyway so an
    /// application does not mistake a readable name, or inclusion in a
    /// historical write bundle, for supported named access.
    pub named_lookup: bool,
    /// What is established about it.
    pub note: &'static str,
}

/// The 25 feature-flag descriptors; 20 eligible for named lookup, 5 not.
///
/// For entries with no decoded consumer, polarity, applied default, packet
/// effect and hardware variation are **unknown** however suggestive the name
/// is. Two are known to be counter-intuitive: writing ASCII `2` to the R22
/// master *removes* its contribution rather than enabling anything, and
/// `disable_pip_r26_packets` has inverse permission polarity.
pub const FEATURE_FLAGS: [FeatureFlag; 25] = [
    FeatureFlag {
        name: "general_ab_test",
        named_lookup: false,
        note: "no operational value mapping established",
    },
    FeatureFlag {
        name: "enable_r22_packets",
        named_lookup: true,
        note: "layout-22 master: 1 permits, 0 and 2 do not",
    },
    FeatureFlag {
        name: "enable_r22_v2_packets",
        named_lookup: true,
        note: "stored 1 is true; 0 and 2 false",
    },
    FeatureFlag {
        name: "enable_r22_v3_packets",
        named_lookup: true,
        note: "stored 1 is true; 0 and 2 false",
    },
    FeatureFlag {
        name: "enable_r22_v4_packets",
        named_lookup: true,
        note: "stored 1 is true; 0 and 2 false",
    },
    FeatureFlag {
        name: "enable_r22_v5_packets",
        named_lookup: true,
        note: "stored 1 is true; 0 and 2 false",
    },
    FeatureFlag {
        name: "enable_r22_v6_packets",
        named_lookup: true,
        note: "stored 1 is true; 0 and 2 false",
    },
    FeatureFlag {
        name: "enable_r22_v8_packets",
        named_lookup: true,
        note: "per-version selector semantics unresolved",
    },
    FeatureFlag {
        name: "enable_r22_v9_packets",
        named_lookup: true,
        note: "stored 1 is true; 0 and 2 false",
    },
    FeatureFlag {
        name: "make_hrfm_visible",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "disable_pip_r26_packets",
        named_lookup: true,
        note: "inverse polarity: 1 removes layout 26; 0 and 2 permit it",
    },
    FeatureFlag {
        name: "wear_detect_bias",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "enable_pdaf_walk_det",
        named_lookup: false,
        note: "no named SET support established",
    },
    FeatureFlag {
        name: "enable_maverick_model",
        named_lookup: false,
        note: "no named SET support established",
    },
    FeatureFlag {
        name: "hr_ch_switching",
        named_lookup: true,
        note: "one prerequisite of the alternate-source branch in R18 bits 4/5",
    },
    FeatureFlag {
        name: "ir_hw_switching",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "enable_passive_strap_fit_gen5",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "enable_sig11_during_sleep",
        named_lookup: false,
        note: "bundle inclusion is not named SET support",
    },
    FeatureFlag {
        name: "dorset_inhibit_wpt",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "enable_sig12",
        named_lookup: false,
        note: "bundle inclusion is not named SET support",
    },
    FeatureFlag {
        name: "enable_frizzle_burst_mode",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "ir_1x_enable",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
    FeatureFlag {
        name: "enable_rocky_again",
        named_lookup: true,
        note: "a change can trigger processing reinitialisation",
    },
    FeatureFlag {
        name: "project_drawbridge",
        named_lookup: true,
        note: "stored 1 enables one side of a conditional gate; not a stream-off control",
    },
    FeatureFlag {
        name: "wear_detect_fast_event",
        named_lookup: true,
        note: "consumer semantics unresolved",
    },
];

/// The R22 flags, in the order a disable run walks them.
///
/// The master first, then each version selector. Ordering matters only in that
/// the run reads each key back after writing it; nothing establishes that the
/// strap applies them atomically.
pub const R22_FEATURE_KEYS: [&str; 8] = [
    "enable_r22_packets",
    "enable_r22_v2_packets",
    "enable_r22_v3_packets",
    "enable_r22_v4_packets",
    "enable_r22_v5_packets",
    "enable_r22_v6_packets",
    "enable_r22_v8_packets",
    "enable_r22_v9_packets",
];

/// The tri-state value a disable run writes: ASCII `0`.
pub const FEATURE_FLAG_OFF: &str = "0";

/// Look up a device key by name.
pub fn device_key(name: &str) -> Option<&'static DeviceKey> {
    DEVICE_KEYS.iter().find(|k| k.name == name)
}

/// Look up a feature flag by name.
pub fn feature_flag(name: &str) -> Option<&'static FeatureFlag> {
    FEATURE_FLAGS.iter().find(|f| f.name == name)
}

// ── Alarm and probe detail codes ─────────────────────────────────────────────

/// The second byte of a WHOOP 5 `SET_ALARM_TIME` response body.
///
/// Distinct from the outer command result, and the distinction matters:
/// [`Self::Accepted`] can accompany outer **failure**, because a pattern can be
/// stored while the time is rejected. Read back both parts when the final state
/// matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlarmSetDetail {
    /// Input passed validation.
    Accepted,
    /// An effect byte was out of range.
    EffectOutOfRange,
    /// Repeat count out of range.
    RepeatOutOfRange,
    /// Duration out of range for repeat count 7.
    DurationOutOfRange,
    /// The time was not strictly later than the current whole second.
    TimeNotInFuture,
    /// Alarm ID out of range.
    IdOutOfRange,
    /// Crescendo out of range.
    CrescendoOutOfRange,
    /// Something else; preserved numerically.
    Unknown(u8),
}

impl AlarmSetDetail {
    /// Classify the detail byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            1 => Self::Accepted,
            2 => Self::EffectOutOfRange,
            3 => Self::RepeatOutOfRange,
            4 => Self::DurationOutOfRange,
            10 => Self::TimeNotInFuture,
            11 => Self::IdOutOfRange,
            12 => Self::CrescendoOutOfRange,
            other => Self::Unknown(other),
        }
    }
}

/// The second byte of a WHOOP 5 `RUN_ALARM` response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlarmRunDetail {
    /// The initial pending reply.
    Started,
    /// Completed. Outer success accompanies this detail only.
    Completed,
    /// The strap gave up after up to three start attempts.
    Timeout,
    /// The ID is invalid or has no stored time.
    IdOutOfRange,
    /// Something else; preserved numerically.
    Unknown(u8),
}

impl AlarmRunDetail {
    /// Classify the detail byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::Started,
            5 => Self::Completed,
            7 => Self::Timeout,
            11 => Self::IdOutOfRange,
            other => Self::Unknown(other),
        }
    }
}

/// Where the strap believes it is worn, from a WHOOP 4 command-84 record.
///
/// The gap at 6 is real: any value not listed is kept raw rather than mapped to
/// a neighbour. **Never** feed this into wear detection, sleep gating or
/// scoring — it is a diagnostic read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyLocation {
    /// 0
    Unknown,
    /// 1
    Wrist,
    /// 2
    Bicep,
    /// 3
    Calf,
    /// 4
    SideTorso,
    /// 5
    Glute,
    /// 7 — note the gap at 6.
    Ankle,
    /// 128
    NotConclusive,
    /// 160
    UnknownGarment,
    /// Anything else, kept as the byte it was.
    Raw(u8),
}

impl BodyLocation {
    /// Classify a location byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::Unknown,
            1 => Self::Wrist,
            2 => Self::Bicep,
            3 => Self::Calf,
            4 => Self::SideTorso,
            5 => Self::Glute,
            7 => Self::Ankle,
            128 => Self::NotConclusive,
            160 => Self::UnknownGarment,
            other => Self::Raw(other),
        }
    }
}

/// Which wrist a `SELECT_WRIST` request selects.
///
/// MG-only, in-memory: persistence across reboot is not established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrist {
    /// 1
    Right = 1,
    /// 2
    Left = 2,
}

/// One candidate of the WHOOP 4 restart probe.
///
/// The documented 41.17.6.0 contract does not evaluate the body for either
/// restart command, so empty, `00` and `01` should be equivalent — and one
/// device observation showed no response, disconnect or reboot at all for
/// command 29. The probe exists because that disagreement is the thing to
/// measure: it sends **one** candidate at a time so a log can say which one
/// dropped the link and which was ignored.
///
/// Every variant is destructive-adjacent and user-gated by construction: a
/// caller has to name one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootProbe {
    /// `REBOOT_STRAP` (29) with an empty body.
    RebootEmpty,
    /// `POWER_CYCLE_STRAP` (32) with an empty body.
    PowerCycleEmpty,
    /// `REBOOT_STRAP` (29) with `[0x01]`.
    RebootOne,
    /// `POWER_CYCLE_STRAP` (32) with `[0x01]`.
    PowerCycleOne,
    /// `REBOOT_STRAP` (29) with `[0x00]`.
    RebootZero,
}

impl RebootProbe {
    /// All five candidates, in the order the reference probe sends them.
    pub const ALL: [RebootProbe; 5] = [
        Self::RebootEmpty,
        Self::PowerCycleEmpty,
        Self::RebootOne,
        Self::PowerCycleOne,
        Self::RebootZero,
    ];

    /// The opcode and body this candidate sends.
    pub fn request(self) -> (u8, &'static [u8]) {
        match self {
            Self::RebootEmpty => (29, &[]),
            Self::PowerCycleEmpty => (32, &[]),
            Self::RebootOne => (29, &[0x01]),
            Self::PowerCycleOne => (32, &[0x01]),
            Self::RebootZero => (29, &[0x00]),
        }
    }

    /// A label for a log line.
    pub fn label(self) -> &'static str {
        match self {
            Self::RebootEmpty => "REBOOT_STRAP(29) empty",
            Self::PowerCycleEmpty => "POWER_CYCLE_STRAP(32) empty",
            Self::RebootOne => "REBOOT_STRAP(29) [01]",
            Self::PowerCycleOne => "POWER_CYCLE_STRAP(32) [01]",
            Self::RebootZero => "REBOOT_STRAP(29) [00]",
        }
    }
}
