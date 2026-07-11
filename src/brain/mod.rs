//! Brain implementations: concrete [`Brain`](crate::ports::Brain) cognition
//! seams.
//!
//! Phase 1 ships the offline [`EchoBrain`], which has no TinyAgents dependency
//! and is the default. Phase 2 adds the [`HostedMedullaBrain`], which drives
//! hosted Medulla cognition over a [`MedullaTransport`](medulla::MedullaTransport);
//! the brain and its whole test surface live in the default build against the
//! in-memory mock transport. The [`sidecar`] brain, behind the `sidecar`
//! feature, drives a local sidecar process over the same wire frames but routes
//! each model pass back into the Rust host through an
//! [`InferenceClient`](sidecar::InferenceClient); it too is fully offline-testable
//! against its mock transport and mock inference client.

pub mod echo;
pub mod hosted;
pub mod medulla;
#[cfg(feature = "sidecar")]
pub mod sidecar;

pub use echo::EchoBrain;
pub use hosted::HostedMedullaBrain;
#[cfg(feature = "sidecar")]
pub use sidecar::{InferenceClient, SidecarBrain, SidecarTransport};
