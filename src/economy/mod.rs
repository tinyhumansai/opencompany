//! The tiny.place economy: identity, Agent Cards, SIWX auth, and x402 payments.
//!
//! This module is split so the default build carries everything that needs no
//! crypto or network, and the `tinyplace` feature layers on the Ed25519
//! identity and payment-authorization surface:
//!
//! - [`card`] — deterministic [`AgentCard`](crate::ports::types::AgentCard)
//!   projection from a Company Charter. **Default build**; no crypto.
//! - [`signer`] — the Ed25519 [`LocalSigner`](signer::LocalSigner) whose seed
//!   persists in the company bundle. **`tinyplace` feature.**
//! - [`siwx`] — per-action `Authorization: tiny.place …` signing and
//!   verification with skew + replay protection. **`tinyplace` feature.**
//! - [`x402`] — payment-challenge parsing and authorization signing.
//!   **`tinyplace` feature.**
//! - [`client`] — the [`TinyplaceClient`](client::TinyplaceClient) REST seam,
//!   its network-free [`MockTinyplaceClient`](client::MockTinyplaceClient), and
//!   the reqwest-backed [`HttpTinyplaceClient`](client::HttpTinyplaceClient).
//!   **`tinyplace` feature.**
//! - [`outbox`] — an in-memory queue of outbound actions deferred while
//!   tiny.place is unreachable. **`tinyplace` feature.**
//! - [`adapter`] — [`TinyplaceEconomy`](adapter::TinyplaceEconomy), the
//!   [`AgentEconomy`](crate::ports::AgentEconomy) implementation over a
//!   [`TinyplaceClient`](client::TinyplaceClient). Budget-fail-closed, journals
//!   every payment to the ledger, and never blocks boot when offline.
//!   **`tinyplace` feature.**
//!
//! The whole economy adapter is feature-gated because it transitively depends on
//! the [`signer`]/[`siwx`]/[`x402`] crypto primitives. Offline testability comes
//! from the network-free [`MockTinyplaceClient`](client::MockTinyplaceClient),
//! not from a default-build mock: the default build links none of this.

pub mod card;

#[cfg(feature = "tinyplace")]
pub mod adapter;
#[cfg(feature = "tinyplace")]
pub mod client;
#[cfg(feature = "tinyplace")]
pub mod outbox;
#[cfg(feature = "tinyplace")]
pub mod signer;
#[cfg(feature = "tinyplace")]
pub mod siwx;
#[cfg(feature = "tinyplace")]
pub mod x402;

pub use card::{build_agent_card, render_skill_md};

#[cfg(feature = "tinyplace")]
pub use adapter::TinyplaceEconomy;
#[cfg(feature = "tinyplace")]
pub use client::{HttpTinyplaceClient, MockTinyplaceClient, TinyplaceClient};
#[cfg(feature = "tinyplace")]
pub use outbox::{Outbox, OutboxAction};
#[cfg(feature = "tinyplace")]
pub use signer::{LocalSigner, load_or_create_signer};
#[cfg(feature = "tinyplace")]
pub use siwx::{NonceCache, SiwxHeader, SiwxPayload};
#[cfg(feature = "tinyplace")]
pub use x402::{X402Authorization, X402Challenge};
