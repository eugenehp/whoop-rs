# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) — noting
that in the `0.0.x` range Cargo treats every release as incompatible with every
other, so a patch bump here is not a compatible upgrade.

## [0.0.1] — 2026-10-12

First release. Both generations, the full read surface, and a simulated strap so
none of it needs hardware to exercise.

The crate and its CLI are named `poop`; the repository is `whoop-rs`.

### Transport

- **WHOOP 4 envelope** — `AA | len u16le | crc8 | type | seq | cmd | payload |
  crc32`, total `len + 4`, CRC8 over the two length bytes, CRC32 over the inner
  record.
- **WHOOP 5/MG format 1** — `AA | 01 | declLen | hdr | crc16 | type | seq | cmd
  | body | pad | crc32`, total `declLen + 8`, CRC16-Modbus over `[0,6)`, CRC32
  over `[8, total-4)` including the padding. The inner record is padded to a
  four-byte boundary **before** the length and checksum are computed.
- `verify_frame` reports structure and both checksums as one verdict with a
  reason, and preserves a computable payload CRC even when the envelope is
  wrong — "payload right, envelope wrong" is the class a capture exists for.
- `Reassembler`, one per notify characteristic, resyncing on the header
  checksum rather than on a bare `0xAA`. Drops are counted in three buckets
  (below-floor length, bad header checksum, impossible size) because they say
  different things about a link.
- Family selection throughout: every function in `protocol` and `parse` takes a
  `DeviceFamily`, and no WHOOP 5 offset is reached by adding four to a WHOOP 4
  one.

### Commands

- A curated `Command` set with per-family frame builders, and request bodies in
  `protocol::payload` — clock (both WHOOP 4 forms and the WHOOP 5 revision-1
  pair), alarms, haptics, named configuration, advertising names, wear-detection
  override, LED accessibility, history acknowledgement, ECG.
- Request/response correlation on the **originating** sequence, registered
  before the write so a reply arriving during it is not missed. A `PENDING`
  result keeps the request open until the final one, within a bounded wait.
- `SendPolicy` gates three separate decisions: destructive commands are refused
  outright, the MG ECG family needs `policy.ecg` plus WHOOP 5, configuration
  writes need `policy.config_write`.
- `RebootProbe` — the five restart candidates, one per call, because which one a
  strap honours is an open question and one observation showed command 29 doing
  nothing at all.

### Records

- **WHOOP 5** R18 biometric, R20 optical, R21 IMU, R26 compact optical, and
  R16/R17 ECG including the 13-byte status region and R16's eleven I/Q lead
  halfwords at their fixed offsets.
- **WHOOP 4** v12/v24 with its raw ADC sensor block, v5/7/9 as generic HR
  records, v25's movement-only layout, and the live type-43 variants selected by
  validated payload length — 1,917-byte six-axis IMU and 1,921-byte optical,
  whose fourth stride byte is retained rather than dropped.
- **Packet types 51/52**, the dedicated IMU stream, with every array positioned
  from the declared counts and the whole calculated span validated first.
- Everything unrecognised is preserved whole with its version, length and the
  reason it was not decoded, carried **inside** the record — so a capture holds
  one copy of those bytes, inside the same `fsync` as the decoded rows. R22 is
  the one documented layout with no decoder, and it says so.

### History

- The offload state machine, with the durability rule expressed in the type
  system: the acknowledgement token is reachable only through
  `PendingChunk::committed`, which consumes the chunk, and `ack_history` accepts
  only what that produces.
- An idle watchdog re-armed by offload traffic alone — the standard Heart Rate
  Service notifies about once a second whether or not history is moving, and a
  watchdog re-armed by every event would never fire.

### Configuration and service reads

- Named configuration: 65-byte key/value records (commands 121/128), the bounded
  enumeration walk (115/116 and 117/118) with named stop conditions, and the R22
  disable sequence that verifies each write by reading it back.
- Inventories for the eight device keys and 25 feature-flag descriptors,
  including the three whose polarity is counter-intuitive.
- Alarms: the WHOOP 5 revision-4 record with explicit crescendo and local
  pattern validation, GET/RUN/DISABLE and their detail codes, and the WHOOP 4
  legacy slot.
- Battery pack (151), extended battery (98), advertising name (141),
  body-location status (84), and the WHOOP 4 serial behind a two-sighting gate.

### Simulation

- `sim` — a simulated strap for both generations that builds complete frames
  with correct checksums and runs them through `whoop_client::decode_frame`,
  the same function the live dispatch loop calls. Deterministic, so a session
  can be replayed while chasing a bug.
- It is stricter than reality in one place: the simulated offload refuses an
  acknowledgement that does not echo its token byte for byte, and resends up to
  four times before abandoning the attempt.
- `strap::Strap` presents a live handle and a simulated one through one type, so
  `--simulate` is the same code path as the real thing rather than a parallel
  one.

### Front ends

- **CLI** (`poop`) — `scan`, `info`, `stream`, `history`, `buzz`, plus `config`, `alarm`
  and `probe` subcommands. `history` writes NDJSON and `fsync`s each chunk
  before acknowledging it; a write failure withholds the acknowledgement and
  ends the session, leaving the data on the strap.
- **TUI** — heart-rate trace with an axis fitted to the data, R-R, battery and
  wrist tiles, event and record logs, and an offload with the same
  write-then-acknowledge ordering. The header reads `SIMULATED` in a different
  colour when it is.
- **Browser** — the same library on `wasm32-unknown-unknown` over the browser's
  own Web Bluetooth, with no `wasm-bindgen`. Its history button is read-only and
  never acknowledges a chunk, because a tab cannot promise durability.

### Layout

`protocol` and `parse` are module directories rather than single files —
framing, commands, payloads, configuration, identity and records on one side;
live streams, stored records and response bodies on the other — re-exported
through one flat namespace, because a caller decoding a frame should not have to
know which file a constant lives in.

`store::NdjsonSink` exists as **one** implementation of write-then-`fsync`
rather than a copy in each front end: that `fsync` is what makes an
acknowledgement safe to send, and two copies is two places for the ordering to
drift, where the symptom of drift is silent data loss rather than a failing
test. Likewise `Record::summary` lives on the type — there were three copies of
that match, and adding three record variants broke all three builds at once.

### Verification

128 tests, all frame-level: each builds a complete frame, runs it through the
real decoder and asserts on the result, including the cases that must **not**
decode.

No WHOOP strap has been in range of this code. Every layout is verified against
constructed frames with correct checksums; a constructed frame is not a
capture.
