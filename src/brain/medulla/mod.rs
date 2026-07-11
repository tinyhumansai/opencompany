//! The hosted Medulla brain and its `/orchestration/v1` wire contract.
//!
//! Medulla is TinyHumans' orchestrator-first cognitive model, consumed by
//! OpenCompany as a hosted service mounted at `/orchestration/v1/*` on
//! `api.tinyhumans.ai`. This module holds, entirely in the default build:
//!
//! - [`wire`]: the typed serde surface — envelope, error codes, HTTP request
//!   and response bodies, read-surface views, and Socket.IO frames.
//! - [`transport`]: the [`MedullaTransport`](transport::MedullaTransport) seam
//!   abstracting the HTTP posts, the effect stream, and device-tool round-trips
//!   so the brain never depends on a concrete network client.
//! - [`mock`]: an in-memory [`MockTransport`](mock::MockTransport) that drives
//!   the seam offline, for tests.
//!
//! The networked `HttpSocketTransport` lands in a later batch behind an
//! optional feature; nothing here pulls a network dependency.

#[cfg(feature = "medulla")]
pub mod http;
pub mod mock;
pub mod transport;
pub mod wire;

#[cfg(feature = "medulla")]
pub use http::HttpSocketTransport;
pub use mock::MockTransport;
pub use transport::{InboundFrame, MedullaTransport};
