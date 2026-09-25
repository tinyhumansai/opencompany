use super::*;

/// A stub whose every mandatory read fails, so the probe has something to
/// find. `NullMemoryProvider` answers everything, which is the right
/// subject for the empty-instance case and useless for the failure one.
#[cfg(feature = "tinymemory")]
#[derive(Debug)]
struct FailingProvider {
    fail_core: bool,
    fail_recall: bool,
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::traits::Memory for FailingProvider {
    fn name(&self) -> &str {
        "failing"
    }
    async fn store(
        &self,
        _namespace: &str,
        _key: &str,
        _content: &str,
        _category: tinymemory_api::types::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get(
        &self,
        _namespace: &str,
        _key: &str,
    ) -> anyhow::Result<Option<tinymemory_api::types::MemoryEntry>> {
        if self.fail_core {
            anyhow::bail!("core is unreachable");
        }
        Ok(None)
    }
    async fn forget(&self, _namespace: &str, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn list(
        &self,
        _namespace: Option<&str>,
        _category: Option<&tinymemory_api::types::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn namespace_summaries(
        &self,
    ) -> anyhow::Result<Vec<tinymemory_api::types::NamespaceSummary>> {
        Ok(Vec::new())
    }
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        true
    }
    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _opts: tinymemory_api::types::RecallOpts<'_>,
    ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
        if self.fail_recall {
            anyhow::bail!("recall is unreachable");
        }
        Ok(Vec::new())
    }
}

#[cfg(feature = "tinymemory")]
fn failing(fail_core: bool, fail_recall: bool) -> tinymemory_api::mandatory::MemoryTraitProvider {
    tinymemory_api::mandatory::MemoryTraitProvider::new(
        Arc::new(FailingProvider {
            fail_core,
            fail_recall,
        }),
        "failing",
    )
}

/// A working engine that holds nothing must not be reported as broken.
///
/// On a freshly provisioned per-tenant instance every family is legitimately
/// empty, so a probe reading "returned no rows" as "not implemented" would
/// refuse every family on day one. `NullMemoryProvider` is exactly that
/// shape — every read succeeds and returns nothing — so it must probe clean.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn an_empty_engine_probes_clean() {
    let provider = tinymemory_api::null::NullMemoryProvider::new();
    let outcome = probe_engine(&provider, std::time::Duration::from_secs(5)).await;
    assert!(
        outcome.unreachable.is_empty() && outcome.slow.is_empty(),
        "an engine that answers every read but holds nothing must not be reported \
             unreachable or slow; got {outcome:?}"
    );
}

/// The direction the clean-probe test cannot pin: an engine that fails must
/// actually be reported. Without this, replacing the probe body with
/// `Vec::new()` still passes the suite.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn a_dead_engine_reports_every_family() {
    let provider = failing(true, true);
    let outcome = probe_engine(&provider, std::time::Duration::from_secs(5)).await;
    assert_eq!(
        outcome.unreachable,
        vec!["core".to_string(), "recall".to_string()]
    );
    assert!(outcome.slow.is_empty(), "an error is not a timeout");
}

/// Attribution: one broken family must not condemn the others, and — the
/// case that caught a real bug — a recall that fails must be *seen* to
/// fail. An empty probe query short-circuits inside `RemoteMemory::recall`
/// before the network, which made this leg unfalsifiable.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn one_broken_family_is_named_alone() {
    let outcome = probe_engine(&failing(false, true), std::time::Duration::from_secs(5)).await;
    assert_eq!(outcome.unreachable, vec!["recall".to_string()]);

    let outcome = probe_engine(&failing(true, false), std::time::Duration::from_secs(5)).await;
    assert_eq!(outcome.unreachable, vec!["core".to_string()]);
}

/// A blackholed endpoint must surface as *slow*, not as clean.
///
/// This is the branch `refresh_health`'s own doc names — packets going
/// nowhere — and it is the one a weakened guard would silently pass.
/// Relaxing `Ok(Ok(_))` to `!matches!(.., Ok(Err(_)))` makes a timeout look
/// healthy; this fails if that happens.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn a_blackholed_engine_reports_slow_not_clean() {
    #[derive(Debug, Default)]
    struct Sleeper;

    #[async_trait]
    impl tinymemory_api::traits::Memory for Sleeper {
        fn name(&self) -> &str {
            "sleeper"
        }
        async fn store(
            &self,
            _n: &str,
            _k: &str,
            _c: &str,
            _cat: tinymemory_api::types::MemoryCategory,
            _s: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get(
            &self,
            _n: &str,
            _k: &str,
        ) -> anyhow::Result<Option<tinymemory_api::types::MemoryEntry>> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(None)
        }
        async fn forget(&self, _n: &str, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn list(
            &self,
            _n: Option<&str>,
            _c: Option<&tinymemory_api::types::MemoryCategory>,
            _s: Option<&str>,
        ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn namespace_summaries(
            &self,
        ) -> anyhow::Result<Vec<tinymemory_api::types::NamespaceSummary>> {
            Ok(Vec::new())
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
        async fn recall(
            &self,
            _q: &str,
            _l: usize,
            _o: tinymemory_api::types::RecallOpts<'_>,
        ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(Vec::new())
        }
    }

    let provider =
        tinymemory_api::mandatory::MemoryTraitProvider::new(Arc::new(Sleeper), "sleeper");
    let outcome = probe_engine(&provider, std::time::Duration::from_millis(50)).await;
    assert_eq!(outcome.slow, vec!["core".to_string(), "recall".to_string()]);
    assert!(
        outcome.unreachable.is_empty(),
        "a timeout is not a refusal: the engine never said no, it just did not answer"
    );
}

/// The probe must send a **non-empty** recall query.
///
/// `RemoteMemory::recall` returns `Ok(vec![])` without reaching the network
/// when the query trims to empty, so an empty probe query makes the recall
/// leg unfalsifiable on every hosted engine: a revoked credential reports
/// healthy. A stub cannot reproduce that short-circuit — it lives in the
/// remote adapter, not the contract — so this asserts the precondition
/// directly instead.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn the_recall_probe_query_is_never_empty() {
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct Recorder(Mutex<Option<String>>);

    #[async_trait]
    impl tinymemory_api::traits::Memory for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        async fn store(
            &self,
            _n: &str,
            _k: &str,
            _c: &str,
            _cat: tinymemory_api::types::MemoryCategory,
            _s: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get(
            &self,
            _n: &str,
            _k: &str,
        ) -> anyhow::Result<Option<tinymemory_api::types::MemoryEntry>> {
            Ok(None)
        }
        async fn forget(&self, _n: &str, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn list(
            &self,
            _n: Option<&str>,
            _c: Option<&tinymemory_api::types::MemoryCategory>,
            _s: Option<&str>,
        ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn namespace_summaries(
            &self,
        ) -> anyhow::Result<Vec<tinymemory_api::types::NamespaceSummary>> {
            Ok(Vec::new())
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
        async fn recall(
            &self,
            query: &str,
            _limit: usize,
            _opts: tinymemory_api::types::RecallOpts<'_>,
        ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
            *self.0.lock().expect("probe query lock") = Some(query.to_string());
            Ok(Vec::new())
        }
    }

    let recorder = Arc::new(Recorder::default());
    let provider = tinymemory_api::mandatory::MemoryTraitProvider::new(
        Arc::clone(&recorder) as Arc<dyn tinymemory_api::traits::Memory>,
        "recorder",
    );
    let _ = probe_engine(&provider, std::time::Duration::from_secs(5)).await;

    let seen = recorder.0.lock().expect("probe query lock").clone();
    let seen = seen.expect("the probe never called recall at all");
    assert!(
        !seen.trim().is_empty(),
        "the recall probe sent `{seen}`, which RemoteMemory short-circuits before the \
             network — the leg would pass against a dead engine"
    );
}

/// `refresh_health` must record what it probed, not just log it — the
/// engine route reads the descriptor.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn refresh_health_records_unreachable_families() {
    let bound = crate::store::memory::BoundMemory::bind(
        Arc::new(failing(true, true)),
        tinymemory::registry::DriverClass::External,
    )
    .expect("bind");
    let mut overlay = MemoryOverlay {
        memory: bound.memory(),
        context: bound.context(),
        facts: Some(bound.facts()),
        inbound_context: Some(bound.inbound_context()),
        scratch: Some(bound.scratch()),
        scopes: Some(Arc::new(bound.clone())),
        descriptor: MemoryDescriptor {
            backend: MemoryBackend::Remote,
            driver_id: "failing".into(),
            capabilities: Vec::new(),
            healthy: None,
            unreachable_families: None,
            degraded_families: None,
            slow_families: None,
        },
        probe: Some(Arc::new(failing(true, true))),
        probe_cache: Arc::default(),
    };
    overlay
        .refresh_health(std::time::Duration::from_secs(5))
        .await;
    assert_eq!(
        overlay.descriptor.unreachable_families,
        Some(vec!["core".to_string(), "recall".to_string()])
    );
}

/// A driver that advertises **every** optional family, delegating each
/// accessor to the null driver — which implements them all and advertises
/// none, so it is exactly the body a wrapper needs and nothing more.
///
/// The subject for the question the per-family table exists to answer: given a
/// driver that claims everything, which families does this host actually read?
#[cfg(feature = "tinymemory")]
#[derive(Debug, Default)]
struct AdvertisesEverything(tinymemory_api::null::NullMemoryProvider);

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryCore for AdvertisesEverything {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: tinymemory_api::types::MemoryCategory,
        session_id: Option<&str>,
        taint: tinymemory_api::types::MemoryTaint,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        self.0
            .store(namespace, key, content, category, session_id, taint)
            .await
    }
    async fn get(
        &self,
        namespace: &str,
        key: &str,
    ) -> std::result::Result<
        Option<tinymemory_api::types::MemoryEntry>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.get(namespace, key).await
    }
    async fn forget(
        &self,
        namespace: &str,
        key: &str,
    ) -> std::result::Result<bool, tinymemory_api::error::MemoryError> {
        self.0.forget(namespace, key).await
    }
    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&tinymemory_api::types::MemoryCategory>,
        session_id: Option<&str>,
    ) -> std::result::Result<
        Vec<tinymemory_api::types::MemoryEntry>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.list(namespace, category, session_id).await
    }
    async fn namespaces(
        &self,
    ) -> std::result::Result<
        Vec<tinymemory_api::types::NamespaceSummary>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.namespaces().await
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryRecall for AdvertisesEverything {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &tinymemory_api::types::OwnedRecallOpts,
        scope: Option<&tinymemory_api::provider::SourceScope>,
    ) -> std::result::Result<
        Vec<tinymemory_api::types::MemoryEntry>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.recall(query, limit, opts, scope).await
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryPortability for AdvertisesEverything {
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> std::result::Result<tinymemory_api::provider::ExportPage, tinymemory_api::error::MemoryError>
    {
        self.0.export_page(cursor, limit).await
    }
    async fn import_records(
        &self,
        records: Vec<tinymemory_api::provider::ExportRecord>,
    ) -> std::result::Result<
        tinymemory_api::provider::ImportOutcome,
        tinymemory_api::error::MemoryError,
    > {
        self.0.import_records(records).await
    }
}

/// The null driver implements every optional family except `episodic`, so that
/// one is answered here. Every method succeeds and returns nothing: the point
/// of this double is what gets *asked*, not what comes back.
#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryEpisodic for AdvertisesEverything {
    async fn insert_turn(
        &self,
        _turn: &tinymemory_api::provider::EpisodicTurn,
    ) -> std::result::Result<i64, tinymemory_api::error::MemoryError> {
        Ok(0)
    }
    async fn session_turns(
        &self,
        _session_id: &str,
    ) -> std::result::Result<
        Vec<tinymemory_api::provider::EpisodicTurn>,
        tinymemory_api::error::MemoryError,
    > {
        Ok(Vec::new())
    }
    async fn open_segment(
        &self,
        _session_id: &str,
    ) -> std::result::Result<
        Option<tinymemory_api::provider::ConversationSegment>,
        tinymemory_api::error::MemoryError,
    > {
        Ok(None)
    }
    async fn create_segment(
        &self,
        _segment_id: &str,
        _session_id: &str,
        _namespace: &str,
        _start_episodic_id: i64,
        _start_seq: Option<u32>,
        _start_timestamp: f64,
        _now: f64,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn append_turn(
        &self,
        _segment_id: &str,
        _episodic_id: i64,
        _seq: Option<u32>,
        _timestamp: f64,
        _now: f64,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn close_segment(
        &self,
        _segment_id: &str,
        _now: f64,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn set_segment_summary(
        &self,
        _segment_id: &str,
        _summary: &str,
        _now: f64,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn insert_event(
        &self,
        _event: &tinymemory_api::provider::EpisodicEvent,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn upsert_segment_embedding(
        &self,
        _segment_id: &str,
        _model_signature: &str,
        _embedding: &[f32],
        _created_at: f64,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryProvider for AdvertisesEverything {
    fn driver_id(&self) -> &str {
        "advertises-everything"
    }
    fn capabilities(&self) -> tinymemory_api::capabilities::Capabilities {
        tinymemory_api::capabilities::Capabilities::all()
    }
    async fn health(&self) -> tinymemory_api::health::MemoryHealth {
        tinymemory_api::health::MemoryHealth::Ready
    }
    fn as_ingest(&self) -> Option<&dyn tinymemory_api::provider::MemoryIngest> {
        Some(&self.0)
    }
    fn as_documents(&self) -> Option<&dyn tinymemory_api::provider::MemoryDocuments> {
        Some(&self.0)
    }
    fn as_tree(&self) -> Option<&dyn tinymemory_api::provider::MemoryTree> {
        Some(&self.0)
    }
    fn as_entities(&self) -> Option<&dyn tinymemory_api::provider::MemoryEntities> {
        Some(&self.0)
    }
    fn as_graph(&self) -> Option<&dyn tinymemory_api::provider::MemoryGraph> {
        Some(&self.0)
    }
    fn as_diff(&self) -> Option<&dyn tinymemory_api::provider::MemoryDiff> {
        Some(&self.0)
    }
    fn as_goals(&self) -> Option<&dyn tinymemory_api::provider::MemoryGoals> {
        Some(&self.0)
    }
    fn as_tool_memory(&self) -> Option<&dyn tinymemory_api::provider::MemoryToolMemory> {
        Some(&self.0)
    }
    fn as_sources(&self) -> Option<&dyn tinymemory_api::provider::MemorySourceSink> {
        Some(&self.0)
    }
    fn as_maintenance(&self) -> Option<&dyn tinymemory_api::provider::MemoryMaintenance> {
        Some(&self.0)
    }
    fn as_people(&self) -> Option<&dyn tinymemory_api::provider::MemoryPeople> {
        Some(&self.0)
    }
    fn as_chunks(&self) -> Option<&dyn tinymemory_api::provider::MemoryChunks> {
        Some(&self.0)
    }
    fn as_retrieval(&self) -> Option<&dyn tinymemory_api::provider::MemoryRetrieval> {
        Some(&self.0)
    }
    fn as_profile(&self) -> Option<&dyn tinymemory_api::provider::MemoryProfile> {
        Some(&self.0)
    }
    fn as_episodic(&self) -> Option<&dyn tinymemory_api::provider::MemoryEpisodic> {
        Some(self)
    }
    fn as_source_sync(&self) -> Option<&dyn tinymemory_api::provider::MemorySourceSync> {
        Some(&self.0)
    }
    fn as_coding_sessions(&self) -> Option<&dyn tinymemory_api::provider::MemoryCodingSessions> {
        Some(&self.0)
    }
    fn as_scoring(&self) -> Option<&dyn tinymemory_api::provider::MemoryScoring> {
        Some(&self.0)
    }
    fn as_document_ingest(&self) -> Option<&dyn tinymemory_api::provider::MemoryDocumentIngest> {
        Some(&self.0)
    }
    fn as_conversation_ingest(
        &self,
    ) -> Option<&dyn tinymemory_api::provider::MemoryConversationIngest> {
        Some(&self.0)
    }
    fn as_learning_ingest(&self) -> Option<&dyn tinymemory_api::provider::MemoryLearningIngest> {
        Some(&self.0)
    }
    fn as_event_ingest(&self) -> Option<&dyn tinymemory_api::provider::MemoryEventIngest> {
        Some(&self.0)
    }
    fn as_answer(&self) -> Option<&dyn tinymemory_api::provider::MemoryAnswer> {
        Some(&self.0)
    }
}

/// The per-family table, pinned.
///
/// `family_leg`'s exhaustive match makes a *new* family a compile error, but it
/// says nothing about an existing arm quietly becoming `None` — which would
/// narrow what this host checks while every other test still passed. This is
/// the list, written out, for a driver that advertises all twenty-six families.
///
/// Changing it means changing `docs/spec/runtime/memory-engine.md` too: the
/// ten absences are documented there with a reason each, and an absence with no
/// reason is the defect issue #1968 is about.
#[cfg(feature = "tinymemory")]
#[test]
fn the_probed_families_are_the_documented_table() {
    let probed = probed_families(&AdvertisesEverything::default());
    assert_eq!(
        probed,
        vec![
            "core",
            "recall",
            // No `ingest`: every required method writes.
            "documents",
            "tree",
            "entities",
            "graph",
            "diff",
            "goals",
            "tool_memory",
            // No `sources`: writes, and `forget_source` deletes.
            // No `maintenance`: whole-store jobs, and the cheap reads are defaulted.
            // No `portability`: `export_page` walks the whole corpus.
            "people",
            "chunks",
            "retrieval",
            "profile",
            "episodic",
            "source_sync",
            "coding_sessions",
            "scoring",
            // No `*_ingest`: single write methods.
            // No `answer`: a metered inference call.
        ],
        "the set of families this host reads at bind time changed; update \
         docs/spec/runtime/memory-engine.md with the reason, or put the arm back"
    );
}

/// A family the driver does not advertise is not read at all.
///
/// Absence is a legitimate answer — a minimal driver implements two families
/// and nothing else — so probing an unadvertised family would report every one
/// of them broken. `NullMemoryProvider` advertises only the mandatory three and
/// implements almost every optional family behind `None` accessors, which is
/// exactly the shape that would break if the probe consulted implementations
/// rather than the advertisement.
#[cfg(feature = "tinymemory")]
#[test]
fn an_unadvertised_family_is_never_probed() {
    let probed = probed_families(&tinymemory_api::null::NullMemoryProvider::new());
    assert_eq!(
        probed,
        vec!["core", "recall"],
        "portability is mandatory and deliberately unprobed; nothing optional is advertised"
    );
}

/// The issue's harm at full width: a driver advertising every family over a
/// body that serves none of them.
///
/// `AdvertisesEverything` is exactly that — the null driver refuses every
/// optional call precisely *because* it advertises none of them, and this
/// wrapper advertises all of them anyway. `audit_provider` passes it without a
/// word: the accessors return objects, so `provides()` is true for all
/// twenty-six.
///
/// Every probed family must therefore come back refused, and none of them may
/// come back `unreachable` — the mandatory legs answered, and an optional
/// refusal is not something the apply route turns an engine away for.
/// `episodic` is the exception and the control: this wrapper answers it
/// itself, so its absence here is the probe's "empty is success" rule holding
/// for an optional family.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn an_engine_refusing_everything_it_advertises_reports_every_probed_family() {
    let outcome = probe_engine(
        &AdvertisesEverything::default(),
        std::time::Duration::from_secs(5),
    )
    .await;
    assert!(outcome.healthy, "the engine answers its health check");
    assert_eq!(
        outcome.degraded,
        vec![
            "documents",
            "tree",
            "entities",
            "graph",
            "diff",
            "goals",
            "tool_memory",
            "people",
            "chunks",
            "retrieval",
            "profile",
            "source_sync",
            "coding_sessions",
            "scoring",
        ],
        "every probed optional family must be read and reported; `episodic` answers Ok here \
         and must not appear"
    );
    assert!(
        outcome.unreachable.is_empty(),
        "core and recall answered; nothing optional may reach the list apply refuses on"
    );
    assert!(outcome.slow.is_empty(), "an error is not a timeout");
}

/// A driver advertising `people` over an engine that refuses it.
#[cfg(feature = "tinymemory")]
#[derive(Debug, Default)]
struct RefusesPeople(tinymemory_api::null::NullMemoryProvider);

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryPeople for RefusesPeople {
    async fn list_people(
        &self,
        _limit: Option<usize>,
    ) -> std::result::Result<
        Vec<tinymemory_api::provider::RankedPerson>,
        tinymemory_api::error::MemoryError,
    > {
        Err(tinymemory_api::error::MemoryError::Backend(
            "the people index is not enabled on this plan".into(),
        ))
    }
    async fn get_person(
        &self,
        _person_id: &str,
    ) -> std::result::Result<
        Option<tinymemory_api::provider::PersonRecord>,
        tinymemory_api::error::MemoryError,
    > {
        Ok(None)
    }
    async fn resolve_handle(
        &self,
        _handle: &tinymemory_api::provider::PersonHandle,
        _create_if_missing: bool,
    ) -> std::result::Result<
        Option<tinymemory_api::provider::ResolvedPerson>,
        tinymemory_api::error::MemoryError,
    > {
        Ok(None)
    }
    async fn add_handle_alias(
        &self,
        _person_id: &str,
        _handle: &tinymemory_api::provider::PersonHandle,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn score_person(
        &self,
        _person_id: &str,
    ) -> std::result::Result<
        Option<tinymemory_api::provider::PersonScore>,
        tinymemory_api::error::MemoryError,
    > {
        Ok(None)
    }
    async fn record_interaction(
        &self,
        _interaction: &tinymemory_api::provider::PersonInteraction,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
    async fn seed_from_address_book(
        &self,
    ) -> std::result::Result<
        tinymemory_api::provider::AddressBookSeedOutcome,
        tinymemory_api::error::MemoryError,
    > {
        Ok(tinymemory_api::provider::AddressBookSeedOutcome::default())
    }
}

/// Answers, and holds nothing. The control beside `list_people`: an empty
/// answer is success, so a driver advertising `goals` over an engine with no
/// goals yet must not be reported as refusing it.
#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryGoals for RefusesPeople {
    async fn goals(
        &self,
    ) -> std::result::Result<tinymemory_api::goals::GoalsDoc, tinymemory_api::error::MemoryError>
    {
        Ok(tinymemory_api::goals::GoalsDoc::default())
    }
    async fn set_goals(
        &self,
        _goals: tinymemory_api::goals::GoalsDoc,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        Ok(())
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryCore for RefusesPeople {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: tinymemory_api::types::MemoryCategory,
        session_id: Option<&str>,
        taint: tinymemory_api::types::MemoryTaint,
    ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
        self.0
            .store(namespace, key, content, category, session_id, taint)
            .await
    }
    async fn get(
        &self,
        namespace: &str,
        key: &str,
    ) -> std::result::Result<
        Option<tinymemory_api::types::MemoryEntry>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.get(namespace, key).await
    }
    async fn forget(
        &self,
        namespace: &str,
        key: &str,
    ) -> std::result::Result<bool, tinymemory_api::error::MemoryError> {
        self.0.forget(namespace, key).await
    }
    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&tinymemory_api::types::MemoryCategory>,
        session_id: Option<&str>,
    ) -> std::result::Result<
        Vec<tinymemory_api::types::MemoryEntry>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.list(namespace, category, session_id).await
    }
    async fn namespaces(
        &self,
    ) -> std::result::Result<
        Vec<tinymemory_api::types::NamespaceSummary>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.namespaces().await
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryRecall for RefusesPeople {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &tinymemory_api::types::OwnedRecallOpts,
        scope: Option<&tinymemory_api::provider::SourceScope>,
    ) -> std::result::Result<
        Vec<tinymemory_api::types::MemoryEntry>,
        tinymemory_api::error::MemoryError,
    > {
        self.0.recall(query, limit, opts, scope).await
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryPortability for RefusesPeople {
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> std::result::Result<tinymemory_api::provider::ExportPage, tinymemory_api::error::MemoryError>
    {
        self.0.export_page(cursor, limit).await
    }
    async fn import_records(
        &self,
        records: Vec<tinymemory_api::provider::ExportRecord>,
    ) -> std::result::Result<
        tinymemory_api::provider::ImportOutcome,
        tinymemory_api::error::MemoryError,
    > {
        self.0.import_records(records).await
    }
}

#[cfg(feature = "tinymemory")]
#[async_trait]
impl tinymemory_api::provider::MemoryProvider for RefusesPeople {
    fn driver_id(&self) -> &str {
        "refuses-people"
    }
    fn capabilities(&self) -> tinymemory_api::capabilities::Capabilities {
        tinymemory_api::capabilities::Capabilities::mandatory()
            .with(tinymemory_api::capabilities::Capability::People)
            .with(tinymemory_api::capabilities::Capability::Goals)
    }
    async fn health(&self) -> tinymemory_api::health::MemoryHealth {
        tinymemory_api::health::MemoryHealth::Ready
    }
    fn as_people(&self) -> Option<&dyn tinymemory_api::provider::MemoryPeople> {
        Some(self)
    }
    fn as_goals(&self) -> Option<&dyn tinymemory_api::provider::MemoryGoals> {
        Some(self)
    }
}

/// The case the whole issue is written about, in its optional form: the driver
/// advertises `people`, the audit passes because the accessor returns an
/// object, and the engine behind it refuses the read.
///
/// It must be *reported* — an agent will be handed that tool and it will fail —
/// and it must not be reported as `unreachable`, which is what the console apply
/// route refuses on. An engine serving every cycle and failing one tool is not
/// an engine to take away from an operator who has no other one.
///
/// The same driver advertises `goals` over a body that answers and holds
/// nothing, so this pins the other direction too: an empty answer from an
/// optional family is success, exactly as it is for the mandatory ones.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn a_refused_optional_family_is_degraded_not_unreachable() {
    let outcome = probe_engine(&RefusesPeople::default(), std::time::Duration::from_secs(5)).await;
    assert!(outcome.healthy, "the engine answers its health check");
    assert_eq!(outcome.degraded, vec!["people".to_string()]);
    assert!(
        outcome.unreachable.is_empty(),
        "an optional family must never reach the list the apply route refuses on; got {outcome:?}"
    );
    assert!(outcome.slow.is_empty(), "an error is not a timeout");
}

/// `refresh_health` must record the optional verdict too, not only log it:
/// the engine route and both console panels read the descriptor.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn refresh_health_records_degraded_families() {
    let mut overlay = overlay_over(Arc::new(RefusesPeople::default()));
    overlay
        .refresh_health(std::time::Duration::from_secs(5))
        .await;
    assert_eq!(
        overlay.descriptor.degraded_families,
        Some(vec!["people".to_string()])
    );
    assert_eq!(overlay.descriptor.unreachable_families, Some(Vec::new()));
}

/// Builds an overlay over `provider`, so a test can probe through
/// `refresh_health` rather than calling `probe_engine` directly.
#[cfg(feature = "tinymemory")]
fn overlay_over(provider: Arc<dyn tinymemory_api::provider::MemoryProvider>) -> MemoryOverlay {
    let bound = crate::store::memory::BoundMemory::bind(
        Arc::clone(&provider),
        tinymemory::registry::DriverClass::External,
    )
    .expect("bind");
    MemoryOverlay {
        memory: bound.memory(),
        context: bound.context(),
        facts: Some(bound.facts()),
        inbound_context: Some(bound.inbound_context()),
        scratch: Some(bound.scratch()),
        scopes: Some(Arc::new(bound.clone())),
        descriptor: MemoryDescriptor {
            backend: MemoryBackend::Remote,
            driver_id: provider.driver_id().to_string(),
            capabilities: Vec::new(),
            healthy: None,
            unreachable_families: None,
            degraded_families: None,
            slow_families: None,
        },
        probe: Some(provider),
        probe_cache: Arc::default(),
    }
}

/// The console read path must not re-probe on every request.
///
/// The probe is one read per advertised family plus health, so re-running it
/// per `GET` charged a page load — and every re-render and poll behind it — a
/// full round against an engine that may meter each call. The reuse has to
/// survive the clone `AppState::memory_overlay` hands out, which is why the
/// cache is an `Arc` on the overlay rather than a field on the descriptor.
#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn a_fresh_probe_answer_is_reused_instead_of_asked_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct Counting(AtomicUsize);

    #[async_trait]
    impl tinymemory_api::traits::Memory for Counting {
        fn name(&self) -> &str {
            "counting"
        }
        async fn store(
            &self,
            _n: &str,
            _k: &str,
            _c: &str,
            _cat: tinymemory_api::types::MemoryCategory,
            _s: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get(
            &self,
            _n: &str,
            _k: &str,
        ) -> anyhow::Result<Option<tinymemory_api::types::MemoryEntry>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
        async fn forget(&self, _n: &str, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn list(
            &self,
            _n: Option<&str>,
            _c: Option<&tinymemory_api::types::MemoryCategory>,
            _s: Option<&str>,
        ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn namespace_summaries(
            &self,
        ) -> anyhow::Result<Vec<tinymemory_api::types::NamespaceSummary>> {
            Ok(Vec::new())
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
        async fn recall(
            &self,
            _q: &str,
            _l: usize,
            _o: tinymemory_api::types::RecallOpts<'_>,
        ) -> anyhow::Result<Vec<tinymemory_api::types::MemoryEntry>> {
            Ok(Vec::new())
        }
    }

    let counter = Arc::new(Counting::default());
    let provider = Arc::new(tinymemory_api::mandatory::MemoryTraitProvider::new(
        Arc::clone(&counter) as Arc<dyn tinymemory_api::traits::Memory>,
        "counting",
    ));
    let mut overlay = overlay_over(provider);
    let timeout = std::time::Duration::from_secs(5);
    let max_age = std::time::Duration::from_secs(60);

    overlay.refresh_health_within(timeout, max_age).await;
    assert_eq!(counter.0.load(Ordering::SeqCst), 1, "the first read asks");

    // A clone, because that is what the route holds: `memory_overlay()` hands
    // out one, and a cache that did not survive it would buy nothing.
    let mut clone = overlay.clone();
    clone.descriptor.healthy = None;
    clone.descriptor.degraded_families = None;
    clone.refresh_health_within(timeout, max_age).await;
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        1,
        "a probe taken moments ago must be reused, not re-run"
    );
    assert_eq!(
        clone.descriptor.healthy,
        Some(true),
        "a reused answer must still be written onto the descriptor — the route reads it"
    );
    assert_eq!(clone.descriptor.degraded_families, Some(Vec::new()));

    // Boot and apply pass ZERO and always ask: an operator who has just fixed a
    // credential must not be shown the verdict from before the fix.
    overlay.refresh_health(timeout).await;
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);
}
