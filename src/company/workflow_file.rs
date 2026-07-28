//! Workflow graph files: `companies/<name>/workflows/<id>.toml`.
//!
//! Each enabled workflow is a data-only node/edge graph edited by the Workflow
//! canvas and referenced by `[workflows].enabled` in the manifest. This module
//! parses those files into a validated [`WorkflowFile`], reporting every problem
//! at once in prosumer language, matching [`super::manifest`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{OpenCompanyError, Result};

/// The node kinds a workflow graph may use, mirroring the tinyflows model. The
/// first six are the original OpenCompany set; the trailing six (P2) complete
/// the tinyflows node catalog: data-shape nodes (`switch` / `merge` /
/// `split_out` / `transform` / `output_parser`) and `sub_workflow` composition.
/// Each string is tinyflows' snake_case wire kind verbatim.
pub const WORKFLOW_NODE_KINDS: &[&str] = &[
    "trigger",
    "agent",
    "tool_call",
    "http_request",
    "condition",
    "output",
    "switch",
    "merge",
    "split_out",
    "transform",
    "output_parser",
    "sub_workflow",
];

/// A node kind in a workflow graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowNodeKind {
    /// Entry point — an event that starts the workflow.
    Trigger,
    /// A roster teammate performs a step.
    Agent,
    /// An automated tool call.
    ToolCall,
    /// An outbound HTTP request.
    HttpRequest,
    /// A branch on some condition.
    Condition,
    /// A terminal report-back node.
    Output,
    /// A multi-way branch: each outgoing edge label is a case name the engine
    /// matches against the routed value (like a `match`).
    Switch,
    /// Fan-in: concatenates the items arriving on its inputs into one stream.
    Merge,
    /// Fan-out: splits a list-valued item into one item per element.
    SplitOut,
    /// Reshapes items via `=expr` bindings evaluated by the engine.
    Transform,
    /// Parses/validates an upstream item against a schema (optionally LLM
    /// auto-fixing a malformed value).
    OutputParser,
    /// Runs another saved workflow (referenced by id) as a nested step.
    SubWorkflow,
}

impl WorkflowNodeKind {
    /// The on-disk `kind` string for this node kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Agent => "agent",
            Self::ToolCall => "tool_call",
            Self::HttpRequest => "http_request",
            Self::Condition => "condition",
            Self::Output => "output",
            Self::Switch => "switch",
            Self::Merge => "merge",
            Self::SplitOut => "split_out",
            Self::Transform => "transform",
            Self::OutputParser => "output_parser",
            Self::SubWorkflow => "sub_workflow",
        }
    }

    /// Parses an on-disk `kind` string, returning `None` for unknown kinds.
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "trigger" => Some(Self::Trigger),
            "agent" => Some(Self::Agent),
            "tool_call" => Some(Self::ToolCall),
            "http_request" => Some(Self::HttpRequest),
            "condition" => Some(Self::Condition),
            "output" => Some(Self::Output),
            "switch" => Some(Self::Switch),
            "merge" => Some(Self::Merge),
            "split_out" => Some(Self::SplitOut),
            "transform" => Some(Self::Transform),
            "output_parser" => Some(Self::OutputParser),
            "sub_workflow" => Some(Self::SubWorkflow),
            _ => None,
        }
    }
}

/// A parsed and validated workflow graph.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowFile {
    /// Workflow id — matches the `workflows/<id>.toml` filename.
    pub id: String,
    /// Human-readable workflow name.
    pub name: String,
    /// What the workflow does.
    pub description: Option<String>,
    /// Graph nodes, in file order.
    pub nodes: Vec<WorkflowNodeDef>,
    /// Directed edges between nodes, in file order.
    pub edges: Vec<WorkflowEdgeDef>,
}

/// A single node in a workflow graph.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowNodeDef {
    /// Node id, unique within the graph.
    pub id: String,
    /// The node kind.
    pub kind: WorkflowNodeKind,
    /// Human-readable node name.
    pub name: String,
    /// A short description of what the node does.
    pub summary: Option<String>,
    /// The roster agent id — only meaningful on `agent` nodes.
    pub agent: Option<String>,
    /// Free-form, kind-specific node configuration ([`tool_call`] slug/args,
    /// [`http_request`] descriptor, …). Layered under the derived defaults and
    /// the first-class fields below by [`translate`](crate::workflows::translate)
    /// before it reaches the engine. Reserved keys (`on_error` / `retry` /
    /// `requires_approval`, plus `agent_ref` on `agent` nodes) are rejected at
    /// validation so they cannot silently shadow a first-class field.
    ///
    /// [`tool_call`]: WorkflowNodeKind::ToolCall
    /// [`http_request`]: WorkflowNodeKind::HttpRequest
    pub config: Option<serde_json::Value>,
    /// Per-node error policy once retries are exhausted: `stop` (default — fail
    /// the run), `continue` (turn the failure into a data item on the default
    /// port), or `route` (emit the failure on the `error` port for a recovery
    /// sub-graph). The tinyflows engine reads this from node config.
    pub on_error: Option<String>,
    /// Per-node retry policy (attempt count + backoff) the engine honors.
    pub retry: Option<WorkflowRetryDef>,
    /// When `true`, the node pauses awaiting operator approval before it runs —
    /// the engine surfaces it on `WorkflowRun.pending_approvals`.
    pub requires_approval: Option<bool>,
}

/// A node's typed retry policy. Mirrors the free-form `retry.*` keys the
/// tinyflows engine reads from node config (`max_attempts` / `backoff_ms` /
/// `backoff`); carried as first-class model data so the console and validation
/// see one shape.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct WorkflowRetryDef {
    /// Total attempts (≥ 1). The engine bounds retries at `max_attempts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Base delay between attempts in milliseconds (default `0` = no wait).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
    /// Backoff curve: `fixed` (default, constant delay) or `exponential`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<String>,
}

/// A directed edge between two nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowEdgeDef {
    /// Source node id.
    pub from: String,
    /// Destination node id.
    pub to: String,
    /// Optional branch label (e.g. `yes` / `no` on a condition).
    pub label: Option<String>,
}

/// Serde-facing shape of the workflow TOML. Enum-like `kind` is read as a plain
/// string and validated so errors read in prosumer language, not serde traces.
///
/// Also carries `Serialize` (`pub(crate)` fields) so the workflow creator
/// (issue #69) can render a candidate graph straight back to this same shape
/// and re-parse it through [`parse_workflow`] for validation before writing
/// anything to disk — see [`render_workflow`].
#[derive(Deserialize, Serialize)]
pub(crate) struct RawWorkflow {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, rename = "node")]
    pub(crate) nodes: Vec<RawNode>,
    #[serde(default, rename = "edge")]
    pub(crate) edges: Vec<RawEdge>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct RawNode {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    /// Free-form node config, read as a TOML value (not `serde_json`) so the
    /// `Serialize` half — used by the workflow creator's
    /// [`render_workflow`] round-trip — stays representable in TOML (TOML has no
    /// `null`). Converted to `serde_json::Value` on the way into
    /// [`WorkflowNodeDef::config`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) on_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retry: Option<WorkflowRetryDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) requires_approval: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct RawEdge {
    #[serde(default)]
    pub(crate) from: String,
    #[serde(default)]
    pub(crate) to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
}

/// Renders a candidate graph as on-disk workflow TOML — the inverse of
/// [`parse_workflow`]. Used by the console's workflow creator (issue #69): the
/// caller builds a [`RawWorkflow`] from the create-workflow request body,
/// renders it here, then re-parses the result through [`parse_workflow`] to
/// get the exact same structural validation (trigger count, duplicate/dangling
/// node ids, unknown kinds, stray `agent` fields) a hand-authored
/// `workflows/<id>.toml` must pass — before anything is written to disk.
pub(crate) fn render_workflow(raw: &RawWorkflow) -> Result<String> {
    toml::to_string(raw).map_err(|err| OpenCompanyError::DataParse {
        path: PathBuf::from(format!("{}.toml", raw.id)),
        message: err.to_string(),
    })
}

/// Parses one workflow graph from TOML source, validating it in full.
///
/// Unknown keys are tolerated. On a validation failure every problem is
/// returned together via [`OpenCompanyError::DataInvalid`].
pub fn parse_workflow(toml_src: &str) -> Result<WorkflowFile> {
    let raw: RawWorkflow = toml::from_str(toml_src).map_err(|err| OpenCompanyError::DataParse {
        path: PathBuf::from("workflow.toml"),
        message: err.message().to_string(),
    })?;

    let path = if raw.id.trim().is_empty() {
        PathBuf::from("workflow.toml")
    } else {
        PathBuf::from(format!("{}.toml", raw.id))
    };

    let problems = validate(&raw);
    if !problems.is_empty() {
        return Err(OpenCompanyError::DataInvalid { path, problems });
    }

    Ok(WorkflowFile {
        id: raw.id,
        name: raw.name,
        description: raw.description,
        nodes: raw
            .nodes
            .into_iter()
            .map(|node| WorkflowNodeDef {
                // Kind was validated above; a known string always parses.
                kind: WorkflowNodeKind::parse(&node.kind).unwrap_or(WorkflowNodeKind::Output),
                id: node.id,
                name: node.name,
                summary: node.summary,
                agent: node.agent,
                // Validation rejects TOML's non-finite floats before this
                // conversion because JSON cannot represent them.
                config: node.config.map(|value| {
                    serde_json::to_value(value)
                        .expect("validated TOML config always converts to JSON")
                }),
                on_error: node.on_error,
                retry: node.retry,
                requires_approval: node.requires_approval,
            })
            .collect(),
        edges: raw
            .edges
            .into_iter()
            .map(|edge| WorkflowEdgeDef {
                from: edge.from,
                to: edge.to,
                label: edge.label,
            })
            .collect(),
    })
}

/// Loads the enabled workflow graphs from a company directory.
///
/// `dir` is the company root; each enabled id resolves to
/// `dir/workflows/<id>.toml`. A missing or malformed file is an error.
pub fn load_company_workflows(dir: &Path, enabled: &[String]) -> Result<Vec<WorkflowFile>> {
    let mut out = Vec::with_capacity(enabled.len());
    for id in enabled {
        let path = dir.join("workflows").join(format!("{id}.toml"));
        let text = std::fs::read_to_string(&path).map_err(|source| OpenCompanyError::DataRead {
            path: path.clone(),
            source,
        })?;
        // Re-label parse/validation errors with the real on-disk path.
        let workflow = match parse_workflow(&text) {
            Ok(workflow) => workflow,
            Err(OpenCompanyError::DataInvalid { problems, .. }) => {
                return Err(OpenCompanyError::DataInvalid { path, problems });
            }
            Err(OpenCompanyError::DataParse { message, .. }) => {
                return Err(OpenCompanyError::DataParse { path, message });
            }
            Err(other) => return Err(other),
        };
        out.push(workflow);
    }
    Ok(out)
}

/// Scans a company's `workflows/` directory and loads every `*.toml` graph it
/// finds, in stable id order.
///
/// The single on-disk enumeration both the REST `list_workflows` route and the
/// orchestrator's `query_company` surface read, so the console picker and the
/// agent can never disagree about which workflows a company has saved. `None`
/// (platform-provisioned mode) or a missing `workflows/` directory yields an
/// empty vec. A malformed `workflows/<id>.toml` skips only itself (logged) so
/// one bad file never hides the rest.
pub fn list_source_workflows(source_dir: Option<&Path>) -> Vec<WorkflowFile> {
    let Some(source_dir) = source_dir else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(source_dir.join("workflows")) else {
        return Vec::new();
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
    ids.sort();
    let mut files = Vec::with_capacity(ids.len());
    for id in &ids {
        match load_company_workflows(source_dir, std::slice::from_ref(id)) {
            Ok(loaded) => files.extend(loaded),
            Err(err) => tracing::warn!(workflow = %id, error = %err, "skipping malformed workflow"),
        }
    }
    files
}

/// Collects every validation problem in prosumer language. Empty means valid.
fn validate(raw: &RawWorkflow) -> Vec<String> {
    let mut problems = Vec::new();

    if raw.id.trim().is_empty() {
        problems.push("this workflow is missing a top-level `id`.".into());
    }
    if raw.name.trim().is_empty() {
        problems.push("this workflow is missing a top-level `name`.".into());
    }

    // Node ids: present, unique. Kinds known. `agent` only on `agent` nodes.
    // Per-node config/error/retry policy is validated in the same pass.
    let mut seen = std::collections::HashSet::new();
    let mut trigger_count = 0usize;
    // Ids of nodes whose `on_error = "route"` — an "error"-labeled edge must
    // leave one, and only one, of these (checked in the edge pass below).
    let mut route_nodes = std::collections::HashSet::new();
    // Ids of every `switch` node. On a switch, an edge label is a case name (the
    // engine keys the branch port on it), so a label of `error` is a legitimate
    // case — it must NOT be caught by the `error`-label ⇔ `on_error = "route"`
    // coupling check in the edge pass.
    let mut switch_nodes = std::collections::HashSet::new();
    for (index, node) in raw.nodes.iter().enumerate() {
        let label = if node.id.trim().is_empty() {
            format!("node #{}", index + 1)
        } else {
            format!("node `{}`", node.id)
        };

        if node.id.trim().is_empty() {
            problems.push(format!("{label} is missing an `id`."));
        } else if !seen.insert(node.id.as_str()) {
            problems.push(format!(
                "node `id` `{}` is used more than once — ids must be unique.",
                node.id
            ));
        }

        let kind = WorkflowNodeKind::parse(&node.kind);
        match kind {
            Some(WorkflowNodeKind::Trigger) => trigger_count += 1,
            Some(kind) => {
                if kind != WorkflowNodeKind::Agent && node.agent.is_some() {
                    problems.push(format!(
                        "{label} sets `agent` but is a `{}` node — only `agent` nodes name a teammate.",
                        kind.as_str()
                    ));
                }
            }
            None => problems.push(format!(
                "{label} has an unknown `kind` `{}` — use one of {}.",
                node.kind,
                WORKFLOW_NODE_KINDS.join(", ")
            )),
        }

        if kind == Some(WorkflowNodeKind::Switch) && !node.id.trim().is_empty() {
            switch_nodes.insert(node.id.as_str());
        }

        if node.config.as_ref().is_some_and(contains_non_finite_float) {
            problems.push(format!(
                "{label} has a non-finite number in `config` — JSON workflow config only supports finite numbers."
            ));
        }

        // A `sub_workflow` node references a saved workflow by id. Its whole
        // contract rides in `config`: require a non-empty `workflow_id` string,
        // reject an inline `workflow` child graph (which would bypass OpenCompany
        // validation of the child), and reject a static self-reference (a graph
        // naming its own id) — the runtime cycle guard backstops dynamic ids.
        if kind == Some(WorkflowNodeKind::SubWorkflow) {
            match node.config.as_ref() {
                Some(toml::Value::Table(table)) => {
                    if table.contains_key("workflow") {
                        problems.push(format!(
                            "{label} sets an inline `workflow` in `config` — a sub_workflow must reference a saved workflow with `workflow_id`, not inline a child graph."
                        ));
                    }
                    match table.get("workflow_id") {
                        Some(toml::Value::String(id)) if id.trim().is_empty() => problems.push(
                            format!("{label} has an empty `workflow_id` — name the saved workflow to run."),
                        ),
                        Some(toml::Value::String(id)) => {
                            if !raw.id.trim().is_empty() && id == &raw.id {
                                problems.push(format!(
                                    "{label} references its own workflow id `{id}` — a workflow cannot run itself as a sub_workflow."
                                ));
                            }
                        }
                        Some(_) => problems.push(format!(
                            "{label} has a non-string `workflow_id` — it must be the id of a saved workflow."
                        )),
                        None => problems.push(format!(
                            "{label} is a sub_workflow node but names no `workflow_id` to run."
                        )),
                    }
                }
                _ => problems.push(format!(
                    "{label} is a sub_workflow node but has no `config` naming a `workflow_id` to run."
                )),
            }
        }

        // `on_error` ∈ {stop, continue, route}. Remember route nodes for the
        // edge-coupling check.
        if let Some(on_error) = node.on_error.as_deref() {
            if !matches!(on_error, "stop" | "continue" | "route") {
                problems.push(format!(
                    "{label} has an unknown `on_error` `{on_error}` — use one of stop, continue, route."
                ));
            } else if on_error == "route" && !node.id.trim().is_empty() {
                route_nodes.insert(node.id.as_str());
            }
        }

        // `retry`: at least one attempt; a known backoff curve.
        if let Some(retry) = &node.retry {
            if let Some(max_attempts) = retry.max_attempts
                && max_attempts < 1
            {
                problems.push(format!(
                    "{label} sets `retry.max_attempts` to {max_attempts} — it must be at least 1."
                ));
            }
            if let Some(backoff) = retry.backoff.as_deref()
                && !matches!(backoff, "fixed" | "exponential")
            {
                problems.push(format!(
                    "{label} has an unknown `retry.backoff` `{backoff}` — use fixed or exponential."
                ));
            }
        }

        // Reserved config keys: the first-class fields above are written into
        // the engine config LAST, so a `config` entry naming one would be
        // silently ignored — reject it as a footgun instead. `agent_ref` is
        // reserved on `agent` nodes (translation binds it from `agent`).
        if let Some(toml::Value::Table(table)) = &node.config {
            for reserved in ["on_error", "retry", "requires_approval"] {
                if table.contains_key(reserved) {
                    problems.push(format!(
                        "{label} puts `{reserved}` inside `config` — set it as a first-class node field, not in `config`."
                    ));
                }
            }
            if kind == Some(WorkflowNodeKind::Agent) && table.contains_key("agent_ref") {
                problems.push(format!(
                    "{label} puts `agent_ref` inside `config` — name the teammate with the node's `agent` field instead."
                ));
            }
        }
    }

    if trigger_count == 0 {
        problems.push("a workflow needs at least one `trigger` node to say what starts it.".into());
    }

    // Edges: endpoints must reference existing nodes; no self-loops. An
    // "error"-labeled edge and an `on_error = "route"` node imply each other.
    let mut route_nodes_with_error_edge = std::collections::HashSet::new();
    for (index, edge) in raw.edges.iter().enumerate() {
        let label = format!("edge #{}", index + 1);

        if edge.from.trim().is_empty() {
            problems.push(format!("{label} is missing a `from` node."));
        } else if !seen.contains(edge.from.as_str()) {
            problems.push(format!(
                "{label} starts at `{}`, which is not a node in this workflow.",
                edge.from
            ));
        }

        if edge.to.trim().is_empty() {
            problems.push(format!("{label} is missing a `to` node."));
        } else if !seen.contains(edge.to.as_str()) {
            problems.push(format!(
                "{label} points to `{}`, which is not a node in this workflow.",
                edge.to
            ));
        }

        if !edge.from.trim().is_empty() && edge.from == edge.to {
            problems.push(format!(
                "{label} loops `{}` back to itself — an edge must connect two different nodes.",
                edge.from
            ));
        }

        // An "error"-labeled edge is the recovery route out of a routing node —
        // UNLESS it leaves a `switch`, where every label (including `error`) is a
        // case name the engine keys the branch port on, not an error route.
        if edge.label.as_deref() == Some("error") {
            if route_nodes.contains(edge.from.as_str()) {
                route_nodes_with_error_edge.insert(edge.from.as_str());
            } else if seen.contains(edge.from.as_str())
                && !switch_nodes.contains(edge.from.as_str())
            {
                problems.push(format!(
                    "{label} is labeled `error` but its source `{}` is not `on_error = \"route\"` — only a routing node emits an error edge.",
                    edge.from
                ));
            }
        }
    }

    // Every routing node must actually have somewhere to route its error to.
    for node_id in &route_nodes {
        if !route_nodes_with_error_edge.contains(node_id) {
            problems.push(format!(
                "node `{node_id}` sets `on_error = \"route\"` but has no outgoing edge labeled `error` to route the failure to."
            ));
        }
    }

    problems
}

/// Whether a TOML config contains a float JSON cannot represent.
fn contains_non_finite_float(value: &toml::Value) -> bool {
    match value {
        toml::Value::Float(value) => !value.is_finite(),
        toml::Value::Array(values) => values.iter().any(contains_non_finite_float),
        toml::Value::Table(values) => values.values().any(contains_non_finite_float),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_workflow_round_trips_through_parse_workflow() {
        let raw = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: Some("A test graph.".to_string()),
            nodes: vec![
                RawNode {
                    id: "start".to_string(),
                    kind: "trigger".to_string(),
                    name: "Start".to_string(),
                    summary: None,
                    agent: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                },
                RawNode {
                    id: "worker".to_string(),
                    kind: "agent".to_string(),
                    name: "Worker".to_string(),
                    summary: Some("Does the thing.".to_string()),
                    agent: Some("ceo".to_string()),
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                },
            ],
            edges: vec![RawEdge {
                from: "start".to_string(),
                to: "worker".to_string(),
                label: Some("ok".to_string()),
            }],
        };
        let toml_src = render_workflow(&raw).expect("renders");
        let file = parse_workflow(&toml_src).expect("re-parses the rendered graph");
        assert_eq!(file.id, "wf");
        assert_eq!(file.nodes.len(), 2);
        assert_eq!(file.edges.len(), 1);
        let worker = file.nodes.iter().find(|n| n.id == "worker").unwrap();
        assert_eq!(worker.agent.as_deref(), Some("ceo"));
        assert_eq!(file.edges[0].label.as_deref(), Some("ok"));
    }

    /// A rendered graph that fails structural validation (no trigger) surfaces
    /// the same prosumer-language problem `parse_workflow` gives a hand-authored
    /// file — the create endpoint relies on this to turn a bad graph into a 4xx.
    #[test]
    fn render_workflow_of_an_invalid_graph_fails_reparse() {
        let raw = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
            nodes: vec![RawNode {
                id: "only".to_string(),
                kind: "output".to_string(),
                name: "Only".to_string(),
                summary: None,
                agent: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
            }],
            edges: vec![],
        };
        let toml_src = render_workflow(&raw).expect("renders even though invalid");
        let err = parse_workflow(&toml_src).unwrap_err();
        assert!(err.to_string().contains("trigger"), "{err}");
    }

    const CAMPAIGN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/companies/agentic_marketing_agency/workflows/campaign_pipeline.toml"
    ));

    #[test]
    fn parses_the_shipped_campaign_pipeline() {
        let workflow = parse_workflow(CAMPAIGN).expect("campaign pipeline is valid");
        assert_eq!(workflow.id, "campaign_pipeline");
        assert_eq!(workflow.name, "Campaign pipeline");
        assert_eq!(workflow.nodes.len(), 8);
        assert_eq!(workflow.edges.len(), 8);
        let strategist = workflow
            .nodes
            .iter()
            .find(|n| n.id == "strategist")
            .unwrap();
        assert_eq!(strategist.kind, WorkflowNodeKind::Agent);
        assert_eq!(strategist.agent.as_deref(), Some("brand_strategist"));
        let brief = workflow.nodes.iter().find(|n| n.id == "brief").unwrap();
        assert_eq!(brief.kind, WorkflowNodeKind::Trigger);
    }

    #[test]
    fn edge_referencing_missing_node_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[edge]]
            from = "start"
            to = "ghost"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("ghost"), "{message}");
        assert!(message.contains("not a node"), "{message}");
    }

    #[test]
    fn missing_trigger_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "only"
            kind = "output"
            name = "Only"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("trigger"), "{err}");
    }

    #[test]
    fn empty_workflow_has_no_trigger() {
        let src = r#"
            id = "wf"
            name = "WF"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("trigger"), "{err}");
    }

    #[test]
    fn duplicate_node_ids_and_self_loops_are_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "a"
            kind = "trigger"
            name = "A"
            [[node]]
            id = "a"
            kind = "output"
            name = "A2"
            [[edge]]
            from = "a"
            to = "a"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("more than once"), "{message}");
        assert!(message.contains("itself"), "{message}");
    }

    #[test]
    fn unknown_kind_and_stray_agent_are_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "weird"
            kind = "teleport"
            name = "Weird"
            [[node]]
            id = "gate"
            kind = "condition"
            name = "Gate"
            agent = "someone"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown `kind`"), "{message}");
        assert!(
            message.contains("only `agent` nodes name a teammate"),
            "{message}"
        );
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        let src = r#"
            id = "wf"
            name = "WF"
            canvas_zoom = 1.5
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            extra = "ignored"
        "#;
        assert!(parse_workflow(src).is_ok());
    }

    // --- Per-node config / error / retry policy (P1) -----------------------

    #[test]
    fn node_config_parses_including_nested_tables() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "call"
            kind = "tool_call"
            name = "Export"
            [node.config]
            slug = "csv_export"
            [node.config.args]
            filename = "out.csv"
            data = "[]"
        "#;
        let file = parse_workflow(src).expect("config parses");
        let call = file.nodes.iter().find(|n| n.id == "call").unwrap();
        let config = call.config.as_ref().expect("config present");
        assert_eq!(config["slug"], "csv_export");
        // Nested table survives the TOML → JSON conversion.
        assert_eq!(config["args"]["filename"], "out.csv");
    }

    #[test]
    fn non_finite_config_number_is_rejected_instead_of_dropped() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [node.config]
            threshold = nan
        "#;
        let err = parse_workflow(src).expect_err("non-JSON config must fail");
        assert!(err.to_string().contains("config"), "{err}");
    }

    #[test]
    fn typed_error_retry_and_approval_fields_parse() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "call"
            kind = "tool_call"
            name = "Export"
            on_error = "continue"
            requires_approval = true
            [node.retry]
            max_attempts = 3
            backoff_ms = 100
            backoff = "exponential"
        "#;
        let file = parse_workflow(src).expect("parses");
        let call = file.nodes.iter().find(|n| n.id == "call").unwrap();
        assert_eq!(call.on_error.as_deref(), Some("continue"));
        assert_eq!(call.requires_approval, Some(true));
        let retry = call.retry.as_ref().expect("retry present");
        assert_eq!(retry.max_attempts, Some(3));
        assert_eq!(retry.backoff_ms, Some(100));
        assert_eq!(retry.backoff.as_deref(), Some("exponential"));
    }

    #[test]
    fn bad_on_error_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            on_error = "explode"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown `on_error`"), "{message}");
    }

    #[test]
    fn retry_max_attempts_zero_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [node.retry]
            max_attempts = 0
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("at least 1"), "{err}");
    }

    #[test]
    fn bad_retry_backoff_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [node.retry]
            backoff = "linear"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("unknown `retry.backoff`"), "{err}");
    }

    #[test]
    fn reserved_config_keys_are_rejected() {
        // `on_error` inside `config` (not as a first-class field) is a footgun.
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [node.config]
            on_error = "route"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("inside `config`"), "{err}");
    }

    #[test]
    fn config_agent_ref_on_agent_node_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "worker"
            kind = "agent"
            name = "Worker"
            agent = "ceo"
            [node.config]
            agent_ref = "impostor"
            [[edge]]
            from = "start"
            to = "worker"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("agent_ref"), "{err}");
    }

    #[test]
    fn route_without_error_edge_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "call"
            kind = "tool_call"
            name = "Call"
            on_error = "route"
            [[edge]]
            from = "start"
            to = "call"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(
            err.to_string().contains("no outgoing edge labeled `error`"),
            "{err}"
        );
    }

    #[test]
    fn error_edge_without_route_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "call"
            kind = "tool_call"
            name = "Call"
            [[node]]
            id = "recover"
            kind = "output"
            name = "Recover"
            [[edge]]
            from = "start"
            to = "call"
            [[edge]]
            from = "call"
            to = "recover"
            label = "error"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("only a routing node"), "{err}");
    }

    #[test]
    fn route_with_matching_error_edge_is_valid() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "call"
            kind = "tool_call"
            name = "Call"
            on_error = "route"
            [[node]]
            id = "recover"
            kind = "output"
            name = "Recover"
            [[edge]]
            from = "start"
            to = "call"
            [[edge]]
            from = "call"
            to = "recover"
            label = "error"
        "#;
        assert!(parse_workflow(src).is_ok());
    }

    // --- P2: the six new node kinds ----------------------------------------

    /// Each new node kind parses to its enum variant, and `WORKFLOW_NODE_KINDS`
    /// advertises all twelve.
    #[test]
    fn new_node_kinds_parse() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "sw"
            kind = "switch"
            name = "Switch"
            [[node]]
            id = "mg"
            kind = "merge"
            name = "Merge"
            [[node]]
            id = "so"
            kind = "split_out"
            name = "Split"
            [[node]]
            id = "tf"
            kind = "transform"
            name = "Transform"
            [[node]]
            id = "op"
            kind = "output_parser"
            name = "Parse"
        "#;
        let file = parse_workflow(src).expect("new kinds parse");
        let kind = |id: &str| file.nodes.iter().find(|n| n.id == id).unwrap().kind;
        assert_eq!(kind("sw"), WorkflowNodeKind::Switch);
        assert_eq!(kind("mg"), WorkflowNodeKind::Merge);
        assert_eq!(kind("so"), WorkflowNodeKind::SplitOut);
        assert_eq!(kind("tf"), WorkflowNodeKind::Transform);
        assert_eq!(kind("op"), WorkflowNodeKind::OutputParser);
        assert_eq!(WORKFLOW_NODE_KINDS.len(), 12);
        assert!(WORKFLOW_NODE_KINDS.contains(&"sub_workflow"));
    }

    /// A `sub_workflow` node with a non-empty `workflow_id` string is valid.
    #[test]
    fn sub_workflow_with_workflow_id_is_valid() {
        let src = r#"
            id = "parent"
            name = "Parent"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "child"
            kind = "sub_workflow"
            name = "Child"
            [node.config]
            workflow_id = "greet"
            [[edge]]
            from = "start"
            to = "child"
        "#;
        let file = parse_workflow(src).expect("sub_workflow parses");
        let child = file.nodes.iter().find(|n| n.id == "child").unwrap();
        assert_eq!(child.kind, WorkflowNodeKind::SubWorkflow);
        assert_eq!(child.config.as_ref().unwrap()["workflow_id"], "greet");
    }

    /// A `sub_workflow` node with no `config` is rejected — it names nothing to run.
    #[test]
    fn sub_workflow_without_config_is_rejected() {
        let src = r#"
            id = "parent"
            name = "Parent"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "child"
            kind = "sub_workflow"
            name = "Child"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("workflow_id"), "{err}");
    }

    /// A `sub_workflow` node with an empty `workflow_id` is rejected.
    #[test]
    fn sub_workflow_with_empty_workflow_id_is_rejected() {
        let src = r#"
            id = "parent"
            name = "Parent"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "child"
            kind = "sub_workflow"
            name = "Child"
            [node.config]
            workflow_id = ""
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("empty `workflow_id`"), "{err}");
    }

    /// A `sub_workflow` node naming its own workflow id is a static self-reference.
    #[test]
    fn sub_workflow_self_reference_is_rejected() {
        let src = r#"
            id = "loopy"
            name = "Loopy"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "child"
            kind = "sub_workflow"
            name = "Child"
            [node.config]
            workflow_id = "loopy"
            [[edge]]
            from = "start"
            to = "child"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("its own workflow id"), "{err}");
    }

    /// An inline `workflow` child graph is reserved — a sub_workflow must
    /// reference a saved workflow by id so the child passes OpenCompany validation.
    #[test]
    fn sub_workflow_inline_child_graph_is_rejected() {
        let src = r#"
            id = "parent"
            name = "Parent"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "child"
            kind = "sub_workflow"
            name = "Child"
            [node.config]
            workflow_id = "greet"
            [node.config.workflow]
            id = "inlined"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("inline `workflow`"), "{err}");
    }

    /// On a `switch`, an `error`-labeled edge is a legitimate case name — it must
    /// NOT trip the `error`-label ⇔ `on_error = "route"` coupling check.
    #[test]
    fn switch_error_label_is_a_case_not_a_route() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "sw"
            kind = "switch"
            name = "Switch"
            [[node]]
            id = "err_case"
            kind = "output"
            name = "Error case"
            [[node]]
            id = "ok_case"
            kind = "output"
            name = "OK case"
            [[edge]]
            from = "start"
            to = "sw"
            [[edge]]
            from = "sw"
            to = "err_case"
            label = "error"
            [[edge]]
            from = "sw"
            to = "ok_case"
            label = "ok"
        "#;
        assert!(
            parse_workflow(src).is_ok(),
            "an error-labeled switch case must be valid without on_error = route"
        );
    }

    #[test]
    fn legacy_files_without_new_fields_parse_unchanged() {
        // A graph authored before the P1 fields existed: every new field is None.
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
        "#;
        let file = parse_workflow(src).expect("legacy parses");
        let start = &file.nodes[0];
        assert!(start.config.is_none());
        assert!(start.on_error.is_none());
        assert!(start.retry.is_none());
        assert!(start.requires_approval.is_none());
    }
}
