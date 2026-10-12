//! Byte-to-record decoders for every frame this crate understands.
//!
//! Every function here is pure: no I/O, no shared state, no clock. That is what
//! makes the layouts testable from a capture and what lets the same code run in
//! a page, in a CLI and in a test.
//!
//! # Fail closed
//!
//! A decoder returns a record only when the layout it was written for is the
//! layout that arrived, which means checking the **validated frame length**, not
//! just the selector byte. Fixed regions in these records do not move when a
//! declared count is smaller, so a decoder that located arrays with `header +
//! count × width` would read the wrong bytes on every partly-populated record.
//! Anything unrecognised comes back as [`crate::types::Record::Unknown`] with its
//! bytes intact — a future firmware's layout then shows up as a preserved
//! unknown rather than as plausible-looking wrong numbers.
//!
//! # Offsets
//!
//! All offsets in the tables below are **absolute in the reassembled frame**,
//! and all integers are little-endian unless a doc comment says otherwise.
//! WHOOP 4 and WHOOP 5 offsets are never related by adding 4 unless a contract
//! says they are.

mod live;
mod records;
mod responses;

// One flat namespace: a caller decoding a frame does not care which file the
// decoder lives in, and the split is for whoever edits them.
pub use live::{battery_level, hr_measurement, imu_stream, realtime_hr, strap_event, whoop4_raw};
pub use records::{historical, whoop5_record};
pub use responses::{
    advertising_name, alarm_record, battery_from_command, battery_pack, body_location,
    command_response, config_count, config_entry, config_value, console_log, data_range,
    extended_battery, hello, history_meta, response_payload, DecodedResponse,
};

#[cfg(test)]
mod tests;
