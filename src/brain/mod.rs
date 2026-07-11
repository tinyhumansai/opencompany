//! Brain implementations: concrete [`Brain`](crate::ports::Brain) cognition
//! seams.
//!
//! Phase 1 ships the offline [`EchoBrain`], which has no TinyAgents dependency
//! and is the default. TinyAgents-backed and hosted brains land behind the
//! `tiny` feature and in later phases.

pub mod echo;

pub use echo::EchoBrain;
