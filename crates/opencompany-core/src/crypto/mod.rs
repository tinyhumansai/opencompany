//! Offline cryptographic primitives shared across the crate.
//!
//! One module so that every caller checking an Ed25519 signature checks it the
//! same way: two base58 key parsers, or two curves, is how a signature comes to
//! verify for one caller and not another.
//!
//! - [`ed25519`] — base58 Ed25519 signature verification, used by wallet
//!   sign-in (`server::users::wallet`) and the runner handshake
//!   (`runner::attest`).

pub mod ed25519;
