//! Entry point for the `poop` command-line client.
//!
//! The body of it lives in `cli.rs`. That is a module here, rather than being
//! this file, so that a single `cfg` takes all of it out at once on
//! `wasm32-unknown-unknown`: this binary parses argv, writes files and drives
//! tokio's multi-threaded runtime, none of which a browser page has. Rather
//! than fail the build for a target the binary was never meant for, it compiles
//! to an empty `main` there.
//!
//! The library is the part a browser wants, and that does build for wasm — see
//! `src/platform.rs` for how spawning and timers get there, and
//! `examples/web/` for the page.

#[cfg(not(target_arch = "wasm32"))]
#[path = "cli.rs"]
mod cli;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    cli::run()
}

/// A page has no argv to read and no file to write.
#[cfg(target_arch = "wasm32")]
fn main() {}
