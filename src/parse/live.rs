//! Decoders for the streams a strap pushes without being asked: the standard
//! SIG characteristics, strap events, live heart rate, and the live raw sensor
//! layouts of both generations.

use crate::protocol::{
    event_name, i16le, payload_end, rr_ms_from_ticks, u16le, u32le, DeviceFamily,
};
use crate::types::{
    Battery, BatterySource, HeartRate, HrSource, ImuStreamRecord, Record, StrapEvent,
    Whoop4RawImuRecord, Whoop4RawOpticalRecord,
};

// ── Standard SIG characteristics ─────────────────────────────────────────────

/// Decode a Heart Rate Measurement (`2A37`) notification.
///
/// | Field | Encoding |
/// |---|---|
/// | flags | `u8`: bit 0 = 16-bit HR, bit 3 = energy expended present, bit 4 = R-R present |
/// | HR | `u8` or `u16le` per bit 0 |
/// | energy | `u16le`, present per bit 3 — skipped, not a heart-rate field |
/// | R-R | `u16le` each, units 1/1024 s, to the end of the value |
///
/// Works unbonded, which makes it the one source available before any custom
/// handshake. Returns `None` for an empty value or one that ends mid-HR.
pub fn hr_measurement(data: &[u8]) -> Option<HeartRate> {
    let flags = *data.first()?;
    let hr_16bit = flags & 0x01 != 0;
    let energy_present = flags & 0x08 != 0;
    let rr_present = flags & 0x10 != 0;

    let mut off = 1;
    let bpm = if hr_16bit {
        // A 16-bit HR above 255 bpm is not a heart rate; saturate rather than
        // truncate so a garbled packet cannot read as a plausible low value.
        let wide = u16le(data, off)?;
        off += 2;
        wide.min(u8::MAX as u16) as u8
    } else {
        let v = *data.get(off)?;
        off += 1;
        v
    };
    if energy_present {
        off += 2;
    }

    let mut rr_ms = Vec::new();
    if rr_present {
        // Only complete words, and never past the end of the value.
        while off + 1 < data.len() {
            let ticks = u16le(data, off)?;
            off += 2;
            if ticks != 0 {
                rr_ms.push(rr_ms_from_ticks(ticks));
            }
        }
    }

    Some(HeartRate {
        source: HrSource::StandardService,
        bpm,
        rr_ms,
        unix_seconds: None,
    })
}

/// Decode a Battery Level (`2A19`) read or notification — a single byte.
///
/// Some WHOOP 4 straps report a constant 100 here, so this is reported with its
/// source attached and never treated as authoritative when command 26 is
/// available.
pub fn battery_level(data: &[u8]) -> Option<Battery> {
    Some(Battery {
        source: BatterySource::StandardService,
        percent: f32::from(*data.first()?),
        millivolts: None,
        charging: None,
    })
}

// ── Events ───────────────────────────────────────────────────────────────────

/// Decode a strap event (packet type 48).
///
/// | Field | WHOOP 4 | WHOOP 5 |
/// |---|---:|---:|
/// | event number | `[6]` | `[10]` |
/// | timestamp `u32` | `[8]` | `[12]` |
///
/// A `BATTERY_LEVEL` event on WHOOP 4 additionally carries the dense layout
/// `soc = u16@17 / 10`, `mV = u16@21`, `charging = bit 0 of [26]`. Those offsets
/// are WHOOP 4 event-frame absolute and are not applied to WHOOP 5.
pub fn strap_event(frame: &[u8], family: DeviceFamily) -> Option<StrapEvent> {
    let inner = family.inner_offset();
    let end = payload_end(frame, family)?;
    let number = *frame.get(inner + 2)?;
    let unix_seconds = u32le(frame, inner + 4)?;

    let battery = (family == DeviceFamily::Whoop4
        && number == crate::protocol::event::BATTERY_LEVEL
        && end > 26)
        .then(|| {
            Some(Battery {
                source: BatterySource::Event,
                percent: f32::from(u16le(frame, 17)?) / 10.0,
                millivolts: u16le(frame, 21),
                charging: Some(frame.get(26)? & 1 == 1),
            })
        })
        .flatten();

    Some(StrapEvent {
        number,
        name: event_name(number),
        unix_seconds,
        battery,
    })
}

// ── Live heart rate ──────────────────────────────────────────────────────────

/// Decode a live heart-rate packet (type 40).
///
/// WHOOP 4, offsets absolute:
///
/// | Offset | Width | Field |
/// |---:|---:|---|
/// | 6 | 4 | Unix/device seconds |
/// | 10 | 2 | subseconds |
/// | 12 | 1 | heart rate |
/// | 13 | 1 | R-R count |
/// | 14 + 2n | 2 | R-R intervals, already **milliseconds** |
///
/// WHOOP 5 is a different map, not these offsets plus four: time at 10,
/// an additional time word at 14, HR at 16, count at 17, and intervals from 18
/// in **ticks of 1/1024 s**. Reading one generation's units with the other's
/// offsets produces intervals wrong by about 2%, which is the kind of error that
/// looks like physiology.
pub fn realtime_hr(frame: &[u8], family: DeviceFamily) -> Option<HeartRate> {
    let end = payload_end(frame, family)?;
    let (time_off, hr_off, count_off, rr_off) = match family {
        DeviceFamily::Whoop4 => (6, 12, 13, 14),
        DeviceFamily::Whoop5 => (10, 16, 17, 18),
    };

    let unix_seconds = u32le(frame, time_off)?;
    let bpm = *frame.get(hr_off)?;
    let declared = *frame.get(count_off)? as usize;

    let mut rr_ms = Vec::with_capacity(declared);
    for i in 0..declared {
        let off = rr_off + i * 2;
        // Never consume an incomplete word, and never read into the trailer.
        if off + 2 > end {
            break;
        }
        let raw = u16le(frame, off)?;
        if raw == 0 {
            // A placeholder, not a measured interval.
            continue;
        }
        rr_ms.push(match family {
            DeviceFamily::Whoop4 => u32::from(raw),
            DeviceFamily::Whoop5 => rr_ms_from_ticks(raw),
        });
    }

    Some(HeartRate {
        source: HrSource::RealtimePacket,
        bpm,
        rr_ms,
        unix_seconds: Some(unix_seconds),
    })
}

// ── WHOOP 4 live raw layouts ─────────────────────────────────────────────────

/// Decode a WHOOP 4 live type-43 frame by its **validated payload length**.
///
/// The common prefix is documented: subtype at 6, record header at 7–14, device
/// seconds at 15 and subseconds at 19–20. After that the packet type alone is
/// not enough — the layout is selected by payload length, which is the declared
/// length minus 7.
///
/// | Payload | Declared | Layout |
/// |---:|---:|---|
/// | 1,917 | 1,924 | six-axis IMU with HR in the header |
/// | 1,921 | 1,928 | one AC-coupled optical waveform |
///
/// Anything else is preserved whole. Two shapes of the same packet type with a
/// four-byte difference between them is exactly the case where guessing from
/// the type would decode an optical waveform as motion.
pub fn whoop4_raw(frame: &[u8]) -> Option<Record> {
    let declared = u16le(frame, 1)? as usize;
    match declared.checked_sub(7)? {
        1917 => whoop4_raw_imu(frame).map(Record::Whoop4RawImu),
        1921 => whoop4_raw_optical(frame).map(Record::Whoop4RawOptical),
        _ => None,
    }
}

/// The 1,917-byte variant: 100 signed `i16le` samples per axis.
///
/// Axis bases are the fixed 89/289/489 for acceleration and 692/892/1092 for
/// the gyroscope — absolute frame offsets, with heart rate at 21 and up to four
/// intervals from 23.
///
/// Scales are the commonly applied interpretations: `1/4096` g, which matched a
/// gravity-sphere fit to within 1%, and `2000/32768` °/s, which matched
/// controlled 720° rotations to within turn-count precision. Neither is a
/// confirmed device parameter.
fn whoop4_raw_imu(frame: &[u8]) -> Option<Whoop4RawImuRecord> {
    const SAMPLES: usize = 100;
    const ACCEL: [usize; 3] = [89, 289, 489];
    const GYRO: [usize; 3] = [692, 892, 1092];
    const ACCEL_SCALE: f32 = 1.0 / 4096.0;
    const GYRO_SCALE: f32 = 2000.0 / 32768.0;

    let unix_seconds = u32le(frame, 15)?;
    let bpm = *frame.get(21)?;
    // At most four intervals live in 23–30; a larger declared count is not more
    // data, it is the next unmapped region.
    let declared = (*frame.get(22)? as usize).min(4);
    let mut rr_ms = Vec::with_capacity(declared);
    for i in 0..declared {
        let ms = u16le(frame, 23 + i * 2)?;
        if ms != 0 {
            rr_ms.push(u32::from(ms));
        }
    }

    let read = |bases: [usize; 3], scale: f32| -> Option<Vec<[f32; 3]>> {
        let mut out = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            out.push([
                f32::from(i16le(frame, bases[0] + i * 2)?) * scale,
                f32::from(i16le(frame, bases[1] + i * 2)?) * scale,
                f32::from(i16le(frame, bases[2] + i * 2)?) * scale,
            ]);
        }
        Some(out)
    };

    Some(Whoop4RawImuRecord {
        unix_seconds,
        heart_rate: HeartRate {
            source: HrSource::RealtimePacket,
            bpm,
            rr_ms,
            unix_seconds: Some(unix_seconds),
        },
        accel_g: read(ACCEL, ACCEL_SCALE)?,
        gyro_dps: read(GYRO, GYRO_SCALE)?,
    })
}

/// The 1,921-byte variant: one optical waveform, stride four from offset 42.
///
/// Each stride is a signed 24-bit little-endian sample in bytes 0–2 and an
/// auxiliary byte in position 3. That fourth byte is status or exposure and is
/// **retained**, not skipped: the contract says to keep it, and a decoder that
/// dropped it would make the unknown unrecoverable from the decoded record.
///
/// This is a single AC-coupled waveform, so no red, IR or ambient identity is
/// assigned to it, and the configuration header at 15–41 stays raw.
fn whoop4_raw_optical(frame: &[u8]) -> Option<Whoop4RawOpticalRecord> {
    const BASE: usize = 42;
    const STRIDE: usize = 4;
    const MAX_SAMPLES: usize = 419;

    let end = payload_end(frame, DeviceFamily::Whoop4)?;
    let mut samples = Vec::with_capacity(MAX_SAMPLES);
    let mut aux = Vec::with_capacity(MAX_SAMPLES);
    for i in 0..MAX_SAMPLES {
        let off = BASE + i * STRIDE;
        // Never a partial stride, and never into the CRC trailer.
        if off + STRIDE > end {
            break;
        }
        let b0 = u32::from(frame[off]);
        let b1 = u32::from(frame[off + 1]);
        let b2 = u32::from(frame[off + 2]);
        let raw24 = b0 | (b1 << 8) | (b2 << 16);
        // Two's complement over 24 bits: at or above 2^23, subtract 2^24.
        samples.push(if raw24 >= 0x80_0000 {
            raw24 as i32 - 0x100_0000
        } else {
            raw24 as i32
        });
        aux.push(frame[off + 3]);
    }

    Some(Whoop4RawOpticalRecord {
        unix_seconds: u32le(frame, 15)?,
        samples,
        aux,
        config: frame.get(15..42).map(<[u8]>::to_vec).unwrap_or_default(),
    })
}

// ── Dedicated IMU streams ────────────────────────────────────────────────────

/// Decode a packet type 51 or 52 — the dedicated IMU stream.
///
/// Unlike every other record here the array positions are **computed from the
/// counts**, not fixed:
///
/// ```text
/// A = u16le @ 24, G = u16le @ 26
/// accel X @ 28,  Y @ 28 + 2A,  Z @ 28 + 4A
/// gyro  X @ 28 + 6A,  Y @ 28 + 6A + 2G,  Z @ 28 + 6A + 4G
/// span ends at 28 + 6A + 6G
/// ```
///
/// So the whole calculated span is validated against the payload boundary
/// before a single sample is read — a lying count here does not merely
/// mis-slice one array, it slides every later one.
///
/// Samples stay raw 16-bit words. Signedness, physical scale, cadence and
/// timestamps are all unspecified for these types, and this layout is
/// explicitly **not** R21, so R21's `raw/4096` g is not applied. No matching
/// strap output has been confirmed for either type; a frame that does not fit
/// the contract is preserved rather than decoded.
pub fn imu_stream(frame: &[u8], family: DeviceFamily, packet_type: u8) -> Option<ImuStreamRecord> {
    let end = payload_end(frame, family)?;
    let accel_count = u16le(frame, 24)?;
    let gyro_count = u16le(frame, 26)?;
    let a = usize::from(accel_count);
    let g = usize::from(gyro_count);

    let gyro_base = 28usize.checked_add(6usize.checked_mul(a)?)?;
    let span_end = gyro_base.checked_add(6usize.checked_mul(g)?)?;
    if span_end > end {
        return None;
    }

    let column = |base: usize, count: usize| -> Option<Vec<u16>> {
        (0..count).map(|i| u16le(frame, base + i * 2)).collect()
    };

    Some(ImuStreamRecord {
        packet_type,
        accel_count,
        gyro_count,
        accel_raw: [
            column(28, a)?,
            column(28 + 2 * a, a)?,
            column(28 + 4 * a, a)?,
        ],
        gyro_raw: [
            column(gyro_base, g)?,
            column(gyro_base + 2 * g, g)?,
            column(gyro_base + 4 * g, g)?,
        ],
    })
}
