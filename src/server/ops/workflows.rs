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
//! [`record_run_finished`] — the same helper the cron
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
//! Execution is dependency-inverted behind the [`WorkflowRunner`] port: when no
//! runner is wired (the default build, or a runtime built without a harness) the
//! run route reports `not_wired` — the same 404 seam the DNS/SMTP surfaces use —
//! so the default build stays inert. The read routes need no runner: they only
//! parse the saved graphs, so the console can list and render workflows even on
//! a build that cannot execute them.

use std::collections::HashSet;
use std::path::Path as FsPath;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::company::{
    RawEdge, RawNode, RawWorkflow, WorkflowDestinationDef, WorkflowEdgeDef, WorkflowFile,
    WorkflowNodeDef, WorkflowRetryDef, create_company_workflow, delete_company_workflow,
    list_workflows_union, load_workflow_union, seed_file_exists, update_company_workflow,
    workflow_version,
};
use crate::error::OpenCompanyError;
use crate::ports::types::{CompanyEvent, CompanyRecord, EventSeq, OverlayWorkflow};
use crate::runtime::cron::{CivilTime, CronExpr};
use crate::runtime::record_run_finished;
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
}

impl WorkflowSummary {
    fn new(f: WorkflowFile, editable: bool) -> Self {
        Self {
            id: f.id,
            name: f.name,
            description: f.description,
            editable,
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
    /// The opaque optimistic-concurrency token for this graph (issue #259),
    /// present only when `editable` (a source-defined graph has nothing to
    /// version, and a token for an overlay body the read path does not even
    /// serve would be actively misleading).
    ///
    /// The contract is **echo it back**: hand it to `PUT` in the body or to
    /// `DELETE` as `?expectedVersion=`, and the write is refused with a `409` if
    /// the graph moved in between. Never parse or derive it.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

impl WorkflowGraph {
    fn new(f: WorkflowFile, editable: bool, version: Option<String>) -> Self {
        Self {
            id: f.id,
            name: f.name,
            description: f.description,
            nodes: f.nodes.into_iter().map(WorkflowNode::from).collect(),
            edges: f.edges.into_iter().map(WorkflowEdge::from).collect(),
            editable,
            version,
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

async fn overlay_workflows(company: &ScopedCompany) -> Result<Vec<OverlayWorkflow>, ApiError> {
    let record: Option<CompanyRecord> = company
        .runtime
        .store()
        .load(company.id())
        .await
        .map_err(ApiError)?;
    Ok(record.map(|r| r.overlay_workflows).unwrap_or_default())
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
    let overlays = overlay_workflows(&company).await?;
    let source_dir = company.runtime.source_dir();
    let files = list_workflows_union(source_dir, &overlays);
    let mut seen: HashSet<String> = files.iter().map(|f| f.id.clone()).collect();
    let mut summaries: Vec<WorkflowSummary> = files
        .into_iter()
        .map(|f| {
            let editable = is_editable(source_dir, &overlays, &f.id);
            WorkflowSummary::new(f, editable)
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
    let overlays = overlay_workflows(&company).await?;
    let file = load_workflow_union(source_dir, &overlays, &wid)
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))))?;
    // Issue #259: the version token rides out with the graph, so the console
    // gets it for free on the same read it renders from — there is no second
    // round trip for a caller to skip and thereby lose the concurrency guard.
    let editable = is_editable(source_dir, &overlays, &wid);
    let version = editable
        .then(|| overlay_toml(&overlays, &wid).map(workflow_version))
        .flatten();
    Ok(Json(WorkflowGraph::new(file, editable, version)))
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
    let overlays = overlay_workflows(company).await?;
    let editable = is_editable(source_dir, &overlays, &file.id);
    let version = editable
        .then(|| overlay_toml(&overlays, &file.id).map(workflow_version))
        .flatten();
    Ok(WorkflowGraph::new(file, editable, version))
}

/// The `PUT …/workflows/{wid}` body: the same camelCase graph shape the read and
/// create routes speak, plus the optional concurrency token.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateWorkflowBody {
    #[serde(flatten)]
    graph: CreateWorkflowBody,
    /// The token from the `GET`/`PUT` this edit was based on. Omit for an
    /// unconditional write (the `curl` path); the console always sends it.
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
/// Statuses: `400` (bad graph, or `id` ≠ `wid`), `404` (unknown id), `409`
/// (source-defined, body-less, name taken, or a stale `expectedVersion`).
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

    let expected = body.expected_version.clone();
    let draft = RawWorkflow::try_from(body.graph)?;
    let file = update_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        Some(company.runtime.events()),
        draft,
        expected.as_deref(),
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
/// `204` on success. `404` for an unknown id; `409` for a source-defined or
/// body-less id, or a stale `expectedVersion`.
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
    delete_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        Some(company.runtime.events()),
        &wid,
        query.expected_version.as_deref(),
    )
    .await
    .map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
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
}

/// `POST …/workflows/{wid}/run` (both scope forms).
async fn run_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    body: Option<Json<RunWorkflowBody>>,
) -> Result<Json<RunWorkflowResponse>, Response> {
    // No runner wired. Two very different causes look identical from here, and
    // `not_wired` only describes the first (issue #266):
    //   1. this build/deployment has no workflow execution at all — nothing the
    //      operator can do, so "not wired in this deployment" is the truth;
    //   2. this *boot* has none, because the company started with no inference
    //      source. The runner is populated from the harness arm at build time, so
    //      configuring inference afterwards leaves it `None` until a restart —
    //      and a `not_wired` 404 sends the operator hunting a deployment problem
    //      that does not exist.
    let Some(runner) = company.runtime.workflow_runner() else {
        if super::inference::restart_pending_for(company.runtime.as_ref()).await {
            return Err(super::restart_required("workflow execution"));
        }
        return Err(super::not_wired("workflow execution"));
    };

    // `wid` becomes a filename — reject anything that could escape `workflows/`.
    if !safe_wid(&wid) {
        return Err(
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response(),
        );
    }

    // Load the saved graph from the seed ∪ overlay union, so a graph created on
    // a hosted tenant (no source directory) runs the same as a committed one.
    let overlays = overlay_workflows(&company)
        .await
        .map_err(IntoResponse::into_response)?;
    let file = load_workflow_union(company.runtime.source_dir(), &overlays, &wid)
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response()
        })?;

    let input = body.map(|Json(b)| b.input).unwrap_or(Value::Null);
    let run = match runner.run(company.id(), &file, input).await {
        Ok(run) => run,
        Err(err) => {
            // Issue #228: a manual run that dies is journaled too, before the
            // error goes back. The caller sees the 5xx and may well close the
            // tab; the record is what is still there tomorrow.
            record_run_finished(
                company.runtime.events(),
                company.id(),
                &wid,
                false,
                Err(err.to_string().as_str()),
            )
            .await;
            return Err(ApiError(err).into_response());
        }
    };

    // Issue #228: journal the outcome so a manual run's delivery rows stop being
    // drawer-transient. Until now they lived in the console's run panel until it
    // was dismissed and then existed nowhere — the operator could not answer
    // "did that report actually go out?" an hour later. Recorded through the
    // same helper the scheduler uses, so history is uniform whatever started the
    // run. Best-effort: the response below is returned either way.
    record_run_finished(
        company.runtime.events(),
        company.id(),
        &wid,
        false,
        Ok(&run),
    )
    .await;

    Ok(Json(RunWorkflowResponse {
        output: run.output,
        pending_approvals: run.pending_approvals,
        deliveries: run.deliveries,
    }))
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

    let mut runs: Vec<WorkflowRunOutcome> = stored
        .into_iter()
        .filter_map(|stored| {
            let CompanyEvent::WorkflowRunFinished {
                workflow_id,
                scheduled,
                run_id,
                deliveries,
                pending_approvals,
                error,
            } = stored.event
            else {
                return None;
            };
            // The `?workflow=` filter is applied here rather than after the
            // `limit` cut, so asking for one workflow returns that workflow's
            // most recent N — not "whichever of the last N happen to match".
            if query
                .workflow
                .as_deref()
                .is_some_and(|wanted| wanted != workflow_id)
            {
                return None;
            }
            Some(WorkflowRunOutcome {
                seq: stored.seq.value(),
                at_millis: stored.at_millis,
                workflow_id,
                scheduled,
                run_id,
                deliveries,
                pending_approvals,
                error,
            })
        })
        .collect();

    // Newest first: a history panel leads with the run that just happened.
    runs.reverse();
    runs.truncate(limit);
    Ok(Json(runs))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .map(|f| WorkflowSummary::new(f, false))
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
        let graph = WorkflowGraph::new(file, false, None);

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
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None)).unwrap();
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

    #[test]
    fn json_serializes_p1_node_fields_in_camelcase() {
        use crate::company::{WorkflowNodeDef, WorkflowNodeKind, WorkflowRetryDef};

        let file = WorkflowFile {
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
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None)).unwrap();
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
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None)).unwrap();
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
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None)).unwrap();
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
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None)).unwrap();
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
        let json = serde_json::to_value(WorkflowGraph::new(file, false, None)).unwrap();
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
        })
        .unwrap();
        assert_eq!(json["deliveries"], serde_json::json!([]));
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
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use super::super::{CompanyEvent, DEFAULT_RUN_LIMIT};
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
            let items = body.as_array().expect("array response");
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
            let items = listed.as_array().expect("array");
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
            let items = listed.as_array().expect("array");
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
            assert_eq!(items.as_array().unwrap().len(), 1, "{items}");
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

        /// Omitting the token is an unconditional write — the `curl` contract.
        #[tokio::test]
        async fn an_edit_without_a_token_is_unconditional() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_greeter(&state).await;

            let response = router(state)
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(None)),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
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
            assert_eq!(items.as_array().unwrap().len(), 1, "{items}");
            assert_eq!(items[0]["id"], "greeter");
        }

        #[tokio::test]
        async fn editing_an_unknown_workflow_is_not_found() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;

            let mut body = edited_body(None);
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
            create_greeter(&state).await;

            // No trigger node at all.
            let body = serde_json::json!({
                "id": "greeter",
                "name": "Greeter",
                "nodes": [ { "id": "done", "kind": "output", "name": "Report" } ],
                "edges": []
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
            assert_eq!(items.as_array().unwrap().len(), 0, "{items}");

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
                items.as_array().unwrap().len(),
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
            create_greeter(&state).await;
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
                .oneshot(request("DELETE", "/api/v1/company/workflows/greeter", None))
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

            // Someone edits after the console loaded the graph.
            let edited = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/company/workflows/greeter",
                    Some(edited_body(None)),
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

            let response = router(state)
                .oneshot(request("DELETE", "/api/v1/company/workflows/ghost", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        /// The write verbs are reachable under the platform scope form too, not
        /// just the prosumer alias.
        #[tokio::test]
        async fn edit_and_delete_serve_both_scope_forms() {
            let home_dir = home();
            let home = home_dir.path().to_path_buf();
            let (state, _store, _id) = hosted_state(&home).await;
            create_greeter(&state).await;

            let response = router(state.clone())
                .oneshot(request(
                    "PUT",
                    "/api/v1/companies/acme/workflows/greeter",
                    Some(edited_body(None)),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let response = router(state)
                .oneshot(request(
                    "DELETE",
                    "/api/v1/companies/acme/workflows/greeter",
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

            // And the host agrees when actually asked to delete it.
            let response = router(state)
                .oneshot(request("DELETE", "/api/v1/company/workflows/legacy", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
        }
    }
}
