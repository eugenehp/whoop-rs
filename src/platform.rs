//! Spawning and timers, chosen per target.
//!
//! The client asks a runtime for three things: run a task in the background,
//! sleep, and put a deadline on a future. Natively all three are tokio's.
//!
//! In a browser none of them are. Tokio's timer is not implemented for
//! `wasm32-unknown-unknown` — tokio's own suite marks `Instant::now()`, and even
//! *building* a current-thread runtime with the `time` feature enabled,
//! `#[should_panic]` there — and its multi-threaded scheduler has no threads to
//! run on. Enabling tokio's `time` for wasm would therefore produce a library
//! that links and then panics on the first runtime it constructs.
//!
//! What a page does have is an event loop, and `webbluetooth-wasm` already
//! speaks to it. Its `exec::spawn` polls a task whenever something wakes it, and
//! the shim's `SET_TIMEOUT` operation is `setTimeout` reached over the same
//! request/settle path as every BLE call. Both are already linked into a wasm
//! build of this crate, so routing onto them costs no new dependency.
//!
//! The two implementations present the same three functions, so the call sites
//! in [`crate::whoop_client`] read identically on either target. That matters
//! more here than it does for a sensor stream: a bounded wait is how every
//! command request ends, and a timer that silently never fires would turn a
//! request into a permanent hang.

use std::future::Future;
// `Duration` is arithmetic over a pair of integers and is available everywhere.
// `Instant` is the part that needs a clock, and the part wasm does not have.
use std::time::Duration;

/// A deadline passed before the future produced a value.
///
/// Its own type rather than `tokio::time::error::Elapsed`, so that a wasm build
/// need not enable tokio's `time` feature — the feature whose mere presence
/// panics there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

// ── Native ────────────────────────────────────────────────────────────────────

/// Run `task` in the background, discarding its handle.
///
/// Nothing here joins a spawned task: each one owns a channel sender and ends
/// when its stream closes, which is also how the receiver learns it ended.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(task: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(task);
}

// On wasm this is what `timeout` is built out of, so it always has a caller
// there. Natively tokio's own `timeout` carries its deadline, which leaves this
// half of the pair with no in-library caller — kept anyway, because a shim whose
// two targets offer different functions is a shim that stops being substitutable
// the first time a call site needs the missing one.
#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| Elapsed)
}

// ── Browser ───────────────────────────────────────────────────────────────────

/// Run `task` on the page's event loop.
///
/// No `Send` bound, and none to add: a page has one thread, and the Web
/// Bluetooth objects these tasks hold are not `Send` to begin with.
#[cfg(target_arch = "wasm32")]
pub fn spawn(task: impl Future<Output = ()> + 'static) {
    webbluetooth_wasm::exec::spawn(task);
}

#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    use webbluetooth_wasm::codec::Writer;
    use webbluetooth_wasm::host::{call, Op};

    // The shim reads one `u32` of milliseconds, then settles the call from a
    // `setTimeout`. Saturating rather than truncating: a wrapped delay is a
    // sleep that returns immediately, and as the gap between two startup
    // commands that is a write storm against the strap.
    let ms = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
    let mut message = Writer::new();
    message.u32(ms);

    // An error here means the page never called back. There is no recovery finer
    // than carrying on: the caller asked to wait, not to hear about the host.
    let _ = call(Op::SetTimeout, &message.finish()).await;
}

#[cfg(target_arch = "wasm32")]
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    use webbluetooth::future::{select, Either};

    // `select` drops the loser, which for the timer side means `Pending`'s
    // `Drop` deregisters the token. A `setTimeout` that has already been
    // scheduled still fires, and settles a token nobody is waiting on.
    let future = std::pin::pin!(future);
    let deadline = std::pin::pin!(sleep(duration));

    match select(future, deadline).await {
        Either::Left((value, _)) => Ok(value),
        Either::Right(((), _)) => Err(Elapsed),
    }
}
