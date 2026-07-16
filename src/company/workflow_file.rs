//! Workflow graph files: `companies/<name>/workflows/<id>.toml`.
//!
//! Each enabled workflow is a data-only node/edge graph edited by the Workflow
//! canvas and referenced by `[workflows].enabled` in the manifest. This module
//! parses those files into a validated [`WorkflowFile`], reporting every problem
//! at once in prosumer language, matching [`super::manifest`].

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{OpenCompanyError, Result};

/// The six node kinds a workflow graph may use, mirroring the tinyflows model.
pub const WORKFLOW_NODE_KINDS: &[&str] = &[
    "trigger",
    "agent",
    "tool_call",
    "http_request",
    "condition",
    "output",
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
#[derive(Deserialize)]
struct RawWorkflow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "node")]
    nodes: Vec<RawNode>,
    #[serde(default, rename = "edge")]
    edges: Vec<RawEdge>,
}

#[derive(Deserialize)]
struct RawNode {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Deserialize)]
struct RawEdge {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    label: Option<String>,
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
    let mut seen = std::collections::HashSet::new();
    let mut trigger_count = 0usize;
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

        match WorkflowNodeKind::parse(&node.kind) {
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
    }

    if trigger_count == 0 {
        problems.push("a workflow needs at least one `trigger` node to say what starts it.".into());
    }

    // Edges: endpoints must reference existing nodes; no self-loops.
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
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
