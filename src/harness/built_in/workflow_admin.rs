//! Read, edit and remove a saved workflow from an agent turn (issue #661, M7).
//!
//! Before this module the orchestrator could *author* a workflow
//! ([`CreateWorkflowTool`](crate::harness::orchestrator::CreateWorkflowTool),
//! issue #112) and *run* one
//! ([`RunWorkflowTool`](crate::harness::orchestrator::RunWorkflowTool), issue
//! #67) — and nothing else. An agent that authored a graph with a wrong node,
//! or that was asked to change one, had no move: it could only create a second
//! workflow beside the broken one, under a different id and a different name,
//! forever. The company layer had the writes all along
//! ([`update_company_workflow`] / [`delete_company_workflow`]); only the tool
//! surface was missing.
//!
//! Three tools, not two:
//!
//! * [`READ_WORKFLOW_TOOL`] — the graph of one workflow, in the **same JSON
//!   [`UPDATE_WORKFLOW_TOOL`] accepts**, plus its `version` token.
//! * [`UPDATE_WORKFLOW_TOOL`] — full-graph replacement, with a **required**
//!   `expected_version`.
//! * [`DELETE_WORKFLOW_TOOL`] — remove one workflow and its revision history.
//!
//! # Why the read tool is not optional
//!
//! `update` is a whole-graph replacement — the same shape create takes — and
//! `query_company` lists a workflow's id and name and nothing else. Without a
//! read, every edit would be a **blind rewrite**: the agent would have to
//! reconstruct a graph it has never seen from the sentence that asked it to
//! change one thing, and whatever it failed to guess would be gone. Shipping
//! the two writes alone would have opened a fresh instance of exactly the class
//! issue #661 exists to close.
//!
//! The required `expected_version` is what turns "read first" from advice into
//! a mechanical property. The token is a hash of the stored body
//! ([`workflow_version`]) and the read is the only place to get one, so an
//! agent that never read the graph has nothing to send; and a console edit that
//! lands in between is a `409` carrying the company layer's own "Reload it to
//! see the latest" message rather than a silent clobber.
//!
//! # What these tools refuse, and why the refusal lives here
//!
//! An agent-authored graph is **manual-run only** — a deliberate, argued
//! narrowing of the create schema (see `CreateWorkflowArgs`): `schedule`,
//! `on_error`, `retry`, `requires_approval` and `repeatable` are unattended-run
//! policy an
//! operator decides. Create can honour that by simply never emitting them.
//! A full-replacement *edit* cannot: replaying a graph back through the narrow
//! schema drops whatever policy the stored body carried, and dropping a
//! `requires_approval` removes an operator's gate rather than forgetting a
//! field.
//!
//! So the write tools refuse a target whose stored body carries any of it —
//! [`refuse_scheduled`] on both, [`refuse_unexpressible_policy`] on update —
//! and say which node and which field, so the answer is "go do that bit in the
//! console", not "it didn't work".
//!
//! Two more things fall out of the schedule half of that refusal, and both are
//! deliberate:
//!
//! * It **closes the agent-driven half of #708** (a delete-then-recreate
//!   inheriting the old schedule's fire ledger) with no work: an agent cannot
//!   delete a scheduled workflow, and cannot recreate one *with* a schedule
//!   because the authoring schema has no field for it. #708 stays open for the
//!   console path, which is where its real fix belongs.
//! * The guard lives **in the tools, not in the company layer**, so console
//!   behaviour is untouched and it is one line to lift per field if agent
//!   self-scheduling ever becomes a yes.
//!
//! It is a steering gate, not a security boundary: it reads the record outside
//! the company layer's write lock, so a console edit racing it wins. That is
//! fine — the console may arm a schedule at any time by design, and nothing
//! here is the enforcement point for anything.
//!
//! # Seeds are already protected, and that is inherited rather than re-checked
//!
//! `locate_editable_overlay` in `workflow_create.rs` is the single resolver
//! behind both writes: seed-backed → `409`, enabled-but-bodiless → `409`,
//! unknown → `404`, with the `is_safe_workflow_id` traversal guard in front. An
//! agent can never edit or delete a shipped seed regardless of what a tool
//! does, so these tools pass that message through verbatim instead of forking
//! it — [`seed_file_exists`]'s own doc warns that duplicated probes are how the
//! three surfaces drift. (The message says "from the console", which reads
//! slightly oddly in an agent's mouth; that is wording debt on the shared
//! string, not a reason to grow a second copy of it.)
//!
//! # The console needs no change
//!
//! These tools write through the same company functions the REST routes call
//! and journal the same [`WorkflowUpdated`](crate::ports::types::CompanyEvent)
//! / `WorkflowDeleted` events, so the workflow list, the editor and the events
//! feed all pick the change up with nothing new wired. The editor's own
//! `expected_version` `409` already protects an operator mid-edit from any
//! concurrent writer, agent or otherwise.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core::openhuman as oh;

use crate::company::{
    RawWorkflow, WorkflowSpecProjection, delete_company_workflow, load_workflow_with_globals,
    project_workflow_spec, raw_workflow_from_toml, seed_file_exists, update_company_workflow,
    workflow_version,
};
use crate::error::OpenCompanyError;
use crate::harness::build::TOOL_RESULT_BUDGET_BYTES;
use crate::harness::orchestrator::CreateWorkflowArgs;
use crate::ports::types::{CompanyId, OverlayWorkflow};
use crate::ports::workflow_revisions::WorkflowRevisionStore;
use crate::ports::{CompanyStore, EventLog};

/// Tool name: read one saved workflow's graph, version token and editability.
pub const READ_WORKFLOW_TOOL: &str = "read_workflow";
/// Tool name: replace one saved workflow's graph wholesale.
pub const UPDATE_WORKFLOW_TOOL: &str = "update_workflow";
/// Tool name: remove one saved workflow permanently.
pub const DELETE_WORKFLOW_TOOL: &str = "delete_workflow";

/// How much of the budget one rendered graph may take. Half, so the reply that
/// quotes a graph back still has room for the sentence explaining it.
const GRAPH_RENDER_BUDGET_BYTES: usize = TOOL_RESULT_BUDGET_BYTES / 2;

// ---------------------------------------------------------------------------
// The shared handle
// ---------------------------------------------------------------------------

/// The wiring all three tools share: which company, where its seed graphs live,
/// and the three ports the company-layer writes need.
///
/// Cloned into each tool rather than referenced, so the belt owns its tools
/// outright — the same shape every other orchestrator tool has.
#[derive(Clone)]
pub struct WorkflowAdmin {
    company: CompanyId,
    /// The company source directory (`companies/<name>`), whose `workflows/`
    /// subtree contributes the seed graphs. Read-only here: it decides what is
    /// *shadowed*, never what is written.
    source_dir: Option<PathBuf>,
    store: Arc<dyn CompanyStore>,
    /// Issue #274's snapshot ring. `None` means no revision store is wired, in
    /// which case the two write tools refuse rather than pretend — an update
    /// with nowhere to snapshot the prior body to is precisely the
    /// unrecoverable edit that feature exists to prevent.
    revisions: Option<Arc<dyn WorkflowRevisionStore>>,
    events: Option<Arc<dyn EventLog>>,
}

impl WorkflowAdmin {
    /// Builds the shared handle. `revisions` is `Option` for the same reason
    /// `events` is: the default build and the tools' own tests wire neither.
    pub fn new(
        company: CompanyId,
        source_dir: Option<PathBuf>,
        store: Arc<dyn CompanyStore>,
        revisions: Option<Arc<dyn WorkflowRevisionStore>>,
        events: Option<Arc<dyn EventLog>>,
    ) -> Self {
        Self {
            company,
            source_dir,
            store,
            revisions,
            events,
        }
    }

    /// This company's runtime-authored graph bodies, or the agent-facing error
    /// to return when the record cannot be read.
    async fn overlays(&self) -> Result<Vec<OverlayWorkflow>, ToolResult> {
        Ok(self.overlays_and_globals().await?.0)
    }

    /// [`overlays`](Self::overlays), with the company's `[globals].disable`
    /// beside them.
    ///
    /// Read from the same record load, because the two are read together: a
    /// union that saw the overlays but not the opt-out would answer with a
    /// global graph this company disabled.
    async fn overlays_and_globals(
        &self,
    ) -> Result<(Vec<OverlayWorkflow>, Vec<String>), ToolResult> {
        match self.store.load(&self.company).await {
            Ok(record) => Ok(record
                .map(|r| (r.overlay_workflows, r.manifest.globals.disable))
                .unwrap_or_default()),
            Err(err) => Err(ToolResult::error(format!(
                "Couldn't read this company's saved workflows: {err}"
            ))),
        }
    }

    /// The revision store, or the refusal for a deployment without one.
    fn revisions(&self) -> Result<&Arc<dyn WorkflowRevisionStore>, ToolResult> {
        self.revisions.as_ref().ok_or_else(|| {
            ToolResult::error(
                "Changing saved workflows isn't available on this deployment (no workflow \
                 revision history is wired, so an edit could not be undone).",
            )
        })
    }
}

/// The `id` argument, trimmed, accepting `workflow` as an alias — the
/// [`RunWorkflowTool`](crate::harness::orchestrator::RunWorkflowTool)
/// convention, so the three workflow tools take the same word for the same
/// thing.
fn id_arg(args: &Value) -> Option<String> {
    args.get("id")
        .or_else(|| args.get("workflow"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(str::to_string)
}

/// The `expected_version` argument, accepting the camelCase spelling the
/// console's REST body uses.
///
/// Read off the raw `Value` rather than declared on a serde struct on purpose:
/// `serde(alias)` hard-fails when a model sends *both* spellings, which is a
/// deserialization trace in place of an actionable message for a caller that
/// did in fact supply the token.
fn expected_version_arg(args: &Value) -> Option<String> {
    args.get("expected_version")
        .or_else(|| args.get("expectedVersion"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The stored overlay TOML for `wid`, when this company has one.
fn overlay_body<'a>(overlays: &'a [OverlayWorkflow], wid: &str) -> Option<&'a str> {
    overlays
        .iter()
        .find(|w| w.id == wid)
        .map(|w| w.toml.as_str())
}

/// Refuse a target that fires on a schedule.
///
/// Both write tools, one gate — see the module docs for the two reasons
/// (agent-authored graphs are manual-run only; and it closes the agent-driven
/// half of #708 outright).
fn refuse_scheduled(wid: &str, projection: &WorkflowSpecProjection, verb: &str) -> Option<String> {
    let cron = projection.schedule.as_deref()?;
    Some(format!(
        "Refused: workflow `{wid}` runs on a schedule (`{cron}`), and a scheduled workflow is the \
         operator's to change — {verb} it in the console. Workflows you author here are \
         manual-run only."
    ))
}

/// Refuse an *edit* to a graph carrying node policy the authoring schema cannot
/// express, naming every node and field.
///
/// Only `update` calls this. A delete removes the whole workflow, so there is
/// nothing to silently drop; an edit replays the graph through a narrower shape
/// and would drop it.
fn refuse_unexpressible_policy(
    wid: &str,
    projection: &WorkflowSpecProjection,
    console_hint: bool,
) -> Option<String> {
    if projection.unexpressible.is_empty() {
        return None;
    }
    let tail = if console_hint {
        " Edit it in the console instead, where those fields exist."
    } else {
        ""
    };
    Some(format!(
        "Refused: workflow `{wid}` carries per-node run policy this tool can't write back — {}. \
         Replacing the graph from here would silently drop it — `requires_approval` is an \
         operator's approval gate, and `repeatable` stops an approval sending the same thing \
         twice.{tail}",
        projection.unexpressible_summary()
    ))
}

/// Turn a company-layer error into the sentence the agent should read.
///
/// `InvalidRequest` / `Conflict` / `CompanyNotFound` all carry prosumer text
/// already (the id guard, the seed refusal, the stale-token message, the #682
/// per-kind config problems); anything else is an infrastructure failure the
/// agent can only retry.
fn detail_of(err: &OpenCompanyError) -> String {
    match err {
        OpenCompanyError::InvalidRequest(message) | OpenCompanyError::Conflict(message) => {
            message.clone()
        }
        // A structured workflow rejection (issue #1016) also carries prosumer
        // text — its `Display` joins every problem's message — so the agent reads
        // the same actionable sentence the flat #682 path gave it.
        OpenCompanyError::WorkflowInvalid { .. } => err.to_string(),
        OpenCompanyError::CompanyNotFound(what) => {
            format!("no {what} exists — check the workflows list for valid ids.")
        }
        _ => "the company couldn't save it right now; try again.".to_string(),
    }
}

/// Render a graph as a fenced JSON block, or explain why it is not rendered.
///
/// Pretty-printed while it fits, compact when it does not, and refused past
/// [`GRAPH_RENDER_BUDGET_BYTES`] — a graph half-quoted into a fence is worse
/// than one the agent is told to open in the console, because the agent would
/// hand the truncation straight back as an edit.
fn render_graph(spec: &Value) -> String {
    let pretty = serde_json::to_string_pretty(spec).unwrap_or_default();
    if pretty.len() <= GRAPH_RENDER_BUDGET_BYTES {
        return format!("```json\n{pretty}\n```");
    }
    let compact = spec.to_string();
    if compact.len() <= GRAPH_RENDER_BUDGET_BYTES {
        return format!("```json\n{compact}\n```");
    }
    "_This graph is too large to quote here. Open it in the console._".to_string()
}

// ---------------------------------------------------------------------------
// read_workflow
// ---------------------------------------------------------------------------

/// Read one saved workflow's graph, its version token and whether it can be
/// changed from here.
///
/// The graph comes back in exactly the shape [`UpdateWorkflowTool`] accepts, so
/// the read → edit → write loop needs no reshaping step and cannot drift from
/// the writer's schema. `editable` is the same [`seed_file_exists`] probe the
/// console's Edit button reads, so the agent learns a graph is source-defined
/// *before* attempting a write rather than from a `409` after.
///
/// A stored body that no longer parses still answers — with its `version`, its
/// `editable` flag and a note — because that is the graph an operator most
/// wants deleted, and an unreadable one must not become an unremovable one.
pub struct ReadWorkflowTool {
    admin: WorkflowAdmin,
}

impl ReadWorkflowTool {
    /// Builds the read tool over the shared handle.
    pub fn new(admin: WorkflowAdmin) -> Self {
        Self { admin }
    }
}

#[async_trait]
impl Tool for ReadWorkflowTool {
    fn name(&self) -> &str {
        READ_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Read one saved workflow's full graph by id, in the exact shape `update_workflow` accepts, plus the `version` token that tool requires. USE FOR seeing what a workflow actually does before changing it, and to get the `expected_version` for an edit — always read before `update_workflow`. NOT for listing what workflows exist (use `query_company`) and NOT for running one (use `run_workflow`). The reply also says whether the workflow is `editable` (a workflow shipped in the company's source tree is not), whether its schedule is armed, and names any per-node run policy (`on_error`, `retry`, `requires_approval`, `repeatable`) or trigger schedule that only the console can change."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the workflow to read (see the workflows list for valid ids)."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(wid) = id_arg(&args) else {
            return Ok(ToolResult::error(
                "`id` is required: pass the id of the workflow to read (see the workflows list).",
            ));
        };

        // Loaded once, together: the fallback below needs `disable` beside
        // `overlays`, and a second, later load of the same record could see a
        // globals opt-out this first load missed (or the reverse), letting a
        // company-disabled global slip through — see `overlays_and_globals`.
        let (overlays, disable) = match self.admin.overlays_and_globals().await {
            Ok(pair) => pair,
            Err(result) => return Ok(result),
        };
        let stored = overlay_body(&overlays, &wid);
        // The same three-way answer the console's list computes, from the same
        // probe: a graph is editable when this company owns an overlay body for
        // it AND no source file shadows that body.
        let seed_backed = seed_file_exists(self.admin.source_dir.as_deref(), &wid);
        let editable = stored.is_some() && !seed_backed;
        let version = editable.then(|| workflow_version(stored.unwrap_or_default()));

        // A body that no longer parses is answered rather than errored: the
        // version and the editable flag are exactly what a delete needs, and a
        // graph nobody can read is the one most likely to want deleting.
        let raw = match stored.map(raw_workflow_from_toml) {
            Some(Ok(raw)) => Some(raw),
            Some(Err(err)) => {
                let md = format!(
                    "Workflow **`{wid}`** is saved but its stored graph can't be read ({err}). \
                     It can't be edited from here in that state; `{DELETE_WORKFLOW_TOOL}` still \
                     removes it, or an operator can repair it in the console."
                );
                return Ok(ToolResult::success_with_markdown(
                    json!({
                        "workflow": wid,
                        "editable": editable,
                        "version": version,
                        "readable": false,
                    }),
                    md,
                ));
            }
            None => None,
        };

        // No overlay body: either a seed graph (readable through the union) or
        // an id this company does not answer for at all.
        let Some(raw) = raw else {
            return Ok(self.read_seed_or_unknown(&overlays, &disable, &wid).await);
        };

        let projection = project_workflow_spec(&raw);
        let enabled = self.enabled_flag(&wid).await;
        Ok(read_result(&wid, &projection, version, editable, enabled))
    }
}

impl ReadWorkflowTool {
    /// Answer for an id with no overlay body: read it through the seed ∪
    /// overlay union so a source-defined graph is still *readable* (it is only
    /// unwritable), and give the unknown case `run_workflow`'s own steer.
    async fn read_seed_or_unknown(
        &self,
        overlays: &[OverlayWorkflow],
        disable: &[String],
        wid: &str,
    ) -> ToolResult {
        let file = match load_workflow_with_globals(
            self.admin.source_dir.as_deref(),
            overlays,
            disable,
            wid,
        ) {
            Ok(file) => file,
            Err(err) => {
                return ToolResult::error(format!("Couldn't load workflow `{wid}`: {err}"));
            }
        };
        let Some(file) = file else {
            return ToolResult::error(format!(
                "No workflow with id `{wid}` exists. Check the workflows list for valid ids."
            ));
        };
        // Back through the same projection the overlay path uses, so a seed and
        // an overlay read identically apart from `editable`.
        let projection = project_workflow_spec(&seed_draft(&file));
        let enabled = self.enabled_flag(wid).await;
        read_result(wid, &projection, None, false, enabled)
    }

    /// Whether this workflow's schedule is armed, per the company record.
    ///
    /// A read that cannot reach the record reports `None` rather than guessing
    /// `false` — "not armed" and "not known" are different answers, and only
    /// one of them is safe to act on.
    async fn enabled_flag(&self, wid: &str) -> Option<bool> {
        let record = self.admin.store.load(&self.admin.company).await.ok()??;
        Some(record.manifest.workflows.enabled.iter().any(|id| id == wid))
    }
}

/// Rebuild a [`RawWorkflow`] draft from a parsed [`WorkflowFile`], for the seed
/// read path (which has no stored TOML of its own to project).
fn seed_draft(file: &crate::company::WorkflowFile) -> RawWorkflow {
    RawWorkflow {
        id: file.id.clone(),
        name: file.name.clone(),
        description: file.description.clone(),
        // Issue #1862 prerequisite: carried, not dropped — the same reason
        // `repeatable` below is: this is a round trip of a stored graph, so
        // losing the owning desk here would clear it as a side effect of an
        // agent merely reading the workflow.
        owner_desk: file.owner_desk.clone(),
        nodes: file
            .nodes
            .iter()
            .map(|n| crate::company::RawNode {
                id: n.id.clone(),
                kind: n.kind.as_str().to_string(),
                name: n.name.clone(),
                summary: n.summary.clone(),
                agent: n.agent.clone(),
                schedule: n.schedule.clone(),
                config: n
                    .config
                    .as_ref()
                    .and_then(|c| toml::Value::try_from(c).ok()),
                on_error: n.on_error.clone(),
                retry: n.retry.clone(),
                requires_approval: n.requires_approval,
                // Carried, not dropped: this is a round trip of a stored graph,
                // so losing a `repeatable = false` here would remove an
                // operator's repeat guard (issue #850) as a side effect of an
                // agent reading the workflow.
                repeatable: n.repeatable,
                destination: n.destination.clone(),
                // Carried, not dropped, for the same reason as `repeatable`
                // above (issue #1866 review): this is a round trip of a
                // stored graph, so silently clearing an existing
                // `postcondition` here would both discard the run-safety
                // gate AND make `project_workflow_spec`'s `unexpressible`
                // residue falsely report no postcondition policy, even
                // though the runtime still enforces one.
                postcondition: n.postcondition.clone(),
            })
            .collect(),
        edges: file
            .edges
            .iter()
            .map(|e| crate::company::RawEdge {
                from: e.from.clone(),
                to: e.to.clone(),
                label: e.label.clone(),
            })
            .collect(),
    }
}

/// The successful `read_workflow` reply — one shape for the overlay and the
/// seed paths, so the two can never describe the same graph differently.
fn read_result(
    wid: &str,
    projection: &WorkflowSpecProjection,
    version: Option<String>,
    editable: bool,
    enabled: Option<bool>,
) -> ToolResult {
    let mut md = format!(
        "Workflow **`{wid}`**\n\n{}\n",
        render_graph(&projection.spec)
    );

    match (&version, editable) {
        (Some(version), true) => md.push_str(&format!(
            "\n`version`: `{version}` — pass this as `expected_version` to `{UPDATE_WORKFLOW_TOOL}`.\n"
        )),
        _ => md.push_str(
            "\nThis workflow is defined by a file in the company's source tree, so it can't be \
             changed or removed from here — an operator edits it in the company repository.\n",
        ),
    }
    if let Some(enabled) = enabled {
        md.push_str(if enabled {
            "Its schedule (if any) is armed.\n"
        } else {
            "It is switched off: still runnable by hand, but it does not fire on its own.\n"
        });
    }
    if let Some(cron) = &projection.schedule {
        md.push_str(&format!(
            "It runs on a schedule (`{cron}`), so it can only be changed in the console.\n"
        ));
    }
    if !projection.unexpressible.is_empty() {
        md.push_str(&format!(
            "Per-node run policy only the console can change: {}. While it is set, \
             `{UPDATE_WORKFLOW_TOOL}` refuses this workflow rather than dropping it.\n",
            projection.unexpressible_summary()
        ));
    }

    ToolResult::success_with_markdown(
        json!({
            "workflow": projection.spec,
            "version": version,
            "editable": editable,
            "enabled": enabled,
            "schedule": projection.schedule,
            "readable": true,
        }),
        md,
    )
}

// ---------------------------------------------------------------------------
// update_workflow
// ---------------------------------------------------------------------------

/// Replace one saved workflow's graph wholesale.
///
/// The payload is [`CreateWorkflowTool`](crate::harness::orchestrator::CreateWorkflowTool)'s
/// schema — reusing that struct and its `TryFrom`, so the two authoring
/// surfaces cannot drift — plus a **required** `expected_version`. Everything
/// past the parse is [`update_company_workflow`]: the same validation, the same
/// #274 snapshot inside the write lock, the same #682 per-kind config
/// enforcement, the same seed refusal and the same optimistic-concurrency
/// `409`.
///
/// See the module docs for what it refuses and why that refusal lives here.
pub struct UpdateWorkflowTool {
    admin: WorkflowAdmin,
}

impl UpdateWorkflowTool {
    /// Builds the update tool over the shared handle.
    pub fn new(admin: WorkflowAdmin) -> Self {
        Self { admin }
    }
}

#[async_trait]
impl Tool for UpdateWorkflowTool {
    fn name(&self) -> &str {
        UPDATE_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Replace a saved workflow's whole graph — use this to FIX a workflow that is wrong rather than creating a second one beside it. You must call `read_workflow` first: this is a full replacement (anything you leave out is gone), and `expected_version` is REQUIRED and only comes from that read. Send the same `{id, name, description, ownerDesk, nodes, edges}` shape `create_workflow` takes, with `id` naming the workflow to replace. Omit `ownerDesk` to leave the current owning desk untouched; send a desk id/name to assign or move it; send `null` or an empty string to explicitly unassign it. NOT for making a new workflow (use `create_workflow`), NOT for removing one (use `delete_workflow`), NOT for running one (use `run_workflow`). Workflows shipped in the company's source tree, workflows that run on a schedule, and workflows whose nodes carry run policy (`on_error`, `retry`, `requires_approval`, `repeatable`) are refused here — those are the operator's to change in the console. If the workflow changed since you read it the edit is refused; read it again and reapply."
    }

    fn parameters_schema(&self) -> Value {
        let mut schema = create_graph_schema();
        let properties = schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("create graph schema has properties");
        properties.insert(
            "expected_version".to_string(),
            json!({
                "type": "string",
                "description": "REQUIRED. The `version` token from `read_workflow` for this workflow. The edit is refused if the workflow changed since that read."
            }),
        );
        schema["required"] = json!(["id", "name", "nodes", "expected_version"]);
        schema
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // The token first: an agent that skipped the read has nothing to send,
        // and telling it *that* is more useful than a graph-shape complaint.
        let Some(expected_version) = expected_version_arg(&args) else {
            return Ok(ToolResult::error(format!(
                "`expected_version` is required: call `{READ_WORKFLOW_TOOL}` for this workflow \
                 first and pass back the `version` it returns. This tool replaces the whole \
                 graph, so editing one you haven't read would drop the rest of it."
            )));
        };
        // PR #1882 review (bot finding on the `owner_desk.is_none()` fallback
        // below): a caller who explicitly sends `"ownerDesk": null` or an
        // all-whitespace string to unassign a workflow is indistinguishable,
        // after parsing, from one who never mentioned the field at all — both
        // normalize to `None` on `draft.owner_desk`. Recorded here, on the raw
        // JSON, before `args` is consumed: presence of the key is the caller's
        // signal that they thought about ownership at all, whatever value they
        // sent it as.
        let owner_desk_mentioned = args.get("ownerDesk").is_some();
        let parsed = match serde_json::from_value::<CreateWorkflowArgs>(args) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::debug!(company = %self.admin.company, error = %err, "update_workflow: unreadable args");
                return Ok(ToolResult::error(format!(
                    "Couldn't read the workflow definition: {err}. Provide `id`, `name`, and \
                     `nodes` (with an `edges` list), exactly as `{READ_WORKFLOW_TOOL}` returned \
                     them."
                )));
            }
        };
        let mut draft: RawWorkflow = match RawWorkflow::try_from(parsed) {
            Ok(draft) => draft,
            Err(msg) => {
                tracing::debug!(company = %self.admin.company, error = %msg, "update_workflow: unstorable config");
                return Ok(ToolResult::error(msg));
            }
        };
        let wid = draft.id.clone();

        let revisions = match self.admin.revisions() {
            Ok(revisions) => revisions.clone(),
            Err(result) => return Ok(result),
        };

        // The agent-surface gate, read off the body being replaced. Skipped
        // entirely when the stored body no longer parses: an unreadable graph
        // has no policy anyone could have reviewed, and the company layer's own
        // rules (#276's disarm, the seed refusal) still apply underneath.
        let overlays = match self.admin.overlays().await {
            Ok(overlays) => overlays,
            Err(result) => return Ok(result),
        };
        if let Some(stored) = overlay_body(&overlays, &wid)
            && let Ok(raw) = raw_workflow_from_toml(stored)
        {
            // Issue #1882 review (PR #1882 bot finding): `ownerDesk` is now on
            // this tool's schema (`create_workflow_parameters_schema`) and
            // `RawWorkflow::try_from(CreateWorkflowArgs)` already resolves and
            // normalizes whatever the caller sent, so only fall back to the
            // STORED value when the caller left `owner_desk` unset. An
            // unconditional overwrite here used to be correct — the schema had
            // no such field — but had gone stale into a bug: it silently
            // discarded a desk the caller explicitly supplied, reporting
            // success while the reassignment never took. Falling back only on
            // `None` still protects the original case this guarded: an agent
            // editing an unrelated node on an owner-assigned workflow, without
            // ever mentioning `ownerDesk`, must not clear the desk an operator
            // (or the workflow-proposal apply path) had already set.
            //
            // PR #1882 review (bot finding, second pass): `draft.owner_desk ==
            // None` alone can't tell "never mentioned" apart from "explicitly
            // sent null/blank to unassign" — both parse to `None`, so the
            // fallback used to restore the stored desk in BOTH cases, leaving
            // no payload shape that could ever clear it. `owner_desk_mentioned`
            // (captured off the raw JSON above, before parsing) breaks the tie:
            // only fall back when the key was truly absent.
            if draft.owner_desk.is_none() && !owner_desk_mentioned {
                draft.owner_desk = raw.owner_desk.clone();
            }
            let projection = project_workflow_spec(&raw);
            if let Some(message) = refuse_scheduled(&wid, &projection, "change") {
                tracing::debug!(company = %self.admin.company, workflow = %wid, "update_workflow: refused scheduled target");
                return Ok(ToolResult::error(message));
            }
            if let Some(message) = refuse_unexpressible_policy(&wid, &projection, true) {
                tracing::debug!(company = %self.admin.company, workflow = %wid, "update_workflow: refused operator policy");
                return Ok(ToolResult::error(message));
            }
        }

        tracing::debug!(company = %self.admin.company, workflow = %wid, "update_workflow: replacing");
        // `wired_channels: None` (issue #1191) — same reason as the
        // orchestrator's `create_workflow` tool: the admin surface holds a store
        // and a revision store, not a `CompanyRuntime`, so it cannot read the
        // deliverable channel set. `None` skips the channel-destination rule
        // rather than guessing at it, leaving delivery's own refusal as the
        // backstop. Status quo, and greppable: the two agent tool surfaces are
        // the only `None` callers that are not tests or a rollback.
        match update_company_workflow(
            &self.admin.company,
            self.admin.source_dir.as_deref(),
            &self.admin.store,
            &revisions,
            self.admin.events.as_ref(),
            draft,
            Some(&expected_version),
            None,
        )
        .await
        {
            Ok(file) => {
                tracing::debug!(company = %self.admin.company, workflow = %file.id, "update_workflow: replaced");
                let md = format!(
                    "Updated workflow **{}** (`{}`). The previous version was saved to its \
                     history, so the change can be rolled back from the console. Read it again \
                     for a fresh `version` before your next edit.",
                    file.name.trim(),
                    file.id
                );
                Ok(ToolResult::success_with_markdown(
                    json!({ "workflow": file.id }),
                    md,
                ))
            }
            Err(err) => {
                tracing::debug!(company = %self.admin.company, workflow = %wid, error = %err, "update_workflow: rejected");
                Ok(ToolResult::error(format!(
                    "Couldn't update the workflow: {}",
                    detail_of(&err)
                )))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// delete_workflow
// ---------------------------------------------------------------------------

/// Remove one saved workflow permanently.
///
/// [`delete_company_workflow`] drops the graph body and its id in
/// `[workflows].enabled` in one save, then cascades the workflow's #274
/// revision history away with it. That cascade is why this tool is the one of
/// the three that carries a consequence gate: the object and its undo trail go
/// in the same call, so there is nothing left for an operator to restore from.
pub struct DeleteWorkflowTool {
    admin: WorkflowAdmin,
}

impl DeleteWorkflowTool {
    /// Builds the delete tool over the shared handle.
    pub fn new(admin: WorkflowAdmin) -> Self {
        Self { admin }
    }
}

#[async_trait]
impl Tool for DeleteWorkflowTool {
    fn name(&self) -> &str {
        DELETE_WORKFLOW_TOOL
    }

    fn description(&self) -> &str {
        "Permanently remove one saved workflow by id, together with its whole edit history — this CANNOT be undone and there is no restore afterwards. USE FOR retiring a workflow that should no longer exist. NOT for fixing one (use `update_workflow`, which keeps the old version in history) and NOT for stopping one from firing on its own (an operator switches that off in the console). Pass `expected_version` from `read_workflow` to be sure you are removing the workflow you looked at. Workflows shipped in the company's source tree and workflows that run on a schedule are refused here — those are the operator's to remove in the console."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the workflow to remove permanently (see the workflows list for valid ids)."
                },
                "expected_version": {
                    "type": "string",
                    "description": "Optional. The `version` token from `read_workflow`. When given, the delete is refused if the workflow changed since that read."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn supports_markdown(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(wid) = id_arg(&args) else {
            return Ok(ToolResult::error(
                "`id` is required: pass the id of the workflow to remove (see the workflows list).",
            ));
        };
        let expected_version = expected_version_arg(&args);

        let revisions = match self.admin.revisions() {
            Ok(revisions) => revisions.clone(),
            Err(result) => return Ok(result),
        };

        let overlays = match self.admin.overlays().await {
            Ok(overlays) => overlays,
            Err(result) => return Ok(result),
        };
        // Only the schedule gate here — a delete drops the whole workflow, so
        // there is no per-node policy it could silently strip.
        if let Some(stored) = overlay_body(&overlays, &wid)
            && let Ok(raw) = raw_workflow_from_toml(stored)
            && let Some(message) = refuse_scheduled(&wid, &project_workflow_spec(&raw), "remove")
        {
            tracing::debug!(company = %self.admin.company, workflow = %wid, "delete_workflow: refused scheduled target");
            return Ok(ToolResult::error(message));
        }

        tracing::debug!(company = %self.admin.company, workflow = %wid, "delete_workflow: removing");
        match delete_company_workflow(
            &self.admin.company,
            self.admin.source_dir.as_deref(),
            &self.admin.store,
            &revisions,
            // Issue #708: no fire-ledger purge from the tool path, and none is
            // owed — this tool refuses to delete a scheduled workflow at all
            // (`refuse_scheduled` above), so it can never orphan a ledger. Only
            // the HTTP delete path (which permits scheduled deletes) wires `Some`.
            None,
            self.admin.events.as_ref(),
            &wid,
            expected_version.as_deref(),
        )
        .await
        {
            Ok(name) => {
                tracing::debug!(company = %self.admin.company, workflow = %wid, "delete_workflow: removed");
                let md = format!(
                    "Deleted workflow **{}** (`{wid}`), along with its edit history. This cannot \
                     be undone — recreate it with `create_workflow` if it is needed again.",
                    name.trim()
                );
                Ok(ToolResult::success_with_markdown(
                    json!({ "workflow": wid, "name": name }),
                    md,
                ))
            }
            Err(err) => {
                tracing::debug!(company = %self.admin.company, workflow = %wid, error = %err, "delete_workflow: rejected");
                Ok(ToolResult::error(format!(
                    "Couldn't delete the workflow: {}",
                    detail_of(&err)
                )))
            }
        }
    }
}

/// The `{id, name, description, nodes, edges}` half of `create_workflow`'s
/// parameter schema, so [`UpdateWorkflowTool`] advertises the same graph shape
/// it actually deserializes (`CreateWorkflowArgs`) rather than a second copy
/// that can drift from it.
///
/// Built by asking the create tool for its own schema and stripping nothing:
/// the update tool then adds `expected_version` and rewrites `required`.
fn create_graph_schema() -> Value {
    crate::harness::orchestrator::create_workflow_parameters_schema()
}

#[cfg(test)]
mod tests;
