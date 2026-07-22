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
/// Filesystem backends for the WS3 console ports (tasks, facts, usage,
/// skill-state, workspace tree) over the same [`Bundle`](paths::Bundle) layout.
pub mod fs_ops;
/// The canonical per-instance directory layout under `OPENCOMPANY_DATA_DIR`
/// (`companies/`, `memory/`, `store/`, `files/`, `logs/`, `tmp/`) and the
/// startup lifecycle that creates them and clears `tmp/`.
pub mod layout;
pub mod paths;

/// Config-driven backend selection: maps `OPENCOMPANY_STORAGE` (fs | sqlite |
/// mongodb) onto opened port implementations, injected once per process into
/// every company's `RuntimeBuilder`.
pub mod select;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// MongoDB-backed implementations of all five storage ports over the official
/// async driver — the multi-tenant platform backend: every document is keyed
/// on `company_id`, the hosting layer points each tenant at its own database
/// on a shared cluster, and an `owners` collection makes the company → tenant
/// map durable for shared-database platform mode. Only links under `mongodb`.
#[cfg(feature = "mongodb")]
pub mod mongodb;

/// TinyCortex-backed memory and context ports over a mockable client. TinyCortex
/// is not checked out here, so the compiled backend is an offline in-memory
/// client; a real HTTP client (inert until the service is reachable through the
/// OpenHuman seam) is present behind the same feature. Only links under
/// `tinycortex`.
#[cfg(feature = "tinycortex")]
pub mod tinycortex;

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

pub use fs::{
    FsCompanyStore, FsContextStore, FsEventLog, FsInboxStore, FsMemoryStore, FsSecretStore,
};
pub use fs_ops::FsOps;
pub use layout::DataLayout;
pub use paths::{Bundle, default_home};
pub use select::{StorageHandles, StorageKind, StorageSettings, open_storage};

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

#[cfg(feature = "mongodb")]
pub use mongodb::MongoStore;

#[cfg(feature = "tinycortex")]
pub use tinycortex::{
    CortexClient, CortexContextStore, CortexMemoryStore, HttpCortexClient, InMemoryCortex,
};

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
