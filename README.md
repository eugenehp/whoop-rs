# whoop-rs

[![crates.io](https://img.shields.io/crates/v/poop.svg)](https://crates.io/crates/poop)
[![docs.rs](https://docs.rs/poop/badge.svg)](https://docs.rs/poop)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

An async Rust client for **WHOOP 4.0**, **WHOOP 5.0** and **WHOOP MG** straps
over Bluetooth Low Energy — library, CLI, terminal UI and a browser build from
one codebase, on [`webbluetooth`](https://docs.rs/webbluetooth).

> The crate and its CLI are named **`poop`**; this repository is `whoop-rs`.

Everything happens on the strap and on your machine. There is no account, no
cloud call and no network code in this crate at all.

```
┌───────────────────────────────────────────────────────────────────┐
│  poop (library)                                                   │
│                                                                   │
│  protocol  ── UUIDs · CRC8/CRC16/CRC32 · framing · commands ·     │
│               payload builders · config inventories · send gate   │
│  parse     ── pure byte→record decoders, fail-closed              │
│  types     ── WhoopEvent and the record structs                   │
│  history   ── offload state machine + the durability rule         │
│  client    ── scan · connect · dispatch · request/response        │
│  sim       ── a simulated strap that builds real frames           │
│  strap     ── one handle over a live strap or a simulated one     │
│  store     ── an NDJSON sink that fsyncs before you acknowledge   │
│  platform  ── spawn/sleep/timeout, tokio natively, event loop in  │
│               a page                                              │
└───────────┬───────────────────┬───────────────────┬───────────────┘
            │                   │                   │
         poop (CLI)         tui (ratatui)     examples/web (wasm)
```

No hardware? Everything below works against a simulated strap:

```sh
poop stream  --simulate --duration 30
poop history --simulate --out demo.ndjson
cargo run --bin tui -- --simulate
```

## Supported hardware

| Strap | Service | Header check | Baseline firmware | ECG |
|---|---|---|---|---|
| WHOOP 4.0 | `61080001-…` | CRC8, poly `0x07` | 41.17.6.0 | ✗ |
| WHOOP 5.0 | `fd4b0001-…` | CRC16-Modbus | 50.42.1.0 | ✗ |
| WHOOP MG | `fd4b0001-…` | CRC16-Modbus | 50.42.1.0 | ✓ |

The generation is detected from the advertised service and then confirmed
against the services the strap exposes — never from its name, which is
user-settable. WHOOP 5.0 and MG share one protocol profile; what differs is the
ECG-conductive clasp, resolved separately from Device Information strings.

**The two generations share vocabulary, not bytes.** A command name, a packet
number and a record layout each mean something different per family, and WHOOP 5
history is not a WHOOP 4 layout with offsets shifted by four. Every function in
`protocol` and `parse` takes a `DeviceFamily` for that reason.

## Install

```sh
cargo install poop            # the CLI and the TUI
```

As a dependency:

```toml
[dependencies]
# Full build (includes the ratatui TUI feature):
poop = "0.0.1"

# Library only — skips ratatui / crossterm compilation:
poop = { version = "0.0.1", default-features = false }
```

While the version is `0.0.x`, Cargo treats every release as incompatible with
every other, so that requirement pins exactly `0.0.1` rather than accepting a
later patch. The same applies to `webbluetooth`, and it matters there: two
copies of it in one dependency graph means two BLE sessions and straps that
will not connect. Keep every `webbluetooth` requirement in a consuming graph on
the same `0.0.x`.

On **macOS** the binaries embed an `Info.plist` with
`NSBluetoothAlwaysUsageDescription` via `build.rs`; without it CoreBluetooth
silently refuses to scan. Nothing to configure — but if you wrap this crate in
your own binary, do the same.

## The CLI

```sh
poop scan                              # what is in range
poop info --family 5                   # identity, battery, clock, range
poop stream --realtime --duration 60   # live traffic
poop history --out tonight.ndjson      # offload the ring buffer
poop buzz                              # confirm which strap this is

poop config flags                      # walk the feature-flag names
poop config get cont_collection_mode   # read one named value
poop config set enable_rfid 0 --allow-writes
poop config disable-r22 --allow-writes # clear R22, verifying each readback

poop alarm set 1760230800000           # arm for an absolute instant
poop alarm get 1                       # read a slot
poop alarm off                         # disarm every slot

poop probe location                    # body-location status
poop probe battery-pack                # cached fuel-gauge record
poop probe name                        # advertised name
poop probe serial                      # WHOOP 4 serial, via two hellos
```

```
$ poop info
connecting to WHOOP 4B2C (1A2B3C…D7E8F9) as WHOOP 5.0 / MG

strap      WHOOP 4B2C (1A2B3C…D7E8F9)
family     WHOOP 5.0 / MG
variant    Mg
battery    47%
firmware   50.42.1.0
clock      1760230751 (in step with this host)
history    118 page(s) behind of 4096 capacity, ~1770 record(s) estimated
newest     1760230740 (in step with this host)
```

`RUST_LOG=debug` adds BLE-level diagnostics on stderr. The TUI writes its logs
to `poop-tui.log` instead, so they cannot corrupt the display.

Destructive commands are not reachable from the CLI at all. Configuration
writes need `--allow-writes`, ECG commands need `--allow-ecg` plus MG hardware,
and `probe reboot <1-5>` restarts the strap — so it names the candidate it sent
and interrupts anything in flight.

## The library

```rust
use poop::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = WhoopClient::new(WhoopClientConfig::default());
    let (mut events, handle) = client.connect().await?;

    println!("battery: {:.0}%", handle.battery().await?);

    while let Some(event) = events.recv().await {
        match event {
            WhoopEvent::HeartRate(hr) => println!("{} bpm {:?} ms", hr.bpm, hr.rr_ms),
            WhoopEvent::Event(e) => println!("strap event: {}", e.name),
            WhoopEvent::Disconnected => break,
            _ => {}
        }
    }
    Ok(())
}
```

`WhoopHandle` also has `hello()`, `data_range()`, `set_clock()`,
`toggle_realtime_hr()`, `begin_history()`, `abort_history()`, `buzz()`,
`ecg_generation()` and the raw `send()` / `request()` pair.

### Offloading history safely

A strap keeps its records in a ring buffer and hands them over in chunks, each
closed by an eight-byte token it wants echoed back. **A successful
acknowledgement can let the strap reclaim that history**, so the acknowledgement
has to come after the chunk is durably stored — and has to be withheld if
storing it failed.

The `history` module enforces that with types rather than with a warning. The
acknowledgement bytes are only reachable through `PendingChunk::committed()`,
which consumes the chunk, and `WhoopHandle::ack_history` takes only what that
returns. There is no method that hands you the token alongside the records, so
"acknowledge, then store" is not a sequence you can write by accident.

```rust
use poop::history::{HistorySession, Step};
use poop::prelude::*;

let mut session = HistorySession::new(handle.family(), true);
handle.begin_history().await?;

while let Some(event) = events.recv().await {
    match session.feed(&event) {
        Step::Chunk(chunk) => {
            store(&chunk.records)?;                 // durable first …
            if let Some(ack) = chunk.committed() {  // … then the token
                handle.ack_history(&ack).await?;
            }
        }
        Step::Complete => break,
        _ => {}
    }
}
```

Drop the chunk instead of committing it and nothing is acknowledged: the strap
keeps the data and sends it again next time. That is what the browser example
does deliberately, because a tab cannot promise durability.

```text
SEND_HISTORICAL_DATA(22, [00])
   └─ HISTORY_START ──▶ records accumulate
       ├─ HISTORY_END ──▶ Step::Chunk(PendingChunk)
       │     write → flush → fsync
       │     chunk.committed() → HISTORICAL_DATA_RESULT(23, [01] + token)
       └─ HISTORY_COMPLETE ──▶ Step::Complete
```

This is not exactly-once delivery, and nothing here pretends otherwise. The
strap resends an unanswered end marker up to four times and abandons the attempt
on the fifth; a fresh attempt restarts from the acknowledged boundary, so
records you received but did not acknowledge come round again. Tolerate
duplicates and keep arrival order.

## Working without hardware

`--simulate` gives you a strap that is not there. It does **not** push events
into a channel — it builds complete frames with correct checksums and runs them
through `whoop_client::decode_frame`, the same function the live dispatch loop
calls:

```
sim::Strap ──▶ FrameBuilder ──▶ complete frame + CRCs
                                      │
                                      ▼
                          whoop_client::decode_frame
                           (verify → route → decode)
                                      │
                                      ▼
                            WhoopEvent, as if live
```

So framing, verification, layout selection, offset tables and every unit
conversion are the real code. A decoder bug shows up in simulation, which is
the entire point — a simulator that emitted events directly would be testing a
second implementation of the protocol living next to the first.

It simulates both generations as genuinely different frames: WHOOP 4 with time
at offset 6 and R-R in milliseconds, WHOOP 5 with time at 10 and R-R in ticks
of 1/1024 s. That is how it catches a decoder applying one family's units to
the other.

```
$ poop history --simulate --out demo.ndjson
simulating a WHOOP 5.0 / MG
writing to demo.ndjson

stream started
chunk 1: 50 record(s) stored and acknowledged (cursor Some(100050))
chunk 2: 50 record(s) stored and acknowledged (cursor Some(100100))
chunk 3: 50 record(s) stored and acknowledged (cursor Some(100150))
chunk 4: 30 record(s) stored and acknowledged (cursor Some(100180))

strap reported the attempt complete
180 record(s) in 4 chunk(s) from WHOOP SIM (simulated) → demo.ndjson
this was a simulated strap — none of it is a measurement
```

### It also checks the client

The simulated offload will not advance its cursor unless the acknowledgement
echoes its eight-byte token **exactly**. The second word is deliberately not
derivable from the first, so a client that rebuilt the token from its decoded
trim cursor — the specific mistake `history` exists to prevent — gets the chunk
resent and the reason logged. It resends up to four times and abandons the
attempt on the fifth, like the real strap, so a client that never acknowledges
sees the transfer end rather than hanging.

Simulation is marked everywhere it surfaces: the CLI says so on the way in and
out, and the TUI header reads `SIMULATED` in a different colour. A reading from
a simulated strap is not a measurement, and the two should never be
screenshot-identical.

Configuration, alarms and the probes are **live-only** and say so. There is no
simulated feature-flag namespace, because inventing one would turn
`--simulate` into a source of confident nonsense.

## What a success is not

The protocol is unusually quick to look like it is working. Worth knowing before
you build on it:

- A **write acknowledgement** means the radio took the bytes.
- A **result of 1** means the strap accepted the request at that command's
  response boundary. Not that asynchronous work finished, that a sensor started,
  or that anything was stored.
- **`HISTORY_COMPLETE`** means the attempt is over, not that every record
  arrived — it can be emitted with a backlog remaining.
- **Reconnecting** restores no sensor session, and a disconnect is not an
  acknowledgement that one was torn down. Re-request collection explicitly.
- A **clock read** has no validity flag. A failure can return zero time beside a
  success, and a strap whose clock was never set reports a number that looks
  like a date. Read it back; `poop info` flags implausible values.
- **Unresolved means unresolved, not absent.** An unknown record layout is
  preserved whole rather than forced through a decoder written for a different
  record, and an unresolved 5.0/MG variant does not mean the strap lacks MG
  hardware.

## Protocol notes

### Framing

```text
WHOOP 4    AA │ len u16le │ crc8 │ type │ seq │ cmd │ payload │ crc32
           [0]│ [1..3)    │ [3]  │ [4]  │ [5] │ [6] │ [7..)   │ [len..+4)
           total = len + 4 · crc8 over bytes 1–2 · crc32 over [4, len)

WHOOP 5    AA │ 01 │ declLen │ hdr │ crc16 │ type │ seq │ cmd │ body │ pad │ crc32
           [0]│[1] │ [2..4)  │[4..6)│[6..8) │ [8]  │ [9] │[10] │[11..)│     │
           total = declLen + 8 · crc16 over [0,6) · crc32 over [8, total-4)
```

The WHOOP 5 inner record is zero-padded to a four-byte boundary **before** the
declared length and the CRC32 are computed. Padding is not a semantic argument:
an explicit `00` and a missing argument are different requests even when the
frames look alike.

Response correlation uses the **originating request sequence** echoed in the
reply (`[7]` on WHOOP 4, `[11]` on WHOOP 5) — not the reply's own sequence byte,
which the strap generates fresh.

### Reassembly

One `Reassembler` per notify characteristic, because each channel is its own
byte stream. A bare `0xAA` occurs inside payload as readily as at a frame start,
so the header checksum — which already covers the length field on both families
— gates acceptance before any bytes are consumed. Without that, a desynced
stream can latch onto a payload byte whose next bytes read as a plausible
length, swallow every real frame inside it, and hand the parser one long piece
of nonsense. Drops are counted and reported in three buckets, because a
below-floor length, a bad header checksum and an impossible size say different
things about a link.

### Records

| Packet / layout | Bytes | Decoded as |
|---|---:|---|
| R18 biometric | 124 | HR, up to 4 R-R, accel/gravity, steps, activity, skin temp |
| R20 optical | 2,140 | 5 blocks × 2 slots × ≤50 signed samples, plus config |
| R21 IMU | 1,244 | 6 axes × ≤100 `i16`, independent accel/gyro counts |
| R26 compact optical | 88 | base + 24 clipped deltas, reconstructed in `i64` |
| R16 ECG raw | 1,584 | 500 × 18-bit two's complement, flags, 11 I/Q lead halfwords |
| R17 ECG filtered | 240 | 100 × `i16`, declared count only |
| WHOOP 4 v12/v24 | — | HR, R-R in ms, gravity, plus the raw ADC sensor block |
| WHOOP 4 v5/7/9 | — | generic HR/R-R records, no sensor block |
| WHOOP 4 v25 | 84 | movement only; no HR field is mapped, so none is invented |
| Type 43 / 1,917 | 1,924 | WHOOP 4 live six-axis IMU, 100 samples per axis, with HR |
| Type 43 / 1,921 | 1,928 | WHOOP 4 live optical, 419 × signed 24-bit, aux byte kept |
| Type 51 / 52 | — | dedicated IMU stream, arrays positioned from the counts |
| R22 | — | **preserved raw** — documented, not implemented |

Fixed regions do not move when a declared count is smaller, so every decoder
checks the validated frame length first. A record of the wrong size is not this
layout with fields missing — it is a different record, and decoding it would
read real numbers out of the wrong offsets.

Two of those need their position computed rather than looked up. The type-43
variants are selected by **validated payload length** — 1,917 and 1,921, four
bytes apart, on the same packet type — so choosing by type would decode an
optical waveform as motion. Types 51/52 position every array from the declared
counts (`accel X @ 28`, `Y @ 28+2A`, … `gyro X @ 28+6A`), so the whole
calculated span is validated first: a lying count there does not mis-slice one
array, it slides every later one.

Unit discipline: WHOOP 5 R-R intervals are ticks of 1/1024 s and WHOOP 4's are
already milliseconds; R18 skin temperature is `raw/100 °C` while WHOOP 4 v24's
is a raw ADC word with no documented conversion, so **no °C is reported for it**
rather than borrowing the other generation's scale — the word goes in the raw
sensor block instead. Types 51/52 keep raw 16-bit words because even their
signedness is unspecified, and R21's `raw/4096` g is explicitly not theirs.
Optical values keep raw names because no calibrated unit is established.

### Named configuration

Eight device keys and 25 feature-flag descriptors are in `protocol`, with their
value types and what is actually established about each. Three of them are
counter-intuitive and the inventory says so: writing ASCII `2` to the R22
master *removes* its contribution, `disable_pip_r26_packets` has inverse
polarity, and `enable_raw_data_w_ecg` treats both `0` and `1` as true.

Reads go through commands 121/128 (a 65-byte key/value record); the enumeration
walk uses 115/116 and 117/118 and reports **why** it stopped — only
`index = 255` is the strap saying it is finished, and everything else
(`EmptyRun`, `ParkedCursor`, `AnnouncedCount`, `HardCap`) is this side's limit,
which must not read as a complete key list. An invalid entry alone does not
stop the walk: it could equally be an empty slot with the list continuing past
it, and on the walks on record the two readings have never been separated.

### Alarms

WHOOP 5 takes six slots and a 21-byte revision-4 record; crescendo is supplied
explicitly rather than left to framing padding, because a body that relies on
padding cannot distinguish "ordinary pattern" from "field absent". The pattern
is validated locally first (effects ≤ 251, repeats < 8, duration 30–120 when
repeats is 7), since a rejected request still costs a round trip and may have
stored half of itself.

The response detail is **separate from the outer result**, and they disagree in
a documented way: a pattern can be stored while the time is rejected. Both
halves are reported. Scheduling is one-shot — a due alarm is cleared before the
haptic finishes — so recurrence means arming the next absolute instant.

### Safety gate

Refused before a frame is built, by `protocol::admits_send`:

| Code | Command | Hazard |
|-----:|---|---|
| 15 | `FORGET_BONDS` | drops the bond |
| 25 | `FORCE_TRIM` | invasive cursor/reclaim; unoffloaded data may go |
| 32 | `POWER_CYCLE_STRAP` | hard power cycle |
| 33 | `SET_READ_POINTER` | invasive cursor mutation |
| 36, 37, 38 | firmware load / data / process | firmware write |
| 45 | `ENTER_BLE_DFU` | enters the bootloader |
| 99 | `RESET_FUEL_GAUGE` | resets the battery gauge |
| 142, 143, 144 | the high-opcode firmware family | firmware write |

Two further gates, both opt-in through `SendPolicy`:

| Family | Commands | Needs |
|---|---|---|
| MG ECG | 123, 124, 125, 139 | `policy.ecg` **and** a WHOOP 5 connection |
| Configuration writes | 119, 120, 140, 148, 149 | `policy.config_write` |

ECG commands change acquisition state, and repeated starts can lose the record
a companion collection needs to clean up. Configuration writes persist, and 148
disables wear detection outright. They are separate flags rather than one
`unsafe` bool, because permitting one should not permit the other.

`RebootProbe` is reachable — which restart candidate a strap honours is a real
open question, and one device observation showed command 29 doing nothing at
all — but a caller has to name one of the five candidates, and it restarts the
strap.

## The terminal UI

```sh
cargo run --bin tui                 # scan, connect, stream
cargo run --bin tui -- --family 5   # only WHOOP 5/MG
```

Heart-rate trace with an axis fitted to the data, R-R and battery tiles, strap
events, and `o` to run an offload with the same write-then-acknowledge ordering
as the CLI. `tab` opens a strap picker; full keys are in the module docs.

## The browser

```sh
./examples/web/build.sh
python3 -m http.server --directory examples/web/dist 8000
```

Then <http://localhost:8000/> in Chrome or Edge — Web Bluetooth needs a secure
context, and Safari and Firefox do not implement it.

The same library, compiled to `wasm32-unknown-unknown`. There is no
`wasm-bindgen`: `webbluetooth-wasm` marshals plain numbers across a hand-written
shim, and the example crosses the boundary the same way. `src/platform.rs`
routes spawning and timers onto the page's event loop, because tokio's timer is
not implemented for that target — enabling it would produce a library that links
and then panics on the first runtime it builds.

"Read history" in the page is **read-only**: it shows the records and never
acknowledges a chunk, so the strap keeps all of it. See
[`examples/web/README.md`](examples/web/README.md) for why.

## Not implemented

Honest gaps, rather than silent ones:

- **R22 queued channels.** The version-9 channel encoding is documented;
  nothing here decodes it, so those records arrive as preserved unknowns.
- **Format 2** framing. Its runtime availability and session negotiation are
  unresolved upstream, and observed ordinary traffic uses format 1.
- **Firmware update and certificate transfer** (commands 36–38, 83, 142–144,
  155–159). Deliberately out of scope: the writes are on the destructive list
  and there is no installation path to validate against.
- **Gyro mode (150/152)** and the **persistent collection toggles (153/154)**
  are named in the decode catalogue only — no sender, because no request
  contract has been validated.
- ECG sample timing and voltage calibration, the vocabulary of the 13-byte
  status region, several physiological field meanings, and parts of the
  runtime/error interaction remain unresolved upstream. Nothing here invents
  them: the status region is preserved whole rather than expanded into named
  booleans.

Still unvalidated against hardware, separately from the above: **no WHOOP strap
has been in range of this code.** Every layout is verified against constructed
frames with correct checksums, and the simulator exercises both generations end
to end — but a constructed frame is not a capture. Treat every physical scale
here as the applied interpretation it is documented to be.

## Testing

```sh
cargo test                                   # 127 tests
cargo clippy --all-targets                   # clean
cargo check --target wasm32-unknown-unknown  # the browser half
poop history --simulate --out /tmp/x     # the offload path, end to end
```

The tests are frame-level: each one builds a complete frame with correct
checksums, feeds it through the real decoder, and asserts on what comes out —
including the cases that should **not** decode. The hardest-won ones pin
mistakes that are easy to make and invisible afterwards:

- reading WHOOP 4's offsets on a WHOOP 5 frame, in both directions
- treating the reply's own sequence byte as the request echo
- rebuilding an acknowledgement token from the trim cursor
- selecting a type-43 layout by packet type instead of payload length
- letting an IMU-stream count slide every array after the first
- truncating an ECG count instead of rejecting it
- reading R16's lead arrays at `header + count × width`
- borrowing R18's `raw/100 °C` for a WHOOP 4 raw ADC word
- letting a declared count walk into the CRC trailer

The simulator is itself tested: every frame it generates must pass the real
verifier, decode back to the values it encoded, and round-trip its end token —
and a rebuilt token must be refused.

## Scope

Independent, and **unaffiliated with WHOOP**. Not a medical device, and nothing
it produces is a clinical measurement. The wire contracts implemented here are
recorded as factual observations for interoperability; no proprietary code is
reproduced. See [DISCLAIMER.md](DISCLAIMER.md).

Licensed under [Apache-2.0](LICENSE).
