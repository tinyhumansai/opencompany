use super::*;

// --- Agent-workspace provisioning (issue #409) --------------------------

fn manifest_agent(role: &str, description: Option<&str>) -> ManifestAgent {
    ManifestAgent {
        provider: None,
        global: false,
        id: "ceo".to_string(),
        role: role.to_string(),
        name: None,
        description: description.map(str::to_string),
        tier: None,
        harness: None,
        tools: None,
        skills: None,
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
    }
}

// --- Dispatched-agent toolbelt contract (issue #188a) -------------------
//
// These tests PIN the tool surface a dispatched company agent receives by
// building a real agent via `build_agent` and reading back its live
// `tools()` list. They lock three things so a future change can neither
// silently widen nor narrow the belt:
//
//   a. the EXACT set of tool names a dispatched desk agent gets (snapshot);
//   b. delegation tools are ABSENT for a dispatched agent but PRESENT for
//      the orchestrator (the depth-cap = 1 / "no re-delegation" invariant,
//      issue #178 — the single most important thing to pin);
//   c. none of the deferred families (browser / search / node / subagent-
//      spawn / skill-exec / memory-tree / `forget`) appear in the belt.
//
// Compiled under the module's default `--features openhuman` config (the
// whole `harness` module is `openhuman`-gated), so the pinned set is the
// openhuman-only belt: the `media` (#109) and `composio` (#110) tool arms
// are inert without their features and are never wired here. Their
// namespace mapping is pinned separately by the `namespace_of` tests in
// `toolbelt.rs`.

use crate::company::Policy;
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::MockProvider;
use crate::ports::CompanyStore;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyRecord, CompanySummary, ContextChunk, LedgerEntry,
};

/// A no-op context store — the belt tests never exercise memory, they only
/// assert the wired tool surface.
struct PinContext;
#[async_trait::async_trait]
impl crate::ports::ContextStore for PinContext {
    async fn put(&self, _: &CompanyId, _: ContextChunk) -> crate::Result<ChunkAddr> {
        Ok(ChunkAddr::new("x"))
    }
    async fn list(&self, _: &CompanyId, _: &str) -> crate::Result<Vec<ChunkMeta>> {
        Ok(Vec::new())
    }
    async fn peek(
        &self,
        _: &CompanyId,
        _: &ChunkAddr,
        _: Option<std::ops::Range<usize>>,
    ) -> crate::Result<String> {
        Ok(String::new())
    }
    async fn search(&self, _: &CompanyId, _: &str, _: usize) -> crate::Result<Vec<ChunkHit>> {
        Ok(Vec::new())
    }
    async fn delete(
        &self,
        _: &CompanyId,
        _: &crate::ports::types::ChunkAddr,
    ) -> crate::Result<bool> {
        Ok(false)
    }
    async fn delete_label(
        &self,
        _: &CompanyId,
        _: &crate::ports::types::ChunkAddr,
        _: &str,
    ) -> crate::Result<bool> {
        Ok(false)
    }
}

/// A no-op company store — `build_agent` only needs a handle; nothing here
/// loads or persists.
struct PinStore;
#[async_trait::async_trait]
impl CompanyStore for PinStore {
    async fn load(&self, _: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        Ok(None)
    }
    async fn save(&self, _: &CompanyRecord) -> crate::Result<()> {
        Ok(())
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        Ok(Vec::new())
    }
    async fn append_ledger(&self, _: &CompanyId, _: LedgerEntry) -> crate::Result<()> {
        Ok(())
    }
}

/// Minimal `HarnessDeps` for building a single agent: offline mock provider,
/// no-op stores, no meter/skills/mcp/media/composio, `AllowAll` capability
/// filter (identity). Workspace lands under a caller-owned tempdir.
fn pin_deps(root: std::path::PathBuf) -> HarnessDeps {
    // Two DISTINCT roots under one caller-owned tempdir, mirroring
    // production (`<home>/harness` beside `<home>/companies`). Reusing one
    // root here would let a test pass while the audit sink sat inside the
    // workspace tree — the exact defect issue #775 fixed.
    let workspace_root = root.join("harness");
    // Production sets this to `<home>/mcp` (see `runtime::builder`); mirror
    // it so the pinned belt reflects a real company rather than the
    // degraded no-MCP-home shape.
    let mcp_home = Some(root.join("mcp"));
    let audit_root = root;
    HarnessDeps {
        takeovers: Default::default(),
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(MockProvider::new("mock: ")),
        provider_slug: "mock".to_string(),
        serves: None,
        context: Arc::new(PinContext),
        store: Arc::new(PinStore),
        meter: None,
        workspace_root,
        mcp_home,
        workspace_git_enabled: false,
        audit_root,
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
        workflow_runner: WorkflowRunnerHandle::default(),
        mcp_failures: McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: ApprovalRequestQueue::default(),
        approval_parker: None,
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: toolbelt::CapabilityFilter::AllowAll,
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
        // Fail-closed default: with no managed search backend wired, the
        // #238 tool is never built and the pinned belt below is the
        // pre-#238 belt exactly.
        search: None,
        tenant_search: None,
        // Fail-closed default: with no workspace store wired, the #237
        // tools are never built and the pinned belt below is the
        // pre-#237 belt exactly.
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    }
}

/// Build one agent under `grants` and return its live tool names, sorted, so
/// a snapshot compares byte-stably against a literal.
fn built_tool_names(grants: &[&str], is_orchestrator: bool) -> Vec<String> {
    built_tool_names_delegating(grants, is_orchestrator, &[])
}

/// [`built_tool_names`] with a `delegates_to` allowlist on the agent (issue
/// #176) — the only difference between a member that may re-delegate and one
/// that may not.
fn built_tool_names_delegating(
    grants: &[&str],
    is_orchestrator: bool,
    delegates_to: &[&str],
) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = pin_deps(dir.path().to_path_buf());
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
        skills: None,
        delegates_to: delegates_to.iter().map(|d| d.to_string()).collect(),
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
    let agent = build_agent(
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
    let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    names
}

/// Build one agent under `grants` with a MANAGED search backend wired, and
/// return its live tool names. Mirrors [`built_tool_names`], differing only
/// in `deps.search` — so the difference between the two is exactly "a
/// credential exists", which is one of the three gate states pinned below.
fn built_tool_names_with_search(grants: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = pin_deps(dir.path().to_path_buf());
    deps.search = Some(crate::harness::search::SearchBackend::new(
        "https://api.example.test".to_string(),
        crate::company::credentials::Credential::from_value("managed-platform-token"),
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    ));
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
        skills: None,
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
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");
    let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    names
}

/// The native capabilities `native_capabilities_on_belt` reads off the SAME
/// agent [`built_tool_names_with_search`] builds — proving the brief's native
/// set is derived from tools that were actually wired, not from the grants.
fn built_native_caps_with_search(grants: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = pin_deps(dir.path().to_path_buf());
    deps.search = Some(crate::harness::search::SearchBackend::new(
        "https://api.example.test".to_string(),
        crate::company::credentials::Credential::from_value("managed-platform-token"),
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    ));
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
        skills: None,
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
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");
    toolbelt::native_capabilities_on_belt(agent.tools())
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Build one agent under `grants` with BOTH a managed search backend and a
/// company's own `provider` connection wired, and return its live tool
/// names. The two together is the interesting case: it is what a company
/// that pasted a key into the console actually has, and what decides which
/// of the two surfaces the model is offered.
fn built_tool_names_with_byo_search(grants: &[&str], provider: &str) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = pin_deps(dir.path().to_path_buf());
    deps.search = Some(crate::harness::search::SearchBackend::new(
        "https://api.example.test".to_string(),
        crate::company::credentials::Credential::from_value("managed-platform-token"),
        crate::company::DEFAULT_SEARCH_DAILY_CALLS,
    ));
    deps.tenant_search = Some(crate::harness::search_byo::TenantSearch::for_test(
        provider,
        Some("tenant-key"),
        Some("https://searx.example"),
    ));
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "desk".to_string(),
        role: "Desk Lead".to_string(),
        name: None,
        description: None,
        tier: None,
        harness: None,
        model: None,
        tools: None,
        skills: None,
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
    };
    let policy = ApprovalPolicy::new(&Policy::default(), None);
    let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");
    let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    names
}

// --- Company-workspace wiring gates (issue #237) -----------------------

/// Build one agent with a workspace store wired and return its tool names.
/// Mirrors [`built_tool_names`], differing only in `deps.workspace`.
fn built_tool_names_with_workspace(grants: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = pin_deps(dir.path().to_path_buf());
    deps.workspace = Some(Arc::new(crate::store::FsOps::new(dir.path())));
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
        skills: None,
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
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");
    let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    names
}

/// Build one agent with an artifact store wired, so the #244 publish gate
/// can be exercised in both its states.
fn built_tool_names_with_artifacts(grants: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = pin_deps(dir.path().to_path_buf());
    deps.artifacts = Some(Arc::new(crate::store::FsOps::new(dir.path())));
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
        skills: None,
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
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");
    let mut names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    names
}

/// [`pin_deps`] with automatic Git checkpoints enabled — the one switch
/// `HarnessDeps::workspace_git_enabled` flips inside `build_agent`.
fn enabled_git_deps(root: std::path::PathBuf) -> HarnessDeps {
    let mut deps = pin_deps(root);
    deps.workspace_git_enabled = true;
    deps
}

/// `git log` inside the workspace, resolving through the checkpointer's
/// `.git` pointer, exactly as the existing checkpoint tests do.
fn git_log(workspace: &std::path::Path) -> String {
    String::from_utf8(
        std::process::Command::new("git")
            .args(["log", "--format=%s"])
            .current_dir(workspace)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

#[path = "build_seat_persona_tests.rs"]
mod seat_persona_tests;
#[path = "build_tests_part1.rs"]
mod tests_part1;
#[path = "build_tests_part2.rs"]
mod tests_part2;
#[path = "build_tests_part3.rs"]
mod tests_part3;
