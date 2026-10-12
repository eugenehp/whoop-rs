//! Tests for the whole decode surface.
//!
//! Kept together rather than split per submodule: the frame builders below are
//! shared by all of them, and a records test routinely checks that a *live*
//! packet type does not reach a historical decoder.

use super::*;
use crate::protocol::{
    crc32, layout, packet, verify_frame, CommandResult, DeviceFamily, MetadataType,
};
use crate::sim::FrameBuilder;
use crate::types::{HrSource, Record};

/// A WHOOP 4 frame of `ptype`, `len` bytes long, with `version` in the
/// sequence byte and a correct envelope. Fields are written at **absolute**
/// frame offsets, then [`seal`] recomputes the payload checksum.
fn whoop4_frame(ptype: u8, version: u8, len: usize) -> Vec<u8> {
    FrameBuilder::new(DeviceFamily::Whoop4, ptype, version, 0, len).finish()
}

/// A WHOOP 5 frame of `ptype`, `len` bytes long, with `layout` at byte 9.
///
/// `len` must be a multiple of four: the inner record is padded to a four-byte
/// boundary, so no other size is a frame this framing can produce.
fn whoop5_frame(ptype: u8, layout_byte: u8, len: usize) -> Vec<u8> {
    FrameBuilder::new(DeviceFamily::Whoop5, ptype, layout_byte, 0x80, len).finish()
}

/// Recompute a frame's payload checksum after its body was edited in place, and
/// assert the result is intact.
///
/// Every fixture goes through this, so a test can never assert against a frame
/// a real strap could not have sent.
fn seal(frame: &mut [u8], family: DeviceFamily) {
    let start = family.inner_offset();
    let end = frame.len() - 4;
    let crc = crc32(&frame[start..end]).to_le_bytes();
    frame[end..].copy_from_slice(&crc);
    assert!(
        verify_frame(frame, family).ok,
        "fixture must be a valid frame"
    );
}

fn seal_whoop4(frame: &mut [u8]) {
    seal(frame, DeviceFamily::Whoop4);
}

fn seal_whoop5(frame: &mut [u8]) {
    seal(frame, DeviceFamily::Whoop5);
}

// ── Standard service ─────────────────────────────────────────────────────

#[test]
fn hr_measurement_reads_8bit_hr_with_rr() {
    // flags 0x10 = 8-bit HR + R-R present; 1024 ticks = 1000 ms.
    let data = [0x10, 60, 0x00, 0x04, 0x00, 0x02];
    let hr = hr_measurement(&data).unwrap();
    assert_eq!(hr.bpm, 60);
    assert_eq!(hr.rr_ms, vec![1000, 500]);
    assert_eq!(hr.source, HrSource::StandardService);
    // The standard service carries no device clock, and inventing one from
    // arrival time would make a jittery link look like a drifting strap.
    assert_eq!(hr.unix_seconds, None);
}

#[test]
fn hr_measurement_skips_energy_expended() {
    // flags 0x18 = energy expended + R-R. The energy word must not be read
    // as the first interval.
    let data = [0x18, 72, 0xFF, 0xFF, 0x00, 0x04];
    let hr = hr_measurement(&data).unwrap();
    assert_eq!(hr.bpm, 72);
    assert_eq!(hr.rr_ms, vec![1000]);
}

#[test]
fn hr_measurement_ignores_an_incomplete_rr_word() {
    let data = [0x10, 60, 0x00, 0x04, 0x11];
    let hr = hr_measurement(&data).unwrap();
    assert_eq!(hr.rr_ms, vec![1000]);
    assert!(hr_measurement(&[]).is_none());
}

// ── Command responses ────────────────────────────────────────────────────

#[test]
fn whoop4_response_correlates_on_the_origin_sequence() {
    let mut frame = whoop4_frame(packet::COMMAND_RESPONSE, 0x99, 16);
    frame[6] = 26; // command
    frame[7] = 0x42; // originating request sequence
    frame[8] = 1; // result
    frame[9] = 0x64; // body
    seal_whoop4(&mut frame);

    let r = command_response(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(r.command, 26);
    assert_eq!(r.name, "GET_BATTERY_LEVEL");
    assert_eq!(r.origin_sequence, 0x42);
    // The response frame's own sequence is not the request echo, and using
    // it to correlate would match the wrong outstanding request.
    assert_eq!(r.response_sequence, 0x99);
    assert_eq!(r.result, CommandResult::Success);
    assert_eq!(r.body, vec![0x64, 0, 0]);
}

#[test]
fn whoop5_response_uses_its_own_offsets() {
    let mut frame = whoop5_frame(packet::COMMAND_RESPONSE, 0x07, 24);
    frame[10] = 26;
    frame[11] = 0x42;
    frame[12] = 1;
    frame[13] = 47;
    seal_whoop5(&mut frame);

    let r = command_response(&frame, DeviceFamily::Whoop5).unwrap();
    assert_eq!(r.command, 26);
    assert_eq!(r.origin_sequence, 0x42);
    assert_eq!(r.response_sequence, 0x07);
    assert_eq!(r.result, CommandResult::Success);
    assert_eq!(r.body[0], 47);
}

#[test]
fn unknown_result_bytes_are_preserved_not_coerced() {
    let mut frame = whoop4_frame(packet::COMMAND_RESPONSE, 1, 16);
    frame[6] = 26;
    frame[8] = 9;
    seal_whoop4(&mut frame);
    let r = command_response(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(r.result, CommandResult::Unknown(9));
    // An unknown result must not decode a body as if the request succeeded.
    assert!(response_payload(&r, DeviceFamily::Whoop4).is_none());
}

#[test]
fn battery_bodies_differ_by_family() {
    // WHOOP 4: u16le tenths of a percent.
    let four = battery_from_command(&[0xE2, 0x01], DeviceFamily::Whoop4).unwrap();
    assert!((four.percent - 48.2).abs() < 0.01);
    // WHOOP 5: a four-byte whole percent.
    let five = battery_from_command(&[47, 0, 0, 0], DeviceFamily::Whoop5).unwrap();
    assert_eq!(five.percent, 47.0);
    // Reading WHOOP 4's body with the WHOOP 5 width is not merely imprecise.
    assert!(battery_from_command(&[0xE2, 0x01, 0x00, 0x00], DeviceFamily::Whoop5).is_none());
}

#[test]
fn data_range_failure_body_is_not_a_range() {
    // Revision 1 plus 64 zero bytes is the documented failure path.
    let mut body = vec![1u8];
    body.extend_from_slice(&[0u8; 64]);
    let range = data_range(&body).unwrap();
    assert_eq!(range.capacity, 0);
    // Every clock pair is unset, so there is no newest time to report.
    assert_eq!(range.newest_unix(), None);
    // A short body is not a range at all.
    assert!(data_range(&[1, 0, 0]).is_none());
}

#[test]
fn data_range_reads_sixteen_words_after_the_revision() {
    let mut body = vec![1u8];
    for i in 0..16u32 {
        body.extend_from_slice(&(i + 1).to_le_bytes());
    }
    let range = data_range(&body).unwrap();
    assert_eq!(range.boundary, 1);
    assert_eq!(range.read_page, 2);
    assert_eq!(range.write_page, 3);
    assert_eq!(range.trim_boundary, 4);
    assert_eq!(range.capacity, 6);
    assert_eq!(range.clock_pairs[0], (9, 10));
    assert_eq!(range.clock_pairs[3], (15, 16));
}

#[test]
fn hello_reports_firmware_without_touching_identity_blocks() {
    let mut body = vec![0u8; 111];
    body[0] = 1;
    body[1..5].copy_from_slice(&470u32.to_le_bytes()); // 47.0 %
    body[6..10].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    // Identity text that a careless decoder would pick up.
    body[14..25].copy_from_slice(b"SECRETNAME\0");
    body[91] = 50;
    body[92] = 42;
    body[93] = 1;
    body[94..98].copy_from_slice(&0u32.to_le_bytes());
    body[103..107].copy_from_slice(&0x30u32.to_le_bytes());

    let h = hello(&body).unwrap();
    assert_eq!(h.firmware, "50.42.1.0");
    assert!((h.battery_percent - 47.0).abs() < 0.01);
    assert_eq!(h.unix_seconds, 1_700_000_000);
    // Both "block could not be filled" bits, preserved beside a success.
    assert_eq!(h.preparation_bits, 0x30);
    // Nothing from the identity region reaches the decoded struct.
    let json = serde_json::to_string(&h).unwrap();
    assert!(!json.contains("SECRETNAME"), "{json}");

    // An unsupported revision is a failure body, not a hello.
    body[0] = 2;
    assert!(hello(&body).is_none());
}

// ── Events ───────────────────────────────────────────────────────────────

#[test]
fn whoop4_battery_event_carries_volts_and_charging() {
    let mut frame = whoop4_frame(packet::EVENT, 1, 36);
    frame[6] = 3; // BATTERY_LEVEL
    frame[8..12].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[17..19].copy_from_slice(&853u16.to_le_bytes()); // 85.3 %
    frame[21..23].copy_from_slice(&3987u16.to_le_bytes());
    frame[26] = 1;
    seal_whoop4(&mut frame);

    let e = strap_event(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(e.name, "BATTERY_LEVEL");
    assert_eq!(e.unix_seconds, 1_700_000_000);
    let b = e.battery.unwrap();
    assert!((b.percent - 85.3).abs() < 0.01);
    assert_eq!(b.millivolts, Some(3987));
    assert_eq!(b.charging, Some(true));
}

#[test]
fn an_unnamed_event_keeps_its_number() {
    let mut frame = whoop4_frame(packet::EVENT, 1, 16);
    frame[6] = 201;
    seal_whoop4(&mut frame);
    let e = strap_event(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(e.number, 201);
    assert_eq!(e.name, "UNKNOWN_EVENT");
    // The dense battery layout belongs to event 3 on WHOOP 4 only.
    assert!(e.battery.is_none());
}

#[test]
fn whoop5_battery_event_does_not_borrow_whoop4_offsets() {
    let mut frame = whoop5_frame(packet::EVENT, 0, 36);
    frame[10] = 3;
    frame[12..16].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    // Bytes a WHOOP 4 decoder would read as percent and millivolts.
    frame[17..19].copy_from_slice(&853u16.to_le_bytes());
    seal_whoop5(&mut frame);
    let e = strap_event(&frame, DeviceFamily::Whoop5).unwrap();
    assert_eq!(e.unix_seconds, 1_700_000_000);
    assert!(e.battery.is_none(), "WHOOP 4 event offsets must not apply");
}

// ── Live heart rate ──────────────────────────────────────────────────────

#[test]
fn whoop4_realtime_rr_is_already_milliseconds() {
    let mut frame = whoop4_frame(packet::REALTIME_DATA, 1, 24);
    frame[6..10].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[12] = 58;
    frame[13] = 3;
    frame[14..16].copy_from_slice(&1000u16.to_le_bytes());
    frame[16..18].copy_from_slice(&0u16.to_le_bytes()); // placeholder
    frame[18..20].copy_from_slice(&1020u16.to_le_bytes());
    seal_whoop4(&mut frame);

    let hr = realtime_hr(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(hr.bpm, 58);
    // The zero is a placeholder, not a measured interval.
    assert_eq!(hr.rr_ms, vec![1000, 1020]);
    assert_eq!(hr.unix_seconds, Some(1_700_000_000));
}

#[test]
fn whoop5_realtime_rr_is_ticks() {
    let mut frame = whoop5_frame(packet::REALTIME_DATA, 0, 32);
    frame[10..14].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[16] = 61;
    frame[17] = 2;
    frame[18..20].copy_from_slice(&1024u16.to_le_bytes());
    frame[20..22].copy_from_slice(&1000u16.to_le_bytes());
    seal_whoop5(&mut frame);

    let hr = realtime_hr(&frame, DeviceFamily::Whoop5).unwrap();
    assert_eq!(hr.bpm, 61);
    // 1024 ticks is exactly 1000 ms; 1000 ticks is 977, not 1000.
    assert_eq!(hr.rr_ms, vec![1000, 977]);
}

#[test]
fn a_declared_count_cannot_read_past_the_payload() {
    let mut frame = whoop4_frame(packet::REALTIME_DATA, 1, 20);
    frame[12] = 60;
    frame[13] = 200; // lying about how many intervals follow
    frame[14..16].copy_from_slice(&1000u16.to_le_bytes());
    seal_whoop4(&mut frame);
    let hr = realtime_hr(&frame, DeviceFamily::Whoop4).unwrap();
    // Only the complete words inside the payload are consumed, and the CRC
    // trailer is never read as an interval.
    assert_eq!(hr.rr_ms, vec![1000]);
}

// ── Metadata ─────────────────────────────────────────────────────────────

#[test]
fn whoop4_history_end_block_is_copied_verbatim() {
    let mut frame = whoop4_frame(packet::METADATA, 1, 32);
    frame[6] = 2; // HISTORY_END
    frame[7..11].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[17..21].copy_from_slice(&112_193u32.to_le_bytes());
    frame[21..25].copy_from_slice(&7u32.to_le_bytes());
    seal_whoop4(&mut frame);

    let meta = history_meta(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(meta.kind, MetadataType::HistoryEnd);
    assert_eq!(meta.trim_cursor, Some(112_193));
    assert_eq!(meta.unix_seconds, Some(1_700_000_000));
    // Both words, exactly as they arrived: the wrap-state half is not
    // optional just because the ordinary path only uses the first.
    let block = meta.end_block.unwrap();
    assert_eq!(&block[..4], &112_193u32.to_le_bytes());
    assert_eq!(&block[4..], &7u32.to_le_bytes());
}

#[test]
fn whoop5_history_end_block_sits_four_bytes_later() {
    let mut frame = whoop5_frame(packet::METADATA, 0, 40);
    frame[10] = 2;
    frame[11..15].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[21..25].copy_from_slice(&112_193u32.to_le_bytes());
    frame[25..29].copy_from_slice(&0xFEFE_FEFEu32.to_le_bytes());
    seal_whoop5(&mut frame);

    let meta = history_meta(&frame, DeviceFamily::Whoop5).unwrap();
    assert_eq!(meta.trim_cursor, Some(112_193));
    let block = meta.end_block.unwrap();
    // A special token in the second word survives the round trip, which a
    // block rebuilt from the trim cursor would have destroyed.
    assert_eq!(&block[4..], &0xFEFE_FEFEu32.to_le_bytes());
}

#[test]
fn start_and_complete_carry_no_ack_token() {
    for (byte, kind) in [
        (1u8, MetadataType::HistoryStart),
        (3, MetadataType::HistoryComplete),
    ] {
        let mut frame = whoop4_frame(packet::METADATA, 1, 32);
        frame[6] = byte;
        seal_whoop4(&mut frame);
        let meta = history_meta(&frame, DeviceFamily::Whoop4).unwrap();
        assert_eq!(meta.kind, kind);
        assert!(meta.end_block.is_none());
    }
}

#[test]
fn a_truncated_end_marker_yields_no_token() {
    // 20 bytes cannot hold frame[17..25], so there is nothing to echo —
    // and nothing to echo must mean no acknowledgement, not a short one.
    let mut frame = whoop4_frame(packet::METADATA, 1, 20);
    frame[6] = 2;
    seal_whoop4(&mut frame);
    let meta = history_meta(&frame, DeviceFamily::Whoop4).unwrap();
    assert_eq!(meta.kind, MetadataType::HistoryEnd);
    assert!(meta.end_block.is_none());
}

#[test]
fn an_unknown_metadata_byte_is_not_a_history_marker() {
    let mut frame = whoop4_frame(packet::METADATA, 1, 32);
    frame[6] = 9;
    seal_whoop4(&mut frame);
    assert!(history_meta(&frame, DeviceFamily::Whoop4).is_none());
}

// ── Historical records ───────────────────────────────────────────────────

#[test]
fn r18_decodes_the_documented_fields() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R18_BIOMETRIC, 124);
    frame[11..15].copy_from_slice(&4242u32.to_le_bytes());
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[22] = 54;
    frame[23] = 2;
    frame[24..26].copy_from_slice(&1024u16.to_le_bytes());
    frame[26..28].copy_from_slice(&1100u16.to_le_bytes());
    frame[36] = 0xA0;
    frame[41..45].copy_from_slice(&0.25f32.to_le_bytes());
    frame[45..49].copy_from_slice(&0.0f32.to_le_bytes());
    frame[49..53].copy_from_slice(&0.0f32.to_le_bytes());
    frame[53..57].copy_from_slice(&1.0f32.to_le_bytes());
    frame[57..59].copy_from_slice(&8123u16.to_le_bytes());
    frame[63] = 1;
    frame[73..75].copy_from_slice(&3350u16.to_le_bytes()); // 33.50 °C
    seal_whoop5(&mut frame);

    let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected a biometric record");
    };
    assert_eq!(r.record_index, Some(4242));
    assert_eq!(r.unix_seconds, 1_700_000_000);
    assert_eq!(r.heart_rate.bpm, 54);
    assert_eq!(r.heart_rate.rr_ms, vec![1000, 1074]);
    assert_eq!(r.steps, Some(8123));
    assert_eq!(r.activity, Some(1));
    assert_eq!(r.quality_flags, Some(0xA0));
    assert!((r.skin_temp_c.unwrap() - 33.50).abs() < 0.01);
    assert!((r.dynamic_accel_g.unwrap() - 0.25).abs() < 0.001);
    assert_eq!(r.gravity_g, Some([0.0, 0.0, 1.0]));
}

#[test]
fn r18_refuses_implausible_temperature_and_activity() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R18_BIOMETRIC, 124);
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[63] = 0xFF; // an input class that is not still/walk/run
    frame[73..75].copy_from_slice(&0u16.to_le_bytes()); // 0.00 °C
    seal_whoop5(&mut frame);
    let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected a biometric record");
    };
    // Reported as absent rather than as 0 °C or as "still".
    assert_eq!(r.skin_temp_c, None);
    assert_eq!(r.activity, None);
}

#[test]
fn r18_clamps_an_overlarge_rr_count() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R18_BIOMETRIC, 124);
    frame[15..19].copy_from_slice(&1u32.to_le_bytes());
    frame[23] = 200;
    for i in 0..8 {
        frame[24 + i * 2..26 + i * 2].copy_from_slice(&1024u16.to_le_bytes());
    }
    seal_whoop5(&mut frame);
    let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected a biometric record");
    };
    // The layout holds at most four words; the rest of the frame is other
    // fields, not more intervals.
    assert_eq!(r.heart_rate.rr_ms.len(), 4);
}

#[test]
fn a_wrong_length_record_is_preserved_rather_than_decoded() {
    // R18's selector on a frame that is not 124 bytes.
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R18_BIOMETRIC, 120);
    seal_whoop5(&mut frame);
    let record = historical(&frame, DeviceFamily::Whoop5);
    let Record::Unknown {
        version,
        length,
        raw,
        ..
    } = record
    else {
        panic!("expected the record to be preserved, not decoded");
    };
    assert_eq!(version, layout::R18_BIOMETRIC);
    assert_eq!(length, 120);
    assert_eq!(raw, frame);
}

#[test]
fn an_unknown_layout_keeps_its_bytes() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, 99, 64);
    seal_whoop5(&mut frame);
    let Record::Unknown {
        version, raw, note, ..
    } = historical(&frame, DeviceFamily::Whoop5)
    else {
        panic!("expected an unknown record");
    };
    assert_eq!(version, 99);
    assert_eq!(raw.len(), 64);
    assert_eq!(note, "unknown WHOOP 5 layout selector");
}

#[test]
fn r22_is_preserved_because_no_decoder_exists() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R22_QUEUED, 64);
    seal_whoop5(&mut frame);
    assert!(matches!(
        historical(&frame, DeviceFamily::Whoop5),
        Record::Unknown {
            note: "R22 queued channels are not decoded",
            ..
        }
    ));
}

#[test]
fn r20_decodes_only_the_declared_samples() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R20_OPTICAL, 2140);
    frame[10] = 0x81;
    frame[11..15].copy_from_slice(&7u32.to_le_bytes());
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());

    let bases = [26usize, 448, 870, 1292, 1714];
    // Counts [12, 0, 0, 12, 12] — the documented count-only example, which
    // populates 3 × 2 × 12 = 72 values out of 500 available positions.
    for (base, count) in bases.iter().zip([12u8, 0, 0, 12, 12]) {
        frame[*base] = count;
        frame[base + 1] = 3; // source A
        frame[base + 4] = 4; // source B
        frame[base + 2..base + 4].copy_from_slice(&1600u16.to_le_bytes());
        for i in 0..count as usize {
            frame[base + 21 + i * 4..base + 25 + i * 4]
                .copy_from_slice(&(-1000i32 - i as i32).to_le_bytes());
            frame[base + 221 + i * 4..base + 225 + i * 4]
                .copy_from_slice(&(2000i32 + i as i32).to_le_bytes());
        }
        // A slot past the count holds a value that must never be decoded.
        frame[base + 21 + 49 * 4..base + 25 + 49 * 4]
            .copy_from_slice(&0x7FFF_FFFFi32.to_le_bytes());
    }
    seal_whoop5(&mut frame);

    let Record::Optical(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected an optical record");
    };
    assert_eq!(r.marker, 0x81);
    assert_eq!(r.blocks.len(), 5);
    let populated: usize = r
        .blocks
        .iter()
        .map(|b| b.slot_a.len() + b.slot_b.len())
        .sum();
    assert_eq!(populated, 72);
    // A count of zero is no readings, not fifty measured zeros.
    assert!(r.blocks[1].slot_a.is_empty());
    // Negative optical counts stay negative rather than becoming large
    // unsigned measurements.
    assert_eq!(r.blocks[0].slot_a[0], -1000);
    assert_eq!(r.blocks[0].slot_b[11], 2011);
    assert_eq!(r.blocks[0].drive_a, 1600);
}

#[test]
fn r20_rejects_an_out_of_range_count() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R20_OPTICAL, 2140);
    frame[26] = 51; // capacity is 50 per slot
    seal_whoop5(&mut frame);
    assert!(matches!(
        historical(&frame, DeviceFamily::Whoop5),
        Record::Unknown {
            note: "R20 counts or samples did not validate",
            ..
        }
    ));
}

#[test]
fn r21_allows_independent_axis_counts() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R21_IMU, 1244);
    frame[11..15].copy_from_slice(&11u32.to_le_bytes());
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[19..21].copy_from_slice(&16384u16.to_le_bytes()); // half a second
    frame[24..26].copy_from_slice(&3u16.to_le_bytes());
    frame[630..632].copy_from_slice(&2u16.to_le_bytes());
    // One g on z, and a known gyro rate on x.
    frame[428..430].copy_from_slice(&4096i16.to_le_bytes());
    frame[640..642].copy_from_slice(&16384i16.to_le_bytes());
    seal_whoop5(&mut frame);

    let Record::Imu(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected an IMU record");
    };
    // The counts are independent; requiring them to match would drop data.
    assert_eq!(r.accel_g.len(), 3);
    assert_eq!(r.gyro_dps.len(), 2);
    assert!((r.accel_g[0][2] - 1.0).abs() < 1e-6);
    assert!((r.gyro_dps[0][0] - 1000.0).abs() < 1e-3);
    assert!((r.timestamp - 1_700_000_000.5).abs() < 1e-6);
}

#[test]
fn r21_clamps_a_count_above_capacity() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R21_IMU, 1244);
    frame[15..19].copy_from_slice(&1u32.to_le_bytes());
    frame[24..26].copy_from_slice(&500u16.to_le_bytes());
    seal_whoop5(&mut frame);
    let Record::Imu(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected an IMU record");
    };
    // Each column reserves 200 bytes: 100 samples, never more.
    assert_eq!(r.accel_g.len(), 100);
}

#[test]
fn r26_reconstructs_a_window_from_deltas() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R26_COMPACT_OPTICAL, 88);
    frame[11..15].copy_from_slice(&3u32.to_le_bytes());
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[21..23].copy_from_slice(&0u16.to_le_bytes()); // zero is valid
    frame[23..27].copy_from_slice(&4_000_000_000u32.to_le_bytes());
    for i in 0..24 {
        frame[27 + i * 2..29 + i * 2].copy_from_slice(&(-100i16).to_le_bytes());
    }
    frame[75..79].copy_from_slice(&0.5f32.to_le_bytes());
    frame[81] = 1;
    seal_whoop5(&mut frame);

    let Record::CompactOptical(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected a compact optical record");
    };
    assert_eq!(r.samples.len(), 25);
    // A base near the top of u32 with negative deltas needs an accumulator
    // wider than either end came in.
    assert_eq!(r.samples[0], 4_000_000_000);
    assert_eq!(r.samples[24], 4_000_000_000 - 2400);
    assert_eq!(r.burst, 0);
}

#[test]
fn r17_reads_only_the_declared_samples_and_never_the_alignment_word() {
    let mut frame = whoop5_frame(packet::REALTIME_RAW_DATA, layout::R17_ECG_FILTERED, 240);
    frame[11..15].copy_from_slice(&5u32.to_le_bytes());
    frame[21] = 2;
    frame[32..34].copy_from_slice(&3u16.to_le_bytes());
    for (i, v) in [100i16, 0, -100].into_iter().enumerate() {
        frame[34 + i * 2..36 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    // The alignment word, which is never a sample.
    frame[234..236].copy_from_slice(&0x7FFFi16.to_le_bytes());
    seal_whoop5(&mut frame);

    let Record::Ecg(r) = whoop5_record(&frame, packet::REALTIME_RAW_DATA) else {
        panic!("expected an ECG record");
    };
    assert_eq!(r.layout, 17);
    assert_eq!(r.quality, 2);
    // A counted zero is an entry, not padding to discard.
    assert_eq!(r.samples, vec![100, 0, -100]);
}

#[test]
fn r16_decodes_18_bit_twos_complement_with_flags() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R16_ECG_RAW, 1584);
    frame[32..34].copy_from_slice(&2u16.to_le_bytes());
    // raw18 = 0x3FFFF = −1, with flag 6 set. Bits 2–5 are reserved and set
    // here to prove they are ignored rather than folded into the sample.
    frame[34] = 0b0111_1111;
    frame[35] = 0xFF;
    frame[36] = 0xFF;
    // raw18 = 1, flag 6 clear.
    frame[37] = 0b0000_0000;
    frame[38] = 0x00;
    frame[39] = 0x01;
    seal_whoop5(&mut frame);

    let Record::Ecg(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected an ECG record");
    };
    assert_eq!(r.samples, vec![-1, 1]);
    assert_eq!(r.sample_flags, vec![true, false]);
}

#[test]
fn an_ecg_count_above_capacity_is_an_anomaly_not_a_truncation() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R17_ECG_FILTERED, 240);
    frame[32..34].copy_from_slice(&101u16.to_le_bytes());
    seal_whoop5(&mut frame);
    assert!(matches!(
        historical(&frame, DeviceFamily::Whoop5),
        Record::Unknown { .. }
    ));
}

// ── WHOOP 4 historical ───────────────────────────────────────────────────

#[test]
fn v24_rr_is_milliseconds_and_temperature_stays_raw() {
    let mut frame = whoop4_frame(packet::HISTORICAL_DATA, 24, 96);
    frame[11..15].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[21] = 52;
    frame[22] = 2;
    frame[23..25].copy_from_slice(&1150u16.to_le_bytes());
    frame[25..27].copy_from_slice(&1160u16.to_le_bytes());
    frame[40..44].copy_from_slice(&0.0f32.to_le_bytes());
    frame[44..48].copy_from_slice(&0.0f32.to_le_bytes());
    frame[48..52].copy_from_slice(&1.0f32.to_le_bytes());
    frame[72..74].copy_from_slice(&3350u16.to_le_bytes());
    seal_whoop4(&mut frame);

    let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop4) else {
        panic!("expected a biometric record");
    };
    assert_eq!(r.family, DeviceFamily::Whoop4);
    assert_eq!(r.heart_rate.bpm, 52);
    // Already milliseconds on this generation — no tick conversion.
    assert_eq!(r.heart_rate.rr_ms, vec![1150, 1160]);
    assert_eq!(r.gravity_g, Some([0.0, 0.0, 1.0]));
    // A raw ADC word with no documented conversion stays unreported rather
    // than borrowing the WHOOP 5 raw/100 scale, which would read 33.5 °C.
    assert_eq!(r.skin_temp_c, None);
    // v24 has no record index; absence is reported, not fabricated.
    assert_eq!(r.record_index, None);
}

#[test]
fn v24_and_v12_share_their_layout_map() {
    for version in [12u8, 24] {
        let mut frame = whoop4_frame(packet::HISTORICAL_DATA, version, 96);
        frame[11..15].copy_from_slice(&1_700_000_000u32.to_le_bytes());
        frame[21] = 60;
        seal_whoop4(&mut frame);
        let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop4) else {
            panic!("expected a biometric record for v{version}");
        };
        assert_eq!(r.version, version);
        assert_eq!(r.heart_rate.bpm, 60);
    }
}

#[test]
fn v25_is_a_different_layout_not_v24_with_offsets_moved() {
    let mut frame = whoop4_frame(packet::HISTORICAL_DATA, 25, 84);
    frame[11..15].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[73..75].copy_from_slice(&0i16.to_le_bytes());
    frame[75..77].copy_from_slice(&0i16.to_le_bytes());
    frame[77..79].copy_from_slice(&16384i16.to_le_bytes());
    seal_whoop4(&mut frame);

    let Record::Whoop4V25(r) = historical(&frame, DeviceFamily::Whoop4) else {
        panic!("expected a v25 record");
    };
    assert_eq!(r.unix_seconds, 1_700_000_000);
    // Raw words kept beside the observation-based 1/16384 g reading.
    assert_eq!(r.movement_raw[2], 16384);
    assert!((r.movement_g[2] - 1.0).abs() < 1e-6);
}

#[test]
fn a_v25_of_the_wrong_size_is_preserved() {
    let mut frame = whoop4_frame(packet::HISTORICAL_DATA, 25, 96);
    seal_whoop4(&mut frame);
    assert!(matches!(
        historical(&frame, DeviceFamily::Whoop4),
        Record::Unknown {
            note: "v25 is an 84-byte frame",
            ..
        }
    ));
}

#[test]
fn v5_7_9_decode_as_generic_hr_records() {
    for version in [5u8, 7, 9] {
        let mut frame = whoop4_frame(packet::HISTORICAL_DATA, version, 96);
        frame[11..15].copy_from_slice(&1_700_000_000u32.to_le_bytes());
        frame[21] = 48;
        frame[22] = 1;
        frame[23..25].copy_from_slice(&1250u16.to_le_bytes());
        // Bytes a v24 decoder would read as the sensor block. This layout
        // has no such block, so none of them may surface as measurements.
        frame[33..35].copy_from_slice(&9999u16.to_le_bytes());
        frame[72..74].copy_from_slice(&3350u16.to_le_bytes());
        seal_whoop4(&mut frame);

        let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop4) else {
            panic!("expected a biometric record for v{version}");
        };
        assert_eq!(r.version, version);
        assert_eq!(r.heart_rate.bpm, 48);
        assert_eq!(r.heart_rate.rr_ms, vec![1250]);
        // The sensor block belongs to v12/v24 only.
        assert!(r.whoop4_sensors.is_none());
        assert_eq!(r.gravity_g, None);
        assert_eq!(r.skin_temp_c, None);
    }
}

#[test]
fn an_unknown_whoop4_version_stays_raw() {
    let mut frame = whoop4_frame(packet::HISTORICAL_DATA, 99, 96);
    seal_whoop4(&mut frame);
    let Record::Unknown { note, raw, .. } = historical(&frame, DeviceFamily::Whoop4) else {
        panic!("expected v99 to be preserved");
    };
    assert_eq!(note, "unknown WHOOP 4 record version");
    assert_eq!(raw.len(), 96);
}

#[test]
fn v24_keeps_its_sensor_block_raw() {
    let mut frame = whoop4_frame(packet::HISTORICAL_DATA, 24, 96);
    frame[11..15].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[21] = 52;
    frame[33..35].copy_from_slice(&4100u16.to_le_bytes()); // green
    frame[35..37].copy_from_slice(&4200u16.to_le_bytes()); // red/IR
    frame[55] = 1; // on wrist
    frame[64..68].copy_from_slice(&1.0f32.to_le_bytes()); // gravity2 z
    frame[68..70].copy_from_slice(&5100u16.to_le_bytes()); // spo2 red
    frame[70..72].copy_from_slice(&5200u16.to_le_bytes()); // spo2 ir
    frame[72..74].copy_from_slice(&3350u16.to_le_bytes()); // skin temp ADC
    frame[74..76].copy_from_slice(&90u16.to_le_bytes()); // ambient
    frame[80..82].copy_from_slice(&16u16.to_le_bytes()); // respiration
    frame[82..84].copy_from_slice(&7u16.to_le_bytes()); // signal quality
    seal_whoop4(&mut frame);

    let Record::Biometric(r) = historical(&frame, DeviceFamily::Whoop4) else {
        panic!("expected a biometric record");
    };
    let block = r.whoop4_sensors.expect("v24 carries a sensor block");
    assert_eq!(block.ppg_green, 4100);
    assert_eq!(block.ppg_red_ir, 4200);
    assert_eq!(block.skin_contact, 1);
    assert_eq!(block.spo2_red, 5100);
    assert_eq!(block.resp_raw, 16);
    assert_eq!(block.signal_quality, 7);
    assert!((block.gravity2_g[2] - 1.0).abs() < 1e-6);
    // 3350 is a raw ADC word here. The R18 raw/100 scale would read it as
    // 33.5 °C, which is plausible and wrong, so no °C is reported at all.
    assert_eq!(block.skin_temp_raw, 3350);
    assert_eq!(r.skin_temp_c, None);
}

// ── WHOOP 4 live raw layouts ─────────────────────────────────────────────

#[test]
fn whoop4_raw_selects_the_imu_layout_by_payload_length() {
    // Declared 1,924 → payload 1,917 → the IMU variant.
    let mut frame = whoop4_frame(packet::REALTIME_RAW_DATA, 1, 1928);
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    frame[21] = 57;
    frame[22] = 2;
    frame[23..25].copy_from_slice(&1030u16.to_le_bytes());
    frame[25..27].copy_from_slice(&0u16.to_le_bytes()); // placeholder
                                                        // One g on acceleration Z, and a known rate on gyroscope X.
    frame[489..491].copy_from_slice(&4096i16.to_le_bytes());
    frame[692..694].copy_from_slice(&16384i16.to_le_bytes());
    seal_whoop4(&mut frame);

    let Some(Record::Whoop4RawImu(r)) = whoop4_raw(&frame) else {
        panic!("expected the 1,917-byte IMU variant");
    };
    assert_eq!(r.heart_rate.bpm, 57);
    assert_eq!(r.heart_rate.rr_ms, vec![1030]);
    assert_eq!(r.accel_g.len(), 100);
    assert_eq!(r.gyro_dps.len(), 100);
    assert!((r.accel_g[0][2] - 1.0).abs() < 1e-6);
    assert!((r.gyro_dps[0][0] - 1000.0).abs() < 1e-3);
}

#[test]
fn whoop4_raw_selects_the_optical_layout_four_bytes_later() {
    // Declared 1,928 → payload 1,921 → the optical variant. Four bytes
    // separate these two layouts; the packet type cannot tell them apart.
    let mut frame = whoop4_frame(packet::REALTIME_RAW_DATA, 1, 1932);
    frame[15..19].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    // One negative sample, with an aux byte that must not become data.
    frame[42] = 0xFF;
    frame[43] = 0xFF;
    frame[44] = 0xFF; // signed 24-bit −1
    frame[45] = 0x5A; // aux
    seal_whoop4(&mut frame);

    let Some(Record::Whoop4RawOptical(r)) = whoop4_raw(&frame) else {
        panic!("expected the 1,921-byte optical variant");
    };
    assert_eq!(r.samples[0], -1);
    // The fourth stride byte is retained rather than folded into the
    // sample or discarded.
    assert_eq!(r.aux[0], 0x5A);
    assert_eq!(r.samples.len(), 419);
    assert_eq!(r.config.len(), 27);
}

#[test]
fn an_unrecognised_whoop4_raw_length_is_not_decoded() {
    let mut frame = whoop4_frame(packet::REALTIME_RAW_DATA, 1, 64);
    seal_whoop4(&mut frame);
    assert!(whoop4_raw(&frame).is_none());
}

// ── Dedicated IMU streams ────────────────────────────────────────────────

#[test]
fn imu_stream_positions_every_array_from_the_counts() {
    let mut frame = whoop4_frame(packet::REALTIME_IMU_DATA_STREAM, 1, 128);
    let (a, g) = (4usize, 3usize);
    frame[24..26].copy_from_slice(&(a as u16).to_le_bytes());
    frame[26..28].copy_from_slice(&(g as u16).to_le_bytes());
    // Accel X at 28, Y at 28+2A, Z at 28+4A; gyro from 28+6A strided by G.
    let gyro_base = 28 + 6 * a;
    for i in 0..a {
        frame[28 + i * 2..30 + i * 2].copy_from_slice(&(100u16 + i as u16).to_le_bytes());
        frame[28 + 2 * a + i * 2..30 + 2 * a + i * 2]
            .copy_from_slice(&(200u16 + i as u16).to_le_bytes());
        frame[28 + 4 * a + i * 2..30 + 4 * a + i * 2]
            .copy_from_slice(&(300u16 + i as u16).to_le_bytes());
    }
    for i in 0..g {
        frame[gyro_base + i * 2..gyro_base + 2 + i * 2]
            .copy_from_slice(&(400u16 + i as u16).to_le_bytes());
    }
    seal_whoop4(&mut frame);

    let r = imu_stream(&frame, DeviceFamily::Whoop4, 51).expect("a valid span");
    assert_eq!((r.accel_count, r.gyro_count), (4, 3));
    assert_eq!(r.accel_raw[0], vec![100, 101, 102, 103]);
    assert_eq!(r.accel_raw[1][0], 200);
    assert_eq!(r.accel_raw[2][3], 303);
    assert_eq!(r.gyro_raw[0], vec![400, 401, 402]);
}

#[test]
fn an_imu_stream_count_that_overruns_the_payload_is_refused() {
    // A lying count here does not mis-slice one array — it slides every
    // later one — so the whole calculated span is validated first.
    let mut frame = whoop4_frame(packet::REALTIME_IMU_DATA_STREAM, 1, 64);
    frame[24..26].copy_from_slice(&1000u16.to_le_bytes());
    frame[26..28].copy_from_slice(&1000u16.to_le_bytes());
    seal_whoop4(&mut frame);
    assert!(imu_stream(&frame, DeviceFamily::Whoop4, 51).is_none());
}

// ── Service and configuration bodies ─────────────────────────────────────

#[test]
fn battery_pack_keeps_its_charge_value_unscaled() {
    let mut body = vec![0u8; 28];
    body[0] = 1;
    body[1] = 1; // present
    body[2..8].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
    body[8..16].copy_from_slice(b"BPK12345");
    body[24..26].copy_from_slice(&842u16.to_le_bytes());
    let pack = battery_pack(&body).unwrap();
    assert!(pack.present);
    assert_eq!(pack.serial, "BPK12345");
    // Not divided by ten: the wire does not establish a percentage scale.
    assert_eq!(pack.charge_raw, 842);
    // Short or wrong-revision bodies are not records.
    assert!(battery_pack(&body[..20]).is_none());
}

#[test]
fn extended_battery_maps_only_the_voltage() {
    let mut body = vec![0u8; 25];
    body[5..7].copy_from_slice(&3987u16.to_le_bytes());
    let battery = extended_battery(&body).unwrap();
    assert_eq!(battery.millivolts, 3987);
    // Everything else is structurally known and semantically not, so the
    // whole record is kept rather than being named field by field.
    assert_eq!(battery.raw.len(), 25);
    assert!(extended_battery(&body[..24]).is_none());
}

#[test]
fn advertising_name_excludes_its_terminator() {
    let mut body = vec![0u8; 19];
    body[0] = 1;
    body[1] = 1; // custom
    body[2] = 6; // NUL-inclusive length
    body[3..8].copy_from_slice(b"Strap");
    let name = advertising_name(&body).unwrap();
    assert_eq!(name.name, "Strap");
    assert!(name.custom);

    // Status 2 covers fallback, empty storage and a read failure alike.
    body[1] = 2;
    body[2] = 1;
    let fallback = advertising_name(&body).unwrap();
    assert_eq!(fallback.name, "");
    assert!(!fallback.custom);

    // A declared length can never reach past the fixed 16-byte storage.
    body[2] = 255;
    assert!(advertising_name(&body).is_some());
}

#[test]
fn body_location_decodes_only_where_the_bytes_mean_something() {
    // WHOOP 4: revision, location, confidence, status.
    let four = body_location(&[1, 2, 0, 0], DeviceFamily::Whoop4).unwrap();
    assert_eq!(four.location, Some(crate::protocol::BodyLocation::Bicep));
    assert_eq!(four.on_body, None);

    // WHOOP 5: [1, 0, 255, on_body] — the middle bytes are placeholders,
    // so decoding them as a location and a confidence would be reporting
    // "unknown, 255" as a measurement.
    let five = body_location(&[1, 0, 255, 1], DeviceFamily::Whoop5).unwrap();
    assert_eq!(five.location, None);
    assert_eq!(five.on_body, Some(true));
    // A value that is neither 0 nor 1 is not a boolean.
    let odd = body_location(&[1, 0, 255, 9], DeviceFamily::Whoop5).unwrap();
    assert_eq!(odd.on_body, None);
}

#[test]
fn config_value_distinguishes_a_fallback_zero_from_a_stored_one() {
    let mut body = vec![0u8; 65];
    body[0] = 1;
    body[1..21].copy_from_slice(b"cont_collection_mode");
    body[33] = b'1';
    let value = config_value(&body).unwrap();
    assert_eq!(value.key, "cont_collection_mode");
    assert_eq!(value.value, "1");
    assert!(!value.zero_filled);

    // An unreadable storage slot comes back as zeros with result 1, so a
    // zero readback has to be distinguishable from a stored zero.
    let mut zeroed = vec![0u8; 65];
    zeroed[0] = 1;
    zeroed[1..12].copy_from_slice(b"enable_rfid");
    let fallback = config_value(&zeroed).unwrap();
    assert_eq!(fallback.value, "");
    assert!(fallback.zero_filled);

    assert!(config_value(&body[..40]).is_none());
}

#[test]
fn an_enumeration_entry_needs_only_its_index() {
    // The end marker arrives with nothing after the index.
    let end = config_entry(&[1, 0xFF]).unwrap();
    assert_eq!(end.index, 0xFF);
    assert!(!end.valid);
    assert_eq!(end.key, None);

    let mut body = vec![1u8, 7, 1];
    body.extend_from_slice(b"enable_r22_packets\0");
    let entry = config_entry(&body).unwrap();
    assert_eq!(entry.index, 7);
    assert!(entry.valid);
    assert_eq!(entry.key.as_deref(), Some("enable_r22_packets"));

    // A contaminated field yields no name rather than control bytes.
    let noisy = config_entry(&[1, 8, 1, 0x01, 0x02, 0x00]).unwrap();
    assert_eq!(noisy.key, None);
    assert!(config_entry(&[1]).is_none());
}

#[test]
fn an_alarm_record_round_trips_through_its_own_builder() {
    let pattern = crate::protocol::payload::HapticPattern::default();
    let body = crate::protocol::payload::alarm_set_whoop5(2, 1_700_000_000_000, pattern);
    let record = alarm_record(&body).expect("SET and GET share this record");
    assert_eq!(record.id, 2);
    assert_eq!(record.unix_seconds, 1_700_000_000);
    assert_eq!(record.repeats, 7);
    assert_eq!(record.duration, 30);
    assert_eq!(record.crescendo, 0);
    assert!(record.is_set());

    // A cleared slot and a read fallback are indistinguishable here.
    let mut cleared = body.clone();
    cleared[2..6].copy_from_slice(&0u32.to_le_bytes());
    assert!(!alarm_record(&cleared).unwrap().is_set());
    // Wrong revision is not an alarm record.
    assert!(alarm_record(&[1u8; 21]).is_none());
}

#[test]
fn ecg_lead_diagnostics_sit_at_fixed_offsets() {
    let mut frame = whoop5_frame(packet::HISTORICAL_DATA, layout::R16_ECG_RAW, 1584);
    // A small waveform count, to prove the lead arrays do not move with it.
    frame[32..34].copy_from_slice(&2u16.to_le_bytes());
    frame[1534] = 10;
    frame[1535..1537].copy_from_slice(&1111u16.to_le_bytes());
    frame[1557..1559].copy_from_slice(&2222u16.to_le_bytes());
    seal_whoop5(&mut frame);

    let Record::Ecg(r) = historical(&frame, DeviceFamily::Whoop5) else {
        panic!("expected an ECG record");
    };
    let lead = r.lead_off.expect("R16 carries lead diagnostics");
    assert_eq!(lead.count, 10);
    assert_eq!(lead.i_channel.len(), 11);
    assert_eq!(lead.q_channel.len(), 11);
    assert_eq!(lead.i_channel[0], 1111);
    assert_eq!(lead.q_channel[0], 2222);
    // The packed status region is kept whole.
    assert_eq!(r.status.len(), 13);

    // R17 has no lead arrays at all; inventing empty ones would imply it
    // reported a clean contact state.
    let mut filtered = whoop5_frame(packet::HISTORICAL_DATA, layout::R17_ECG_FILTERED, 240);
    filtered[32..34].copy_from_slice(&1u16.to_le_bytes());
    seal_whoop5(&mut filtered);
    let Record::Ecg(r) = historical(&filtered, DeviceFamily::Whoop5) else {
        panic!("expected an ECG record");
    };
    assert!(r.lead_off.is_none());
}

// ── Console logs ──    // ── Console logs ─────────────────────────────────────────────────────────

#[test]
fn console_log_text_stops_before_the_trailer() {
    let mut frame = whoop4_frame(packet::CONSOLE_LOGS, 1, 24);
    frame[11..18].copy_from_slice(b"booting");
    seal_whoop4(&mut frame);
    let text = console_log(&frame, DeviceFamily::Whoop4).unwrap();
    assert!(text.starts_with("booting"));
    // Never the CRC bytes, whatever they happen to decode to.
    assert_eq!(text.len(), 24 - 4 - 11 - 1);
}
