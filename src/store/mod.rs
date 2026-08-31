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
pub mod layout;
/// The canonical per-instance directory layout under `OPENCOMPANY_DATA_DIR`
/// (`companies/`, `memory/`, `store/`, `files/`, `logs/`, `tmp/`) and the
/// startup lifecycle that creates them and, by default, clears `tmp/`.
pub mod lock;
/// The one-shot boot migration off the legacy doubled home layout
/// (`companies/companies/<slug>`), run by `serve`, `export`, and `import`
/// against the resolved home before anything reads it.
pub mod migrate;
pub mod paths;

/// Config-driven backend selection: maps `OPENCOMPANY_STORAGE` (fs | sqlite |
/// mongodb) onto opened port implementations, injected once per process into
/// every company's `RuntimeBuilder`. `OPENCOMPANY_MEMORY` can select a hosted
/// provider overlay for the memory, context, and facts ports.
pub mod select;

/// Char-boundary-safe slicing shared by the context backends' ranged `peek`
/// and search-snippet windows, so a byte offset landing mid-codepoint widens
/// to the boundary instead of panicking the slice.
pub(crate) mod text;

/// The shared lexical ranker behind `ContextStore::search`.
///
/// Stood four times over in `mongodb.rs`, `fs.rs`, `sqlite.rs` and
/// `tinycortex.rs`; three of those four copies carried the same two defects (a
/// substring test scored 1.0, and truncation to `limit` before any sorting).
/// One module, so the backends cannot drift apart again.
pub mod lexical;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// MongoDB-backed implementations of all five storage ports over the official
/// async driver — the multi-tenant platform backend: every document is keyed
/// on `company_id`, the hosting layer points each tenant at its own database
/// on a shared cluster, and an `owners` collection makes the company → tenant
/// map durable for shared-database platform mode. Only links under `mongodb`.
#[cfg(feature = "mongodb")]
pub mod mongodb;

/// The TinyMemory `MemoryProvider` seam (issue #914): one engine-neutral driver
/// contract behind the three memory ports, with the provider chosen by
/// configuration — a hosted service behind a URL and a credential, or nothing.
/// [`memory::BoundMemory`] is the only public way to
/// get a port out of a provider, and it derives the namespace from the
/// `CompanyId`, which is what keeps the tenant-isolation invariant the ports'
/// `&CompanyId` argument gives us and the contract's bare `namespace: &str` does
/// not. Only links under `tinymemory`.
#[cfg(feature = "tinymemory")]
pub mod memory;

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
    FsCompanyStore, FsContextStore, FsEventLog, FsInboxStore, FsJournalStore, FsMemoryStore,
    FsSecretStore,
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
    MemoryBackend, MemoryOverlay, MemoryScopes, MemorySelection, StorageHandles, StorageKind,
    StorageSettings, open_memory_overlay, open_storage, plaintext_secret_refusal,
    refuse_bundle_env,
};

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

#[cfg(feature = "mongodb")]
pub use mongodb::MongoStore;

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
