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
//!
//! Later batches add the `TinyplaceClient` transport, the `TinyplaceEconomy`
//! adapter, and the A2A inbound routes on top of these primitives.

pub mod card;

#[cfg(feature = "tinyplace")]
pub mod signer;
#[cfg(feature = "tinyplace")]
pub mod siwx;
#[cfg(feature = "tinyplace")]
pub mod x402;

pub use card::{build_agent_card, render_skill_md};

#[cfg(feature = "tinyplace")]
pub use signer::{LocalSigner, load_or_create_signer};
#[cfg(feature = "tinyplace")]
pub use siwx::{NonceCache, SiwxHeader, SiwxPayload};
#[cfg(feature = "tinyplace")]
pub use x402::{X402Authorization, X402Challenge};
