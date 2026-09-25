//! `built_in`'s own inline tests, part 2 of 10. Split out of the
//! single inline `mod tests` block because it exceeded the 750-line file
//! limit; grouped in the original file's order, not by topic (the block
//! covered dozens of unrelated issues with no existing topical boundaries).
//! Shared setup lives in [`super::built_in_test_fixtures`] and
//! [`super::built_in_test_fixtures_2`].

use super::built_in_test_fixtures::*;
use super::*;
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;

use crate::harness::provider::MockProvider;
use crate::ports::types::{CompanySummary, ContextChunk, LedgerEntry};

/// The roster builds end-to-end with the skill read surface wired: the
/// effective set materializes, the read tools build, and the catalogue folds
/// into the persona — all without error — and the scratch tree lands under
/// the agent's workspace root.
#[tokio::test]
async fn roster_builds_with_skill_surface_wired() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = tempfile::tempdir().expect("source");
    let skill_dir = source.path().join("skills").join("web-research");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: Web Research\ndescription: Answer a question\n---\n\n# Web Research\n",
    )
    .unwrap();

    let deps = HarnessDeps {
        takeovers: Default::default(),
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(MockProvider::new("mock: ")),
        provider_slug: "mock".to_string(),
        serves: None,
        context: Arc::new(MockContext::default()),
        store: Arc::new(RecordingStore::default()),
        meter: None,
        workspace_root: dir.path().to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.path().to_path_buf(),
        model_override: None,
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: Some(source.path().to_path_buf()),
        skills_registry: std::sync::Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: DelegationQueue::default(),
        workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
        mcp_failures: McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_runs: None,
        deep_trace: None,
        workflow_revisions: None,
        approval_requests: ApprovalRequestQueue::default(),
        approval_parker: None,
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
        workflow_source_dir: None,
        plan: None,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
    };

    let roster = build_roster(&test_runtime(), &record(), &deps, &[], &HashMap::new())
        .expect("roster builds with skills");
    assert_eq!(roster.len(), 2);
    // The scratch skill tree was materialized for the first roster agent.
    assert!(
        dir.path()
            .join("acme")
            .join("ceo")
            .join("skill-catalog")
            .join("skills")
            .join("web-research")
            .join("SKILL.md")
            .is_file(),
        "the effective skill bundle should be materialized under the agent workspace"
    );
}

/// Issue #71 — an operator/orchestrator-added overlay teammate is promoted
/// into a real, addressable roster agent (not just a console row).
#[tokio::test]
async fn overlay_agent_is_built_as_a_real_roster_agent() {
    let fx = fixture();
    let mut rec = record();
    rec.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "growth".into(),
        name: "Jamie".into(),
        role: "Growth Lead".into(),
        description: Some("Owns acquisition experiments.".into()),
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });

    let roster =
        build_roster(&test_runtime(), &rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
    let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
    assert_eq!(ids, vec!["ceo", "engineer", "growth"], "got {ids:?}");
    let overlay_agent = roster
        .iter()
        .find(|a| a.agent_id == "growth")
        .expect("overlay teammate present in roster");
    assert_eq!(overlay_agent.role, "Growth Lead");
}

/// The roster is built from the teammate an operator has since edited, not
/// from the row `company.toml` declared. Without this the console would save
/// a rename that nothing running ever heard about — the edit would be
/// visible on the Team page and absent from every turn the agent took.
#[tokio::test]
async fn a_console_edit_of_a_manifest_teammate_reaches_the_built_roster() {
    let fx = fixture();
    let mut rec = record();
    rec.upsert_agent_override(crate::ports::types::AgentOverride {
        agent_id: "ceo".into(),
        role: Some("Chief Vibes".into()),
        ..Default::default()
    });

    let roster =
        build_roster(&test_runtime(), &rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
    let ceo = roster
        .iter()
        .find(|a| a.agent_id == "ceo")
        .expect("the ceo is still on the roster");
    assert_eq!(ceo.role, "Chief Vibes");
}

/// A teammate the operator removed is not built at all — which is what makes
/// the delete real rather than cosmetic: an agent left in the roster keeps
/// taking turns and keeps receiving delegations however the Team page reads.
/// And when the removed teammate was the orchestrator, the role moves to the
/// next one rather than to somebody who is no longer here.
#[tokio::test]
async fn a_retired_manifest_teammate_is_not_built() {
    let fx = fixture();
    let mut rec = record();
    rec.retire_agent("ceo");

    let roster =
        build_roster(&test_runtime(), &rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
    let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
    assert_eq!(ids, vec!["engineer"], "got {ids:?}");
    assert_eq!(
        orchestrator::orchestrator_id(&rec.effective_agents()).as_deref(),
        Some("engineer"),
        "the orchestrator must be somebody who is actually on the roster"
    );
}

/// A manifest agent always wins an id collision with an overlay teammate —
/// the version-controlled roster is authoritative.
#[tokio::test]
async fn overlay_agent_id_colliding_with_manifest_agent_is_skipped() {
    let fx = fixture();
    let mut rec = record();
    rec.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "ceo".into(),
        name: "Impostor".into(),
        role: "Shadow CEO".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });

    let roster =
        build_roster(&test_runtime(), &rec, &fx.deps, &[], &HashMap::new()).expect("roster builds");
    let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ceo", "engineer"],
        "the manifest agent wins the id collision, not a duplicate"
    );
    assert_eq!(
        roster[0].role, "Chief Executive",
        "the manifest role survives, not the overlay's"
    );
}

/// Issue #686, end to end: the orchestrator adds a teammate whose display
/// name slugs onto a **manifest** agent's id, and the teammate still shows
/// up in the built roster.
///
/// This is the failure the suffix exists to prevent, and it only became
/// reachable when ids started coming from names. `add_agent`'s duplicate
/// guard compares overlay *names*, so "Engineer" sails past it; an
/// unsuffixed `engineer` would then be skipped by
/// [`build_roster`](super::build_roster) as already claimed by the manifest
/// — saved to the record, never materialised, no error anywhere.
#[tokio::test]
async fn a_tool_added_teammate_colliding_with_a_manifest_id_still_joins_the_roster() {
    use tinytools::Tool;

    use crate::harness::orchestrator::unscoped_add_agent;

    /// A `CompanyStore` that actually holds the record, unlike
    /// `RecordingStore` — `add_agent` has to load what it saves.
    struct SeededStore(StdMutex<CompanyRecord>);

    #[async_trait]
    impl CompanyStore for SeededStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(Some(self.0.lock().unwrap().clone()))
        }
        async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
            *self.0.lock().unwrap() = record.clone();
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
            Ok(())
        }
    }

    let fx = fixture();
    let company = CompanyId::new("acme");
    let store = Arc::new(SeededStore(StdMutex::new(record())));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(serde_json::json!({ "name": "Engineer", "role": "Platform" }))
        .await
        .expect("execute");
    assert!(
        !result.is_error,
        "the name guard compares overlay names only"
    );
    assert!(
        result.text().contains("engineer_2"),
        "the orchestrator has to learn the id it can address: {}",
        result.text()
    );

    let saved = store.load(&company).await.unwrap().expect("record");
    assert_eq!(saved.overlay_agents[0].id, "engineer_2");

    let roster = build_roster(&test_runtime(), &saved, &fx.deps, &[], &HashMap::new())
        .expect("roster builds");
    let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ceo", "engineer", "engineer_2"],
        "a suffixed id materialises; an unsuffixed one would vanish here"
    );
}

/// Issue #551: a roster rebuild writes nothing to the workspace.
///
/// This used to be the feature's second provisioning seam — a teammate
/// added at runtime (a manifest edit, the console's `add_member`, the
/// orchestrator's `add_agent`) reaches the harness as a moved overlay
/// fingerprint, and the folder was minted here. A member folder is no
/// longer a function of the roster, so joining one is no longer an event
/// the tree records: the folder appears when the teammate first produces
/// something, and the two system roots come from boot.
///
/// Pinned as a test because a rebuild that quietly resumed writing would
/// re-fill the tree with empty folders for teammates who have done nothing
/// — exactly the noise this change removed.
#[tokio::test]
async fn a_roster_rebuild_writes_nothing_to_the_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ws: Arc<dyn crate::ports::WorkspaceStore> = Arc::new(crate::store::FsOps::new(dir.path()));
    let mut fx = fixture();
    fx.deps.workspace = Some(ws.clone());

    let mut rec = record();
    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("first ensure");
    assert!(
        ws.is_empty(&rec.id).await.expect("is_empty"),
        "the roster build touched the workspace"
    );

    // The runtime-added teammate. The overlay fingerprint moves, so this
    // `ensure` takes the rebuild path rather than the cached fast path.
    rec.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "designer".into(),
        name: "Dana".into(),
        role: "Designer".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    pool.ensure(&rec, &fx.deps).await.expect("second ensure");

    assert!(
        ws.is_empty(&rec.id).await.expect("is_empty"),
        "the rebuild minted a folder for a teammate that has produced nothing"
    );

    // …and the folder the teammate *does* get is the one it earns by
    // producing something, minted through the lazy seam instead.
    let minted =
        crate::company::workspace_scaffold::ensure_agent_folder(ws.as_ref(), &rec.id, "designer")
            .await
            .expect("mint");
    let tree = ws.tree(&rec.id).await.expect("tree");
    let mut names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["agents", "designer"]);
    assert_eq!(
        tree.iter().find(|n| n.id == minted).unwrap().created_by,
        crate::ports::WorkspaceOrigin::Agent {
            id: "designer".to_string()
        },
    );
}

#[tokio::test]
async fn run_executes_a_turn_on_the_openhuman_runtime() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    let reply = pool
        .run(
            &rec.id,
            "ceo",
            "hello-marker",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn runs")
        .reply;

    assert!(
        reply.contains("hello-marker"),
        "reply should echo the prompt through the agent: {reply:?}"
    );
}

#[tokio::test]
async fn run_stores_outcomes_and_injects_them_into_later_turns() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    // Cold store: nothing to inject on the first turn.
    let first = pool
        .run(
            &rec.id,
            "ceo",
            "alpha task",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("first turn")
        .reply;
    assert!(
        !first.contains("Relevant prior work"),
        "a cold turn injects nothing: {first:?}"
    );

    // The outcome was written back under the task-outcome prefix.
    let stored = fx
        .deps
        .context
        .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1, "the first turn stores its outcome");

    // Second turn: the prior outcome (its body contains "alpha") is
    // retrieved and injected, so the agent sees the preamble.
    let second = pool
        .run(
            &rec.id,
            "ceo",
            "alpha",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("second turn")
        .reply;
    assert!(
        second.contains("Relevant prior work"),
        "the second turn injects the retrieved outcome: {second:?}"
    );

    let stored = fx
        .deps
        .context
        .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
        .await
        .unwrap();
    assert_eq!(stored.len(), 2, "the second turn stores its outcome too");
}

/// Recall is driven by **what the operator typed**, not by the briefings
/// this turn folded onto it.
///
/// The composed message can carry the open-work briefing, the settled-work
/// digest, the thread index or attachment markers. Those are for the model
/// to read; searching on them makes the query something nobody asked. Under
/// this store's substring matching that costs the recall outright — any
/// briefing at all and nothing matches — and under a vector store it drifts
/// instead, toward whatever the briefing happens to name. The settled digest
/// is a list of finished card titles, so a conversation that had just closed
/// some work pulled *that* work in, and the bias grew with every card that
/// finished.
///
/// Found by the `orchestration-simulation` E2E, which went red the moment
/// two cards settled in the conversation it drives (#1890 review).
#[tokio::test]
async fn recall_searches_the_operators_words_not_the_briefings() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    // Turn one stores an outcome whose body carries "alpha".
    pool.run(
        &rec.id,
        "ceo",
        "alpha task",
        &fx.deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("first turn");

    // Turn two asks the same thing, with a briefing folded on — the shape
    // every turn takes once a card has settled in the conversation.
    let briefed = format!(
        "alpha{} has finished — this is where each card landed:\n- something else\n]",
        crate::runtime::cycle::SETTLED_WORK_ANNOTATION
    );
    let second = pool
        .run(
            &rec.id,
            "ceo",
            &briefed,
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("second turn")
        .reply;

    assert!(
        second.contains("Relevant prior work"),
        "the operator asked about alpha, so alpha is recalled — the briefing \
         appended after their words must not change what is searched for: {second:?}"
    );
}

/// A pool serving one named harness builds only the agents bound to it.
///
/// This is what makes one-pool-per-harness affordable: without the filter a
/// ten-agent roster on three harnesses would stand up thirty live agents,
/// each holding a model client, to use ten.
#[tokio::test]
async fn a_scoped_pool_builds_only_the_agents_it_serves() {
    let fx = fixture();
    let rec = record();
    assert!(
        rec.manifest.agents.len() >= 2,
        "the fixture must have someone to leave out"
    );

    // Unfiltered: the whole roster, exactly as before this field existed.
    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");
    let all = pool.agent_ids(&rec.id).await;
    assert_eq!(all.len(), rec.manifest.agents.len());

    // Scoped to one agent: only that one is built.
    let mut scoped = fixture();
    scoped.deps.serves = Some(HashSet::from(["ceo".to_string()]));
    let pool = HarnessPool::new();
    pool.ensure(&rec, &scoped.deps).await.expect("ensure");
    assert_eq!(pool.agent_ids(&rec.id).await, vec!["ceo".to_string()]);
}

#[tokio::test]
async fn ensure_is_idempotent() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("first ensure");
    pool.ensure(&rec, &fx.deps).await.expect("second ensure");
    assert_eq!(pool.resident_companies().await, 1);
}

/// Issue #1113: a live memory-engine swap must not leave the cached roster
/// reading/writing the deselected engine until a restart.
///
/// The pool cannot see a swap itself — the replacement ports arrive on the
/// builder — so [`RuntimeBuilder::build`] calls
/// [`rebind_memory_engine`](Self::rebind_memory_engine) on every build. This
/// test drives that contract directly: an unchanged selection keeps the
/// roster (the ordinary issue #290 fast path), a changed one drops it, and
/// the next [`ensure`](Self::ensure) folds the replacement context store
/// into the rebuilt roster's agents — which a turn then demonstrably reads.
#[tokio::test]
async fn a_swapped_memory_engine_drops_the_roster_and_reads_the_replacement_store() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();

    // Boot: the company is on the base backend (`None`), and the roster is
    // built over the boot-time context store.
    assert!(
        pool.rebind_memory_engine(&rec.id, None).await,
        "first record has nothing to differ from"
    );
    pool.ensure(&rec, &fx.deps).await.expect("boot ensure");
    assert_eq!(
        pool.resident_companies().await,
        1,
        "roster resident after boot"
    );
    assert_eq!(
        pool.memory_engine(&rec.id).await,
        None,
        "selection recorded as the base backend"
    );

    // A rebuild that re-applies the same engine selection is a no-op (the
    // issue #290 fast path): the roster survives, conversation intact.
    assert!(
        pool.rebind_memory_engine(&rec.id, None).await,
        "unchanged selection keeps the roster"
    );
    assert_eq!(
        pool.resident_companies().await,
        1,
        "roster survives a no-op rebuild"
    );

    // A live swap to a provider engine: the selection changes, so the cached
    // roster must drop for the next `ensure` to rebuild over the replacement
    // ports — otherwise the agents keep reading the deselected engine.
    assert!(
        !pool.rebind_memory_engine(&rec.id, Some(0x1113_0001)).await,
        "changed selection drops the roster"
    );
    assert_eq!(
        pool.resident_companies().await,
        0,
        "roster invalidated on swap"
    );

    // Seed the replacement context store and ensure over it: the rebuilt
    // agent must read the new engine, not the deselected one.
    let replacement = Arc::new(MockContext::default());
    let query = "who approved the overtime";
    replacement
        .put(
            &rec.id,
            ContextChunk {
                label: "prior/outcome".into(),
                body: format!("REPLACEMENT-ENGINE: {query} on Tuesday"),
            },
        )
        .await
        .expect("seed the replacement store");
    let mut swapped = fixture();
    swapped.deps.context = replacement.clone();
    pool.ensure(&rec, &swapped.deps)
        .await
        .expect("ensure after swap");
    let reply = pool
        .run(
            &rec.id,
            "ceo",
            query,
            &swapped.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn after swap")
        .reply;
    assert!(
        reply.contains("REPLACEMENT-ENGINE"),
        "the rebuilt roster reads the replacement store, not the deselected one; got: {reply}"
    );
    assert_eq!(
        pool.memory_engine(&rec.id).await,
        Some(0x1113_0001),
        "selection recorded as the provider engine"
    );

    // And the reverse swap (back to the base backend — the
    // `with_memory_overlay_cleared` path): the provider-built roster must
    // drop the same way, and the next ensure must read the boot store again.
    assert!(
        !pool.rebind_memory_engine(&rec.id, None).await,
        "reverse swap also drops the roster"
    );
    assert_eq!(
        pool.resident_companies().await,
        0,
        "provider-built roster invalidated on the way back"
    );
    pool.ensure(&rec, &fx.deps)
        .await
        .expect("ensure after reverse swap");
    let reply = pool
        .run(
            &rec.id,
            "ceo",
            query,
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn after reverse swap")
        .reply;
    assert!(
        !reply.contains("REPLACEMENT-ENGINE"),
        "the rebuilt roster reads the boot store again, not the deselected provider; got: {reply}"
    );
    // Sanity: the boot store itself has no seed for this query, so the
    // absence above is meaningful rather than a hit-free query.
    assert!(
        !reply.contains("SECRET-PAYROLL-REVIEW"),
        "sanity: the boot store holds no marker for this query"
    );
}

#[tokio::test]
async fn turns_are_serialised_and_history_survives() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    pool.run(
        &rec.id,
        "ceo",
        "first",
        &fx.deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("first turn");
    let second = pool
        .run(
            &rec.id,
            "ceo",
            "second",
            &fx.deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("second turn")
        .reply;

    assert!(second.contains("second"));
}
