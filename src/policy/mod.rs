//! Approval policy: the manifest-`[policy]`-driven [`ApprovalGate`].
//!
//! The [`gate`] module implements the default
//! [`ApprovalGate`](crate::ports::ApprovalGate) that evaluates emitted effects
//! against a company's declared policy and holds the in-memory approval queue.

pub mod gate;

pub use gate::{DEFAULT_TTL_MILLIS, ManifestApprovalGate};
