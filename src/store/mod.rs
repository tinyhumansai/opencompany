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
/// startup lifecycle that creates them and, by default, clears `tmp/`.
pub mod layout;
/// The one-shot boot migration off the legacy doubled home layout
/// (`companies/companies/<slug>`), run by `serve`, `export`, and `import`
/// against the resolved home before anything reads it.
pub mod migrate;
pub mod paths;

/// Config-driven backend selection: maps `OPENCOMPANY_STORAGE` (fs | sqlite |
/// mongodb) onto opened port implementations, injected once per process into
/// every company's `RuntimeBuilder`. `OPENCOMPANY_MEMORY` (store | tinycortex)
/// selects an optional overlay that swaps just the memory + context ports onto
/// a dedicated engine on top of that base.
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

/// TinyCortex-backed memory and context ports over a mockable client. The
/// company-scoped [`CortexClient`](tinycortex::CortexClient) seam plus the
/// offline [`InMemoryCortex`](tinycortex::InMemoryCortex) test/fallback backend;
/// the real persistent engine lives in [`tinycortex_engine`]. Only links under
/// `tinycortex`.
#[cfg(feature = "tinycortex")]
pub mod tinycortex;

/// The in-pod, persistent TinyCortex memory engine
/// ([`EngineCortex`](tinycortex_engine::EngineCortex)): a real engine-backed
/// [`CortexClient`](tinycortex::CortexClient) over the vendored `tinycortex`
/// crate, keeping each company's traces, task results, and context chunks in a
/// durable per-company SQLite workspace. Ships in degraded lexical/recency recall
/// mode (no embedding compute — that lands in 188c2). Only links under
/// `tinycortex`.
#[cfg(feature = "tinycortex")]
pub mod tinycortex_engine;

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
// Only the boot entry point is re-exported here. The migration is a one-shot
// step the binary runs before it reads anything, and its silent core and result
// types are its own business — reachable at `store::migrate::*` for anyone
// reading the rules, not part of the store's own surface.
pub use migrate::migrate_legacy_nest_announced;
pub use paths::{Bundle, DATA_DIR_ENV, home_divergence_warning, resolve_home};
pub use select::{
    MemoryBackend, MemoryOverlay, StorageHandles, StorageKind, StorageSettings,
    open_memory_overlay, open_storage,
};

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

#[cfg(feature = "mongodb")]
pub use mongodb::MongoStore;

#[cfg(feature = "tinycortex")]
pub use tinycortex::{CortexClient, CortexContextStore, CortexMemoryStore, InMemoryCortex};

#[cfg(feature = "tinycortex")]
pub use tinycortex_engine::EngineCortex;

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
