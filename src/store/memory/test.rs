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

    let kept = mem.archived_traces(&id).await.unwrap();
    let mut ids: Vec<&str> = kept.iter().map(|t| t.cycle_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["c1", "c2", "c3"], "evicted traces live on");
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

/// The port conformance suite over the REAL `namespace` driver — not the fake.
///
/// Everything above proves the decorator against `FakeEngine`; the driver
/// tests in `driver.rs` prove the namespace driver against the raw provider
/// contract. Neither proves the two composed: that the three ports a company
/// actually holds — traces, chunks with search, facts — round-trip through
/// `BoundMemory`'s facades into `tinymemory-core`'s durable store and back.
/// This is that proof, run over the same shared asserts every other backend
/// answers, so the embedded contract driver cannot quietly drift below the
/// bar the fs/sqlite/mongodb stores are held to.
#[cfg(feature = "tinymemory-embedded")]
mod namespace_driver_conformance {
    use super::*;
    use crate::store::{FsCompanyStore, FsEventLog};

    /// A `BoundMemory` over the real driver, plus the tempdir keeping the
    /// store alive for the test's duration.
    fn real_engine() -> (tempfile::TempDir, BoundMemory) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::store::memory::MemoryDriverConfig {
            mode: crate::store::memory::MemoryMode::Embedded,
            driver_id: Some("namespace".into()),
            url: None,
            api_key: None,
            data_dir: Some(dir.path().to_path_buf()),
        };
        let (provider, class) = crate::store::memory::open_driver(&config)
            .expect("the namespace driver binds")
            .expect("embedded with a driver named yields a provider");
        (dir, BoundMemory::bind(provider, class).unwrap())
    }

    #[tokio::test]
    async fn isolation_by_company_holds_on_the_namespace_driver() {
        let dir = tempfile::tempdir().unwrap();
        let (store_dir, engine) = real_engine();
        conformance::assert_isolation_by_company(
            Arc::new(FsCompanyStore::new(dir.path().to_path_buf())),
            Arc::new(FsEventLog::new(dir.path().to_path_buf())),
            engine.memory(),
            engine.context(),
        )
        .await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn export_totality_holds_on_the_namespace_driver() {
        let dir = tempfile::tempdir().unwrap();
        let (store_dir, engine) = real_engine();
        conformance::assert_export_totality(
            Arc::new(FsCompanyStore::new(dir.path().to_path_buf())),
            Arc::new(FsEventLog::new(dir.path().to_path_buf())),
            engine.memory(),
            engine.context(),
        )
        .await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn the_fact_store_contract_holds_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        conformance::assert_fact_store(engine.facts()).await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn context_chunk_stamps_hold_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        conformance::assert_context_chunk_stamps(engine.context()).await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn context_peek_many_answers_positionally_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        conformance::assert_context_peek_many_answers_positionally(engine.context()).await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn multibyte_bodies_survive_search_and_peek_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        conformance::assert_multibyte_bodies_survive_search_and_ranged_peek(engine.context()).await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn identical_body_two_labels_holds_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        conformance::assert_identical_body_two_labels(engine.context()).await;
        drop(store_dir);
    }

    #[tokio::test]
    async fn delete_label_is_scoped_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        conformance::assert_delete_label_scoped(engine.context()).await;
        drop(store_dir);
    }

    /// #1201: the conformance suite failed at random on this driver because
    /// its write path runs a PII scrubber over the serialized envelope, and a
    /// 13-digit `at_millis` that happens to pass Luhn (~10% of epoch-millis
    /// stamps do) was redacted into unparseable JSON — the read side then
    /// dropped the whole record. Deterministic pin: the middle stamp below is
    /// the Luhn-valid one from the original failure, so this test fails 100%
    /// of the time on a driver with that defect, not 10%.
    #[tokio::test]
    async fn luhn_valid_timestamps_round_trip_on_the_namespace_driver() {
        let (store_dir, engine) = real_engine();
        let memory = engine.memory();
        let traces = vec![
            CompressedTrace {
                cycle_id: "c0".into(),
                summary: "summary 0".into(),
                at_millis: 1_787_178_633_770,
            },
            CompressedTrace {
                cycle_id: "c1".into(),
                summary: "summary 1".into(),
                at_millis: 1_787_178_633_773,
            },
            CompressedTrace {
                cycle_id: "c2".into(),
                summary: "summary 2".into(),
                at_millis: 1_787_178_633_774,
            },
        ];
        for trace in &traces {
            memory.save_trace(&acme_id(), trace.clone()).await.unwrap();
        }
        let read = memory.recent_traces(&acme_id(), usize::MAX).await.unwrap();
        assert_eq!(
            read, traces,
            "every trace round-trips regardless of its timestamp's digits"
        );
        drop(store_dir);
    }
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
