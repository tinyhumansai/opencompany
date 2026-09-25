//! Shared fixtures for `built_in`'s own inline tests (part 2 of 2):
//! mock stores, scripted providers, and the `Fixture`/`manifest`/`record`
//! builders test groups reach for. Split out of the inline `mod tests` block,
//! and split further across files because the combined fixtures exceeded the
//! 750-line file limit.

use super::*;
pub(super) use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

pub(super) use crate::company::CompanyManifest;
pub(super) use crate::harness::provider::MockProvider;
pub(super) use crate::ports::UsageSample;
pub(super) use crate::ports::types::{CompanySummary, LedgerEntry};
// The two-level resolver. Test-only now: the roster build goes through
// `agent_scoped_grants`, and these tests assert the desk-less case still
// resolves identically to what shipped before desks could scope tools.
use super::built_in_test_fixtures::*;

/// Build a single [`CompanyAgent`] over a scripted provider so the wrapper can
/// be exercised directly (its retry logic is the unit under test).
pub(super) fn scripted_agent(
    outcomes: Vec<Result<String, String>>,
) -> (Arc<CompanyAgent>, HarnessDeps) {
    scripted_agent_over(ScriptedProvider::new(outcomes))
}

/// As [`scripted_agent`], over an already-configured provider — the seam a
/// case that needs the provider to *report usage* builds through.
pub(super) fn scripted_agent_over(provider: ScriptedProvider) -> (Arc<CompanyAgent>, HarnessDeps) {
    scripted_agent_over_arc(Arc::new(provider) as Arc<dyn HarnessModel>)
}

/// Build a [`CompanyAgent`] over a scripted provider and return a shared
/// reference to the provider so tests can inspect per-call captures
/// (`ScriptedProvider::captured`) after the run (issue #1871).
pub(super) fn scripted_agent_with_capture(
    outcomes: Vec<Result<String, String>>,
) -> (Arc<CompanyAgent>, HarnessDeps, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(outcomes));
    let capture = Arc::clone(&provider);
    let (agent, deps) = scripted_agent_over_arc(provider as Arc<dyn HarnessModel>);
    (agent, deps, capture)
}

/// Internal: build a [`CompanyAgent`] with the given `Arc<dyn HarnessModel>`
/// provider, shared by [`scripted_agent_over`] and [`scripted_agent_with_capture`]
/// so both paths use exactly the same `HarnessDeps` structure.
fn scripted_agent_over_arc(provider: Arc<dyn HarnessModel>) -> (Arc<CompanyAgent>, HarnessDeps) {
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = HarnessDeps {
        takeovers: Default::default(),
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider,
        provider_slug: "scripted".to_string(),
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
    let roster =
        build_roster(&test_runtime(), &record(), &deps, &[], &HashMap::new()).expect("roster");
    // Keep the tempdir alive for the agent's workspace by leaking it into the
    // test's lifetime — the process ends the test anyway.
    std::mem::forget(dir);
    (roster.into_iter().next().expect("one agent"), deps)
}

/// The leaf as the vendored harness actually writes it, taken verbatim from
/// the failing run in issue #1680.
pub(super) fn ceiling_error() -> anyhow::Error {
    anyhow::anyhow!(
        "tinyagents harness run failed; model error; run timed out; model call for run \
         'agent_turn' exceeded its remaining wall-clock budget (56636 ms)"
    )
}

/// The same failure as [`ceiling_error`], but arriving as an `anyhow`
/// context chain instead of one flattened string. Nothing guarantees the
/// vendored crate keeps flattening it, and `is_wall_clock_ceiling` already
/// assumes it might not.
pub(super) fn chained_ceiling_error() -> anyhow::Error {
    use anyhow::Context as _;
    Err::<(), _>(anyhow::anyhow!(
        "model call for run 'agent_turn' exceeded its remaining wall-clock budget (56636 ms)"
    ))
    .context("run timed out")
    .context("model error")
    .context("tinyagents harness run failed")
    .unwrap_err()
}

/// In-memory secret store so `ensure` can re-resolve the runtime MCP index.
#[derive(Default)]
pub(super) struct MemSecrets {
    pub(super) map: StdMutex<std::collections::HashMap<String, String>>,
}

#[async_trait]
impl SecretStore for MemSecrets {
    async fn get(
        &self,
        _c: &CompanyId,
        key: &str,
    ) -> crate::Result<Option<crate::ports::types::SecretValue>> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(key)
            .map(|v| crate::ports::types::SecretValue(v.clone())))
    }
    async fn set(
        &self,
        _c: &CompanyId,
        key: &str,
        value: crate::ports::types::SecretValue,
    ) -> crate::Result<()> {
        self.map.lock().unwrap().insert(key.to_string(), value.0);
        Ok(())
    }
}

/// An in-memory `SkillStateStore` whose delta set a test can mutate between
/// two `ensure` calls — the same way the console Skills tab authors, edits,
/// enables, or disables a skill — so the freshness gate can be observed
/// reacting with no restart.
#[derive(Default)]
pub(super) struct MemSkills {
    pub(super) deltas: StdMutex<Vec<SkillState>>,
}

#[async_trait]
impl SkillStateStore for MemSkills {
    async fn list(&self, _company: &CompanyId) -> crate::Result<Vec<SkillState>> {
        Ok(self.deltas.lock().unwrap().clone())
    }
    async fn set(&self, _company: &CompanyId, state: &SkillState) -> crate::Result<()> {
        let mut deltas = self.deltas.lock().unwrap();
        match deltas.iter_mut().find(|s| s.slug == state.slug) {
            Some(slot) => *slot = state.clone(),
            None => deltas.push(state.clone()),
        }
        Ok(())
    }
    async fn remove(&self, _company: &CompanyId, slug: &str) -> crate::Result<bool> {
        let mut deltas = self.deltas.lock().unwrap();
        let before = deltas.len();
        deltas.retain(|s| s.slug != slug);
        Ok(deltas.len() != before)
    }
}

/// A valid custom-skill delta (its `custom_doc` parses, so `materialize`
/// writes it to the scratch tree).
pub(super) fn custom_skill(slug: &str, enabled: bool, body: &str) -> SkillState {
    SkillState {
        slug: slug.to_string(),
        enabled,
        source: crate::ports::skills_state::SkillSource::Custom,
        custom_doc: Some(body.to_string()),
    }
}

pub(super) const STANDUP_MD: &str =
    "---\nname: Standup Digest\ndescription: Summarize the standup\n---\n\n# Standup Digest\n";

/// The scratch path a materialized skill lands at for the first roster agent
/// (`ceo`) under a company's workspace root.
pub(super) fn skill_scratch(ws: &std::path::Path, slug: &str) -> std::path::PathBuf {
    ws.join("acme")
        .join("ceo")
        .join("skill-catalog")
        .join("skills")
        .join(slug)
        .join("SKILL.md")
}

/// A `CompanyStore` backed by a live, mutable record — so a test can mutate
/// it between two `ensure` calls the same way the console `POST .../team`
/// route or the orchestrator's `add_agent` tool would, and observe the
/// freshness gate react.
#[derive(Default)]
pub(super) struct LiveStore {
    pub(super) record: StdMutex<Option<CompanyRecord>>,
}

#[async_trait]
impl CompanyStore for LiveStore {
    async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        Ok(self.record.lock().unwrap().clone())
    }
    async fn save(&self, record: &CompanyRecord) -> crate::Result<()> {
        *self.record.lock().unwrap() = Some(record.clone());
        Ok(())
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        Ok(Vec::new())
    }
    async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
        Ok(())
    }
}

/// A manifest that grants every tool namespace, so the roster actually builds
/// the exec tools the capability filter then trims. (The default `manifest()`
/// grants nothing, so no exec tools would be present to gate.)
pub(super) fn granting_manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["shell", "code", "web", "files"]

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."
"#,
    )
    .expect("valid manifest")
}

pub(super) fn granting_record() -> CompanyRecord {
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: granting_manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        setup: None,
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// The `ceo` roster agent's live tool names (test introspection via the
/// public `Agent::tools()` accessor).
pub(super) async fn ceo_tool_names(pool: &HarnessPool, id: &CompanyId) -> Vec<String> {
    let guard = pool.agents.read().await;
    let roster = guard.get(id).expect("roster present");
    let ceo = roster
        .iter()
        .find(|a| a.agent_id == "ceo")
        .expect("ceo present");
    ceo.tool_names()
}

/// Builds a `HarnessDeps` carrying the given plan + meter, for the total-
/// ceiling dispatch tests (issue #188). Everything else is the inert fixture
/// wiring (mock provider/context, recording store).
pub(super) fn deps_with_plan(
    dir: &std::path::Path,
    context: Arc<MockContext>,
    meter: Option<Arc<dyn UsageMeter>>,
    plan: Option<crate::harness::capability_budget::CapabilityPlan>,
) -> HarnessDeps {
    HarnessDeps {
        takeovers: Default::default(),
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(MockProvider::new("mock: ")),
        provider_slug: "mock".to_string(),
        serves: None,
        context,
        store: Arc::new(RecordingStore::default()),
        meter,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: None,
        tasks: None,
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
        plan,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        artifacts: None,
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
    }
}

/// A company whose `ceo` carries a $5/day cap and whose `engineer` carries
/// none — the pair that proves the gate is per-teammate, not per-company.
pub(super) fn capped_record() -> CompanyRecord {
    let manifest: CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."
budget_usd_daily = 5.0

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds the product."
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..record()
    }
}

/// A `$usd` inference sample for `agent`, stamped at `at_millis`.
pub(super) fn spend_sample(agent: &str, usd: f64, at_millis: u64) -> UsageSample {
    UsageSample {
        at_millis,
        agent: agent.into(),
        provider: "managed".into(),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cost_usd: usd,
        kind: crate::ports::SampleKind::Inference,
        run_id: None,
        model: None,
    }
}

/// A company whose `treasurer` carries a `budget_usd_daily` of exactly
/// `0.0` — the value `validate_cap` in `server::ops::team` accepts as a
/// non-negative, finite number with no special-case.
pub(super) fn zero_capped_record() -> CompanyRecord {
    let manifest: CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "treasurer"
role = "Treasurer"
description = "Handles spend."
budget_usd_daily = 0.0

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds the product."
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..record()
    }
}

/// Build one agent and return the tools it actually received.
///
/// A local mirror of `build`'s own `built_tool_names` — that one is private
/// to its test module, and this file owns `deps_with_plan`, which is the
/// expensive half.
pub(super) fn belt(grants: &[&str], is_orchestrator: bool, wire_everything: bool) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = deps_with_plan(dir.path(), Arc::new(MockContext::default()), None, None);
    if wire_everything {
        // The three tool families gated on a wired dependency rather than
        // on a cargo feature. Without these the belt is missing exactly the
        // tools most likely to be misclassified — the workspace writes and
        // the priced search.
        deps.workspace = Some(Arc::new(crate::store::FsOps::new(dir.path())));
        deps.artifacts = Some(Arc::new(crate::store::FsOps::new(dir.path())));
        deps.search = Some(crate::harness::search::SearchBackend::new(
            "https://api.example.test".to_string(),
            crate::company::credentials::Credential::from_value("managed-platform-token"),
            crate::company::DEFAULT_SEARCH_DAILY_CALLS,
        ));
        // A registered MCP server is what puts `mcp_list_servers`,
        // `mcp_list_tools` and `mcp_call_tool` on the belt — the three
        // tools issue #443 is about. Without one the coverage check would
        // pass while never having looked at them.
        // A skills source dir is what puts `list_skills`, `describe_skill`
        // and `read_skill_resource` on the belt (named for skills since
        // issue #845; upstream calls them `*_workflow*`). Leaving it `None`
        // is how those three stayed invisible to this check while
        // `describe_workflow` parked in production.
        let company_src = dir.path().join("company-src");
        std::fs::create_dir_all(company_src.join("skills").join("brief")).expect("skill dir");
        std::fs::write(
            company_src.join("skills").join("brief").join("SKILL.md"),
            "---\nname: brief\ndescription: Write a brief\n---\n\nWrite one.\n",
        )
        .expect("skill file");
        deps.skills_source_dir = Some(company_src);
        deps.mcp_servers = vec![McpServerDecl {
            name: "notes".to_string(),
            endpoint: "https://mcp.example.test".to_string(),
            description: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            read_only_tools: Vec::new(),
            timeout_secs: 30,
            enabled: true,
            source: crate::company::mcp::McpSource::Runtime,
            auth: crate::company::mcp::AuthMaterial::None,
            tool_policies: Default::default(),
            tool_inventory: Default::default(),
        }];
    }
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "desk".to_string(),
        role: "Desk Lead".to_string(),
        name: None,
        description: None,
        tier: None,
        harness: None,
        tools: None,
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    };
    let policy = ApprovalPolicy::new(&Policy::default(), None);
    let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
    let agent = build::build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        is_orchestrator,
    )
    .expect("agent builds");
    agent.tools().iter().map(|t| t.name().to_string()).collect()
}

/// A secret store that reads back what was seeded, or fails every read.
#[cfg(any(feature = "chargebee", feature = "paypal"))]
#[derive(Default)]
pub(super) struct BillingSecrets {
    pub(super) map: StdMutex<std::collections::HashMap<String, String>>,
    pub(super) fail: bool,
}

#[cfg(any(feature = "chargebee", feature = "paypal"))]
#[async_trait]
impl SecretStore for BillingSecrets {
    async fn get(
        &self,
        _c: &CompanyId,
        key: &str,
    ) -> crate::Result<Option<crate::ports::types::SecretValue>> {
        if self.fail {
            return Err(crate::error::OpenCompanyError::Store(
                "the secret store is unreachable".into(),
            ));
        }
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(key)
            .map(|v| crate::ports::types::SecretValue(v.clone())))
    }
    async fn set(
        &self,
        _c: &CompanyId,
        key: &str,
        value: crate::ports::types::SecretValue,
    ) -> crate::Result<()> {
        self.map.lock().unwrap().insert(key.to_string(), value.0);
        Ok(())
    }
}

/// A company whose manifest allows exactly `grants`.
#[cfg(any(feature = "chargebee", feature = "paypal"))]
pub(super) fn record_granting(grants: &[&str]) -> CompanyRecord {
    let mut rec = record();
    rec.manifest.tools.allow = grants.iter().map(|g| g.to_string()).collect();
    rec
}

/// The inert fixture deps, with a secret store and a "last known" connection.
#[cfg(any(feature = "chargebee", feature = "paypal"))]
pub(super) fn billing_deps(dir: &std::path::Path, secrets: Arc<dyn SecretStore>) -> HarnessDeps {
    let mut deps = deps_with_plan(dir, Arc::new(MockContext::default()), None, None);
    deps.secrets = Some(secrets);
    deps
}

#[cfg(feature = "chargebee")]
pub(super) async fn pool_resolve_chargebee(
    deps: &HarnessDeps,
) -> Option<crate::harness::chargebee::TenantChargebee> {
    HarnessPool::new()
        .resolve_chargebee(&record_granting(&["chargebee"]), deps)
        .await
}
