//! # poop
//!
//! Async Rust library, terminal UI and browser client for WHOOP straps over
//! Bluetooth Low Energy, built on the Web Bluetooth API via
//! [`webbluetooth`](https://docs.rs/webbluetooth) — so the same code runs
//! natively and in a page.
//!
//! The crate is `poop`; the repository is
//! [whoop-rs](https://github.com/eugenehp/whoop-rs).
//!
//! ## Supported hardware
//!
//! | Strap | Service | Framing | Baseline firmware | ECG |
//! |---|---|---|---|---|
//! | WHOOP 4.0 | `61080001-…` | CRC8 header, CRC32 payload | 41.17.6.0 | ✗ |
//! | WHOOP 5.0 | `fd4b0001-…` | CRC16-Modbus header, CRC32 payload | 50.42.1.0 | ✗ |
//! | WHOOP MG | `fd4b0001-…` | as 5.0 | 50.42.1.0 | ✓ |
//!
//! The generation is detected from the advertised service and confirmed against
//! the services the strap exposes — never from its name, which is user-settable.
//! WHOOP 5.0 and MG share one protocol profile; what differs is the
//! ECG-conductive clasp, resolved separately from Device Information strings by
//! [`protocol::whoop5_variant`].
//!
//! The two generations **share vocabulary, not bytes**. A command name, a packet
//! number and a record layout each mean something different per family, and
//! WHOOP 5 history is not a WHOOP 4 layout with offsets shifted by four.
//! Everything in [`protocol`] and [`parse`] takes a [`protocol::DeviceFamily`]
//! for that reason.
//!
//! ## Quick start
//!
//! ```no_run
//! use poop::prelude::*;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let client = WhoopClient::new(WhoopClientConfig::default());
//!     let (mut events, handle) = client.connect().await?;
//!     println!("battery: {:.0}%", handle.battery().await?);
//!
//!     while let Some(event) = events.recv().await {
//!         match event {
//!             WhoopEvent::HeartRate(hr) => println!("{} bpm {:?}", hr.bpm, hr.rr_ms),
//!             WhoopEvent::Event(e) => println!("strap event: {}", e.name),
//!             WhoopEvent::Disconnected => break,
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Offloading history safely
//!
//! A strap stores its records in a ring buffer and hands them over in chunks,
//! each closed by an eight-byte token it wants echoed back. **A successful
//! acknowledgement can let the strap reclaim that history**, so the
//! acknowledgement must come after the chunk is durably stored — and must be
//! withheld if storing it failed.
//!
//! [`history`] enforces that with types rather than with a warning: the
//! acknowledgement bytes are only reachable through
//! [`history::PendingChunk::committed`], which consumes the chunk.
//!
//! ```no_run
//! use poop::history::{HistorySession, Step};
//! use poop::prelude::*;
//!
//! # async fn example(mut events: tokio::sync::mpsc::Receiver<WhoopEvent>, handle: WhoopHandle)
//! # -> anyhow::Result<()> {
//! let mut session = HistorySession::new(handle.family());
//! handle.begin_history().await?;
//!
//! while let Some(event) = events.recv().await {
//!     match session.feed(&event) {
//!         Step::Chunk(chunk) => {
//!             persist(&chunk.records)?;              // durable first …
//!             if let Some(ack) = chunk.committed() { // … then the token
//!                 handle.ack_history(&ack).await?;
//!             }
//!         }
//!         Step::Complete => break,
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! # fn persist(_: &[poop::types::Record]) -> anyhow::Result<()> { Ok(()) }
//! ```
//!
//! ## What a success is not
//!
//! Worth knowing before building on any of this, because the protocol is
//! unusually quick to look like it is working:
//!
//! - A **write acknowledgement** says the radio took the bytes.
//! - A **result of 1** says the strap accepted the request at that command's
//!   response boundary — not that asynchronous work finished, that a sensor
//!   started, or that anything was stored.
//! - **`HISTORY_COMPLETE`** says the attempt is over, not that every record
//!   arrived; it can be emitted with a backlog remaining.
//! - **Reconnecting** restores no sensor session, and a disconnect is not an
//!   acknowledgement that one was torn down. Re-request collection explicitly.
//! - An **unresolved** field means unresolved, not absent. Unknown record
//!   layouts are preserved whole rather than forced through a decoder written
//!   for a different record.
//!
//! ## Safety gate
//!
//! Three decisions, checked before a frame is built, by
//! [`protocol::admits_send`]:
//!
//! 1. Commands that can discard unoffloaded history, write firmware or enter
//!    the bootloader are refused outright — [`protocol::is_destructive`] lists
//!    them, and no policy admits them.
//! 2. The MG ECG family needs WHOOP 5 hardware **and**
//!    [`protocol::SendPolicy::ecg`], because those commands change acquisition
//!    state and repeated starts can lose a companion-collection record.
//! 3. Configuration writes need [`protocol::SendPolicy::config_write`] —
//!    [`protocol::CONFIG_WRITE_COMMANDS`]. These persist, and command 148
//!    disables wear detection outright.
//!
//! [`protocol::RebootProbe`] is reachable, because measuring which restart
//! candidate a strap honours is a real question — but every variant has to be
//! named by a caller, and it restarts the strap.
//!
//! ## Working without hardware
//!
//! [`sim`] is a simulated strap. It does **not** push events into a channel —
//! it builds complete frames with correct checksums and runs them through
//! [`whoop_client::decode_frame`], the same function the live dispatch loop
//! calls. So framing, verification, layout selection and every unit conversion
//! are the real thing, and a decoder bug shows up in simulation.
//!
//! It is also stricter than reality in one place: the simulated offload refuses
//! an acknowledgement that does not echo its token byte for byte, so a client
//! that rebuilt the token from its decoded trim cursor gets the chunk resent
//! rather than advanced.
//!
//! [`strap::Strap`] presents a live handle and a simulated one through one
//! type, which is what lets `whoop-rs stream --simulate` and `tui --simulate`
//! be the same code path as the real thing.
//!
//! ```no_run
//! use poop::prelude::*;
//! use poop::sim;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let (mut events, strap) = sim::connect(sim::SimConfig::default());
//! println!("battery: {:.0}%", strap.battery().await?);
//! # let _ = events.recv().await;
//! # Ok(()) }
//! ```
//!
//! ## Module overview
//!
//! | Module | Purpose |
//! |---|---|
//! | [`prelude`] | One-line glob import of the commonly needed types |
//! | [`whoop_client`] | Scanning, connecting, dispatch, the command API |
//! | [`history`] | The offload state machine and its durability rule |
//! | [`sim`] | A simulated strap that drives the real decoders |
//! | [`strap`] | One handle over a live strap or a simulated one |
//! | [`store`] | An NDJSON sink that `fsync`s before you acknowledge |
//! | [`types`] | Every event and record the client produces |
//! | [`protocol`] | UUIDs, checksums, framing, commands, payload builders |
//! | [`parse`] | Pure byte-to-record decoders for every frame |
//!
//! ## Scope
//!
//! Independent, and unaffiliated with WHOOP. Not a medical device, and nothing
//! it produces is a clinical measurement. The wire contracts implemented here
//! are recorded as factual observations for interoperability; no proprietary
//! code is reproduced. See [`DISCLAIMER.md`] in the repository.
//!
//! [`DISCLAIMER.md`]: https://github.com/eugenehp/whoop-rs/blob/main/DISCLAIMER.md

// A published crate documents its whole public surface: an undocumented item on
// a protocol type is exactly where a reader needs to know what is established
// and what is merely structural.
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod history;
pub mod parse;
pub mod protocol;
pub mod sim;
pub mod store;
pub mod strap;
pub mod types;
pub mod whoop_client;

// Spawning and timers differ between a native runtime and a browser event loop.
// Internal: which one is in use is not something a caller chooses.
mod platform;

// ── Prelude ───────────────────────────────────────────────────────────────────

/// Convenience re-exports for downstream crates.
///
/// A single glob import covers scanning, connecting and processing events:
///
/// ```no_run
/// use poop::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// let straps = WhoopClient::new(WhoopClientConfig::default()).scan_all().await?;
/// let (mut events, handle) = WhoopClient::new(WhoopClientConfig::default())
///     .connect_to(straps.into_iter().next().unwrap())
///     .await?;
///
/// while let Some(event) = events.recv().await {
///     if let WhoopEvent::HeartRate(hr) = event {
///         println!("{} bpm", hr.bpm);
///     }
/// }
/// # drop(handle);
/// # Ok(())
/// # }
/// ```
pub mod prelude {
    // ── Client ────────────────────────────────────────────────────────────────
    pub use crate::whoop_client::{WhoopClient, WhoopClientConfig, WhoopDevice, WhoopHandle};

    // ── Events and data types ─────────────────────────────────────────────────
    pub use crate::types::{
        Battery, BatterySource, BiometricRecord, CommandResponse, DataRange, HeartRate, Hello,
        HistoryMeta, HrSource, ImuRecord, Record, StrapEvent, WhoopEvent,
    };

    // ── History ───────────────────────────────────────────────────────────────
    // The session and its chunk, because the durability rule is not optional
    // and a caller cannot follow it without these two.
    pub use crate::history::{AckPayload, HistorySession, PendingChunk, Step};

    // ── Simulation ────────────────────────────────────────────────────────────
    pub use crate::strap::Strap;

    // ── Protocol ──────────────────────────────────────────────────────────────
    // `DeviceFamily` is in the prelude because nearly every protocol call takes
    // one: it is the selector, not a detail. `SendPolicy` is here because a
    // caller cannot write configuration or drive ECG without naming one.
    pub use crate::protocol::{
        Command, CommandResult, DeviceFamily, MetadataType, SendPolicy, Whoop5Variant,
    };
}
