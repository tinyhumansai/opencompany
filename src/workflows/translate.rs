//! Translate a company [`WorkflowFile`] into a tinyflows
//! [`WorkflowGraph`](tinyflows::model::WorkflowGraph).
//!
//! OpenCompany's on-disk model is a validated six-kind node/edge graph (see
//! [`crate::company::workflow_file`]); tinyflows' runnable model is a
//! twelve-kind graph. The mapping is mostly one-to-one, with two deliberate
//! choices:
//!
//! * **`output` → [`Transform`](tinyflows::model::NodeKind::Transform)** —
//!   tinyflows has no `output` kind. A `transform` node with no `set` config is
//!   a pure pass-through, which is exactly the terminal "report back" semantics
//!   of an `output` node (its predecessors' items flow through unchanged).
//! * **condition edge labels → `true`/`false` ports** — tinyflows keys a
//!   `condition` node's branch EXCLUSIVELY on the edge `from_port`, which must be
//!   `"true"` or `"false"` (any other value is a hard validation error). The
//!   OpenCompany model carries the branch on an edge `label` (`"yes"`/`"no"`),
//!   so an edge leaving a condition node maps its label to a `true`/`false`
//!   port. Every other edge stays on the default `"main"` port.
//!
//! An **agent** node's roster teammate id becomes the tinyflows `agent_ref` in
//! node config, which the engine's `agent` node routes to the injected
//! `AgentRunner` — that is how a step lands on the harness pool (see
//! [`super::caps`]). `tool_call` and `http_request` nodes are mapped
//! structurally; executing them end-to-end (real tool/HTTP semantics) is a
//! documented follow-on — [`super::caps`] wires them to explicit "not yet wired"
//! capabilities so an unreached node is inert and a reached one fails loudly
//! rather than silently.

use std::collections::HashSet;

use serde_json::{Value, json};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

use crate::company::{WorkflowEdgeDef, WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};

/// Translates a validated [`WorkflowFile`] into a tinyflows
/// [`WorkflowGraph`](tinyflows::model::WorkflowGraph) ready for
/// [`tinyflows::compiler::compile`].
///
/// The source file is assumed already validated by
/// [`parse_workflow`](crate::company::workflow_file::parse_workflow) (exactly
/// one trigger, unique node ids, edges reference real nodes), so this is a
/// total, side-effect-free mapping.
pub fn translate(file: &WorkflowFile) -> WorkflowGraph {
    // The ids of every `condition` node, so an edge leaving one can map its
    // label onto the required `true`/`false` branch port.
    let condition_ids: HashSet<&str> = file
        .nodes
        .iter()
        .filter(|n| n.kind == WorkflowNodeKind::Condition)
        .map(|n| n.id.as_str())
        .collect();

    WorkflowGraph {
        id: Some(file.id.clone()),
        name: file.name.clone(),
        nodes: file.nodes.iter().map(translate_node).collect(),
        edges: file
            .edges
            .iter()
            .map(|edge| translate_edge(edge, &condition_ids))
            .collect(),
        ..WorkflowGraph::default()
    }
}

/// Maps one OpenCompany node to its tinyflows [`Node`], carrying the kind's
/// relevant config (an `agent` node's `agent_ref` + prompt; a placeholder
/// `slug` for `tool_call`).
fn translate_node(def: &WorkflowNodeDef) -> Node {
    let (kind, config) = match def.kind {
        WorkflowNodeKind::Trigger => (NodeKind::Trigger, json!({})),
        WorkflowNodeKind::Agent => (NodeKind::Agent, agent_config(def)),
        // A `tool_call` needs a `slug` in config or the engine's node errors; the
        // OpenCompany model carries no tool binding yet, so use the node id as a
        // stable placeholder. Real tool wiring is follow-on (see module docs).
        WorkflowNodeKind::ToolCall => (NodeKind::ToolCall, json!({ "slug": def.id })),
        WorkflowNodeKind::HttpRequest => (NodeKind::HttpRequest, json!({})),
        WorkflowNodeKind::Condition => (NodeKind::Condition, json!({})),
        // No `output` kind in tinyflows; a config-less `transform` is a pure
        // pass-through terminal — the "report back" semantics of `output`.
        WorkflowNodeKind::Output => (NodeKind::Transform, json!({})),
    };
    Node {
        id: def.id.clone(),
        kind,
        type_version: 1,
        name: def.name.clone(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

/// Builds an `agent` node's config: the roster teammate id as `agent_ref` (so
/// the engine routes to the harness `AgentRunner`) plus a `prompt` drawn from
/// the node's summary, falling back to its name.
fn agent_config(def: &WorkflowNodeDef) -> Value {
    let mut config = serde_json::Map::new();
    if let Some(agent) = def.agent.as_deref().filter(|a| !a.is_empty()) {
        config.insert("agent_ref".to_string(), json!(agent));
    }
    config.insert("prompt".to_string(), json!(prompt_for(def)));
    Value::Object(config)
}

/// The instruction handed to an agent node: its summary when present, else its
/// human-readable name.
fn prompt_for(def: &WorkflowNodeDef) -> String {
    def.summary
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&def.name)
        .to_string()
}

/// Maps one OpenCompany edge to a tinyflows [`Edge`]. Edges leaving a
/// `condition` node carry their branch on `from_port` (`true`/`false`, mapped
/// from the label); every other edge stays on the default `main` port.
fn translate_edge(edge: &WorkflowEdgeDef, condition_ids: &HashSet<&str>) -> Edge {
    let from_port = if condition_ids.contains(edge.from.as_str()) {
        condition_port(edge.label.as_deref())
    } else {
        "main".to_string()
    };
    Edge {
        from_node: edge.from.clone(),
        from_port,
        to_node: edge.to.clone(),
        to_port: "main".to_string(),
    }
}

/// Maps a condition edge's label onto the required `true`/`false` branch port.
/// Negative labels (`no`/`false`/`n`) map to `"false"`; everything else
/// (including an absent label) maps to `"true"`.
fn condition_port(label: Option<&str>) -> String {
    let negative = label
        .map(|l| l.trim().to_ascii_lowercase())
        .is_some_and(|l| matches!(l.as_str(), "no" | "false" | "n"));
    if negative { "false" } else { "true" }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::parse_workflow;

    const CAMPAIGN: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/companies/agentic_marketing_agency/workflows/campaign_pipeline.toml"
    ));

    /// The shipped campaign pipeline translates into a graph tinyflows accepts,
    /// exercising every one of the six node kinds.
    #[test]
    fn translates_the_shipped_campaign_pipeline() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);

        assert_eq!(graph.id.as_deref(), Some("campaign_pipeline"));
        assert_eq!(graph.name, "Campaign pipeline");
        assert_eq!(graph.nodes.len(), file.nodes.len());
        assert_eq!(graph.edges.len(), file.edges.len());

        // The translated graph is structurally valid for the engine.
        tinyflows::compiler::compile(&graph).expect("translated graph compiles");
    }

    /// Node kinds map across, and an `output` node becomes a pass-through
    /// `transform`.
    #[test]
    fn maps_every_node_kind() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);
        let kind = |id: &str| {
            graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.kind.clone())
        };

        assert_eq!(kind("brief"), Some(NodeKind::Trigger));
        assert_eq!(kind("strategist"), Some(NodeKind::Agent));
        assert_eq!(kind("gate"), Some(NodeKind::Condition));
        assert_eq!(kind("research"), Some(NodeKind::ToolCall));
        assert_eq!(kind("publish"), Some(NodeKind::HttpRequest));
        // `output` lowers to a pass-through `transform`.
        assert_eq!(kind("done"), Some(NodeKind::Transform));
    }

    /// An agent node carries its roster teammate id as `agent_ref` plus a prompt.
    #[test]
    fn agent_node_carries_agent_ref_and_prompt() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);
        let strategist = graph.nodes.iter().find(|n| n.id == "strategist").unwrap();
        assert_eq!(strategist.config["agent_ref"], "brand_strategist");
        assert_eq!(
            strategist.config["prompt"],
            "Turns the brief into an angle + outline."
        );
    }

    /// A condition node's `yes`/`no` labels become `true`/`false` branch ports.
    #[test]
    fn condition_labels_map_to_true_false_ports() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);
        let port = |to: &str| {
            graph
                .edges
                .iter()
                .find(|e| e.from_node == "gate" && e.to_node == to)
                .map(|e| e.from_port.clone())
        };
        assert_eq!(port("research").as_deref(), Some("true")); // label "yes"
        assert_eq!(port("copy").as_deref(), Some("false")); // label "no"
    }

    /// Non-condition edges keep the default `main` port.
    #[test]
    fn plain_edges_stay_on_main() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);
        let edge = graph
            .edges
            .iter()
            .find(|e| e.from_node == "brief" && e.to_node == "strategist")
            .unwrap();
        assert_eq!(edge.from_port, "main");
        assert_eq!(edge.to_port, "main");
    }

    /// The label mapping is total: negatives → false, everything else → true.
    #[test]
    fn condition_port_mapping() {
        assert_eq!(condition_port(Some("yes")), "true");
        assert_eq!(condition_port(Some("no")), "false");
        assert_eq!(condition_port(Some("TRUE")), "true");
        assert_eq!(condition_port(Some("False")), "false");
        assert_eq!(condition_port(None), "true");
    }
}
