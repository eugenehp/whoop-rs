//! Decoders for replies: command responses and every body this crate reads out
//! of one, plus offload metadata and console logs.

use crate::protocol::{
    command_name, payload_end, u16le, u32le, Command, CommandResult, DeviceFamily, MetadataType,
};
use crate::types::{
    AdvertisingName, AlarmRecord, Battery, BatteryPack, BatterySource, BodyLocationStatus,
    CommandResponse, ConfigEntry, ConfigValue, DataRange, ExtendedBattery, Hello, HistoryMeta,
};

// ── Command responses ────────────────────────────────────────────────────────

/// Decode a command response (packet type 36).
///
/// | Field | WHOOP 4 | WHOOP 5 |
/// |---|---:|---:|
/// | command | `[6]` | `[10]` |
/// | origin sequence | `[7]` | `[11]` |
/// | result | `[8]` | `[12]` |
/// | body | `[9..]` | `[13..]` |
///
/// The frame's own sequence — `[5]` / `[9]` — is the response's, not an echo of
/// the request, so it is reported separately and never used to correlate.
pub fn command_response(frame: &[u8], family: DeviceFamily) -> Option<CommandResponse> {
    let inner = family.inner_offset();
    let end = payload_end(frame, family)?;

    let response_sequence = *frame.get(inner + 1)?;
    let command = *frame.get(inner + 2)?;
    let origin_sequence = *frame.get(inner + 3)?;
    let result = CommandResult::from_byte(*frame.get(inner + 4)?);

    // The body starts after command, origin and result, and stops at the CRC
    // trailer: the padding and the checksum are not response fields.
    let body_start = inner + 5;
    let body = if body_start < end {
        frame[body_start..end].to_vec()
    } else {
        Vec::new()
    };

    Some(CommandResponse {
        command,
        name: command_name(command),
        origin_sequence,
        response_sequence,
        result,
        body,
    })
}

/// Battery percent from a `GET_BATTERY_LEVEL` (26) response body.
///
/// WHOOP 4 sends `u16le` tenths of a percent; WHOOP 5 50.42.1.0 sends a
/// four-byte unsigned whole percent, with four zero bytes on the documented
/// error path. Reading WHOOP 5's body as one byte happens to work up to 100 and
/// is still the wrong contract, so the width is read per family.
pub fn battery_from_command(body: &[u8], family: DeviceFamily) -> Option<Battery> {
    let percent = match family {
        DeviceFamily::Whoop4 => f32::from(u16le(body, 0)?) / 10.0,
        DeviceFamily::Whoop5 => u32le(body, 0)? as f32,
    };
    // A plausibility gate, not a validity flag: the wire has no "this reading
    // is good" bit, and a conversion fallback of zero is indistinguishable from
    // a flat battery, so only impossible values are refused.
    (percent <= 100.0).then_some(Battery {
        source: BatterySource::Command,
        percent,
        millivolts: None,
        charging: None,
    })
}

/// Decode a WHOOP 5 `GET_HELLO` (145) response body.
///
/// | Offset | Width | Field |
/// |---:|---:|---|
/// | 0 | 1 | revision, 1 or 3 |
/// | 1 | 4 | battery, tenths of a percent |
/// | 6 | 4 | Unix seconds |
/// | 91 | 3 | version components |
/// | 94 | 4 | version suffix |
/// | 103 | 4 | preparation status bits |
///
/// The identity text blocks at 14, 25 and 49 are deliberately **not** read: they
/// carry personal device information and key material, and a decoder needs
/// neither to report a firmware version. A PENDING reply is zero-filled except
/// the revision, so it decodes to a zero battery and a zero clock — which is why
/// the caller waits for the final result rather than the first.
pub fn hello(body: &[u8]) -> Option<Hello> {
    let revision = *body.first()?;
    if revision != 1 && revision != 3 {
        return None;
    }
    let battery_tenths = u32le(body, 1)?;
    let unix_seconds = u32le(body, 6)?;
    let major = *body.get(91)?;
    let minor = *body.get(92)?;
    let patch = *body.get(93)?;
    let suffix = u32le(body, 94)?;
    let preparation_bits = u32le(body, 103)?;

    Some(Hello {
        revision,
        battery_percent: battery_tenths as f32 / 10.0,
        unix_seconds,
        firmware: format!("{major}.{minor}.{patch}.{suffix}"),
        preparation_bits,
    })
}

/// Decode a `GET_DATA_RANGE` (34) response body.
///
/// Revision 1 followed by sixteen `u32` fields — 65 bytes in total. Failure,
/// including the response-timeout path, is revision 1 followed by 64 zero
/// bytes, which decodes to an all-zero range rather than being mistaken for a
/// strap with one page of data; the caller checks the result byte first.
pub fn data_range(body: &[u8]) -> Option<DataRange> {
    if *body.first()? != 1 || body.len() < 65 {
        return None;
    }
    let at = |i: usize| u32le(body, 1 + i * 4).unwrap_or(0);
    Some(DataRange {
        boundary: at(0),
        read_page: at(1),
        write_page: at(2),
        trim_boundary: at(3),
        wrap_state: at(4),
        capacity: at(5),
        record_estimate: at(6),
        // at(7) is the derived quantity 15 × (adjusted D + capacity −
        // adjusted C). It is a restatement of the fields above under one set of
        // fallback assumptions, so it is not stored as if it were a measurement.
        clock_pairs: [
            (at(8), at(9)),
            (at(10), at(11)),
            (at(12), at(13)),
            (at(14), at(15)),
        ],
    })
}

// ── Metadata ─────────────────────────────────────────────────────────────────

/// Decode an offload control message (packet type 49 or 56).
///
/// The metadata type byte sits at the inner record's third slot — `[6]` on
/// WHOOP 4, `[10]` on WHOOP 5 — and an end marker carries:
///
/// | Field | WHOOP 4 | WHOOP 5 |
/// |---|---:|---:|
/// | Unix seconds | `[7]` | `[11]` |
/// | subseconds | `[11]` | `[15]` |
/// | trim cursor | `[17]` | `[21]` |
/// | acknowledgement block | `[17..25]` | `[21..29]` |
///
/// The eight-byte block is copied out verbatim. It is never rebuilt from the
/// decoded trim cursor, even though the cursor is its first word: the second
/// word reflects write-wrap state, and the special tokens (`0xffffffff`, paired
/// `0xfefefefe`, paired `0xfdfdfdfd`) select strap behaviour that a synthesised
/// token would trigger by accident.
pub fn history_meta(frame: &[u8], family: DeviceFamily) -> Option<HistoryMeta> {
    let inner = family.inner_offset();
    let kind = MetadataType::from_byte(*frame.get(inner + 2)?)?;
    let (time_off, ack_off) = match family {
        DeviceFamily::Whoop4 => (7, 17),
        DeviceFamily::Whoop5 => (11, 21),
    };

    if kind != MetadataType::HistoryEnd {
        return Some(HistoryMeta {
            kind,
            unix_seconds: None,
            trim_cursor: None,
            end_block: None,
        });
    }

    // An end marker without a complete block is not acknowledgeable: report the
    // marker, withhold the token, and let the session decline to ack.
    let end = payload_end(frame, family)?;
    let (trim_cursor, end_block) = if ack_off + 8 <= end {
        let mut block = [0u8; 8];
        block.copy_from_slice(&frame[ack_off..ack_off + 8]);
        (u32le(frame, ack_off), Some(block))
    } else {
        (None, None)
    };

    Some(HistoryMeta {
        kind,
        unix_seconds: u32le(frame, time_off),
        trim_cursor,
        end_block,
    })
}

// ── Console logs ─────────────────────────────────────────────────────────────

/// Decode firmware log text (packet type 50).
///
/// Text runs from offset 11 to one byte before the CRC trailer. Capped at 2 KB:
/// a log line is never this long, and without a cap a garbled peer could pin
/// arbitrary bytes per frame as a string on the decode path.
pub fn console_log(frame: &[u8], family: DeviceFamily) -> Option<String> {
    let end = payload_end(frame, family)?;
    let start = 11;
    let stop = end.saturating_sub(1).min(frame.len());
    if start >= stop {
        return None;
    }
    let text = String::from_utf8_lossy(&frame[start..stop]);
    Some(text.chars().take(2048).collect())
}

// ── Service and configuration response bodies ────────────────────────────────

/// Decode a `GET_BATTERY_PACK_INFO` (151) body — 28 bytes.
///
/// | Offset | Width | Field |
/// |---:|---:|---|
/// | 0 | 1 | revision 1 |
/// | 1 | 1 | presence flag |
/// | 2 | 6 | address/identifier block |
/// | 8 | 16 | serial/text block |
/// | 24 | 2 | charge value |
///
/// The charge value is **not** scaled. Dividing it by ten is a client display
/// convention, not a property the wire establishes.
pub fn battery_pack(body: &[u8]) -> Option<BatteryPack> {
    if *body.first()? != 1 || body.len() < 28 {
        return None;
    }
    let serial = String::from_utf8_lossy(&body[8..24])
        .trim_matches('\0')
        .trim()
        .to_owned();
    Some(BatteryPack {
        present: body[1] != 0,
        address: body[2..8].to_vec(),
        serial,
        charge_raw: u16le(body, 24)?,
    })
}

/// Decode a WHOOP 4 `GET_EXTENDED_BATTERY_INFO` (98) body — 25 bytes.
///
/// `u8`, seven `u16le`, two `u32le`, then a trailing `u16le`. Only the pack
/// voltage is mapped — the third `u16le`, at body offset 5 — so the complete
/// record is kept beside it. An empty cache returns failure instead, which is
/// why a decoded record means something was actually stored.
pub fn extended_battery(body: &[u8]) -> Option<ExtendedBattery> {
    if body.len() < 25 {
        return None;
    }
    Some(ExtendedBattery {
        millivolts: u16le(body, 5)?,
        raw: body[..25].to_vec(),
    })
}

/// Decode a WHOOP 5 `GET_ADVERTISING_NAME` (141) body — a fixed 19 bytes.
///
/// Revision at 0, source/status at 1, a NUL-inclusive length of 1–16 at 2, and
/// 16 bytes of name storage at 3–18 whose last byte is always zero. Decoding is
/// limited to that fixed storage and the terminator is excluded, so a status-2
/// reply — fallback, empty storage **or** a read failure, indistinguishably —
/// yields an empty name rather than 16 bytes of whatever was there.
pub fn advertising_name(body: &[u8]) -> Option<AdvertisingName> {
    if *body.first()? != 1 || body.len() < 19 {
        return None;
    }
    let status = body[1];
    // The length counts the terminator, so the text is one byte shorter, and a
    // declared length can never reach past the fixed storage.
    let declared = usize::from(body[2]).clamp(1, 16);
    let text_end = 3 + declared.saturating_sub(1);
    let name = String::from_utf8_lossy(&body[3..text_end.min(19)])
        .trim_matches('\0')
        .to_owned();
    Some(AdvertisingName {
        name,
        status,
        custom: status == 1,
    })
}

/// Decode a `GET_BODY_LOCATION_AND_STATUS` (84) body.
///
/// WHOOP 4 carries a four-byte record — revision, location, confidence, status
/// — whose location byte maps to [`crate::protocol::BodyLocation`]; confidence
/// and status stay raw until captures establish them.
///
/// WHOOP 5's reply is `[1, 0, 255, on_body]`: the two middle bytes are fixed
/// placeholders, **not** a measured location and confidence, so no location is
/// decoded for it and only the on-body flag is reported.
pub fn body_location(body: &[u8], family: DeviceFamily) -> Option<BodyLocationStatus> {
    if *body.first()? != 1 || body.len() < 4 {
        return None;
    }
    let (location, on_body) = match family {
        DeviceFamily::Whoop4 => (
            Some(crate::protocol::BodyLocation::from_byte(body[1])),
            None,
        ),
        // Only 0 and 1 are documented; anything else is not a boolean.
        DeviceFamily::Whoop5 => (None, matches!(body[3], 0 | 1).then_some(body[3] == 1)),
    };
    Some(BodyLocationStatus {
        location,
        on_body,
        raw: body[..4].to_vec(),
    })
}

/// Decode a named configuration read — the 65-byte reply to command 121 or 128.
///
/// Revision at 0, the normalised key echo at 1–32, and the formatted value at
/// 33–64. Both fields are NUL-terminated within their width.
///
/// The caller must still check the outer result **and** match the returned key:
/// an unknown key, a read error or a formatting failure all return result 0
/// with the key echo and a zero value field, and a non-NUL 32-byte key is
/// normalised and will not echo exactly. Separately, an unreadable stored value
/// comes back as zeros with result **1** — hence [`ConfigValue::zero_filled`],
/// so a fallback is distinguishable from a stored zero.
pub fn config_value(body: &[u8]) -> Option<ConfigValue> {
    if *body.first()? != 1 || body.len() < 65 {
        return None;
    }
    let field = |range: std::ops::Range<usize>| -> String {
        let bytes = &body[range];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    Some(ConfigValue {
        key: field(1..33),
        value: field(33..65),
        zero_filled: body[33..65].iter().all(|&b| b == 0),
    })
}

/// Decode an enumeration-count reply — command 115 or 117.
///
/// Two body bytes: revision 1 and a **one-byte** entry count. An invalid
/// revision fails with `01 00`, which decodes to a count of zero rather than
/// being mistaken for an empty namespace.
pub fn config_count(body: &[u8]) -> Option<u8> {
    (*body.first()? == 1)
        .then(|| body.get(1).copied())
        .flatten()
}

/// Decode an enumeration entry — command 116 or 118.
///
/// `[revision][index][validity][key ASCII, NUL-terminated][padding]`. The end
/// marker arrives with nothing after the index, so revision and index are the
/// only fields required.
///
/// A name is returned only when it decodes as printable ASCII; a contaminated
/// or empty field yields `None` rather than a string of control bytes. One
/// hands-on report has the value field of a *value* read contaminated by a
/// stale shared buffer, which is why the walk reads names and not values.
pub fn config_entry(body: &[u8]) -> Option<ConfigEntry> {
    if body.len() < 2 {
        return None;
    }
    let index = body[1];
    let valid = body.get(2).is_some_and(|&v| v != 0);
    let key = body.get(3..).and_then(|rest| {
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let text = &rest[..end];
        (!text.is_empty() && text.iter().all(|&b| (0x20..0x7F).contains(&b)))
            .then(|| String::from_utf8_lossy(text).into_owned())
    });
    Some(ConfigEntry { index, valid, key })
}

/// Decode a WHOOP 5 `GET_ALARM_TIME` (67) body — the 21-byte alarm record.
///
/// The same layout `SET` writes. An all-zero time can mean a cleared slot
/// **or** a read fallback: GET can return zeros after a read failure while
/// still reporting success, and there is no flag distinguishing the two.
pub fn alarm_record(body: &[u8]) -> Option<AlarmRecord> {
    if *body.first()? != 4 || body.len() < 21 {
        return None;
    }
    let mut effects = [0u8; 8];
    effects.copy_from_slice(&body[8..16]);
    Some(AlarmRecord {
        id: body[1],
        unix_seconds: u32le(body, 2)?,
        ticks: u16le(body, 6)?,
        effects,
        loop_control: u16le(body, 16)?,
        repeats: body[18],
        duration: body[19],
        crescendo: body[20],
    })
}

// ── Response-body helpers ────────────────────────────────────────────────────

/// Decode whichever payload a successful response body carries, for the
/// commands whose bodies this crate maps.
///
/// Returns `None` for a command with no mapped body, which is the common case:
/// most replies are an acceptance, and inventing a decode for one is how a
/// stub response starts being read as data.
pub fn response_payload(
    response: &CommandResponse,
    family: DeviceFamily,
) -> Option<DecodedResponse> {
    if response.result != CommandResult::Success {
        return None;
    }
    match response.command {
        c if c == Command::GetBatteryLevel.opcode() => {
            battery_from_command(&response.body, family).map(DecodedResponse::Battery)
        }
        c if c == Command::GetHello.opcode() => hello(&response.body).map(DecodedResponse::Hello),
        c if c == Command::GetDataRange.opcode() && family == DeviceFamily::Whoop5 => {
            data_range(&response.body).map(DecodedResponse::DataRange)
        }
        _ => None,
    }
}

/// A response body this crate knows how to read.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedResponse {
    /// From `GET_BATTERY_LEVEL` (26).
    Battery(Battery),
    /// From `GET_HELLO` (145).
    Hello(Hello),
    /// From `GET_DATA_RANGE` (34).
    DataRange(DataRange),
}
