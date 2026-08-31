//! The three OC-native tools the create-time copilot agent drives (issue #840).
//!
//! PR-2 turns the create-time copilot from one tool-less `call_model` into a
//! real tool-using [`Agent`](openhuman_core::openhuman::agent::Agent). The agent
//! reasons; these tools are the only things it can DO, and every one of them is
//! read-only or host-gated — none of them runs a workflow, sends a message, or
//! persists anything:
//!
//! - [`ListEffectiveToolsTool`] grounds the model in the company's real roster
//!   and its wired `tool_call` slugs (PR-1's effective-tool set), so an `agent`
//!   step names a real teammate id and a `tool_call` step names a real slug.
//! - [`CheckWorkflowTool`] statically checks a candidate graph — the tinyflows
//!   [`gates::failures`](tinyflows::gates::failures) authoring gates plus the
//!   host's own courtesy validation — with NO run, no testkit, no dry-run
//!   sandbox (that is PR-3). An empty result is a pass.
//! - [`ProposeWorkflowTool`] is the acceptance signal. It runs the SAME host
//!   authority the old inline path did — a safe/unique id, name dedup, stripped
//!   approval gating (#460), the `DESCRIPTION_NODE_KINDS` refusal, grounding, and
//!   courtesy validation — and on a pass stashes the accepted `(summary, spec,
//!   notes)` into a shared cell the caller reads. On a failure it hands the model
//!   the specific sentences so it can fix and re-propose.
//!
//! The tools share one [`CopilotContext`] (the gathered company evidence,
//! the operator's description, and PR-1's effective / granted-but-unwired slug
//! sets) plus two cells: the accepted proposal, and the last diagnostic
//! sentences a check or propose produced — the caller folds those into a
//! [`NotAutomatable`](super::DescriptionDraftOutcome::NotAutomatable) reason when
//! the agent never lands a proposal (a cap hit, a timeout, an honest decline).

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use openhuman_core::openhuman as oh;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};

use crate::company::{
    WorkflowGraphSpec, courtesy_validate_draft, raw_workflow_from_spec, workflow_graph_from_spec,
};

use super::{
    CompanyEvidence, DESCRIPTION_NODE_KINDS, ground_and_validate, roster_line, safe_workflow_id,
    safe_workflow_name,
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything the copilot's tools read: the gathered company evidence (the
/// record, roster, existing ids/names), the operator's description (grounding for
/// the delivery and wrong-but-real gates), and PR-1's effective / granted-but-
/// unwired slug sets. Assembled once in
/// [`draft_workflow_from_description`](super::draft_workflow_from_description) and
/// shared by reference into all three tools.
pub(super) struct CopilotContext {
    pub(super) company: CompanyEvidence,
    pub(super) description: String,
    pub(super) effective_slugs: Vec<String>,
    pub(super) unwired_slugs: Vec<String>,
    /// When set, the copilot is CORRECTING an existing workflow (issue #840,
    /// PR-3) rather than drafting a new one. The host then pins the corrected
    /// spec's id/name to this saved identity — instead of minting a fresh, deduped
    /// id — so the operator's Save is a NEW VERSION of that workflow, not an
    /// orphan graph with a `-2` id. `None` for the create-from-description path.
    pub(super) fixing: Option<FixTarget>,
}

impl CopilotContext {
    /// The owning desk the correction's saved counterpart already carries, for
    /// [`courtesy_validate_draft`]'s update semantics (issue #1882 review).
    /// `None` on the create-from-description path, which has no saved body — a
    /// desk named there is a genuinely new assignment and is validated as one.
    pub(super) fn previous_owner_desk(&self) -> Option<&str> {
        self.fixing
            .as_ref()
            .and_then(|fix| fix.owner_desk.as_deref())
    }
}

/// The saved identity a fix-from-run correction must preserve (issue #840, PR-3):
/// the exact `wid` the edit route writes under, and the display name the operator
/// recognises. The model corrects the GRAPH; the host keeps the identity.
pub(super) struct FixTarget {
    pub(super) id: String,
    pub(super) name: String,
    /// The owning desk the failing workflow is saved with (issue #1882 review,
    /// PR #1882 bot finding, comment 3879878907). Like `id` and `name` this is
    /// host-owned across a correction — `ownerDesk` is not on either tool's
    /// parameter schema, so the model has no sanctioned way to change it, and a
    /// value it echoed from the evidence prompt (or dropped entirely) must not
    /// decide ownership. Pinned back onto the corrected spec, and passed to
    /// `courtesy_validate_draft` as the previous owner so an unchanged desk that
    /// has since gone stale is grandfathered exactly as the real update path
    /// grandfathers it, instead of blocking the correction of an unrelated run
    /// failure.
    pub(super) owner_desk: Option<String>,
}

/// A graph the propose tool accepted under full host authority — the value the
/// caller returns as [`DescriptionDraftOutcome::Graph`](super::DescriptionDraftOutcome::Graph).
pub(super) struct AcceptedProposal {
    pub(super) summary: String,
    pub(super) spec: WorkflowGraphSpec,
    pub(super) notes: Vec<String>,
}

/// The one-slot cell the propose tool writes an accepted proposal into and the
/// caller reads back as the acceptance signal after the agent's turn.
pub(super) type AcceptedCell = Arc<StdMutex<Option<AcceptedProposal>>>;

/// The last diagnostic sentences a check or propose produced. The caller folds
/// these into a not-automatable reason when the agent ends without a proposal, so
/// the operator sees WHY rather than a bare "not automatable".
pub(super) type DiagCell = Arc<StdMutex<Vec<String>>>;

fn lock_vec(cell: &DiagCell) -> std::sync::MutexGuard<'_, Vec<String>> {
    cell.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reads the `workflow` argument into a [`WorkflowGraphSpec`], returning the
/// model-facing error sentence when it is missing or malformed — shared by the
/// check and propose tools so a bad payload reads the same on both.
fn spec_from_args(args: &Value) -> std::result::Result<WorkflowGraphSpec, String> {
    let Some(raw) = args.get("workflow") else {
        return Err(
            "the `workflow` argument is required — pass the graph as { name, description, nodes, \
             edges }."
                .to_string(),
        );
    };
    serde_json::from_value(raw.clone()).map_err(|err| {
        format!("the `workflow` argument is not a valid workflow graph: {err}. Pass it as {{ name, description, nodes, edges }}.")
    })
}

// ---------------------------------------------------------------------------
// list_effective_tools
// ---------------------------------------------------------------------------

/// Grounds the model in the company's real roster and wired tool slugs (issue
/// #840). Read-only, no arguments — everything it reports is gathered evidence
/// already in [`CopilotContext`], reusing PR-1's
/// [`workflow_effective_tool_slugs`](crate::company::workflow_effective_tool_slugs)
/// and the [`workflow_tool_info`](crate::workflows::caps::workflow_tool_info)
/// capability catalogue so what the agent sees and what a proposed node clears at
/// courtesy validation cannot drift.
pub(super) struct ListEffectiveToolsTool {
    ctx: Arc<CopilotContext>,
}

impl ListEffectiveToolsTool {
    pub(super) fn new(ctx: Arc<CopilotContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for ListEffectiveToolsTool {
    fn name(&self) -> &str {
        "list_effective_tools"
    }

    fn description(&self) -> &str {
        "List what this company can actually be automated with: the teammates an `agent` step may \
         name, and the tools a `tool_call` step may run (each with an honest one-line capability \
         and the arguments it needs), plus the tools the company was granted but has not wired \
         here — which you must NOT author. Call this before proposing a graph that uses an agent \
         or tool step, so you ground each one on a real id or slug rather than a guess. Takes no \
         arguments."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let ctx = &self.ctx;
        let mut out = String::new();

        out.push_str(
            "## Teammates — an `agent` step's `agent` must be one of these ids, copied exactly\n",
        );
        if ctx.company.roster.is_empty() {
            out.push_str(
                "- (no teammates — do not author an `agent` step; keep the graph to trigger, \
                 tool_call and output)\n",
            );
        }
        for entry in &ctx.company.roster {
            out.push_str(&roster_line(entry));
        }

        out.push_str(
            "\n## Tools — a `tool_call`'s `config.slug` must be one of these, exactly; put its \
             arguments under `config.args`\n",
        );
        if ctx.effective_slugs.is_empty() {
            out.push_str("- (no callable tools are wired — do not author a `tool_call` step)\n");
        }
        for slug in &ctx.effective_slugs {
            match crate::workflows::caps::workflow_tool_info(slug) {
                Some(info) => {
                    out.push_str(&format!("- `{}` — {}", info.slug, info.capability));
                    if !info.required_args.is_empty() {
                        out.push_str(&format!(" (args: {})", info.required_args.join(", ")));
                    }
                    out.push('\n');
                }
                None => out.push_str(&format!("- `{slug}`\n")),
            }
        }

        out.push_str(
            "\n## Granted but not wired on this deployment — do NOT author these; if the task \
             needs one, say so\n",
        );
        if ctx.unwired_slugs.is_empty() {
            out.push_str("- (none)\n");
        } else {
            out.push_str("- ");
            out.push_str(&ctx.unwired_slugs.join(", "));
            out.push('\n');
        }

        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// check_workflow
// ---------------------------------------------------------------------------

/// Statically checks a candidate graph (issue #840): the tinyflows authoring
/// gates ([`gates::failures`](tinyflows::gates::failures) — null bindings,
/// prose-as-expression prompts, and the rest) over the SAME translated graph the
/// engine would compile, unioned with the host's own
/// [`courtesy_validate_draft`] (shape / render / roster / tool). It runs NOTHING
/// — no node executes — so a clean result is a wiring check, not a live test (a
/// real dry-run is PR-3).
pub(super) struct CheckWorkflowTool {
    ctx: Arc<CopilotContext>,
    diag: DiagCell,
}

impl CheckWorkflowTool {
    pub(super) fn new(ctx: Arc<CopilotContext>, diag: DiagCell) -> Self {
        Self { ctx, diag }
    }
}

#[async_trait]
impl Tool for CheckWorkflowTool {
    fn name(&self) -> &str {
        "check_workflow"
    }

    fn description(&self) -> &str {
        "Statically check a candidate workflow graph before you propose it. It compiles the graph \
         the way the engine would and reports every problem it can prove — bindings that resolve \
         to nothing, a step that would run with an empty instruction, a shape the company would \
         refuse. An empty result means the graph passes; a reported problem is a REAL problem to \
         fix, never a run-time detail to wave away. It runs nothing. Pass the graph as the \
         `workflow` argument, shaped { name, description, nodes, edges }."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "workflow": {
                    "type": "object",
                    "description": "The candidate graph to check, as { name, description, nodes, edges }."
                }
            },
            "required": ["workflow"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let mut spec = match spec_from_args(&args) {
            Ok(spec) => spec,
            Err(msg) => {
                *lock_vec(&self.diag) = vec![msg.clone()];
                return Ok(ToolResult::success(msg));
            }
        };

        // The host owns the id — it is minted from the name at propose time and a
        // model-supplied id is always discarded. Mint the same host-safe id here so
        // `check_workflow` validates the graph the host WOULD build: a check that
        // honoured a model id could flag a duplicate/invalid id the propose pass
        // would have overridden, misleading the agent into reworking a fine graph.
        // Deliberately does NOT touch the name: an omitted name is honest feedback
        // the courtesy pass should surface.
        //
        // On the fix path (issue #840, PR-3) the host preserves the failing
        // workflow's saved id, so `check_workflow` validates the graph the host
        // WOULD build — a check that minted a fresh deduped id could flag a
        // duplicate the propose pass will not hit, misleading the agent.
        match self.ctx.fixing.as_ref() {
            // Mirror the propose path's fixing branch exactly: the host pins BOTH
            // id and name to the failing workflow's, so check must validate against
            // the same name the host will keep — otherwise it validates a
            // model-supplied name the propose pass overwrites, a false positive.
            Some(fix) => {
                spec.id = fix.id.clone();
                spec.name = fix.name.clone();
                spec.owner_desk = fix.owner_desk.clone();
            }
            None => {
                spec.id = safe_workflow_id(
                    &spec.name,
                    &self.ctx.description,
                    &self.ctx.company.existing_ids,
                );
            }
        }

        let mut problems: Vec<String> = Vec::new();

        // The host's own courtesy validation (shape / render / roster / tool),
        // the same gate the create route runs, WITHOUT persisting.
        match raw_workflow_from_spec(&spec) {
            Ok(raw) => {
                if let Err(err) = courtesy_validate_draft(
                    &raw,
                    &self.ctx.company.record,
                    self.ctx.company.source_dir.as_deref(),
                    Some(&self.ctx.company.wired_channels),
                    self.ctx.previous_owner_desk(),
                ) {
                    problems.push(err.to_string());
                }
            }
            Err(err) => problems.push(err.to_string()),
        }

        // The tinyflows authoring gates over the translated graph — the class of
        // "compiles but resolves to null at run time" a courtesy pass does not
        // catch. Only reachable when the graph translates (a kind the engine
        // refuses is already reported above), so a translate error is folded in
        // without duplicating the courtesy message.
        match workflow_graph_from_spec(&spec) {
            Ok(graph) => problems.extend(tinyflows::gates::failures(&graph)),
            Err(err) => {
                let sentence = err.to_string();
                if !problems.contains(&sentence) {
                    problems.push(sentence);
                }
            }
        }

        problems.dedup();
        *lock_vec(&self.diag) = problems.clone();

        if problems.is_empty() {
            Ok(ToolResult::success(
                "The workflow passes the static checks (this is a wiring check, not a live run).",
            ))
        } else {
            Ok(ToolResult::success(format!(
                "{} problem(s) — fix each before proposing:\n- {}",
                problems.len(),
                problems.join("\n- ")
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// propose_company_workflow
// ---------------------------------------------------------------------------

/// The acceptance signal (issue #840). Runs the SAME host authority the old
/// inline copilot path did — a safe, unique id; case-insensitive name dedup;
/// stripped model-chosen approval gating (#460); the `DESCRIPTION_NODE_KINDS`
/// refusal; [`ground_and_validate`] (name/role→id resolution, the delivery gate,
/// the wrong-but-real arm); and [`courtesy_validate_draft`] — so a graph reaches
/// the operator on exactly the terms it did before PR-2. On a pass it stashes the
/// accepted `(summary, spec, notes)` in the shared cell the caller reads; on a
/// failure it returns the specific sentences so the agent can fix and re-propose.
pub(super) struct ProposeWorkflowTool {
    ctx: Arc<CopilotContext>,
    accepted: AcceptedCell,
    diag: DiagCell,
}

impl ProposeWorkflowTool {
    pub(super) fn new(ctx: Arc<CopilotContext>, accepted: AcceptedCell, diag: DiagCell) -> Self {
        Self {
            ctx,
            accepted,
            diag,
        }
    }
}

#[async_trait]
impl Tool for ProposeWorkflowTool {
    fn name(&self) -> &str {
        // NOT `propose_workflow` (issue #1931 regression, vendor bump to
        // upstream `main`): the vendored `openhuman` runtime compiles in a
        // `workflows` toolpack (`vendor/openhuman/src/openhuman/tools/toolpacks/
        // registry.rs`) whose member list claims that bare name, owned only by
        // openhuman's OWN `workflow_builder`/`flow_discovery` agents. Every
        // pack defaults to `GroupMode::Withheld`, and `strip_packed_from_visible`
        // matches by NAME ALONE across every registered agent — it has no idea
        // this tool is ours, not theirs. An agent whose `agent_definition_name`
        // is not in that pack's `owners` (ours is `workflow:copilot`) silently
        // has any tool sharing a packed name stripped from the model's
        // advertised belt and replaced with `load_skill`/`use_skill`, which this
        // copilot doesn't register — so the model called a tool that no longer
        // existed on the wire. There is no ownership/group-mode escape hatch
        // reachable without editing the vendored tree: pack `owners` are a
        // `&'static` compile-time list, and per-pack `GroupMode` is read from an
        // ambient `CoreContext` OpenCompany never establishes (and flipping it
        // would be process-wide, not scoped to this one agent). Keeping this
        // name unmistakably ours is the durable fix.
        "propose_company_workflow"
    }

    fn description(&self) -> &str {
        "Propose your final workflow for the operator to review. Pass a one-line `summary` (name \
         what you inferred) and the graph as `workflow` ({ name, description, nodes, edges }). The \
         host re-checks it under its own authority — it assigns the id, dedups the name, sets \
         approval gating, grounds every teammate and tool, and refuses an unsupported node kind. \
         If it passes you are DONE: reply in one short line. If it returns problems, fix every one \
         and call propose_company_workflow again. Always run check_workflow first."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "One line on what the workflow does, naming any choice you inferred."
                },
                "workflow": {
                    "type": "object",
                    "description": "The final graph, as { name, description, nodes, edges }."
                }
            },
            "required": ["summary", "workflow"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        // Validates and stashes a draft in a shared cell. It persists nothing and
        // runs nothing — the operator's Create button is still the only write.
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut spec = match spec_from_args(&args) {
            Ok(spec) => spec,
            Err(msg) => {
                *lock_vec(&self.diag) = vec![msg.clone()];
                return Ok(ToolResult::success(format!("Not accepted: {msg}")));
            }
        };

        // Host authority, byte-for-byte the old inline path: the host owns the id,
        // the name dedup, and approval gating — the model gets no vote.
        match self.ctx.fixing.as_ref() {
            // The fix path (issue #840, PR-3) preserves the failing workflow's
            // saved identity: the id must equal the `wid` the edit route writes a
            // NEW VERSION under, and the name stays the one the operator knows. A
            // freshly deduped id/name here would orphan the correction.
            Some(fix) => {
                spec.id = fix.id.clone();
                spec.name = fix.name.clone();
                // The saved owner is host-owned across a correction, the same
                // footing as the id and the name (issue #1882 review): neither
                // tool advertises `ownerDesk`, so a model echo of it is noise
                // and an omission of it would silently unassign the workflow.
                spec.owner_desk = fix.owner_desk.clone();
            }
            None => {
                spec.id = safe_workflow_id(
                    &spec.name,
                    &self.ctx.description,
                    &self.ctx.company.existing_ids,
                );
                spec.name = safe_workflow_name(&spec.name, &self.ctx.company.existing_names);
            }
        }
        for node in &mut spec.nodes {
            node.requires_approval = None;
        }

        let mut errors: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        // The host owns the node-kind vocabulary: a kind the prompt never taught
        // blocks grounding (which assumes the known kinds), so it is a gate error.
        if let Some(node) = spec
            .nodes
            .iter()
            .find(|n| !DESCRIPTION_NODE_KINDS.contains(&n.kind.as_str()))
        {
            errors.push(format!(
                "node `{}` uses an unsupported kind `{}` — use only: {}.",
                node.id,
                node.kind,
                DESCRIPTION_NODE_KINDS.join(", ")
            ));
        } else {
            match ground_and_validate(&mut spec, &self.ctx.company, &self.ctx.description) {
                Ok(mut resolved_notes) => notes.append(&mut resolved_notes),
                Err(mut gate_errors) => errors.append(&mut gate_errors),
            }
            if errors.is_empty() {
                match raw_workflow_from_spec(&spec) {
                    Ok(raw) => {
                        if let Err(err) = courtesy_validate_draft(
                            &raw,
                            &self.ctx.company.record,
                            self.ctx.company.source_dir.as_deref(),
                            Some(&self.ctx.company.wired_channels),
                            self.ctx.previous_owner_desk(),
                        ) {
                            errors.push(err.to_string());
                        }
                    }
                    Err(err) => errors.push(err.to_string()),
                }
            }
        }

        if errors.is_empty() {
            *self.accepted.lock().unwrap_or_else(|e| e.into_inner()) = Some(AcceptedProposal {
                summary,
                spec,
                notes,
            });
            *lock_vec(&self.diag) = Vec::new();
            Ok(ToolResult::success(
                "Accepted — the draft passed every check and is ready for the operator to review. \
                 You are done: reply in one short line.",
            ))
        } else {
            *lock_vec(&self.diag) = errors.clone();
            Ok(ToolResult::success(format!(
                "Not accepted yet — fix each of these and call propose_company_workflow again:\n- {}",
                errors.join("\n- ")
            )))
        }
    }
}
