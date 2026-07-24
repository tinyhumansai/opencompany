//! Workflow surfaces: create a graph (`POST /workflows`), read the company's
//! saved graphs (`GET /workflows`, `GET /workflows/{wid}`), and run one
//! (`POST /workflows/{wid}/run`) — under both scope forms.
//!
//! Graphs are loaded from the company's on-disk source directory
//! (`companies/<name>/workflows/<wid>.toml`) via
//! [`load_company_workflows`](crate::company::load_company_workflows), which
//! takes an explicit id list (it never scans) — so `list_workflows` enumerates
//! the `workflows/` directory itself to build that list.
//!
//! A platform-provisioned tenant has no source directory, so there is nothing
//! to scan — but it can still declare `[workflows].enabled` ids in its
//! manifest. `list_workflows` unions those manifest-enabled ids in (deduped by
//! id) so the console's picker isn't empty just because the deployment has no
//! `workflows/*.toml` files on disk, mirroring the `Company.workflows`
//! GraphQL resolver. Where the definition body isn't available to load (no
//! source directory, or the id has no matching file), the summary falls back
//! to the id as its name — the same fallback the GraphQL resolver uses. Full
//! graphs (`GET …/workflows/{wid}`) still require a source directory, since
//! there is currently no other place a graph body can come from.
//!
//! Creation (issue #69) writes a new `workflows/<id>.toml` into that same
//! source directory — reusing [`parse_workflow`](crate::company::parse_workflow)
//! to validate the graph a caller posts before anything touches disk — and
//! records the id as enabled on the operator's live [`CompanyRecord`], mirroring
//! the team overlay convention: the version-controlled `company.toml` is never
//! rewritten (see `crate::server::ops::team`). A deployment with no source
//! directory (platform-provisioned mode with nothing seeded on disk yet) has
//! nowhere to write the graph file, so creation is refused with a 4xx rather
//! than crashing.
//!
//! Execution is dependency-inverted behind the [`WorkflowRunner`] port: when no
//! runner is wired (the default build, or a runtime built without a harness) the
//! run route reports `not_wired` — the same 404 seam the DNS/SMTP surfaces use —
//! so the default build stays inert. The read routes need no runner: they only
//! parse the committed graph files, so the console can list and render workflows
//! even on a build that cannot execute them.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path as FsPath;

use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::company::{
    RawEdge, RawNode, RawWorkflow, WorkflowEdgeDef, WorkflowFile, WorkflowNodeDef,
    load_company_workflows, parse_workflow, render_workflow,
};
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workflow route fragment: create + list, one graph read, and the
/// run write.
pub fn router() -> Router<AppState> {
    scoped("/workflows", post(create_workflow).get(list_workflows))
        .merge(scoped("/workflows/{wid}", get(get_workflow)))
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
}

impl From<WorkflowFile> for WorkflowSummary {
    fn from(f: WorkflowFile) -> Self {
        Self {
            id: f.id,
            name: f.name,
            description: f.description,
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
}

impl From<WorkflowFile> for WorkflowGraph {
    fn from(f: WorkflowFile) -> Self {
        Self {
            id: f.id,
            name: f.name,
            description: f.description,
            nodes: f.nodes.into_iter().map(WorkflowNode::from).collect(),
            edges: f.edges.into_iter().map(WorkflowEdge::from).collect(),
        }
    }
}

/// A single graph node. `kind` is the on-disk string
/// (`trigger`/`agent`/`tool_call`/`http_request`/`condition`/`output`); `agent`
/// is only meaningful on `agent` nodes.
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
}

impl From<WorkflowNodeDef> for WorkflowNode {
    fn from(n: WorkflowNodeDef) -> Self {
        Self {
            id: n.id,
            kind: n.kind.as_str().to_string(),
            name: n.name,
            summary: n.summary,
            agent: n.agent,
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

/// `GET …/workflows` — the company's saved workflows as picker summaries.
///
/// The loader takes an explicit id list rather than scanning, so this reads the
/// company's `workflows/` directory to collect the `*.toml` file stems as ids,
/// then loads and summarizes them. No source directory (platform-provisioned
/// mode) or no `workflows/` directory yields an empty filesystem scan — but not
/// necessarily an empty response: the manifest's `[workflows].enabled` ids are
/// unioned in (deduped against the filesystem scan by id), falling back to the
/// id as the name when there's no file to load a real name from. Only when
/// both the scan and the manifest are empty does this return `200 []`, so the
/// console renders "no workflows yet" rather than a failure.
async fn list_workflows(company: ScopedCompany) -> Result<Json<Vec<WorkflowSummary>>, ApiError> {
    let source_dir = company.runtime.source_dir();
    let files = load_source_workflows(source_dir)?;
    let mut seen: HashSet<String> = files.iter().map(|f| f.id.clone()).collect();
    let mut summaries: Vec<WorkflowSummary> =
        files.into_iter().map(WorkflowSummary::from).collect();

    let enabled_ids = company
        .runtime
        .enabled_workflow_ids()
        .await
        .map_err(ApiError)?;
    for id in enabled_ids {
        // Already present from the filesystem scan — skip so hosted mode
        // (source dir present, manifest also lists the same ids) doesn't
        // double-list.
        if !seen.insert(id.clone()) {
            continue;
        }
        let loaded = source_dir
            .and_then(|dir| load_company_workflows(dir, std::slice::from_ref(&id)).ok())
            .and_then(|mut files| files.pop());
        summaries.push(match loaded {
            Some(file) => WorkflowSummary::from(file),
            None => WorkflowSummary {
                id: id.clone(),
                name: id,
                description: None,
            },
        });
    }

    Ok(Json(summaries))
}

/// `GET …/workflows/{wid}` — the full graph for one workflow.
///
/// An unknown `wid` (or a deployment with no source directory) is a `404`,
/// mirroring the sub-resource-not-found shape the task routes use.
async fn get_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    // `wid` becomes a filename — reject anything that could escape `workflows/`.
    if !safe_wid(&wid) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    let source_dir = company
        .runtime
        .source_dir()
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))))?;
    // Only try to load ids that exist on disk, so a missing file is a clean 404
    // rather than the loader's `DataRead` (a 500).
    let path = source_dir.join("workflows").join(format!("{wid}.toml"));
    if !path.is_file() {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workflow {wid}"
        ))));
    }
    let file = load_company_workflows(source_dir, std::slice::from_ref(&wid))
        .map_err(ApiError)?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))))?;
    Ok(Json(WorkflowGraph::from(file)))
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
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateEdge {
    from: String,
    to: String,
    #[serde(default)]
    label: Option<String>,
}

impl From<CreateWorkflowBody> for RawWorkflow {
    fn from(body: CreateWorkflowBody) -> Self {
        Self {
            id: body.id,
            name: body.name,
            description: body.description,
            nodes: body
                .nodes
                .into_iter()
                .map(|n| RawNode {
                    id: n.id,
                    kind: n.kind,
                    name: n.name,
                    summary: n.summary,
                    agent: n.agent,
                })
                .collect(),
            edges: body
                .edges
                .into_iter()
                .map(|e| RawEdge {
                    from: e.from,
                    to: e.to,
                    label: e.label,
                })
                .collect(),
        }
    }
}

/// `POST …/workflows` — authors a new workflow graph (issue #69): the console's
/// form creator, or any direct API caller, posts the graph shape and it lands
/// as a new `workflows/<id>.toml` in the company's source directory.
///
/// Order of checks, each returning an actionable 4xx before anything is
/// written: the id must be a safe filename, the deployment must have a source
/// directory to write into, the id must not already be taken (409), every
/// `agent` node must name a real roster teammate, and the graph must pass the
/// same structural validation ([`parse_workflow`]) a hand-authored file would
/// (at least one trigger, unique node ids, edges that reference real nodes, no
/// stray `agent` field on a non-agent node).
async fn create_workflow(
    company: ScopedCompany,
    Json(body): Json<CreateWorkflowBody>,
) -> Result<Json<WorkflowGraph>, ApiError> {
    if !safe_wid(&body.id) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            language::WORKFLOW_ID_INVALID.to_string(),
        )));
    }

    let source_dir = company.runtime.source_dir().ok_or_else(|| {
        ApiError(OpenCompanyError::InvalidRequest(
            language::WORKFLOW_NEEDS_SOURCE_DIR.to_string(),
        ))
    })?;

    let workflows_dir = source_dir.join("workflows");
    let path = workflows_dir.join(format!("{}.toml", body.id));

    std::fs::create_dir_all(&workflows_dir).map_err(|source| {
        ApiError(OpenCompanyError::StoreIo {
            path: workflows_dir.clone(),
            source,
        })
    })?;

    // `parse_workflow` only rejects zero triggers (a saved graph may legally
    // have more, e.g. multiple entry points); the creator is stricter — a
    // freshly authored graph must name exactly one starting point.
    let trigger_count = body.nodes.iter().filter(|n| n.kind == "trigger").count();
    if trigger_count != 1 {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "a workflow needs exactly one `trigger` node to say what starts it (found {trigger_count})."
        ))));
    }

    // Cross-check every `agent` node against the company's effective roster
    // (manifest agents + operator overlay teammates) before writing anything.
    // `parse_workflow` validates the graph's own shape but has no roster to
    // check names against.
    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    let roster: HashSet<&str> = record
        .manifest
        .agents
        .iter()
        .map(|a| a.id.as_str())
        .chain(record.overlay_agents.iter().map(|a| a.id.as_str()))
        .collect();
    for node in &body.nodes {
        if node.kind != "agent" {
            continue;
        }
        match node.agent.as_deref() {
            Some(agent_id) if roster.contains(agent_id) => {}
            Some(agent_id) => {
                return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                    "node `{}` names teammate `{agent_id}`, which is not on this company's roster.",
                    node.id
                ))));
            }
            None => {
                return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                    "node `{}` is an agent node but names no teammate.",
                    node.id
                ))));
            }
        }
    }

    // Save `body.id` before `body` is consumed by `Into<RawWorkflow>` below.
    let body_id = body.id.clone();

    // Render the candidate graph to TOML and reuse `parse_workflow` to
    // validate its structure end to end — the same rules a hand-authored
    // `workflows/<id>.toml` must satisfy. Any problem becomes a 400, never the
    // 500 a malformed on-disk file gets from the read routes.
    let toml_src = render_workflow(&body.into())?;
    let file = parse_workflow(&toml_src).map_err(|err| match err {
        OpenCompanyError::DataInvalid { problems, .. } => {
            ApiError(OpenCompanyError::InvalidRequest(problems.join(" ")))
        }
        OpenCompanyError::DataParse { message, .. } => {
            ApiError(OpenCompanyError::InvalidRequest(message))
        }
        other => ApiError(other),
    })?;

    // Write the file atomically: `create_new(true)` fails if the path already
    // exists, closing the TOCTOU gap between a separate `is_file()` check and
    // `fs::write`. If `write_all` fails, clean up the empty file so the id is
    // not permanently blocked. Also, save the store record **after** the file
    // lands so a store failure doesn't orphan a file — if the save fails the
    // file is cleaned up and the caller can retry.
    let mut wf_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => ApiError(OpenCompanyError::Conflict(format!(
                "A workflow named `{}` already exists.",
                body_id
            ))),
            _ => ApiError(OpenCompanyError::StoreIo {
                path: path.clone(),
                source: e,
            }),
        })?;
    wf_file.write_all(toml_src.as_bytes()).map_err(|source| {
        let _ = std::fs::remove_file(&path);
        ApiError(OpenCompanyError::StoreIo {
            path: path.clone(),
            source,
        })
    })?;
    drop(wf_file);

    // Record the id as enabled on the operator's live copy of the manifest —
    // mirrors the team overlay: the version-controlled `company.toml` on disk
    // is never rewritten (see `crate::server::ops::team`).
    let save_result = if !record
        .manifest
        .workflows
        .enabled
        .iter()
        .any(|e| e == &file.id)
    {
        record.manifest.workflows.enabled.push(file.id.clone());
        company.runtime.store().save(&record).await
    } else {
        Ok(())
    };

    // If the store save failed, remove the file we just wrote so a retry can
    // succeed without admin intervention.
    if let Err(e) = save_result {
        let _ = std::fs::remove_file(&path);
        return Err(ApiError(e));
    }

    Ok(Json(WorkflowGraph::from(file)))
}

/// Loads every saved workflow under `source_dir/workflows/`, or an empty list
/// when there is no source directory or no `workflows/` directory.
fn load_source_workflows(source_dir: Option<&FsPath>) -> Result<Vec<WorkflowFile>, ApiError> {
    let Some(source_dir) = source_dir else {
        return Ok(Vec::new());
    };
    let Ok(entries) = std::fs::read_dir(source_dir.join("workflows")) else {
        return Ok(Vec::new());
    };
    let mut ids: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect();
    // Stable, deterministic order for the picker.
    ids.sort();
    // Load each id on its own so one malformed `workflows/*.toml` skips only
    // itself instead of 500-ing the whole picker.
    let mut files = Vec::with_capacity(ids.len());
    for id in &ids {
        match load_company_workflows(source_dir, std::slice::from_ref(id)) {
            Ok(loaded) => files.extend(loaded),
            Err(err) => tracing::warn!(workflow = %id, error = %err, "skipping malformed workflow"),
        }
    }
    Ok(files)
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
}

/// `POST …/workflows/{wid}/run` (both scope forms).
async fn run_workflow(
    company: ScopedCompany,
    Path(WorkflowPath { wid }): Path<WorkflowPath>,
    body: Option<Json<RunWorkflowBody>>,
) -> Result<Json<RunWorkflowResponse>, Response> {
    // No runner wired (default build / no harness) → the same "not wired" seam
    // the networked surfaces use.
    let Some(runner) = company.runtime.workflow_runner() else {
        return Err(super::not_wired("workflow execution"));
    };

    // `wid` becomes a filename — reject anything that could escape `workflows/`.
    if !safe_wid(&wid) {
        return Err(
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response(),
        );
    }

    // Load the saved graph from the company's on-disk source directory. Without
    // one (platform-provisioned mode) there is nothing to run.
    let source_dir = company.runtime.source_dir().ok_or_else(|| {
        super::not_wired("workflow source (no company definition directory on this deployment)")
    })?;
    let file = load_company_workflows(source_dir, std::slice::from_ref(&wid))
        .map_err(|e| ApiError(e).into_response())?
        .into_iter()
        .next()
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(format!("workflow {wid}"))).into_response()
        })?;

    let input = body.map(|Json(b)| b.input).unwrap_or(Value::Null);
    let run = runner
        .run(company.id(), &file, input)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    Ok(Json(RunWorkflowResponse {
        output: run.output,
        pending_approvals: run.pending_approvals,
    }))
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

    #[test]
    fn list_returns_a_summary_per_saved_workflow() {
        let dir = seed_demo();
        let files = load_source_workflows(Some(dir.path())).expect("lists");
        let summaries: Vec<WorkflowSummary> =
            files.into_iter().map(WorkflowSummary::from).collect();
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
        let ids = ["demo".to_string()];
        let file = load_company_workflows(dir.path(), &ids)
            .expect("loads")
            .into_iter()
            .next()
            .expect("one file");
        let graph = WorkflowGraph::from(file);

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
    fn no_source_dir_lists_empty() {
        assert!(load_source_workflows(None).unwrap().is_empty());
    }

    #[test]
    fn no_workflows_dir_lists_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_source_workflows(Some(dir.path())).unwrap().is_empty());
    }

    #[test]
    fn json_serializes_camelcase_and_omits_empty_options() {
        let dir = seed_demo();
        let ids = ["demo".to_string()];
        let file = load_company_workflows(dir.path(), &ids)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let json = serde_json::to_value(WorkflowGraph::from(file)).unwrap();
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
        let files = load_source_workflows(Some(dir.path())).expect("lists despite a bad file");
        let ids: Vec<_> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["demo"]);
    }

    // HTTP-level: a hosted tenant has no source directory to scan, so these
    // exercise the manifest-enabled union path end to end via the router.
    mod hosted_mode {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::company::CompanyManifest;
        use crate::ports::CompanyStore;
        use crate::ports::types::{CompanyId, CompanyRecord};
        use crate::runtime::RuntimeBuilder;
        use crate::server::router;
        use crate::store::FsCompanyStore;
        use crate::{AppConfig, AppState};

        fn home() -> std::path::PathBuf {
            std::env::temp_dir().join(format!(
                "oc-workflows-hosted-{}",
                crate::ports::generate_id()
            ))
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
            let home = home();
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

            std::fs::remove_dir_all(&home).ok();
        }
    }
}
