//! `built_in`'s own inline tests, part 7 of 10. Split out of the
//! single inline `mod tests` block because it exceeded the 750-line file
//! limit; grouped in the original file's order, not by topic (the block
//! covered dozens of unrelated issues with no existing topical boundaries).
//! Shared setup lives in [`super::built_in_test_fixtures`] and
//! [`super::built_in_test_fixtures_2`].

use super::built_in_test_fixtures::*;
use super::built_in_test_fixtures_2::*;
use super::*;
use crate::harness::provider::MockProvider;
use crate::ports::UsageSample;
use crate::ports::types::LedgerEntry;

/// An overlay teammate added through the live company store (the same path
/// the console `POST .../team` route and the orchestrator's `add_agent` tool
/// both write through) reaches the roster on the company's NEXT `ensure` —
/// no restart — mirroring `ensure_rebuilds_when_a_runtime_mcp_server_is_added`.
#[tokio::test]
async fn ensure_rebuilds_when_an_overlay_agent_is_added() {
    let live_store = Arc::new(LiveStore::default());
    let rec = record();
    live_store.save(&rec).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
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
        store: live_store.clone(),
        meter: None,
        workspace_root: dir.path().to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.path().to_path_buf(),
        model_override: None,
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
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
    let pool = HarnessPool::new();

    pool.ensure(&rec, &deps).await.expect("first ensure");
    let before = pool
        .overlay_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");
    assert_eq!(pool.resident_companies().await, 1);
    // The roster is not addressable under "growth" yet.
    assert!(
        pool.run(
            &rec.id,
            "growth",
            "hi",
            &deps,
            crate::runtime::delegation::ChatTarget::default()
        )
        .await
        .is_err(),
        "the overlay teammate must not exist before it is added"
    );

    // Add a teammate directly through the live store — the same write path
    // `AddAgentTool` and the console `POST .../team` route both use.
    let mut updated = rec.clone();
    updated.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "growth".into(),
        name: "Jamie".into(),
        role: "Growth Lead".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    live_store.save(&updated).await.unwrap();

    // Next ensure re-resolves the live store → fingerprint changes → roster
    // rebuilt, so the new teammate reaches the company without a restart.
    pool.ensure(&rec, &deps).await.expect("second ensure");
    let after = pool
        .overlay_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");
    assert_ne!(
        before, after,
        "adding a teammate must change the overlay fingerprint"
    );
    assert_eq!(
        pool.resident_companies().await,
        1,
        "same company, rebuilt in place"
    );

    let reply = pool
        .run(
            &rec.id,
            "growth",
            "hello-marker",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("the new teammate is addressable on the very next turn")
        .reply;
    assert!(reply.contains("hello-marker"), "got {reply:?}");

    // A third ensure with no further change is a no-op (fingerprint stable).
    pool.ensure(&rec, &deps).await.expect("third ensure");
    assert_eq!(pool.overlay_fingerprint_of(&rec.id).await, Some(after));
}

/// Issue #1455: the roster's approval policy is pinned to the cycle-start
/// snapshot the native gate was re-applied from, so a console override that
/// lands mid-turn (after the runtime's store load, before the harness's own
/// refresh) cannot reach the harness gate a turn early. The override is not
/// lost — it moves the fingerprint on the NEXT cycle, the same boundary the
/// native gate moves on.
#[tokio::test]
async fn a_cycle_policy_snapshot_wins_over_a_mid_turn_store_edit() {
    let live_store = Arc::new(LiveStore::default());
    let mut rec = record();
    rec.overlay_policy = Some(fp_entry_full(Some("supervised"), None, None, None));
    live_store.save(&rec).await.unwrap();

    let mut fx = fixture();
    fx.deps.store = live_store.clone();
    let pool = HarnessPool::new();

    // The snapshot the runtime loads at the top of the cycle and re-applies
    // to the native gate.
    let snapshot = rec.effective_policy();

    // First cycle: the roster builds against the snapshot.
    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("first ensure");
    let pinned = pool
        .policy_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");

    // A redundant ensure with the same snapshot is a no-op — the stability
    // direction the mid-turn assertion below is read against.
    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("redundant ensure");
    assert_eq!(pool.policy_fingerprint_of(&rec.id).await, Some(pinned));

    // Mid-window PUT: the store now holds a `full` override. The brain's
    // refresh picks this record up, but the cycle still carries the old
    // snapshot — so the roster must not rebuild against `full` a turn early.
    let mut edited = rec.clone();
    edited.overlay_policy = Some(fp_entry_full(Some("full"), None, None, None));
    live_store.save(&edited).await.unwrap();

    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("mid-turn ensure");
    assert_eq!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(pinned),
        "a mid-turn store edit must not reach the roster while the cycle still \
         carries the old snapshot"
    );

    // The NEXT cycle captures the new policy: the fingerprint moves, the
    // roster rebuilds — deferred to the same boundary the native gate moves
    // on, so the change is applied, just not a turn early.
    let next = edited.effective_policy();
    pool.ensure_with_policy(&rec, &fx.deps, &next)
        .await
        .expect("next cycle");
    assert_ne!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(pinned),
        "the new policy must reach the roster on the next cycle"
    );
}

/// The codex P1 regression (commit 11a1f12ed): a manifest `[policy]` edit
/// with no stored override must still move the roster's policy fingerprint.
///
/// `ensure_with_policy` previously fingerprinted the *synthesized relative
/// override* against the manifest. When effective == manifest — the
/// no-override case, or a redundant override a rebuild carried and then
/// cleared — that synthesis is all-`None`, the empty fingerprint, so the
/// cache key never moved and the next `ensure` reused the cached roster
/// (with its old `ApprovalPolicy`) while the native gate already enforced
/// the new tier. Fingerprinting the effective policy values closes it.
#[tokio::test]
async fn a_manifest_policy_edit_rebuilds_the_roster_with_no_override() {
    let live_store = Arc::new(LiveStore::default());
    let rec = record();
    live_store.save(&rec).await.unwrap();

    let mut fx = fixture();
    fx.deps.store = live_store.clone();
    let pool = HarnessPool::new();

    // First cycle: manifest `[policy] mode = "full"`, no override. The
    // snapshot equals the manifest's own policy.
    let snapshot = rec.effective_policy();
    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("first ensure");
    let pinned = pool
        .policy_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");

    // A redundant ensure with the same snapshot is a no-op — the stability
    // direction the manifest edit below is read against.
    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("redundant ensure");
    assert_eq!(pool.policy_fingerprint_of(&rec.id).await, Some(pinned));

    // Version control edits the manifest tier to `readonly`. No override is
    // stored, so the effective policy IS the manifest itself.
    let mut edited = rec.clone();
    edited.manifest.policy.mode = "readonly".to_string();
    live_store.save(&edited).await.unwrap();

    // The next cycle captures the new effective policy: the fingerprint
    // must move, or the cached roster's `ApprovalPolicy` (still `full`)
    // keeps governing harness tool calls while the native gate already
    // enforces `readonly`.
    let next = edited.effective_policy();
    pool.ensure_with_policy(&edited, &fx.deps, &next)
        .await
        .expect("next cycle");
    assert_ne!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(pinned),
        "a manifest [policy] edit must move the fingerprint even with no override"
    );
}

/// The workflow-runner regression (issue #1455): a plain `ensure` while a
/// cycle snapshot is pinned cannot adopt a mid-turn console override a turn
/// early.
///
/// A cycle pins the roster to the policy snapshot the native gate was
/// re-applied from. The workflow runner drives turns from a spawned task
/// outside the cycle serial lock and calls plain `ensure`, so without the
/// pool remembering the pin it would re-resolve the live store, see the
/// mid-window `full` override, and rebuild the roster against it — running
/// one turn with the harness gate auto-approving what the native gate still
/// parks. The pin is what keeps the plain ensure on the cycle's cadence.
#[tokio::test]
async fn a_live_ensure_cannot_clobber_a_pinned_cycle_snapshot() {
    let live_store = Arc::new(LiveStore::default());
    let mut rec = record();
    rec.overlay_policy = Some(fp_entry_full(Some("supervised"), None, None, None));
    live_store.save(&rec).await.unwrap();

    let mut fx = fixture();
    fx.deps.store = live_store.clone();
    let pool = HarnessPool::new();

    let snapshot = rec.effective_policy();

    // Cycle 1: the roster pins the strict snapshot.
    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("cycle ensure");
    let pinned = pool
        .policy_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");

    // Mid-window PUT: the store now holds a `full` override, still unseen
    // by the cycle's snapshot.
    let mut edited = rec.clone();
    edited.overlay_policy = Some(fp_entry_full(Some("full"), None, None, None));
    live_store.save(&edited).await.unwrap();

    // The workflow runner's plain ensure fires while the pin is active. It
    // must rebuild against the pin, not the live `full` override.
    pool.ensure(&rec, &fx.deps).await.expect("workflow ensure");
    assert_eq!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(pinned),
        "a plain ensure must not adopt a mid-cycle override a turn early"
    );

    // The NEXT cycle captures the new policy: the fingerprint moves, the
    // roster rebuilds — the change lands, just at the native gate's own
    // boundary.
    let next = edited.effective_policy();
    pool.ensure_with_policy(&rec, &fx.deps, &next)
        .await
        .expect("next cycle");
    assert_ne!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(pinned),
        "the new policy must reach the roster on the next cycle"
    );

    // Cycle 2 ends: the pin is released, so a standalone workflow turn
    // between cycles rebuilds against the live override the store already
    // holds instead of a snapshot that would otherwise outlive its cycle.
    pool.end_cycle(&rec.id).await;
    pool.ensure(&rec, &fx.deps)
        .await
        .expect("post-cycle ensure");
    assert_eq!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(effective_policy_fingerprint(&edited.effective_policy())),
        "after end_cycle a plain ensure must adopt the live override"
    );
}

/// The drop-guard half of the release (issue #1455): a cycle cancelled or
/// unwound through a panic after installing its pin cannot await
/// `end_cycle`, but the synchronous `release_policy_pin_sync` — what the
/// guard calls from `Drop` — must clear the pin just the same, so a
/// standalone workflow turn between cycles rebuilds against the live
/// override rather than a snapshot the abandoned cycle left behind.
#[tokio::test]
async fn a_sync_pin_release_restores_the_live_policy_axis() {
    let live_store = Arc::new(LiveStore::default());
    let mut rec = record();
    rec.overlay_policy = Some(fp_entry_full(Some("supervised"), None, None, None));
    live_store.save(&rec).await.unwrap();

    let mut fx = fixture();
    fx.deps.store = live_store.clone();
    let pool = HarnessPool::new();

    let snapshot = rec.effective_policy();

    // The cycle pins the strict snapshot, exactly as it does on entry.
    pool.ensure_with_policy(&rec, &fx.deps, &snapshot)
        .await
        .expect("cycle ensure");
    let pinned = pool
        .policy_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");

    // Mid-window PUT, then the cycle future is dropped without end_cycle:
    // the release must come from the drop guard's sync path.
    let mut edited = rec.clone();
    edited.overlay_policy = Some(fp_entry_full(Some("full"), None, None, None));
    live_store.save(&edited).await.unwrap();
    pool.release_policy_pin_sync(&rec.id);

    pool.ensure(&rec, &fx.deps).await.expect("post-drop ensure");
    assert_eq!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(effective_policy_fingerprint(&edited.effective_policy())),
        "after a sync release a plain ensure must adopt the live override"
    );
    assert_ne!(
        pool.policy_fingerprint_of(&rec.id).await,
        Some(pinned),
        "the abandoned cycle's snapshot must not outlive its release"
    );
}

/// End-to-end capability gating: a plan budgeting `shell` at 100 tokens grants
/// the shell tools while spend is under budget; once a recorded turn pushes
/// period spend past the threshold, the very next `ensure` rebuilds the roster
/// with the shell namespace dropped — while intrinsic tools (memory) and the
/// ungated `files` namespace survive. Mirrors the MCP-freshness test shape.
#[tokio::test]
async fn ensure_gates_shell_tools_once_the_token_budget_is_crossed() {
    let dir = tempfile::tempdir().unwrap();
    let meter = Arc::new(RecordingMeter::default());
    let plan = crate::harness::capability_budget::CapabilityPlan {
        period: crate::harness::capability_budget::BudgetPeriod::Daily,
        budgets: std::collections::BTreeMap::from([("shell".to_string(), 100u64)]),
        total_budget: None,
    };
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
        meter: Some(meter.clone()),
        workspace_root: dir.path().to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.path().to_path_buf(),
        model_override: None,
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
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
        plan: Some(plan),
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
    let pool = HarnessPool::new();
    let rec = granting_record();

    // First ensure: 0 spend < 100 → shell granted.
    pool.ensure(&rec, &deps).await.expect("first ensure");
    let before_fp = pool
        .capability_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");
    let before = ceo_tool_names(&pool, &rec.id).await;
    assert!(before.contains(&"shell".to_string()), "got {before:?}");
    assert!(
        before.contains(&"read_workspace_state".to_string()),
        "got {before:?}"
    );
    // `memory_store`/`memory_recall` are currently withheld altogether
    // (see `harness::build::memory_tools`'s doc comment) — openhuman
    // removed the constructor seam that let either tool act on a
    // company's own `ContextStore` rather than one shared,
    // unconfigured store. `file_read` is this test's example of an
    // intrinsic, ungated tool instead.
    assert!(
        before.contains(&"file_read".to_string()),
        "ungated files namespace must be present: {before:?}"
    );

    // Record a turn that burns 150 inference tokens — past the 100 budget.
    meter
        .record(
            &rec.id,
            &UsageSample {
                at_millis: crate::ports::now_millis(),
                agent: "ceo".into(),
                provider: "managed".into(),
                input_tokens: 100,
                output_tokens: 50,
                cached_input_tokens: 0,
                cost_usd: 0.0,
                kind: crate::ports::SampleKind::Inference,
                run_id: None,
                model: None,
            },
        )
        .await
        .unwrap();

    // Second ensure: 150 >= 100 → shell exhausted → roster rebuilt without it.
    pool.ensure(&rec, &deps).await.expect("second ensure");
    let after_fp = pool
        .capability_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");
    assert_ne!(
        before_fp, after_fp,
        "crossing the budget must change the capability fingerprint"
    );
    assert_eq!(pool.resident_companies().await, 1, "rebuilt in place");

    let after = ceo_tool_names(&pool, &rec.id).await;
    assert!(
        !after.contains(&"shell".to_string()),
        "shell must be gated off once exhausted: {after:?}"
    );
    assert!(
        !after.contains(&"read_workspace_state".to_string()),
        "the whole shell namespace drops: {after:?}"
    );
    assert!(
        after.contains(&"file_read".to_string()),
        "ungated files namespace survives gating: {after:?}"
    );

    // Third ensure with no new spend → no rebuild (fingerprint stable).
    pool.ensure(&rec, &deps).await.expect("third ensure");
    assert_eq!(
        pool.capability_fingerprint_of(&rec.id).await,
        Some(after_fp)
    );
}

/// With no plan wired, the capability fingerprint is stable across ensures —
/// gating stays off, byte-identical to Cell A (no rebuild on this axis).
#[tokio::test]
async fn ensure_without_a_plan_never_gates() {
    let fx = fixture();
    let pool = HarnessPool::new();
    let rec = record();
    pool.ensure(&rec, &fx.deps).await.expect("first ensure");
    let fp = pool
        .capability_fingerprint_of(&rec.id)
        .await
        .expect("fingerprinted");
    pool.ensure(&rec, &fx.deps).await.expect("second ensure");
    assert_eq!(
        pool.capability_fingerprint_of(&rec.id).await,
        Some(fp),
        "no plan → stable fingerprint → no capability-driven rebuild"
    );
}

#[tokio::test]
async fn monthly_inference_cap_refuses_a_non_discoverable_company_after_spend() {
    let dir = tempfile::tempdir().expect("temporary workspace");
    let store = Arc::new(crate::store::FsCompanyStore::new(dir.path()));
    let provider = Arc::new(ScriptedProvider::new(vec![Ok("model-ran".to_string())]));
    let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
    deps.store = store.clone();
    deps.provider = provider.clone();

    let mut rec = record();
    rec.manifest.place.discoverable = false;
    rec.manifest.budget.monthly_usd = Some(1.0);
    let prior_spend = LedgerEntry {
        at_millis: crate::ports::now_millis(),
        kind: "inference.spend".to_string(),
        amount_usd: -1.0,
        memo: "prior inference".to_string(),
    };
    store.save(&rec).await.expect("company is persisted");
    store
        .append_ledger(&rec.id, prior_spend)
        .await
        .expect("prior inference spend is persisted");

    let pool = HarnessPool::new();
    pool.ensure(&rec, &deps).await.expect("roster builds");
    let outcome = pool
        .run(
            &rec.id,
            "ceo",
            "must-not-run",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a budget refusal is an ordinary outcome");

    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a company at its monthly cap must not make another inference call"
    );
    assert_eq!(
        outcome.reply,
        "This company has reached its monthly spend cap of $1.00 — dispatch is paused until the month resets.",
        "the refusal must explain the company-wide monthly cap"
    );
}
