//! Decoders for stored records — packet type 47 on both generations, plus the
//! WHOOP 5 layouts that also arrive live on type 43.

use crate::protocol::{
    f32le, i16le, i32le, inertial_timestamp, layout, packet_type, payload_end, rr_ms_from_ticks,
    u16le, u32le, whoop5_layout_length, DeviceFamily,
};
use crate::types::{
    BiometricRecord, CompactOpticalRecord, EcgLeadOff, EcgRecord, HeartRate, HrSource, ImuRecord,
    OpticalBlock, OpticalRecord, Record, Whoop4SensorBlock, Whoop4V25Record,
};

// ── Historical records ───────────────────────────────────────────────────────

/// Decode one record from the historical offload (packet type 47), or preserve
/// it whole.
///
/// The selector differs by generation and this is the only place that knows it:
/// WHOOP 5 reads a layout byte at 9, WHOOP 4 a version byte at 5. Never select
/// by the marketing generation alone — 41.17.6.0 emits both v24 and the
/// distinct 84-byte v25 depending on configuration.
pub fn historical(frame: &[u8], family: DeviceFamily) -> Record {
    let ptype = packet_type(frame, family).unwrap_or(0);
    match family {
        DeviceFamily::Whoop4 => whoop4_historical(frame, ptype),
        DeviceFamily::Whoop5 => whoop5_record(frame, ptype),
    }
}

/// WHOOP 5 record dispatch, shared by historical type 47 and live type 43 —
/// both use byte 9 as the layout selector, which is why ECG arrives on either.
pub fn whoop5_record(frame: &[u8], ptype: u8) -> Record {
    let version = frame.get(9).copied().unwrap_or(0);
    let unknown = |note: &'static str| Record::Unknown {
        packet_type: ptype,
        version,
        length: frame.len(),
        note,
        raw: frame.to_vec(),
    };

    // The length gate. Fixed regions do not move when a count is smaller, so a
    // record of the wrong size is not this layout with fields missing — it is a
    // different record, and decoding it would read real numbers out of the
    // wrong offsets.
    if let Some(expected) = whoop5_layout_length(version) {
        if frame.len() != expected {
            return unknown("length does not match the layout's fixed size");
        }
    }

    match version {
        layout::R18_BIOMETRIC => {
            r18(frame).map_or_else(|| unknown("R18 fields did not validate"), Record::Biometric)
        }
        layout::R20_OPTICAL => r20(frame).map_or_else(
            || unknown("R20 counts or samples did not validate"),
            Record::Optical,
        ),
        layout::R21_IMU => r21(frame).map_or_else(
            || unknown("R21 counts or samples did not validate"),
            Record::Imu,
        ),
        layout::R26_COMPACT_OPTICAL => r26(frame).map_or_else(
            || unknown("R26 window did not validate"),
            Record::CompactOptical,
        ),
        layout::R16_ECG_RAW | layout::R17_ECG_FILTERED => ecg(frame).map_or_else(
            || unknown("ECG count exceeds the layout capacity, or the frame is short"),
            Record::Ecg,
        ),
        // R22's version-9 queued-channel encoding is documented but not
        // implemented here, so it stays raw rather than being half-decoded.
        layout::R22_QUEUED => unknown("R22 queued channels are not decoded"),
        _ => unknown("unknown WHOOP 5 layout selector"),
    }
}

/// WHOOP 5 **R18** — the 124-byte biometric summary, CRC at 120.
///
/// | Offset | Width | Field |
/// |---:|---:|---|
/// | 11 | 4 | record index |
/// | 15 | 4 | Unix seconds |
/// | 22 | 1 | heart rate |
/// | 23 | 1 | R-R count, at most four complete words |
/// | 24 + 2i | 2 | R-R, ticks of 1/1024 s |
/// | 36 | 1 | HR/R-R quality flags |
/// | 41 | 4 | dynamic acceleration, f32 g |
/// | 45, 49, 53 | 4 | gravity x/y/z, f32 g |
/// | 57 | 2 | selected step counter |
/// | 63 | 1 | activity class |
/// | 73 | 2 | skin temperature, raw/100 °C |
fn r18(frame: &[u8]) -> Option<BiometricRecord> {
    let record_index = u32le(frame, 11)?;
    let unix_seconds = u32le(frame, 15)?;
    let bpm = *frame.get(22)?;
    // The documented layout holds at most four complete positive words; a
    // larger declared count is an anomaly, so it is clamped rather than used to
    // walk into the neighbouring fields.
    let declared = (*frame.get(23)? as usize).min(4);

    let mut rr_ms = Vec::with_capacity(declared);
    for i in 0..declared {
        let ticks = u16le(frame, 24 + i * 2)?;
        if ticks != 0 {
            rr_ms.push(rr_ms_from_ticks(ticks));
        }
    }

    // Each plausibility range below is the application's, not a device validity
    // flag: out of range means "do not present this as a measurement", never
    // "the strap said it is invalid".
    let skin_temp_c = u16le(frame, 73)
        .map(|raw| f32::from(raw) / 100.0)
        .filter(|c| (5.0..=45.0).contains(c));

    let dynamic_accel_g = f32le(frame, 41).filter(|g| g.is_finite() && (0.0..=8.0).contains(g));

    let gravity_g = match (f32le(frame, 45), f32le(frame, 49), f32le(frame, 53)) {
        (Some(x), Some(y), Some(z)) if x.is_finite() && y.is_finite() && z.is_finite() => {
            Some([x, y, z])
        }
        _ => None,
    };

    // 0 still, 1 walk, 2 run. Every other input class arrives as 0xFF and must
    // not be surfaced as one of those three.
    let activity = frame.get(63).copied().filter(|&a| a <= 2);

    Some(BiometricRecord {
        family: DeviceFamily::Whoop5,
        version: layout::R18_BIOMETRIC,
        record_index: Some(record_index),
        unix_seconds,
        heart_rate: HeartRate {
            source: HrSource::Historical,
            bpm,
            rr_ms,
            unix_seconds: Some(unix_seconds),
        },
        skin_temp_c,
        dynamic_accel_g,
        gravity_g,
        steps: u16le(frame, 57),
        activity,
        quality_flags: frame.get(36).copied(),
        // R18 is a WHOOP 5 layout; the raw WHOOP 4 sensor block has no
        // counterpart in it, and inventing an empty one would imply it does.
        whoop4_sensors: None,
    })
}

/// WHOOP 5 **R20** — 2,140 bytes: a 26-byte header then five 422-byte blocks.
///
/// Block bases are the fixed 26, 448, 870, 1292, 1714. Each block holds a
/// 21-byte configuration header, then two 200-byte sample regions at `B+21` and
/// `B+221` — fifty four-byte slots each, **all present** even when the count is
/// smaller.
///
/// Only `0..count` is decoded. A count of zero means no populated readings, not
/// fifty measured zeros, and a count above 50 is rejected outright.
fn r20(frame: &[u8]) -> Option<OpticalRecord> {
    const BASES: [usize; 5] = [26, 448, 870, 1292, 1714];

    let mut blocks = Vec::with_capacity(BASES.len());
    for base in BASES {
        let count = *frame.get(base)?;
        if count > 50 {
            // An out-of-range count makes the whole record untrustworthy rather
            // than this one block: the capacity is fixed, so a count that
            // cannot fit means the bytes are not this layout.
            return None;
        }
        let n = count as usize;
        let mut slot_a = Vec::with_capacity(n);
        let mut slot_b = Vec::with_capacity(n);
        for i in 0..n {
            // Signed containers, covering −524288..=524287 including the
            // positive rail. Negative values must not become large unsigned
            // measurements, which is the whole reason these are read as i32.
            slot_a.push(i32le(frame, base + 21 + i * 4)?);
            slot_b.push(i32le(frame, base + 221 + i * 4)?);
        }
        blocks.push(OpticalBlock {
            count,
            source_a: *frame.get(base + 1)?,
            source_b: *frame.get(base + 4)?,
            drive_a: u16le(frame, base + 2)?,
            drive_b: u16le(frame, base + 5)?,
            slot_a,
            slot_b,
        });
    }

    Some(OpticalRecord {
        record_index: u32le(frame, 11)?,
        unix_seconds: u32le(frame, 15)?,
        marker: *frame.get(10)?,
        blocks,
    })
}

/// WHOOP 5 **R21** — 1,244 bytes, a one-second six-axis buffer.
///
/// | Offset | Field |
/// |---:|---|
/// | 24 | accelerometer count, capacity 100 |
/// | 28 + 2i, 228 + 2i, 428 + 2i | acceleration x, y, z |
/// | 630 | gyroscope count, capacity 100 |
/// | 640 + 2i, 840 + 2i, 1040 + 2i | gyroscope x, y, z |
///
/// Each of the six columns reserves 200 bytes whether or not the count fills
/// them. The two counts are independent and are **not** required to be equal.
///
/// Scales are the retained conventions — `raw / 4096` g and
/// `raw × 2000 / 32768` °/s — applied interpretations rather than confirmed
/// device parameters, which is why the raw counts remain recoverable by
/// reversing them exactly.
fn r21(frame: &[u8]) -> Option<ImuRecord> {
    const ACCEL_COLUMNS: [usize; 3] = [28, 228, 428];
    const GYRO_COLUMNS: [usize; 3] = [640, 840, 1040];
    const CAPACITY: usize = 100;
    const ACCEL_SCALE: f32 = 1.0 / 4096.0;
    const GYRO_SCALE: f32 = 2000.0 / 32768.0;

    // Counts are u8 values widened to u16 in this version, so their high bytes
    // are zero; bound each against the column capacity before consuming it.
    let accel_count = (u16le(frame, 24)? as usize).min(CAPACITY);
    let gyro_count = (u16le(frame, 630)? as usize).min(CAPACITY);

    let read = |columns: [usize; 3], count: usize, scale: f32| -> Option<Vec<[f32; 3]>> {
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            out.push([
                f32::from(i16le(frame, columns[0] + i * 2)?) * scale,
                f32::from(i16le(frame, columns[1] + i * 2)?) * scale,
                f32::from(i16le(frame, columns[2] + i * 2)?) * scale,
            ]);
        }
        Some(out)
    };

    let unix_seconds = u32le(frame, 15)?;
    Some(ImuRecord {
        record_index: u32le(frame, 11)?,
        timestamp: inertial_timestamp(unix_seconds, u16le(frame, 19)?),
        unix_seconds,
        accel_g: read(ACCEL_COLUMNS, accel_count, ACCEL_SCALE)?,
        gyro_dps: read(GYRO_COLUMNS, gyro_count, GYRO_SCALE)?,
    })
}

/// WHOOP 5 **R26** — 88 bytes: an optical base value plus 24 adjacent deltas.
///
/// | Offset | Field |
/// |---:|---|
/// | 11 | record index |
/// | 15 | Unix seconds |
/// | 21 | burst counter |
/// | 23 | absolute first optical code |
/// | 27 + 2i | `i16` delta for sample i+1 |
/// | 75 | summary motion, f32 |
/// | 81 | on-wrist-related byte |
///
/// Reconstructed in `i64` because the accumulator has to be wider than the
/// values: 24 deltas of up to ±32767 on top of a `u32` base do not fit in the
/// type either end came in.
fn r26(frame: &[u8]) -> Option<CompactOpticalRecord> {
    let base = u32le(frame, 23)?;
    let mut samples = Vec::with_capacity(25);
    samples.push(i64::from(base));
    for i in 0..24 {
        let delta = i64::from(i16le(frame, 27 + i * 2)?);
        let previous = *samples.last()?;
        samples.push(previous + delta);
    }

    Some(CompactOpticalRecord {
        record_index: u32le(frame, 11)?,
        unix_seconds: u32le(frame, 15)?,
        burst: u16le(frame, 21)?,
        samples,
        motion: f32le(frame, 75)?,
        wrist_byte: *frame.get(81)?,
    })
}

/// WHOOP 5 **R16/R17** — an ECG waveform. MG hardware.
///
/// Shared header: layout at 9, sensor flags at 10, sequence at 11, timestamp at
/// 15, and a 13-byte packed status region from 21 whose quality code is byte 21
/// and whose declared sample count is the `u16` at 32.
///
/// R17 is 240 bytes with **100 two-byte slots** at 34–233; bytes 234–235 are
/// alignment and never a sample, so `[34,236)` must not be read as 101 samples.
///
/// R16 is 1,584 bytes with **500 three-byte slots** at 34–1533, decoded as an
/// 18-bit two's-complement payload plus two flags:
///
/// ```text
/// raw18 = ((b0 & 0x03) << 16) | (b1 << 8) | b2
/// flag6 = (b0 >> 6) & 1
/// flag7 = (b0 >> 7) & 1
/// ```
///
/// Not little-endian `i16`, and not a signed 24-bit sample. Bits 2–5 of `b0`
/// are reserved and left alone; the flags are kept separate because they are
/// not sign bits.
///
/// A declared count above the layout's capacity is an anomaly: it is refused
/// rather than silently truncated. Zero-valued samples are kept — they can be
/// meaningful, so nonzero detection is no substitute for the count.
fn ecg(frame: &[u8]) -> Option<EcgRecord> {
    let layout_byte = *frame.get(9)?;
    let declared_count = u16le(frame, 32)?;

    let (capacity, width) = match layout_byte {
        layout::R17_ECG_FILTERED => (100usize, 2usize),
        layout::R16_ECG_RAW => (500, 3),
        _ => return None,
    };
    if declared_count as usize > capacity {
        return None;
    }

    let mut samples = Vec::with_capacity(declared_count as usize);
    let mut sample_flags = Vec::new();
    for i in 0..declared_count as usize {
        let off = 34 + i * width;
        if layout_byte == layout::R17_ECG_FILTERED {
            samples.push(i32::from(i16le(frame, off)?));
        } else {
            let b0 = *frame.get(off)?;
            let b1 = *frame.get(off + 1)?;
            let b2 = *frame.get(off + 2)?;
            let raw18 = ((u32::from(b0) & 0x03) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            // Two's complement over 18 bits: at or above 2^17, subtract 2^18.
            let signed = if raw18 >= 131_072 {
                raw18 as i32 - 262_144
            } else {
                raw18 as i32
            };
            samples.push(signed);
            sample_flags.push((b0 >> 6) & 1 == 1);
        }
    }

    // The 13-byte packed status region, kept whole. The codes are observed but
    // their vocabulary is not, so expanding them into named booleans would be
    // inventing a contract.
    let status = frame.get(21..34).map(<[u8]>::to_vec).unwrap_or_default();

    // R16's lead arrays sit at fixed offsets that do **not** move with the
    // waveform count — locating them with `header + count × width` is the
    // documented way to read the wrong bytes. The declared count is reported
    // beside the arrays rather than used as their length, so a count above the
    // eleven available slots cannot truncate or overrun them.
    let lead_off = (layout_byte == layout::R16_ECG_RAW)
        .then(|| {
            let halfwords = |base: usize| -> Option<Vec<u16>> {
                (0..11).map(|i| u16le(frame, base + i * 2)).collect()
            };
            Some(EcgLeadOff {
                count: *frame.get(1534)?,
                i_channel: halfwords(1535)?,
                q_channel: halfwords(1557)?,
            })
        })
        .flatten();

    Some(EcgRecord {
        layout: layout_byte,
        sequence: u32le(frame, 11)?,
        timestamp_word: u32le(frame, 15)?,
        quality: *frame.get(21)?,
        declared_count,
        samples,
        sample_flags,
        status,
        lead_off,
    })
}

// ── WHOOP 4 historical ───────────────────────────────────────────────────────

/// WHOOP 4 historical dispatch — version byte at 5, not a layout byte at 9.
///
/// v12 and v24 share a retained layout map. v25 is a different layout, not v24
/// with shifted fields, and v5/7/9 have their own maps which are not implemented
/// here. An otherwise unknown version stays unknown even when some of its values
/// resemble v24.
fn whoop4_historical(frame: &[u8], ptype: u8) -> Record {
    let version = frame.get(5).copied().unwrap_or(0);
    let unknown = |note: &'static str| Record::Unknown {
        packet_type: ptype,
        version,
        length: frame.len(),
        note,
        raw: frame.to_vec(),
    };

    match version {
        12 | 24 => v24(frame, version)
            .map_or_else(|| unknown("v24 fields did not validate"), Record::Biometric),
        25 => {
            if frame.len() != 84 {
                return unknown("v25 is an 84-byte frame");
            }
            v25(frame).map_or_else(|| unknown("v25 fields did not validate"), Record::Whoop4V25)
        }
        // Generic HR/R-R-only records: the same header prefix as v24 with no
        // DSP sensor block behind it. Decoded as a biometric record carrying
        // exactly the three fields the layout has, which is why the sensor
        // block stays `None` rather than being zero-filled.
        5 | 7 | 9 => whoop4_hr_prefix(frame, version).map_or_else(
            || unknown("v5/7/9 fields did not validate"),
            Record::Biometric,
        ),
        _ => unknown("unknown WHOOP 4 record version"),
    }
}

/// The header prefix v5, v7, v9, v12 and v24 share: time, heart rate, intervals.
///
/// Unix seconds at 11, heart rate at 21, interval count at 22 and the intervals
/// from 23 in **milliseconds** — this generation does not use ticks. What v24
/// adds is the sensor block from 33 onwards; the generic versions stop here.
fn whoop4_hr_prefix(frame: &[u8], version: u8) -> Option<BiometricRecord> {
    let end = payload_end(frame, DeviceFamily::Whoop4)?;
    let unix_seconds = u32le(frame, 11)?;
    let bpm = *frame.get(21)?;
    let declared = *frame.get(22)? as usize;

    let mut rr_ms = Vec::with_capacity(declared);
    for i in 0..declared {
        let off = 23 + i * 2;
        if off + 2 > end {
            break;
        }
        let ms = u16le(frame, off)?;
        if ms != 0 {
            rr_ms.push(u32::from(ms));
        }
    }

    Some(BiometricRecord {
        family: DeviceFamily::Whoop4,
        version,
        // No record-index field on this generation; the absence is reported
        // rather than filled with the frame sequence, which counts something
        // else entirely.
        record_index: None,
        unix_seconds,
        heart_rate: HeartRate {
            source: HrSource::Historical,
            bpm,
            rr_ms,
            unix_seconds: Some(unix_seconds),
        },
        skin_temp_c: None,
        dynamic_accel_g: None,
        gravity_g: None,
        steps: None,
        activity: None,
        quality_flags: None,
        whoop4_sensors: None,
    })
}

/// WHOOP 4 **v24** — the legacy biometric/optical/IMU record. v12 shares it.
///
/// On top of [`whoop4_hr_prefix`]:
///
/// | Offset | Width | Field |
/// |---:|---:|---|
/// | 33, 35 | 2 | green and red/IR optical scalars |
/// | 40/44/48 | 4 | gravity vector, f32 g |
/// | 55 | 1 | skin-contact state |
/// | 56/60/64 | 4 | a second gravity triplet |
/// | 68, 70 | 2 | the SpO₂-adjacent pair |
/// | 72 | 2 | skin temperature, raw ADC |
/// | 74 | 2 | ambient light |
/// | 76, 78 | 2 | LED-drive words |
/// | 80 | 2 | respiration-adjacent raw |
/// | 82 | 2 | signal-quality word |
///
/// Every one of those is a **raw ADC word**: WHOOP computes SpO₂, temperature
/// and respiratory rate from them server-side. The temperature is therefore not
/// centidegrees — the R18 `raw/100` conversion does not apply and no
/// family-specific one is documented — so [`BiometricRecord::skin_temp_c`]
/// stays `None` and the word goes in [`Whoop4SensorBlock::skin_temp_raw`]
/// instead. A decoder that borrowed the WHOOP 5 scale would produce a number in
/// the right ballpark for the wrong reason.
///
/// One 41.17.6.0 offload put 1,704 CRC-valid records of this layout through the
/// equivalent decoder with HR agreeing to about 1 bpm against
/// `60000 / mean(R-R)` and gravity magnitude near 1 g. That validates the
/// decode for that capture, not universal v24 output.
fn v24(frame: &[u8], version: u8) -> Option<BiometricRecord> {
    let mut record = whoop4_hr_prefix(frame, version)?;

    record.gravity_g = match (f32le(frame, 40), f32le(frame, 44), f32le(frame, 48)) {
        (Some(x), Some(y), Some(z)) if x.is_finite() && y.is_finite() && z.is_finite() => {
            Some([x, y, z])
        }
        _ => None,
    };

    let gravity2_g = [
        f32le(frame, 56).unwrap_or(0.0),
        f32le(frame, 60).unwrap_or(0.0),
        f32le(frame, 64).unwrap_or(0.0),
    ];

    record.whoop4_sensors = Some(Whoop4SensorBlock {
        ppg_green: u16le(frame, 33)?,
        ppg_red_ir: u16le(frame, 35)?,
        skin_contact: *frame.get(55)?,
        gravity2_g,
        spo2_red: u16le(frame, 68)?,
        spo2_ir: u16le(frame, 70)?,
        skin_temp_raw: u16le(frame, 72)?,
        ambient: u16le(frame, 74)?,
        led_drive: [u16le(frame, 76)?, u16le(frame, 78)?],
        resp_raw: u16le(frame, 80)?,
        signal_quality: u16le(frame, 82)?,
    });

    Some(record)
}

/// WHOOP 4 **v25** — 84 bytes, emitted by 41.17.6.0 depending on configuration.
///
/// Seconds at 11, a sensor block from 23, and three signed movement values at
/// 73/75/77. The `1/16384` gravity reading is observation-based, not a confirmed
/// device parameter, so both the raw words and the converted values are kept.
///
/// No per-second HR field is mapped in this layout, and none is inferred from
/// the waveform.
fn v25(frame: &[u8]) -> Option<Whoop4V25Record> {
    const SCALE: f32 = 1.0 / 16384.0;
    let movement_raw = [i16le(frame, 73)?, i16le(frame, 75)?, i16le(frame, 77)?];
    Some(Whoop4V25Record {
        unix_seconds: u32le(frame, 11)?,
        movement_raw,
        movement_g: [
            f32::from(movement_raw[0]) * SCALE,
            f32::from(movement_raw[1]) * SCALE,
            f32::from(movement_raw[2]) * SCALE,
        ],
    })
}
