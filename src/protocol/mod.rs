//! GATT UUIDs, frame envelopes, checksums and command builders for WHOOP straps.
//!
//! Two generations are covered, and they are **different protocols that share
//! vocabulary**: WHOOP 4.0 under service `61080001-…`, WHOOP 5.0/MG under
//! `fd4b0001-…`. A shared command name does not establish the same request or
//! response bytes, and WHOOP 5 history is not a WHOOP 4 layout with shifted
//! offsets. Everything here is selected by [`DeviceFamily`] first, offsets
//! second — see [`verify_frame`] and [`command_frame`].
//!
//! Version baselines for the layouts encoded here: WHOOP 5.0/MG firmware
//! **50.42.1.0**, WHOOP 4.0 **41.17.6.0**. A field being decodable is not a
//! promise that a given strap emits it.

mod command;
mod config;
mod framing;
mod gatt;
mod identity;
mod payload_impl;
mod record;

// One flat namespace, deliberately. The split below is for whoever edits this
// crate — six hundred lines of framing has nothing to say to four hundred lines
// of configuration inventory — but a caller does not care which file a constant
// lives in, and making them care would be a worse API for no benefit.
pub use command::{
    command_name, is_destructive, Command, CommandResult, MetadataType, SendPolicy, SendRefused,
    CONFIG_WRITE_COMMANDS, ECG_COMMANDS,
};
pub use config::{
    device_key, feature_flag, AlarmRunDetail, AlarmSetDetail, BodyLocation, ConfigValueType,
    DeviceKey, FeatureFlag, RebootProbe, Wrist, DEVICE_KEYS, FEATURE_FLAGS, FEATURE_FLAG_OFF,
    R22_FEATURE_KEYS,
};
pub use framing::{
    command_frame, crc16_modbus, crc32, crc8, f32le, frame_from_payload, i16le, i32le,
    is_offload_frame, packet, packet_type, packet_type_name, payload_end, u16le, u32le,
    verify_frame, whoop4_command_frame, whoop5_command_frame, whoop5_frame_with, FrameCheck,
    FrameRejectReason, Reassembler,
};
pub use gatt::{
    whoop5_variant, DeviceFamily, HeaderCrc, ServiceFamily, Whoop5Variant, BATTERY_LEVEL,
    BATTERY_SERVICE, DEVICE_INFORMATION_SERVICE, HARDWARE_REVISION_STRING, HEART_RATE_MEASUREMENT,
    HEART_RATE_SERVICE, MODEL_NUMBER_STRING, SERIAL_NUMBER_STRING, WHOOP4_COMMAND, WHOOP4_NOTIFY,
    WHOOP4_SERVICE, WHOOP5_CLIENT_HELLO, WHOOP5_COMMAND, WHOOP5_NOTIFY, WHOOP5_SERVICE,
};
pub use identity::{whoop4_serial, RepeatedSerialGate, WHOOP4_SERIAL_LENGTH, WHOOP4_SERIAL_OFFSET};
pub use record::{
    event, event_name, inertial_timestamp, layout, rr_ms_from_ticks, whoop5_layout_length,
};

/// Request bodies for the commands in [`Command`].
///
/// Every multibyte field is little-endian. Where two request forms exist for the
/// same operation, both are here under their own names rather than one being
/// picked silently.
pub mod payload {
    pub use super::payload_impl::*;
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
    command::admits_send(opcode, family, policy)
}

#[cfg(test)]
mod tests;
