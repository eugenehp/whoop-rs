//! One handle over a real strap or a simulated one.
//!
//! A front end that supports `--simulate` needs to call the same half-dozen
//! methods on whichever it got. Rust's `async fn` in traits is not
//! dyn-compatible, so the choice is boxed futures or an enum — and an enum of
//! two known variants costs nothing, needs no allocation, and keeps every
//! method's signature readable.
//!
//! The method set is deliberately **small**: exactly what the CLI and the TUI
//! use. Everything else — configuration, alarms, the probes — is live-only, and
//! reachable through [`crate::whoop_client::WhoopHandle`] directly. Simulating
//! a feature-flag namespace would mean inventing strap behaviour nobody has
//! observed, which is the opposite of what the simulator is for.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::history::AckPayload;
use crate::protocol::{DeviceFamily, Whoop5Variant};
use crate::sim::SimHandle;
use crate::whoop_client::WhoopHandle;

/// A live connection or a simulated one.
#[derive(Clone)]
pub enum Strap {
    /// A real strap over BLE.
    Live(Arc<WhoopHandle>),
    /// A simulated strap — see [`crate::sim`].
    Sim(Arc<SimHandle>),
}

impl std::fmt::Debug for Strap {
    /// Which kind, which family, and whether it is a simulation — the three
    /// things worth seeing in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Strap")
            .field("simulated", &self.is_simulated())
            .field("family", &self.family())
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl Strap {
    /// A live strap.
    pub fn live(handle: WhoopHandle) -> Self {
        Self::Live(Arc::new(handle))
    }

    /// A simulated strap.
    pub fn sim(handle: SimHandle) -> Self {
        Self::Sim(Arc::new(handle))
    }

    /// Whether this is a simulation.
    ///
    /// Worth surfacing, not hiding: a reading from a simulated strap is not a
    /// measurement, and a UI that presented the two identically would be
    /// inviting someone to screenshot invented data.
    pub fn is_simulated(&self) -> bool {
        matches!(self, Self::Sim(_))
    }

    /// Which wire profile this strap speaks.
    pub fn family(&self) -> DeviceFamily {
        match self {
            Self::Live(h) => h.family(),
            Self::Sim(h) => h.family(),
        }
    }

    /// Which 5-family hardware this is, where it could be resolved.
    pub fn variant(&self) -> Whoop5Variant {
        match self {
            Self::Live(h) => h.variant(),
            Self::Sim(h) => h.variant(),
        }
    }

    /// Read the battery percentage.
    pub async fn battery(&self) -> Result<f32> {
        match self {
            Self::Live(h) => h.battery().await,
            Self::Sim(h) => h.battery().await,
        }
    }

    /// Begin the historical offload.
    pub async fn begin_history(&self) -> Result<()> {
        match self {
            Self::Live(h) => h.begin_history().await,
            Self::Sim(h) => h.begin_history().await,
        }
    }

    /// Acknowledge one committed chunk.
    ///
    /// Takes an [`AckPayload`], which only [`crate::history::PendingChunk::committed`]
    /// produces — so the durability rule holds for a simulated offload too.
    /// The simulator checks the token byte for byte and resends the chunk if it
    /// does not match, which makes this the one method where simulating is
    /// stricter than reality rather than looser.
    pub async fn ack_history(&self, ack: &AckPayload) -> Result<()> {
        match self {
            Self::Live(h) => h.ack_history(ack).await,
            Self::Sim(h) => h.ack_history(ack).await,
        }
    }

    /// Ask the strap to stop sending history.
    pub async fn abort_history(&self) -> Result<()> {
        match self {
            Self::Live(h) => h.abort_history().await,
            Self::Sim(h) => h.abort_history().await,
        }
    }

    /// Start or stop the proprietary live heart-rate stream.
    pub async fn toggle_realtime_hr(&self, on: bool) -> Result<()> {
        match self {
            Self::Live(h) => h.toggle_realtime_hr(on).await,
            Self::Sim(h) => h.toggle_realtime_hr(on).await,
        }
    }

    /// Buzz the strap.
    pub async fn buzz(&self, loops: u8) -> Result<()> {
        match self {
            Self::Live(h) => h.buzz(loops).await,
            Self::Sim(h) => h.buzz(loops).await,
        }
    }

    /// The strap's stored record range — live only.
    ///
    /// An error on a simulation rather than an invented range: a backlog figure
    /// is the kind of number that gets quoted, and there is nothing behind a
    /// simulated one.
    pub async fn data_range(&self) -> Result<crate::types::DataRange> {
        match self {
            Self::Live(h) => h.data_range().await,
            Self::Sim(_) => Err(anyhow!("a simulated strap has no ring buffer to report")),
        }
    }

    /// Whether the link is still up.
    pub fn is_connected(&self) -> bool {
        match self {
            Self::Live(h) => h.is_connected(),
            Self::Sim(h) => h.is_connected(),
        }
    }

    /// Disconnect.
    pub async fn disconnect(&self) -> Result<()> {
        match self {
            Self::Live(h) => h.disconnect().await,
            Self::Sim(h) => h.disconnect().await,
        }
    }
}
