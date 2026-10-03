//! The TinyMemory `MemoryEngine` seam (issue #914).
//!
//! One engine-neutral contract behind the three memory ports, with the engine
//! chosen by configuration: a hosted engine behind a URL and a credential
//! (CortexDB, or the TinyHumans memory wire), or nothing at all.
//!
//! # The decorator is the whole point
//!
//! [`BoundMemory`] is the **only** public way to obtain a memory port from an
//! engine. That is a deliberate constraint, not an ergonomic accident.
//!
//! The three ports take `&CompanyId` as an explicit first argument — a
//! compiler-enforced tenant-isolation invariant. `MemoryEngine` has no tenant
//! argument at all: TinyMemory v2 dropped namespaces, and the only scoping it
//! offers is metadata (`MemoryMeta::workspace`, `MetaFilter`). Handing the raw
//! engine to call sites would trade a guarantee the compiler checks for a
//! convention the reviewer checks, and a missing filter would be a silent
//! cross-tenant leak with nothing to catch it. With a hosted engine it is worse
//! still: that metadata is the only thing separating tenants inside somebody
//! else's database.
//!
//! So: [`Namespace`](namespace::Namespace) has no public constructor, every
//! port method takes `&CompanyId` and derives its namespace fresh from it
//! (`Namespace::company_root` is the only way to make one), every write stamps
//! that namespace onto the item's `workspace`, every read forces it into the
//! filter, and every hit that comes back is re-checked against it. There is no
//! `pub fn` in this module tree that accepts a namespace string or exposes the
//! engine. `BoundMemory::bind(engine)` itself takes no company, because one
//! bound engine serves every company this host runs.
//!
//! # What else the decorator owns
//!
//! The contract deliberately owns no policy, which leaves five duties here:
//!
//! - **Scratch firewall.** Provisional working-out lives in its own namespace
//!   and is unreachable from durable recall *by construction* — the durable
//!   facades scope every read to their own namespace and re-check every hit
//!   that comes back, so scratch cannot appear in a durable result even if an
//!   engine ignores the filter.
//! - **Archive on evict.** The contract has no archive tier, so eviction is a
//!   move between namespaces rather than a delete — and every eviction bounds
//!   the archive by the eviction policy's own `n` or, for a policy without
//!   one, by the retention limit, so the move cannot grow storage without
//!   bound either. See [`facades::ProviderMemoryStore::evict`].
//! - **Provenance.** Inbound-channel writes are marked external via
//!   [`BoundMemory::inbound_context`] (`SourceKind::Link` plus the
//!   [`facades::EXTERNAL_TAG`] tag). The v1 contract carried a taint enum; v2
//!   does not, so the mark is metadata this host stamps and owns.
//! - **Per-agent and per-desk scoping**, which neither cognition port has today.
//! - **Operator rights** — inspect, delete, redact, export — from
//!   `docs/spec/company-brain/memory.md`.
//!
//! ## Who may run `migrate`
//!
//! [`migrate`](migrate::migrate) is deliberately a **local CLI operation**
//! (`opencompany memory migrate`), not an HTTP surface: it never binds a
//! route, so the only principal who can reach it is whoever already runs the
//! binary on this host and supplies BOTH engines' credentials. That person
//! owns the data on both ends by definition — an in-app authorization layer
//! here would gate the operator against themselves. If a remote-triggered
//! migration surface is ever added, it must carry its own operator-auth and
//! per-tenant scoping; do not lift this function onto a route as-is.

pub mod driver;
pub mod facades;
pub mod migrate;
mod namespace;
pub mod null;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tinymemory::MemoryEngine;

use facades::{Bound, Provenance, ProviderContextStore, ProviderFactStore, ProviderMemoryStore};
use namespace::Scope;

use crate::Result;
use crate::ports::{CompanyId, ContextStore, FactStore, MemoryStore};

#[async_trait::async_trait]
impl crate::store::select::MemoryScopes for BoundMemory {
    fn agent_context(&self, agent_id: &str) -> Arc<dyn ContextStore> {
        Self::agent_context(self, agent_id)
    }

    fn desk_context(&self, desk_id: &str) -> Arc<dyn ContextStore> {
        Self::desk_context(self, desk_id)
    }

    async fn archived_traces(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::CompressedTrace>> {
        Self::archived_traces(self, company).await
    }

    async fn restore_archived_traces(
        &self,
        company: &CompanyId,
        traces: &[crate::ports::CompressedTrace],
    ) -> Result<()> {
        Self::restore_archived_traces(self, company, traces).await
    }
}

pub use driver::{MemoryDriverConfig, MemoryDriverError, MemoryMode, open_driver};

/// Process-wide cache of per-scope context stores, keyed by the scope label.
///
/// Populated lazily on first use of a scope ([`BoundMemory::scoped_context`]),
/// so one bound engine serves every company and every scope it is asked for
/// without rebuilding stores. Factored out of the struct field so
/// `clippy::type-complexity` stays satisfied — the shape is deliberately the
/// once-init + lock + map + arc composition.
type ContextStoreCache =
    Arc<OnceLock<std::sync::Mutex<HashMap<String, Arc<ProviderContextStore>>>>>;

/// Upper bound on distinct scope labels the per-scope context cache holds.
///
/// The labels are agent/desk ids — internal identifiers, so a legitimate
/// company has a handful — but nothing else stops an unbounded number of them
/// from accumulating for the life of the process: every distinct id ever used
/// inserts an entry that is never removed. The cache exists to make repeated
/// access to the *same* scope cheap, not to remember every scope forever, so
/// the bound is a plain capacity cap with arbitrary eviction. Eviction is safe
/// because entries are `Arc`-backed: an in-flight holder keeps its store, and
/// the next access to an evicted scope rebuilds it.
const SCOPED_CONTEXT_CACHE_CAPACITY: usize = 4096;

/// A bound memory engine, and the only way to get a memory port out of one.
///
/// Process-scoped, like the `MemoryOverlay` it is opened into: one engine serves
/// every company this host runs, and each port method derives its namespace from
/// the `&CompanyId` it is given. See `facades::Bound` for why the company is a
/// per-call argument rather than a field — briefly, a namespace fixed at
/// construction would be one tenant's namespace serving all of them.
///
/// Clone is cheap: the engine is shared.
#[derive(Clone)]
pub struct BoundMemory {
    engine: Arc<dyn MemoryEngine>,
    engine_id: String,
    context_stores: ContextStoreCache,
}

impl std::fmt::Debug for BoundMemory {
    /// Renders the engine identity only.
    ///
    /// Never anything from the engine's own configuration, which is where the
    /// endpoint and the credential live.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundMemory")
            .field("engine_id", &self.engine_id)
            .finish_non_exhaustive()
    }
}

impl BoundMemory {
    /// Binds `engine` as this host's memory engine.
    ///
    /// Offline by design: nothing is sent to the engine here. Reachability is
    /// the boot path's separate, bounded probe
    /// ([`MemoryOverlay::refresh_health`](crate::store::select::MemoryOverlay::refresh_health)).
    pub fn bind(engine: Arc<dyn MemoryEngine>) -> Self {
        Self {
            engine_id: engine.descriptor().id.to_string(),
            engine,
            context_stores: Arc::new(OnceLock::new()),
        }
    }

    /// The bound engine's own id (`cortexdb`, `tinyhumans`, `null`, …).
    ///
    /// Safe to surface to an operator — unlike the endpoint and the credential,
    /// which are not.
    pub fn engine_id(&self) -> &str {
        &self.engine_id
    }

    /// The retrieval modes the engine serves, as stable names for status
    /// output (`keyword`, `vector`, `hybrid`).
    ///
    /// The v2 contract's only negotiated surface: every engine serves the same
    /// five operations, and differs only in how `fetch` ranks.
    pub fn capability_names(&self) -> Vec<&'static str> {
        self.engine
            .descriptor()
            .fetch_modes
            .iter()
            .map(|mode| mode.as_str())
            .collect()
    }

    /// Builds a facade addressing one scope of a company's memory.
    fn bound(&self, scope: Scope, provenance: Provenance) -> Bound {
        Bound::new(self.engine.clone(), scope, provenance)
    }

    /// The operator's hand-curated facts.
    ///
    /// Operator-authored, so internal — this is the company writing about
    /// itself, not content arriving from outside.
    pub fn facts(&self) -> Arc<dyn FactStore> {
        Arc::new(ProviderFactStore::new(
            self.bound(Scope::Facts, Provenance::Internal),
        ))
    }

    /// The durable context store: the RLM environment the brain queries.
    pub fn context(&self) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Context, Provenance::Internal),
        ))
    }

    /// The durable context store, for writes arriving from an inbound channel.
    ///
    /// Identical to [`context`](Self::context) except that every write is
    /// marked external ([`Provenance::External`]). A company that reads the web needs
    /// this: content that arrived from outside must stay marked as such, because
    /// laundering it into internal-trust content is what lets a page the agent
    /// read be treated as something the company decided.
    pub fn inbound_context(&self) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Context, Provenance::External),
        ))
    }

    pub fn agent_context(&self, agent_id: &str) -> Arc<dyn ContextStore> {
        self.scoped_context(format!("agent:{agent_id}"), || {
            ProviderContextStore::new(
                self.bound(Scope::Agent(agent_id.to_string()), Provenance::Internal),
            )
        })
    }

    /// One desk's shared partition.
    pub fn desk_context(&self, desk_id: &str) -> Arc<dyn ContextStore> {
        self.scoped_context(format!("desk:{desk_id}"), || {
            ProviderContextStore::new(
                self.bound(Scope::Desk(desk_id.to_string()), Provenance::Internal),
            )
        })
    }

    fn scoped_context(
        &self,
        key: String,
        make: impl FnOnce() -> ProviderContextStore,
    ) -> Arc<dyn ContextStore> {
        let mut cache = self
            .context_stores
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .expect("context store cache lock poisoned");
        // Bound the cache at [`SCOPED_CONTEXT_CACHE_CAPACITY`]: an unbounded
        // scope set must not grow the map without limit. Evict only entries
        // no caller still holds — `Arc::strong_count == 1` means the cache is
        // the sole holder, so dropping one cannot leave a second facade for a
        // scope beside the one a running agent already has. Two facades for a
        // scope would each carry their own `label_lock`, and two concurrent
        // `put`/`delete_label` read-merge-writes through the pair could
        // interleave and lose a label claim (#1300). If every entry is still
        // held, accept the temporary over-capacity: those `Arc`s drain as
        // their callers drop them, and the next access to an unheld scope
        // evicts again.
        //
        // When over capacity, drain the whole surplus down to the cap rather
        // than evict a single entry. A burst of live agent/desk scopes can
        // push the map past its capacity while every entry has an external
        // holder; once those holders drop, one-per-miss eviction would leave
        // the surplus in place forever — each miss removes one entry and
        // reinserts its replacement, so the process-lifetime cache stays
        // permanently oversized. Removing every unheld entry down to the cap
        // (plus one for the key being inserted) walks it back in a single
        // miss once the burst's callers are gone.
        if !cache.contains_key(&key) && cache.len() >= SCOPED_CONTEXT_CACHE_CAPACITY {
            let surplus = cache.len() - SCOPED_CONTEXT_CACHE_CAPACITY + 1;
            let victims: Vec<String> = cache
                .iter()
                .filter(|(_, facade)| Arc::strong_count(facade) == 1)
                .take(surplus)
                .map(|(k, _)| k.clone())
                .collect();
            for victim in victims {
                cache.remove(&victim);
            }
        }
        cache.entry(key).or_insert_with(|| Arc::new(make())).clone()
    }

    /// Provisional working-out, unreachable from durable recall.
    ///
    /// Nothing written here can be returned by [`context`](Self::context),
    /// [`agent_context`](Self::agent_context) or
    /// [`desk_context`](Self::desk_context): those facades scope recall to their
    /// own namespace and drop any hit reported outside it, and the scratch
    /// namespace is a sibling of all three. The roles that judge get neither
    /// half — unsettled working-out read as progress is what keeps a loop
    /// retrying.
    pub fn scratch(&self) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Scratch, Provenance::Internal),
        ))
    }

    /// The brain's traces and task results.
    pub fn memory(&self) -> Arc<dyn MemoryStore> {
        Arc::new(self.trace_store())
    }

    /// The same store, concretely, for the operator rights the port does not
    /// carry.
    fn trace_store(&self) -> ProviderMemoryStore {
        ProviderMemoryStore::new(
            self.bound(Scope::Traces, Provenance::Internal),
            self.bound(Scope::Archive, Provenance::Internal),
            self.bound(Scope::TaskResults, Provenance::Internal),
        )
    }

    /// Traces that eviction archived.
    ///
    /// An operator right, not a port method: `MemoryStore` has no way to ask
    /// this, and `evict` promises the traces still exist, so something has to be
    /// able to show them. Also what makes "archives rather than destroys"
    /// testable as a property rather than as a count.
    pub async fn archived_traces(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::CompressedTrace>> {
        self.trace_store().archived_traces(company).await
    }

    /// Restores traces directly into the archive namespace for bundle import.
    pub async fn restore_archived_traces(
        &self,
        company: &CompanyId,
        traces: &[crate::ports::CompressedTrace],
    ) -> Result<()> {
        self.trace_store()
            .restore_archived_traces(company, traces)
            .await
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "memory_behavior_tests.rs"]
mod tests_behavior;
