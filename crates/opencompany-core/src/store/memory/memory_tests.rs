//! `BoundMemory` against an in-memory engine: the port conformance suites,
//! the tenant boundary, provenance, and the per-scope cache bound.
//!
//! The engine is TinyMemory's own `ReferenceEngine` — the same in-memory
//! implementation upstream's conformance suite certifies — so these tests
//! exercise this host's decorator, not a fake written to agree with it.

use std::sync::{Arc, Mutex};

use tinymemory::{
    EngineDescriptor, EngineHealth, Error as EngineError, FetchPage, FetchRequest, ForgetReport,
    ForgetTarget, ListPage, ListRequest, MemoryEngine, MetaFilter, RecallAnswer, RecallRequest,
    StoreItem, StoreReceipt, async_trait,
};
use tinymemory_conformance::ReferenceEngine;

use super::BoundMemory;
use super::facades::EXTERNAL_TAG;
use super::tests_behavior::ConformanceStores;
use crate::ports::{CompanyId, ContextChunk, FactKind, FactRecord};
use crate::store::conformance;
use crate::store::{FsCompanyStore, FsEventLog};

/// An engine that wraps [`ReferenceEngine`] and injects one failure into the
/// archive partition.
///
/// Counts stores whose workspace is an archive tier and errors on the
/// `fail_archive_store_on`-th one, then recovers. Eviction's
/// archive-then-delete order is what this targets: the maintenance loop
/// archives a few traces, hits the injected failure, and the archive must
/// still be bounded on that partial-failure path.
pub(super) struct FlakyStore {
    inner: ReferenceEngine,
    fail_archive_store_on: u32,
    archive_stores: Mutex<u32>,
}

impl FlakyStore {
    pub(super) fn arc(fail_archive_store_on: u32) -> Arc<dyn MemoryEngine> {
        Arc::new(Self {
            inner: ReferenceEngine::new(),
            fail_archive_store_on,
            archive_stores: Mutex::new(0),
        })
    }
}

#[async_trait]
impl MemoryEngine for FlakyStore {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }

    async fn recall(&self, req: RecallRequest) -> tinymemory::Result<RecallAnswer> {
        self.inner.recall(req).await
    }

    async fn fetch(&self, req: FetchRequest) -> tinymemory::Result<FetchPage> {
        self.inner.fetch(req).await
    }

    async fn store(&self, item: StoreItem) -> tinymemory::Result<StoreReceipt> {
        let archive = item
            .meta()
            .workspace
            .as_deref()
            .is_some_and(|workspace| workspace.ends_with("/archive"));
        if archive {
            let mut stores = self.archive_stores.lock().unwrap();
            *stores += 1;
            if *stores == self.fail_archive_store_on {
                return Err(EngineError::Unavailable(
                    "injected archive store failure".into(),
                ));
            }
        }
        self.inner.store(item).await
    }

    async fn forget(&self, target: ForgetTarget) -> tinymemory::Result<ForgetReport> {
        self.inner.forget(target).await
    }

    async fn list(&self, req: ListRequest) -> tinymemory::Result<ListPage> {
        self.inner.list(req).await
    }
}

/// One shared engine, which is the arrangement every isolation test needs: a
/// leak is only observable when both tenants are in the same store. There is
/// deliberately no per-company binding to build — the engine is process-scoped
/// and the company arrives with each call.
pub(super) fn engine() -> BoundMemory {
    BoundMemory::bind(Arc::new(ReferenceEngine::new()))
}

/// [`engine`], plus the raw engine for asserting what actually landed in it.
pub(super) fn with_handle() -> (Arc<ReferenceEngine>, BoundMemory) {
    let raw = Arc::new(ReferenceEngine::new());
    (raw.clone(), BoundMemory::bind(raw))
}

/// Every item the raw engine holds, unfiltered.
pub(super) async fn everything(raw: &ReferenceEngine) -> Vec<tinymemory::Hit> {
    raw.list(ListRequest::new(MetaFilter::default(), 10_000))
        .await
        .unwrap()
        .items
}

pub(super) fn acme_id() -> CompanyId {
    CompanyId::new("acme")
}

pub(super) fn globex_id() -> CompanyId {
    CompanyId::new("globex")
}

pub(super) fn a_fact(id: &str, title: &str) -> FactRecord {
    FactRecord {
        id: id.to_string(),
        kind: FactKind::Fact,
        title: title.to_string(),
        body: format!("body of {title}"),
        source: "cto".to_string(),
        updated_at_millis: 1_700_000_000_000,
    }
}

fn conformance_stores(dir: &std::path::Path) -> ConformanceStores {
    let memory = engine();
    (
        Arc::new(FsCompanyStore::new(dir.to_path_buf())),
        Arc::new(FsEventLog::new(dir.to_path_buf())),
        memory.memory(),
        memory.context(),
    )
}

#[tokio::test]
async fn conformance_isolation_by_company() {
    let dir = tempfile::tempdir().unwrap();
    let (store, events, memory, context) = conformance_stores(dir.path());
    conformance::assert_isolation_by_company(store, events, memory, context).await;
}

#[tokio::test]
async fn conformance_export_totality() {
    let dir = tempfile::tempdir().unwrap();
    let (store, events, memory, context) = conformance_stores(dir.path());
    conformance::assert_export_totality(store, events, memory, context).await;
}

#[tokio::test]
async fn conformance_context_chunk_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, _events, _memory, context) = conformance_stores(dir.path());
    conformance::assert_context_chunk_stamps(context).await;
}

#[tokio::test]
async fn conformance_fact_store() {
    conformance::assert_fact_store(engine().facts()).await;
}

// Exercises the facade's one-enumeration `peek_many` override against the
// same positional contract the default implementation gives.
#[tokio::test]
async fn conformance_context_peek_many() {
    conformance::assert_context_peek_many_answers_positionally(engine().context()).await;
}

#[tokio::test]
async fn conformance_context_multibyte_bodies() {
    conformance::assert_multibyte_bodies_survive_search_and_ranged_peek(engine().context()).await;
}

#[tokio::test]
async fn conformance_context_identical_body_two_labels() {
    conformance::assert_identical_body_two_labels(engine().context()).await;
}

#[tokio::test]
async fn conformance_context_delete_label_scoped() {
    conformance::assert_delete_label_scoped(engine().context()).await;
}

#[tokio::test]
async fn conformance_context_delete_label_survives_a_concurrent_identical_put() {
    conformance::assert_delete_label_survives_a_concurrent_identical_put(engine().context()).await;
}

/// #914's acceptance names "taint survives export and re-import". v2 has no
/// taint, so provenance is the `EXTERNAL_TAG` this host stamps — and it must
/// survive an engine-to-engine migration: content a company read from the web
/// must not re-enter the target as something the company decided.
#[tokio::test]
async fn provenance_survives_migration() {
    let (raw, memory) = with_handle();
    memory
        .inbound_context()
        .put(
            &acme_id(),
            ContextChunk {
                label: "web".into(),
                body: "scraped from a page, must stay external".into(),
            },
        )
        .await
        .unwrap();

    let from: Arc<dyn MemoryEngine> = raw;
    let fresh = Arc::new(ReferenceEngine::new());
    let to: Arc<dyn MemoryEngine> = fresh.clone();
    super::migrate::migrate(&from, &to, 10, None, |_| {})
        .await
        .unwrap()
        .unwrap();

    let landed: Vec<_> = everything(&fresh)
        .await
        .into_iter()
        .filter(|hit| hit.text.contains("must stay external"))
        .collect();
    assert!(!landed.is_empty(), "the inbound chunk must migrate");
    for hit in landed {
        assert!(
            hit.meta.tags.iter().any(|tag| tag == EXTERNAL_TAG),
            "migration must carry the external mark: {:?}",
            hit.meta
        );
    }
}

/// Internal writes are never marked external — the mark has to mean something.
#[tokio::test]
async fn internal_writes_carry_no_external_mark() {
    let (raw, memory) = with_handle();
    memory
        .context()
        .put(
            &acme_id(),
            ContextChunk {
                label: "plan".into(),
                body: "the company decided this".into(),
            },
        )
        .await
        .unwrap();
    for hit in everything(&raw).await {
        assert!(!hit.meta.tags.iter().any(|tag| tag == EXTERNAL_TAG));
    }
}

/// Every write lands under its own company's workspace, and each company's
/// items are counted in its own workspace only — the raw-engine view of the
/// tenant boundary the port tests check from the outside.
#[tokio::test]
async fn every_write_lands_in_its_own_companys_workspace() {
    let (raw, memory) = with_handle();
    for (id, keys) in [(acme_id(), 3usize), (globex_id(), 1usize)] {
        for index in 0..keys {
            memory
                .context()
                .put(
                    &id,
                    ContextChunk {
                        label: format!("chunk-{index}"),
                        body: format!("{} body {index}", id.as_ref()),
                    },
                )
                .await
                .unwrap();
        }
    }
    let items = everything(&raw).await;
    assert_eq!(items.len(), 4);
    let mut workspaces: Vec<String> = items
        .iter()
        .map(|hit| hit.meta.workspace.clone().unwrap())
        .collect();
    workspaces.sort();
    workspaces.dedup();
    assert_eq!(workspaces.len(), 2, "{workspaces:?}");
    for hit in &items {
        let workspace = hit.meta.workspace.as_deref().unwrap();
        assert_eq!(hit.meta.folder.as_deref(), Some(workspace));
        let company = if hit.text.contains("acme body") {
            "acme"
        } else {
            "globex"
        };
        assert!(
            workspace.starts_with(&format!("oc/{company}-")),
            "{workspace}"
        );
    }
}

#[test]
fn scoped_context_cache_stays_bounded() {
    // The per-scope cache is keyed by agent/desk id, which is attacker-adjacent
    // input in a multi-company host: nothing but internal id hygiene stops a
    // process-lifetime HashMap from accumulating one entry per distinct id
    // ever seen. The bound is a capacity cap with arbitrary eviction, safe
    // because entries are `Arc`-backed — this proves the map cannot outgrow it
    // no matter how many distinct scopes are asked for.
    let mem = engine();
    let cache = mem
        .context_stores
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()));

    let overflow = super::SCOPED_CONTEXT_CACHE_CAPACITY + 64;
    for i in 0..overflow {
        // Distinct ids: the cache is keyed by the `agent:<id>` label, so
        // repeated access to the same scope would never grow it. Use the raw
        // private field via `agent_context` and drop the Arc each iteration so
        // nothing pins the victim entries alive across eviction.
        let _ = mem.agent_context(&format!("agent-{i}"));
    }

    let len = cache.lock().unwrap().len();
    assert!(
        len <= super::SCOPED_CONTEXT_CACHE_CAPACITY,
        "per-scope context cache grew to {len} entries, past the {}-entry cap",
        super::SCOPED_CONTEXT_CACHE_CAPACITY
    );
}

#[test]
fn scoped_context_keeps_facades_still_held() {
    // A facade handed to a caller must not be evicted to make room for a new
    // scope. Evicting one whose `Arc` is still live would let a later request
    // build a SECOND facade for the same scope with its own `label_lock`, and
    // two concurrent read-merge-writes through the pair could lose a label
    // claim (#1300). Only entries the cache alone holds (`strong_count == 1`)
    // are evictable; when every entry is still held the cache grows past the
    // cap rather than drop a live lock.
    let mem = engine();
    let cache = mem
        .context_stores
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()));

    let cap = super::SCOPED_CONTEXT_CACHE_CAPACITY;
    // Fill the cache to capacity while KEEPING every facade alive, so every
    // entry has an external holder.
    let held: Vec<_> = (0..cap)
        .map(|i| mem.agent_context(&format!("agent-{i}")))
        .collect();
    assert_eq!(cache.lock().unwrap().len(), cap);

    // A new scope at capacity must not evict a held facade: the cache grows
    // past the cap by one rather than drop a live `label_lock`.
    let _new = mem.agent_context(&format!("agent-{cap}"));
    assert_eq!(
        cache.lock().unwrap().len(),
        cap + 1,
        "held facades must not be evicted to make room"
    );

    // Once callers drop their `Arc`s, the entries become evictable again: a
    // further new scope evicts one and the cache stops growing.
    drop(held);
    let _another = mem.agent_context("agent-next");
    assert!(
        cache.lock().unwrap().len() <= cap + 1,
        "the cache should stop growing once held facades drain"
    );
}

#[test]
fn scoped_context_drains_back_to_capacity() {
    // A burst of live scopes can push the cache past its capacity while every
    // entry has an external holder. Once those holders drop, the next miss
    // must drain the whole surplus back to the cap — not replace a single
    // entry and reinsert, which would leave the process-lifetime map
    // permanently oversized: every later miss would evict exactly one unheld
    // entry and insert its replacement, so the surplus would never shrink.
    let mem = engine();
    let cache = mem
        .context_stores
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()));

    let cap = super::SCOPED_CONTEXT_CACHE_CAPACITY;
    let burst = 64;

    // Grow the cache past the cap while keeping every facade alive.
    let held: Vec<_> = (0..cap + burst)
        .map(|i| mem.agent_context(&format!("agent-{i}")))
        .collect();
    assert_eq!(cache.lock().unwrap().len(), cap + burst);

    // Drop the holders: every entry is now held by the cache alone and
    // evictable. One miss must drain the entire surplus (plus room for the
    // key being inserted), walking the map back to exactly its capacity.
    drop(held);
    let _drain = mem.agent_context("agent-drain");
    assert_eq!(
        cache.lock().unwrap().len(),
        cap,
        "a miss after holders drop must drain the cache back to its capacity"
    );
}
