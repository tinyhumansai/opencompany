//! Workflow surfaces: create a graph (`POST /workflows`), read the company's
//! saved graphs (`GET /workflows`, `GET /workflows/{wid}`), run one
//! (`POST /workflows/{wid}/run`), read back what past runs did
//! (`GET /workflows/runs`), and preview what a trigger's cron means
//! (`POST /workflows/cron/preview`) — under both scope forms.
//!
//! ## Cron preview (issue #262)
//!
//! `POST /workflows/cron/preview` answers what a 5-field expression means and
//! when it next fires. A schedule's dangerous failure is the one that
//! *validates*: `0 9 * * *` and `9 0 * * *` are both valid and nine hours
//! apart, and the dialect is always UTC, so nothing in the authoring flow
//! contradicts an author who meant something else. Reading the parsed schedule
//! back is the only defence, since neither expression is invalid.
//!
//! It is the only route here that answers **200 on bad input**, carrying the
//! parser's message in the body — the console calls it while the author is
//! still typing, so a half-written expression is the normal state rather than
//! an error. See [`preview_cron`] for the full reasoning; `POST /workflows`
//! still refuses to save a graph whose schedule does not parse.
//!
//! ## Run history (issue #228)
//!
//! A run's outcome used to exist only in the moment: a manual run's
//! [`DeliveryReport`](crate::ports::DeliveryReport) rows lived in the console
//! drawer until it was dismissed, and a scheduled run's reached only host
//! stdout. The run route now journals every finished run through
//! [`record_run_finished`](crate::runtime::record_run_finished) — the same helper the cron
//! [`WorkflowScheduler`](crate::runtime::WorkflowScheduler) calls, so history is
//! uniform whatever started the run — and `GET /workflows/runs` folds those
//! events back out, newest first.
//!
//! A company's graphs come from two places, unioned by
//! [`load_workflow_union`](crate::company::load_workflow_union) /
//! [`list_workflows_union`](crate::company::list_workflows_union):
//!
//! * the **seed** files committed to the company source directory
//!   (`companies/<name>/workflows/<wid>.toml`), and
//! * the **runtime-authored** graph bodies persisted on the [`CompanyRecord`]
//!   overlay.
//!
//! The seed wins on an id collision. A hosted tenant has no source directory at
//! all (or a read-only one), so every graph it owns is an overlay body — which
//! is why these routes never require a source directory to answer.
//!
//! Creation (issues #69, #112, #168) delegates to
//! [`create_company_workflow`](crate::company::create_company_workflow), the
//! single validated-persist core the orchestrator's `create_workflow` tool also
//! runs, so the two surfaces cannot drift. It persists the graph body **and**
//! the enabled id on the operator's live record in one save; the
//! version-controlled `company.toml` and the source tree are never written
//! (see `crate::server::ops::team`) — the source tree is read-only in hosted
//! mode, which is what issue #168 reports.
//!
//! `list_workflows` additionally unions in the manifest's `[workflows].enabled`
//! ids that have no body in either source, falling back to the id as the display
//! name — the same fallback the GraphQL `Company.workflows` resolver uses — so a
//! provisioned tenant's picker isn't empty.
//!
//! Execution is dependency-inverted behind the [`WorkflowRunner`] port. When no
//! runner is wired the run route classifies *why* (see
//! [`runner_gap_for`](crate::server::ops::inference::runner_gap_for)) and answers
//! with one of three responses, because the operator's next step differs: the
//! default build or a runtime built without a harness reports `not_wired` (the
//! same 404 seam the DNS/SMTP surfaces use) so it stays inert; a company holding
//! a saved config a restart would pick up reports `restart_required` (409, issue
//! #266); and a company that never configured inference reports
//! `inference_required` (409, issue #514) so the console points the operator at
//! Settings instead of degrading to read-only. The read routes need no runner:
//! they only parse the saved graphs, so the console can list and render
//! workflows even on a build that cannot execute them.

use std::collections::HashSet;
use std::path::Path as FsPath;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::company::{
    RawEdge, RawNode, RawWorkflow, WorkflowDestinationDef, WorkflowEdgeDef, WorkflowFile,
    WorkflowNodeDef, WorkflowRetryDef, create_company_workflow, delete_company_workflow,
    list_workflows_with_globals, load_workflow_with_globals, rollback_company_workflow,
    seed_file_exists, set_company_workflow_enabled, update_company_workflow, workflow_version,
};
use crate::error::OpenCompanyError;
use crate::ports::types::{
    CompanyEvent, CompanyRecord, EventSeq, OverlayWorkflow, WorkflowNodeStatus,
};
use crate::runtime::cron::{CivilTime, CronExpr};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workflow route fragment: create + list, one graph read, and the
/// run write.
pub fn router() -> Router<AppState> {
    scoped("/workflows", post(create_workflow).get(list_workflows))
        // The static `/workflows/runs` GET is registered BEFORE the dynamic
        // `/workflows/{wid}`, mirroring `GET /tasks/inflight` in
        // [`super::tasks`]. Axum prefers a static segment over a parameter, so
        // the run-history read wins even though `runs` is a syntactically valid
        // `wid`; `run_history_is_not_shadowed_by_the_graph_read` pins it,
        // because a regression here would silently 404 the history panel.
        //
        // The cost is that a workflow whose id is literally `runs` cannot be
        // read through this route. That is the same trade `tasks/inflight`
        // takes, and the history surface is worth more than the one reserved id.
        .merge(scoped("/workflows/runs", get(list_runs)))
        // Same static-before-dynamic ordering as `/workflows/runs` above, for
        // the same reason: `cron` is a syntactically valid `wid`.
        .merge(scoped("/workflows/cron/preview", post(preview_cron)))
        // Issue #753: the create-time copilot. Drafts a graph from a free-text
        // description and hands it back for the New-workflow dialog to hydrate —
        // it never persists (Create still does). A static prefix registered here
        // with the others and BEFORE the dynamic `/workflows/{wid}`, for the
        // reason the comment above gives: `draft-from-description` is a
        // syntactically valid `wid`.
        .merge(scoped(
            "/workflows/draft-from-description",
            post(draft_from_description),
        ))
        // Issue #783: the `tool_call` slugs this company can reach from a
        // workflow, so the per-workflow copilot can ground a proposal on real
        // tools instead of guessing (`github_integration` and the like). Reads
        // the SAME `workflow_effective_tool_slugs` the create-time copilot
        // grounds on (issues #753, #874), so the two cannot drift — and, since
        // #874, so neither offers a tool this deployment has not wired. A static
        // prefix registered here with the others and BEFORE the dynamic
        // `/workflows/{wid}` below — `tool-slugs` is a syntactically valid `wid`.
        .merge(scoped("/workflows/tool-slugs", get(workflow_tool_slugs)))
        // Issue #813: the chat channels actually wired for this running company,
        // so the output-node destination editor can offer a picker of real
        // targets instead of a free-text box that only fails at delivery time
        // with `ChannelNotWired`. A static prefix registered here BEFORE the
        // dynamic `/workflows/{wid}` — `wired-channels` is a valid `wid`.
        .merge(scoped(
            "/workflows/wired-channels",
            get(workflow_wired_channels),
        ))
        // Issue #383: stop a run that is still walking its graph. Registered
        // here, with the other static `/workflows/...` prefixes and BEFORE the
        // dynamic `/workflows/{wid}` below, for the reason the comment above
        // gives — `runs` is a syntactically valid `wid`. This particular path is
        // four segments deep so it could not actually collide with the two- and
        // three-segment dynamic routes, but keeping the registration order
        // uniform is what stops the next four-segment static route from being
        // the one that silently loses.
        .merge(scoped(
            "/workflows/runs/{rid}/cancel",
            post(cancel_workflow_run),
        ))
        // Issue #596: read one past run's per-node output for the run inspector.
        // Same static-before-dynamic family as the cancel route above and, like
        // it, four segments deep so it cannot collide with the two/three-segment
        // dynamic routes — but registered here to keep the ordering uniform.
        // Deliberately a lazy per-run fetch, NOT folded into `list_runs`: that
        // fold is already expensive, and an inspector only ever opens one run at
        // a time (the make.com pattern).
        .merge(scoped("/workflows/runs/{rid}/output", get(get_run_output)))
        // Issue #259: read, replace, remove — all on the same id. `PUT` is a
        // full replace rather than a `PATCH` merge because a workflow *is* its
        // graph: a partial node/edge merge has no well-defined meaning (which
        // half of a rewired edge set wins?), and the console always holds the
        // whole graph anyway.
        //
        // Registered LAST of the `/workflows/...` reads, after both static
        // segments above: this is the dynamic route they have to outrank.
        .merge(scoped(
            "/workflows/{wid}",
            get(get_workflow)
                .put(update_workflow)
                .delete(delete_workflow),
        ))
        .merge(scoped("/workflows/{wid}/run", post(run_workflow)))
        // Issue #840 (PR-3): correct a saved workflow whose run failed, with the
        // create-time copilot. Drafts a corrected graph from the failing graph +
        // the run's journaled failure and hands it back for the edit dialog to
        // hydrate — it never persists (Save still does). A sub-resource of
        // `{wid}`, strictly more specific than the dynamic `{wid}` reads above, so
        // registration order cannot make it lose.
        .merge(scoped("/workflows/{wid}/fix-from-run", post(fix_from_run)))
        // Issue #276: the pause switch. A sub-resource `PUT` rather than a field
        // on the graph `PUT` above, because the two are different decisions with
        // different permissions to grow into and different bodies: replacing a
        // graph requires holding the whole graph and a version token, and an
        // operator who only wants to stop a schedule should not have to send
        // one — nor risk a 409 from a stale editor tab while doing it.
        .merge(scoped(
            "/workflows/{wid}/enabled",
            put(set_workflow_enabled),
        ))
        // Issue #274: the edit history of one workflow, and the restore of one of
        // its snapshots. Both hang off `{wid}` after a further static segment
        // (`revisions`), so they cannot collide with the dynamic `{wid}` reads
        // above — they are strictly more specific — and they carry zero overlap
        // with the `…/runs*` family, which lives on a different static prefix.
        .merge(scoped(
            "/workflows/{wid}/revisions",
            get(list_workflow_revisions),
        ))
        .merge(scoped(
            "/workflows/{wid}/revisions/{rev}/restore",
            post(restore_workflow_revision),
        ))
}

/// A one-line workflow entry as the console's picker renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSummary {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Whether `PUT`/`DELETE` on this id will be accepted (issue #259) — see
    /// [`is_editable`]. The console disables its Edit/Delete affordances on a
    /// `false`, so an operator is told *before* clicking rather than by a 409
    /// after.
    editable: bool,
    /// Whether this workflow's schedule is armed (issue #276). `false` means the
    /// graph is saved and still runnable by hand, but
    /// [`WorkflowScheduler::tick`](crate::runtime::WorkflowScheduler) skips it.
    ///
    /// Always serialized, including `true`, so the console can render the toggle
    /// from the list read alone. Unlike `editable` this is a property of the
    /// *company record*, not of where the graph lives — a seed-defined workflow
    /// is `editable: false` and still toggleable.
    enabled: bool,
}

impl WorkflowSummary {
    fn new(f: WorkflowFile, editable: bool, enabled: bool) -> Self {
        Self {
            id: f.id,
            name: f.name,
            description: f.description,
            editable,
            enabled,
        }
    }
}

/// The full graph the canvas renders — nodes and directed edges, camelCase.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowGraph {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    nodes: Vec<WorkflowNode>,
    edges: Vec<WorkflowEdge>,
    /// Whether this graph can be replaced or removed through the API — see
    /// [`is_editable`].
    editable: bool,
    /// The opaque optimistic-concurrency token for this graph (issue #259).
    /// Always serialized: a string when the graph is `editable`, and explicit
    /// `null` when it is not (a source-defined or body-less graph has nothing to
    /// version). It is deliberately NOT omitted — a client that read `version`
    /// off a graph whose key was absent got `undefined` and sent nothing,
    /// silently overwriting a concurrent save (issue #1013). An explicit `null`
    /// says "no token here" instead of hiding the field.
    ///
    /// The contract is **echo it back**: hand it to `PUT` in the body or to
    /// `DELETE` as `?expectedVersion=`, and the write is refused with a `409` if
    /// the graph moved in between — and refused with a `400` if you omit it
    /// entirely (issue #1013). Never parse or derive it.
    version: Option<String>,
    /// Whether this workflow's schedule is armed (issue #276) — see
    /// [`WorkflowSummary::enabled`]. Carried on the graph read as well as the
    /// list so the editor can show the state it is about to change, and so a
    /// `PUT` response reports the disarm the edit may have just triggered
    /// without a follow-up read.
    enabled: bool,
}

impl WorkflowGraph {
    fn new(f: WorkflowFile, editable: bool, version: Option<String>, enabled: bool) -> Self {
        Self {
            id: f.id,
            name: f.name,
            description: f.description,
            nodes: f.nodes.into_iter().map(WorkflowNode::from).collect(),
            edges: f.edges.into_iter().map(WorkflowEdge::from).collect(),
            editable,
            version,
            enabled,
        }
    }
}

/// A single graph node. `kind` is the on-disk string
/// (`trigger`/`agent`/`tool_call`/`http_request`/`condition`/`output`); `agent`
/// is only meaningful on `agent` nodes; `schedule` (a 5-field UTC cron) only on
/// `trigger` nodes. The P1 fields (`config` / `onError` / `retry` /
/// `requiresApproval`) are serialized so `GET …/workflows/{wid}` does not drop
/// model data (they are omitted entirely when unset).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowNode {
    id: String,
    kind: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<WorkflowRetryOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requires_approval: Option<bool>,
    /// Where an `output` node's report is routed once the run finishes (issue
    /// #170). The model shape is reused verbatim in both directions: its two
    /// fields (`kind` / `target`) are single words, so there is no snake_case →
    /// camelCase gap to bridge and no second shape to drift from.
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<WorkflowDestinationDef>,
}

/// The camelCase retry policy shape the console reads back (`maxAttempts` /
/// `backoffMs` / `backoff`). Distinct from the snake_case
/// [`WorkflowRetryDef`] the model/TOML use.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRetryOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backoff_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backoff: Option<String>,
}

impl From<WorkflowRetryDef> for WorkflowRetryOut {
    fn from(r: WorkflowRetryDef) -> Self {
        Self {
            max_attempts: r.max_attempts,
            backoff_ms: r.backoff_ms,
            backoff: r.backoff,
        }
    }
}

impl From<WorkflowNodeDef> for WorkflowNode {
    fn from(n: WorkflowNodeDef) -> Self {
        Self {
            id: n.id,
            kind: n.kind.as_str().to_string(),
            name: n.name,
            summary: n.summary,
            agent: n.agent,
            schedule: n.schedule,
            config: n.config,
            on_error: n.on_error,
            retry: n.retry.map(WorkflowRetryOut::from),
            requires_approval: n.requires_approval,
            destination: n.destination,
        }
    }
}

/// A directed edge between two node ids, with an optional branch label.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowEdge {
    from: String,
    to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

impl From<WorkflowEdgeDef> for WorkflowEdge {
    fn from(e: WorkflowEdgeDef) -> Self {
        Self {
            from: e.from,
            to: e.to,
            label: e.label,
        }
    }
}

/// The company's runtime-authored graph bodies, read once per request from the
/// live record. A company with no persisted record contributes none — the read
/// routes stay tolerant (they still serve the seed files), and the create route
/// re-loads the record under its write lock and 404s properly there.
/// Whether `wid` can be replaced or removed through `PUT`/`DELETE` (issue #259):
/// it is backed by a **record overlay body** and is **not shadowed by a seed
/// file**.
///
/// Both halves are the reader's rules, restated. `load_workflow_union` gives a
/// seed file precedence on an id collision, so an edit to a seed-shadowed id
/// would persist a graph nothing serves; and `merge_enabled_workflows` (#208)
/// re-derives `[workflows].enabled` from seed ids at boot, so a "delete" of a
/// seed-backed workflow would undo itself on restart. The write core enforces
/// exactly this and answers `409` — this flag is the same predicate, projected
/// so the console can grey the button instead of surfacing that 409.
///
/// The seed probe is [`seed_file_exists`], shared with the create path's
/// id-uniqueness check, so the flag and the host's actual answer cannot drift.
fn is_editable(source_dir: Option<&FsPath>, overlays: &[OverlayWorkflow], wid: &str) -> bool {
    overlays.iter().any(|w| w.id == wid) && !seed_file_exists(source_dir, wid)
}

/// The stored overlay TOML for `wid`, when the record has one.
fn overlay_toml<'a>(overlays: &'a [OverlayWorkflow], wid: &str) -> Option<&'a str> {
    overlays
        .iter()
        .find(|w| w.id == wid)
        .map(|w| w.toml.as_str())
}

/// This company's overlay graph bodies and its `[globals].disable`, read
/// together — the pair every union read needs.
async fn overlay_workflows_and_globals(
    company: &ScopedCompany,
) -> Result<(Vec<OverlayWorkflow>, Vec<String>), ApiError> {
    let (overlays, _, globals_disable) = workflow_state(company).await?;
    Ok((overlays, globals_disable))
}

/// The company's runtime-authored graph bodies **and** the ids the operator has
/// switched off (issue #276), from one record read.
///
/// One read rather than two because the two answers have to agree: a route that
/// loaded the bodies and the switches separately could list a workflow from one
/// record and its armed state from another, and the window is exactly the moment
/// an operator has just toggled it.
async fn workflow_state(
    company: &ScopedCompany,
) -> Result<(Vec<OverlayWorkflow>, Vec<String>, Vec<String>), ApiError> {
    let record: Option<CompanyRecord> = company
        .runtime
        .store()
        .load(company.id())
        .await
        .map_err(ApiError)?;
    // The company's `[globals].disable` rides along for the same reason the
    // other two do: a route that resolved a global graph without it would serve
    // one this company opted out of.
    Ok(record
        .map(|r| {
            (
                r.overlay_workflows,
                r.disabled_workflows,
                r.manifest.globals.disable,
            )
        })
        .unwrap_or_default())
}

/// `GET …/workflows` — the company's saved workflows as picker summaries.
///
/// Summaries come from the union of the company's two graph sources — the seed
/// `workflows/*.toml` files and the record's runtime-authored bodies — so a
/// hosted tenant with no source directory still lists everything it created.
/// The manifest's `[workflows].enabled` ids are then unioned in (deduped by id),
/// falling back to the id as the name for an id with no body in either source —
/// the same fallback the GraphQL resolver uses. Only when all three are empty
/// does this return `200 []`, so the console renders "no workflows yet" rather
/// than a failure.
async fn list_workflows(company: ScopedCompany) -> Result<Json<Vec<WorkflowSummary>>, ApiError> {
    let (overlays, disabled, globals_disable) = workflow_state(&company).await?;
    let source_dir = company.runtime.source_dir();
    let files = list_workflows_with_globals(source_dir, &overlays, &globals_disable);
    let mut seen: HashSet<String> = files.iter().map(|f| f.id.clone()).collect();
    let mut summaries: Vec<WorkflowSummary> = files
        .into_iter()
        .map(|f| {
            let editable = is_editable(source_dir, &overlays, &f.id);
            let enabled = !disabled.iter().any(|id| id == &f.id);
            WorkflowSummary::new(f, editable, enabled)
        })
        .collect();

    let enabled_ids = company
        .runtime
        .enabled_workflow_ids()
        .await
        .map_err(ApiError)?;
    for id in enabled_ids {
        // Already summarized from a real body — skip so an id that is both
        // enabled and saved doesn't double-list.
        if !seen.insert(id.clone()) {
            continue;
        }
        summaries.push(WorkflowSummary {
            id: id.clone(),
            name: id,
            description: None,
            // A manifest-`enabled` id with no body in either source: there is
            // nothing to replace or remove, and the write core says so with a
            // 409. Never editable.
            editable: false,
            // Nor toggleable, for the same reason — there is no graph, so no
            // schedule to switch off. Reported as `true` because that is what it
            // is: nothing is holding it back, there is simply nothing there.
            enabled: true,
        });
    }

    Ok(Json(summaries))
}

/// `GET …/workflows/{wid}` — the full graph for one workflow.
///
/// Reads through the seed ∪ overlay union, so a graph created on a hosted
/// tenant (no source directory) resolves here too. An unknown `wid` is a `404`,
/// mirroring the sub-resource-not-found shape the task routes use.
async fn get_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    // `wid` may become a filename on the seed side — reject anything that could
    // escape `workflows/`.
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    let source_dir = company.runtime.source_dir();
    let (overlays, disabled, globals_disable) = workflow_state(&company).await?;
    let file = load_workflow_with_globals(source_dir, &overlays, &globals_disable, &wid)
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))))?;
    // Issue #259: the version token rides out with the graph, so the console
    // gets it for free on the same read it renders from — there is no second
    // round trip for a caller to skip and thereby lose the concurrency guard.
    let editable = is_editable(source_dir, &overlays, &wid);
    let version = editable
        .then(|| overlay_toml(&overlays, &wid).map(workflow_version))
        .flatten();
    let enabled = !disabled.iter().any(|id| id == &wid);
    Ok(Json(WorkflowGraph::new(file, editable, version, enabled)))
}

/// The create-workflow body — the same camelCase graph shape the GET routes
/// return (`id`/`name`/`description?`/`nodes`/`edges`), so the console's
/// creator can post exactly what it would otherwise read back.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkflowBody {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    nodes: Vec<CreateNode>,
    #[serde(default)]
    edges: Vec<CreateEdge>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateNode {
    id: String,
    kind: String,
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    /// A 5-field UTC cron saying when the workflow starts on its own — only
    /// valid on a `trigger` node (issue #169). Validated by the render → parse
    /// round trip below, so a bad expression or a schedule on the wrong node
    /// kind is a `400`, not a persisted graph that never fires.
    #[serde(default)]
    schedule: Option<String>,
    /// Free-form, kind-specific node config (P2): `switch`/`transform`
    /// `=expr` bindings, a `sub_workflow` `workflow_id`, a `tool_call` slug/args,
    /// … . Carried as JSON on the wire and converted to a `toml::Value` on the
    /// way to disk; a JSON `null` anywhere inside is rejected as a 4xx (TOML has
    /// no null to represent it).
    #[serde(default)]
    config: Option<serde_json::Value>,
    /// Per-node error policy: `stop` (default) / `continue` / `route`.
    #[serde(default)]
    on_error: Option<String>,
    /// Per-node retry policy the engine honors.
    #[serde(default)]
    retry: Option<CreateRetry>,
    /// When `true`, the node pauses awaiting operator approval before it runs.
    #[serde(default)]
    requires_approval: Option<bool>,
    /// Where an `output` node's report goes once the run finishes:
    /// `{"kind": "owner"|"email"|"channel", "target"?: "…"}`. Rejected on any
    /// other node kind, and each kind's target contract is enforced by
    /// `parse_workflow` before anything is persisted.
    #[serde(default)]
    destination: Option<WorkflowDestinationDef>,
}

/// The camelCase retry policy the create body carries (`maxAttempts` /
/// `backoffMs` / `backoff`), mapped to the snake_case [`WorkflowRetryDef`] the
/// model + TOML use — the inverse of the [`WorkflowRetryOut`] read shape.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRetry {
    #[serde(default)]
    max_attempts: Option<u32>,
    #[serde(default)]
    backoff_ms: Option<u64>,
    #[serde(default)]
    backoff: Option<String>,
}

impl From<CreateRetry> for WorkflowRetryDef {
    fn from(r: CreateRetry) -> Self {
        Self {
            max_attempts: r.max_attempts,
            backoff_ms: r.backoff_ms,
            backoff: r.backoff,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateEdge {
    from: String,
    to: String,
    #[serde(default)]
    label: Option<String>,
}

impl TryFrom<CreateWorkflowBody> for RawWorkflow {
    type Error = ApiError;

    fn try_from(body: CreateWorkflowBody) -> Result<Self, ApiError> {
        let mut nodes = Vec::with_capacity(body.nodes.len());
        for n in body.nodes {
            // JSON config → TOML value. TOML has no `null`, so a `null` anywhere
            // in the config is a caller error (400), not a 500 on write.
            let config = match n.config {
                Some(json) => Some(toml::Value::try_from(json).map_err(|err| {
                    ApiError(OpenCompanyError::InvalidRequest(format!(
                        "node `{}` has config that can't be stored ({err}) — TOML has no null; drop null-valued keys.",
                        n.id
                    )))
                })?),
                None => None,
            };
            nodes.push(RawNode {
                id: n.id,
                kind: n.kind,
                name: n.name,
                summary: n.summary,
                agent: n.agent,
                schedule: n.schedule,
                config,
                on_error: n.on_error,
                retry: n.retry.map(WorkflowRetryDef::from),
                requires_approval: n.requires_approval,
                destination: n.destination,
            });
        }
        Ok(Self {
            id: body.id,
            name: body.name,
            description: body.description,
            nodes,
            edges: body
                .edges
                .into_iter()
                .map(|e| RawEdge {
                    from: e.from,
                    to: e.to,
                    label: e.label,
                })
                .collect(),
        })
    }
}

/// Rejects a draft whose `output` node routes its report to a channel this
/// runtime cannot deliver to (issue #981).
///
/// The delivery layer already refuses such a target at run time with a
/// `ChannelNotWired` row, and that row stays — desks come and go, so a graph
/// valid at save can be invalid at run, and this is a guard rather than a
/// guarantee. What it removes is the case the QA pass found: a workflow saved
/// against `operator`, which delivery refuses **by name** on every runtime and
/// so can never succeed, discovered only after a scheduled run nobody watched
/// silently dropped its report.
///
/// Both write routes run it, from the same set the destination picker is served
/// from and with the same sentence the failed delivery would have carried, so
/// an author who trips it is told what a run would have told them.
///
/// Deliberately narrow. A missing target, a `destination` on a non-`output`
/// node and an unknown `kind` are [`parse_workflow`](crate::company::parse_workflow)'s
/// to report — it says something more specific about each, and reporting the
/// wrong problem first is worse than reporting it second. This is also NOT run
/// at TOML parse time: seed templates are parsed with no runtime in hand, and
/// checking there would refuse to boot a template on a company whose desks are
/// not resolved yet.
fn reject_undeliverable_channel_destinations(
    company: &ScopedCompany,
    draft: &RawWorkflow,
) -> Result<(), ApiError> {
    let deliverable = company.runtime.deliverable_channel_ids();
    for node in &draft.nodes {
        let Some(destination) = &node.destination else {
            continue;
        };
        if destination.kind.trim() != "channel" || node.kind.trim() != "output" {
            continue;
        }
        let target = destination.target.as_deref().map(str::trim).unwrap_or("");
        if target.is_empty() || deliverable.iter().any(|id| id == target) {
            continue;
        }
        let live: Vec<&str> = deliverable.iter().map(String::as_str).collect();
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "node `{}`: {}",
            node.id,
            crate::runtime::undeliverable_channel_message(target, &live)
        ))));
    }
    Ok(())
}

/// `POST …/workflows` — authors a new workflow graph (issues #69, #112, #168):
/// the console's form creator, or any direct API caller, posts the graph shape
/// and it is persisted on the company record.
///
/// The whole validated-persist sequence lives in
/// [`create_company_workflow`] — the same core the orchestrator's
/// `create_workflow` tool runs — so the two surfaces cannot drift: safe id,
/// size caps, exactly one trigger, every `agent` node on the roster, unique id
/// (409) and unique display name (409), full [`parse_workflow`] structural
/// validation, one atomic record save, and an audit event.
///
/// No source directory is required: the body lands on the record, so a hosted
/// tenant whose source tree is a read-only mount can create workflows (issue
/// #168 — this used to fail with `EROFS`).
async fn create_workflow(
    company: ScopedCompany,
    Json(body): Json<CreateWorkflowBody>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    let draft = RawWorkflow::try_from(body)?;
    reject_undeliverable_channel_destinations(&company, &draft)?;
    let file = create_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        Some(company.runtime.events()),
        draft,
    )
    .await
    .map_err(ApiError)?;
    // A freshly created graph is always an overlay body and, since create
    // refuses a seed-colliding id, never seed-shadowed — so it is editable, and
    // its version token goes back on the create response. That lets the console
    // hold a valid token from the moment it creates a workflow, without a
    // follow-up GET.
    Ok(Json(graph_with_version(&company, file).await?))
}

/// Re-reads the just-written overlay body to attach the current `editable` flag
/// and version token to a write response.
///
/// The re-read is deliberate rather than derived from the value we just
/// rendered: the token must be the hash of what is *stored*, so that echoing it
/// back is guaranteed to match. Computing it from an in-memory copy would work
/// right up until the persist path ever normalizes the TOML, and then fail as a
/// mysterious permanent 409.
async fn graph_with_version(
    company: &ScopedCompany,
    file: WorkflowFile,
) -> Result<WorkflowGraph, ApiError> {
    let source_dir = company.runtime.source_dir();
    let (overlays, disabled, _) = workflow_state(company).await?;
    let editable = is_editable(source_dir, &overlays, &file.id);
    let version = editable
        .then(|| overlay_toml(&overlays, &file.id).map(workflow_version))
        .flatten();
    // Read back rather than assumed, so a create or an edit that the disarm rule
    // just switched off reports `enabled: false` on its own response — the
    // console learns about the disarm from the write it made, not from a later
    // refresh it might not do.
    let enabled = !disabled.iter().any(|id| id == &file.id);
    Ok(WorkflowGraph::new(file, editable, version, enabled))
}

/// The `PUT …/workflows/{wid}` body: the same camelCase graph shape the read and
/// create routes speak, plus the optional concurrency token.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateWorkflowBody {
    #[serde(flatten)]
    graph: CreateWorkflowBody,
    /// The token from the `GET`/`PUT` this edit was based on. **Required** (issue
    /// #1013): a missing token is a `400`, not an unconditional write, so a stale
    /// editor can't silently clobber a concurrent save. Kept `Option` +
    /// `serde(default)` so an omitted field is a clean handler-level `400` with a
    /// recovery message, rather than an opaque serde `422`.
    #[serde(default)]
    expected_version: Option<String>,
}

/// `PUT …/workflows/{wid}` — replaces a saved workflow graph wholesale (issue
/// #259).
///
/// Before this, a workflow was write-once: a typo in a cron expression or a
/// node pointed at the wrong teammate was permanent, and the only recovery was
/// to author a second workflow and leave the broken one firing forever.
///
/// The body's `id` **must equal** `wid`. Renaming an id through `PUT` is
/// deliberately rejected rather than quietly supported: the id keys the union
/// read path, the scheduler's per-workflow fire bookkeeping, and every
/// journalled run in the history — a rename would silently orphan all three. A
/// rename is a create plus a delete, and the operator should say so.
///
/// `expectedVersion` is **required** (issue #1013): omitting it used to mean an
/// unconditional write, so a console holding a stale graph — or one that read
/// `version` as `undefined` and sent nothing — silently clobbered a concurrent
/// save. A missing token is now a `400`, matching the agent `update_workflow`
/// tool, which has always demanded it. A caller re-reads the workflow and echoes
/// back its `version`; the conditional write then refuses with a `409` if the
/// graph moved in between.
///
/// Statuses: `400` (bad graph, `id` ≠ `wid`, or a missing `expectedVersion`),
/// `404` (unknown id), `409` (source-defined, body-less, name taken, or a stale
/// `expectedVersion`).
async fn update_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    Json(body): Json<UpdateWorkflowBody>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    if body.graph.id != wid {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "this request would change the workflow's id from `{wid}` to `{}`. A workflow's id \
             can't change — it keys the saved graph, its schedule and its run history. Create a \
             new workflow under the new id and delete this one instead.",
            body.graph.id
        ))));
    }

    // `expectedVersion` is required (issue #1013). An absent token used to mean
    // an unconditional write; that let a stale editor overwrite a concurrent save
    // without ever seeing a 409. Refuse the write with a 400 instead, mirroring
    // the agent `update_workflow` tool, and tell the caller how to recover.
    let Some(expected) = body.expected_version.clone() else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "`expectedVersion` is required: re-read this workflow and send back the `version` it \
             returns. A `PUT` replaces the whole graph, so saving without the version you read \
             from could silently overwrite a change made since."
                .to_string(),
        )));
    };
    let draft = RawWorkflow::try_from(body.graph)?;
    reject_undeliverable_channel_destinations(&company, &draft)?;
    let file = update_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        company.runtime.workflow_revisions(),
        Some(company.runtime.events()),
        draft,
        Some(expected.as_str()),
    )
    .await
    .map_err(ApiError)?;
    // The response carries the NEW token, so a console can save twice in a row
    // without a re-read in between.
    Ok(Json(graph_with_version(&company, file).await?))
}

/// The optional `?expectedVersion=` query on `DELETE …/workflows/{wid}`.
///
/// A query param rather than a body because a `DELETE` with a body is poorly
/// supported by intermediaries and by `fetch`; the token is 64 hex characters,
/// so it is URL-safe without escaping.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorkflowQuery {
    /// The token of the graph being removed. **Required** (issue #1013): an
    /// absent `?expectedVersion=` is a `400`, not an unconditional delete, so a
    /// stale editor can't drop a workflow that changed since they last looked.
    #[serde(default)]
    expected_version: Option<String>,
}

/// `DELETE …/workflows/{wid}` — removes a saved workflow (issue #259).
///
/// Drops the graph body **and** the id from `[workflows].enabled` in one save,
/// so the workflow stops appearing in the picker and stops firing on its
/// schedule, and stays gone across a restart.
///
/// **Past runs are deliberately kept.** They are journal entries recording what
/// the workflow did, and that stays true after it is gone — `GET
/// …/workflows/runs` keeps serving them. See the module doc.
///
/// `expectedVersion` is **required** (issue #1013), for the same reason it is on
/// `PUT`: an absent token used to mean an unconditional delete, so a console
/// holding a stale graph could remove a workflow that changed underneath it. A
/// missing `?expectedVersion=` is now a `400`.
///
/// `204` on success. `400` for a missing `expectedVersion`; `404` for an unknown
/// id; `409` for a source-defined or body-less id, or a stale `expectedVersion`.
async fn delete_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    Query(query): Query<DeleteWorkflowQuery>,
) -> Result<StatusCode, ApiError> {
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    // `expectedVersion` is required (issue #1013) — a tokenless delete is refused
    // rather than run unconditionally, so a stale editor can't drop a workflow
    // that moved since they loaded it.
    let Some(expected) = query.expected_version.as_deref() else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "`expectedVersion` is required: read this workflow and pass its `version` as \
             `?expectedVersion=`. Deleting without the version you read from could remove a \
             workflow that changed since you last looked."
                .to_string(),
        )));
    };
    delete_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        company.runtime.workflow_revisions(),
        Some(company.runtime.schedule_fires()),
        Some(company.runtime.events()),
        &wid,
        Some(expected),
    )
    .await
    .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The `PUT …/workflows/{wid}/enabled` body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetEnabledBody {
    /// The state to move to: `true` arms the schedule, `false` pauses it.
    enabled: bool,
}

/// `PUT …/workflows/{wid}/enabled` — arms or pauses a workflow's schedule
/// (issue #276).
///
/// Before this, the only way to stop a schedule firing was to delete the
/// workflow, which threw the graph away to silence it for an afternoon.
///
/// **Pausing stops the schedule, not the workflow.** A paused workflow keeps its
/// graph, stays in the picker, and still runs from the console's Run button —
/// [`WorkflowScheduler::tick`](crate::runtime::WorkflowScheduler) is the only
/// reader of the flag. That split is the point: "don't fire this on its own" and
/// "I can't run this" are different asks, and an operator debugging a workflow
/// needs the first without the second.
///
/// Idempotent: setting the state a workflow already holds is a `200` that writes
/// nothing and journals nothing, so a double-click costs one no-op rather than a
/// second audit entry.
///
/// Statuses: `200` (armed state is now what was asked, changed or not), `404`
/// (unknown id), `409` (a manifest-`enabled` id with no saved graph — no
/// schedule to switch off). No `expectedVersion`: see
/// [`set_company_workflow_enabled`].
async fn set_workflow_enabled(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    Json(body): Json<SetEnabledBody>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    set_company_workflow_enabled(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        Some(company.runtime.events()),
        &wid,
        body.enabled,
    )
    .await
    .map_err(ApiError)?;

    // Answer with the graph, re-read, rather than a bare 204: the console
    // renders the row from this shape, and reading it back means the `enabled`
    // it shows is what the store holds rather than what the request asked for.
    let (overlays, disabled, globals_disable) = workflow_state(&company).await?;
    let source_dir = company.runtime.source_dir();
    let file = load_workflow_with_globals(source_dir, &overlays, &globals_disable, &wid)
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))))?;
    let editable = is_editable(source_dir, &overlays, &wid);
    let version = editable
        .then(|| overlay_toml(&overlays, &wid).map(workflow_version))
        .flatten();
    let enabled = !disabled.iter().any(|id| id == &wid);
    Ok(Json(WorkflowGraph::new(file, editable, version, enabled)))
}

// ---------------------------------------------------------------------------
// Revision history + rollback (issue #274)
// ---------------------------------------------------------------------------

/// One revision as the console's history panel renders it — **metadata only**.
///
/// The graph body is deliberately absent: the list is a chooser, and shipping a
/// full graph per row would make the history read as heavy as N graph reads for
/// no benefit. The restore route fetches (and returns) the body when an operator
/// actually picks one. `version` is the same opaque token
/// `GET …/workflows/{wid}` hands out for the current body, so a console can tell
/// which revision matches what it is looking at.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevisionSummary {
    id: String,
    name: String,
    version: String,
    created_at_millis: u64,
}

/// The `GET …/workflows/{wid}/revisions` response: the snapshots, newest first.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRevisionsResponse {
    revisions: Vec<RevisionSummary>,
}

/// `GET …/workflows/{wid}/revisions` — one workflow's edit history (issue #274),
/// newest first, **metadata only** (no graph bodies — see [`RevisionSummary`]).
///
/// A workflow with no history (never edited, or seed-backed) answers `200` with
/// an empty list rather than a `404`: "no revisions" is a normal state the
/// console renders as an empty panel, not an error. A malformed `wid` is a `404`
/// like every other read here.
async fn list_workflow_revisions(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
) -> Result<Json<WorkflowRevisionsResponse>, ApiError> {
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    let rows = company
        .runtime
        .workflow_revisions()
        .list_revisions(company.id(), &wid)
        .await
        .map_err(ApiError)?;
    let revisions = rows
        .into_iter()
        .map(|r| RevisionSummary {
            id: r.id,
            name: r.name,
            // The token the current-graph read would hand out for this body, so
            // the console can correlate a row with what it currently holds.
            version: workflow_version(&r.toml),
            created_at_millis: r.created_at_millis,
        })
        .collect();
    Ok(Json(WorkflowRevisionsResponse { revisions }))
}

/// The sub-resource path on the restore route: the workflow id and the revision
/// id. The scope `id` is consumed by the extractor.
#[derive(Debug, Deserialize)]
struct RevisionPath {
    wid: String,
    rev: String,
}

/// The `POST …/workflows/{wid}/revisions/{rev}/restore` body: the optional
/// concurrency token of the graph being replaced.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreRevisionBody {
    /// The token from the `GET`/`PUT` the operator was looking at when they hit
    /// Restore. **Required** (issue #1013): an absent token — or an absent body —
    /// is a `400`, not an unconditional restore, so a stale editor can't overwrite
    /// a concurrent save. On a `409` reload rather than retry — the graph moved
    /// under it.
    #[serde(default)]
    expected_version: Option<String>,
}

/// `POST …/workflows/{wid}/revisions/{rev}/restore` — roll a workflow back to a
/// captured revision (issue #274), returning the restored [`WorkflowGraph`] with
/// a fresh version token.
///
/// This is an ordinary edit whose new body is an old one, so it routes through
/// the same [`rollback_company_workflow`] → [`update_company_workflow`] path a
/// `PUT` does and inherits every one of its guarantees: re-validation against
/// the *current* record, a snapshot of the body it replaces (so the restore is
/// itself undoable), the optimistic-concurrency token, and the #276 disarm of a
/// restored schedule.
///
/// `expectedVersion` is **required** (issue #1013), aligning restore with `PUT`:
/// an absent token — or an omitted body — used to mean an unconditional restore,
/// so a stale editor could overwrite a concurrent save. A missing token is now a
/// `400`.
///
/// Statuses: `200` (restored), `400` (a missing `expectedVersion`, or the
/// revision is invalid against the current record — e.g. it names a since-removed
/// teammate), `404` (unknown `wid` or unknown `rev`), `409` (seed-backed /
/// body-less `wid`, a stale `expectedVersion`, or a name collision).
async fn restore_workflow_revision(
    company: ScopedCompany,
    Path(RevisionPath { wid, rev }): Path<RevisionPath>,
    body: Option<Json<RestoreRevisionBody>>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    // `expectedVersion` is required (issue #1013). Resolve it from the optional
    // body; an absent token or an absent body alike is a 400, not an
    // unconditional restore, so a stale editor can't clobber a concurrent save.
    let Some(expected) = body.and_then(|Json(b)| b.expected_version) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "`expectedVersion` is required: read this workflow and send back its `version`. A \
             restore replaces the current graph, so doing it without the version you read from \
             could silently overwrite a change made since."
                .to_string(),
        )));
    };
    let file = rollback_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        company.runtime.workflow_revisions(),
        Some(company.runtime.events()),
        &wid,
        &rev,
        Some(expected.as_str()),
    )
    .await
    .map_err(ApiError)?;
    // The response carries the restored graph plus its NEW token — a restore is a
    // write, so the console holds a valid token for the next edit without a
    // follow-up read (and can see the #276 disarm on `enabled` if the restored
    // graph re-introduced a schedule).
    Ok(Json(graph_with_version(&company, file).await?))
}

/// Whether `wid` is a single safe on-disk filename stem — no path separators,
/// no `..`, not empty — so it can't escape the `workflows/` directory.
fn safe_wid(wid: &str) -> bool {
    use std::path::Component;
    let mut comps = FsPath::new(wid).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// The sub-resource path (`wid`); the scope `id` is consumed by the extractor.
#[derive(Debug, Deserialize)]
struct WorkflowPath {
    wid: String,
}

/// The run body: an optional trigger `input` payload seeded as the trigger
/// node's item. An empty object (`{}`) runs with a null input.
#[derive(Debug, Default, Deserialize)]
struct RunWorkflowBody {
    #[serde(default)]
    input: Value,
    /// Return as soon as the run has an id, instead of holding the request open
    /// for the whole run (issue #383).
    ///
    /// **Opt-in, and compatible in both directions.** A caller that omits it
    /// gets today's synchronous response byte-for-byte. A newer console talking
    /// to an *older* host sends it and the old host ignores the unknown field
    /// (this struct has no `deny_unknown_fields`) and answers the full
    /// synchronous 200 — which is exactly why the console must decide what
    /// happened from the response's **shape**, not from what it asked for.
    #[serde(default)]
    detach: bool,
    /// Run as a **dry run / test run** (issue #542): walk the real graph with
    /// real branch selection over stubbed effectful capabilities, so the run
    /// proves routing and output shape without any real effect — no agent
    /// inference, no tool/http execution, no delivery, no journaling, no gate
    /// parked in Approvals.
    ///
    /// **Opt-in and compatible in both directions, exactly like `detach`.** A
    /// caller that omits it gets today's behaviour byte-for-byte. A newer
    /// console asking an *older* host for a dry run sends it and the old host
    /// ignores the unknown field (no `deny_unknown_fields`) and runs it **FOR
    /// REAL** — which is why the response carries a `dryRun` presence
    /// discriminator the console must read, never trusting what it asked for.
    #[serde(default)]
    dry_run: bool,
}

/// The run response: the engine's final state plus any nodes left pending
/// approval.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunWorkflowResponse {
    output: Value,
    pending_approvals: Vec<String>,
    /// One row per attempt to route a reached `output` node's report to its
    /// destination (issue #170). Empty for a graph that routes nothing. This is
    /// where an operator learns a report was NOT delivered — a delivery failure
    /// never fails the run, so it has nowhere else to surface.
    deliveries: Vec<crate::ports::DeliveryReport>,
    /// The run's correlation id (issue #371).
    ///
    /// Additive, and the console needs it for one specific reason: the run's
    /// progress events arrive over SSE *while this request is still in flight*,
    /// so without an id handed back the console cannot be certain the frames it
    /// has been painting belong to the run it just awaited rather than a cron
    /// fire that overlapped it.
    run_id: String,
    /// Whether an operator stopped this run while the request was still open
    /// (issue #383).
    ///
    /// **A synchronous run is cancellable too**, which is easy to miss: the run
    /// id is registered the moment `spawn_workflow_run` returns, and the console
    /// learns it from the `workflow_run_started` SSE frame — so
    /// `POST …/runs/{rid}/cancel` is reachable long before this response is
    /// written. When that happens the runner resolves to a cancelled run whose
    /// `output` is `null` with no approvals and no deliveries, which without
    /// this flag is indistinguishable from a run that legitimately produced null
    /// output and routed nothing.
    ///
    /// Omitted when false, exactly like
    /// [`WorkflowRunOutcome::cancelled`](WorkflowRunOutcome), so an existing
    /// caller's body is byte-unchanged.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
    /// Per-node progress for this run, in the order the nodes finished (issue
    /// #542). Carried for **every** synchronous run, not only a dry one — it is
    /// the same structural per-node timeline `GET …/workflows/runs` returns, so
    /// the run-result panel can render it without a second read. For a dry run
    /// it is the *only* record of what ran, since a test run journals nothing.
    ///
    /// Empty for a run whose nodes all failed to report (or a build with no
    /// progress observer), so an empty list means "no per-node trail", never
    /// "the run did nothing".
    nodes: Vec<WorkflowRunNode>,
    /// Whether this was a **dry run** (issue #542) — the presence discriminator.
    ///
    /// **A constant `true` when set, and absent otherwise, on purpose** — the
    /// exact shape `detached` takes for #383. A newer console asking an older
    /// host for a dry run gets a *real* run back (the old host ignored the
    /// unknown request field), and the body then carries no `dryRun` key. So the
    /// console cannot tell a dry run from a real one by what it asked for — only
    /// by what came back. A field that is only ever `true` makes that a presence
    /// check, and its absence a loud signal that the run was REAL.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
    /// The board writes this run's agent nodes performed (issue #661 / M5).
    ///
    /// The same rows `GET …/workflows/runs` returns and the same rows the
    /// `WorkflowRunFinished` event carries — one shape across all three, so the
    /// console reads a run's board effects identically whether it awaited the run
    /// or found it in the history.
    ///
    /// Omitted when empty, so an existing caller's body is byte-unchanged for every
    /// run that touched no card.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    board: Vec<crate::ports::WorkflowRunBoardRow>,
    /// The nodes this run blocked on a human (issue #881).
    ///
    /// The same rows `GET …/workflows/runs` returns and the same rows the
    /// `WorkflowRunFinished` event carries — one shape across all three, so a
    /// console reads a blocked run identically whether it awaited it or found
    /// it in the history. Omitted when empty, so a run that blocked on nobody
    /// is byte-unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocked_nodes: Vec<crate::ports::WorkflowBlockedNode>,
    /// The approvals this run parked (issue #880) — what it opened, not what
    /// is still outstanding.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    approvals: Vec<crate::ports::WorkflowRunApprovalRow>,
}

/// The `detach: true` response (issue #383): the run's id, handed back before
/// the engine has walked a single node.
///
/// **`detached` is the discriminator, and it is a constant `true` on purpose.**
/// A newer console pointed at an older host sends `detach` and gets the *full
/// synchronous* body back, because the old host ignores the unknown field. So
/// the console cannot tell the two apart by what it asked for — only by what
/// came back. `output` present means the run already settled; `detached`
/// present means watch the stream. A field that is only ever `true` is what
/// makes that a presence check rather than a guess.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetachedRunResponse {
    run_id: String,
    detached: bool,
}

/// The two shapes `POST …/workflows/{wid}/run` can answer with.
///
/// An enum rather than a bare [`Response`] so the two bodies stay typed and the
/// status codes live in one place: `200` for the settled run the route has
/// always returned, `202 Accepted` for a run that has been accepted and started
/// but has not finished — which is precisely what `202` means.
enum RunWorkflowOk {
    /// Boxed: the settled body is far wider than the detached one (it carries
    /// the run's output, node trail, deliveries, board rows and — since #881 /
    /// #880 — its blocked nodes and parked-approval receipts), and holding it
    /// inline made the 202 path pay that width too.
    Settled(Box<RunWorkflowResponse>),
    Detached(DetachedRunResponse),
}

impl IntoResponse for RunWorkflowOk {
    fn into_response(self) -> Response {
        match self {
            Self::Settled(body) => Json(body).into_response(),
            Self::Detached(body) => (StatusCode::ACCEPTED, Json(body)).into_response(),
        }
    }
}

/// Registers a run with the company's [`RunSupervisor`] and drives it on its own
/// task (issue #383).
///
/// # Why both modes go through here
///
/// The detached mode obviously needs a spawned task — there is no request left
/// to hold it. The *synchronous* mode does not, and routes it through anyway,
/// which buys something the old inline `await` did not have: the run no longer
/// dies with the connection. Axum drops a handler future when the client goes
/// away, so before this, a `curl` killed mid-run took the run's remaining nodes
/// with it — leaving a `WorkflowRunStarted` with no finish, which the boot sweep
/// then stamped "interrupted by a host restart" even though no restart happened.
/// A spawned task outlives the handler, so the sync path now journals its
/// outcome whether or not anyone is still listening.
///
/// The [`RunGuard`](crate::runtime::RunGuard) is moved into the task and held
/// across the `record_run_finished`, so a run stays cancellable right up to the
/// moment it settles and not one moment after.
///
/// A **fresh task is correct here** for the same reason the cron scheduler's is
/// (see `workflow_scheduler`): the `WORKFLOW_DEPTH` re-entry guard counts one
/// causal chain, and an operator pressing Run is a new root at depth 0. What
/// would break the guard is spawning *inside* an existing run's chain — which is
/// why the orchestrator's `run_workflow` tool deliberately does NOT use this.
fn spawn_workflow_run(
    runtime: &crate::company::runtime::CompanyRuntime,
    runner: std::sync::Arc<dyn crate::ports::WorkflowRunner>,
    workflow: WorkflowFile,
    input: Value,
    dry_run: bool,
) -> crate::Result<(
    String,
    tokio::task::JoinHandle<crate::Result<crate::ports::WorkflowRun>>,
)> {
    // Issue #395: the supervisor registration and the both-arms outcome
    // journalling that used to live here now live in `WorkflowSpawn`, because
    // approving a paused workflow gate starts a run too and owes exactly the
    // same two things. One copy of the discipline, two entry points.
    //
    // Issue #401: `spawn` is fallible — a company at its in-flight run ceiling
    // is refused here, before any task is spawned, and the caller maps that to
    // a 429.
    //
    // Issue #542: `dry_run` rides through to the spawn task, which stamps it on
    // the run context and skips the outcome journal write when set.
    crate::runtime::WorkflowSpawn::new(runtime, runner).spawn(workflow, input, false, dry_run)
}

/// `POST …/workflows/{wid}/run` (both scope forms).
async fn run_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    body: Option<Json<RunWorkflowBody>>,
) -> Result<RunWorkflowOk, Response> {
    // No runner wired. THREE very different causes look identical from here —
    // `workflow_runner() == None` — and each points the operator at a different
    // next step (issues #266, #514):
    //   1. this build/deployment has no workflow execution at all — nothing the
    //      operator can do, so "not wired in this deployment" is the truth;
    //   2. this *boot* has none, because the company started with no inference
    //      source but one is saved now. The runner is populated from the harness
    //      arm at build time, so configuring inference afterwards leaves it
    //      `None` until a restart — reported as `restart_required` (#266);
    //   3. nothing was ever configured, but this host can run the harness. The
    //      fix is to configure an inference source (which rebuilds in place,
    //      #290) — reported as `inference_required`, not the `not_wired` 404 that
    //      would send the operator hunting a deployment problem that does not
    //      exist (#514).
    let Some(runner) = company.runtime.workflow_runner() else {
        use super::inference::RunnerGap;
        return Err(
            match super::inference::runner_gap_for(company.runtime.as_ref()).await {
                RunnerGap::RestartPending => super::restart_required("workflow execution"),
                RunnerGap::InferenceRequired => super::inference_required("workflow execution"),
                RunnerGap::NotWired => super::not_wired("workflow execution"),
            },
        );
    };

    // `wid` becomes a filename — reject anything that could escape `workflows/`.
    if !safe_wid(&wid) {
        return Err(
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response(),
        );
    }

    // Load the saved graph from the seed ∪ overlay union, so a graph created on
    // a hosted tenant (no source directory) runs the same as a committed one.
    let (overlays, globals_disable) = overlay_workflows_and_globals(&company)
        .await
        .map_err(IntoResponse::into_response)?;
    let file = load_workflow_with_globals(
        company.runtime.source_dir(),
        &overlays,
        &globals_disable,
        &wid,
    )
    .map_err(|e| ApiError(e).into_response())?
    .ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response()
    })?;

    let body = body.map(|Json(b)| b).unwrap_or_default();
    let detach = body.detach;
    // Issue #542: captured before `body.input` moves, and threaded into both the
    // spawn (so the run runs dry) and the settled response's discriminator (so
    // the console can confirm the host honoured the request rather than running
    // for real).
    let dry_run = body.dry_run;

    // Issue #383: registered and spawned before either mode branches, so the two
    // modes cannot drift in what they start. Issue #228's journalling now lives
    // inside the task rather than around this await.
    //
    // Issue #401: the concurrency ceiling is enforced HERE, before the
    // detach/sync branch below, so both modes refuse identically — a 429 with
    // the actionable `{error, code: "workflow_run_limit"}` envelope and no run
    // id, because nothing started. The rejection precedes any task or any
    // `WorkflowRunStarted`, so there is nothing to unwind.
    let (run_id, handle) = spawn_workflow_run(
        company.runtime.as_ref(),
        runner.clone(),
        file,
        body.input,
        dry_run,
    )
    .map_err(|e| ApiError(e).into_response())?;

    if detach {
        // Returned before the engine has walked a node. From here the client
        // follows the run through the SSE frames issue #371 already keys by this
        // id, and reads its outcome back from `GET …/workflows/runs`, whose fold
        // already reports `running: true` for a run in flight.
        //
        // The task is deliberately NOT joined and its handle is dropped: it
        // settles itself, journals its own outcome, and its guard deregisters
        // it. Detaching is the entire point.
        return Ok(RunWorkflowOk::Detached(DetachedRunResponse {
            run_id,
            detached: true,
        }));
    }

    // The synchronous mode, whose response is unchanged: await the task rather
    // than the runner. A `JoinError` here means the run task panicked or was
    // aborted — the run's outcome was never journaled, so there is nothing
    // truthful to hand back and this is a genuine 500 rather than a run result.
    match handle.await {
        Ok(Ok(run)) => Ok(RunWorkflowOk::Settled(Box::new(RunWorkflowResponse {
            output: run.output,
            pending_approvals: run.pending_approvals,
            deliveries: run.deliveries,
            run_id,
            cancelled: run.cancelled,
            // Issue #542: the runner collects this per-node trail on every run;
            // map the port rows onto the wire shape the history route already
            // uses. `dry_run` is the request's, echoed back as the presence
            // discriminator a console pointed at an old host would never see.
            nodes: run.nodes.into_iter().map(WorkflowRunNode::from).collect(),
            dry_run,
            // Issue #661 (M5): carried on the synchronous path too, so a console
            // that pressed Run learns what the run did to the board without a
            // second read of the history.
            board: run.board,
            // Issues #881 / #880: likewise. An operator who pressed Run and
            // watched eight green nodes come back is exactly the reader these
            // two exist for — the run drawer is where they first learn the
            // pipeline delivered nothing and why.
            blocked_nodes: run.blocked_nodes,
            approvals: run.approvals,
        }))),
        Ok(Err(err)) => Err(ApiError(err).into_response()),
        Err(join) => {
            tracing::error!(
                company = %company.id(),
                workflow = %wid,
                %run_id,
                %join,
                "workflow run task did not complete; no outcome was journaled for it"
            );
            // `BackgroundTask`, not a harness error: the distinction it draws —
            // "the work's outcome is unknown", as opposed to "the work failed" —
            // is exactly right here. The run may even have done most of its
            // nodes; what is missing is an answer.
            Err(ApiError(OpenCompanyError::BackgroundTask(
                "the workflow run task did not complete".to_string(),
            ))
            .into_response())
        }
    }
}

/// The sub-resource path on the cancel route: the run id.
#[derive(Debug, Deserialize)]
struct RunPath {
    rid: String,
}

/// The cancel acknowledgement (issue #383).
///
/// `cancelling`, not `cancelled`: this route fires a signal and returns. The run
/// is stopped at the engine future's next suspension point and settles itself a
/// moment later with a `WorkflowRunFinished{cancelled: true}` — which is the
/// event the console should believe, not this body.
#[derive(Debug, Serialize)]
struct CancelRunResponse {
    cancelling: bool,
}

/// `POST …/workflows/runs/{rid}/cancel` (both scope forms) — stop a run that is
/// still walking its graph (issue #383).
///
/// # Who may cancel
///
/// Anyone who passes this route's [`ScopedCompany`] guard, i.e. any operator of
/// the company. There is deliberately no "only the operator who started it"
/// rule: a run is a company-level activity, the console shows it to every
/// operator, and the case this exists for — a run wedged on a slow agent node —
/// is exactly the one where whoever started it may have gone home.
///
/// # 404 covers two cases, and means one thing
///
/// An unknown run id and an already-settled run both answer `404`. They are the
/// same answer to the operator: there is nothing here to stop. Keeping a
/// tombstone to tell them apart would mean picking an expiry for it, and the run
/// history already says what became of a settled run.
///
/// # What a cancelled run leaves behind
///
/// Journaled node rows for the nodes that completed, a `WorkflowRunFinished`
/// carrying `cancelled: true` and no error, and **any approvals earlier nodes
/// parked stay valid in the queue** — those are journal-backed and independent
/// of the run, so an operator may still approve or deny them afterwards. No
/// grant minted during the run is revoked.
async fn cancel_workflow_run(
    company: ScopedCompany,
    Path(RunPath { rid }): Path<RunPath>,
) -> Result<Json<CancelRunResponse>, ApiError> {
    if company.runtime.run_supervisor().cancel(&rid) {
        return Ok(Json(CancelRunResponse { cancelling: true }));
    }
    Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
        "workflow run {rid}"
    ))))
}

/// `GET …/workflows/runs/{rid}/output` (both scope forms) — the durable per-node
/// output snapshot of one past (or live-and-settled) run (issue #596).
///
/// This is the data the run inspector renders: `{ runId, workflowId, atMillis,
/// nodes, truncated }`, where `nodes` is the engine's `{ "<node id>": { "items":
/// [ … ] } }` map, bounded for storage. The console opens one node in a past run
/// and shows what it produced — the make.com per-node output view.
///
/// # 404 means "no output snapshot", and that covers three honest cases
///
/// A `404` here is not an error the console should surface loudly: it means this
/// run has no stored output, which is true for **every run that predates this
/// feature**, for a **dry run** (writes nothing durable), and for a
/// **hard-aborted** run (dropped mid-flight, no outcome to persist). The console
/// renders an explicit empty state ("this run predates output capture / produced
/// none") rather than a failure. An unknown run id lands here too, and means the
/// same thing to the operator: there is nothing to show.
///
/// Deliberately a lazy per-run fetch rather than a field folded into
/// [`list_runs`]: that fold is already expensive, and the inspector only ever
/// needs the one run an operator clicked into.
async fn get_run_output(
    company: ScopedCompany,
    Path(RunPath { rid }): Path<RunPath>,
) -> Result<Json<crate::ports::WorkflowRunOutputRecord>, ApiError> {
    match company
        .runtime
        .workflow_run_outputs()
        .get_run_output(company.id(), &rid)
        .await
        .map_err(ApiError)?
    {
        Some(record) => Ok(Json(record)),
        None => Err(ApiError(OpenCompanyError::NotFound(format!(
            "no output captured for workflow run {rid}"
        )))),
    }
}

// ---------------------------------------------------------------------------
// Cron preview (issue #262)
// ---------------------------------------------------------------------------

/// How many upcoming fire times the preview returns. Three is enough to show
/// the *interval* — one time tells you when, three tell you how often — without
/// turning a one-line hint into a table.
const CRON_PREVIEW_FIRES: usize = 3;

/// The preview request: the expression as typed, plus an optional instant to
/// compute the next fires from.
#[derive(Debug, Deserialize)]
struct PreviewCronBody {
    expr: String,
    /// Epoch millis to search forward from. Defaults to now; present so tests
    /// can pin the answer instead of asserting against a moving clock.
    #[serde(default)]
    after: Option<u64>,
}

/// The preview response. Untagged, so the two outcomes are two shapes rather
/// than one shape with half its fields null.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum PreviewCronResponse {
    /// A valid expression: what it means (`null` when the shape is one
    /// [`CronExpr::describe`] declines to paraphrase) and when it next fires.
    Parsed {
        description: Option<String>,
        /// Epoch millis, ascending. The console renders each one twice — UTC
        /// and the viewer's local zone — from this single number, which is what
        /// makes the two readings incapable of disagreeing.
        next: Vec<u64>,
    },
    /// A malformed expression, carrying the parser's own message.
    Invalid { error: String },
}

/// `POST …/workflows/cron/preview` (both scope forms).
///
/// Issue #262: a trigger's schedule is a bare cron field, and the failure that
/// is NOT handled is the *successful* one — `0 9 * * *` saves cleanly whether or
/// not the author meant 9am, and it is UTC whether or not they read the hint.
/// Echoing the parsed meaning and the next fire times is the only thing that
/// turns a silently-wrong schedule into an obviously-wrong one.
///
/// **Always answers 200**, including for a malformed expression. The console
/// calls this while the author is still typing, so half-written garbage is the
/// normal state, not an exception — and its HTTP client throws on any non-2xx,
/// so a 400 per keystroke would make an ordinary parse failure arrive as a
/// thrown error and force try/catch as control flow. The rejection that matters
/// still happens: the create route validates the schedule and refuses to save a
/// bad one.
///
/// Scoped (and so authenticated) like every other route in this module even
/// though the computation touches no company state — an unauthenticated compute
/// endpoint would be a new kind of surface here for no gain.
async fn preview_cron(
    _company: ScopedCompany,
    Json(body): Json<PreviewCronBody>,
) -> Json<PreviewCronResponse> {
    let expr = match CronExpr::parse(&body.expr) {
        Ok(expr) => expr,
        Err(err) => {
            return Json(PreviewCronResponse::Invalid {
                error: err.to_string(),
            });
        }
    };

    let after = body.after.unwrap_or_else(now_millis);
    let mut cursor = CivilTime::from_unix_millis(after);
    let mut next = Vec::with_capacity(CRON_PREVIEW_FIRES);
    for _ in 0..CRON_PREVIEW_FIRES {
        // `next_after` is bounded and returns `None` only for an expression
        // that can never fire, which a parsed one cannot be — but stopping
        // early is still the right answer if that ever changes.
        let Some(fire) = expr.next_after(&cursor) else {
            break;
        };
        next.push(fire.unix_millis());
        cursor = fire;
    }

    Json(PreviewCronResponse::Parsed {
        description: expr.describe(),
        next,
    })
}

/// Wall-clock epoch millis. Saturates at the epoch rather than panicking on a
/// clock set before 1970 — a preview is not worth a 500.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Create-time copilot (issue #753)
// ---------------------------------------------------------------------------

/// The cap on a create-time copilot description (issue #753), in codepoints —
/// applied to the request body before it reaches the metered draft path. Gated
/// with the handler arm that reads it: the default build's `not_wired` arm never
/// drafts, so it never caps.
#[cfg(feature = "openhuman")]
const MAX_DRAFT_DESCRIPTION_CHARS: usize = 4_000;

/// The `POST …/workflows/draft-from-description` body: a free-text description of
/// the workflow the operator wants built.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftFromDescriptionBody {
    description: String,
}

/// The draft-from-description answer (issue #753).
///
/// Like the cron preview, it answers **200 in both model-answer cases** — a
/// drafted graph, or an honest "this is better done once" — because neither is
/// an error the operator fixes by retrying differently; the console renders
/// whichever came back and keys on `automatable`. Only a request problem (an
/// empty description → 400) or a capability gap (no brain wired → 404/409) is a
/// non-2xx.
#[derive(Debug, Serialize)]
#[serde(untagged)]
// The default build's `not_wired` arm returns this type but constructs neither
// variant — only the `openhuman` arm answers 200. The variants are live under
// the feature CI actually builds and tests, so this is a cfg artefact, not a
// dead type.
#[cfg_attr(not(feature = "openhuman"), allow(dead_code))]
enum DraftFromDescriptionResponse {
    /// A drafted graph for the New-workflow dialog to hydrate its form from.
    /// `workflow` is a `WorkflowGraphSpec` — the same camelCase node/edge shape
    /// the read routes return, so the console loads it with no adapter.
    Drafted {
        automatable: bool,
        summary: String,
        workflow: Value,
        /// Host corrections the operator should see (issue #813) — e.g. a
        /// name/role→id rewrite the resolver made. Empty when the draft needed
        /// none; `#[serde(default)]` on the reader keeps the older shape valid.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notes: Vec<String>,
    },
    /// The described work is not worth a reusable workflow — or could not be
    /// drafted into one that would survive Create; `reason` says why.
    NotAutomatable { automatable: bool, reason: String },
}

/// `POST …/workflows/draft-from-description` (both scope forms) — the New-workflow
/// dialog's copilot (issue #753). Drafts a graph from the operator's description
/// and hands it back for review; it never persists, so the ordinary Create path
/// (`POST …/workflows`) stays the only way a graph reaches the workflow list.
#[cfg(feature = "openhuman")]
async fn draft_from_description(
    company: ScopedCompany,
    Json(body): Json<DraftFromDescriptionBody>,
) -> Result<Json<DraftFromDescriptionResponse>, Response> {
    let description = body.description.trim();
    if description.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "describe the workflow you want in a sentence or two.".to_string(),
        ))
        .into_response());
    }
    // Char-safe cap on the request body before it reaches the metered path.
    let description: String = description
        .chars()
        .take(MAX_DRAFT_DESCRIPTION_CHARS)
        .collect();

    // No builder wired: classify WHY exactly as the run route does (issues #266,
    // #514), so the console points the operator at the same next step — restart,
    // configure inference, or "not in this deployment" — instead of a bare fail.
    if company.runtime.builder().is_none() {
        use super::inference::RunnerGap;
        return Err(
            match super::inference::runner_gap_for(company.runtime.as_ref()).await {
                RunnerGap::RestartPending => super::restart_required("the workflow copilot"),
                RunnerGap::InferenceRequired => super::inference_required("the workflow copilot"),
                RunnerGap::NotWired => super::not_wired("the workflow copilot"),
            },
        );
    }

    use crate::harness::workflow_build::{
        DescriptionDraftOutcome, draft_workflow_from_description,
    };
    match draft_workflow_from_description(&company.runtime, &description).await {
        Ok(DescriptionDraftOutcome::Graph {
            summary,
            spec,
            notes,
        }) => Ok(Json(DraftFromDescriptionResponse::Drafted {
            automatable: true,
            summary,
            workflow: serde_json::to_value(&spec).unwrap_or(Value::Null),
            notes,
        })),
        Ok(DescriptionDraftOutcome::NotAutomatable(reason)) => {
            Ok(Json(DraftFromDescriptionResponse::NotAutomatable {
                automatable: false,
                reason,
            }))
        }
        // A read the drafter could not proceed without (the company record) — a
        // genuine 500, not a model answer.
        Err(err) => Err(ApiError(err).into_response()),
    }
}

/// `POST …/workflows/draft-from-description` on a build with no harness. The
/// copilot needs the embedded brain, so it answers `not_wired` — the same 404
/// the run route's default-build arm gives. The empty-description 400 still runs
/// first, so the contract's shape is identical across builds.
#[cfg(not(feature = "openhuman"))]
async fn draft_from_description(
    company: ScopedCompany,
    Json(body): Json<DraftFromDescriptionBody>,
) -> Result<Json<DraftFromDescriptionResponse>, Response> {
    let _ = &company;
    if body.description.trim().is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "describe the workflow you want in a sentence or two.".to_string(),
        ))
        .into_response());
    }
    Err(super::not_wired("the workflow copilot"))
}

// ---------------------------------------------------------------------------
// Fix a failed run with the copilot (issue #840, PR-3)
// ---------------------------------------------------------------------------

/// The `POST …/workflows/{wid}/fix-from-run` body (issue #840, PR-3): the failed
/// run to correct from, plus an optional caller-supplied error hint for a run the
/// journal never recorded a failure for (or that predates the failure journal).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixFromRunBody {
    /// The failed run's correlation id — the `runId` the run-history row carries.
    run_id: String,
    /// The run's error as the row already shows it, used only when the journal has
    /// no `WorkflowRunFinished{error}` for `run_id` to read.
    #[serde(default)]
    error_hint: Option<String>,
}

/// The static authoring readiness of a corrected graph (issue #840, PR-3) —
/// **advisory only**. `ok` is whether the always-compiled tinyflows authoring
/// gates found nothing; `advisories` names each remaining smell for the operator
/// to look at before saving. It NEVER blocks the save (Save is still the only
/// write), so a non-`ok` readiness rides a 200 alongside the corrected graph.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(feature = "openhuman"), allow(dead_code))]
struct ReadinessNote {
    ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    advisories: Vec<String>,
}

/// The fix-from-run answer (issue #840, PR-3), mirroring
/// [`DraftFromDescriptionResponse`]: **200 in both model-answer cases** — a
/// corrected graph to review, or an honest "this cannot be fixed by re-wiring".
/// Only a request problem (no error to fix from → 400) or a capability gap (no
/// brain wired → 404/409) is a non-2xx.
#[derive(Debug, Serialize)]
#[serde(untagged)]
// Only the `openhuman` arm constructs these variants; the default build's
// `not_wired` arm returns the type without building either. Live under the feature
// CI builds and tests, so this is a cfg artefact, not a dead type.
#[cfg_attr(not(feature = "openhuman"), allow(dead_code))]
enum FixFromRunResponse {
    /// A corrected graph for the edit dialog to hydrate, with the static readiness
    /// advisories over it. `workflow` is a `WorkflowGraphSpec` — the same camelCase
    /// node/edge shape the read routes return, and it keeps the SAME id as `wid` so
    /// the operator's Save is a new version, not an orphan.
    Fixed {
        automatable: bool,
        summary: String,
        workflow: Value,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        notes: Vec<String>,
        readiness: ReadinessNote,
    },
    /// The failure could not be fixed by re-wiring the graph with the teammates
    /// and tools available; `reason` says why.
    NotAutomatable { automatable: bool, reason: String },
}

/// The failure a past run recorded, scanned out of the company journal for a
/// `run_id` (issue #840, PR-3).
#[cfg(feature = "openhuman")]
struct JournaledFailure {
    /// The run's error, when it failed outright. `None` for a run that finished
    /// clean (fixing which makes no sense unless the caller passes a hint).
    error: Option<String>,
    /// The id of the node whose step errored, when the per-node trail named one.
    failed_node_id: Option<String>,
}

/// Scans the company journal for what run `run_id` recorded (issue #840, PR-3).
/// `None` means no `WorkflowRunFinished` for that id exists — the caller falls back
/// to a caller-supplied hint. Follows the same whole-log fold `list_runs` uses.
#[cfg(feature = "openhuman")]
async fn journaled_run_failure(
    company: &ScopedCompany,
    run_id: &str,
) -> Result<Option<JournaledFailure>, ApiError> {
    let stored = company
        .runtime
        .events()
        .read_from(company.id(), EventSeq::new(0), usize::MAX)
        .await
        .map_err(ApiError)?;

    let mut failed_node_id: Option<String> = None;
    // `Some(error)` once the run's finish is seen; the outer Option distinguishes
    // "the run finished (maybe cleanly)" from "no finish for this id at all".
    let mut finished: Option<Option<String>> = None;
    for stored in stored {
        match stored.event {
            CompanyEvent::WorkflowNodeFinished {
                run_id: rid,
                node_id,
                status,
                ..
            } if rid == run_id && status == WorkflowNodeStatus::Error => {
                failed_node_id = Some(node_id);
            }
            CompanyEvent::WorkflowRunFinished {
                run_id: Some(rid),
                error,
                ..
            } if rid == run_id => {
                finished = Some(error);
            }
            _ => {}
        }
    }
    Ok(finished.map(|error| JournaledFailure {
        error,
        failed_node_id,
    }))
}

/// Resolves the error + failing node a fix should be grounded on from what the
/// journal recorded and what the caller hinted (issue #840, PR-3). `None` means
/// there is nothing to fix from: neither a journaled error nor a usable hint.
///
/// A pure decision, factored out of [`fix_from_run`] so the fallback matrix — a
/// journaled error, a hint fallback, a clean run with no hint — is unit-testable
/// without a running host.
#[cfg(feature = "openhuman")]
fn resolve_fix_error(
    journaled: Option<JournaledFailure>,
    hint: Option<String>,
) -> Option<(String, Option<String>)> {
    let (error, failed_node_id) = match journaled {
        Some(j) => (j.error.or(hint), j.failed_node_id),
        // No finish for this run id in the journal — lean entirely on the hint.
        None => (hint, None),
    };
    let error = error.filter(|e| !e.trim().is_empty())?;
    Some((error, failed_node_id))
}

/// `POST …/workflows/{wid}/fix-from-run` (both scope forms) — correct a saved
/// workflow whose run failed, with the create-time copilot (issue #840, PR-3).
/// Drafts a corrected graph and hands it back for the edit dialog to hydrate; it
/// never persists, so Save (`PUT …/workflows/{wid}`) stays the only write, and the
/// corrected graph keeps the same id so Save is a new version of the workflow.
#[cfg(feature = "openhuman")]
async fn fix_from_run(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    Json(body): Json<FixFromRunBody>,
) -> Result<Json<FixFromRunResponse>, Response> {
    // `wid` becomes a filename on the read below — reject anything that could
    // escape `workflows/`.
    if !safe_wid(&wid) {
        return Err(
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response(),
        );
    }

    // No builder wired: classify WHY exactly as the draft + run routes do (issues
    // #266, #514), so the console points the operator at the same next step.
    if company.runtime.builder().is_none() {
        use super::inference::RunnerGap;
        return Err(
            match super::inference::runner_gap_for(company.runtime.as_ref()).await {
                RunnerGap::RestartPending => super::restart_required("the workflow copilot"),
                RunnerGap::InferenceRequired => super::inference_required("the workflow copilot"),
                RunnerGap::NotWired => super::not_wired("the workflow copilot"),
            },
        );
    }

    // Load the saved graph for `wid` (seed ∪ overlay) and convert it to the spec
    // the copilot corrects and pins its identity to.
    let (overlays, globals_disable) = overlay_workflows_and_globals(&company)
        .await
        .map_err(IntoResponse::into_response)?;
    // A source-defined workflow (seed-backed, or seed-shadowed) can never take
    // the correction: `PUT …/workflows/{wid}` refuses it with the same 409 this
    // mirrors (`locate_editable_overlay`). Catching it here — before the copilot
    // turn — saves the tokens and the wait on a proposal the operator could never
    // save; without this a tinysweeper review flagged the route as misleading the
    // operator into drafting a fix it would then refuse.
    if !is_editable(company.runtime.source_dir(), &overlays, &wid) {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "workflow `{wid}` is defined by a file in the company source tree, so a copilot fix \
             can't be saved for it. Edit `workflows/{wid}.toml` in the company repository instead."
        )))
        .into_response());
    }
    let file = load_workflow_with_globals(
        company.runtime.source_dir(),
        &overlays,
        &globals_disable,
        &wid,
    )
    .map_err(|e| ApiError(e).into_response())?
    .ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response()
    })?;
    // `workflow_spec_from_graph` below has no `on_error`/`retry` field on
    // `WorkflowNodeSpec` (the builder never authors them — see its own doc
    // comment), so a node that had either set loses it silently once the
    // operator saves the correction. Correlating retry/error policy across a
    // copilot rewrite that may rename or drop nodes is the harder problem this
    // PR does not take on; naming it in a note at least makes the loss visible
    // instead of silent.
    let dropped_error_policy_nodes: Vec<String> = file
        .nodes
        .iter()
        .filter(|n| n.on_error.is_some() || n.retry.is_some())
        .map(|n| n.name.clone())
        .collect();
    let spec = crate::company::workflow_spec_from_graph(file);

    // The failure to correct from: prefer what the run journaled, fall back to the
    // caller's hint. Neither → there is nothing to fix from.
    let journaled = journaled_run_failure(&company, &body.run_id)
        .await
        .map_err(IntoResponse::into_response)?;
    let hint = body
        .error_hint
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    let Some((error, failed_node_id)) = resolve_fix_error(journaled, hint) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this run recorded no error to fix from — reopen the run, or pass its error as a hint."
                .to_string(),
        ))
        .into_response());
    };
    // The journal names a node id; the human-readable name comes from the saved
    // graph the id belongs to.
    let failed_node_name = failed_node_id
        .as_deref()
        .and_then(|id| spec.nodes.iter().find(|n| n.id == id))
        .map(|n| n.name.clone());

    use crate::harness::workflow_build::{
        DescriptionDraftOutcome, RunFailureContext, fix_workflow_from_failure, workflow_readiness,
    };
    let failure = RunFailureContext {
        run_id: body.run_id.clone(),
        error,
        failed_node_id,
        failed_node_name,
    };
    match fix_workflow_from_failure(&company.runtime, &spec, &failure).await {
        Ok(DescriptionDraftOutcome::Graph {
            summary,
            spec,
            mut notes,
        }) => {
            let (ok, advisories) = workflow_readiness(&spec);
            if !dropped_error_policy_nodes.is_empty() {
                notes.push(format!(
                    "on_error/retry on {} — this correction does not carry per-node error \
                     policy through; reapply it after reviewing if the node is still there.",
                    dropped_error_policy_nodes.join(", ")
                ));
            }
            Ok(Json(FixFromRunResponse::Fixed {
                automatable: true,
                summary,
                workflow: serde_json::to_value(&spec).unwrap_or(Value::Null),
                notes,
                readiness: ReadinessNote { ok, advisories },
            }))
        }
        Ok(DescriptionDraftOutcome::NotAutomatable(reason)) => {
            Ok(Json(FixFromRunResponse::NotAutomatable {
                automatable: false,
                reason,
            }))
        }
        // A read the drafter could not proceed without — a genuine 500.
        Err(err) => Err(ApiError(err).into_response()),
    }
}

/// `POST …/workflows/{wid}/fix-from-run` on a build with no harness (issue #840,
/// PR-3). The copilot needs the embedded brain, so it answers `not_wired` — the
/// same 404 the draft route's default-build arm gives.
#[cfg(not(feature = "openhuman"))]
async fn fix_from_run(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    Json(body): Json<FixFromRunBody>,
) -> Result<Json<FixFromRunResponse>, Response> {
    let _ = (&company, &wid, &body.run_id, &body.error_hint);
    Err(super::not_wired("the workflow copilot"))
}

/// The `GET …/workflows/tool-slugs` answer (issues #783, #874): the tools the
/// per-workflow copilot may ground a proposal on, and — separately — the ones
/// this company holds a grant for that cannot run on this deployment.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowToolSlugsResponse {
    /// The **effective** slugs: granted by `[tools].allow` *and* wired here, so
    /// a proposed `tool_call` naming one has a chance of running. This is the
    /// only list a prompt should be grounded on.
    slugs: Vec<String>,
    /// Granted, but not wired on this deployment (issue #874). Reported rather
    /// than silently dropped so a reader can tell "this company is not allowed
    /// that tool" (absent from both lists) from "allowed, nobody has configured
    /// the provider yet" (here) — and so authoring ahead of wiring, which
    /// create validation still permits, remains a visible option.
    ///
    /// Empty when the wiring is not knowable (no harness deps attached): the
    /// honest answer to "which of these are unwired" is then "cannot say", and
    /// `slugs` degrades to the grant-only set rather than emptying out.
    unwired: Vec<UnwiredWorkflowTool>,
}

/// One granted-but-unwired tool, with the reason it cannot run here (issue
/// #874) — the same distinction
/// [`refusal_for`](crate::workflows::caps) draws at run time, moved forward to
/// the moment the console asks, instead of arriving as a failed run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnwiredWorkflowTool {
    slug: String,
    /// A stable machine token for a client that wants to branch:
    /// `searchBackendNotConfigured`, `capabilityTierFiltered`, or the
    /// cause-less `unwired`. Treat it as open — a new deployment-wiring cause
    /// adds a token here, so match the ones you handle and fall back to
    /// [`detail`](Self::detail) rather than assuming the set is closed.
    reason: &'static str,
    /// The same sentence in prose, for a client that just wants to show it.
    detail: &'static str,
}

/// `GET …/workflows/tool-slugs` (both scope forms) — the per-workflow copilot's
/// tool grounding (issue #783), narrowed to the **effective** set by issue #874.
///
/// Answers what
/// [`workflow_effective_tool_slugs`](crate::company::workflow_effective_tool_slugs)
/// computes — catalogue, company grant and deployment wiring all agreeing — so
/// this route and the in-process create/fix copilot
/// (`crate::harness::workflow_build`) ground on one set and cannot drift.
///
/// It deliberately does **not** answer the wider *grant-only* set that
/// create/save validation accepts. That gate stays permissive on purpose so an
/// operator may author now and wire the provider later, and this route does not
/// change it. Serving the grant-only set here was issue #874 — a
/// granted-but-unwired `web_search` was offered to the copilot, which authored a
/// node that failed at the first run.
#[cfg(feature = "openhuman")]
async fn workflow_tool_slugs(
    company: ScopedCompany,
) -> Result<Json<WorkflowToolSlugsResponse>, Response> {
    let record = company
        .runtime
        .store()
        .load(company.runtime.id())
        .await
        .map_err(|err| ApiError(err).into_response())?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(
                company.runtime.id().to_string(),
            ))
            .into_response()
        })?;
    // `None` — no harness deps on this runtime — means the wiring is unknowable,
    // not that nothing is wired. Both helpers below read it that way: `slugs`
    // falls back to the grant-only set and `unwired` stays empty, which is the
    // pre-#874 answer. That keeps a harness-less host honest instead of telling
    // the copilot every granted tool is broken.
    let wiring = company.runtime.workflow_tool_wiring(&record).await;
    let wired = wiring.as_ref().map(|w| &w.wired_namespaces);
    let unwired = crate::company::workflow_granted_but_unwired_tool_slugs(&record, wired)
        .into_iter()
        .map(|slug| {
            // Every slug here came out of `WORKFLOW_TOOL_CATALOG`, whose entries
            // are pinned to `namespace_of`, and it is unwired precisely because
            // its namespace is in `missing` — so both lookups resolve and `None`
            // is unreachable in practice.
            //
            // Matched EXHAUSTIVELY rather than with a catch-all: the whole point
            // of this field is letting an operator tell one cause from another,
            // so a third `MissingReason` must break the build here instead of
            // compiling into "raise your capability tier" — advice that would be
            // actively wrong for a cause that is not tier filtering. It also
            // keeps the defensive `None` from being conflated with the tier case.
            let missing = crate::workflows::caps::workflow_tool_info(&slug)
                .map(|info| info.namespace)
                .and_then(|ns| wiring.as_ref().and_then(|w| w.missing.get(ns)).copied());
            let (reason, detail) = match missing {
                Some(crate::workflows::caps::MissingReason::SearchBackendNotConfigured) => (
                    "searchBackendNotConfigured",
                    "granted, but no managed search backend is configured on this deployment; \
                     ask the platform operator to configure search",
                ),
                Some(crate::workflows::caps::MissingReason::CapabilityTierFiltered) => (
                    "capabilityTierFiltered",
                    "granted, but the deployment's capability tier filtered it; ask the platform \
                     operator to raise the capability tier",
                ),
                // Unreachable given the pairing above; answered honestly rather
                // than guessing a cause we do not have.
                None => (
                    "unwired",
                    "granted, but not wired on this deployment; ask the platform operator why",
                ),
            };
            UnwiredWorkflowTool {
                slug,
                reason,
                detail,
            }
        })
        .collect();
    Ok(Json(WorkflowToolSlugsResponse {
        slugs: crate::company::workflow_effective_tool_slugs(&record, wired),
        unwired,
    }))
}

/// `GET …/workflows/tool-slugs` on a build with no harness. The workflow tool
/// surface lives behind the `openhuman` feature, so a default build wires no
/// `tool_call` grants at all: the honest answer is an empty list, not a 404 —
/// the copilot then grounds on "no tools" rather than being unable to tell.
#[cfg(not(feature = "openhuman"))]
async fn workflow_tool_slugs(
    company: ScopedCompany,
) -> Result<Json<WorkflowToolSlugsResponse>, Response> {
    let _ = &company;
    Ok(Json(WorkflowToolSlugsResponse {
        slugs: Vec::new(),
        unwired: Vec::new(),
    }))
}

/// The `GET …/workflows/wired-channels` answer (issue #813): the chat channel
/// ids this running company can actually deliver to, so the output-node
/// destination editor offers a picker of real targets. Not feature-gated — the
/// channel set exists on every build.
///
/// **`operator` is not among them** (issue #981). This used to say the opposite
/// and serve the unfiltered adapter list, which offered authors the one target
/// workflow delivery refuses by name. An empty list is a truthful answer for a
/// company with no desks and no provider channels: there is nowhere to deliver.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WiredChannelsResponse {
    /// The channel ids an `output` node's `channel` destination may target.
    /// Anything else is rejected when the workflow is saved, and — for a graph
    /// saved before the desk went away — fails at delivery with
    /// `ChannelNotWired`.
    channels: Vec<String>,
}

/// `GET …/workflows/wired-channels` (both scope forms). Reads the running
/// company's deliverable channels directly — infallible, no record load needed.
async fn workflow_wired_channels(company: ScopedCompany) -> Json<WiredChannelsResponse> {
    Json(WiredChannelsResponse {
        channels: company.runtime.deliverable_channel_ids(),
    })
}

// ---------------------------------------------------------------------------
// Run history (issue #228)
// ---------------------------------------------------------------------------

/// How many run outcomes `GET …/workflows/runs` returns when the caller names
/// no `?limit=`, and the ceiling it clamps a larger one to. The console's
/// history panel shows a short recent list; a bigger page would only make the
/// journal fold slower for no one's benefit.
const DEFAULT_RUN_LIMIT: usize = 20;
const MAX_RUN_LIMIT: usize = 200;

/// The `?workflow=` / `?limit=` selectors on the run-history read.
#[derive(Debug, Deserialize)]
struct RunsQuery {
    /// Return only runs of this workflow id. Absent = every workflow.
    workflow: Option<String>,
    /// Cap the page. Clamped to [`MAX_RUN_LIMIT`]; `0` falls back to the
    /// default rather than returning an empty page, which is never what a
    /// caller means.
    limit: Option<usize>,
}

/// One finished run as the console's history panel renders it (camelCase).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunOutcome {
    /// The journal sequence position — a stable, monotonic row key.
    seq: u64,
    /// Epoch-millis the outcome was journaled.
    at_millis: u64,
    workflow_id: String,
    /// Whether a cron started this run rather than an operator. The console
    /// shows the distinction because a scheduled run is the one nobody watched.
    scheduled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    /// The same delivery rows a manual run's response carries — including
    /// `target`, which the run response already ships to this same console.
    deliveries: Vec<crate::ports::DeliveryReport>,
    pending_approvals: Vec<String>,
    /// Set when the run failed outright instead of finishing with rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Per-node progress for this run, in the order the nodes finished (issue
    /// #371). Empty for a run journaled before #371, and for one whose nodes all
    /// failed to journal — so an empty list means "no per-node trail", never
    /// "the run did nothing".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    nodes: Vec<WorkflowRunNode>,
    /// The nodes this run has *begun* executing, in start order (issue #1010),
    /// folded from `WorkflowNodeStarted` (issue #382).
    ///
    /// The half of the trail the fold never carried. `nodes` is written by the
    /// *finish* bracket, so a run in flight came back listing only what was
    /// already over — and a console joining mid-run (a reload, a cron fire, an
    /// `EventSource` reconnect, or simply switching workflow and back) could
    /// render the graph's past but never the node executing right now. The
    /// engine has reported the opening bracket since #382; nothing read it.
    ///
    /// A **receipt of what started**, kept once the run settles rather than
    /// cleared: an id here with no matching `nodes` row on a settled run is the
    /// node the run was standing on when it was cancelled or lost, which is the
    /// one thing neither list says on its own. Consumers must therefore pair it
    /// with [`running`](Self::running) before painting anything as in-flight —
    /// see `statesFromRun` in the console.
    ///
    /// Omitted when empty, like `nodes` — which is every run journaled before
    /// #382 and every run whose nodes all failed to journal a start.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    started_nodes: Vec<String>,
    /// When the run *started*, from its `WorkflowRunStarted` row (issue #371).
    /// Absent on a pre-#371 row, whose only timestamp is the finish.
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at_millis: Option<u64>,
    /// `true` for a run that has started and not yet settled.
    ///
    /// Honest rather than optimistic, and only because of the boot sweep: a run
    /// whose host died is settled with an "interrupted" outcome at the next
    /// start, so nothing sits here spinning forever. Omitted when false, which
    /// keeps every settled row's wire shape as short as it was.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    running: bool,
    /// `true` for a run an operator stopped (issue #383).
    ///
    /// Separate from [`error`](Self::error) because it is a separate outcome: a
    /// cancelled run carries no error, so a console reading only `error` would
    /// render a deliberate stop as a clean success. Together with `error` these
    /// give the three terminal readings the history panel distinguishes —
    /// failed, interrupted by a host restart, stopped by an operator. Omitted
    /// when false, like `running`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
    /// System notices raised about this run (issue #638) — today, that a node
    /// gated more tool calls than the per-batch cap allows and the excess was
    /// discarded.
    ///
    /// Not an `error`: the run succeeded. The history panel renders these as a
    /// warning rather than a failure, so a run that overflowed still reads as
    /// the success it was, with something the operator needs to know attached.
    /// Omitted when empty, like `nodes` — which is nearly every run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notices: Vec<String>,
    /// The board writes this run's agent nodes performed (issue #661 / M5) — one
    /// row per card opened or re-owned.
    ///
    /// The port row is projected **verbatim** rather than reshaped: it is already
    /// camelCase and already structural (see
    /// [`WorkflowRunBoardRow`](crate::ports::WorkflowRunBoardRow)), so a second
    /// transcription here would only be a place for the journal's shape and the
    /// console's to drift apart. Same choice `deliveries` makes one field up.
    ///
    /// Omitted when empty, like `notices` — which is nearly every run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    board: Vec<crate::ports::WorkflowRunBoardRow>,
    /// The nodes this run blocked on a human (issue #881) — one row per node
    /// whose agent turn had a tool call parked, so it produced no deliverable
    /// and nothing after it ran.
    ///
    /// Projected **verbatim** from the port row, like `board` and `deliveries`
    /// above: it is already camelCase and already structural, and a second
    /// transcription is only a place for the two shapes to drift.
    ///
    /// This is what stops a blocked run reading as a clean one. Its nodes'
    /// rows arrive relabelled too — see the settle arm in [`list_runs`], which
    /// flips each blocked node's journaled `error` status to `blocked`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocked_nodes: Vec<crate::ports::WorkflowBlockedNode>,
    /// The approvals this run parked (issue #880) — a receipt of what it
    /// opened, the failed parks included.
    ///
    /// Named for what the run *parked*, never for what is still outstanding: a
    /// receipt cannot go stale, whereas a settle-time "still waiting on N"
    /// count becomes a fresh lie the moment somebody approves one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    approvals: Vec<crate::ports::WorkflowRunApprovalRow>,
}

/// One node's outcome inside a run (issue #371).
///
/// Structural only — id, status, duration. The node's own output and error text
/// are deliberately absent from the event this is folded from, so they cannot
/// appear here either.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunNode {
    node_id: String,
    status: WorkflowNodeStatus,
    elapsed_ms: u64,
}

impl From<crate::ports::WorkflowRunNodeRow> for WorkflowRunNode {
    /// The run-response path (issue #542): the runner hands its per-node trail
    /// back on [`WorkflowRun::nodes`](crate::ports::WorkflowRun) as
    /// [`WorkflowRunNodeRow`](crate::ports::WorkflowRunNodeRow)s, which carry the
    /// same three structural scalars this wire shape does — so the run response
    /// reuses the identical camelCase rows the history route already serves.
    fn from(row: crate::ports::WorkflowRunNodeRow) -> Self {
        Self {
            node_id: row.node_id,
            status: row.status,
            elapsed_ms: row.elapsed_ms,
        }
    }
}

/// `GET …/workflows/runs?workflow=&limit=` — the company's finished workflow
/// runs, **newest first** (issue #228).
///
/// This is the durable half of the issue: a manual run's delivery rows used to
/// live only in the console drawer until it was dismissed, and a scheduled run's
/// only on host stdout. Folding
/// [`CompanyEvent::WorkflowRunFinished`](crate::ports::types::CompanyEvent) out
/// of the journal makes both survive a console reload, which is the whole point.
///
/// The fold reads the company's whole event log
/// (`read_from(0, MAX)`) — the same thing
/// [`chat_history`](crate::server::chat_history) already does on every history
/// GET. Following that precedent keeps this route from inventing an index the
/// rest of the read plane doesn't have; if the journal scan ever becomes the
/// bottleneck it should be fixed for both surfaces at once, not just here.
async fn list_runs(
    company: ScopedCompany,
    Query(query): Query<RunsQuery>,
) -> Result<Json<Vec<WorkflowRunOutcome>>, ApiError> {
    let limit = match query.limit {
        Some(0) | None => DEFAULT_RUN_LIMIT,
        Some(n) => n.min(MAX_RUN_LIMIT),
    };

    let stored = company
        .runtime
        .events()
        .read_from(company.id(), EventSeq::new(0), usize::MAX)
        .await
        .map_err(ApiError)?;

    // Issue #371 turned this from a filter into a **group-by-run fold**: a run
    // now contributes up to N+2 rows (a start, one per node, a finish) instead
    // of one, and they have to come back as a single history entry.
    //
    // The invariant that keeps it simple: the journal is append-only and
    // single-writer, so a run's rows are ordered `Started < Node… < Finished` —
    // the runner drains and joins its progress collector before returning, which
    // is what makes the last part true rather than a race. Rows of *different*
    // runs may interleave (two workflows can run at once), so the grouping is
    // keyed on run id rather than on adjacency.
    //
    // A pre-#371 finished row has no run id and no start, so it simply folds to
    // itself — one row in, one entry out, exactly as before.
    let mut runs: Vec<WorkflowRunOutcome> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // The `?workflow=` filter is applied per event rather than after the `limit`
    // cut, so asking for one workflow returns that workflow's most recent N —
    // not "whichever of the last N happen to match".
    let wanted = query.workflow.as_deref();
    let matches = |workflow_id: &str| wanted.is_none_or(|w| w == workflow_id);

    for stored in stored {
        let seq = stored.seq.value();
        let at_millis = stored.at_millis;
        match stored.event {
            CompanyEvent::WorkflowRunStarted {
                workflow_id,
                run_id,
                scheduled,
            } => {
                if !matches(&workflow_id) {
                    continue;
                }
                index.insert(run_id.clone(), runs.len());
                runs.push(WorkflowRunOutcome {
                    // The start's own position and time key the entry. The
                    // finish overwrites `at_millis` below so a settled run still
                    // sorts and displays by when it *ended*, as it always has;
                    // `seq` likewise, so ordering is unchanged for settled runs.
                    seq,
                    at_millis,
                    workflow_id,
                    scheduled,
                    run_id: Some(run_id),
                    deliveries: Vec::new(),
                    pending_approvals: Vec::new(),
                    error: None,
                    nodes: Vec::new(),
                    // Issue #1010: filled by the `WorkflowNodeStarted` arm
                    // below, as the engine walks the graph.
                    started_nodes: Vec::new(),
                    started_at_millis: Some(at_millis),
                    // Flipped off by the finish. A start that never gets one is
                    // a run in flight — or one the boot sweep has yet to settle.
                    running: true,
                    // Only a finish can say this, so a run still in flight is
                    // never cancelled from the fold's point of view — even one
                    // whose signal has already been fired, because it has not
                    // wound down yet.
                    cancelled: false,
                    notices: Vec::new(),
                    // Only a finish carries these, so a run in flight lists none —
                    // even one whose nodes have already opened cards. The rows
                    // arrive with the settle below.
                    board: Vec::new(),
                    // Issues #881 / #880: same — only a finish carries these.
                    // A run in flight has blocked on nobody *yet*, and any
                    // approval it has already parked is listed once it settles.
                    blocked_nodes: Vec::new(),
                    approvals: Vec::new(),
                });
            }
            // Issue #1010: the opening bracket, folded at last. The engine has
            // emitted this since #382 and this fold ignored it, so the only
            // per-node fact the history carried was "finished" — and a console
            // that had to read the history to learn about a run (every console
            // that joined mid-run) could not paint the node executing right
            // now, because nothing on the wire said which one it was.
            //
            // Recorded in start order, and deliberately NOT paired against the
            // finishes here: the subtraction belongs to the reader, which is
            // the only side that knows whether it is drawing a live canvas or a
            // settled run's overlay. See `started_nodes`.
            CompanyEvent::WorkflowNodeStarted {
                workflow_id,
                run_id,
                node_id,
            } => {
                if !matches(&workflow_id) {
                    continue;
                }
                // Same rule the finish arm follows one arm down: a node whose
                // run has no entry — a journal truncated below the start, or a
                // `?workflow=` filter that cannot match — is dropped rather
                // than synthesising a headless run.
                if let Some(entry) = index.get(&run_id).and_then(|i| runs.get_mut(*i)) {
                    entry.started_nodes.push(node_id);
                }
            }
            CompanyEvent::WorkflowNodeFinished {
                workflow_id,
                run_id,
                node_id,
                status,
                elapsed_ms,
            } => {
                if !matches(&workflow_id) {
                    continue;
                }
                // A node whose start is missing (a journal truncated below it,
                // or a `?workflow=` filter that cannot match) has no entry to
                // attach to. Dropped rather than synthesising a headless run.
                if let Some(entry) = index.get(&run_id).and_then(|i| runs.get_mut(*i)) {
                    entry.nodes.push(WorkflowRunNode {
                        node_id,
                        status,
                        elapsed_ms,
                    });
                }
            }
            CompanyEvent::WorkflowRunFinished {
                workflow_id,
                scheduled,
                run_id,
                deliveries,
                pending_approvals,
                error,
                cancelled,
                notices,
                board,
                blocked_nodes,
                approvals,
            } => {
                if !matches(&workflow_id) {
                    continue;
                }
                // Settle the open entry when there is one…
                if let Some(entry) = run_id
                    .as_ref()
                    .and_then(|id| index.get(id))
                    .and_then(|i| runs.get_mut(*i))
                {
                    entry.seq = seq;
                    entry.at_millis = at_millis;
                    entry.deliveries = deliveries;
                    entry.pending_approvals = pending_approvals;
                    entry.error = error;
                    entry.running = false;
                    entry.cancelled = cancelled;
                    entry.notices = notices;
                    entry.board = board;
                    // Issue #881: the node rows for this run were folded from
                    // `WorkflowNodeFinished` events the engine wrote, and the
                    // engine reported a blocked node as `error` — honestly, in
                    // its own terms: the capability really did return an error,
                    // which is what halted the branch. The finish is the first
                    // point that knows *why*, so the relabelling happens here,
                    // on the read, rather than by rewriting the durable node
                    // rows. Same host-side reclassification the run record
                    // itself performs; see `workflows::runner`.
                    relabel_blocked(&mut entry.nodes, &blocked_nodes);
                    entry.blocked_nodes = blocked_nodes;
                    entry.approvals = approvals;
                    continue;
                }
                // …else stand alone. Two ways to get here, both legitimate: a
                // pre-#371 row (no run id, no start), and a #371 row whose start
                // fell off the readable journal. Either way the entry looks
                // exactly like the pre-#371 shape — no nodes, no start time, not
                // running — so old history renders unchanged.
                runs.push(WorkflowRunOutcome {
                    seq,
                    at_millis,
                    workflow_id,
                    scheduled,
                    run_id,
                    deliveries,
                    pending_approvals,
                    error,
                    nodes: Vec::new(),
                    // No start row means no node rows either — of either
                    // bracket (issue #1010).
                    started_nodes: Vec::new(),
                    started_at_millis: None,
                    running: false,
                    cancelled,
                    notices,
                    board,
                    // No start row means no node rows either, so there is
                    // nothing here to relabel — the blocked list is still
                    // carried, because it is the only thing that tells this
                    // orphaned row apart from a clean finish.
                    blocked_nodes,
                    approvals,
                });
            }
            _ => {}
        }
    }

    // Issue #1009: cross-check the still-`running` rows against the live run set
    // and settle the ones nobody is running.
    //
    // The fold above marks a start with no finish `running: true`, which is only
    // ever settled by the boot sweep ([`sweep_interrupted_runs`]). Three ways a
    // finish never lands — a task that panicked, an append that failed, a host
    // that died — therefore all read as an eternal spinner *until the next host
    // restart*, with a Stop button that cannot help and a 2s console poll that
    // never stops. This closes the gap between restarts: any run the fold thinks
    // is in flight whose id is **absent** from the supervisor's live set has no
    // task behind it here and now, so it is journaled a synthetic finish (the
    // same `INTERRUPTED_BY_RESTART` the boot sweep uses) and flipped in the
    // in-memory row, so this very response is already self-consistent.
    //
    // Keyed strictly on `live()` membership. A run the current process is
    // genuinely running is registered there and is left untouched — the watchdog
    // (issue #1009, path A) is what guarantees a *panicking* run never reaches
    // this predicate, because it journals its own finish before its guard drops.
    //
    // The one accepted false positive: a run that survived a live
    // `rebuild_company` swap is registered on the *old* supervisor and so is
    // absent from the successor's `live()`, so this could settle a run that is
    // still walking its graph. Accepted because (i) the watchdog keeps panics out
    // of this path entirely, (ii) that run's real finish lands later in journal
    // order and wins the read's last-writer-wins display, and (iii) it is the
    // same class the boot sweep already accepts — which is why that sweep gates
    // on the handover being absent (see the runtime builder call site). It never
    // corrupts the journal: a second, truthful finish simply supersedes it.
    let live_ids: HashSet<String> = company
        .runtime
        .run_supervisor()
        .live()
        .into_iter()
        .map(|(run_id, _workflow_id)| run_id)
        .collect();
    for entry in runs.iter_mut() {
        if !entry.running {
            continue;
        }
        let Some(run_id) = entry.run_id.clone() else {
            continue;
        };
        if live_ids.contains(&run_id) {
            continue;
        }
        // Durable half: append the finish so it survives this response, folds
        // settled on the next `GET …/workflows/runs`, and stops the boot sweep
        // from having to. Best-effort by construction — a failed append leaves
        // the row as the in-memory flip below still makes it, and the next read
        // simply retries.
        crate::runtime::record_run_finished(
            company.runtime.events(),
            company.id(),
            &entry.workflow_id,
            entry.scheduled,
            &run_id,
            Err(crate::runtime::workflow_outcome::INTERRUPTED_BY_RESTART),
        )
        .await;
        // In-memory half: flip the row this response returns, so the console does
        // not have to wait for the next poll to stop the spinner.
        entry.running = false;
        entry.error = Some(crate::runtime::workflow_outcome::INTERRUPTED_BY_RESTART.to_string());
    }

    // Newest first: a history panel leads with the run that just happened. The
    // `limit` now cuts *runs* rather than journal rows, which is the number the
    // caller was asking about all along.
    runs.reverse();
    runs.truncate(limit);
    Ok(Json(runs))
}

/// Relabels a run's node rows for the nodes it blocked on a human (issue #881).
///
/// The read-side half of the host reclassification. `WorkflowNodeFinished` is
/// written live, node by node, long before anything knows the run stopped for an
/// approval rather than a fault — so the durable row says `error` and stays that
/// way. Fixing it up here, against the finish's own blocked list, is what keeps
/// the history panel's node chips agreeing with the run's terminal reading; the
/// alternative is a run that says "blocked" beside a node chip that says
/// "failed".
fn relabel_blocked(nodes: &mut [WorkflowRunNode], blocked: &[crate::ports::WorkflowBlockedNode]) {
    if blocked.is_empty() {
        return;
    }
    for node in nodes.iter_mut() {
        if blocked.iter().any(|b| b.node_id == node.node_id) {
            node.status = WorkflowNodeStatus::Blocked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The globals-unaware readers: these tests assert the company's own two
    // sources, so they call the form that resolves no baseline.
    use crate::company::{list_workflows_union, load_workflow_union};

    /// The listed rows this company itself has, with the global baseline
    /// filtered out. Every company lists the baseline graphs; these tests are
    /// about what this one created, deleted, or declared.
    ///
    /// This is an **id heuristic**, not provenance: `WorkflowSummary` carries
    /// no `global` flag, so a row is classified as "the baseline's" purely by
    /// id membership in `crate::globals::workflows()`. A company definition of
    /// the *same* id supersedes the global one and would be wrongly excluded
    /// here — none of the fixtures below give a company workflow a colliding
    /// id, so the gap does not fire in this suite; see
    /// `write_test::workflow_create_of_an_id_matching_a_global_wins_by_content`
    /// for that case asserted directly, without this helper.
    fn own_rows(listed: &serde_json::Value) -> Vec<&serde_json::Value> {
        listed
            .as_array()
            .expect("array response")
            .iter()
            .filter(|row| {
                let id = row["id"].as_str().unwrap_or_default();
                !crate::globals::workflows().iter().any(|w| w.id == id)
            })
            .collect()
    }

    const DEMO: &str = r#"
        id = "demo"
        name = "Demo flow"
        description = "A tiny trigger → agent → output graph."
        [[node]]
        id = "start"
        kind = "trigger"
        name = "Start"
        summary = "Kicks it off."
        [[node]]
        id = "worker"
        kind = "agent"
        name = "Worker"
        summary = "Does the thing."
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
        label = "ok"
    "#;

    /// Writes `DEMO` to `<dir>/workflows/demo.toml` and returns `dir`.
    fn seed_demo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("demo.toml"), DEMO).unwrap();
        dir
    }

    /// **The `editable` predicate, including the case the route harness can't
    /// reach** (its runtimes are hosted, so they have no source directory).
    ///
    /// A seed file wins the union read, so an overlay body sitting behind one is
    /// not editable even though a body exists — persisting an edit there would
    /// store a graph nothing serves. This is the same predicate the write core
    /// enforces with a 409; if the two ever disagree the console offers a button
    /// that cannot work.
    #[test]
    fn editable_is_overlay_backed_and_not_seed_shadowed() {
        let dir = seed_demo(); // writes workflows/demo.toml
        let source = Some(dir.path());
        let overlay = |id: &str| OverlayWorkflow {
            id: id.to_string(),
            toml: DEMO.to_string(),
        };

        // Overlay body, no seed file → editable.
        assert!(is_editable(source, &[overlay("mine")], "mine"));
        // Overlay body shadowed by a seed file of the same id → NOT editable.
        assert!(!is_editable(source, &[overlay("demo")], "demo"));
        // Seed file only → not editable.
        assert!(!is_editable(source, &[], "demo"));
        // Nothing at all → not editable.
        assert!(!is_editable(source, &[], "ghost"));
        // No source tree (the hosted shape): an overlay body is editable, and a
        // seed id that no longer has a tree behind it simply isn't there.
        assert!(is_editable(None, &[overlay("mine")], "mine"));
        assert!(!is_editable(None, &[], "demo"));
    }

    #[test]
    fn list_returns_a_summary_per_saved_workflow() {
        let dir = seed_demo();
        let files = list_workflows_union(Some(dir.path()), &[]);
        let summaries: Vec<WorkflowSummary> = files
            .into_iter()
            .map(|f| WorkflowSummary::new(f, false, true))
            .collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "demo");
        assert_eq!(summaries[0].name, "Demo flow");
        assert_eq!(
            summaries[0].description.as_deref(),
            Some("A tiny trigger → agent → output graph.")
        );
    }

    #[test]
    fn get_returns_the_full_graph_with_nodes_and_edges() {
        let dir = seed_demo();
        let file = load_workflow_union(Some(dir.path()), &[], "demo")
            .expect("loads")
            .expect("one file");
        let graph = WorkflowGraph::new(file, false, None, true);

        assert_eq!(graph.id, "demo");
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 2);

        // The `kind` field is the on-disk string via `as_str()`.
        let worker = graph.nodes.iter().find(|n| n.id == "worker").unwrap();
        assert_eq!(worker.kind, "agent");
        assert_eq!(worker.agent.as_deref(), Some("assistant"));

        let trigger = graph.nodes.iter().find(|n| n.id == "start").unwrap();
        assert_eq!(trigger.kind, "trigger");
        assert!(trigger.agent.is_none());

        let labeled = graph.edges.iter().find(|e| e.to == "done").unwrap();
        assert_eq!(labeled.from, "worker");
        assert_eq!(labeled.label.as_deref(), Some("ok"));
    }

    #[test]
    fn no_source_dir_and_no_overlay_lists_empty() {
        assert!(list_workflows_union(None, &[]).is_empty());
    }

    #[test]
    fn no_workflows_dir_lists_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_workflows_union(Some(dir.path()), &[]).is_empty());
    }

    #[test]
    fn json_serializes_camelcase_and_omits_empty_options() {
        let dir = seed_demo();
        let file = load_workflow_union(Some(dir.path()), &[], "demo")
            .unwrap()
            .unwrap();
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, true)).unwrap();
        // A node with no summary/agent omits those keys entirely.
        let done = json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "done")
            .unwrap();
        assert!(done.get("agent").is_none());
        assert!(done.get("summary").is_none());
        assert_eq!(done["kind"], "output");
    }

    /// A non-editable graph serializes `version` as an explicit `null` rather
    /// than omitting the key (issue #1013). Omitting it made a client read
    /// `version` as `undefined` and send nothing, silently overwriting a
    /// concurrent save; an explicit `null` is the honest "no token here".
    #[test]
    fn a_non_editable_graph_serializes_version_as_null() {
        let dir = seed_demo();
        let file = load_workflow_union(Some(dir.path()), &[], "demo")
            .unwrap()
            .unwrap();
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, false)).unwrap();
        assert!(
            json.get("version").is_some(),
            "version key must be present, not omitted: {json}"
        );
        assert!(
            json["version"].is_null(),
            "no token serializes as null: {json}"
        );
    }

    #[test]
    fn json_serializes_p1_node_fields_in_camelcase() {
        use crate::company::{WorkflowNodeDef, WorkflowNodeKind, WorkflowRetryDef};

        let file = WorkflowFile {
            global: false,
            id: "wf".into(),
            name: "WF".into(),
            description: None,
            nodes: vec![WorkflowNodeDef {
                id: "call".into(),
                kind: WorkflowNodeKind::ToolCall,
                name: "Call".into(),
                summary: None,
                agent: None,
                schedule: None,
                config: Some(serde_json::json!({ "slug": "csv_export" })),
                on_error: Some("continue".into()),
                retry: Some(WorkflowRetryDef {
                    max_attempts: Some(3),
                    backoff_ms: Some(250),
                    backoff: Some("exponential".into()),
                }),
                requires_approval: Some(true),
                destination: None,
            }],
            edges: Vec::new(),
        };
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, true)).unwrap();
        let node = &json["nodes"][0];
        assert_eq!(node["config"]["slug"], "csv_export");
        assert_eq!(node["onError"], "continue");
        assert_eq!(node["retry"]["maxAttempts"], 3);
        assert_eq!(node["retry"]["backoffMs"], 250);
        assert_eq!(node["retry"]["backoff"], "exponential");
        assert_eq!(node["requiresApproval"], true);
    }

    // --- P2: create body maps the new node fields (config/error/retry/approval)

    /// A create body carrying P2 node fields round-trips them through the
    /// render → parse pipeline the endpoint uses before writing to disk: config
    /// (with an `=expr` binding), `onError`, `retry` (camelCase → snake), and
    /// `requiresApproval` all survive.
    #[test]
    fn create_body_round_trips_p2_node_fields() {
        use crate::company::WorkflowNodeKind;

        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [
                { "id": "start", "kind": "trigger", "name": "Start" },
                {
                    "id": "tf", "kind": "transform", "name": "Transform",
                    "config": { "set": { "count": "=items | length" } },
                    "onError": "continue",
                    "retry": { "maxAttempts": 3, "backoffMs": 250, "backoff": "exponential" },
                    "requiresApproval": true
                }
            ],
            "edges": [ { "from": "start", "to": "tf" } ]
        }))
        .expect("body deserializes");

        let raw = RawWorkflow::try_from(body).expect("converts");
        let toml_src = crate::company::render_workflow(&raw).expect("renders");
        let file = crate::company::parse_workflow(&toml_src).expect("re-parses");

        let tf = file.nodes.iter().find(|n| n.id == "tf").unwrap();
        assert_eq!(tf.kind, WorkflowNodeKind::Transform);
        assert_eq!(tf.on_error.as_deref(), Some("continue"));
        assert_eq!(tf.requires_approval, Some(true));
        let retry = tf.retry.as_ref().expect("retry present");
        assert_eq!(retry.max_attempts, Some(3));
        assert_eq!(retry.backoff_ms, Some(250));
        assert_eq!(retry.backoff.as_deref(), Some("exponential"));
        // The `=expr` binding is preserved verbatim for the engine to evaluate.
        assert_eq!(
            tf.config.as_ref().unwrap()["set"]["count"],
            "=items | length"
        );
    }

    /// An old create body (no P2 fields) still produces a bare node — every new
    /// field unset — so nothing changes for existing callers.
    #[test]
    fn create_body_without_new_fields_is_unchanged() {
        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [ { "id": "start", "kind": "trigger", "name": "Start" } ],
            "edges": []
        }))
        .unwrap();
        let raw = RawWorkflow::try_from(body).expect("converts");
        let node = &raw.nodes[0];
        assert!(node.config.is_none());
        assert!(node.on_error.is_none());
        assert!(node.retry.is_none());
        assert!(node.requires_approval.is_none());
    }

    /// A JSON `null` inside node config is a 400 — TOML has no null to store it,
    /// so it is rejected before anything touches disk.
    #[test]
    fn create_body_with_null_config_is_a_bad_request() {
        use axum::http::StatusCode;

        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [
                { "id": "call", "kind": "tool_call", "name": "Call", "config": { "slug": null } }
            ],
            "edges": []
        }))
        .unwrap();
        // `RawWorkflow` is not `Debug`, so unwrap the error by hand rather than
        // via `expect_err`.
        let err = match RawWorkflow::try_from(body) {
            Ok(_) => panic!("a null config value must be rejected"),
            Err(err) => err,
        };
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(err.0, OpenCompanyError::InvalidRequest(_)));
    }

    // --- trigger schedule (issue #169) --------------------------------------

    /// A create body carrying a trigger `schedule` round-trips it through the
    /// render → parse pipeline the endpoint runs before persisting.
    #[test]
    fn create_body_round_trips_a_trigger_schedule() {
        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [
                { "id": "start", "kind": "trigger", "name": "Start", "schedule": "0 9 * * MON" },
                { "id": "done", "kind": "output", "name": "Done" }
            ],
            "edges": [ { "from": "start", "to": "done" } ]
        }))
        .expect("body deserializes");

        let raw = RawWorkflow::try_from(body).expect("converts");
        let toml_src = crate::company::render_workflow(&raw).expect("renders");
        let file = crate::company::parse_workflow(&toml_src).expect("re-parses");

        let start = file.nodes.iter().find(|n| n.id == "start").unwrap();
        assert_eq!(start.schedule.as_deref(), Some("0 9 * * MON"));
        let done = file.nodes.iter().find(|n| n.id == "done").unwrap();
        assert!(done.schedule.is_none());
    }

    /// The create route needs no schedule-specific validation code: the same
    /// render → parse round trip surfaces a bad cron (and a schedule on the
    /// wrong node kind) as the model's prosumer error, which the handler maps
    /// to a `400`.
    #[test]
    fn create_body_with_a_bad_schedule_fails_reparse() {
        let bad_cron: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [ { "id": "start", "kind": "trigger", "name": "Start", "schedule": "hourly" } ],
            "edges": []
        }))
        .unwrap();
        let raw = RawWorkflow::try_from(bad_cron).expect("converts");
        let toml_src = crate::company::render_workflow(&raw).expect("renders");
        let err = crate::company::parse_workflow(&toml_src).unwrap_err();
        assert!(err.to_string().contains("not a valid cron"), "{err}");

        let wrong_kind: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [
                { "id": "start", "kind": "trigger", "name": "Start" },
                { "id": "done", "kind": "output", "name": "Done", "schedule": "0 * * * *" }
            ],
            "edges": [ { "from": "start", "to": "done" } ]
        }))
        .unwrap();
        let raw = RawWorkflow::try_from(wrong_kind).expect("converts");
        let toml_src = crate::company::render_workflow(&raw).expect("renders");
        let err = crate::company::parse_workflow(&toml_src).unwrap_err();
        assert!(err.to_string().contains("only `trigger` nodes"), "{err}");
    }

    /// `GET …/workflows/{wid}` serializes the schedule in camelCase (it is a
    /// single word, so the key is `schedule`) and omits it when unset.
    #[test]
    fn json_serializes_the_trigger_schedule() {
        use crate::company::{WorkflowNodeDef, WorkflowNodeKind};

        let file = WorkflowFile {
            global: false,
            id: "wf".into(),
            name: "WF".into(),
            description: None,
            nodes: vec![
                WorkflowNodeDef {
                    id: "start".into(),
                    kind: WorkflowNodeKind::Trigger,
                    name: "Start".into(),
                    summary: None,
                    agent: None,
                    schedule: Some("0 * * * *".into()),
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
                WorkflowNodeDef {
                    id: "done".into(),
                    kind: WorkflowNodeKind::Output,
                    name: "Done".into(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
            ],
            edges: Vec::new(),
        };
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, true)).unwrap();
        assert_eq!(json["nodes"][0]["schedule"], "0 * * * *");
        assert!(json["nodes"][1].get("schedule").is_none());
    }

    /// A legacy create body with no `schedule` key still converts, with the
    /// field unset — nothing changes for existing callers.
    #[test]
    fn create_body_without_a_schedule_is_unchanged() {
        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [ { "id": "start", "kind": "trigger", "name": "Start" } ],
            "edges": []
        }))
        .unwrap();
        let raw = RawWorkflow::try_from(body).expect("converts");
        assert!(raw.nodes[0].schedule.is_none());
    }

    // --- Output destination on the wire (issue #170) ------------------------

    /// A create body's `destination` survives the render → parse pipeline the
    /// endpoint runs before persisting, and comes back out on the GET shape
    /// under the same key — so the console can post exactly what it reads.
    #[test]
    fn create_body_round_trips_an_output_destination() {
        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [
                { "id": "start", "kind": "trigger", "name": "Start" },
                {
                    "id": "done", "kind": "output", "name": "Report",
                    "destination": { "kind": "email", "target": "ada@example.com" }
                }
            ],
            "edges": [ { "from": "start", "to": "done" } ]
        }))
        .expect("body deserializes");

        let raw = RawWorkflow::try_from(body).expect("converts");
        let toml_src = crate::company::render_workflow(&raw).expect("renders");
        let file = crate::company::parse_workflow(&toml_src).expect("re-parses");

        let done = file.nodes.iter().find(|n| n.id == "done").unwrap();
        let dest = done.destination.as_ref().expect("destination survived");
        assert_eq!(dest.kind, "email");
        assert_eq!(dest.target.as_deref(), Some("ada@example.com"));

        // …and back out on the read shape, under the same key.
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, true)).unwrap();
        let node = json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "done")
            .unwrap()
            .clone();
        assert_eq!(node["destination"]["kind"], "email");
        assert_eq!(node["destination"]["target"], "ada@example.com");
    }

    /// An `owner` destination carries no target, and the key is omitted rather
    /// than serialized as `null`.
    #[test]
    fn an_owner_destination_omits_the_target_key() {
        let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
            "id": "wf",
            "name": "WF",
            "nodes": [
                { "id": "start", "kind": "trigger", "name": "Start" },
                {
                    "id": "done", "kind": "output", "name": "Report",
                    "destination": { "kind": "owner" }
                }
            ],
            "edges": [ { "from": "start", "to": "done" } ]
        }))
        .unwrap();
        let raw = RawWorkflow::try_from(body).expect("converts");
        let file = crate::company::parse_workflow(
            &crate::company::render_workflow(&raw).expect("renders"),
        )
        .expect("re-parses");
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, true)).unwrap();
        let node = &json["nodes"][1];
        assert_eq!(node["destination"]["kind"], "owner");
        assert!(node["destination"].get("target").is_none());
    }

    /// A node with no destination omits the key entirely — the pre-#170 read
    /// shape is byte-identical for every existing graph.
    #[test]
    fn a_node_without_a_destination_omits_the_key() {
        let dir = seed_demo();
        let file = load_workflow_union(Some(dir.path()), &[], "demo")
            .unwrap()
            .unwrap();
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None, true)).unwrap();
        for node in json["nodes"].as_array().unwrap() {
            assert!(node.get("destination").is_none(), "{node}");
        }
    }

    /// A destination the host cannot honour is rejected before anything is
    /// persisted — the create route surfaces the same prosumer-language problem
    /// a hand-authored file gets.
    #[test]
    fn create_body_with_a_bad_destination_is_rejected_at_validation() {
        for (dest, expected) in [
            (
                serde_json::json!({ "kind": "email", "target": "ada" }),
                "not an email address",
            ),
            (
                serde_json::json!({ "kind": "carrier_pigeon" }),
                "unknown `destination.kind`",
            ),
            (serde_json::json!({ "kind": "channel" }), "no `target`"),
        ] {
            let body: CreateWorkflowBody = serde_json::from_value(serde_json::json!({
                "id": "wf",
                "name": "WF",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    { "id": "done", "kind": "output", "name": "Report", "destination": dest }
                ],
                "edges": [ { "from": "start", "to": "done" } ]
            }))
            .unwrap();
            let raw = RawWorkflow::try_from(body).expect("converts");
            let toml_src = crate::company::render_workflow(&raw).expect("renders");
            let err = crate::company::parse_workflow(&toml_src)
                .expect_err("an unhonourable destination must not persist");
            assert!(err.to_string().contains(expected), "{err}");
        }
    }

    /// Issue #661 (M5): the synchronous run response carries the run's board
    /// rows, in the same camelCase shape the journal event and the history route
    /// use — so a console that pressed Run learns what the run did to the board
    /// without a second read.
    ///
    /// The omission half matters as much: a run that touched no card must send no
    /// `board` key, so every existing caller's body is byte-unchanged.
    #[test]
    fn the_run_response_carries_board_rows_and_omits_them_when_empty() {
        let json = serde_json::to_value(RunWorkflowResponse {
            output: serde_json::json!({ "nodes": {} }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            run_id: "run-1".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: vec![crate::ports::WorkflowRunBoardRow {
                action: crate::ports::WorkflowBoardAction::Assigned,
                task_id: Some("card-1".into()),
                title: None,
                assignee: Some("ceo".into()),
            }],
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
        .expect("serialize");
        assert_eq!(json["board"][0]["action"], "assigned");
        assert_eq!(json["board"][0]["taskId"], "card-1");
        assert_eq!(json["board"][0]["assignee"], "ceo");
        assert!(
            json["board"][0].get("title").is_none(),
            "an assign row names no title — the console resolves it by id: {json}"
        );

        let json = serde_json::to_value(RunWorkflowResponse {
            output: serde_json::json!({ "nodes": {} }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            run_id: "run-2".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
        .expect("serialize");
        assert!(
            json.get("board").is_none(),
            "a run that touched no card sends no board key: {json}"
        );
    }

    /// Issues #881 / #880: the synchronous run response carries the blocked
    /// nodes and the parked-approval receipts, in the same camelCase shape the
    /// journal event and the history route use.
    ///
    /// The operator who pressed Run is the reader these exist for: before this,
    /// a pipeline whose first step had its `publish_artifact` parked came back
    /// with every node green and an empty body, and there was nothing anywhere
    /// in the response that said otherwise.
    ///
    /// Omission matters as much, for the same reason it does on `board`: a run
    /// that blocked on nobody sends neither key, so every existing caller's body
    /// is byte-unchanged.
    #[test]
    fn the_run_response_carries_blocked_nodes_and_parked_approvals() {
        let json = serde_json::to_value(RunWorkflowResponse {
            output: Value::Null,
            pending_approvals: vec!["spec".into()],
            deliveries: Vec::new(),
            run_id: "run-1".into(),
            cancelled: false,
            nodes: vec![WorkflowRunNode {
                node_id: "spec".into(),
                status: WorkflowNodeStatus::Blocked,
                elapsed_ms: 42,
            }],
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                node_id: "spec".into(),
                tools: vec!["publish_artifact".into()],
                approval_ids: vec!["appr-1".into()],
                unparkable: 0,
            }],
            approvals: vec![crate::ports::WorkflowRunApprovalRow {
                node_id: Some("spec".into()),
                tool: Some("publish_artifact".into()),
                outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                approval_id: Some("appr-1".into()),
            }],
        })
        .expect("serialize");
        assert_eq!(json["nodes"][0]["status"], "blocked");
        assert_eq!(json["blockedNodes"][0]["nodeId"], "spec");
        assert_eq!(json["blockedNodes"][0]["tools"][0], "publish_artifact");
        assert_eq!(json["blockedNodes"][0]["approvalIds"][0], "appr-1");
        assert!(
            json["blockedNodes"][0].get("unparkable").is_none(),
            "the ordinary case — every call was parked — stays off the wire: {json}"
        );
        assert_eq!(json["approvals"][0]["outcome"], "parked");
        assert_eq!(json["approvals"][0]["approvalId"], "appr-1");

        let json = serde_json::to_value(RunWorkflowResponse {
            output: serde_json::json!({ "nodes": {} }),
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            run_id: "run-2".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
        .expect("serialize");
        assert!(json.get("blockedNodes").is_none(), "{json}");
        assert!(json.get("approvals").is_none(), "{json}");
    }

    /// A park that could NOT happen is on the wire as loudly as one that did
    /// (issue #880).
    ///
    /// This is the arm whose only previous record was a `tracing::error!` — the
    /// operator will never be asked about the call, so a run that hides it is
    /// telling them the least when it matters most.
    #[test]
    fn a_failed_park_is_reported_rather_than_only_logged() {
        let json = serde_json::to_value(RunWorkflowResponse {
            output: Value::Null,
            pending_approvals: vec!["spec".into()],
            deliveries: Vec::new(),
            run_id: "run-3".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                node_id: "spec".into(),
                tools: vec!["publish_artifact".into()],
                approval_ids: Vec::new(),
                unparkable: 2,
            }],
            approvals: vec![crate::ports::WorkflowRunApprovalRow {
                node_id: Some("spec".into()),
                tool: Some("publish_artifact".into()),
                outcome: crate::ports::WorkflowApprovalOutcome::ParkFailed,
                approval_id: None,
            }],
        })
        .expect("serialize");
        assert_eq!(json["blockedNodes"][0]["unparkable"], 2);
        assert_eq!(json["approvals"][0]["outcome"], "parkFailed");
        assert!(
            json["approvals"][0].get("approvalId").is_none(),
            "there is no card, so naming one would point at nothing: {json}"
        );
    }

    /// The run response carries `deliveries` in camelCase — this is the ONLY
    /// place an operator learns a report was not delivered, since a delivery
    /// failure never fails the run.
    #[test]
    fn run_response_serializes_delivery_rows_in_camelcase() {
        use crate::ports::{DeliveryReport, DeliveryStatus};

        let json = serde_json::to_value(RunWorkflowResponse {
            output: serde_json::json!({ "nodes": {} }),
            pending_approvals: Vec::new(),
            deliveries: vec![DeliveryReport {
                node: "done".into(),
                kind: "email".into(),
                target: Some("ada@example.com".into()),
                status: DeliveryStatus::Skipped,
                detail: "never written in".into(),
                reason: crate::ports::DeliveryReason::RecipientNotEstablished,
            }],
            run_id: "run-1".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
        .unwrap();
        assert_eq!(json["deliveries"][0]["node"], "done");
        assert_eq!(json["deliveries"][0]["status"], "skipped");
        assert_eq!(json["deliveries"][0]["target"], "ada@example.com");
        assert_eq!(json["deliveries"][0]["detail"], "never written in");
        assert!(json["pendingApprovals"].is_array());
    }

    /// Issue #227: a parked report rides the run response as `"pending"`. The
    /// console's `DeliveryStatus` union is spelled in lowercase strings, so the
    /// wire word is the contract — a rename here silently drops the row into
    /// the frontend's fallback tone.
    #[test]
    fn run_response_serializes_a_parked_delivery_as_pending() {
        use crate::ports::{DeliveryReport, DeliveryStatus};

        let json = serde_json::to_value(RunWorkflowResponse {
            output: serde_json::json!({ "nodes": {} }),
            pending_approvals: Vec::new(),
            deliveries: vec![DeliveryReport {
                node: "done".into(),
                kind: "email".into(),
                target: Some("stranger@example.com".into()),
                status: DeliveryStatus::Pending,
                detail: "waiting for you in Approvals".into(),
                reason: crate::ports::DeliveryReason::ParkedForApproval,
            }],
            run_id: "run-1".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
        .unwrap();
        assert_eq!(json["deliveries"][0]["status"], "pending");
    }

    /// A graph that routes nothing serializes an empty list, not a missing key —
    /// the console can render "no deliveries" without a null check.
    #[test]
    fn run_response_with_no_deliveries_is_an_empty_list() {
        let json = serde_json::to_value(RunWorkflowResponse {
            output: Value::Null,
            pending_approvals: Vec::new(),
            deliveries: Vec::new(),
            run_id: "run-1".into(),
            cancelled: false,
            nodes: Vec::new(),
            dry_run: false,
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        })
        .unwrap();
        assert_eq!(json["deliveries"], serde_json::json!([]));
        // Issue #371: the correlation id rides the response in camelCase, so the
        // console can tie the frames it painted mid-request to the run it awaited.
        assert_eq!(json["runId"], "run-1");
    }

    #[test]
    fn safe_wid_rejects_traversal() {
        assert!(safe_wid("demo"));
        assert!(safe_wid("my-workflow_2"));
        assert!(!safe_wid(""));
        assert!(!safe_wid(".."));
        assert!(!safe_wid("."));
        assert!(!safe_wid("../secrets"));
        assert!(!safe_wid("a/b"));
        assert!(!safe_wid("/etc/passwd"));
        assert!(!safe_wid("foo/../bar"));
    }

    #[test]
    fn one_malformed_workflow_does_not_break_the_list() {
        let dir = seed_demo();
        // A second, broken workflow file must not 500 the whole picker.
        std::fs::write(
            dir.path().join("workflows").join("broken.toml"),
            "id = \"broken\"\nname = \n[[node]] oops",
        )
        .unwrap();
        let files = list_workflows_union(Some(dir.path()), &[]);
        let ids: Vec<_> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["demo"]);
    }

    // HTTP-level: a hosted tenant has no source directory to scan, so these
    // exercise the manifest-enabled union path end to end via the router.
    mod hosted_mode {
        use super::own_rows;
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use super::super::{CompanyEvent, DEFAULT_RUN_LIMIT, WorkflowNodeStatus};
        use crate::company::CompanyManifest;
        use crate::ports::CompanyStore;
        use crate::ports::types::{CompanyId, CompanyRecord};
        use crate::runtime::RuntimeBuilder;
        use crate::server::router;
        use crate::store::FsCompanyStore;
        use crate::{AppConfig, AppState};

        fn home() -> tempfile::TempDir {
            tempfile::Builder::new()
                .prefix("oc-workflows-hosted-")
                .tempdir()
                .expect("tempdir")
        }

        /// A manifest declaring one enabled workflow — mirrors what a
        /// platform tenant provisions with, minus any `workflows/` directory
        /// on disk (there isn't one: hosted tenants have no source dir).
        fn manifest_with_enabled() -> CompanyManifest {
            toml::from_str(
                "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[workflows]\nenabled = [\"demo\"]\n",
            )
            .unwrap()
        }

        /// Builds a running company whose runtime has **no source directory**
        /// (built without `with_seed_dir`, matching how the platform builds a
        /// provisioned tenant) but whose persisted record declares an enabled
        /// workflow — the exact hosted-mode gap #70 reports.
        async fn state_with_hosted_company(home: &std::path::Path) -> AppState {
            let store = FsCompanyStore::new(home.to_path_buf());
            let id = CompanyId::new("acme");
            store
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: manifest_with_enabled(),
                    ledger: Vec::new(),
                    lifecycle: "running".to_string(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: Vec::new(),
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                })
                .await
                .unwrap();
            let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest_with_enabled())
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            assert!(
                runtime.source_dir().is_none(),
                "test setup must simulate hosted mode: no source dir"
            );
            let state = AppState::new(AppConfig::default());
            state.registry().insert(id, std::sync::Arc::new(runtime));
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;
            state
        }

        #[tokio::test]
        async fn manifest_enabled_workflow_lists_with_no_source_dir() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let state = state_with_hosted_company(&home).await;

            let response = router(state)
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v1/company/workflows")
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            // Regression for #70: the REST list used to scan the filesystem
            // only, so a hosted tenant with no source dir always got `[]`
            // here even though its manifest declared an enabled workflow.
            let items = own_rows(&body);
            assert_eq!(items.len(), 1, "body: {body}");
            assert_eq!(items[0]["id"], "demo");
            // No file to load a real name from, so the id is the fallback
            // name — same fallback the GraphQL `Company.workflows` resolver
            // uses for the same case.
            assert_eq!(items[0]["name"], "demo");
        }

        /// A hosted tenant's record with no manifest-enabled workflows — the
        /// blank-slate a real tenant starts from before it creates anything.
        fn empty_manifest() -> CompanyManifest {
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
        }

        /// Same as [`state_with_hosted_company`] but with nothing enabled, and
        /// returning the store so a test can rebuild state from it.
        async fn hosted_state(home: &std::path::Path) -> (AppState, FsCompanyStore, CompanyId) {
            let store = FsCompanyStore::new(home.to_path_buf());
            let id = CompanyId::new("acme");
            store
                .save(&CompanyRecord {
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
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                })
                .await
                .unwrap();
            let state = state_over(home, &id, true).await;
            (state, store, id)
        }

        /// Builds an `AppState` whose runtime for `id` has **no source
        /// directory** — the hosted shape — over the store rooted at `home`.
        ///
        /// `seed_admin` seeds the fixed admin + session; a *rebuild* over the
        /// same home must pass `false` (the durable user store already has that
        /// admin, and its session survives with it).
        async fn state_over(home: &std::path::Path, id: &CompanyId, seed_admin: bool) -> AppState {
            let runtime = RuntimeBuilder::new(home.to_path_buf(), empty_manifest())
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            assert!(
                runtime.source_dir().is_none(),
                "test setup must simulate hosted mode: no source dir"
            );
            let state = AppState::new(AppConfig::default());
            state
                .registry()
                .insert(id.clone(), std::sync::Arc::new(runtime));
            if seed_admin {
                crate::server::test_support::seed_fixed_admin(&state, "acme").await;
            }
            state
        }

        /// The graph body the console posts.
        fn create_body() -> serde_json::Value {
            serde_json::json!({
                "id": "greeter",
                "name": "Greeter",
                "description": "Say hi.",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start" },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [ { "from": "start", "to": "done", "label": "ok" } ]
            })
        }

        fn request(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
            let builder = Request::builder()
                .method(method)
                .uri(uri)
                .header("cookie", crate::server::test_support::fixed_cookie("acme"));
            match body {
                Some(json) => builder
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&json).unwrap()))
                    .unwrap(),
                None => builder.body(Body::empty()).unwrap(),
            }
        }

        async fn json_body(response: axum::response::Response) -> serde_json::Value {
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        }

        /// **The #168 regression test.** Creating a workflow on a tenant with no
        /// (writable) source directory used to fail with
        /// `Read-only file system (os error 30)` — the handler wrote the graph
        /// into the crate's read-only company source tree. It now persists on
        /// the record, so the create succeeds, the graph lists under its real
        /// name, and the full body reads back.
        #[tokio::test]
        async fn create_persists_and_reads_back_with_no_source_dir() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            // POST → 200 with the graph echoed back.
            let response = router(state.clone())
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let created = json_body(response).await;
            assert_eq!(created["id"], "greeter");
            assert_eq!(created["nodes"].as_array().unwrap().len(), 2);

            // GET list → the real name, not the id fallback.
            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let listed = json_body(response).await;
            let items = own_rows(&listed);
            assert_eq!(items.len(), 1, "body: {listed}");
            assert_eq!(items[0]["id"], "greeter");
            assert_eq!(items[0]["name"], "Greeter");
            assert_eq!(items[0]["description"], "Say hi.");

            // GET one → the full graph.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let graph = json_body(response).await;
            assert_eq!(graph["id"], "greeter");
            assert_eq!(graph["edges"][0]["label"], "ok");
        }

        // --- Save-time channel-destination guard (issue #981) ---------------

        /// A hosted tenant WITH a desk, so it has one real delivery channel.
        /// `hosted_state`'s manifest declares none, which makes its deliverable
        /// set empty — fine for the nowhere-to-deliver case below, useless for
        /// telling an accepted target from a refused one.
        fn desk_manifest() -> CompanyManifest {
            toml::from_str(
                "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
                 [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
                 [[group_chat]]\nid = \"engineering\"\nname = \"Engineering\"\nmembers = [\"ceo\"]\n",
            )
            .unwrap()
        }

        /// `hosted_state` over [`desk_manifest`] — a running company whose
        /// deliverable set is exactly `["engineering"]`.
        async fn desk_state(home: &std::path::Path) -> AppState {
            let store = FsCompanyStore::new(home.to_path_buf());
            let id = CompanyId::new("acme");
            store
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: desk_manifest(),
                    ledger: Vec::new(),
                    lifecycle: "running".to_string(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: Vec::new(),
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                })
                .await
                .unwrap();
            let runtime = RuntimeBuilder::new(home.to_path_buf(), desk_manifest())
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            assert_eq!(
                runtime.deliverable_channel_ids(),
                vec!["engineering".to_string()],
                "the fixture must have exactly one delivery channel, or these tests prove nothing"
            );
            let state = AppState::new(AppConfig::default());
            state
                .registry()
                .insert(id.clone(), std::sync::Arc::new(runtime));
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;
            state
        }

        /// [`create_body`] with the output node routing its report to `target`
        /// on `kind`.
        fn body_with_destination(kind: &str, target: Option<&str>) -> serde_json::Value {
            let mut destination = serde_json::json!({ "kind": kind });
            if let Some(target) = target {
                destination["target"] = serde_json::Value::String(target.to_string());
            }
            let mut body = create_body();
            body["nodes"][1]["destination"] = destination;
            body
        }

        async fn post_create(state: AppState, body: serde_json::Value) -> axum::response::Response {
            router(state)
                .oneshot(request("POST", "/api/v1/company/workflows", Some(body)))
                .await
                .unwrap()
        }

        /// **The #981 regression.** `operator` was in the picker the console
        /// showed the author, and delivery refuses it by name on every runtime —
        /// so the graph saved, ran green, and dropped its report. It is now
        /// refused at save, naming the channels that would work.
        #[tokio::test]
        async fn a_report_routed_to_operator_is_refused_at_save() {
            let home_dir = home();
            let state = desk_state(home_dir.path()).await;

            let response =
                post_create(state, body_with_destination("channel", Some("operator"))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let message = json_body(response).await.to_string();
            assert!(
                message.contains("is not a workflow delivery channel"),
                "{message}"
            );
            // The live set, so the fix is legible from the refusal alone.
            assert!(message.contains("engineering"), "{message}");
            assert!(
                message.contains("done"),
                "the refusal must name the node: {message}"
            );
        }

        /// A channel nobody wired is refused the same way. The author's typo and
        /// the author's `operator` are the same mistake — a destination this
        /// company cannot deliver to — and get one answer.
        #[tokio::test]
        async fn a_report_routed_to_an_unwired_channel_is_refused_at_save() {
            let home_dir = home();
            let state = desk_state(home_dir.path()).await;

            let response =
                post_create(state, body_with_destination("channel", Some("enginering"))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let message = json_body(response).await.to_string();
            assert!(
                message.contains("is not a workflow delivery channel"),
                "{message}"
            );
            assert!(message.contains("engineering"), "{message}");
        }

        /// The guard refuses what delivery would refuse and nothing more: a real
        /// desk saves, and reads back with its destination intact.
        #[tokio::test]
        async fn a_report_routed_to_a_real_desk_saves() {
            let home_dir = home();
            let state = desk_state(home_dir.path()).await;

            let response = post_create(
                state.clone(),
                body_with_destination("channel", Some("engineering")),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            let graph = json_body(response).await;
            assert_eq!(graph["nodes"][1]["destination"]["kind"], "channel");
            assert_eq!(graph["nodes"][1]["destination"]["target"], "engineering");
        }

        /// An edit is a save too. The create route was never the only way in —
        /// `PUT` replaces the graph wholesale, so a destination refused on
        /// create must not be reachable by saving a clean graph and then
        /// editing it.
        #[tokio::test]
        async fn an_edit_cannot_introduce_an_undeliverable_destination() {
            let home_dir = home();
            let state = desk_state(home_dir.path()).await;

            let created = post_create(state.clone(), create_body()).await;
            assert_eq!(created.status(), StatusCode::OK);
            // Carry the created graph's token (required since #1013) so the 400
            // comes from the destination guard, not the missing-token guard.
            let version = json_body(created).await["version"]
                .as_str()
                .expect("create returns a version")
                .to_string();

            let mut body = body_with_destination("channel", Some("operator"));
            body["expectedVersion"] = serde_json::json!(version);
            let response = router(state)
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let message = json_body(response).await.to_string();
            assert!(
                message.contains("is not a workflow delivery channel"),
                "{message}"
            );
        }

        /// A company with no desks and no provider channels has nowhere to
        /// deliver (#963), and says so in its own words rather than trailing off
        /// after `has: `. The destinations that do NOT depend on a channel still
        /// save — the guard is about channels, and a company with no desks can
        /// still mail its owner.
        #[tokio::test]
        async fn a_company_with_no_delivery_channel_says_so_and_still_saves_an_owner_report() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let response = post_create(
                state.clone(),
                body_with_destination("channel", Some("engineering")),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let message = json_body(response).await.to_string();
            assert!(message.contains("no durable channels"), "{message}");

            // …and the picker it was offered is empty, not `["operator"]`.
            let response = router(state.clone())
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/wired-channels",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(json_body(response).await["channels"], serde_json::json!([]));

            let response = post_create(state, body_with_destination("owner", None)).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "an `owner` report needs no channel"
            );
        }

        /// The guard stays out of everything that is not a channel destination
        /// on an `output` node: a graph that routes nowhere saves, and a
        /// `destination` on a non-`output` node is still `parse_workflow`'s
        /// refusal to report — reporting the wrong problem first would send an
        /// author looking for a channel that was never their mistake (#947).
        #[tokio::test]
        async fn the_guard_leaves_non_channel_graphs_alone() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            // No destination at all: unchanged, saves.
            assert_eq!(
                post_create(state.clone(), create_body()).await.status(),
                StatusCode::OK
            );

            // A channel destination on the TRIGGER node, on a company with no
            // delivery channels at all: still rejected for being on the wrong
            // kind of node, not for the channel.
            let mut body = create_body();
            body["id"] = serde_json::Value::String("second".into());
            body["name"] = serde_json::Value::String("Second".into());
            body["nodes"][0]["destination"] =
                serde_json::json!({ "kind": "channel", "target": "engineering" });
            let response = post_create(state, body).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let message = json_body(response).await.to_string();
            assert!(
                message.contains("only `output` nodes route a report"),
                "the structural problem must be the one reported: {message}"
            );
        }

        /// A duplicate id is a clean 409, not a 500 — the id-uniqueness check
        /// that replaced the filesystem's `create_new(true)`.
        #[tokio::test]
        async fn duplicate_create_is_a_conflict() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let first = router(state.clone())
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::OK);

            let second = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(second.status(), StatusCode::CONFLICT);
        }

        /// Issue #753: an empty description is a `400` on **both** scope forms —
        /// which also proves the route is wired under each (a route-miss would be
        /// a `404`, not the `400` the handler returns before it ever looks for a
        /// builder). The empty check runs ahead of the capability gate, so this
        /// holds on every build.
        #[tokio::test]
        async fn draft_from_description_rejects_empty_on_both_scope_forms() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            for uri in [
                "/api/v1/company/workflows/draft-from-description",
                "/api/v1/companies/acme/workflows/draft-from-description",
            ] {
                let response = router(state.clone())
                    .oneshot(request(
                        "POST",
                        uri,
                        Some(serde_json::json!({ "description": "   " })),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "empty description must 400 on {uri}"
                );
            }
        }

        /// Issue #753: with a real description but no builder wired on the running
        /// runtime, the copilot classifies the gap exactly as the run route does —
        /// a `not_wired` 404 or a `restart_required` / `inference_required` 409,
        /// each carrying its `code` — rather than a bare failure. The hosted test
        /// runtime wires no harness, so this is the gap path.
        #[tokio::test]
        async fn draft_from_description_reports_a_builder_gap() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let response = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows/draft-from-description",
                    Some(serde_json::json!({
                        "description": "email the weekly digest every Monday"
                    })),
                ))
                .await
                .unwrap();
            let status = response.status();
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::CONFLICT,
                "a builder gap is a 404/409, got {status}"
            );
            let body = json_body(response).await;
            let code = body["code"].as_str().unwrap_or_default();
            assert!(
                matches!(
                    code,
                    "not_wired" | "restart_required" | "inference_required"
                ),
                "gap response carries a known code, got: {body}"
            );
        }

        /// Issue #840 (PR-3): with a real body but no builder wired, the
        /// fix-from-run route classifies the gap exactly as the draft + run routes
        /// do — a `not_wired` 404 or a `restart_required` / `inference_required`
        /// 409 — on **both** scope forms. Also proves the sub-resource route is
        /// wired (a route-miss would be a bare 404 with no `code`). The hosted test
        /// runtime wires no harness, so this is the gap path.
        #[tokio::test]
        async fn fix_from_run_reports_a_builder_gap_on_both_scope_forms() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            for uri in [
                "/api/v1/company/workflows/weekly-digest/fix-from-run",
                "/api/v1/companies/acme/workflows/weekly-digest/fix-from-run",
            ] {
                let response = router(state.clone())
                    .oneshot(request(
                        "POST",
                        uri,
                        Some(serde_json::json!({
                            "runId": "run-1",
                            "errorHint": "it failed at the search node"
                        })),
                    ))
                    .await
                    .unwrap();
                let status = response.status();
                assert!(
                    status == StatusCode::NOT_FOUND || status == StatusCode::CONFLICT,
                    "a builder gap is a 404/409 on {uri}, got {status}"
                );
                let body = json_body(response).await;
                let code = body["code"].as_str().unwrap_or_default();
                assert!(
                    matches!(
                        code,
                        "not_wired" | "restart_required" | "inference_required"
                    ),
                    "gap response carries a known code on {uri}, got: {body}"
                );
            }
        }

        /// Issue #840 (PR-3): the fix route's error-resolution matrix — a journaled
        /// error wins (carrying its failing node), a clean/absent run falls back to
        /// the caller's hint, and a clean run with no usable hint is nothing to fix
        /// from (a 400). Unit-tested on the pure helper so the whole matrix is
        /// pinned without a running host.
        #[cfg(feature = "openhuman")]
        #[test]
        fn fix_error_resolution_prefers_journal_then_hint_then_nothing() {
            use super::super::{JournaledFailure, resolve_fix_error};
            // A journaled error wins, carrying the failing node id.
            assert_eq!(
                resolve_fix_error(
                    Some(JournaledFailure {
                        error: Some("boom".to_string()),
                        failed_node_id: Some("n1".to_string()),
                    }),
                    Some("hint".to_string()),
                ),
                Some(("boom".to_string(), Some("n1".to_string())))
            );
            // A run that finished CLEAN (no error) falls back to the hint.
            assert_eq!(
                resolve_fix_error(
                    Some(JournaledFailure {
                        error: None,
                        failed_node_id: None,
                    }),
                    Some("hint".to_string()),
                ),
                Some(("hint".to_string(), None))
            );
            // No finish for this run id at all → the hint is the only source.
            assert_eq!(
                resolve_fix_error(None, Some("hint".to_string())),
                Some(("hint".to_string(), None))
            );
            // A clean run and no hint → nothing to fix from.
            assert_eq!(
                resolve_fix_error(
                    Some(JournaledFailure {
                        error: None,
                        failed_node_id: None,
                    }),
                    None
                ),
                None
            );
            // No run and no hint → nothing to fix from.
            assert_eq!(resolve_fix_error(None, None), None);
            // A whitespace-only hint is not usable.
            assert_eq!(resolve_fix_error(None, Some("   ".to_string())), None);
        }

        /// Journals a `WorkflowRunFinished` naming a `run_id`, the shape
        /// `journaled_run_failure` scans for — distinct from `journal_run` above,
        /// which always journals `run_id: None` for the delivery-history tests.
        #[cfg(feature = "openhuman")]
        async fn journal_run_with_id(
            state: &AppState,
            id: &CompanyId,
            workflow_id: &str,
            run_id: &str,
            error: &str,
        ) {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .append(
                    id,
                    CompanyEvent::WorkflowRunFinished {
                        workflow_id: workflow_id.to_string(),
                        scheduled: false,
                        run_id: Some(run_id.to_string()),
                        deliveries: Vec::new(),
                        pending_approvals: Vec::new(),
                        error: Some(error.to_string()),
                        cancelled: false,
                        notices: Vec::new(),
                        board: Vec::new(),
                        // Added by #881/#880 after this fixture was written. A
                        // failed run parks nothing and blocks nothing, so both
                        // are empty here — see `Settled::from`'s Err arm, which
                        // makes the same choice for the same reason.
                        blocked_nodes: Vec::new(),
                        approvals: Vec::new(),
                    },
                )
                .await
                .expect("append");
        }

        /// Issue #840 (PR-3), tinysweeper finding: the only prior server test for
        /// `fix_from_run` exercised the builder-gap path (404/409, above). This is
        /// the core of the feature — a valid request with the `openhuman` feature
        /// on, asserting a 200 with `automatable: true`, the corrected workflow,
        /// and its readiness — proven at the HTTP boundary rather than only at the
        /// `fix_workflow_from_failure` unit layer (`workflow_build::test` already
        /// covers identity-preservation there).
        ///
        /// Reuses `workflow_build::test`'s scripted-model + `HarnessDeps` fixture
        /// (widened to `pub(crate)` for this) rather than hand-rolling a second
        /// `HarnessModel`/`HarnessDeps` here — that struct has ~30 fields and
        /// duplicating it would drift silently the next time one is added.
        ///
        /// A copilot turn nests the provider/tool loop deep enough to overflow
        /// tokio's default 2 MiB worker-thread stack — the same exposure
        /// `openhuman_core::core::runtime::AGENT_WORKER_STACK_BYTES`'s doc comment
        /// names for production hosts. Every other agent-turn test in this module
        /// (and `workflow_build::test`) is plain `#[tokio::test]` and relies on
        /// CI setting `RUST_MIN_STACK=16777216` for this job
        /// (`.github/workflows/ci.yml`'s `Rust (openhuman, tinycortex)` lane) —
        /// this one follows the same convention rather than wrapping itself in a
        /// custom-stack thread, which no sibling test does. Run locally with
        /// `RUST_MIN_STACK=16777216 cargo test …` if it overflows outside CI.
        #[cfg(feature = "openhuman")]
        #[tokio::test]
        async fn fix_from_run_returns_the_corrected_graph_on_success() {
            use crate::harness::provider::HarnessModel;
            use crate::harness::workflow_build::WorkflowBuilder;
            use crate::harness::workflow_build::test::{
                NativeCopilotModel, NativeStep, agent_deps, propose_step,
            };

            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            // Wire a builder AND the harness deps `run_copilot` builds its agent
            // from — the route's own capability gate only checks the former, but
            // the copilot needs both (issue #840, PR-2's `HarnessDeps` wiring).
            let model = NativeCopilotModel::scripting(vec![
                propose_step(
                    "dropped the unwired step",
                    serde_json::json!({
                        "name": "Greeter",
                        "nodes": [
                            { "id": "start", "kind": "trigger", "name": "Start" },
                            { "id": "done", "kind": "output", "name": "Report" }
                        ],
                        "edges": [ { "from": "start", "to": "done" } ]
                    }),
                ),
                NativeStep::done("Corrected the workflow."),
            ]);
            {
                let mut runtime =
                    std::sync::Arc::into_inner(state.registry().remove(&id).expect("registered"))
                        .expect("uniquely held in this test");
                let deps = agent_deps(&runtime, model.clone() as std::sync::Arc<dyn HarnessModel>);
                runtime.set_builder(std::sync::Arc::new(WorkflowBuilder::new(
                    model as std::sync::Arc<dyn HarnessModel>,
                    "test-model",
                )));
                runtime.set_workflow_harness_deps(deps);
                state
                    .registry()
                    .insert(id.clone(), std::sync::Arc::new(runtime));
            }

            // Seed the workflow the run failed on (hosted mode has no source
            // dir, so it exists only as an overlay created via the API).
            let created = router(state.clone())
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(created.status(), StatusCode::OK);

            journal_run_with_id(
                &state,
                &id,
                "greeter",
                "run-1",
                "the tool `web_search` is not wired on this deployment",
            )
            .await;

            let response = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows/greeter/fix-from-run",
                    Some(serde_json::json!({ "runId": "run-1" })),
                ))
                .await
                .unwrap();
            let status = response.status();
            let body = json_body(response).await;
            assert_eq!(status, StatusCode::OK, "body: {body}");
            assert_eq!(body["automatable"], true, "body: {body}");
            assert_eq!(
                body["workflow"]["id"], "greeter",
                "the fix keeps the workflow's id"
            );
            assert!(
                body["workflow"]["nodes"]
                    .as_array()
                    .is_some_and(|n| !n.is_empty()),
                "body: {body}"
            );
            assert!(body["readiness"]["ok"].is_boolean(), "body: {body}");
        }

        /// Issue #783: the per-workflow copilot's tool-grounding read answers
        /// `200 {"slugs":[…],"unwired":[…]}` on **both** scope forms — which also
        /// proves the static prefix is wired ahead of the dynamic
        /// `/workflows/{wid}` (a route-miss, or a `tool-slugs` swallowed as a
        /// `wid`, would not be this shape). The blank tenant grants no tools, so
        /// both lists are empty here; the point pinned is the contract shape and
        /// that the route exists.
        ///
        /// Issue #874 added `unwired` and it is pinned here as **always present**,
        /// because the console reads it unconditionally: a body that omitted the
        /// key on a wired host would read as "nothing is unwired" and silently
        /// restore the bug this route was narrowed to fix.
        #[tokio::test]
        async fn tool_slugs_answers_a_slug_array_on_both_scope_forms() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            for uri in [
                "/api/v1/company/workflows/tool-slugs",
                "/api/v1/companies/acme/workflows/tool-slugs",
            ] {
                let response = router(state.clone())
                    .oneshot(request("GET", uri, None))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "tool-slugs on {uri}");
                let body = json_body(response).await;
                assert!(
                    body["slugs"].is_array(),
                    "tool-slugs answers a `slugs` array on {uri}, got: {body}"
                );
                assert!(
                    body["unwired"].is_array(),
                    "tool-slugs answers an `unwired` array on {uri}, got: {body}"
                );
            }
        }

        /// Issue #874, the staging repro through the route itself: a company that
        /// explicitly grants `search`, on a deployment with no managed search
        /// backend, must NOT be handed `web_search` to ground a proposal on.
        ///
        /// This is the regression that shipped. The route answered the
        /// **grant-only** set, so `web_search` was advertised, the copilot
        /// authored a `tool_call` on it, and the run died at the first node with
        /// `tool_call 'web_search' is not available in company workflows`. What
        /// pins the fix is the pair of assertions: the slug is gone from `slugs`
        /// **and** present in `unwired` with a reason — dropping it silently would
        /// leave an operator unable to tell "not allowed" from "not configured".
        ///
        /// A granted-and-wired tool (`shell`) stays offered in the same answer, so
        /// this cannot pass by narrowing the list to nothing.
        #[cfg(feature = "openhuman")]
        #[tokio::test]
        async fn tool_slugs_omits_a_granted_but_unwired_tool_and_says_why() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let id = CompanyId::new("acme");
            let mut manifest = empty_manifest();
            manifest.tools.allow = vec!["search".to_string(), "shell".to_string()];

            let store = FsCompanyStore::new(home.clone());
            store
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: manifest.clone(),
                    ledger: Vec::new(),
                    lifecycle: "running".to_string(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: Vec::new(),
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                })
                .await
                .unwrap();

            let mut runtime = RuntimeBuilder::new(home.clone(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            // `workflow_wiring_deps` pins `search: None` — the deployment half of
            // the repro. Everything else is allowed, so `shell` stays wired.
            runtime.set_workflow_harness_deps(crate::harness::workflow_wiring_deps(
                &runtime,
                None,
                crate::harness::toolbelt::CapabilityFilter::AllowAll,
                None,
            ));
            let state = AppState::new(AppConfig::default());
            state.registry().insert(id, std::sync::Arc::new(runtime));
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/tool-slugs", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;

            let slugs: Vec<&str> = body["slugs"]
                .as_array()
                .expect("slugs")
                .iter()
                .map(|v| v.as_str().expect("slug"))
                .collect();
            assert!(
                !slugs.contains(&"web_search"),
                "a granted-but-unwired tool is not offered for grounding: {body}"
            );
            assert!(
                slugs.contains(&"shell"),
                "a granted AND wired tool is still offered: {body}"
            );

            let unwired = body["unwired"].as_array().expect("unwired");
            let entry = unwired
                .iter()
                .find(|e| e["slug"] == "web_search")
                .unwrap_or_else(|| panic!("web_search is reported as unwired: {body}"));
            assert_eq!(
                entry["reason"], "searchBackendNotConfigured",
                "the reason distinguishes an unconfigured provider from a filtered \
                 capability tier: {body}"
            );
            assert!(
                entry["detail"]
                    .as_str()
                    .is_some_and(|d| d.contains("search backend")),
                "the prose reason is servable as-is: {body}"
            );
        }

        /// A create body whose trigger carries a cron.
        fn scheduled_create_body() -> serde_json::Value {
            serde_json::json!({
                "id": "digest",
                "name": "Digest",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start", "schedule": "0 9 * * *" },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [ { "from": "start", "to": "done", "label": "ok" } ]
            })
        }

        /// `PUT …/workflows/{wid}/enabled` round-trips through the API and shows
        /// up on the list read (issue #276).
        #[tokio::test]
        async fn the_enabled_route_toggles_and_the_list_reports_it() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let created = router(state.clone())
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(created.status(), StatusCode::OK);
            // A manual workflow is armed — there is nothing to disarm.
            assert_eq!(json_body(created).await["enabled"], serde_json::json!(true));

            let paused = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter/enabled",
                    Some(serde_json::json!({ "enabled": false })),
                ))
                .await
                .unwrap();
            assert_eq!(paused.status(), StatusCode::OK);
            assert_eq!(json_body(paused).await["enabled"], serde_json::json!(false));

            // And the picker sees it, which is what the console renders from.
            let list = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let body = json_body(list).await;
            let row = body
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["id"] == "greeter")
                .expect("listed");
            assert_eq!(row["enabled"], serde_json::json!(false));
            assert_eq!(
                row["editable"],
                serde_json::json!(true),
                "pausing must not change whether the graph can be edited"
            );

            // Back on again.
            let armed = router(state)
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter/enabled",
                    Some(serde_json::json!({ "enabled": true })),
                ))
                .await
                .unwrap();
            assert_eq!(armed.status(), StatusCode::OK);
            assert_eq!(json_body(armed).await["enabled"], serde_json::json!(true));
        }

        /// **Issue #276's safety half, over the wire.** Creating a workflow with
        /// a schedule answers `enabled: false` on its own response, so a console
        /// learns about the disarm from the write it made rather than from a
        /// refresh it might not do.
        #[tokio::test]
        async fn creating_a_scheduled_workflow_answers_switched_off() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let created = router(state.clone())
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(scheduled_create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(created.status(), StatusCode::OK);
            assert_eq!(
                json_body(created).await["enabled"],
                serde_json::json!(false)
            );

            // And the graph read agrees, so it is the store's answer rather than
            // something the write path made up on the way out.
            let read = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/digest", None))
                .await
                .unwrap();
            assert_eq!(json_body(read).await["enabled"], serde_json::json!(false));
        }

        /// An unknown id is a 404 rather than a silently-created disable entry —
        /// a switch that accepted any string would let a typo look like a
        /// successful pause.
        #[tokio::test]
        async fn toggling_an_unknown_workflow_is_not_found() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let response = router(state)
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/nowhere/enabled",
                    Some(serde_json::json!({ "enabled": false })),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// A **global-only** workflow — no seed file, no overlay body, just the
        /// baseline every company gets — must still be toggleable: it has a
        /// schedule to pause exactly like a company-authored one, and
        /// `disabled_workflows` (what this route writes) is a separate
        /// mechanism from `[globals].disable` (what drops the global outright).
        /// Before this, `set_company_workflow_enabled`'s "does this company
        /// have a body for `wid`" check only looked at seed files and overlays,
        /// so a global-only id read as a bodiless manifest-`enabled` id and the
        /// route answered 409 for a graph that plainly exists and runs.
        #[tokio::test]
        async fn the_enabled_route_toggles_a_global_only_workflow() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            let global_id = crate::globals::workflows()[0].id.clone();

            let paused = router(state.clone())
                .oneshot(request(
                    "PUT",
                    &format!("/api/v1/company/workflows/{global_id}/enabled"),
                    Some(serde_json::json!({ "enabled": false })),
                ))
                .await
                .unwrap();
            assert_eq!(paused.status(), StatusCode::OK);
            assert_eq!(json_body(paused).await["enabled"], serde_json::json!(false));

            let list = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let body = json_body(list).await;
            let row = body
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["id"] == global_id.as_str())
                .expect("still listed");
            assert_eq!(row["enabled"], serde_json::json!(false));

            // Back on, and still resolvable through `GET …/workflows/{wid}`
            // throughout — pausing a global must not make it unreadable.
            let armed = router(state.clone())
                .oneshot(request(
                    "PUT",
                    &format!("/api/v1/company/workflows/{global_id}/enabled"),
                    Some(serde_json::json!({ "enabled": true })),
                ))
                .await
                .unwrap();
            assert_eq!(armed.status(), StatusCode::OK);
            assert_eq!(json_body(armed).await["enabled"], serde_json::json!(true));

            let read = router(state)
                .oneshot(request(
                    "GET",
                    &format!("/api/v1/company/workflows/{global_id}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(read.status(), StatusCode::OK);
        }

        /// A workflow this company has explicitly dropped via
        /// `[globals].disable` no longer exists as far as this company is
        /// concerned, so toggling it is the same 404 an unknown id gets — the
        /// global-only arm above must not treat a disabled global as having a
        /// body.
        #[tokio::test]
        async fn toggling_a_company_disabled_global_is_not_found() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let store = FsCompanyStore::new(home.to_path_buf());
            let id = CompanyId::new("acme");
            let global_id = crate::globals::workflows()[0].id.clone();
            let manifest: CompanyManifest = toml::from_str(&format!(
                "[company]\nname = \"Acme\"\n\n[globals]\ndisable = [\"workflow:{global_id}\"]\n"
            ))
            .unwrap();
            store
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: manifest.clone(),
                    ledger: Vec::new(),
                    lifecycle: "running".to_string(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: Vec::new(),
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                })
                .await
                .unwrap();
            // Not `state_over`: it always builds with `empty_manifest()`, which
            // would overwrite this test's `[globals].disable` and silently pass
            // for the wrong reason.
            let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            let state = AppState::new(AppConfig::default());
            state.registry().insert(id, std::sync::Arc::new(runtime));
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;

            let response = router(state)
                .oneshot(request(
                    "PUT",
                    &format!("/api/v1/company/workflows/{global_id}/enabled"),
                    Some(serde_json::json!({ "enabled": false })),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// Restart survival: a workflow created through the API is still listed
        /// by a completely fresh `AppState` rebuilt over the same store — proving
        /// the body is durable, not process-local.
        #[tokio::test]
        async fn a_created_workflow_survives_a_state_rebuild() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            let response = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            // Rebuild everything from the same durable store.
            let rebuilt = state_over(&home, &id, false).await;
            let response = router(rebuilt)
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let listed = json_body(response).await;
            let items = own_rows(&listed);
            assert_eq!(items.len(), 1, "body: {listed}");
            assert_eq!(items[0]["id"], "greeter");
            assert_eq!(items[0]["name"], "Greeter");
        }

        // ── Issue #228: the run-history read ────────────────────────────────

        /// Journals a finished-run outcome directly on the company's event log,
        /// the way both entry points do via `record_run_finished`.
        async fn journal_run(
            state: &AppState,
            id: &CompanyId,
            workflow_id: &str,
            scheduled: bool,
            deliveries: Vec<crate::ports::DeliveryReport>,
            error: Option<&str>,
        ) {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .append(
                    id,
                    CompanyEvent::WorkflowRunFinished {
                        workflow_id: workflow_id.to_string(),
                        scheduled,
                        run_id: None,
                        deliveries,
                        pending_approvals: Vec::new(),
                        error: error.map(str::to_string),
                        cancelled: false,
                        notices: Vec::new(),
                        board: Vec::new(),
                        blocked_nodes: Vec::new(),
                        approvals: Vec::new(),
                    },
                )
                .await
                .expect("append");
        }

        fn undelivered_row(node: &str) -> crate::ports::DeliveryReport {
            crate::ports::DeliveryReport {
                node: node.to_string(),
                kind: "email".to_string(),
                target: Some("ada@example.com".to_string()),
                status: crate::ports::DeliveryStatus::Skipped,
                detail: "this recipient has never written to the company".to_string(),
                reason: crate::ports::DeliveryReason::RecipientNotEstablished,
            }
        }

        /// **The issue, at the HTTP boundary.** A run's delivery rows read back
        /// after the fact, newest first — which is what survives a console
        /// reload, and the reason #228 exists.
        #[tokio::test]
        async fn run_history_reads_back_newest_first_with_its_rows() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_run(
                &state,
                &id,
                "digest",
                true,
                vec![undelivered_row("owner")],
                None,
            )
            .await;
            journal_run(&state, &id, "greeter", false, Vec::new(), None).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 2, "body: {body}");

            // Newest first: the history panel leads with the run that just ran.
            assert_eq!(rows[0]["workflowId"], "greeter");
            assert_eq!(rows[0]["scheduled"], false);
            assert_eq!(rows[1]["workflowId"], "digest");
            assert_eq!(rows[1]["scheduled"], true);

            // The delivery row — including the `detail` that names the fix, and
            // the `target`, which the manual-run response already ships to this
            // same console.
            let deliveries = rows[1]["deliveries"].as_array().expect("deliveries");
            assert_eq!(deliveries.len(), 1);
            assert_eq!(deliveries[0]["node"], "owner");
            assert_eq!(deliveries[0]["status"], "skipped");
            assert_eq!(deliveries[0]["target"], "ada@example.com");
            assert!(
                deliveries[0]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("never written")
            );
            // A run that finished carries no `error` key at all.
            assert!(rows[0].get("error").is_none(), "{body}");
        }

        /// Issue #596: the run-output route serves a stored snapshot (200) and
        /// 404s a run with none. Runs in the DEFAULT lane, which also proves the
        /// route + store are present with the openhuman-gated *writer* compiled
        /// out — the default build reads back exactly what was written and 404s
        /// otherwise.
        #[tokio::test]
        async fn run_output_route_serves_a_snapshot_and_404s_an_unknown_run() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            let runtime = state.registry().get(&id).expect("registered");
            let record = crate::ports::WorkflowRunOutputRecord {
                run_id: "run-xyz".to_string(),
                workflow_id: "greeter".to_string(),
                at_millis: 123,
                nodes: serde_json::json!({
                    "writer": { "items": [{ "json": { "text": "the draft" } }] }
                }),
                truncated: false,
                partial: false,
            };
            runtime
                .workflow_run_outputs()
                .put_run_output(&id, &record)
                .await
                .expect("store write");

            // 200 with the record for a stored run.
            let response = router(state.clone())
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/runs/run-xyz/output",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert_eq!(body["runId"], "run-xyz", "{body}");
            assert_eq!(body["workflowId"], "greeter");
            assert_eq!(body["truncated"], false);
            assert_eq!(
                body["nodes"]["writer"]["items"][0]["json"]["text"], "the draft",
                "the durable per-node output must round-trip through the route: {body}"
            );

            // 404 for a run with no captured output.
            let missing = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/runs/nope/output",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        }

        /// A run that died outright reads back with its reason. This is the
        /// outcome that previously left nothing behind but a host-stdout warning.
        #[tokio::test]
        async fn run_history_carries_a_failed_run_error() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_run(
                &state,
                &id,
                "digest",
                true,
                Vec::new(),
                Some("no inference source for agent node `worker`"),
            )
            .await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            assert_eq!(
                body[0]["error"],
                "no inference source for agent node `worker`"
            );
            assert_eq!(body[0]["deliveries"].as_array().unwrap().len(), 0);
        }

        // ── Issue #371: the per-node progress fold ─────────────────────────

        /// Journals a `WorkflowRunStarted`, the way the runner does before the
        /// engine call.
        async fn journal_start(
            state: &AppState,
            id: &CompanyId,
            workflow_id: &str,
            run_id: &str,
            scheduled: bool,
        ) {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .append(
                    id,
                    CompanyEvent::WorkflowRunStarted {
                        workflow_id: workflow_id.to_string(),
                        run_id: run_id.to_string(),
                        scheduled,
                    },
                )
                .await
                .expect("append");
        }

        /// Journals one `WorkflowNodeFinished`, the way the run observer does.
        async fn journal_node(
            state: &AppState,
            id: &CompanyId,
            workflow_id: &str,
            run_id: &str,
            node_id: &str,
            status: WorkflowNodeStatus,
        ) {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .append(
                    id,
                    CompanyEvent::WorkflowNodeFinished {
                        workflow_id: workflow_id.to_string(),
                        run_id: run_id.to_string(),
                        node_id: node_id.to_string(),
                        status,
                        elapsed_ms: 42,
                    },
                )
                .await
                .expect("append");
        }

        /// Journals one `WorkflowNodeStarted`, the way the run observer does
        /// immediately before a node's first attempt (issue #382).
        async fn journal_node_started(
            state: &AppState,
            id: &CompanyId,
            workflow_id: &str,
            run_id: &str,
            node_id: &str,
        ) {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .append(
                    id,
                    CompanyEvent::WorkflowNodeStarted {
                        workflow_id: workflow_id.to_string(),
                        run_id: run_id.to_string(),
                        node_id: node_id.to_string(),
                    },
                )
                .await
                .expect("append");
        }

        /// Journals a finished outcome carrying a run id, the way every entry
        /// point does post-#371.
        async fn journal_finish(
            state: &AppState,
            id: &CompanyId,
            workflow_id: &str,
            run_id: &str,
            scheduled: bool,
            error: Option<&str>,
        ) {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .append(
                    id,
                    CompanyEvent::WorkflowRunFinished {
                        workflow_id: workflow_id.to_string(),
                        scheduled,
                        run_id: Some(run_id.to_string()),
                        deliveries: Vec::new(),
                        pending_approvals: Vec::new(),
                        error: error.map(str::to_string),
                        cancelled: false,
                        notices: Vec::new(),
                        board: Vec::new(),
                        blocked_nodes: Vec::new(),
                        approvals: Vec::new(),
                    },
                )
                .await
                .expect("append");
        }

        /// **The issue's durable half at the HTTP boundary.** A run's start,
        /// its per-node rows and its outcome come back as ONE history entry
        /// carrying the node trail — which is what makes a scheduled run's
        /// failure point readable after the fact.
        #[tokio::test]
        async fn run_history_groups_a_runs_nodes_under_one_entry() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_start(&state, &id, "digest", "run-1", true).await;
            journal_node(
                &state,
                &id,
                "digest",
                "run-1",
                "ceo",
                WorkflowNodeStatus::Ok,
            )
            .await;
            journal_node(
                &state,
                &id,
                "digest",
                "run-1",
                "send",
                WorkflowNodeStatus::Error,
            )
            .await;
            journal_finish(&state, &id, "digest", "run-1", true, Some("send failed")).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 1, "four journal rows fold to one run: {body}");

            assert_eq!(rows[0]["runId"], "run-1");
            assert_eq!(rows[0]["error"], "send failed");
            assert!(rows[0].get("running").is_none(), "a settled run: {body}");
            assert!(rows[0]["startedAtMillis"].is_number(), "{body}");

            // In finish order, with the status and duration the canvas paints.
            let nodes = rows[0]["nodes"].as_array().expect("nodes");
            assert_eq!(nodes.len(), 2);
            assert_eq!(nodes[0]["nodeId"], "ceo");
            assert_eq!(nodes[0]["status"], "ok");
            assert_eq!(nodes[0]["elapsedMs"], 42);
            assert_eq!(nodes[1]["nodeId"], "send");
            assert_eq!(nodes[1]["status"], "error");
        }

        // ── Issue #1010: the node executing RIGHT NOW ──────────────────────

        /// **The issue.** A run still in flight comes back naming the node it
        /// is standing on, not just the ones it is done with.
        ///
        /// Before this the fold read `WorkflowNodeStarted` nowhere, so an
        /// in-flight run's only per-node facts were its finishes — and every
        /// console that learned about a run from the history rather than from a
        /// start frame (a reload, a cron fire, a reconnect, a workflow switch
        /// and back) painted a graph with a gap where the working node was.
        #[tokio::test]
        async fn run_history_names_the_node_a_running_run_is_executing() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            // Registered on the supervisor, and the guard held across the read:
            // since #1009 a start with no finish whose id is NOT live is settled
            // by the read itself, so a genuinely in-flight run is the only way
            // to see `running: true` — and it is the case under test.
            let runtime = state.registry().get(&id).expect("registered");
            let (ctx, _guard) = runtime
                .run_supervisor()
                .begin("digest", true)
                .expect("under the default cap");
            let run = ctx.run_id.clone();
            journal_start(&state, &id, "digest", &run, true).await;
            journal_node_started(&state, &id, "digest", &run, "ceo").await;
            journal_node(&state, &id, "digest", &run, "ceo", WorkflowNodeStatus::Ok).await;
            // Started and NOT finished — the node the run is on. No finish is
            // journaled for it, which is the whole shape under test.
            journal_node_started(&state, &id, "digest", &run, "draft").await;

            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 1, "one run: {body}");
            assert_eq!(rows[0]["running"], true, "still in flight: {body}");
            assert_eq!(rows[0]["runId"], run, "{body}");

            // In start order, both brackets — the reader subtracts.
            let started = rows[0]["startedNodes"].as_array().expect("startedNodes");
            assert_eq!(
                started.len(),
                2,
                "both starts are recorded, finished or not: {body}"
            );
            assert_eq!(started[0], "ceo");
            assert_eq!(started[1], "draft");

            // Only the finished one has a node row, so "started minus finished"
            // is exactly the node executing now.
            let nodes = rows[0]["nodes"].as_array().expect("nodes");
            assert_eq!(nodes.len(), 1, "one node has finished: {body}");
            assert_eq!(nodes[0]["nodeId"], "ceo");
        }

        /// A start whose run has no entry is dropped, not turned into a run of
        /// its own — the same rule the finish arm follows.
        ///
        /// The `?workflow=` filter is the reachable way to produce this: the
        /// start row for another workflow's run never opened an entry, so its
        /// node brackets have nothing to attach to.
        #[tokio::test]
        async fn a_started_node_of_a_filtered_out_run_is_dropped() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_start(&state, &id, "other", "run-other", false).await;
            journal_node_started(&state, &id, "other", "run-other", "ceo").await;
            journal_start(&state, &id, "digest", "run-mine", false).await;
            journal_node_started(&state, &id, "digest", "run-mine", "draft").await;

            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/runs?workflow=digest",
                    None,
                ))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 1, "only the asked-for workflow: {body}");
            assert_eq!(rows[0]["runId"], "run-mine");
            let started = rows[0]["startedNodes"].as_array().expect("startedNodes");
            assert_eq!(started.len(), 1, "{body}");
            assert_eq!(started[0], "draft");
        }

        /// A run journaled before #382 — no starts at all — keeps the wire shape
        /// it had: `startedNodes` is omitted entirely rather than sent empty.
        #[tokio::test]
        async fn a_run_with_no_started_rows_omits_the_field() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_start(&state, &id, "digest", "run-old", false).await;
            journal_node(
                &state,
                &id,
                "digest",
                "run-old",
                "ceo",
                WorkflowNodeStatus::Ok,
            )
            .await;
            journal_finish(&state, &id, "digest", "run-old", false, None).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            assert!(
                body[0].get("startedNodes").is_none(),
                "an empty trail is absent, not `[]`: {body}"
            );
        }

        /// The receipt SURVIVES the finish, so a run that was cancelled or lost
        /// mid-node still says which node it was standing on.
        ///
        /// That id is the one fact neither list carries alone: `nodes` never
        /// gets a row for a node that did not finish, and a cleared
        /// `startedNodes` would throw away the only record that it began. The
        /// console pairs this with `running` before painting anything live —
        /// see `statesFromRun` — so keeping it cannot leave a settled run
        /// spinning.
        #[tokio::test]
        async fn a_settled_run_keeps_the_node_it_was_standing_on() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_start(&state, &id, "digest", "run-cut", false).await;
            journal_node_started(&state, &id, "digest", "run-cut", "ceo").await;
            journal_node(
                &state,
                &id,
                "digest",
                "run-cut",
                "ceo",
                WorkflowNodeStatus::Ok,
            )
            .await;
            // Begun, and then the run ended without it ever finishing.
            journal_node_started(&state, &id, "digest", "run-cut", "draft").await;
            journal_finish(&state, &id, "digest", "run-cut", false, Some("cancelled")).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            assert!(body[0].get("running").is_none(), "settled: {body}");
            let started = body[0]["startedNodes"].as_array().expect("startedNodes");
            assert_eq!(started.len(), 2, "{body}");
            assert_eq!(started[1], "draft");
            let nodes = body[0]["nodes"].as_array().expect("nodes");
            assert_eq!(nodes.len(), 1, "`draft` never finished: {body}");
        }

        /// Issues #881 / #880 at the HTTP boundary: a blocked run reads as
        /// blocked in the history, and **its node chip is relabelled too**.
        ///
        /// The node row is journaled live, node by node, long before anything
        /// knows the run stopped for an approval rather than a fault — so the
        /// durable `WorkflowNodeFinished` says `error`, honestly, in the
        /// engine's own terms. Without the read-side relabel the panel would
        /// show a run that says "blocked" beside a node chip that says
        /// "failed", and an operator would go hunting for a bug that is not
        /// there.
        #[tokio::test]
        async fn run_history_reports_a_blocked_node_and_the_approvals_it_parked() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_start(&state, &id, "digest", "run-b", false).await;
            // The engine's own account: the capability returned an error, so
            // the observer reported `error`.
            journal_node(
                &state,
                &id,
                "digest",
                "run-b",
                "spec",
                WorkflowNodeStatus::Error,
            )
            .await;
            let runtime = state.registry().get(&id).expect("registered");
            runtime
                .events()
                .append(
                    &id,
                    CompanyEvent::WorkflowRunFinished {
                        workflow_id: "digest".to_string(),
                        scheduled: false,
                        run_id: Some("run-b".to_string()),
                        deliveries: Vec::new(),
                        pending_approvals: vec!["spec".to_string()],
                        // The whole point: a blocked run carries NO error.
                        error: None,
                        cancelled: false,
                        notices: Vec::new(),
                        board: Vec::new(),
                        blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
                            node_id: "spec".to_string(),
                            tools: vec!["publish_artifact".to_string()],
                            approval_ids: vec!["appr-1".to_string()],
                            unparkable: 0,
                        }],
                        approvals: vec![crate::ports::WorkflowRunApprovalRow {
                            node_id: Some("spec".to_string()),
                            tool: Some("publish_artifact".to_string()),
                            outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                            approval_id: Some("appr-1".to_string()),
                        }],
                    },
                )
                .await
                .expect("append");

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 1, "{body}");
            assert!(
                rows[0].get("error").is_none(),
                "a run waiting on a person did not fail: {body}"
            );
            assert_eq!(rows[0]["blockedNodes"][0]["nodeId"], "spec");
            assert_eq!(rows[0]["approvals"][0]["outcome"], "parked");
            assert_eq!(
                rows[0]["nodes"][0]["status"], "blocked",
                "the node chip must agree with the run's terminal reading: {body}"
            );
        }

        /// A run the process is genuinely executing — registered on the
        /// supervisor, so its id is in `live()` — folds as `running: true` with
        /// the nodes it has completed so far. Since #1009 a start with no finish
        /// whose id is NOT live is settled on the read instead
        /// ([`a_run_absent_from_the_live_set_is_settled_by_the_read`]); this pins
        /// the still-running half of that split, with the guard held across the
        /// request so the registration stays live for the whole read.
        #[tokio::test]
        async fn run_history_reports_an_unsettled_run_as_running() {
            let home_dir = home();
            let (state, _store, id) = hosted_state(home_dir.path()).await;

            let runtime = state.registry().get(&id).expect("registered");
            let (ctx, _guard) = runtime
                .run_supervisor()
                .begin("digest", false)
                .expect("under the default cap");
            journal_start(&state, &id, "digest", &ctx.run_id, false).await;
            journal_node(
                &state,
                &id,
                "digest",
                &ctx.run_id,
                "ceo",
                WorkflowNodeStatus::Ok,
            )
            .await;

            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            assert_eq!(body[0]["running"], true, "{body}");
            assert_eq!(body[0]["nodes"].as_array().unwrap().len(), 1);
            assert!(body[0].get("error").is_none(), "{body}");
        }

        /// **The compatibility claim, pinned.** A journal written before #371
        /// carries finished rows with no run id and no starts. Those fold
        /// exactly as they always did — one row in, one entry out, no `nodes`
        /// key, no `running` key, no `startedAtMillis`.
        #[tokio::test]
        async fn run_history_folds_pre_371_rows_unchanged() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_run(&state, &id, "digest", true, Vec::new(), None).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["workflowId"], "digest");
            assert!(rows[0].get("nodes").is_none(), "{body}");
            assert!(rows[0].get("running").is_none(), "{body}");
            assert!(rows[0].get("startedAtMillis").is_none(), "{body}");
            assert!(rows[0].get("runId").is_none(), "{body}");
        }

        /// Two runs interleaving on one journal — the shape two concurrent
        /// workflows produce — attach their nodes to the right entry. This is
        /// why the fold groups on run id rather than on row adjacency.
        #[tokio::test]
        async fn run_history_keeps_interleaved_runs_apart() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_start(&state, &id, "a", "run-a", false).await;
            journal_start(&state, &id, "b", "run-b", false).await;
            journal_node(&state, &id, "b", "run-b", "b1", WorkflowNodeStatus::Ok).await;
            journal_node(&state, &id, "a", "run-a", "a1", WorkflowNodeStatus::Ok).await;
            journal_finish(&state, &id, "a", "run-a", false, None).await;
            journal_finish(&state, &id, "b", "run-b", false, None).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 2, "{body}");
            let by_id = |run: &str| {
                rows.iter()
                    .find(|r| r["runId"] == run)
                    .unwrap_or_else(|| panic!("{run} missing: {body}"))
                    .clone()
            };
            assert_eq!(by_id("run-a")["nodes"][0]["nodeId"], "a1");
            assert_eq!(by_id("run-a")["nodes"].as_array().unwrap().len(), 1);
            assert_eq!(by_id("run-b")["nodes"][0]["nodeId"], "b1");
            assert_eq!(by_id("run-b")["nodes"].as_array().unwrap().len(), 1);
        }

        /// `?limit=` now cuts **runs**, not journal rows — the number the caller
        /// was asking about all along. Without the group-aware cut, a limit of 2
        /// over three 4-row runs would return fragments.
        #[tokio::test]
        async fn run_history_limit_counts_runs_not_journal_rows() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            for i in 0..3 {
                let run = format!("run-{i}");
                journal_start(&state, &id, "digest", &run, false).await;
                journal_node(&state, &id, "digest", &run, "ceo", WorkflowNodeStatus::Ok).await;
                journal_node(&state, &id, "digest", &run, "done", WorkflowNodeStatus::Ok).await;
                journal_finish(&state, &id, "digest", &run, false, None).await;
            }

            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/runs?limit=2",
                    None,
                ))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 2, "{body}");
            // Newest first, and each one whole.
            assert_eq!(rows[0]["runId"], "run-2");
            assert_eq!(rows[0]["nodes"].as_array().unwrap().len(), 2);
            assert_eq!(rows[1]["runId"], "run-1");
        }

        /// `?workflow=` narrows to one graph, and does so BEFORE the limit cut —
        /// otherwise asking for one workflow would return "whichever of the last
        /// N happen to match", which for a busy company is usually none.
        #[tokio::test]
        async fn run_history_filters_by_workflow_before_the_limit_cut() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            journal_run(&state, &id, "digest", true, Vec::new(), None).await;
            for _ in 0..5 {
                journal_run(&state, &id, "greeter", false, Vec::new(), None).await;
            }

            // Only ONE `digest` run exists, and it is the OLDEST of six. A
            // filter applied after a `limit=2` cut would find nothing.
            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/runs?workflow=digest&limit=2",
                    None,
                ))
                .await
                .unwrap();
            let body = json_body(response).await;
            let rows = body.as_array().expect("array");
            assert_eq!(rows.len(), 1, "body: {body}");
            assert_eq!(rows[0]["workflowId"], "digest");
        }

        /// `?limit=` caps the page from the newest end, defaults when absent or
        /// zero, and clamps above the ceiling rather than folding the whole log.
        #[tokio::test]
        async fn run_history_limit_defaults_caps_and_clamps() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;

            for i in 0..25 {
                journal_run(&state, &id, &format!("wf-{i}"), false, Vec::new(), None).await;
            }

            let page = |uri: &'static str| {
                let state = state.clone();
                async move {
                    json_body(
                        router(state)
                            .oneshot(request("GET", uri, None))
                            .await
                            .unwrap(),
                    )
                    .await
                }
            };

            // Explicit cap, taken from the newest end.
            let capped = page("/api/v1/company/workflows/runs?limit=3").await;
            let rows = capped.as_array().expect("array");
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0]["workflowId"], "wf-24", "newest first: {capped}");

            // No `limit` → the default page, not the whole 25.
            let defaulted = page("/api/v1/company/workflows/runs").await;
            assert_eq!(
                defaulted.as_array().unwrap().len(),
                DEFAULT_RUN_LIMIT,
                "{defaulted}"
            );

            // `limit=0` means "I didn't really mean zero" — an empty page is
            // never what a caller wants, so it falls back to the default.
            let zero = page("/api/v1/company/workflows/runs?limit=0").await;
            assert_eq!(zero.as_array().unwrap().len(), DEFAULT_RUN_LIMIT, "{zero}");

            // Above the ceiling clamps; with only 25 rows that is all of them.
            let huge = page("/api/v1/company/workflows/runs?limit=100000").await;
            assert_eq!(huge.as_array().unwrap().len(), 25, "{huge}");
        }

        /// A company that has never run a workflow gets an empty list, not a
        /// 404 — the history panel renders "nothing yet" rather than an error.
        #[tokio::test]
        async fn run_history_is_empty_before_any_run() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(json_body(response).await.as_array().unwrap().len(), 0);
        }

        /// **Route-ordering pin.** `runs` is a syntactically valid `wid`, so
        /// `GET /workflows/runs` overlaps `GET /workflows/{wid}`. Axum prefers
        /// the static segment; if that ever changed, the history panel would
        /// silently 404 (there is no workflow named `runs`) instead of failing
        /// loudly. Same trade, and same pin, as `GET /tasks/inflight`.
        #[tokio::test]
        async fn run_history_is_not_shadowed_by_the_graph_read() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;
            journal_run(&state, &id, "digest", true, Vec::new(), None).await;

            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "the static /workflows/runs must win over /workflows/{{wid}}"
            );
            // An array of outcomes, not a single graph object.
            let body = json_body(response).await;
            assert!(body.is_array(), "graph read shadowed the history: {body}");
        }

        // -------------------------------------------------------------------
        // Cron preview (issue #262)
        // -------------------------------------------------------------------

        /// 2026-08-02 12:00 UTC, as epoch millis — the `after` pin every
        /// preview test searches forward from, so the answers are fixed rather
        /// than relative to whenever CI runs.
        const AFTER: u64 = 1_785_672_000_000;

        async fn preview(state: &AppState, expr: &str) -> serde_json::Value {
            json_body(
                router(state.clone())
                    .oneshot(request(
                        "POST",
                        "/api/v1/company/workflows/cron/preview",
                        Some(serde_json::json!({ "expr": expr, "after": AFTER })),
                    ))
                    .await
                    .unwrap(),
            )
            .await
        }

        /// **The issue #262 pin.** `0 9 * * *` and `9 0 * * *` are two
        /// characters apart, both valid, and nine hours different. The preview
        /// is the only thing that tells them apart before the report arrives at
        /// the wrong time, so the distinction is pinned end-to-end over HTTP,
        /// not just in the matcher's unit tests.
        #[tokio::test]
        async fn cron_preview_distinguishes_nine_am_from_nine_past_midnight() {
            let home_dir = home();
            let (state, _store, _id) = hosted_state(home_dir.path()).await;

            let morning = preview(&state, "0 9 * * *").await;
            assert_eq!(morning["description"], "Every day at 09:00 UTC");
            // 2026-08-02 12:00 UTC → the next 09:00 is the following morning.
            assert_eq!(morning["next"][0], 1_785_747_600_000u64);
            assert_eq!(morning["next"].as_array().unwrap().len(), 3);

            let midnight = preview(&state, "9 0 * * *").await;
            assert_eq!(midnight["description"], "Every day at 00:09 UTC");
            assert_ne!(morning["next"][0], midnight["next"][0]);
        }

        /// A shape the humaniser declines to paraphrase still previews: the
        /// description is `null` and the fire times carry the meaning. The
        /// console shows "Next runs: …" rather than nothing.
        #[tokio::test]
        async fn cron_preview_returns_fires_without_a_description() {
            let home_dir = home();
            let (state, _store, _id) = hosted_state(home_dir.path()).await;

            let body = preview(&state, "0 0 1 * *").await;
            assert!(body["description"].is_null(), "{body}");
            assert_eq!(body["next"].as_array().unwrap().len(), 3, "{body}");
        }

        /// **Malformed input answers 200, not 4xx.** The console previews while
        /// the author is still typing, so a half-written expression is the
        /// normal live state — and the console's client throws on any non-2xx,
        /// which would turn every keystroke into a caught exception. The parser
        /// message rides in the body instead.
        #[tokio::test]
        async fn cron_preview_reports_a_parse_error_as_a_200_body() {
            let home_dir = home();
            let (state, _store, _id) = hosted_state(home_dir.path()).await;

            let response = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows/cron/preview",
                    Some(serde_json::json!({ "expr": "every day" })),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = json_body(response).await;
            let error = body["error"].as_str().expect("an error message: {body}");
            assert!(error.contains("5 fields"), "{body}");
            assert!(body["next"].is_null(), "no fire times on a parse error");
        }

        /// **Route-ordering pin**, the same trade `/workflows/runs` takes:
        /// `cron` is a syntactically valid `wid`, so the static preview path is
        /// registered before `/workflows/{wid}`. A regression would route the
        /// preview into the graph read and 404.
        #[tokio::test]
        async fn cron_preview_is_not_shadowed_by_the_graph_read() {
            let home_dir = home();
            let (state, _store, _id) = hosted_state(home_dir.path()).await;

            let response = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/companies/acme/workflows/cron/preview",
                    Some(serde_json::json!({ "expr": "0 9 * * MON" })),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "the static /workflows/cron/preview must win over /workflows/{{wid}}"
            );
            let body = json_body(response).await;
            assert_eq!(body["description"], "Every Mon at 09:00 UTC", "{body}");
        }

        /// Both scope forms serve the history — the platform
        /// `…/companies/{id}/…` address as well as the prosumer alias.
        #[tokio::test]
        async fn run_history_serves_both_scope_forms() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;
            journal_run(&state, &id, "digest", true, Vec::new(), None).await;

            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/companies/acme/workflows/runs",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert_eq!(body[0]["workflowId"], "digest");
        }

        // ── Issue #259: edit + delete at the HTTP boundary ──────────────────

        /// Creates `greeter` and returns its current version token.
        async fn create_greeter(state: &AppState) -> String {
            let response = router(state.clone())
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows",
                    Some(create_body()),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let created = json_body(response).await;
            // A freshly created overlay graph is editable and carries a token.
            assert_eq!(created["editable"], true, "{created}");
            created["version"]
                .as_str()
                .unwrap_or_else(|| panic!("create must return a version token: {created}"))
                .to_string()
        }

        /// `create_body()` with a schedule on the trigger and a changed
        /// description — the exact "I typo'd my cron" edit the issue is about.
        fn edited_body(expected_version: Option<&str>) -> serde_json::Value {
            let mut body = serde_json::json!({
                "id": "greeter",
                "name": "Greeter",
                "description": "Say hi, every morning.",
                "nodes": [
                    { "id": "start", "kind": "trigger", "name": "Start", "schedule": "0 9 * * *" },
                    { "id": "done", "kind": "output", "name": "Report" }
                ],
                "edges": [ { "from": "start", "to": "done", "label": "ok" } ]
            });
            if let Some(v) = expected_version {
                body["expectedVersion"] = serde_json::json!(v);
            }
            body
        }

        /// **The issue, at the HTTP boundary.** A saved workflow's cron was
        /// permanent; now it can be corrected and the correction reads back.
        #[tokio::test]
        async fn edit_replaces_the_graph_and_reads_back() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            let version = create_greeter(&state).await;

            let response = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(Some(&version))),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let updated = json_body(response).await;
            assert_eq!(updated["nodes"][0]["schedule"], "0 9 * * *");
            // The response carries the NEW token, so a second save needs no
            // intervening read.
            let next = updated["version"].as_str().expect("new token").to_string();
            assert_ne!(next, version, "the token must move with the body");

            // And a fresh read agrees — the edit is what the read path serves.
            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            let graph = json_body(response).await;
            assert_eq!(graph["nodes"][0]["schedule"], "0 9 * * *");
            assert_eq!(graph["description"], "Say hi, every morning.");
            assert_eq!(graph["version"], next.as_str());

            // Still exactly one workflow — an edit replaces, never forks.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let items = json_body(response).await;
            assert_eq!(own_rows(&items).len(), 1, "{items}");
        }

        // ── Issue #274: revision history + rollback at the HTTP boundary ────

        /// Edits `greeter` once (adding a schedule) so exactly one revision — the
        /// original, schedule-less body — is captured, and returns the token of
        /// the now-current (scheduled) graph.
        async fn create_then_edit_greeter(state: &AppState) -> String {
            let version = create_greeter(state).await;
            let response = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(Some(&version))),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            json_body(response).await["version"]
                .as_str()
                .expect("new token")
                .to_string()
        }

        /// `GET …/revisions` returns metadata only — id, name, version,
        /// createdAtMillis — and never a graph body. Leaking the TOML/nodes here
        /// would make the list as heavy as N graph reads and expose the raw
        /// stored body the console never asked for.
        #[tokio::test]
        async fn revisions_list_is_metadata_only_and_newest_first() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_then_edit_greeter(&state).await;

            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/greeter/revisions",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            let revs = body["revisions"].as_array().expect("revisions array");
            assert_eq!(revs.len(), 1, "one edit captured one revision: {body}");
            let row = &revs[0];
            assert!(row["id"].is_string());
            assert_eq!(row["name"], "Greeter");
            assert!(row["version"].is_string());
            assert!(row["createdAtMillis"].is_number());
            // No graph body leaks into the list.
            assert!(row.get("nodes").is_none(), "metadata only: {row}");
            assert!(row.get("edges").is_none(), "metadata only: {row}");
            assert!(
                row.get("toml").is_none(),
                "the raw body must never leak: {row}"
            );
        }

        /// `POST …/revisions/{rev}/restore` reverts the live graph to the
        /// snapshot and answers with the restored body + a fresh token. The
        /// captured revision was the schedule-less original, so the restore
        /// removes the schedule the edit added.
        #[tokio::test]
        async fn restore_reverts_the_live_graph() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            // The token of the now-current (edited) graph — the one the restore
            // replaces, and which it must carry (required since #1013).
            let current = create_then_edit_greeter(&state).await;

            // Discover the revision id from the list.
            let list = json_body(
                router(state.clone())
                    .oneshot(request(
                        "GET",
                        "/api/v1/company/workflows/greeter/revisions",
                        None,
                    ))
                    .await
                    .unwrap(),
            )
            .await;
            let rev_id = list["revisions"][0]["id"].as_str().unwrap().to_string();

            // Restore it, carrying the current graph's token.
            let response = router(state.clone())
                .oneshot(request(
                    "POST",
                    &format!("/api/v1/company/workflows/greeter/revisions/{rev_id}/restore"),
                    Some(serde_json::json!({ "expectedVersion": current })),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let restored = json_body(response).await;
            // The schedule the edit introduced is gone — the original is back.
            assert!(
                restored["nodes"][0].get("schedule").is_none(),
                "restore should drop the edit's schedule: {restored}"
            );
            assert!(
                restored["version"].is_string(),
                "restore returns a fresh token"
            );

            // A fresh read agrees, and the restore itself was captured, so the
            // history now holds two snapshots (the original + the scheduled body
            // the restore replaced).
            let graph = json_body(
                router(state.clone())
                    .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                    .await
                    .unwrap(),
            )
            .await;
            assert!(graph["nodes"][0].get("schedule").is_none(), "{graph}");
            let list = json_body(
                router(state)
                    .oneshot(request(
                        "GET",
                        "/api/v1/company/workflows/greeter/revisions",
                        None,
                    ))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(
                list["revisions"].as_array().unwrap().len(),
                2,
                "the restore captured the body it replaced: {list}"
            );
        }

        /// Restoring a revision id that does not exist is a clean `404`.
        #[tokio::test]
        async fn restore_unknown_revision_is_not_found() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            // A token is required (issue #1013), so send one; the unknown revision
            // is resolved before the token is ever compared, so this stays a 404.
            let current = create_then_edit_greeter(&state).await;

            let response = router(state)
                .oneshot(request(
                    "POST",
                    "/api/v1/company/workflows/greeter/revisions/no-such-rev/restore",
                    Some(serde_json::json!({ "expectedVersion": current })),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// **The silent-clobber guard on restore (issue #1013).** A restore with
        /// no token — like an omitted body — used to overwrite unconditionally,
        /// so a stale editor could clobber a concurrent save. It is now a `400`
        /// that tells the operator to re-read and send the `version`.
        #[tokio::test]
        async fn a_restore_without_a_token_is_rejected() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_then_edit_greeter(&state).await;

            // Discover a real revision id so the 400 is about the missing token,
            // not the revision.
            let list = json_body(
                router(state.clone())
                    .oneshot(request(
                        "GET",
                        "/api/v1/company/workflows/greeter/revisions",
                        None,
                    ))
                    .await
                    .unwrap(),
            )
            .await;
            let rev_id = list["revisions"][0]["id"].as_str().unwrap().to_string();

            let response = router(state)
                .oneshot(request(
                    "POST",
                    &format!("/api/v1/company/workflows/greeter/revisions/{rev_id}/restore"),
                    Some(serde_json::json!({})),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = json_body(response).await;
            let message = body["error"].as_str().unwrap_or_default().to_lowercase();
            assert!(
                message.contains("version") && message.contains("read"),
                "the 400 must tell the operator to re-read and send the version: {body}"
            );
        }

        /// A workflow that was never edited has an empty history — `200 []`, not
        /// a `404`.
        #[tokio::test]
        async fn revisions_of_an_unedited_workflow_are_empty() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_greeter(&state).await;

            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/greeter/revisions",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert_eq!(body["revisions"].as_array().unwrap().len(), 0, "{body}");
        }

        /// **The silent-overwrite guard.** Two consoles hold the same graph; one
        /// saves, then the other saves its stale copy. The second must be
        /// refused, not silently win.
        #[tokio::test]
        async fn a_stale_expected_version_is_a_conflict() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            let stale = create_greeter(&state).await;

            // Console A saves first.
            let first = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(Some(&stale))),
                ))
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::OK);

            // Console B saves with the token it loaded before A's write.
            let second = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(Some(&stale))),
                ))
                .await
                .unwrap();
            assert_eq!(second.status(), StatusCode::CONFLICT);
            // The message must tell the operator what to do, not just say no.
            let body = json_body(second).await;
            let message = body["error"].as_str().unwrap_or_default().to_lowercase();
            assert!(message.contains("reload"), "unhelpful 409: {body}");

            // A's edit is intact — the refusal changed nothing.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            let graph = json_body(response).await;
            assert_eq!(graph["description"], "Say hi, every morning.");
        }

        /// **The silent-clobber guard, at the front door (issue #1013).** Omitting
        /// the token used to be an unconditional write; a stale editor could then
        /// overwrite a concurrent save without ever seeing a `409`. A tokenless
        /// `PUT` is now a `400` that tells the operator to re-read and resend the
        /// `version`.
        #[tokio::test]
        async fn an_edit_without_a_token_is_rejected() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_greeter(&state).await;

            let response = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(None)),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = json_body(response).await;
            let message = body["error"].as_str().unwrap_or_default().to_lowercase();
            assert!(
                message.contains("version") && message.contains("read"),
                "the 400 must tell the operator to re-read and resend the version: {body}"
            );

            // The refusal changed nothing — the original description is intact.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            let graph = json_body(response).await;
            assert_eq!(graph["description"], "Say hi.");
        }

        /// A `PUT` that would rename the id is a 400, not a silent create — the
        /// id keys the saved graph, its schedule and its run history.
        #[tokio::test]
        async fn an_id_mismatch_is_a_bad_request() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_greeter(&state).await;

            let mut body = edited_body(None);
            body["id"] = serde_json::json!("greeter-v2");
            let response = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);

            // Nothing was created under the new id.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let items = json_body(response).await;
            let own = own_rows(&items);
            assert_eq!(own.len(), 1, "{items}");
            assert_eq!(own[0]["id"], "greeter");
        }

        #[tokio::test]
        async fn editing_an_unknown_workflow_is_not_found() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            // A token is required (issue #1013), so send one; the unknown id is
            // resolved before the token is ever compared, so this stays a 404.
            let mut body = edited_body(Some("deadbeef"));
            body["id"] = serde_json::json!("ghost");
            let response = router(state)
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/ghost",
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// A bad edit is refused on the same terms as a bad create — the shared
        /// validation, at the HTTP boundary.
        #[tokio::test]
        async fn a_structurally_invalid_edit_is_a_bad_request() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            let version = create_greeter(&state).await;

            // No trigger node at all. Carries a valid token (required since #1013)
            // so the 400 comes from structural validation, not the token guard.
            let body = serde_json::json!({
                "id": "greeter",
                "name": "Greeter",
                "nodes": [ { "id": "done", "kind": "output", "name": "Report" } ],
                "edges": [],
                "expectedVersion": version
            });
            let response = router(state)
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        /// **The delete, and its durability.** A removed workflow leaves the
        /// picker AND stays gone across a full state rebuild — the property that
        /// matters, because `merge_enabled_workflows` (#208) re-derives the
        /// enabled list at boot and would resurrect a half-delete.
        #[tokio::test]
        async fn delete_removes_it_from_the_picker_and_survives_a_rebuild() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;
            let version = create_greeter(&state).await;

            let response = router(state.clone())
                .oneshot(request(
                    "DELETE",
                    &format!("/api/v1/company/workflows/greeter?expectedVersion={version}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);

            // Gone from the picker…
            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let items = json_body(response).await;
            assert_eq!(own_rows(&items).len(), 0, "{items}");

            // …and from the graph read.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            // Rebuild everything from the same durable store: still gone.
            let rebuilt = state_over(&home, &id, false).await;
            let response = router(rebuilt)
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let items = json_body(response).await;
            assert_eq!(
                own_rows(&items).len(),
                0,
                "a deleted workflow must not come back on restart: {items}"
            );
        }

        /// **Run history is orphaned, not reaped.** What a workflow did stays
        /// true after the workflow is gone, and the journal is append-only.
        #[tokio::test]
        async fn deleting_a_workflow_keeps_its_run_history() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, id) = hosted_state(&home).await;
            let version = create_greeter(&state).await;
            journal_run(
                &state,
                &id,
                "greeter",
                true,
                vec![undelivered_row("done")],
                None,
            )
            .await;

            let response = router(state.clone())
                .oneshot(request(
                    "DELETE",
                    &format!("/api/v1/company/workflows/greeter?expectedVersion={version}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);

            let response = router(state)
                .oneshot(request(
                    "GET",
                    "/api/v1/company/workflows/runs?workflow=greeter",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let rows = json_body(response).await;
            assert_eq!(
                rows.as_array().unwrap().len(),
                1,
                "past runs must outlive the workflow: {rows}"
            );
            assert_eq!(rows[0]["workflowId"], "greeter");
        }

        #[tokio::test]
        async fn deleting_with_a_stale_version_is_a_conflict() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            let stale = create_greeter(&state).await;

            // Someone edits after the console loaded the graph. This uses the
            // then-current token (`stale`); the edit moves the version, so the
            // delete below carries a now-stale one.
            let edited = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(Some(&stale))),
                ))
                .await
                .unwrap();
            assert_eq!(edited.status(), StatusCode::OK);

            let response = router(state.clone())
                .oneshot(request(
                    "DELETE",
                    &format!("/api/v1/company/workflows/greeter?expectedVersion={stale}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);

            // Still there.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn deleting_an_unknown_workflow_is_not_found() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            // A token is required (issue #1013), so send one; the unknown id is
            // resolved before the token is ever compared, so this stays a 404.
            let response = router(state)
                .oneshot(request(
                    "DELETE",
                    "/api/v1/company/workflows/ghost?expectedVersion=deadbeef",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// **The silent-clobber guard on delete (issue #1013).** A tokenless
        /// `DELETE` used to remove unconditionally; a stale editor could drop a
        /// workflow that changed underneath them. It is now a `400` that tells the
        /// operator to re-read and pass the `version`, and removes nothing.
        #[tokio::test]
        async fn a_delete_without_a_token_is_rejected() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_greeter(&state).await;

            let response = router(state.clone())
                .oneshot(request("DELETE", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = json_body(response).await;
            let message = body["error"].as_str().unwrap_or_default().to_lowercase();
            assert!(
                message.contains("version") && message.contains("read"),
                "the 400 must tell the operator to re-read and pass the version: {body}"
            );

            // The refusal removed nothing — the workflow is still there.
            let response = router(state)
                .oneshot(request("GET", "/api/v1/company/workflows/greeter", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        /// The write verbs are reachable under the platform scope form too, not
        /// just the prosumer alias.
        #[tokio::test]
        async fn edit_and_delete_serve_both_scope_forms() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            let version = create_greeter(&state).await;

            let response = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/companies/acme/workflows/greeter",
                    Some(edited_body(Some(&version))),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            // The edit moved the token; delete with the one it just returned.
            let next = json_body(response).await["version"]
                .as_str()
                .expect("edit returns a fresh token")
                .to_string();

            let response = router(state)
                .oneshot(request(
                    "DELETE",
                    &format!("/api/v1/companies/acme/workflows/greeter?expectedVersion={next}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        /// A manifest-`enabled` id with no saved graph is listed but NOT
        /// editable — there is nothing to replace or remove, and the console
        /// must not offer a button that can only 409.
        #[tokio::test]
        async fn a_bodiless_enabled_id_is_listed_but_not_editable() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let store = FsCompanyStore::new(home.clone());
            let id = CompanyId::new("acme");
            let mut manifest = empty_manifest();
            manifest.workflows.enabled.push("legacy".to_string());
            store
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: manifest.clone(),
                    ledger: Vec::new(),
                    lifecycle: "running".to_string(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: Vec::new(),
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                })
                .await
                .unwrap();
            // The runtime carries its own manifest — the enabled list the list
            // route reads comes from there, not from the record we just saved.
            let runtime = RuntimeBuilder::new(home.clone(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            let state = AppState::new(AppConfig::default());
            state
                .registry()
                .insert(id.clone(), std::sync::Arc::new(runtime));
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;

            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows", None))
                .await
                .unwrap();
            let items = json_body(response).await;
            let legacy = items
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["id"] == "legacy")
                .expect("listed under its id");
            assert_eq!(legacy["editable"], false, "{items}");

            // And the host agrees when actually asked to delete it. A token is
            // required (issue #1013), so send one; the body-less id is a 409
            // before the token is ever compared.
            let response = router(state)
                .oneshot(request(
                    "DELETE",
                    "/api/v1/company/workflows/legacy?expectedVersion=deadbeef",
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
        }

        // ── Issue #1009: settle eternal-`running` rows on the read ─────────

        /// Every `WorkflowRunFinished` the company journaled carrying `run_id`.
        async fn finishes_for(
            state: &AppState,
            id: &CompanyId,
            run_id: &str,
        ) -> Vec<(Option<String>, bool)> {
            let runtime = state.registry().get(id).expect("registered");
            runtime
                .events()
                .read_from(id, crate::ports::types::EventSeq::new(0), usize::MAX)
                .await
                .expect("read")
                .into_iter()
                .filter_map(|s| match s.event {
                    CompanyEvent::WorkflowRunFinished {
                        run_id: Some(rid),
                        error,
                        cancelled,
                        ..
                    } if rid == run_id => Some((error, cancelled)),
                    _ => None,
                })
                .collect()
        }

        /// **The decisive case (issue #1009, path B).** A run whose start was
        /// journaled but whose finish never landed — and whose id is absent from
        /// the live run set — is settled by `list_runs` itself, between boots.
        ///
        /// Two halves, both asserted: the returned row is `running: false` +
        /// `INTERRUPTED_BY_RESTART` (so this very response is self-consistent),
        /// AND a synthetic finish is **durably appended** so the next read folds
        /// it settled and the boot sweep has nothing left to do. Before the fix
        /// the row read `running: true` and nothing was appended.
        #[tokio::test]
        async fn a_run_absent_from_the_live_set_is_settled_by_the_read() {
            let home_dir = home();
            let (state, _store, id) = hosted_state(home_dir.path()).await;

            // A start with no finish. Nothing is registered on the supervisor,
            // so this id is absent from `live()`: the process that owned it went
            // away without journaling a finish.
            journal_start(&state, &id, "digest", "run-dead", true).await;

            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            assert_eq!(body.as_array().unwrap().len(), 1);
            // `running` is skip-serialized when false, so a settled row simply
            // omits it — assert it is not `true` rather than equal to `false`.
            assert_ne!(
                body[0]["running"], true,
                "an absent run is settled on the read, not left spinning: {body}"
            );
            assert_eq!(
                body[0]["error"],
                crate::runtime::workflow_outcome::INTERRUPTED_BY_RESTART
            );

            // Durable half: exactly one synthetic finish is now in the journal.
            let finishes = finishes_for(&state, &id, "run-dead").await;
            assert_eq!(finishes.len(), 1, "exactly one synthetic finish appended");
            assert_eq!(
                finishes[0].0.as_deref(),
                Some(crate::runtime::workflow_outcome::INTERRUPTED_BY_RESTART)
            );
            assert!(!finishes[0].1, "a host-restart settle is not a cancel");
        }

        /// **The mandatory negative (issue #1009, rebuild/clean guard).** A run
        /// the current process is genuinely running is registered on the
        /// supervisor, so its id is in `live()` — and `list_runs` must leave it
        /// alone: the row stays `running: true` and **nothing** is appended.
        ///
        /// This is what keeps the cross-check keyed strictly on `live()`
        /// membership rather than on "has no finish yet", which would stamp
        /// `INTERRUPTED_BY_RESTART` on a run still walking its graph and then
        /// contradict its real finish. The guard is held across the read so the
        /// registration stays live for the whole request.
        #[tokio::test]
        async fn a_run_in_the_live_set_is_left_running() {
            let home_dir = home();
            let (state, _store, id) = hosted_state(home_dir.path()).await;

            let runtime = state.registry().get(&id).expect("registered");
            // Register a live run and HOLD its guard: its id is now in `live()`.
            let (ctx, _guard) = runtime
                .run_supervisor()
                .begin("digest", true)
                .expect("under the default cap");
            journal_start(&state, &id, "digest", &ctx.run_id, true).await;

            let response = router(state.clone())
                .oneshot(request("GET", "/api/v1/company/workflows/runs", None))
                .await
                .unwrap();
            let body = json_body(response).await;
            assert_eq!(body.as_array().unwrap().len(), 1);
            assert_eq!(
                body[0]["running"], true,
                "a run the process is running must not be settled from under it"
            );

            assert!(
                finishes_for(&state, &id, &ctx.run_id).await.is_empty(),
                "a live run gets no synthetic finish appended"
            );
        }
    }

    /// Issue #383: the run route driven against a runner that can be held
    /// mid-run, so detach, cancellation, and surviving a dropped client are all
    /// observable through the real router.
    mod running {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::company::CompanyManifest;
        use crate::ports::types::{CompanyEvent, CompanyId, CompanyRecord, EventSeq};
        use crate::ports::{CompanyStore, WorkflowRun, WorkflowRunContext, WorkflowRunner};
        use crate::runtime::RuntimeBuilder;
        use crate::server::router;
        use crate::store::FsCompanyStore;
        use crate::{AppConfig, AppState};

        /// A runner that parks until released, and settles as cancelled if the
        /// run's stop signal fires first.
        ///
        /// It is the real `WorkflowRunner` port, so everything above it — the
        /// route, the supervisor registration, the spawned task, the journal
        /// write — is production code. Only the graph walk is stubbed, which is
        /// what lets these tests be about the *entry point* rather than about
        /// the engine (the engine's own cancel behaviour is pinned in
        /// `workflows::runner`).
        struct StalledRunner {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            /// Set only if the run was allowed to finish on its own terms —
            /// which is how a test tells "the run completed" from "the run was
            /// dropped with the connection".
            completed: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl WorkflowRunner for StalledRunner {
            async fn run(
                &self,
                _company: &CompanyId,
                _workflow: &crate::company::WorkflowFile,
                _input: serde_json::Value,
                ctx: &WorkflowRunContext,
            ) -> crate::Result<WorkflowRun> {
                // `notify_one` on both, not `notify_waiters`: a permit is
                // stored, so neither side has to be already parked. The detached
                // run answers before its task has even been polled, so a test
                // that waits on `entered` afterwards would otherwise race the
                // notification and hang.
                self.entered.notify_one();
                let released = self.release.notified();
                tokio::select! {
                    () = released => {}
                    () = ctx.cancel.cancelled() => {
                        return Ok(WorkflowRun {
                            output: serde_json::Value::Null,
                            pending_approvals: Vec::new(),
                            deliveries: Vec::new(),
                            cancelled: true,
                            nodes: Vec::new(),
                            notices: Vec::new(),
                            board: Vec::new(),
                            blocked_nodes: Vec::new(),
                            approvals: Vec::new(),
                        });
                    }
                }
                self.completed.store(true, Ordering::SeqCst);
                Ok(WorkflowRun {
                    output: serde_json::json!({ "run": {}, "nodes": {} }),
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

        struct Stalled {
            app: axum::Router,
            runtime: Arc<crate::company::runtime::CompanyRuntime>,
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            completed: Arc<AtomicBool>,
        }

        const GRAPH: &str = r#"
id = "demo"
name = "Demo"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Report"
[[edge]]
from = "start"
to = "done"
label = "ok"
"#;

        fn home() -> tempfile::TempDir {
            tempfile::Builder::new()
                .prefix("oc-workflows-running-")
                .tempdir()
                .expect("tempdir")
        }

        /// A hosted company with one overlay workflow and a runner that stalls.
        async fn stalled_company(home: &std::path::Path) -> Stalled {
            let manifest: CompanyManifest =
                toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
            let id = CompanyId::new("acme");
            FsCompanyStore::new(home.to_path_buf())
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: manifest.clone(),
                    ledger: Vec::new(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                        id: "demo".to_string(),
                        toml: GRAPH.to_string(),
                    }],
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    lifecycle: "running".to_string(),
                    template_provenance: None,
                })
                .await
                .unwrap();

            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let completed = Arc::new(AtomicBool::new(false));
            let mut runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            runtime.set_workflow_runner(Arc::new(StalledRunner {
                entered: entered.clone(),
                release: release.clone(),
                completed: completed.clone(),
            }));
            let runtime = Arc::new(runtime);

            let state = AppState::new(AppConfig::default());
            state.registry().insert(id.clone(), runtime.clone());
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;

            Stalled {
                app: router(state),
                runtime,
                entered,
                release,
                completed,
            }
        }

        fn run_request(body: serde_json::Value) -> Request<Body> {
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/workflows/demo/run")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        }

        fn cancel_request(run_id: &str) -> Request<Body> {
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/company/workflows/runs/{run_id}/cancel"))
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap()
        }

        async fn json_body(response: axum::response::Response) -> serde_json::Value {
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        }

        /// Every event the company journaled, oldest first.
        async fn journal(
            runtime: &Arc<crate::company::runtime::CompanyRuntime>,
        ) -> Vec<CompanyEvent> {
            runtime
                .events()
                .read_from(runtime.id(), EventSeq::new(0), usize::MAX)
                .await
                .expect("read")
                .into_iter()
                .map(|s| s.event)
                .collect()
        }

        /// Waits (bounded) for a `WorkflowRunFinished` to appear.
        async fn await_finished(
            runtime: &Arc<crate::company::runtime::CompanyRuntime>,
        ) -> Option<CompanyEvent> {
            for _ in 0..200 {
                if let Some(event) = journal(runtime)
                    .await
                    .into_iter()
                    .find(|e| matches!(e, CompanyEvent::WorkflowRunFinished { .. }))
                {
                    return Some(event);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            None
        }

        /// **The keystone.** A client that walks away mid-run must not take the
        /// run with it.
        ///
        /// The route used to await the run *inside* the request future, and
        /// hyper drops that future when the peer closes — so `record_run_finished`
        /// never ran and the run produced no history entry at all. Post-#385
        /// that is strictly worse: the run has already journaled a
        /// `WorkflowRunStarted`, so the fold reports `running: true` forever and
        /// `sweep_interrupted_runs` is boot-only. "Workflow Run produces no
        /// run-history entry" is that bug, reported from staging.
        ///
        /// `Router::oneshot` reproduces the cancellation by the same mechanism
        /// hyper uses rather than by analogy: the handler future is owned by the
        /// future being polled, so dropping the latter drops the former.
        #[tokio::test]
        async fn a_dropped_connection_does_not_cancel_a_synchronous_run() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let mut running = Box::pin(c.app.clone().oneshot(run_request(
                serde_json::json!({ "input": { "request": "go" } }),
            )));
            tokio::select! {
                _ = &mut running => panic!("the run answered before the runner was under way"),
                () = c.entered.notified() => {}
            }
            // The client hangs up, exactly as a proxy does when it gives up.
            drop(running);

            // The run must still be there to finish.
            c.release.notify_one();
            let finished = await_finished(&c.runtime).await.expect(
                "the run died with the dropped connection: nothing was journaled, so the \
                         history shows it running forever",
            );
            assert!(
                c.completed.load(Ordering::SeqCst),
                "the runner never got to finish its work"
            );
            let CompanyEvent::WorkflowRunFinished {
                error, cancelled, ..
            } = finished
            else {
                unreachable!()
            };
            assert!(error.is_none(), "a client hanging up is not a run failure");
            assert!(!cancelled, "nobody cancelled this run");
        }

        /// The same proof over a **real socket**, so the keystone rests on
        /// hyper's actual behaviour rather than on `oneshot` modelling it well.
        ///
        /// **The pause after the close is load-bearing.** Hyper does not learn
        /// the peer is gone when the client calls `shutdown` — it learns when
        /// its connection task next polls the socket and reads EOF. Release the
        /// runner before that happens and the run finishes on its own merits, so
        /// the test passes against the broken code and proves nothing.
        #[tokio::test]
        async fn a_real_socket_close_does_not_cancel_a_synchronous_run() {
            use tokio::io::AsyncWriteExt;

            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = c.app.clone();
            let server = tokio::spawn(async move { axum::serve(listener, app).await });

            let body = serde_json::json!({ "input": {} }).to_string();
            let request = format!(
                "POST /api/v1/company/workflows/demo/run HTTP/1.1\r\n\
                 Host: {addr}\r\n\
                 Cookie: {}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                crate::server::test_support::fixed_cookie("acme"),
                body.len(),
            );
            let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            socket.write_all(request.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();

            c.entered.notified().await;
            socket.shutdown().await.unwrap();
            drop(socket);
            // See the doc comment: without this, hyper has not yet noticed.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

            c.release.notify_one();
            assert!(
                await_finished(&c.runtime).await.is_some(),
                "a real peer close cancelled the run: nothing was journaled"
            );
            assert!(c.completed.load(Ordering::SeqCst));
            server.abort();
        }

        /// `detach: true` answers `202` with the run id while the run is
        /// demonstrably still going — the half that removes the wait.
        #[tokio::test]
        async fn a_detached_run_answers_202_before_the_run_finishes() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let response = c
                .app
                .clone()
                .oneshot(run_request(serde_json::json!({ "detach": true })))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let body = json_body(response).await;
            assert_eq!(body["detached"], true, "{body}");
            assert!(
                body["runId"].as_str().is_some_and(|s| !s.is_empty()),
                "the id is the whole point of the response: {body}"
            );
            assert!(
                body.get("output").is_none(),
                "a detached response must not look settled: {body}"
            );
            assert!(
                !c.completed.load(Ordering::SeqCst),
                "the response arrived before the run finished, which is the point"
            );

            // And it settles on its own, with nobody waiting.
            c.release.notify_one();
            assert!(await_finished(&c.runtime).await.is_some());
        }

        /// The wire-compat guarantee in the other direction: a caller that sends
        /// no `detach` gets exactly the response it always got — a `200`
        /// carrying the settled run — so an older console is untouched.
        #[tokio::test]
        async fn a_body_without_detach_still_gets_the_synchronous_response() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;
            // Pre-release, so the runner never parks and this is a plain
            // start-to-finish call — the shape an older console makes.
            c.release.notify_one();

            let response = c
                .app
                .clone()
                .oneshot(run_request(serde_json::json!({ "input": {} })))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert!(
                body.get("output").is_some(),
                "the settled shape carries `output`: {body}"
            );
            assert!(
                body.get("detached").is_none(),
                "the synchronous response must not carry the detach discriminator: {body}"
            );
            assert!(body["runId"].as_str().is_some(), "{body}");
        }

        /// Cancel a live run: `200`, and it settles as cancelled rather than as
        /// an error.
        #[tokio::test]
        async fn cancelling_a_live_run_stops_it_and_records_it_as_cancelled() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let response = c
                .app
                .clone()
                .oneshot(run_request(serde_json::json!({ "detach": true })))
                .await
                .unwrap();
            let run_id = json_body(response).await["runId"]
                .as_str()
                .unwrap()
                .to_string();
            c.entered.notified().await;

            let response = c
                .app
                .clone()
                .oneshot(cancel_request(&run_id))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(json_body(response).await["cancelling"], true);

            let CompanyEvent::WorkflowRunFinished {
                cancelled,
                error,
                run_id: journaled_id,
                ..
            } = await_finished(&c.runtime)
                .await
                .expect("a cancelled run still journals a finish")
            else {
                unreachable!()
            };
            assert!(cancelled, "the outcome must say it was stopped");
            assert!(
                error.is_none(),
                "a deliberate stop is not a failure: {error:?}"
            );
            assert_eq!(
                journaled_id.as_deref(),
                Some(run_id.as_str()),
                "the finish carries the id the run route handed back — no second identifier"
            );
            assert!(
                !c.completed.load(Ordering::SeqCst),
                "the run must not have completed its work"
            );
        }

        /// **A synchronous run can be cancelled mid-request, and its response
        /// has to say so.**
        ///
        /// Easy to miss, because "detached" and "cancellable" sound like the
        /// same feature: the run id is registered the moment the task is
        /// spawned, and the console learns it from the `workflow_run_started`
        /// frame — so the cancel route is reachable well before the synchronous
        /// response is written. The runner then resolves to a cancelled run
        /// whose `output` is `null` with no approvals and no deliveries, which
        /// is byte-identical to a run that legitimately produced nothing. This
        /// caller was the last reader in the PR still left guessing.
        #[tokio::test]
        async fn a_synchronous_run_cancelled_mid_request_says_so_in_its_response() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let mut running = Box::pin(
                c.app
                    .clone()
                    .oneshot(run_request(serde_json::json!({ "input": {} }))),
            );
            // Wait until the runner is actually parked, then find the run the
            // way the console does — by its id — and stop it.
            tokio::select! {
                _ = &mut running => panic!("the run answered before the runner was under way"),
                () = c.entered.notified() => {}
            }
            // Off the supervisor rather than the journal: this stub is the
            // `WorkflowRunner` port, so it never reaches the harness runner that
            // writes `WorkflowRunStarted`. The supervisor is the registration
            // the cancel route itself consults, which makes it the more direct
            // assertion anyway — the id is addressable while the request is open.
            let live = c.runtime.run_supervisor().live();
            assert_eq!(live.len(), 1, "the open synchronous run is registered");
            let (run_id, workflow_id) = live.into_iter().next().unwrap();
            assert_eq!(workflow_id, "demo");
            let response = c
                .app
                .clone()
                .oneshot(cancel_request(&run_id))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "a synchronous run is cancellable while its request is open"
            );

            // The still-open request now answers, and must not read as a clean
            // empty success.
            let response = running.await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert_eq!(body["cancelled"], true, "{body}");
            assert_eq!(body["runId"], run_id.as_str(), "{body}");
            assert!(
                !c.completed.load(Ordering::SeqCst),
                "the run must not have completed its work"
            );
        }

        /// …and the flag is omitted entirely on a run nobody stopped, so an
        /// existing caller's body is byte-unchanged.
        #[tokio::test]
        async fn an_uncancelled_synchronous_response_omits_the_flag() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;
            c.release.notify_one();

            let response = c
                .app
                .clone()
                .oneshot(run_request(serde_json::json!({ "input": {} })))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert!(
                body.get("cancelled").is_none(),
                "a run nobody stopped carries no flag at all: {body}"
            );
        }

        /// The history fold reports it, so the console can render a stopped run
        /// as stopped rather than as a clean success.
        #[tokio::test]
        async fn a_cancelled_run_reads_back_as_cancelled_and_not_running() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let response = c
                .app
                .clone()
                .oneshot(run_request(serde_json::json!({ "detach": true })))
                .await
                .unwrap();
            let run_id = json_body(response).await["runId"]
                .as_str()
                .unwrap()
                .to_string();
            c.entered.notified().await;
            c.app
                .clone()
                .oneshot(cancel_request(&run_id))
                .await
                .unwrap();
            await_finished(&c.runtime).await.expect("settles");

            let response = c
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v1/company/workflows/runs")
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let rows = json_body(response).await;
            let row = &rows.as_array().expect("array")[0];
            assert_eq!(row["runId"], run_id.as_str(), "{rows}");
            assert_eq!(row["cancelled"], true, "{rows}");
            assert!(
                row.get("running").is_none(),
                "a settled run is not running: {rows}"
            );
            assert!(
                row.get("error").is_none(),
                "a stopped run carries no error: {rows}"
            );
        }

        /// Unknown and already-settled are the same `404`: there is nothing to
        /// stop. Keeping a tombstone to tell them apart would mean choosing an
        /// expiry for it, and the run history already says what became of a
        /// settled run.
        #[tokio::test]
        async fn cancelling_an_unknown_or_settled_run_is_not_found() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let response = c
                .app
                .clone()
                .oneshot(cancel_request("never-existed"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            // Now run one to completion and try again.
            let response = c
                .app
                .clone()
                .oneshot(run_request(serde_json::json!({ "detach": true })))
                .await
                .unwrap();
            let run_id = json_body(response).await["runId"]
                .as_str()
                .unwrap()
                .to_string();
            c.entered.notified().await;
            c.release.notify_one();
            await_finished(&c.runtime).await.expect("settles");

            let response = c
                .app
                .clone()
                .oneshot(cancel_request(&run_id))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "a settled run is no longer cancellable"
            );
        }

        /// The cancel route is behind the same `ScopedCompany` guard as every
        /// other route in this module — an unauthenticated caller cannot stop a
        /// company's work.
        #[tokio::test]
        async fn cancelling_without_a_session_is_rejected() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let response = c
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/workflows/runs/whatever/cancel")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                StatusCode::OK,
                "an unauthenticated cancel must not be accepted"
            );
            assert!(
                response.status() == StatusCode::UNAUTHORIZED
                    || response.status() == StatusCode::FORBIDDEN,
                "expected an auth rejection, got {}",
                response.status()
            );
        }

        /// The cancel path is a static prefix under `/workflows`, and `runs` is
        /// a syntactically valid workflow id — so this pins that it is not
        /// shadowed by the dynamic `/workflows/{wid}` routes, the same guarantee
        /// `run_history_is_not_shadowed_by_the_graph_read` makes for the GET.
        #[tokio::test]
        async fn the_cancel_route_is_not_shadowed_by_the_dynamic_workflow_routes() {
            let home_dir = home();
            let c = stalled_company(home_dir.path()).await;

            let response = c
                .app
                .clone()
                .oneshot(cancel_request("anything"))
                .await
                .unwrap();
            // 404 from the *cancel handler* (nothing to stop), not a 405 or a
            // route miss — reaching the handler at all is the assertion.
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let body = json_body(response).await;
            assert!(
                body.to_string().contains("workflow run"),
                "the 404 should come from the cancel handler: {body}"
            );
        }

        // ── Issue #542: dry run through the real route ──────────────────────

        /// A runner that completes immediately, returning one node row — enough
        /// to prove the route maps `WorkflowRun.nodes` onto the response and
        /// echoes the request's `dry_run` as the discriminator.
        struct EchoRunner;

        #[async_trait::async_trait]
        impl WorkflowRunner for EchoRunner {
            async fn run(
                &self,
                _company: &CompanyId,
                _workflow: &crate::company::WorkflowFile,
                _input: serde_json::Value,
                _ctx: &WorkflowRunContext,
            ) -> crate::Result<WorkflowRun> {
                Ok(WorkflowRun {
                    output: serde_json::json!({ "run": {}, "nodes": {} }),
                    pending_approvals: Vec::new(),
                    deliveries: Vec::new(),
                    cancelled: false,
                    nodes: vec![crate::ports::WorkflowRunNodeRow {
                        node_id: "done".to_string(),
                        status: crate::ports::types::WorkflowNodeStatus::Ok,
                        elapsed_ms: 3,
                    }],
                    notices: Vec::new(),
                    board: Vec::new(),
                    blocked_nodes: Vec::new(),
                    approvals: Vec::new(),
                })
            }
        }

        /// A hosted company whose runner echoes immediately.
        async fn echo_company(home: &std::path::Path) -> axum::Router {
            let manifest: CompanyManifest =
                toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
            let id = CompanyId::new("acme");
            FsCompanyStore::new(home.to_path_buf())
                .save(&CompanyRecord {
                    id: id.clone(),
                    manifest: manifest.clone(),
                    ledger: Vec::new(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                        id: "demo".to_string(),
                        toml: GRAPH.to_string(),
                    }],
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    lifecycle: "running".to_string(),
                    template_provenance: None,
                })
                .await
                .unwrap();
            let mut runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            runtime.set_workflow_runner(Arc::new(EchoRunner));
            let state = AppState::new(AppConfig::default());
            state.registry().insert(id.clone(), Arc::new(runtime));
            crate::server::test_support::seed_fixed_admin(&state, "acme").await;
            router(state)
        }

        /// T8 — `{"dry_run":true}` answers 200 carrying `dryRun:true` and the
        /// per-node `nodes`; a plain body carries neither `dryRun` (a real run's
        /// shape an old host would produce) — the presence discriminator the
        /// console reads instead of trusting what it asked for.
        #[tokio::test]
        async fn dry_run_request_echoes_the_marker_and_nodes_a_plain_body_omits_it() {
            let home_dir = home();
            let app = echo_company(home_dir.path()).await;

            let dry = app
                .clone()
                .oneshot(run_request(serde_json::json!({ "dry_run": true })))
                .await
                .unwrap();
            assert_eq!(dry.status(), StatusCode::OK);
            let body = json_body(dry).await;
            assert_eq!(body["dryRun"], serde_json::json!(true), "{body}");
            assert_eq!(body["nodes"][0]["nodeId"], "done", "{body}");
            assert_eq!(body["nodes"][0]["status"], "ok", "{body}");

            let plain = app
                .oneshot(run_request(serde_json::json!({})))
                .await
                .unwrap();
            assert_eq!(plain.status(), StatusCode::OK);
            let body = json_body(plain).await;
            assert!(
                body.get("dryRun").is_none(),
                "a real run must carry no dryRun key: {body}"
            );
            // The node trail rides every settled run, dry or not.
            assert_eq!(body["nodes"][0]["nodeId"], "done", "{body}");
        }
    }
}
