//! Tests for the #661 (M7) workflow admin tools.
//!
//! The company layer's `update_company_workflow` / `delete_company_workflow`
//! are covered in `workflow_create.rs`; nothing here re-tests them. What is
//! tested is the part that is new — the tool surface: the read projection the
//! writer accepts back, the required version token, the agent-surface refusals
//! that do NOT exist in the company layer, and that the company layer's own
//! gates (seeds, #682 validation, optimistic concurrency) still bite when a
//! tool is what is calling them.

use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};

use super::*;
use crate::company::{CompanyManifest, update_company_workflow};
use crate::error::Result;
use crate::ports::types::{
    CompanyEvent, CompanyRecord, CompanySummary, EventSeq, LedgerEntry, OverlayWorkflow,
    StoredEvent,
};
use crate::ports::workflow_revisions::WorkflowRevisionRecord;

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemStore {
    record: StdMutex<Option<CompanyRecord>>,
}

impl MemStore {
    fn seeded(record: CompanyRecord) -> Self {
        Self {
            record: StdMutex::new(Some(record)),
        }
    }
}

#[async_trait]
impl CompanyStore for MemStore {
    async fn load(&self, _id: &CompanyId) -> Result<Option<CompanyRecord>> {
        Ok(self.record.lock().unwrap().clone())
    }
    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        *self.record.lock().unwrap() = Some(record.clone());
        Ok(())
    }
    async fn list(&self) -> Result<Vec<CompanySummary>> {
        Ok(Vec::new())
    }
    async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> Result<()> {
        Ok(())
    }
}

/// Wraps a [`MemStore`], counting `load` calls and failing every one after
/// the first.
///
/// A stand-in for a transient store hiccup between two reads of what should
/// be "the same" record. `read_workflow`'s overlay body and its
/// `[globals].disable` list must come from **one** load — a second, later
/// load disagreeing with the first is exactly the shape that let a
/// company-disabled global slip through a fallback that turned that second
/// load's failure into an empty (not-disabled) list.
#[derive(Default)]
struct FailsAfterFirstLoadStore {
    inner: MemStore,
    calls: StdMutex<u32>,
}

impl FailsAfterFirstLoadStore {
    fn seeded(record: CompanyRecord) -> Self {
        Self {
            inner: MemStore::seeded(record),
            calls: StdMutex::new(0),
        }
    }
}

#[async_trait]
impl CompanyStore for FailsAfterFirstLoadStore {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>> {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if count > 1 {
            return Err(crate::error::OpenCompanyError::Store(
                "simulated store failure on a second read".to_string(),
            ));
        }
        self.inner.load(id).await
    }
    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        self.inner.save(record).await
    }
    async fn list(&self) -> Result<Vec<CompanySummary>> {
        self.inner.list().await
    }
    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()> {
        self.inner.append_ledger(id, entry).await
    }
}

#[derive(Default)]
struct MemLog {
    events: StdMutex<Vec<CompanyEvent>>,
}

#[async_trait]
impl EventLog for MemLog {
    async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
        let mut guard = self.events.lock().unwrap();
        guard.push(event);
        Ok(EventSeq::new(guard.len() as u64))
    }
    async fn read_from(
        &self,
        _id: &CompanyId,
        _seq: EventSeq,
        _limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    fn subscribe(
        &self,
        _id: &CompanyId,
    ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
        Box::pin(stream::empty())
    }
}

#[derive(Default)]
struct MemRevisions {
    rows: StdMutex<Vec<WorkflowRevisionRecord>>,
}

#[async_trait]
impl WorkflowRevisionStore for MemRevisions {
    async fn push_revision(
        &self,
        _company: &CompanyId,
        revision: &WorkflowRevisionRecord,
    ) -> Result<()> {
        self.rows.lock().unwrap().push(revision.clone());
        Ok(())
    }
    async fn list_revisions(
        &self,
        _company: &CompanyId,
        workflow_id: &str,
    ) -> Result<Vec<WorkflowRevisionRecord>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.workflow_id == workflow_id)
            .cloned()
            .collect())
    }
    async fn get_revision(
        &self,
        _company: &CompanyId,
        workflow_id: &str,
        revision_id: &str,
    ) -> Result<Option<WorkflowRevisionRecord>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.workflow_id == workflow_id && r.id == revision_id)
            .cloned())
    }
    async fn delete_revisions(&self, _company: &CompanyId, workflow_id: &str) -> Result<u64> {
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|r| r.workflow_id != workflow_id);
        Ok((before - rows.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A harness holding every double, so a test can read back what the tools wrote.
struct Fixture {
    company: CompanyId,
    dir: tempfile::TempDir,
    store: Arc<MemStore>,
    revisions: Arc<MemRevisions>,
    log: Arc<MemLog>,
}

impl Fixture {
    fn new() -> Self {
        let company = CompanyId::new("acme");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n",
        )
        .expect("valid manifest");
        let record = CompanyRecord {
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
        };
        Self {
            company,
            dir: tempfile::tempdir().expect("tempdir"),
            store: Arc::new(MemStore::seeded(record)),
            revisions: Arc::new(MemRevisions::default()),
            log: Arc::new(MemLog::default()),
        }
    }

    fn source_dir(&self) -> &Path {
        self.dir.path()
    }

    fn admin(&self) -> WorkflowAdmin {
        let store: Arc<dyn CompanyStore> = self.store.clone();
        let revisions: Arc<dyn WorkflowRevisionStore> = self.revisions.clone();
        let events: Arc<dyn EventLog> = self.log.clone();
        WorkflowAdmin::new(
            self.company.clone(),
            Some(self.dir.path().to_path_buf()),
            store,
            Some(revisions),
            Some(events),
        )
    }

    /// The same handle with no revision store, for the degraded-deployment case.
    fn admin_without_revisions(&self) -> WorkflowAdmin {
        let store: Arc<dyn CompanyStore> = self.store.clone();
        WorkflowAdmin::new(
            self.company.clone(),
            Some(self.dir.path().to_path_buf()),
            store,
            None,
            None,
        )
    }

    /// Put a body straight onto the record's overlay, bypassing the tools —
    /// for the shapes the agent schema cannot author (a schedule, node policy,
    /// a corrupt body).
    async fn put_overlay(&self, id: &str, toml_src: &str) {
        let mut record = self
            .store
            .load(&self.company)
            .await
            .unwrap()
            .expect("record");
        record.overlay_workflows.push(OverlayWorkflow {
            id: id.to_string(),
            toml: toml_src.to_string(),
        });
        record.manifest.workflows.enabled.push(id.to_string());
        self.store.save(&record).await.unwrap();
    }

    async fn overlays(&self) -> Vec<OverlayWorkflow> {
        self.store
            .load(&self.company)
            .await
            .unwrap()
            .map(|r| r.overlay_workflows)
            .unwrap_or_default()
    }

    async fn enabled(&self) -> Vec<String> {
        self.store
            .load(&self.company)
            .await
            .unwrap()
            .map(|r| r.manifest.workflows.enabled)
            .unwrap_or_default()
    }

    fn events(&self) -> Vec<CompanyEvent> {
        self.log.events.lock().unwrap().clone()
    }

    /// Write a seed file into `workflows/`, so an id is source-defined.
    fn write_seed(&self, id: &str, toml_src: &str) {
        let dir = self.dir.path().join("workflows");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("{id}.toml")), toml_src).expect("write seed");
    }
}

/// The graph the `create_workflow`/`update_workflow` schema accepts, as JSON.
fn graph_args(id: &str, name: &str, worker_name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "description": "A tiny graph.",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            { "id": "worker", "kind": "agent", "name": worker_name, "agent": "assistant" },
            { "id": "done", "kind": "output", "name": "Report" }
        ],
        "edges": [
            { "from": "start", "to": "worker" },
            { "from": "worker", "to": "done" }
        ]
    })
}

/// A graph with an owning desk already set (issue #1862 prerequisite). Tests
/// put it on the record directly rather than via a tool call so a fixture can
/// start from "already owned" without depending on `UpdateWorkflowTool`'s own
/// desk handling — the same reason `SCHEDULED_TOML` does.
const OWNED_TOML: &str = r#"
id = "owned"
name = "Owned flow"
owner_desk = "engineering"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#;

const SEED_TOML: &str = r#"
id = "seeded"
name = "Seeded flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#;

/// A graph whose trigger carries a cron. Only an operator can author this, so
/// tests put it on the record directly.
const SCHEDULED_TOML: &str = r#"
id = "nightly"
name = "Nightly flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
schedule = "0 3 * * *"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#;

/// A graph with an operator's approval gate on a node.
const GATED_TOML: &str = r#"
id = "gated"
name = "Gated flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "worker"
kind = "agent"
name = "Worker"
agent = "assistant"
requires_approval = true
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "worker"
[[edge]]
from = "worker"
to = "done"
"#;

/// The markdown a tool result puts in front of the model.
fn md(result: &ToolResult) -> String {
    result.markdown_formatted.clone().unwrap_or_default()
}

/// The text of an error result.
fn err_text(result: &ToolResult) -> String {
    assert!(result.is_error, "expected an error result");
    result.output_for_llm(false)
}

/// The JSON payload a successful result carries.
fn data(result: &ToolResult) -> Value {
    assert!(!result.is_error, "expected a success result: {result:?}");
    for block in &result.content {
        if let oh::skills::types::ToolContent::Json { data } = block {
            return data.clone();
        }
    }
    panic!("no JSON block in {result:?}");
}

// ---------------------------------------------------------------------------
// 1. create → read → update round trip
// ---------------------------------------------------------------------------

/// The contract the whole feature rests on: what `read_workflow` hands back is
/// what `update_workflow` accepts, unmodified apart from the edit itself.
///
/// If this fails, the two tools have different schemas and every edit is a
/// guess — which is the blind rewrite the read tool exists to prevent.
#[tokio::test]
async fn read_round_trips_into_an_update_that_keeps_the_workflow_enabled() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let events: Arc<dyn EventLog> = fx.log.clone();
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        Some(&events),
        crate::company::RawWorkflow::try_from(
            serde_json::from_value::<CreateWorkflowArgs>(graph_args(
                "greeter", "Greeter", "Worker",
            ))
            .unwrap(),
        )
        .unwrap(),
        None,
        None,
    )
    .await
    .expect("creates");

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "greeter" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["editable"], json!(true));
    assert_eq!(payload["enabled"], json!(true));
    let version = payload["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // Take the graph exactly as read, change one node's name, hand it straight
    // back with the token. No reshaping step.
    let mut graph = payload["workflow"].clone();
    graph["nodes"][1]["name"] = json!("Renamed worker");
    graph["expected_version"] = json!(version);

    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "greeter")
            .expect("loads")
            .expect("present");
    assert_eq!(reloaded.nodes[1].name, "Renamed worker");
    assert_eq!(reloaded.nodes.len(), 3, "the rest of the graph survived");
    assert_eq!(reloaded.description.as_deref(), Some("A tiny graph."));

    // Enablement is not touched by an edit, and the update journals.
    assert!(fx.enabled().await.contains(&"greeter".to_string()));
    assert!(
        fx.events()
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowUpdated { workflow_id, .. } if workflow_id == "greeter")),
        "{:?}",
        fx.events()
    );
    // #274: the prior body was snapshotted, so the edit is undoable.
    assert_eq!(
        fx.revisions
            .list_revisions(&fx.company, "greeter")
            .await
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// 2. the version token
// ---------------------------------------------------------------------------

/// An agent that never read the graph has no token, and is told exactly that
/// rather than being allowed a blind full replacement.
#[tokio::test]
async fn an_update_without_a_version_token_is_refused_before_anything_is_read() {
    let fx = Fixture::new();
    let result = UpdateWorkflowTool::new(fx.admin())
        .execute(graph_args("greeter", "Greeter", "Worker"))
        .await
        .unwrap();
    let text = err_text(&result);
    assert!(text.contains("expected_version"), "{text}");
    assert!(text.contains(READ_WORKFLOW_TOOL), "{text}");
    assert!(fx.overlays().await.is_empty(), "nothing was written");
}

/// A token from before a concurrent write is the company layer's 409, passed
/// through with its own reload instruction.
#[tokio::test]
async fn a_stale_version_token_is_the_company_layers_conflict() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let revisions: Arc<dyn WorkflowRevisionStore> = fx.revisions.clone();
    let draft = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args("greeter", "Greeter", "Worker"))
            .unwrap(),
    )
    .unwrap();
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        None,
        draft,
        None,
        None,
    )
    .await
    .expect("creates");

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "greeter" }))
        .await
        .unwrap();
    let stale = data(&read)["version"].as_str().unwrap().to_string();

    // Somebody else (the console) edits it in between.
    let other = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args(
            "greeter",
            "Greeter",
            "Console worker",
        ))
        .unwrap(),
    )
    .unwrap();
    update_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        &revisions,
        None,
        other,
        None,
        None,
    )
    .await
    .expect("console edit lands");

    let mut graph = graph_args("greeter", "Greeter", "Agent worker");
    graph["expected_version"] = json!(stale);
    let result = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    let text = err_text(&result);
    assert!(text.contains("changed since you loaded it"), "{text}");
    assert!(text.contains("Reload it"), "{text}");

    // The console's edit survived — that is the whole point of the token.
    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "greeter")
            .unwrap()
            .unwrap();
    assert_eq!(reloaded.nodes[1].name, "Console worker");
}

// ---------------------------------------------------------------------------
// 3. seeds
// ---------------------------------------------------------------------------

/// A workflow shipped in the company's source tree is readable and unwritable,
/// and the read says so BEFORE a write is attempted.
#[tokio::test]
async fn a_seed_backed_workflow_reads_uneditable_and_refuses_both_writes() {
    let fx = Fixture::new();
    fx.write_seed("seeded", SEED_TOML);

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "seeded" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["editable"], json!(false));
    assert!(
        payload["version"].is_null(),
        "no token for an unwritable graph"
    );
    assert_eq!(payload["workflow"]["name"], json!("Seeded flow"));

    let mut graph = graph_args("seeded", "Seeded flow", "Worker");
    graph["expected_version"] = json!("anything");
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(
        err_text(&updated).contains("defined by a file in the company source tree"),
        "{}",
        err_text(&updated)
    );

    let deleted = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "seeded" }))
        .await
        .unwrap();
    assert!(
        err_text(&deleted).contains("defined by a file in the company source tree"),
        "{}",
        err_text(&deleted)
    );
}

const SEED_WITH_POSTCONDITION_TOML: &str = r#"
id = "seeded-pc"
name = "Seeded worker flow"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "worker"
kind = "agent"
name = "Worker"
agent = "assistant"
[node.postcondition]
require = "non_empty"
[[edge]]
from = "start"
to = "worker"
"#;

/// Codex review on #1937 (issue #1866, thread 3) — the RED-on-old proof.
/// `seed_draft` rebuilds a [`crate::company::RawNode`] per node from the
/// parsed [`crate::company::WorkflowFile`] for the seed read path — every
/// other run-policy field (`on_error`, `retry`, `requires_approval`,
/// `repeatable`, `destination`) is carried through `.clone()`, so a seed
/// node's declared `postcondition` must be too, or two things go wrong at
/// once: the runtime still enforces a gate the agent is never told about,
/// and `project_workflow_spec`'s `unexpressible` residue — the ONLY place
/// `read_workflow` surfaces a run-policy field the agent-facing spec can't
/// carry — silently omits it. On the code as it stood before this fix, the
/// second assertion below fails: `seed_draft` zeroed `postcondition` before
/// `project_workflow_spec` ever ran, so `unexpressible` was empty and the
/// whole "per-node run policy" sentence never appeared.
#[tokio::test]
async fn a_seed_backed_postcondition_is_named_in_the_read_projection() {
    let fx = Fixture::new();
    fx.write_seed("seeded-pc", SEED_WITH_POSTCONDITION_TOML);

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "seeded-pc" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["editable"], json!(false));

    // The agent-facing spec has no `postcondition` field at all (same as
    // `on_error`/`retry`) — it can only ever be named in the `unexpressible`
    // prose the markdown reply carries. Match the exact phrase
    // `unexpressible_summary` renders (`node \`worker\` (postcondition)`),
    // not a bare substring — the workflow's own name must not collide.
    let markdown = read.output_for_llm(true);
    assert!(
        markdown.contains("node `worker` (postcondition)"),
        "a seed-defined postcondition must be named in the read reply's \
         per-node run policy summary, or the agent is told a stricter gate \
         does not exist when the runtime still enforces one: {markdown}"
    );
}

// ---------------------------------------------------------------------------
// 4. the agent-surface refusals
// ---------------------------------------------------------------------------

/// The gate is on the TOOLS, not on the company layer: the same target the
/// tools refuse is still writable through `update_company_workflow`, so the
/// console is untouched.
///
/// That asymmetry is the whole design claim of the guard, and this is the only
/// test that can show it.
#[tokio::test]
async fn a_scheduled_workflow_is_refused_by_the_tools_and_still_writable_by_the_console() {
    let fx = Fixture::new();
    fx.put_overlay("nightly", SCHEDULED_TOML).await;

    let mut graph = graph_args("nightly", "Nightly flow", "Worker");
    graph["expected_version"] = json!(crate::company::workflow_version(SCHEDULED_TOML));
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    let text = err_text(&updated);
    assert!(text.contains("runs on a schedule"), "{text}");
    assert!(text.contains("0 3 * * *"), "{text}");
    assert!(text.contains("console"), "{text}");

    let deleted = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "nightly" }))
        .await
        .unwrap();
    assert!(err_text(&deleted).contains("runs on a schedule"));
    assert_eq!(fx.overlays().await.len(), 1, "nothing was removed");

    // The company layer — the console's path — still accepts the same write.
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let revisions: Arc<dyn WorkflowRevisionStore> = fx.revisions.clone();
    let draft = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args(
            "nightly",
            "Nightly flow",
            "Worker",
        ))
        .unwrap(),
    )
    .unwrap();
    update_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        &revisions,
        None,
        draft,
        None,
        None,
    )
    .await
    .expect("the console path is not gated by the agent tools' refusal");
}

/// An edit that would silently drop an operator's approval gate is refused and
/// names the node and the field.
#[tokio::test]
async fn an_update_will_not_silently_drop_a_nodes_approval_gate() {
    let fx = Fixture::new();
    fx.put_overlay("gated", GATED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "gated" }))
        .await
        .unwrap();
    // The read warns before the write is attempted.
    assert!(md(&read).contains("requires_approval"), "{}", md(&read));

    let mut graph = graph_args("gated", "Gated flow", "Worker");
    graph["expected_version"] = json!(data(&read)["version"].as_str().unwrap());
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    let text = err_text(&updated);
    assert!(text.contains("requires_approval"), "{text}");
    assert!(text.contains("`worker`"), "{text}");

    // And the gate is still on the stored graph.
    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "gated")
            .unwrap()
            .unwrap();
    assert_eq!(reloaded.nodes[1].requires_approval, Some(true));
}

/// **Regression, issue #1882 review.** `owner_desk` must survive an
/// agent-tool update that never mentions it. `ownerDesk` IS on the schema
/// (`create_workflow_parameters_schema`) so a caller can supply it — but an
/// agent that builds a full-replacement edit the way `read_workflow`'s own
/// projection encourages (the fields it returned, not a value it never
/// surfaced) naturally omits a field it was never shown, `RawWorkflow::
/// try_from` then leaves `owner_desk: None` on the draft. Before the fix, ANY
/// full-replacement update through this tool cleared whatever desk was
/// already on the workflow whenever the caller's edit omitted it — the
/// preserve has to happen server-side for that omitted-field case, which is
/// what this pins. The sibling case — the caller DOES supply a different
/// `ownerDesk` and that reassignment must actually apply — is pinned by
/// `an_update_applies_a_newly_supplied_owner_desk` below.
#[tokio::test]
async fn an_update_preserves_the_workflows_owner_desk() {
    let fx = Fixture::new();
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // A full-replacement edit built the way an agent naturally would — the
    // edit omits `ownerDesk` entirely, the same as an agent that never read
    // (or does not care about) the desk assignment.
    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk.as_deref(),
        Some("engineering"),
        "an agent update must not clear a desk it was never shown"
    );
}

/// **Regression, PR #1882 review (bot finding on `workflow_admin.rs:707`).**
/// When the agent DOES supply an `ownerDesk` on a full-replacement update —
/// possible since `ownerDesk` was added to `create_workflow_parameters_schema`
/// / `CreateWorkflowArgs` (issue #1862 prerequisite) — that value must reach
/// storage. Before the fix, the tool ran `draft.owner_desk =
/// raw.owner_desk.clone()` unconditionally after `RawWorkflow::try_from` had
/// already resolved and normalized whatever the caller sent, so a caller
/// reassigning a workflow to a different desk saw `update_workflow` report
/// success while the desk silently stayed on the old value — the stale
/// "the schema has no field for it" reasoning in the comment above this test
/// no longer held once that field existed.
#[tokio::test]
async fn an_update_applies_a_newly_supplied_owner_desk() {
    let fx = Fixture::new();
    // Give the record two real desks so a reassignment resolves: the stored
    // "engineering" and a distinct target "sales".
    {
        let mut record = fx.store.load(&fx.company).await.unwrap().unwrap();
        record.manifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n\
             [[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\nmembers = [\"assistant\"]\n\
             [[group_chat]]\nid = \"sales\"\nname = \"Sales\"\nmembers = [\"assistant\"]\n",
        )
        .expect("valid manifest");
        fx.store.save(&record).await.unwrap();
    }
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // A full-replacement edit that explicitly reassigns the desk.
    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["ownerDesk"] = json!("sales");
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk.as_deref(),
        Some("sales"),
        "an agent explicitly reassigning ownerDesk must have that value applied, not silently discarded for the stored one"
    );
}

/// **Regression, PR #1882 review (bot finding on `workflow_admin.rs:713`).**
/// An explicit `"ownerDesk": null` must unassign the workflow, not restore the
/// stored desk. `RawWorkflow::try_from(CreateWorkflowArgs)` parses `null` to
/// `draft.owner_desk == None` — the exact same value an omitted key produces
/// — so the `owner_desk.is_none()` fallback alone cannot tell "the caller
/// never mentioned ownership" (must preserve, per
/// `an_update_preserves_the_workflows_owner_desk` above) apart from "the
/// caller explicitly cleared it" (must apply). Before the fix, `update_workflow`
/// had no payload that could ever produce an unowned result: every value
/// resolved to either "keep stored" or "move to a different desk". This pins
/// the fix's `owner_desk_mentioned` presence check on the raw JSON.
#[tokio::test]
async fn an_update_can_explicitly_clear_owner_desk_with_null() {
    let fx = Fixture::new();
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    // A full-replacement edit that explicitly unassigns the desk.
    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["ownerDesk"] = Value::Null;
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk, None,
        "an agent explicitly sending ownerDesk: null must clear the stored desk, not restore it"
    );
}

/// Sibling of the null case above: an explicit all-whitespace `ownerDesk`
/// carries the same "I thought about this and I want it unowned" signal as
/// `null` — `normalize_owner_desk` already treats blank the same as absent
/// for validation purposes, and this pins that the update tool's presence
/// check (keyed on the JSON field existing, not on what it normalizes to)
/// clears rather than preserves for this shape too.
#[tokio::test]
async fn an_update_can_explicitly_clear_owner_desk_with_blank_string() {
    let fx = Fixture::new();
    fx.put_overlay("owned", OWNED_TOML).await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "owned" }))
        .await
        .unwrap();
    let version = data(&read)["version"]
        .as_str()
        .expect("a version token")
        .to_string();

    let mut graph = graph_args("owned", "Owned flow", "Worker");
    graph["ownerDesk"] = json!("   ");
    graph["expected_version"] = json!(version);
    let updated = UpdateWorkflowTool::new(fx.admin())
        .execute(graph)
        .await
        .unwrap();
    assert!(!updated.is_error, "{}", err_text(&updated));

    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "owned")
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded.owner_desk, None,
        "an agent explicitly sending a blank ownerDesk must clear the stored desk, not restore it"
    );
}

// ---------------------------------------------------------------------------
// 5. delete
// ---------------------------------------------------------------------------

/// A delete takes the body, the enabled id and the revision history, journals
/// what went, and names it.
#[tokio::test]
async fn delete_removes_the_body_the_enabled_id_and_the_history() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let revisions: Arc<dyn WorkflowRevisionStore> = fx.revisions.clone();
    let draft = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args("greeter", "Greeter", "Worker"))
            .unwrap(),
    )
    .unwrap();
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        None,
        draft,
        None,
        None,
    )
    .await
    .expect("creates");
    // Give it a history to cascade.
    let edited = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args("greeter", "Greeter", "Second"))
            .unwrap(),
    )
    .unwrap();
    update_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        &revisions,
        None,
        edited,
        None,
        None,
    )
    .await
    .expect("edits");
    assert_eq!(
        fx.revisions
            .list_revisions(&fx.company, "greeter")
            .await
            .unwrap()
            .len(),
        1
    );

    let result = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "greeter" }))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", err_text(&result));
    assert!(
        md(&result).contains("Greeter"),
        "names what went: {}",
        md(&result)
    );
    assert!(md(&result).contains("cannot be undone"), "{}", md(&result));

    assert!(fx.overlays().await.is_empty(), "body gone");
    assert!(
        !fx.enabled().await.contains(&"greeter".to_string()),
        "enabled id gone"
    );
    assert!(
        fx.revisions
            .list_revisions(&fx.company, "greeter")
            .await
            .unwrap()
            .is_empty(),
        "history cascaded"
    );
    assert!(
        fx.events()
            .iter()
            .any(|e| matches!(e, CompanyEvent::WorkflowDeleted { name, .. } if name == "Greeter")),
        "{:?}",
        fx.events()
    );
}

// ---------------------------------------------------------------------------
// 6. unknown / bodiless / corrupt ids
// ---------------------------------------------------------------------------

/// An id nobody answers for gets `run_workflow`'s steer, not a raw error.
#[tokio::test]
async fn an_unknown_id_steers_to_the_workflows_list() {
    let fx = Fixture::new();
    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "nope" }))
        .await
        .unwrap();
    assert!(
        err_text(&read).contains("Check the workflows list"),
        "{}",
        err_text(&read)
    );

    let deleted = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "nope" }))
        .await
        .unwrap();
    assert!(
        err_text(&deleted).contains("check the workflows list"),
        "{}",
        err_text(&deleted)
    );
}

/// A stored body that no longer parses still answers a read — with its token
/// and its editable flag — so an unreadable workflow does not become an
/// unremovable one.
#[tokio::test]
async fn a_corrupt_body_still_reads_a_version_and_still_deletes() {
    let fx = Fixture::new();
    fx.put_overlay("broken", "this is not = valid toml [[[")
        .await;

    let read = ReadWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "broken" }))
        .await
        .unwrap();
    let payload = data(&read);
    assert_eq!(payload["readable"], json!(false));
    assert_eq!(payload["editable"], json!(true));
    assert!(payload["version"].is_string(), "{payload}");
    assert!(md(&read).contains(DELETE_WORKFLOW_TOOL), "{}", md(&read));

    let deleted = DeleteWorkflowTool::new(fx.admin())
        .execute(json!({ "id": "broken", "expected_version": payload["version"] }))
        .await
        .unwrap();
    assert!(!deleted.is_error, "{}", err_text(&deleted));
    assert!(fx.overlays().await.is_empty());
}

// ---------------------------------------------------------------------------
// 7. the #682 non-weakening pin — THE test that matters most
// ---------------------------------------------------------------------------

/// An agent edit is held to the console's own author-time validation (#682).
///
/// This is the constraint the whole issue is about: a second authoring surface
/// that skipped per-kind config enforcement would reopen exactly the hole #661
/// exists to close. Both shapes below are ones the pre-#682 code accepted and
/// that fail silently at run time.
#[tokio::test]
async fn an_agent_edit_cannot_bypass_the_per_kind_config_rules() {
    let fx = Fixture::new();
    let store: Arc<dyn CompanyStore> = fx.store.clone();
    let draft = crate::company::RawWorkflow::try_from(
        serde_json::from_value::<CreateWorkflowArgs>(graph_args("greeter", "Greeter", "Worker"))
            .unwrap(),
    )
    .unwrap();
    crate::company::create_company_workflow(
        &fx.company,
        Some(fx.source_dir()),
        &store,
        None,
        draft,
        None,
        None,
    )
    .await
    .expect("creates");
    let version = crate::company::workflow_version(&fx.overlays().await[0].toml);

    // A `condition` node with no `field` — the shape 19 shipped seeds carried
    // before #682 repaired them.
    let fieldless = json!({
        "id": "greeter",
        "name": "Greeter",
        "expected_version": version,
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            { "id": "check", "kind": "condition", "name": "Check" },
            { "id": "done", "kind": "output", "name": "Report" }
        ],
        "edges": [
            { "from": "start", "to": "check" },
            { "from": "check", "to": "done", "label": "yes" }
        ]
    });
    let result = UpdateWorkflowTool::new(fx.admin())
        .execute(fieldless)
        .await
        .unwrap();
    let text = err_text(&result);
    assert!(text.contains("field"), "{text}");

    // A `tool_call` node with no `slug`.
    let slugless = json!({
        "id": "greeter",
        "name": "Greeter",
        "expected_version": version,
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            { "id": "call", "kind": "tool_call", "name": "Call", "config": { "args": {} } },
            { "id": "done", "kind": "output", "name": "Report" }
        ],
        "edges": [
            { "from": "start", "to": "call" },
            { "from": "call", "to": "done" }
        ]
    });
    let result = UpdateWorkflowTool::new(fx.admin())
        .execute(slugless)
        .await
        .unwrap();
    assert!(err_text(&result).contains("slug"), "{}", err_text(&result));

    // Neither reached the record.
    let reloaded =
        crate::company::load_workflow_union(Some(fx.source_dir()), &fx.overlays().await, "greeter")
            .unwrap()
            .unwrap();
    assert_eq!(reloaded.nodes[1].name, "Worker", "the original survived");
}

// ---------------------------------------------------------------------------
// 8. deployment without a revision store
// ---------------------------------------------------------------------------

/// With no revision store, the writes refuse rather than write with no undo.
#[tokio::test]
async fn without_a_revision_store_the_writes_refuse_rather_than_lose_the_prior_body() {
    let fx = Fixture::new();
    fx.put_overlay("greeter", SEED_TOML).await;

    let mut graph = graph_args("greeter", "Greeter", "Worker");
    graph["expected_version"] = json!("whatever");
    let updated = UpdateWorkflowTool::new(fx.admin_without_revisions())
        .execute(graph)
        .await
        .unwrap();
    assert!(
        err_text(&updated).contains("isn't available on this deployment"),
        "{}",
        err_text(&updated)
    );

    let deleted = DeleteWorkflowTool::new(fx.admin_without_revisions())
        .execute(json!({ "id": "greeter" }))
        .await
        .unwrap();
    assert!(err_text(&deleted).contains("isn't available on this deployment"));
    assert_eq!(fx.overlays().await.len(), 1, "nothing was touched");
}

// ---------------------------------------------------------------------------
// 9. policy
// ---------------------------------------------------------------------------

/// The gating split, asserted at the classifier rather than read off the table.
#[test]
fn only_the_delete_parks_and_none_of_the_three_is_ever_grantable() {
    use crate::policy::consequence::consequence_of;
    let args = json!({});

    let read = consequence_of(READ_WORKFLOW_TOOL, &args);
    let update = consequence_of(UPDATE_WORKFLOW_TOOL, &args);
    let delete = consequence_of(DELETE_WORKFLOW_TOOL, &args);

    // Reads and edits run; the removal parks, under BOTH tiers.
    assert!(!read.reach.parks_under_supervision());
    assert!(!update.reach.parks_under_supervision());
    assert!(delete.reach.parks_under_supervision());
    assert!(delete.parks_under_auto());
    assert!(!read.parks_under_auto());
    assert!(!update.parks_under_auto());

    // `readonly` denies the removal and permits the other two.
    assert!(delete.reach.denied_under_readonly());
    assert!(!read.reach.denied_under_readonly());
    assert!(!update.reach.denied_under_readonly());

    // None of the three may be granted standing: a week-long licence to delete
    // workflows is not a sentence an operator can consent to, and the other two
    // never park so a standing grant on them would be unobservable.
    for tool in [
        READ_WORKFLOW_TOOL,
        UPDATE_WORKFLOW_TOOL,
        DELETE_WORKFLOW_TOOL,
    ] {
        assert!(
            !consequence_of(tool, &args).standing.is_grantable(),
            "{tool} must not be grantable"
        );
    }
}

// ---------------------------------------------------------------------------
// 10. the descriptions carry the routing
// ---------------------------------------------------------------------------

/// The three descriptions are the only place a model learns the read-first
/// contract and the permanence of the delete, so the words are pinned.
#[test]
fn the_descriptions_route_the_model_and_name_the_contract() {
    let fx = Fixture::new();
    let read = ReadWorkflowTool::new(fx.admin());
    let update = UpdateWorkflowTool::new(fx.admin());
    let delete = DeleteWorkflowTool::new(fx.admin());

    // The read is routed away from the two tools it is easily confused with.
    assert!(read.description().contains("query_company"));
    assert!(read.description().contains("run_workflow"));
    assert!(read.description().contains("update_workflow"));

    // The update names the read-first contract and the required token.
    assert!(update.description().contains("read_workflow"));
    assert!(update.description().contains("expected_version"));
    assert!(update.description().contains("REQUIRED"));
    assert!(update.description().contains("full replacement"));

    // The delete leads with permanence, and points fixes at the update.
    assert!(delete.description().contains("CANNOT be undone"));
    assert!(delete.description().contains("update_workflow"));

    // The update advertises the same graph shape it deserializes.
    let schema = update.parameters_schema();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(required.contains(&"expected_version"), "{schema}");
    assert!(schema["properties"]["nodes"].is_object(), "{schema}");
    assert!(schema["properties"]["edges"].is_object(), "{schema}");
}

// ---------------------------------------------------------------------------
// 11. one record load, not two (globals opt-out)
// ---------------------------------------------------------------------------

/// A company that disabled a global workflow must never see it through
/// `read_workflow`, even when a *second* read of the record — which the tool
/// must not perform — would fail.
///
/// Before the fix, `read_workflow`'s fallback path re-loaded the record just
/// to fetch `[globals].disable`, and turned a failed second load into an
/// empty (i.e. nothing-disabled) list. With
/// [`FailsAfterFirstLoadStore`] failing every load after the first, that bug
/// would surface the disabled global's graph as a success; the fix reads the
/// overlay body and the disable list from one load, so only one `load` call
/// ever happens and the global stays hidden.
#[tokio::test]
async fn a_disabled_global_stays_hidden_even_if_a_second_read_would_fail() {
    let dropped = crate::globals::workflows()[0].id.clone();
    let company = CompanyId::new("acme");
    let manifest: CompanyManifest = toml::from_str(&format!(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"assistant\"\nrole = \"Assistant\"\n\n\
         [globals]\ndisable = [\"workflow:{dropped}\"]\n"
    ))
    .expect("valid manifest");
    let record = CompanyRecord {
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
    };
    let store: Arc<dyn CompanyStore> = Arc::new(FailsAfterFirstLoadStore::seeded(record));
    let admin = WorkflowAdmin::new(company, None, store, None, None);
    let read = ReadWorkflowTool::new(admin);

    let result = read
        .execute(json!({ "id": dropped }))
        .await
        .expect("tool call succeeds");
    assert!(
        result.is_error,
        "a company-disabled global must stay hidden, not succeed with its graph: {result:?}"
    );
    assert!(
        err_text(&result).contains("No workflow"),
        "{}",
        err_text(&result)
    );
}
