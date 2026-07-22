//! Workflow surfaces: read the company's saved graphs (`GET /workflows`,
//! `GET /workflows/{wid}`) and run one (`POST /workflows/{wid}/run`) under both
//! scope forms.
//!
//! Graphs are loaded from the company's on-disk source directory
//! (`companies/<name>/workflows/<wid>.toml`) via
//! [`load_company_workflows`](crate::company::load_company_workflows), which
//! takes an explicit id list (it never scans) — so `list_workflows` enumerates
//! the `workflows/` directory itself to build that list.
//!
//! Execution is dependency-inverted behind the [`WorkflowRunner`] port: when no
//! runner is wired (the default build, or a runtime built without a harness) the
//! run route reports `not_wired` — the same 404 seam the DNS/SMTP surfaces use —
//! so the default build stays inert. The read routes need no runner: they only
//! parse the committed graph files, so the console can list and render workflows
//! even on a build that cannot execute them.

use std::path::Path as FsPath;

use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::company::{WorkflowEdgeDef, WorkflowFile, WorkflowNodeDef, load_company_workflows};
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workflow route fragment: list + graph reads and the run write.
pub fn router() -> Router<AppState> {
    scoped("/workflows", get(list_workflows))
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
/// mode) or no `workflows/` directory yields an empty list — a `200`, not an
/// error, so the console renders "no workflows yet" rather than a failure.
async fn list_workflows(company: ScopedCompany) -> Result<Json<Vec<WorkflowSummary>>, ApiError> {
    let files = load_source_workflows(company.runtime.source_dir())?;
    Ok(Json(files.into_iter().map(WorkflowSummary::from).collect()))
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
}
