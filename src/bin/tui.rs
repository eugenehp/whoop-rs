//! Entry point for the `tui` binary.
//!
//! The body of it lives in `src/tui.rs`; see the note in `src/main.rs` for why
//! it is a module behind a `cfg` rather than being this file. A full-screen
//! terminal UI has even less to do in a browser than the CLI does, and
//! `crossterm` has no `wasm32-unknown-unknown` backend to build against — which
//! is why `Cargo.toml` gates it by target as well as by feature.

#[cfg(not(target_arch = "wasm32"))]
#[path = "../tui.rs"]
mod tui;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    tui::run()
}

/// A page has no terminal to take over.
#[cfg(target_arch = "wasm32")]
fn main() {}
