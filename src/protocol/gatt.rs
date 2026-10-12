//! GATT topology: device families, service and characteristic UUIDs, and
//! which 5-family hardware a strap is.

use uuid::Uuid;

// ── Device families ──────────────────────────────────────────────────────────

/// Which WHOOP hardware generation a connection or capture belongs to.
///
/// The one switch every other item in this module branches on. Nothing here
/// imports a Bluetooth type: the UUID constants are plain [`Uuid`]s so the
/// protocol code runs in tests, in a CLI and in a page unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceFamily {
    /// WHOOP 4.0 — CRC8 (poly 0x07) header check over the length field.
    Whoop4,
    /// WHOOP 5.0 / MG — CRC16-Modbus header check, format-1 framing.
    Whoop5,
}

impl DeviceFamily {
    /// Primary custom GATT service for this family.
    pub const fn service_uuid(self) -> Uuid {
        match self {
            Self::Whoop4 => WHOOP4_SERVICE,
            Self::Whoop5 => WHOOP5_SERVICE,
        }
    }

    /// The characteristic CLIENT_HELLO and every command frame is written to.
    pub const fn command_characteristic(self) -> Uuid {
        match self {
            Self::Whoop4 => WHOOP4_COMMAND,
            Self::Whoop5 => WHOOP5_COMMAND,
        }
    }

    /// Notification characteristics to subscribe to, in ascending order.
    ///
    /// WHOOP 4 separates command replies (`0003`), events (`0004`) and
    /// fragmented data (`0005`). WHOOP 5 adds a fifth (`0007`) and does not
    /// document a fixed role per channel, so all of them are read the same way:
    /// bytes into a [`super::Reassembler`], frames out.
    pub const fn notify_characteristics(self) -> &'static [Uuid] {
        match self {
            Self::Whoop4 => &WHOOP4_NOTIFY,
            Self::Whoop5 => &WHOOP5_NOTIFY,
        }
    }

    /// Which checksum guards the frame header.
    pub const fn header_crc(self) -> HeaderCrc {
        match self {
            Self::Whoop4 => HeaderCrc::Crc8,
            Self::Whoop5 => HeaderCrc::Crc16Modbus,
        }
    }

    /// Offset of the inner record (`[type][seq][cmd]`) in a complete frame.
    ///
    /// 4 on WHOOP 4, 8 on WHOOP 5 — the whole +4 shift between the two
    /// generations' field tables comes from this one number, and it is the
    /// reason a WHOOP 5 offset must never be reached by adding 4 to a WHOOP 4
    /// one without a contract that says so.
    pub const fn inner_offset(self) -> usize {
        match self {
            Self::Whoop4 => 4,
            Self::Whoop5 => 8,
        }
    }

    /// Smallest frame this family will accept.
    ///
    /// WHOOP 4: envelope (4) + inner `[type][seq][cmd]` (3) + CRC32 (4) = 11.
    /// WHOOP 5: envelope (8) + at least one inner byte + CRC32 (4) = 13. The
    /// reference parser permits a 12-byte empty payload; no such hardware frame
    /// is on record, so the floor stays where observation puts it.
    pub const fn minimum_frame_bytes(self) -> usize {
        match self {
            Self::Whoop4 => 11,
            Self::Whoop5 => 13,
        }
    }

    /// Fixed frame written straight after discovery to open a session, if this
    /// family uses one.
    ///
    /// WHOOP 5's hello is a fully-formed type-35 COMMAND frame with a correct
    /// CRC16 header and CRC32 trailer. WHOOP 4 has no fixed hello: its link
    /// becomes usable once a confirmed write is acknowledged, which the client
    /// performs with [`super::Command::GetBatteryLevel`].
    pub const fn client_hello(self) -> Option<&'static [u8]> {
        match self {
            Self::Whoop4 => None,
            Self::Whoop5 => Some(&WHOOP5_CLIENT_HELLO),
        }
    }

    /// Human-readable generation label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Whoop4 => "WHOOP 4.0",
            Self::Whoop5 => "WHOOP 5.0 / MG",
        }
    }
}

/// Which checksum guards the frame header for a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderCrc {
    /// CRC8 (poly 0x07) over the two declared-length bytes — WHOOP 4.0.
    Crc8,
    /// CRC16-Modbus (poly 0xA001, init 0xFFFF, reflected) over bytes `[0,6)`.
    Crc16Modbus,
}

// ── Custom service UUIDs ─────────────────────────────────────────────────────

/// WHOOP 4.0 custom service.
pub const WHOOP4_SERVICE: Uuid = Uuid::from_u128(0x61080001_8d6d_82b8_614a_1c8cb0f8dcc6);
/// WHOOP 4.0 command write characteristic (app → strap).
pub const WHOOP4_COMMAND: Uuid = Uuid::from_u128(0x61080002_8d6d_82b8_614a_1c8cb0f8dcc6);
/// WHOOP 4.0 notification characteristics: command replies, events, data.
pub const WHOOP4_NOTIFY: [Uuid; 3] = [
    Uuid::from_u128(0x61080003_8d6d_82b8_614a_1c8cb0f8dcc6), // command responses
    Uuid::from_u128(0x61080004_8d6d_82b8_614a_1c8cb0f8dcc6), // events
    Uuid::from_u128(0x61080005_8d6d_82b8_614a_1c8cb0f8dcc6), // data (fragmented)
];

/// WHOOP 5.0 / MG custom service.
pub const WHOOP5_SERVICE: Uuid = Uuid::from_u128(0xfd4b0001_cce1_4033_93ce_002d5875f58a);
/// WHOOP 5.0 / MG command write characteristic.
pub const WHOOP5_COMMAND: Uuid = Uuid::from_u128(0xfd4b0002_cce1_4033_93ce_002d5875f58a);
/// WHOOP 5.0 / MG notification characteristics — one more than WHOOP 4.
pub const WHOOP5_NOTIFY: [Uuid; 4] = [
    Uuid::from_u128(0xfd4b0003_cce1_4033_93ce_002d5875f58a),
    Uuid::from_u128(0xfd4b0004_cce1_4033_93ce_002d5875f58a),
    Uuid::from_u128(0xfd4b0005_cce1_4033_93ce_002d5875f58a),
    Uuid::from_u128(0xfd4b0007_cce1_4033_93ce_002d5875f58a),
];

/// The 16 bytes a WHOOP 5/MG expects as the first write after discovery.
///
/// `AA 01 08 00 00 01 E6 71 23 01 91 01 36 3E 5C 8D` — a type-35 COMMAND frame
/// carrying command 145 (`GET_HELLO`) with revision 1. Written verbatim rather
/// than rebuilt, because this exact frame is the one a strap is known to accept;
/// [`super::whoop5_command_frame`] reproduces it byte for byte, which the test suite
/// pins.
pub const WHOOP5_CLIENT_HELLO: [u8; 16] = [
    0xAA, 0x01, 0x08, 0x00, 0x00, 0x01, 0xE6, 0x71, 0x23, 0x01, 0x91, 0x01, 0x36, 0x3E, 0x5C, 0x8D,
];

// ── Advertised service families ──────────────────────────────────────────────

/// A WHOOP custom GATT service family as seen in an advertisement.
///
/// Only [`Self::Whoop4`] and [`Self::Whoop5Fd4b`] are connectable. The
/// other three are protocol facts with no mapped framing, so they are reported
/// and refused rather than guessed at: connecting to one and writing a frame
/// built for a different family is how a scan turns into an unexplained strap
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceFamily {
    /// WHOOP 4.0 — `61080001-…`.
    Whoop4,
    /// WHOOP 5.0 / MG — `fd4b0001-…`.
    Whoop5Fd4b,
    /// `11500001-…` — framing and operations unresolved.
    Service1150,
    /// `8a580001-…` — framing and operations unresolved.
    Monument,
    /// `59830001-…` — framing and operations unresolved.
    Symphony,
}

impl ServiceFamily {
    /// Every family this crate can recognise in an advertisement.
    pub const ALL: [ServiceFamily; 5] = [
        Self::Whoop4,
        Self::Whoop5Fd4b,
        Self::Service1150,
        Self::Monument,
        Self::Symphony,
    ];

    /// The advertised service UUID.
    pub const fn service_uuid(self) -> Uuid {
        match self {
            Self::Whoop4 => WHOOP4_SERVICE,
            Self::Whoop5Fd4b => WHOOP5_SERVICE,
            Self::Service1150 => Uuid::from_u128(0x11500001_6215_11ee_8c99_0242ac120002),
            Self::Monument => Uuid::from_u128(0x8a580001_2fe8_4796_9267_b87a2b0c8234),
            Self::Symphony => Uuid::from_u128(0x59830001_5955_419b_bb8d_c8262926af23),
        }
    }

    /// Which wire profile this service speaks, or `None` when that is unmapped.
    pub const fn connectable_family(self) -> Option<DeviceFamily> {
        match self {
            Self::Whoop4 => Some(DeviceFamily::Whoop4),
            Self::Whoop5Fd4b => Some(DeviceFamily::Whoop5),
            Self::Service1150 | Self::Monument | Self::Symphony => None,
        }
    }

    /// Whether this crate will enter GATT on this service.
    pub const fn is_connectable(self) -> bool {
        self.connectable_family().is_some()
    }

    /// Display name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Whoop4 => "WHOOP 4.0",
            Self::Whoop5Fd4b => "WHOOP 5.0 / MG (fd4b)",
            Self::Service1150 => "WHOOP PUFFIN service 1150",
            Self::Monument => "WHOOP MONUMENT",
            Self::Symphony => "WHOOP SYMPHONY",
        }
    }

    /// Recognise an advertised service UUID.
    pub fn from_uuid(uuid: Uuid) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.service_uuid() == uuid)
    }
}

// ── Standard SIG services ────────────────────────────────────────────────────

/// Heart Rate service (`180D`). Works **unbonded** on both generations and is a
/// source entirely separate from the custom command replies.
pub const HEART_RATE_SERVICE: Uuid = Uuid::from_u128(0x0000180d_0000_1000_8000_00805f9b34fb);
/// Heart Rate Measurement (`2A37`) — flags, 8- or 16-bit HR, optional energy
/// expended, optional R-R intervals in units of 1/1024 s.
pub const HEART_RATE_MEASUREMENT: Uuid = Uuid::from_u128(0x00002a37_0000_1000_8000_00805f9b34fb);
/// Battery service (`180F`).
pub const BATTERY_SERVICE: Uuid = Uuid::from_u128(0x0000180f_0000_1000_8000_00805f9b34fb);
/// Battery Level (`2A19`) — a single byte of percent. Some WHOOP 4 straps
/// report a constant 100 here, so it is not authoritative when the proprietary
/// source is available.
pub const BATTERY_LEVEL: Uuid = Uuid::from_u128(0x00002a19_0000_1000_8000_00805f9b34fb);
/// Device Information service (`180A`).
pub const DEVICE_INFORMATION_SERVICE: Uuid =
    Uuid::from_u128(0x0000180a_0000_1000_8000_00805f9b34fb);
/// Model Number String (`2A24`) — `MG` identifies an MG.
pub const MODEL_NUMBER_STRING: Uuid = Uuid::from_u128(0x00002a24_0000_1000_8000_00805f9b34fb);
/// Serial Number String (`2A25`) — prefix `5AM` is MG, `5AG` is 5.0. Absent on
/// WHOOP 4, whose serial only appears in the command-35 hello.
pub const SERIAL_NUMBER_STRING: Uuid = Uuid::from_u128(0x00002a25_0000_1000_8000_00805f9b34fb);
/// Hardware Revision String (`2A27`) — containing `WG50` reads as a 5.0.
pub const HARDWARE_REVISION_STRING: Uuid = Uuid::from_u128(0x00002a27_0000_1000_8000_00805f9b34fb);

// ── Variant resolution ───────────────────────────────────────────────────────

/// Which WHOOP 5-family hardware a strap is.
///
/// Both labels share the `fd4b` service and the same envelope, so this changes
/// nothing about parsing. What differs is hardware: an MG carries the
/// ECG-conductive clasp, a 5.0 does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Whoop5Variant {
    /// WHOOP 5.0 — no ECG clasp.
    Whoop50,
    /// WHOOP MG — ECG-capable.
    Mg,
    /// Signals absent or contradictory.
    Unknown,
}

/// Resolve the 5.0/MG variant from Device Information Service strings.
///
/// Contradictory inputs resolve to [`Whoop5Variant::Unknown`] rather than
/// picking a side, and an unresolved variant does **not** establish that the
/// strap lacks MG hardware: an MG has been observed with a serial prefix and
/// hardware revision outside this table, so absence of a recognised token is
/// not evidence of absence.
pub fn whoop5_variant(
    serial: Option<&str>,
    hardware_revision: Option<&str>,
    model_number: Option<&str>,
) -> Whoop5Variant {
    let model_says_mg = model_number
        .map(|m| m.trim().eq_ignore_ascii_case("MG"))
        .unwrap_or(false);
    let serial_says_mg = serial.map(|s| s.starts_with("5AM")).unwrap_or(false);
    let serial_says_50 = serial.map(|s| s.starts_with("5AG")).unwrap_or(false);
    let hw_says_50 = hardware_revision
        .map(|h| h.contains("WG50"))
        .unwrap_or(false);

    let mg = model_says_mg || serial_says_mg;
    let fifty = serial_says_50 || hw_says_50;

    match (mg, fifty) {
        (true, false) => Whoop5Variant::Mg,
        (false, true) => Whoop5Variant::Whoop50,
        // A `5AM` serial beside a `WG50` hardware revision is exactly the
        // conflict the reference leaves unresolved. So does no signal at all.
        _ => Whoop5Variant::Unknown,
    }
}
