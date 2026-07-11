//! Filesystem-backed persistence for the runtime's durable ports.
//!
//! Each company owns a [`Bundle`] directory (see [`paths`]) holding its
//! manifest, event log, ledger, memory, context, and secrets. The [`fs`]
//! module implements [`CompanyStore`](crate::ports::CompanyStore),
//! [`EventLog`](crate::ports::EventLog),
//! [`MemoryStore`](crate::ports::MemoryStore),
//! [`ContextStore`](crate::ports::ContextStore), and
//! [`SecretStore`](crate::ports::SecretStore) over that layout.

/// Store-agnostic bundle export and import: read everything through the four
/// durable ports and write the canonical fs [`Bundle`](paths::Bundle) layout
/// (and the inverse). The dep-free core operates on an unpacked bundle
/// directory; a single-file `.tar` wrapper is gated behind the `export` feature.
pub mod export;
pub mod fs;
pub mod paths;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// A backend-agnostic port-conformance suite: async assertions parameterized
/// over any [`CompanyStore`](crate::ports::CompanyStore) /
/// [`EventLog`](crate::ports::EventLog) /
/// [`MemoryStore`](crate::ports::MemoryStore) /
/// [`ContextStore`](crate::ports::ContextStore) implementation. Both the fs and
/// sqlite backends run the identical suite, so a new store proves it upholds the
/// port contract (per-company isolation, append-only logs, monotonic seqs,
/// export totality) rather than re-testing each backend by hand. Test-only.
#[cfg(test)]
pub mod conformance;

pub use fs::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore, FsSecretStore};
pub use paths::{Bundle, default_home};

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

use std::hash::{DefaultHasher, Hash, Hasher};

/// Computes the content address of a context-chunk body.
///
/// Shared by every [`ContextStore`](crate::ports::ContextStore) backend so the
/// fs and sqlite stores mint identical addresses for identical bodies. Phase 1
/// uses a non-cryptographic [`DefaultHasher`]; a real content hash (sha-256) is
/// a documented follow-up.
pub(crate) fn content_address(body: &str) -> String {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
