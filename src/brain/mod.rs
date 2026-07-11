//! Brain implementations: concrete [`Brain`](crate::ports::Brain) cognition
//! seams.
//!
//! Phase 1 ships the offline [`EchoBrain`], which has no TinyAgents dependency
//! and is the default. Phase 2 adds the [`HostedMedullaBrain`], which drives
//! hosted Medulla cognition over a [`MedullaTransport`](medulla::MedullaTransport);
//! the brain and its whole test surface live in the default build against the
//! in-memory mock transport. TinyAgents-backed and sidecar brains land behind
//! the `tiny` feature and in later phases.

pub mod echo;
pub mod hosted;
pub mod medulla;

pub use echo::EchoBrain;
pub use hosted::HostedMedullaBrain;
