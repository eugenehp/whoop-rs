# poop in a browser

The same `poop` library the CLI and the TUI drive, compiled to
`wasm32-unknown-unknown` and talking to a strap over the browser's own Web
Bluetooth.

## Build and serve

```sh
./build.sh
python3 -m http.server --directory dist 8000
```

Then open <http://localhost:8000/> in Chrome or Edge. Web Bluetooth needs a
secure context, so `localhost` or HTTPS — a `file://` URL will not work, and
Safari and Firefox do not implement it at all.

`build.sh` puts four files in `dist/`:

| File | Where it comes from |
|---|---|
| `web.wasm` | `cargo build --example web --target wasm32-unknown-unknown` |
| `webbluetooth.js` | copied out of the resolved `webbluetooth-wasm`, not vendored |
| `index.html`, `app.js` | this directory |

The shim is copied rather than committed so it cannot drift from the ABI the
module was just compiled against.

## What the two buttons do

**Connect & stream** subscribes to the live sources and asks for the
proprietary heart-rate packet as well as the standard Heart Rate Service. It
turns the proprietary stream off again on the way out.

**Read history** runs the offload in **read-only** mode. It reads the strap's
stored records and shows them, and it never acknowledges a chunk — so every
record is still on the strap afterwards.

That is deliberate. Acknowledging a chunk is what lets the strap reclaim the
space, so the acknowledgement must come after the data is durably stored. A page
cannot promise that: a tab can be closed mid-transfer and there is no
filesystem to `fsync`. `examples/web/lib.rs` therefore drops each
`PendingChunk` without calling `committed()`, which is the API's way of saying
"seen, not stored".

Use `poop history` if you want an offload that keeps what it reads.

## No wasm-bindgen

`webbluetooth-wasm` does not use one, and neither does this: the boundary is
plain numbers and pointers across a hand-written shim. `lib.rs` declares an
extern block in the `whoop` import namespace, `app.js` supplies those functions
alongside the shim's own, and strings are read out of the module's memory with a
`TextDecoder`. Views are rebuilt per call because linear memory can grow and
detach them.

## No render worker

The muse-rs example moves its drawing to an `OffscreenCanvas` in a worker,
because 256 Hz across eight channels is half a million path operations a second.
A WHOOP sends a heart rate about once a second. Everything here runs on the main
thread, which is where Web Bluetooth has to live anyway — `navigator.bluetooth`
is not exposed on `WorkerNavigator`.
