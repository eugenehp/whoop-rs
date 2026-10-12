//! Tests for the whole protocol façade.
//!
//! Kept together rather than split per submodule: they cut across the split —
//! a framing test builds a command and a command test checks a payload — and
//! separating them would put each assertion in a different file from the
//! invariant it pins.

use super::*;

#[test]
fn crc32_matches_zlib_check_value() {
    // The standard CRC-32 check value: "123456789" → 0xCBF43926.
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn crc16_modbus_matches_check_value() {
    // The standard Modbus check value: "123456789" → 0x4B37.
    assert_eq!(crc16_modbus(b"123456789"), 0x4B37);
}

#[test]
fn crc8_is_the_poly_07_table() {
    // Table entry 1 is the polynomial itself; this pins table orientation.
    assert_eq!(crc8(&[0x01]), 0x07);
    assert_eq!(crc8(&[0x00]), 0x00);
}

#[test]
fn whoop4_round_trip_is_intact() {
    let frame = whoop4_command_frame(Command::GetBatteryLevel.opcode(), 7, &payload::ZERO);
    // 4 envelope + 3 inner + 1 payload + 4 CRC32.
    assert_eq!(frame.len(), 12);
    let check = verify_frame(&frame, DeviceFamily::Whoop4);
    assert!(check.ok, "{check:?}");
    assert_eq!(check.reason, FrameRejectReason::None);
    assert_eq!(
        packet_type(&frame, DeviceFamily::Whoop4),
        Some(packet::COMMAND)
    );
}

#[test]
fn whoop4_minimum_frame_is_eleven_bytes() {
    // A zero-payload command: declared length 7, complete size 11.
    let frame = whoop4_command_frame(Command::GetBatteryLevel.opcode(), 0, &[]);
    assert_eq!(frame.len(), 11);
    assert_eq!(u16le(&frame, 1), Some(7));
    assert!(verify_frame(&frame, DeviceFamily::Whoop4).ok);
}

#[test]
fn the_builder_reproduces_the_known_client_hello() {
    // The one WHOOP 5 frame a real strap is known to accept: GET_HELLO
    // (145) revision 1, sequence 1. If the builder ever stops reproducing
    // it, every other frame it makes is suspect. Note the inner record
    // reads `23 01 91 01` — the leading 0x23 is the COMMAND type byte, not
    // the sequence, which is a mistake worth making only once.
    let frame = whoop5_command_frame(145, 0x01, &payload::REVISION_1);
    assert_eq!(frame, WHOOP5_CLIENT_HELLO.to_vec());
    assert!(verify_frame(&frame, DeviceFamily::Whoop5).ok);
}

#[test]
fn whoop5_pads_the_inner_record_to_four_bytes() {
    // The 12-byte haptics body: inner 15 → 16, declared 20, total 28.
    let body = payload::maverick_buzz(4);
    assert_eq!(body.len(), 12);
    let frame = whoop5_command_frame(Command::RunHapticPatternMaverick.opcode(), 1, &body);
    assert_eq!(frame.len(), 28);
    assert_eq!(u16le(&frame, 2), Some(20));
    // (declared - 4) divisible by four, per the acceptance constraints.
    assert_eq!((20 - 4) % 4, 0);
    assert!(verify_frame(&frame, DeviceFamily::Whoop5).ok);
}

#[test]
fn maverick_buzz_writes_repeats_not_totals() {
    // overallLoop counts the pulses after the first: 4 buzzes → 3.
    assert_eq!(*payload::maverick_buzz(4).last().unwrap(), 3);
    assert_eq!(*payload::maverick_buzz(1).last().unwrap(), 0);
    // Clamped to the largest value any captured body evidences.
    assert_eq!(*payload::maverick_buzz(255).last().unwrap(), 7);
}

#[test]
fn a_truncated_frame_is_rejected_but_keeps_its_length() {
    let mut frame = whoop4_command_frame(26, 1, &payload::ZERO);
    frame.pop();
    let check = verify_frame(&frame, DeviceFamily::Whoop4);
    assert!(!check.ok);
    assert_eq!(check.reason, FrameRejectReason::LengthMismatch);
    assert_eq!(check.length, Some(8));
    // Truncation leaves the payload CRC uncomputable rather than wrong.
    assert_eq!(check.payload_crc_ok, None);
}

#[test]
fn a_surplus_tail_is_rejected_with_the_payload_crc_preserved() {
    let mut frame = whoop4_command_frame(26, 1, &payload::ZERO);
    frame.push(0x00);
    let check = verify_frame(&frame, DeviceFamily::Whoop4);
    assert!(!check.ok);
    assert_eq!(check.reason, FrameRejectReason::LengthMismatch);
    // "Payload CRC right, envelope wrong" is the class a capture preserves.
    assert_eq!(check.payload_crc_ok, Some(true));
}

#[test]
fn a_corrupt_payload_is_distinguished_from_a_corrupt_header() {
    let mut frame = whoop4_command_frame(26, 1, &payload::ZERO);
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;
    let check = verify_frame(&frame, DeviceFamily::Whoop4);
    assert_eq!(check.reason, FrameRejectReason::PayloadChecksum);
    assert_eq!(check.header_crc_ok, Some(true));

    let mut frame = whoop4_command_frame(26, 1, &payload::ZERO);
    frame[3] ^= 0xFF;
    let check = verify_frame(&frame, DeviceFamily::Whoop4);
    assert_eq!(check.reason, FrameRejectReason::HeaderChecksum);
    assert_eq!(check.payload_crc_ok, Some(true));
}

#[test]
fn the_families_do_not_validate_each_others_frames() {
    let four = whoop4_command_frame(26, 1, &payload::ZERO);
    assert!(!verify_frame(&four, DeviceFamily::Whoop5).ok);
    let five = whoop5_command_frame(26, 1, &payload::ZERO);
    assert!(!verify_frame(&five, DeviceFamily::Whoop4).ok);
}

#[test]
fn reassembler_rejoins_a_split_frame() {
    let frame = whoop4_command_frame(26, 1, &payload::ZERO);
    let mut r = Reassembler::new(DeviceFamily::Whoop4);
    assert!(r.feed(&frame[..5]).is_empty());
    let out = r.feed(&frame[5..]);
    assert_eq!(out, vec![frame.clone()]);
    assert_eq!(r.pending_bytes(), 0);
}

#[test]
fn reassembler_splits_two_frames_in_one_notification() {
    let a = whoop4_command_frame(26, 1, &payload::ZERO);
    let b = whoop4_command_frame(34, 2, &payload::ZERO);
    let mut joined = a.clone();
    joined.extend_from_slice(&b);
    let mut r = Reassembler::new(DeviceFamily::Whoop4);
    assert_eq!(r.feed(&joined), vec![a, b]);
}

#[test]
fn reassembler_resyncs_past_a_false_start_byte() {
    // A stray 0xAA whose following bytes read as a plausible length is the
    // case that used to swallow the real frames behind it. The header
    // checksum rejects it in one byte.
    let frame = whoop4_command_frame(26, 1, &payload::ZERO);
    let mut stream = vec![0xAA, 0x40, 0x00, 0x00];
    stream.extend_from_slice(&frame);
    let mut r = Reassembler::new(DeviceFamily::Whoop4);
    assert_eq!(r.feed(&stream), vec![frame]);
    assert_eq!(r.header_checksum_drops(), 1);
}

#[test]
fn reassembler_drops_a_below_floor_length() {
    let frame = whoop4_command_frame(26, 1, &payload::ZERO);
    // Declared length 1 → total 5, under the 11-byte floor.
    let mut stream = vec![0xAA, 0x01, 0x00, crc8(&[0x01, 0x00])];
    stream.extend_from_slice(&frame);
    let mut r = Reassembler::new(DeviceFamily::Whoop4);
    assert_eq!(r.feed(&stream), vec![frame]);
    assert_eq!(r.below_minimum_length_drops(), 1);
}

#[test]
fn reassembler_handles_whoop5_frames() {
    let frame = whoop5_command_frame(145, 0x01, &payload::REVISION_1);
    let mut r = Reassembler::new(DeviceFamily::Whoop5);
    assert!(r.feed(&frame[..10]).is_empty());
    assert_eq!(r.feed(&frame[10..]), vec![frame]);
}

#[test]
fn destructive_commands_are_refused_before_framing() {
    // Refused whatever the policy says: there is no opt-in for these.
    let everything = SendPolicy::READ_ONLY.with_ecg().with_config_write();
    for opcode in [15u8, 25, 32, 33, 36, 37, 38, 45, 99, 142, 143, 144] {
        assert!(
            admits_send(opcode, DeviceFamily::Whoop4, everything).is_err(),
            "opcode {opcode} should be refused"
        );
    }
    assert!(admits_send(26, DeviceFamily::Whoop4, SendPolicy::READ_ONLY).is_ok());
}

#[test]
fn ecg_commands_need_whoop5_and_an_opt_in() {
    let ecg = SendPolicy::READ_ONLY.with_ecg();
    for opcode in ECG_COMMANDS {
        assert!(matches!(
            admits_send(opcode, DeviceFamily::Whoop4, ecg),
            Err(SendRefused::WrongFamily { .. })
        ));
        assert!(matches!(
            admits_send(opcode, DeviceFamily::Whoop5, SendPolicy::READ_ONLY),
            Err(SendRefused::EcgNotOptedIn { .. })
        ));
        assert!(admits_send(opcode, DeviceFamily::Whoop5, ecg).is_ok());
    }
}

#[test]
fn config_writes_need_their_own_opt_in() {
    // Separate from the ECG opt-in: permitting one must not permit the
    // other, which a single bool could not express.
    let ecg_only = SendPolicy::READ_ONLY.with_ecg();
    let writes = SendPolicy::READ_ONLY.with_config_write();
    for opcode in CONFIG_WRITE_COMMANDS {
        assert!(matches!(
            admits_send(opcode, DeviceFamily::Whoop5, ecg_only),
            Err(SendRefused::ConfigWriteNotOptedIn { .. })
        ));
        assert!(admits_send(opcode, DeviceFamily::Whoop5, writes).is_ok());
    }
    // The matching reads are never gated.
    for opcode in [115u8, 116, 117, 118, 121, 128, 141] {
        assert!(admits_send(opcode, DeviceFamily::Whoop5, SendPolicy::READ_ONLY).is_ok());
    }
}

#[test]
fn named_fields_are_nul_terminated_and_bounded() {
    let body = payload::named_write("cont_collection_mode", "1");
    assert_eq!(body.len(), 65);
    assert_eq!(body[0], 0x01);
    assert_eq!(&body[1..21], b"cont_collection_mode");
    assert_eq!(body[21], 0); // terminated inside its field
    assert_eq!(body[33], b'1');
    assert!(body[34..].iter().all(|&b| b == 0));

    // 31 meaningful bytes at most, because the 32nd must be the terminator.
    let long = payload::named_write(&"k".repeat(40), "0");
    assert_eq!(long.len(), 65);
    assert_eq!(long[32], 0);
    assert_eq!(long[1..32], [b'k'; 31]);

    // A read is the same key field with no value field after it.
    assert_eq!(payload::named_read("enable_rfid").len(), 33);
}

#[test]
fn the_alarm_record_is_twenty_one_bytes_with_an_explicit_crescendo() {
    let pattern = payload::HapticPattern::default();
    let body = payload::alarm_set_whoop5(3, 1_700_000_000_500, pattern);
    assert_eq!(body.len(), 21);
    assert_eq!(body[0], 0x04); // revision 4
    assert_eq!(body[1], 3); // alarm id
    assert_eq!(&body[2..6], &1_700_000_000u32.to_le_bytes());
    // 500 ms in 1/32768-second ticks.
    assert_eq!(u16le(&body, 6), Some(16384));
    assert_eq!(&body[8..16], &[47, 152, 0, 0, 0, 0, 0, 0]);
    assert_eq!(body[18], 7); // repeats
    assert_eq!(body[19], 30); // duration
                              // Supplied, not left to framing padding: a body that relied on padding
                              // could not distinguish "ordinary pattern" from "field absent".
    assert_eq!(body[20], 0);
}

#[test]
fn the_pattern_validator_mirrors_the_documented_limits() {
    let ok = payload::HapticPattern::default();
    assert!(ok.validate().is_ok());

    assert!(payload::HapticPattern {
        effects: [252, 0, 0, 0, 0, 0, 0, 0],
        ..ok
    }
    .validate()
    .is_err());
    assert!(payload::HapticPattern { repeats: 8, ..ok }
        .validate()
        .is_err());
    assert!(payload::HapticPattern { crescendo: 2, ..ok }
        .validate()
        .is_err());
    // The 30–120 duration window applies only at seven repeats.
    assert!(payload::HapticPattern { duration: 10, ..ok }
        .validate()
        .is_err());
    assert!(payload::HapticPattern {
        repeats: 3,
        duration: 10,
        ..ok
    }
    .validate()
    .is_ok());
}

#[test]
fn advertising_name_bodies_respect_each_familys_shape() {
    // WHOOP 5: revision, a length that counts bytes, then the name.
    let five = payload::set_advertising_name_whoop5("Strap");
    assert_eq!(five, vec![0x01, 5, b'S', b't', b'r', b'a', b'p']);
    // Truncated to the 15 bytes the strap stores, rather than sent with a
    // length the body cannot satisfy.
    let long = payload::set_advertising_name_whoop5(&"n".repeat(40));
    assert_eq!(long[1], 15);
    assert_eq!(long.len(), 17);

    // WHOOP 4: two reserved bytes, the name, then NUL.
    let four = payload::set_advertising_name_whoop4("AB");
    assert_eq!(four, vec![0x00, 0x00, b'A', b'B', 0x00]);
}

#[test]
fn stop_haptics_bodies_differ_by_generation() {
    assert_eq!(payload::stop_haptics(DeviceFamily::Whoop4), vec![0x00]);
    assert_eq!(payload::stop_haptics(DeviceFamily::Whoop5), vec![0x01]);
}

#[test]
fn whoop4_haptics_uses_the_one_observed_preset() {
    // Preset 2, three loops — the pairing reported to actually buzz.
    assert_eq!(payload::whoop4_haptics_proven(), vec![0x02, 0x03, 0, 0, 0]);
}

#[test]
fn the_serial_window_is_read_but_never_searched_for() {
    // Nine alphanumeric bytes at body offset 14, then the terminator.
    let mut body = vec![0u8; 131];
    body[14..23].copy_from_slice(b"4B2C91AF7");
    // Key and signature material begins at 24 — a decoder that scanned for
    // the longest alphanumeric run could walk into it and mint an id from
    // sensitive bytes.
    body[24..78].copy_from_slice(&[b'Z'; 54]);
    assert_eq!(whoop4_serial(&body).as_deref(), Some("4B2C91AF7"));

    // A non-alphanumeric byte inside the window leaves identity
    // unresolved rather than looking elsewhere for it.
    body[18] = 0x00;
    assert_eq!(whoop4_serial(&body), None);
    assert_eq!(whoop4_serial(&[0u8; 10]), None);
}

#[test]
fn the_serial_gate_withholds_the_first_sighting() {
    let mut gate = RepeatedSerialGate::new();
    // One sighting could be a per-session token; adopting it immediately
    // would mint a new identity on every connect.
    assert_eq!(gate.offer("4B2C91AF7"), None);
    assert_eq!(gate.offer("4B2C91AF7").as_deref(), Some("4B2C91AF7"));
    // Confirmed once, reported once: a caller need not re-adopt.
    assert_eq!(gate.offer("4B2C91AF7"), None);
    assert_eq!(gate.confirmed(), Some("4B2C91AF7"));

    // A different value restarts confirmation rather than replacing it.
    let mut gate = RepeatedSerialGate::new();
    assert_eq!(gate.offer("AAAAAAAAA"), None);
    assert_eq!(gate.offer("BBBBBBBBB"), None);
    assert_eq!(gate.confirmed(), None);
}

#[test]
fn alarm_details_are_preserved_not_coerced() {
    assert_eq!(AlarmSetDetail::from_byte(1), AlarmSetDetail::Accepted);
    assert_eq!(
        AlarmSetDetail::from_byte(10),
        AlarmSetDetail::TimeNotInFuture
    );
    assert_eq!(AlarmSetDetail::from_byte(99), AlarmSetDetail::Unknown(99));
    assert_eq!(AlarmRunDetail::from_byte(0), AlarmRunDetail::Started);
    assert_eq!(AlarmRunDetail::from_byte(5), AlarmRunDetail::Completed);
    assert_eq!(AlarmRunDetail::from_byte(7), AlarmRunDetail::Timeout);
}

#[test]
fn the_body_location_gap_at_six_stays_raw() {
    assert_eq!(BodyLocation::from_byte(1), BodyLocation::Wrist);
    assert_eq!(BodyLocation::from_byte(7), BodyLocation::Ankle);
    assert_eq!(BodyLocation::from_byte(160), BodyLocation::UnknownGarment);
    // 6 is a real gap in the enum, not a neighbour of 5 or 7.
    assert_eq!(BodyLocation::from_byte(6), BodyLocation::Raw(6));
}

#[test]
fn the_inventories_match_the_documented_counts() {
    assert_eq!(DEVICE_KEYS.len(), 8);
    assert_eq!(FEATURE_FLAGS.len(), 25);
    // Twenty eligible for named lookup, five not.
    assert_eq!(FEATURE_FLAGS.iter().filter(|f| f.named_lookup).count(), 20);
    assert_eq!(
        device_key("max_collection_backlog").unwrap().value_type,
        ConfigValueType::Tenths
    );
    assert!(device_key("not_a_key").is_none());
    // Every R22 key is a real flag, and each is eligible to be written.
    for key in R22_FEATURE_KEYS {
        let flag = feature_flag(key).expect("R22 key should be in the inventory");
        assert!(flag.named_lookup, "{key} must support named access");
    }
}

#[test]
fn rr_conversion_rounds_to_nearest() {
    assert_eq!(rr_ms_from_ticks(0), 0);
    assert_eq!(rr_ms_from_ticks(1024), 1000);
    assert_eq!(rr_ms_from_ticks(512), 500);
    // Rounding, not saturation: 65535 ticks is 63999.5 ms, truncated after
    // the +512 nudge rather than rounded up to a whole 64 s.
    assert_eq!(rr_ms_from_ticks(u16::MAX), 63999);
}

#[test]
fn variant_resolution_prefers_evidence_over_a_guess() {
    assert_eq!(
        whoop5_variant(Some("5AM12345"), None, None),
        Whoop5Variant::Mg
    );
    assert_eq!(whoop5_variant(None, None, Some("MG")), Whoop5Variant::Mg);
    assert_eq!(
        whoop5_variant(Some("5AG12345"), None, None),
        Whoop5Variant::Whoop50
    );
    assert_eq!(
        whoop5_variant(None, Some("WG50-B"), None),
        Whoop5Variant::Whoop50
    );
    // Contradictory signals stay unresolved rather than picking a side.
    assert_eq!(
        whoop5_variant(Some("5AM12345"), Some("WG50-B"), None),
        Whoop5Variant::Unknown
    );
    assert_eq!(whoop5_variant(None, None, None), Whoop5Variant::Unknown);
}

#[test]
fn offload_types_exclude_the_live_streams() {
    assert!(is_offload_frame(packet::HISTORICAL_DATA));
    assert!(is_offload_frame(packet::METADATA));
    assert!(is_offload_frame(packet::WHOOP5_METADATA));
    // A type-43 flood must never keep an offload watchdog alive.
    assert!(!is_offload_frame(packet::REALTIME_RAW_DATA));
    assert!(!is_offload_frame(packet::REALTIME_DATA));
}

#[test]
fn the_whoop5_type_aliases_never_decode_as_unknown() {
    assert_eq!(packet_type_name(38), "COMMAND_RESPONSE");
    assert_eq!(packet_type_name(56), "METADATA");
}
