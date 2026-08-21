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

/// The TinyMemory `MemoryProvider` seam (issue #914): one engine-neutral driver
/// contract behind the three memory ports, with the engine chosen by
/// configuration — the embedded engine in-pod, a hosted service behind a URL and
/// a credential, or nothing. [`memory::BoundMemory`] is the only public way to
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
    MemoryBackend, MemoryOverlay, StorageHandles, StorageKind, StorageSettings,
    open_memory_overlay, open_storage, plaintext_secret_refusal, refuse_bundle_env,
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
use std::ops::Range;

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

/// Slices `body` on `range`, clamped to its length and widened outward to the
/// nearest char boundaries.
///
/// A byte range that lands mid-character panics a naive `body[range]`. Every
/// [`ContextStore`](crate::ports::ContextStore) backend derives such ranges from
/// byte offsets — ranged `peek` from a caller's own byte count, and `search`
/// from `str::find` positions widened by a fixed ±24-byte window — so a
/// multibyte char within the window is reachable with any non-ASCII body, and is
/// agent-reachable through `memory_recall` (see
/// `harness::built_in::memory_tools`). Widening to the nearest boundary returns
/// slightly more than asked rather than panicking the read.
///
/// A start at or past the (clamped) end yields the empty string, so a reversed
/// or out-of-range request is an empty slice, not a panic.
pub(crate) fn slice_on_char_boundaries(body: &str, range: Range<usize>) -> String {
    let start = floor_char_boundary(body, range.start.min(body.len()));
    let end = ceil_char_boundary(body, range.end.min(body.len()));
    if start >= end {
        return String::new();
    }
    body[start..end].to_string()
}

/// The largest char boundary at or below `at` (which must be `<= s.len()`).
fn floor_char_boundary(s: &str, mut at: usize) -> usize {
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest char boundary at or above `at` (which must be `<= s.len()`).
fn ceil_char_boundary(s: &str, mut at: usize) -> usize {
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Bytes of context kept on each side of a search match in a snippet.
const SNIPPET_CONTEXT_BYTES: usize = 24;

/// Builds a search-hit snippet: `query` found at byte `pos` in `body`, plus up
/// to [`SNIPPET_CONTEXT_BYTES`] of surrounding context on each side, sliced on
/// char boundaries so a multibyte char inside the window can't panic the read.
///
/// Shared by every [`ContextStore`](crate::ports::ContextStore) backend's
/// `search` so the fs, sqlite, and mongodb snippets are byte-for-byte identical
/// and share the one boundary-safe path.
pub(crate) fn search_snippet(body: &str, pos: usize, query: &str) -> String {
    let start = pos.saturating_sub(SNIPPET_CONTEXT_BYTES);
    let end = pos + query.len() + SNIPPET_CONTEXT_BYTES;
    slice_on_char_boundaries(body, start..end)
}

#[cfg(test)]
mod slice_tests {
    use super::{search_snippet, slice_on_char_boundaries};

    #[test]
    fn ascii_range_is_returned_verbatim() {
        assert_eq!(slice_on_char_boundaries("hello world", 0..5), "hello");
        assert_eq!(slice_on_char_boundaries("hello world", 6..11), "world");
    }

    #[test]
    fn a_mid_codepoint_bound_widens_instead_of_panicking() {
        // "é" is two bytes (0xC3 0xA9). A range that starts or ends inside it
        // would panic a naive slice; both bounds widen outward to keep the whole
        // char rather than aborting the read.
        let body = "aé b";
        assert_eq!(slice_on_char_boundaries(body, 1..2), "é");
        assert_eq!(slice_on_char_boundaries(body, 0..2), "aé");
        // A range wholly inside the multibyte char still yields that char.
        assert_eq!(slice_on_char_boundaries(body, 2..3), "é");
    }

    #[test]
    fn out_of_range_and_reversed_bounds_are_empty() {
        assert_eq!(slice_on_char_boundaries("abc", 0..999), "abc");
        // A reversed range (start past end) is empty, not a panic. Built from
        // variables so it isn't a literal empty-range clippy flags at compile time.
        let (start, end) = (3usize, 1usize);
        assert_eq!(slice_on_char_boundaries("abc", start..end), "");
        assert_eq!(slice_on_char_boundaries("abc", 10..20), "");
    }

    #[test]
    fn a_snippet_next_to_a_multibyte_char_does_not_panic() {
        // The match sits one byte after a 2-byte "é", well within the 24-byte
        // context window, so the naive `body[pos-24..]` used to land mid-"é" and
        // panic. The snippet must come back whole, containing the match.
        let body = "café menu today";
        let pos = body.find("menu").expect("query present");
        let snippet = search_snippet(body, pos, "menu");
        assert!(
            snippet.contains("menu"),
            "snippet keeps the match: {snippet}"
        );
        assert!(snippet.contains("café"), "leading context is char-safe");
    }

    #[test]
    fn a_snippet_bounded_by_a_trailing_multibyte_char_does_not_panic() {
        // The 24-byte trailing window ends inside a multibyte char; widening
        // keeps the whole char rather than aborting.
        let body = "find the café";
        let pos = body.find("find").expect("query present");
        let snippet = search_snippet(body, pos, "find");
        assert!(
            snippet.ends_with("café"),
            "trailing char kept whole: {snippet}"
        );
    }
}
