//! End-to-end tests for the decorator, against a real provider.
//!
//! The fake below is a full `tinymemory_api::traits::Memory` backend composed
//! through `MemoryTraitProvider`, which is the same composition every shipped
//! driver goes through — so these exercise the real provider path rather than a
//! mock of it. It **overrides `store_with_taint`**, which is the whole reason a
//! backend is written by hand here: the trait's default silently drops the taint
//! argument, and a fake that inherited it would make the taint tests pass while
//! proving nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tinymemory::mandatory::MemoryTraitProvider;
use tinymemory::registry::DriverClass;
use tinymemory_api::traits::Memory;
use tinymemory_api::types::{
    MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, RecallOpts,
};

use super::BoundMemory;
use crate::ports::{
    CompanyId, CompressedTrace, ContextChunk, EvictionPolicy, FactKind, FactRecord,
};

/// An in-memory `Memory` backend, keyed exactly as the contract specifies.
#[derive(Default)]
struct FakeEngine {
    rows: Mutex<BTreeMap<(String, String), MemoryEntry>>,
}

impl FakeEngine {
    fn provider() -> Arc<dyn tinymemory_api::provider::MemoryProvider> {
        Self::with_handle().1
    }

    /// The engine *and* its provider, for tests that need to inspect what was
    /// actually persisted rather than what a read path chose to return.
    fn with_handle() -> (Arc<Self>, Arc<dyn tinymemory_api::provider::MemoryProvider>) {
        let engine = Arc::new(Self::default());
        let provider = Arc::new(MemoryTraitProvider::new(engine.clone(), "fake-engine"));
        (engine, provider)
    }

    /// The taint actually stored alongside the row whose content contains
    /// `needle`.
    ///
    /// Reads the backing rows directly. Going through a read path would test the
    /// read path's fidelity as much as the write's, and taint is a property of
    /// what was *written* — a read that dropped it would make this pass.
    fn taint_of(&self, needle: &str) -> Option<MemoryTaint> {
        self.rows
            .lock()
            .unwrap()
            .values()
            .find(|entry| entry.content.contains(needle))
            .map(|entry| entry.taint)
    }
}

#[async_trait]
impl Memory for FakeEngine {
    fn name(&self) -> &str {
        "fake-engine"
    }

    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_with_taint(
            namespace,
            key,
            content,
            category,
            session_id,
            MemoryTaint::Internal,
        )
        .await
    }

    /// Overridden — see the module docs. The default drops `taint`.
    async fn store_with_taint(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> anyhow::Result<()> {
        self.rows.lock().unwrap().insert(
            (namespace.to_string(), key.to_string()),
            MemoryEntry {
                id: format!("{namespace}/{key}"),
                key: key.to_string(),
                content: content.to_string(),
                namespace: Some(namespace.to_string()),
                category,
                timestamp: "1970-01-01T00:00:00Z".to_string(),
                session_id: session_id.map(str::to_owned),
                score: None,
                taint,
            },
        );
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .values()
            .filter(|entry| {
                opts.namespace
                    .is_none_or(|ns| entry.namespace.as_deref() == Some(ns))
            })
            .filter(|entry| entry.content.to_lowercase().contains(&query.to_lowercase()))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(&(namespace.to_string(), key.to_string()))
            .cloned())
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .values()
            .filter(|entry| namespace.is_none_or(|ns| entry.namespace.as_deref() == Some(ns)))
            .filter(|entry| category.is_none_or(|cat| &entry.category == cat))
            .filter(|entry| session_id.is_none_or(|s| entry.session_id.as_deref() == Some(s)))
            .cloned()
            .collect())
    }

    async fn forget(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .remove(&(namespace.to_string(), key.to_string()))
            .is_some())
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        // Real summaries, derived from the rows: the mandatory composition's
        // `export_page` WALKS these namespaces — a fake that reports none
        // exports nothing, and every export/import test silently asserts on
        // an empty page.
        let rows = self.rows.lock().unwrap();
        let mut namespaces: Vec<String> = rows.keys().map(|(ns, _)| ns.clone()).collect();
        namespaces.sort();
        namespaces.dedup();
        Ok(namespaces
            .into_iter()
            .map(|namespace| {
                let count = rows.keys().filter(|(ns, _)| *ns == namespace).count();
                NamespaceSummary {
                    namespace,
                    count,
                    last_updated: None,
                }
            })
            .collect())
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.rows.lock().unwrap().len())
    }

    async fn health_check(&self) -> bool {
        true
    }
}

/// A `Memory` backend that wraps [`FakeEngine`] and injects one failure into
/// the archive partition.
///
/// `store_with_taint` counts writes whose namespace is the archive tier and
/// errors on the `fail_archive_store_on`-th one, then recovers. Eviction's
/// archive-then-delete order is what this targets: the maintenance loop
/// archives a few traces, hits the injected failure, and the archive must
/// still be bounded on that partial-failure path.
#[derive(Clone)]
struct FlakyStore {
    inner: Arc<FakeEngine>,
    fail_archive_store_on: u32,
    archive_stores: Arc<Mutex<u32>>,
}

impl FlakyStore {
    fn arc(fail_archive_store_on: u32) -> Arc<dyn Memory> {
        Arc::new(Self {
            inner: Arc::new(FakeEngine::default()),
            fail_archive_store_on,
            archive_stores: Arc::new(Mutex::new(0)),
        })
    }
}

#[async_trait]
impl Memory for FlakyStore {
    fn name(&self) -> &str {
        "flaky-store"
    }

    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_with_taint(
            namespace,
            key,
            content,
            category,
            session_id,
            MemoryTaint::Internal,
        )
        .await
    }

    async fn store_with_taint(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> anyhow::Result<()> {
        if namespace.ends_with("/archive") {
            let mut stores = self.archive_stores.lock().unwrap();
            *stores += 1;
            if *stores == self.fail_archive_store_on {
                anyhow::bail!("injected archive store failure");
            }
        }
        self.inner
            .store_with_taint(namespace, key, content, category, session_id, taint)
            .await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.recall(query, limit, opts).await
    }

    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get(namespace, key).await
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.list(namespace, category, session_id).await
    }

    async fn forget(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(namespace, key).await
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        self.inner.namespace_summaries().await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
}

/// One shared engine, which is the arrangement every isolation test needs: a
/// leak is only observable when both tenants are in the same store. There is
/// deliberately no per-company binding to build — the engine is process-scoped
/// and the company arrives with each call.
fn engine() -> BoundMemory {
    BoundMemory::bind(FakeEngine::provider(), DriverClass::Embedded).unwrap()
}

fn acme_id() -> CompanyId {
    CompanyId::new("acme")
}

fn globex_id() -> CompanyId {
    CompanyId::new("globex")
}

fn a_fact(id: &str, title: &str) -> FactRecord {
    FactRecord {
        id: id.to_string(),
        kind: FactKind::Fact,
        title: title.to_string(),
        body: format!("body of {title}"),
        source: "cto".to_string(),
        updated_at_millis: 1_700_000_000_000,
    }
}

#[tokio::test]
async fn binding_runs_the_capability_audit() {
    let provider = FakeEngine::provider();
    assert!(BoundMemory::bind(provider, DriverClass::Embedded).is_ok());
}

#[tokio::test]
async fn facts_round_trip_through_the_provider() {
    let mem = engine();
    let facts = mem.facts();
    let id = acme_id();
    let fact = a_fact("f1", "Ships on Fridays");
    facts.upsert(&id, &fact).await.unwrap();
    let listed = facts.list(&id, None, None).await.unwrap();
    assert_eq!(listed, vec![fact]);
}

#[tokio::test]
async fn one_companys_facts_are_invisible_to_another() {
    // The single largest risk in this phase, against one shared engine.
    let mem = engine();
    mem.facts()
        .upsert(&acme_id(), &a_fact("f1", "secret"))
        .await
        .unwrap();
    let theirs = mem.facts().list(&globex_id(), None, None).await.unwrap();
    assert!(
        theirs.is_empty(),
        "a company read another's facts: {theirs:?}"
    );
}

#[tokio::test]
async fn deleting_a_fact_reports_whether_it_existed() {
    let mem = engine();
    let id = acme_id();
    mem.facts().upsert(&id, &a_fact("f1", "t")).await.unwrap();
    assert!(mem.facts().delete(&id, "f1").await.unwrap());
    assert!(!mem.facts().delete(&id, "f1").await.unwrap());
}

#[tokio::test]
async fn context_round_trips_and_peeks_a_range() {
    let mem = engine();
    let id = acme_id();
    let context = mem.context();
    let addr = context
        .put(
            &id,
            ContextChunk {
                label: "notes/one".into(),
                body: "the quick brown fox".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        context.peek(&id, &addr, None).await.unwrap(),
        "the quick brown fox"
    );
    assert_eq!(context.peek(&id, &addr, Some(4..9)).await.unwrap(), "quick");
    let listed = context.list(&id, "notes/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].label, "notes/one");
    assert!(listed[0].stored_at_millis > 0, "the stamp must be recorded");
}

#[tokio::test]
async fn scratch_is_unreachable_from_durable_recall() {
    // Asserted against the recall path itself, not against a routing table:
    // the durable facade is asked to find content that only exists in scratch.
    let mem = engine();
    let id = acme_id();
    mem.scratch()
        .put(
            &id,
            ContextChunk {
                label: "wip".into(),
                body: "provisional working-out about penguins".into(),
            },
        )
        .await
        .unwrap();

    let hits = mem.context().search(&id, "penguins", 10).await.unwrap();
    assert!(hits.is_empty(), "durable recall reached scratch: {hits:?}");
    let listed = mem.context().list(&id, "").await.unwrap();
    assert!(
        listed.is_empty(),
        "durable list reached scratch: {listed:?}"
    );

    // The agent and desk partitions are siblings of scratch too.
    assert!(
        mem.agent_context("cto")
            .search(&id, "penguins", 10)
            .await
            .unwrap()
            .is_empty()
    );

    // And it really is stored — the emptiness above is a firewall, not a
    // silently-dropped write.
    assert_eq!(mem.scratch().list(&id, "").await.unwrap().len(), 1);
}

#[tokio::test]
async fn agent_partitions_do_not_leak_into_each_other() {
    let mem = engine();
    let id = acme_id();
    mem.agent_context("cto")
        .put(
            &id,
            ContextChunk {
                label: "l".into(),
                body: "cto private note".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        mem.agent_context("cfo")
            .list(&id, "")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn inbound_writes_are_stamped_external_and_internal_writes_are_not() {
    // Laundering external content into internal-trust content is the failure
    // the taint parameter exists to prevent.
    let (fake, provider) = FakeEngine::with_handle();
    let memory = BoundMemory::bind(provider, DriverClass::Embedded).unwrap();
    let id = acme_id();
    memory
        .inbound_context()
        .put(
            &id,
            ContextChunk {
                label: "web".into(),
                body: "scraped from a page".into(),
            },
        )
        .await
        .unwrap();
    memory
        .context()
        .put(
            &id,
            ContextChunk {
                label: "own".into(),
                body: "the company decided this".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        fake.taint_of("scraped from a page"),
        Some(MemoryTaint::ExternalSync),
        "an inbound-channel write must stay marked as external"
    );
    assert_eq!(
        fake.taint_of("the company decided this"),
        Some(MemoryTaint::Internal),
        "the company's own writes must not be marked external"
    );
}

#[tokio::test]
async fn recent_traces_are_newest_last_and_bounded() {
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: format!("cycle {cycle}"),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    let recent = memory.recent_traces(&id, 2).await.unwrap();
    assert_eq!(
        recent
            .iter()
            .map(|t| t.cycle_id.as_str())
            .collect::<Vec<_>>(),
        vec!["c4", "c5"],
        "newest last"
    );
}

#[tokio::test]
async fn evict_archives_rather_than_destroys() {
    // Normative in docs/spec/company-brain/memory.md. The contract has no
    // archive tier, so this is the decorator's own behaviour and it needs its
    // own assertion against the archive, not just against a count.
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }

    let archived = memory
        .evict(&id, EvictionPolicy::KeepRecent { n: 2 })
        .await
        .unwrap();
    assert_eq!(archived, 3);
    let live = memory.recent_traces(&id, usize::MAX).await.unwrap();
    assert_eq!(
        live.iter().map(|t| t.cycle_id.as_str()).collect::<Vec<_>>(),
        vec!["c4", "c5"]
    );

    // Eviction MOVES the traces out of the live set, and the archive is
    // bounded by the same policy: of the three evicted, the newest two (c2,
    // c3) are retained and the oldest (c1) is pruned. The archive keeps the
    // eviction history nearest to the live window, never the whole lifetime.
    let kept = mem.archived_traces(&id).await.unwrap();
    let mut ids: Vec<&str> = kept.iter().map(|t| t.cycle_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["c2", "c3"],
        "evicted traces live on, bounded at n"
    );
}

#[tokio::test]
async fn evict_keep_recent_bounds_the_archive() {
    // The retention policy must bound storage on this backend too, not just
    // the inspectable live window: a company that cycles for years would
    // otherwise accumulate every trace it ever evicted in the archive.
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    // 100 cycles in batches of 10, evicting down to 8 after each batch.
    for batch in 0..10 {
        for n in 0..10 {
            let i = batch * 10 + n;
            memory
                .save_trace(
                    &id,
                    CompressedTrace {
                        cycle_id: format!("c{i}"),
                        summary: "s".into(),
                        at_millis: 100 + i as u64,
                    },
                )
                .await
                .unwrap();
        }
        memory
            .evict(&id, EvictionPolicy::KeepRecent { n: 8 })
            .await
            .unwrap();
    }
    let live = memory.recent_traces(&id, usize::MAX).await.unwrap();
    assert_eq!(live.len(), 8, "the live window stays bounded");
    let archived = mem.archived_traces(&id).await.unwrap();
    assert!(
        archived.len() <= 8,
        "the archive stays bounded at n too: {} traces retained from 100 cycles",
        archived.len()
    );
}

#[tokio::test]
async fn evict_older_than_archives_everything_it_removes() {
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    let removed = memory
        .evict(
            &id,
            EvictionPolicy::OlderThan {
                before_millis: u64::MAX,
            },
        )
        .await
        .unwrap();
    assert_eq!(removed, 3);
    assert!(
        memory
            .recent_traces(&id, usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mem.archived_traces(&id).await.unwrap().len(),
        3,
        "none lost"
    );
}

#[tokio::test]
async fn evict_older_than_bounds_the_archive_at_the_retention_limit() {
    // `OlderThan` has no `n` of its own to bound the archive by, so eviction
    // prunes it to the retention limit — the same window the live set is held
    // to. Without this, an operator-sized older-than sweep would move the whole
    // lifetime into the archive and grow storage without bound, and
    // `GET /memory/archives` would be a bounded read only by the route's
    // sort-and-tail rather than by construction.
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    let limit = crate::runtime::maintenance::TRACE_RETENTION_LIMIT;
    let n = limit + 10;
    for i in 0..n {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: format!("c{i}"),
                    summary: "s".into(),
                    at_millis: 100 + i as u64,
                },
            )
            .await
            .unwrap();
    }
    let removed = memory
        .evict(
            &id,
            EvictionPolicy::OlderThan {
                before_millis: u64::MAX,
            },
        )
        .await
        .unwrap();
    assert_eq!(removed, n as u64, "the whole live window is evicted");
    assert!(
        memory
            .recent_traces(&id, usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        mem.archived_traces(&id).await.unwrap().len(),
        limit,
        "an older-than sweep cannot grow the archive past the retention limit"
    );
}

#[tokio::test]
async fn evict_bounds_the_archive_when_the_loop_fails_partway() {
    // The archive bound must survive a partial eviction failure. Archive-
    // then-delete has already moved the traces before the failure, so
    // skipping the prune on the error path would let a maintenance pass that
    // repeatedly fails partway grow the archive past its retention limit. The
    // prune is best-effort there: the original provider error still
    // propagates.
    //
    // The flaky engine fails the 6th store into the archive partition. The
    // first eviction archives four traces (calls 1-4) and prunes to n=1; the
    // second archives one trace (call 5), then fails on the next (call 6) —
    // leaving c5 and c4 in the archive, which the failure-path prune must
    // bring back down to the newest one (c5).
    let mem = BoundMemory::bind(
        Arc::new(MemoryTraitProvider::new(FlakyStore::arc(6), "flaky-engine")),
        DriverClass::Embedded,
    )
    .unwrap();
    let id = acme_id();
    let memory = mem.memory();
    for (n, cycle) in ["c1", "c2", "c3", "c4", "c5"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 100 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    // First pass: fully successful, leaves the archive bounded at 1.
    let archived = memory
        .evict(&id, EvictionPolicy::KeepRecent { n: 1 })
        .await
        .unwrap();
    assert_eq!(archived, 4);

    for (n, cycle) in ["c6", "c7", "c8"].iter().enumerate() {
        memory
            .save_trace(
                &id,
                CompressedTrace {
                    cycle_id: (*cycle).to_string(),
                    summary: "s".into(),
                    at_millis: 105 + n as u64,
                },
            )
            .await
            .unwrap();
    }
    // Second pass: archives c5 (call 5), then fails on c6 (call 6).
    let err = memory
        .evict(&id, EvictionPolicy::KeepRecent { n: 1 })
        .await
        .expect_err("the injected archive store failure must propagate");
    assert!(
        err.to_string().contains("injected archive store failure"),
        "the original provider error must survive: {err}"
    );

    // The trace moved before the failure is still bounded: the archive keeps
    // the newest evicted trace (c5) and prunes the one it displaced (c4).
    let kept = mem.archived_traces(&id).await.unwrap();
    let ids: Vec<&str> = kept.iter().map(|t| t.cycle_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["c5"],
        "the archive must be bounded even when the eviction loop fails partway"
    );
}

#[tokio::test]
async fn task_results_do_not_appear_among_traces() {
    let mem = engine();
    let id = acme_id();
    let memory = mem.memory();
    memory
        .save_task_result(
            &id,
            crate::ports::TaskResult {
                task_id: "t1".into(),
                ok: true,
                output: serde_json::json!({ "done": true }),
            },
        )
        .await
        .unwrap();
    assert!(
        memory
            .recent_traces(&id, usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn traces_are_isolated_between_companies() {
    let mem = engine();
    mem.memory()
        .save_trace(
            &acme_id(),
            CompressedTrace {
                cycle_id: "c1".into(),
                summary: "acme only".into(),
                at_millis: 1,
            },
        )
        .await
        .unwrap();
    assert!(
        mem.memory()
            .recent_traces(&globex_id(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn re_putting_an_identical_body_keeps_the_first_stamp() {
    let mem = engine();
    let id = acme_id();
    let context = mem.context();
    let chunk = ContextChunk {
        label: "notes/one".into(),
        body: "same body".into(),
    };
    let first = context.put(&id, chunk.clone()).await.unwrap();
    let stamp = context.list(&id, "").await.unwrap()[0].stored_at_millis;
    let second = context.put(&id, chunk).await.unwrap();
    assert_eq!(first, second, "the same body mints the same address");
    let listed = context.list(&id, "").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].stored_at_millis, stamp, "the stamp did not move");
}

#[tokio::test]
async fn the_driver_id_and_capabilities_are_reportable() {
    let mem = engine();
    assert_eq!(mem.driver_id(), "fake-engine");
    let caps = mem.capability_names();
    // The mandatory three, and — for a mandatory-only composition — nothing else.
    assert!(caps.contains(&"core"), "{caps:?}");
    assert!(caps.contains(&"recall"), "{caps:?}");
    assert!(caps.contains(&"portability"), "{caps:?}");
    assert!(!caps.contains(&"graph"), "{caps:?}");
}

#[tokio::test]
async fn debug_names_the_driver_and_its_class() {
    // The engine is process-scoped and structurally holds no company, so the
    // only thing `Debug` can say is which driver was bound and how — which is
    // exactly what an operator reading a boot log needs, and nothing more.
    let rendered = format!("{:?}", engine());
    assert!(rendered.contains("fake-engine"), "{rendered}");
    assert!(rendered.contains("embedded"), "{rendered}");
}

// ---------------------------------------------------------------------------
// Port conformance
// ---------------------------------------------------------------------------
//
// Issue #914 requires the same conformance suite to hold for every port that
// binds a provider, so a provider-backed store is held to the identical
// assertions the fs, sqlite and mongodb backends are. This matters more here
// than for an in-tree backend: these facades encode records into an opaque
// `content` string and re-derive everything on the way out, so "it round-trips"
// is a property to prove against the shared suite rather than to assume.
//
// `assert_fact_store` is new coverage rather than a mirror of the cortex
// backends' three: the embedded engine implements memory + context only, so
// there has never been a cortex `FactStore` to run it against.

use crate::ports::events::EventLog;
use crate::ports::store::CompanyStore;
use crate::store::conformance;
use crate::store::{FsCompanyStore, FsEventLog};

/// The four trait objects the suite drives: fs company and event stores, paired
/// with provider-backed memory and context. The two fs slots are the ports a
/// memory engine does not implement — same arrangement the cortex backends use.
type ConformanceStores = (
    Arc<dyn CompanyStore>,
    Arc<dyn EventLog>,
    Arc<dyn crate::ports::MemoryStore>,
    Arc<dyn crate::ports::ContextStore>,
);

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

/// #914's acceptance names "taint survives export and re-import" and #1113
/// found no test for it. This is the round trip: inbound content stored
/// through the decorator (stamped `ExternalSync`), exported through the
/// provider's portability family, imported into a *fresh* engine — and the
/// taint must still be `ExternalSync` on the other side. A driver or a
/// migration that laundered the stamp here would let content a company read
/// from the web re-enter as something the company decided.
#[tokio::test]
async fn taint_survives_export_and_reimport() {
    let (_fake, provider) = FakeEngine::with_handle();
    let memory = BoundMemory::bind(provider.clone(), DriverClass::Embedded).unwrap();
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

    let page = provider.export_page(None, 100).await.unwrap();
    let exported: Vec<_> = page
        .records
        .iter()
        .filter(|record| record.payload.to_string().contains("must stay external"))
        .collect();
    assert!(!exported.is_empty(), "the inbound chunk must export");
    for record in &exported {
        assert_eq!(
            record.taint,
            MemoryTaint::ExternalSync,
            "export must carry the stamp, record {}",
            record.id
        );
    }

    let (importer, fresh) = FakeEngine::with_handle();
    fresh.import_records(page.records).await.unwrap();
    assert_eq!(
        importer.taint_of("must stay external"),
        Some(MemoryTaint::ExternalSync),
        "re-import must land the content still stamped ExternalSync"
    );
}

/// The fake's `namespace_summaries` is what the mandatory composition's export
/// WALKS, so a wrong count or a duplicated namespace there silently weakens
/// every export test built on it. The round trip above uses one namespace and
/// cannot see either fault; this pins both directly.
#[tokio::test]
async fn namespace_summaries_reports_each_namespace_once_with_its_count() {
    let (fake, provider) = FakeEngine::with_handle();
    let memory = BoundMemory::bind(provider.clone(), DriverClass::Embedded).unwrap();

    // Two companies so the namespaces differ, and differing key counts so a
    // summary that reported the store's total instead of the namespace's
    // would fail.
    for (id, keys) in [(acme_id(), 3usize), (globex_id(), 1usize)] {
        for index in 0..keys {
            memory
                .context()
                .put(
                    &id,
                    ContextChunk {
                        label: format!("chunk-{index}"),
                        body: format!("body {index}"),
                    },
                )
                .await
                .unwrap();
        }
    }

    let summaries = fake.namespace_summaries().await.unwrap();
    let mut namespaces: Vec<&str> = summaries.iter().map(|s| s.namespace.as_str()).collect();
    namespaces.sort_unstable();
    let mut deduped = namespaces.clone();
    deduped.dedup();
    assert_eq!(
        namespaces, deduped,
        "each namespace must appear exactly once"
    );

    let total: usize = summaries.iter().map(|s| s.count).sum();
    assert_eq!(
        total, 4,
        "counts must sum to the rows stored: {summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.count == 3) && summaries.iter().any(|s| s.count == 1),
        "each namespace must carry ITS own count, not the store total: {summaries:?}"
    );
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
