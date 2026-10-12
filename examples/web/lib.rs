//! A web interface for a WHOOP strap: the same library the TUI drives, in a
//! browser, over the browser's own Web Bluetooth.
//!
//! Build it with `examples/web/build.sh`, which also lays out a directory you
//! can serve. `examples/web/README.md` has the details.
//!
//! # Shape of it
//!
//! There is no `wasm-bindgen` here, because `webbluetooth-wasm` does not use
//! one: the boundary is plain numbers across a hand-written shim, and this
//! example crosses it the same way.
//!
//! - Out of the page, into this module: [`whoop_connect`] and
//!   [`whoop_offload`], which the two buttons call.
//! - Out of this module, into the page: the `whoop` import namespace below —
//!   status text, heart rate, battery, events. `app.js` supplies them.
//! - Everything Bluetooth: `webbluetooth`'s own namespace, which the shim fills
//!   in. This file never touches it.
//!
//! # The offload button
//!
//! A browser page is the one place where "persist before acknowledging" needs
//! saying out loud, because a tab can be closed mid-transfer and there is no
//! filesystem to `fsync`. So the page does not acknowledge chunks at all by
//! default: [`whoop_connect`] streams live data only, and [`whoop_offload`]
//! runs the offload in **read-only** mode, handing records to the page and
//! deliberately *not* calling [`PendingChunk::committed`]. The strap keeps
//! everything and resends it next time, which is the correct behaviour for a
//! viewer that cannot promise durability.
//!
//! # Native builds are empty
//!
//! The target is `wasm32-unknown-unknown`. `cargo build --examples` on a native
//! target still builds this file, so the browser half is behind a `cfg` and what
//! remains natively is an empty library — the same arrangement as the binaries,
//! for the same reason.

#[cfg(target_arch = "wasm32")]
mod web {
    use poop::history::{HistorySession, Step};
    use poop::protocol::DeviceFamily;
    use poop::types::WhoopEvent;
    use poop::whoop_client::{WhoopClient, WhoopClientConfig, WhoopHandle};

    // What the page provides. (A plain comment, not a doc comment: rustdoc does
    // not document extern blocks.)
    //
    // `#[link(wasm_import_module)]` names the namespace these arrive in, which
    // is what `app.js` passes to `instantiate` as its extra imports. Without it
    // they would land in `env`, where the shim puts nothing and the page
    // supplies nothing, and instantiation would fail on a missing import.
    #[link(wasm_import_module = "whoop")]
    unsafe extern "C" {
        fn whoop_status(ptr: *const u8, len: usize);
        fn whoop_log(level: u32, ptr: *const u8, len: usize);
        fn whoop_identity(ptr: *const u8, len: usize);
        fn whoop_hr(bpm: u32);
        fn whoop_rr(ms: u32);
        fn whoop_battery(percent: f64);
        fn whoop_event(number: u32, ptr: *const u8, len: usize);
        fn whoop_record(ptr: *const u8, len: usize);
        fn whoop_progress(chunks: u32, records: u32);
        fn whoop_active(active: u32);
    }

    /// Sends the library's `log` records to the page's console.
    ///
    /// Without this a browser build is silent. `env_logger` is native-only — it
    /// reads `RUST_LOG` and writes to stderr, and a page has neither — so every
    /// `info!` in `whoop_client`, including the frame counters that say whether
    /// anything is arriving at all, goes nowhere. The console is the equivalent
    /// surface, and the volume is safe: the dispatch loop logs the first few
    /// frames and then every five hundredth.
    struct PageLog;

    impl log::Log for PageLog {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let line = format!("{}: {}", record.target(), record.args());
            // `Level` is 1..=5, error through trace; `app.js` maps it to the
            // matching console method.
            unsafe { whoop_log(record.level() as u32, line.as_ptr(), line.len()) }
        }

        fn flush(&self) {}
    }

    static PAGE_LOG: PageLog = PageLog;

    /// Tell the page what is happening, in words it can put on screen.
    fn status(message: &str) {
        // The page copies the bytes out before it returns and never keeps the
        // pointer — the same discipline the shim uses on the other side.
        unsafe { whoop_status(message.as_ptr(), message.len()) }
    }

    fn identity(line: &str) {
        unsafe { whoop_identity(line.as_ptr(), line.len()) }
    }

    fn install_logger() {
        // Once per page. A second press is an `Err` rather than a panic, which
        // is why the result is discarded.
        let _ = log::set_logger(&PAGE_LOG);
        log::set_max_level(log::LevelFilter::Info);
    }

    /// Connect to a strap and stream live data from it.
    ///
    /// Has to be reached from a click, not from page load: `requestDevice`
    /// opens the browser's own chooser, and browsers only allow that during a
    /// user gesture. Returning straight away matters for the same reason — the
    /// work goes onto the page's event loop rather than being awaited here, so
    /// the chooser opens inside the window the click grants.
    #[no_mangle]
    pub extern "C" fn whoop_connect() {
        install_logger();
        webbluetooth_wasm::exec::spawn(async {
            // Bracketing the whole task, so the buttons come back whether this
            // ended in a disconnection or an error.
            unsafe { whoop_active(1) };
            if let Err(error) = stream().await {
                // `{:#}` rather than `{}`: the context chain is where the
                // useful half of a BLE failure is written down.
                status(&format!("{error:#}"));
            }
            unsafe { whoop_active(0) };
        });
    }

    /// Connect and read the stored history, **without acknowledging it**.
    ///
    /// See the module docs: a page cannot promise durability, so it never takes
    /// the acknowledgement token. Every record shown here is still on the strap
    /// afterwards.
    #[no_mangle]
    pub extern "C" fn whoop_offload() {
        install_logger();
        webbluetooth_wasm::exec::spawn(async {
            unsafe { whoop_active(1) };
            if let Err(error) = offload().await {
                status(&format!("{error:#}"));
            }
            unsafe { whoop_active(0) };
        });
    }

    /// Connect, report what the strap says about itself, and return the handle.
    async fn open() -> anyhow::Result<(tokio::sync::mpsc::Receiver<WhoopEvent>, WhoopHandle)> {
        status("Opening the browser's device chooser…");

        let config = WhoopClientConfig {
            // The proprietary live packet carries the strap's own clock reading,
            // which the standard Heart Rate Service does not — worth having in
            // a viewer whose whole job is showing what the strap sends.
            realtime_hr: true,
            ..WhoopClientConfig::default()
        };
        let (events, handle) = WhoopClient::new(config).connect().await?;

        let family = handle.family();
        identity(&format!("{}", family.label()));
        if family == DeviceFamily::Whoop5 {
            identity(&format!("variant {:?}", handle.variant()));
            if let Ok(hello) = handle.hello().await {
                identity(&format!("firmware {}", hello.firmware));
            }
        }
        match handle.battery().await {
            Ok(percent) => unsafe { whoop_battery(f64::from(percent)) },
            // A strap that will not answer the battery request is still worth
            // streaming from, so this is reported and not fatal.
            Err(e) => log::warn!("battery unavailable: {e}"),
        }

        Ok((events, handle))
    }

    async fn stream() -> anyhow::Result<()> {
        let (mut events, handle) = open().await?;
        status("Streaming.");

        // Ends when the strap disconnects: the sending half is dropped with the
        // dispatch task, which closes the channel.
        while let Some(event) = events.recv().await {
            match event {
                WhoopEvent::HeartRate(hr) => {
                    unsafe { whoop_hr(u32::from(hr.bpm)) };
                    for interval in hr.rr_ms {
                        unsafe { whoop_rr(interval) }
                    }
                }
                WhoopEvent::Battery(b) => unsafe { whoop_battery(f64::from(b.percent)) },
                WhoopEvent::Event(e) => unsafe {
                    whoop_event(u32::from(e.number), e.name.as_ptr(), e.name.len())
                },
                WhoopEvent::Disconnected => break,
                _ => {}
            }
        }

        let _ = handle.toggle_realtime_hr(false).await;
        status("Disconnected.");
        Ok(())
    }

    async fn offload() -> anyhow::Result<()> {
        let (mut events, handle) = open().await?;

        let mut session = HistorySession::new(handle.family());
        handle.begin_history().await?;
        status("Reading stored history (read-only — nothing is acknowledged).");

        let mut chunks = 0u32;
        let mut records = 0u32;
        while let Some(event) = events.recv().await {
            match session.feed(&event) {
                Step::Chunk(chunk) => {
                    chunks += 1;
                    records += chunk.len() as u32;
                    for record in &chunk.records {
                        // `Record::summary` rather than a local match: this was
                        // the third copy of one, and adding three record
                        // variants broke all three at once. An undecoded record
                        // says so out loud there, which matters here — a viewer
                        // that silently dropped what it could not read would
                        // make a strap emitting an unknown record look like a
                        // strap emitting nothing.
                        let line = record.summary();
                        unsafe { whoop_record(line.as_ptr(), line.len()) }
                    }
                    unsafe { whoop_progress(chunks, records) };
                    // `chunk` is dropped here without `committed()` being
                    // called, which is exactly the intent: no token is sent, so
                    // the strap keeps every record. See the module docs.
                }
                Step::Complete => break,
                _ => {}
            }
        }

        // Tell the strap to stop rather than leaving it streaming into a page
        // that has finished looking.
        let _ = handle.abort_history().await;
        status(&format!(
            "Read {records} record(s) in {chunks} chunk(s). Nothing was acknowledged, \
             so the strap still has all of it."
        ));
        Ok(())
    }
}
