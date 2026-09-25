use super::super::*;
use crate::ports::types::{CompanyRecord, CompanySummary, LedgerEntry};
use std::sync::Mutex as StdMutex;

pub(super) fn agent(id: &str, tier: Option<&str>) -> ManifestAgent {
    ManifestAgent {
        provider: None,
        global: false,
        id: id.to_string(),
        role: "Role".to_string(),
        name: None,
        description: None,
        tier: tier.map(str::to_string),
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

// ── Issue #419: the cap is announced, not silently applied ─────────────

// ── Issue #453: no claim, no delegation ────────────────────────────────

// ── Issue #186 part b: the lifecycle tools ─────────────────────────────

/// A company with a `strategy` desk led by a roster teammate, an
/// `archive` desk nobody on the roster sits on, and a `writer` teammate who
/// is *not* a desk — the exact shape issue #272 was observed on.
pub(super) fn desks_record(id: &CompanyId) -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "writer"
role = "Writer"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["writer"]

[[group_chat]]
id = "archive"
name = "Archive desk"
members = ["nobody"]
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..seeded_record(id)
    }
}

pub(super) fn desk_tool(record: CompanyRecord, queue: &DelegationQueue) -> DelegateToDeskTool {
    let company = record.id.clone();
    DelegateToDeskTool::new(
        queue.clone(),
        company,
        Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
    )
}

// --- Recursive desk delegation (issue #176) -----------------------------

/// A three-desk record where two desks have roster leads, so a member of one
/// can be given an allowlist that admits one desk and not another.
pub(super) fn nested_desks_record(id: &CompanyId) -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "writer"
role = "Writer"
delegates_to = ["research"]

[[agent]]
id = "analyst"
role = "Analyst"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["writer"]

[[group_chat]]
id = "research"
name = "Research desk"
members = ["analyst"]

[[group_chat]]
id = "legal"
name = "Legal desk"
members = ["ceo"]
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..seeded_record(id)
    }
}

/// The `writer`'s copy of `delegate_to_desk`: allowed `research` only.
pub(super) fn member_desk_tool(
    record: CompanyRecord,
    queue: &DelegationQueue,
) -> DelegateToDeskTool {
    let company = record.id.clone();
    DelegateToDeskTool::for_member(
        queue.clone(),
        company,
        Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
        MemberScope {
            member: "writer".to_string(),
            delegates_to: vec!["research".to_string()],
        },
    )
}

/// A store that cannot answer, so the grounding read has nothing to check
/// the target against.
pub(super) struct BrokenStore;

#[async_trait::async_trait]
impl CompanyStore for BrokenStore {
    async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        Err(crate::OpenCompanyError::Store("store is down".to_string()))
    }
    async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
        Ok(())
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        Ok(Vec::new())
    }
    async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
        Ok(())
    }
}

// ── delegate_to_teammate at the tool boundary (issue #884) ──────────────

/// A company whose `strategy` desk has THREE members, so its lead has peers
/// to reach — the shape D1 was observed on — plus an `analyst` on a desk the
/// lead's `delegates_to` permits and a `legal_counsel` on one it does not.
pub(super) fn peers_record(id: &CompanyId) -> CompanyRecord {
    let manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "writer"
role = "Writer"
delegates_to = ["research"]

[[agent]]
id = "editor"
role = "Editor"

[[agent]]
id = "analyst"
role = "Analyst"

[[agent]]
id = "legal_counsel"
role = "Counsel"

[[group_chat]]
id = "strategy"
name = "Strategy desk"
members = ["writer", "editor"]

[[group_chat]]
id = "research"
name = "Research desk"
members = ["analyst"]

[[group_chat]]
id = "legal"
name = "Legal desk"
members = ["legal_counsel"]
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        manifest,
        ..seeded_record(id)
    }
}

/// `writer`'s copy of the teammate tool: a desk lead with one peer on its
/// own desk and a `research` allowlist.
pub(super) fn member_teammate_tool(
    record: CompanyRecord,
    queue: &DelegationQueue,
) -> DelegateToTeammateTool {
    let company = record.id.clone();
    DelegateToTeammateTool::for_member(
        queue.clone(),
        company,
        Arc::new(MemStore::seeded(record)) as Arc<dyn CompanyStore>,
        MemberScope {
            member: "writer".to_string(),
            delegates_to: vec!["research".to_string()],
        },
    )
}

// --- add_agent (issue #71) ----------------------------------------------

/// An in-memory `CompanyStore` so `AddAgentTool` can be exercised without a
/// filesystem, mirroring `crate::server::ops::team`'s `add_member` write
/// path (load → push overlay → save).
#[derive(Default)]
pub(super) struct MemStore {
    pub(super) record: StdMutex<Option<CompanyRecord>>,
}

impl MemStore {
    pub(super) fn seeded(record: CompanyRecord) -> Self {
        Self {
            record: StdMutex::new(Some(record)),
        }
    }
}

#[async_trait::async_trait]
impl CompanyStore for MemStore {
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

pub(super) fn empty_manifest() -> crate::company::CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n").expect("valid manifest")
}

pub(super) fn seeded_record(id: &CompanyId) -> CompanyRecord {
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: id.clone(),
        manifest: empty_manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
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
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// A minter scoped to part of the company grant, for the #619 tests below.
/// `minter_tools` is the line it declares; `minter_grants` is that line
/// already narrowed by the company `allow` — what `build_agent` hands the
/// tool.
pub(super) fn scoped_add_agent(company: CompanyId, store: Arc<dyn CompanyStore>) -> AddAgentTool {
    AddAgentTool::new(
        company,
        store,
        "ceo".to_string(),
        Some(vec!["workspace".to_string()]),
        vec!["workspace".to_string()],
    )
}

// ---- run_workflow (issue #67) ----

/// A valid trigger → agent → output graph, mirroring the REST route's fixture.
pub(super) const DEMO_WF: &str = r#"
    id = "demo"
    name = "Demo flow"
    description = "A tiny trigger → agent → output graph."
    [[node]]
    id = "start"
    kind = "trigger"
    name = "Start"
    [[node]]
    id = "worker"
    kind = "agent"
    name = "Worker"
    agent = "assistant"
    [[node]]
    id = "done"
    kind = "output"
    name = "Report"
    [[edge]]
    from = "start"
    to = "worker"
    [[edge]]
    from = "worker"
    to = "done"
"#;

/// A [`WorkflowRunner`] test double: records the ids it was asked to run and
/// returns a canned [`WorkflowRun`].
pub(super) struct StubRunner {
    pub(super) calls: Arc<Mutex<Vec<String>>>,
    pub(super) run: WorkflowRun,
}

impl StubRunner {
    pub(super) fn new(run: WorkflowRun) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            run,
        }
    }

    pub(super) fn empty() -> Self {
        Self::new(WorkflowRun {
            output: Value::Null,
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
    }
}

#[async_trait::async_trait]
impl WorkflowRunner for StubRunner {
    async fn run(
        &self,
        _company: &CompanyId,
        workflow: &WorkflowFile,
        _input: Value,
        _ctx: &crate::ports::WorkflowRunContext,
    ) -> crate::Result<WorkflowRun> {
        self.calls.lock().unwrap().push(workflow.id.clone());
        Ok(self.run.clone())
    }
}

/// A [`WorkflowRunner`] test double whose `run` always returns `Err` — the
/// engine-failed shape issue #1865's review comment 3877185396 flagged as
/// silent: `RunWorkflowTool`'s `Ok(Err(err))` arm journaled a finish but
/// filed no `workflow_run_failed` notification, unlike the console run
/// route, the cron scheduler, and the approval-resume path, which all
/// file one through `WorkflowSpawn`.
pub(super) struct FailingRunner;

#[async_trait::async_trait]
impl WorkflowRunner for FailingRunner {
    async fn run(
        &self,
        _company: &CompanyId,
        _workflow: &WorkflowFile,
        _input: Value,
        _ctx: &crate::ports::WorkflowRunContext,
    ) -> crate::Result<WorkflowRun> {
        Err(crate::error::OpenCompanyError::Harness(
            "the engine blew up".to_string(),
        ))
    }
}

/// Writes `DEMO_WF` to `<dir>/workflows/demo.toml`.
pub(super) fn seed_demo_workflow(dir: &std::path::Path) {
    let wf = dir.join("workflows");
    std::fs::create_dir_all(&wf).unwrap();
    std::fs::write(wf.join("demo.toml"), DEMO_WF).unwrap();
}

// ---- create_workflow (issue #112) ----

/// A record with an `assistant` roster agent so an `agent`-node graph passes
/// the roster cross-check inside the create core.
pub(super) fn record_with_assistant(company: &CompanyId) -> CompanyRecord {
    let manifest: crate::company::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n",
    )
    .expect("valid manifest");
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: company.clone(),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
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
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// The canonical happy graph the create tool accepts (camelCase body).
pub(super) fn greeter_body() -> Value {
    json!({
        "id": "greeter",
        "name": "Greeter",
        "description": "Says hi.",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            { "id": "worker", "kind": "agent", "name": "Worker", "agent": "assistant" },
            { "id": "done", "kind": "output", "name": "Report" }
        ],
        "edges": [
            { "from": "start", "to": "worker" },
            { "from": "worker", "to": "done", "label": "ok" }
        ]
    })
}

/// Like [`record_with_assistant`], but the company `[tools].allow` grants the
/// `web` namespace so a `web_fetch` `tool_call` clears the author-time grant
/// gate under the `openhuman` build (issue #661).
pub(super) fn record_granting_web(company: &CompanyId) -> CompanyRecord {
    let mut record = record_with_assistant(company);
    record.manifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"web\"]\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n",
    )
    .expect("valid manifest");
    record
}

// ---- read_run_output (issue #418) ----

/// Builds a `RunWorkflowTool` over the demo graph in `dir`, a stub runner
/// returning `run`, and the given caches — the shared setup the round-trip
/// tests need.
/// Returns the tool **and** the runner `Arc` — the handle keeps only a weak
/// reference, so the caller must hold the returned runner alive for the
/// duration of the test or the run tool reports "no runner wired".
pub(super) fn run_tool_over(
    dir: &std::path::Path,
    run: WorkflowRun,
    refs: WorkflowRefQueue,
    cache: RunOutputCache,
) -> (RunWorkflowTool, Arc<dyn WorkflowRunner>) {
    let runner: Arc<dyn WorkflowRunner> = Arc::new(StubRunner::new(run));
    let handle = WorkflowRunnerHandle::default();
    handle.set(&runner);
    let tool = RunWorkflowTool::new(
        CompanyId::new("acme"),
        Some(dir.to_path_buf()),
        Arc::new(MemStore::default()),
        handle,
        crate::runtime::RunSupervisor::default(),
        None,
        refs,
        cache,
        None,
    );
    (tool, runner)
}
