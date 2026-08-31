//! Translate a company [`WorkflowFile`] into a tinyflows
//! [`WorkflowGraph`](tinyflows::model::WorkflowGraph).
//!
//! OpenCompany's on-disk model is a validated node/edge graph whose accepted
//! node kinds are the
//! [`WORKFLOW_NODE_KINDS`](crate::company::workflow_file::WORKFLOW_NODE_KINDS)
//! authoring set (see [`crate::company::workflow_file`]); tinyflows' runnable
//! model carries the wider `NODE_KINDS` engine catalog. Every accepted kind
//! lowers into that catalog, but the parser deliberately refuses the
//! engine-only kinds — the authoring contract and the rejected set are spelled
//! out in `docs/spec/runtime/workflow-vocabulary.md`. The mapping is mostly
//! one-to-one, with two deliberate choices:
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
//! An `output` node's [`destination`](crate::company::WorkflowDestinationDef)
//! is deliberately **not** translated. Delivery runs host-side after the engine
//! returns (see [`super::delivery`]), so the engine has no use for it and a
//! `destination` key in node config would be inert cargo. A destination-bearing
//! `output` node therefore lowers to exactly the same bare pass-through
//! `Transform` as one without — pinned by the
//! `an_output_destination_never_reaches_the_engine_graph` test below.
//!
//! An **agent** node's roster teammate id becomes the tinyflows `agent_ref` in
//! node config, which the engine's `agent` node routes to the injected
//! `AgentRunner` — that is how a step lands on the harness pool (see
//! [`super::caps`]). It also carries an `input = "=items"` binding (issue #782)
//! so the engine resolves the full set of upstream node outputs into the node's
//! config at run time, giving the agent a channel to the previous step's result
//! (the runner folds it into the turn message; fan-in delivers every predecessor). `tool_call` and `http_request` nodes are mapped
//! structurally and both execute for real: a `tool_call` node runs a Cell A
//! toolbelt tool, fail-closed on the company's `[tools].allow` grants, and an
//! `http_request` node routes through the SSRF-guarded `GuardedHttpClient` —
//! both wired in [`super::caps`].

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
    // The ids of every `on_error = "route"` node, so an "error"-labeled edge
    // leaving one maps onto the engine's `error` port (the same mechanism as a
    // condition node's `true`/`false` ports).
    let route_ids: HashSet<&str> = file
        .nodes
        .iter()
        .filter(|n| n.on_error.as_deref() == Some("route"))
        .map(|n| n.id.as_str())
        .collect();
    // The ids of every `switch` node, so an edge leaving one carries its label
    // VERBATIM as the branch port — the engine's switch node routes to the port
    // whose name equals the computed case value (an unlabeled edge → `default`,
    // the same fallback the engine emits for a null/non-scalar discriminant).
    let switch_ids: HashSet<&str> = file
        .nodes
        .iter()
        .filter(|n| n.kind == WorkflowNodeKind::Switch)
        .map(|n| n.id.as_str())
        .collect();

    WorkflowGraph {
        id: Some(file.id.clone()),
        name: file.name.clone(),
        nodes: file.nodes.iter().map(translate_node).collect(),
        edges: file
            .edges
            .iter()
            .map(|edge| translate_edge(edge, &condition_ids, &route_ids, &switch_ids))
            .collect(),
        ..WorkflowGraph::default()
    }
}

/// The tinyflows [`NodeKind`] one OpenCompany node kind lowers to. `output` has
/// no tinyflows counterpart — a config-less `transform` is a pure pass-through
/// terminal, exactly the "report back" semantics of an `output` node.
fn tinyflows_kind(kind: WorkflowNodeKind) -> NodeKind {
    match kind {
        WorkflowNodeKind::Trigger => NodeKind::Trigger,
        WorkflowNodeKind::Agent => NodeKind::Agent,
        WorkflowNodeKind::ToolCall => NodeKind::ToolCall,
        WorkflowNodeKind::HttpRequest => NodeKind::HttpRequest,
        WorkflowNodeKind::Condition => NodeKind::Condition,
        WorkflowNodeKind::Output => NodeKind::Transform,
        // The P2 catalog maps one-to-one onto tinyflows' own kinds; all of their
        // contract rides in the node `config` overlay (P1), so `translate_node`
        // needs no per-kind handling.
        WorkflowNodeKind::Switch => NodeKind::Switch,
        WorkflowNodeKind::Merge => NodeKind::Merge,
        WorkflowNodeKind::SplitOut => NodeKind::SplitOut,
        WorkflowNodeKind::Transform => NodeKind::Transform,
        WorkflowNodeKind::OutputParser => NodeKind::OutputParser,
        WorkflowNodeKind::SubWorkflow => NodeKind::SubWorkflow,
    }
}

/// Maps one OpenCompany node to its tinyflows [`Node`], assembling the engine
/// config in three layers so a node's own config can specialize a step without
/// ever subverting the graph's identity:
///
/// 1. **Derived defaults** — the kind's built-in config (an `agent` node's
///    `prompt`).
/// 2. **User config overlay** — the node's free-form `config`, laid over the
///    defaults (so an author can override the derived `prompt`, add a
///    `tool_call` `slug`/`args`, or shape an `http_request` descriptor).
/// 3. **First-class fields LAST** — `agent_ref` (bound from `agent`, so config
///    can never rebind the node to another teammate) and the engine-read
///    `on_error` / `retry` / `requires_approval` keys. A `tool_call`'s `slug`
///    rides in the config overlay (layer 2); author-time validation guarantees
///    it is present, so translation adds no placeholder (issue #661).
///
/// A legacy node (no `config`, no typed fields) yields exactly the pre-P1
/// config, so translation of an unchanged file is byte-identical.
fn translate_node(def: &WorkflowNodeDef) -> Node {
    let mut config = serde_json::Map::new();

    // 1. Derived defaults.
    if def.kind == WorkflowNodeKind::Agent {
        config.insert("prompt".to_string(), json!(prompt_for(def)));
        // Issue #782: bind the FULL upstream node output so the agent's turn can
        // reference what the previous step produced. `=items` resolves (via the
        // engine's `resolve_config_traced`) to the `json` of every input item —
        // i.e. every direct-predecessor item, so a fan-in (`merge -> agent`, or
        // several edges into one agent) delivers ALL predecessors rather than
        // silently losing all but the first. The runner folds the resolved value
        // into the turn message (see `super::caps`); before this an agent node
        // lowered to only a static `prompt`, so an upstream node's output had no
        // channel to the next agent and was dropped. Kept in the derived-default
        // layer (like `prompt`), so an author can override the binding with their
        // own expression (e.g. `=nodes.<id>.item.text`) via node config.
        config.insert("input".to_string(), json!("=items"));
    }

    // 2. User config overlay.
    if let Some(Value::Object(user)) = &def.config {
        for (key, value) in user {
            config.insert(key.clone(), value.clone());
        }
    }

    // 3. First-class fields, written last so config cannot shadow them.
    match def.kind {
        WorkflowNodeKind::Agent => {
            if let Some(agent) = def.agent.as_deref().filter(|a| !a.is_empty()) {
                config.insert("agent_ref".to_string(), json!(agent));
            }
            // Issue #881: which node this is. The vendored `AgentRunner` trait
            // hands the capability only the resolved config and the trusted
            // `agent_ref` — the node's own id is lost at that boundary — so a
            // node that blocks on an approval could not say *which* node
            // blocked. Written in the first-class layer beside `agent_ref`, and
            // for the same reason: config must not be able to rebind a node's
            // identity to another node's name.
            config.insert("node_id".to_string(), json!(def.id));
        }
        // A `tool_call` needs a `slug` (the config overlay above carries it). The
        // masking node-id default was removed (issue #661): author-time
        // validation now rejects a slug-less `tool_call` on BOTH the on-disk seed
        // path (`workflow_file::validate`) and the console-draft path
        // (`validate_draft_against_record`), so a validated graph always binds a
        // real slug. Defaulting to the node id instead pointed the engine's
        // "missing tool" error at the wrong name and could collide with a real
        // tool, silently invoking one the author never chose; absent a slug the
        // engine's `tool_call` node now fails loudly on the missing key.
        WorkflowNodeKind::ToolCall => {}
        _ => {}
    }

    // Per-node error policy the tinyflows engine reads straight off config.
    if let Some(on_error) = def.on_error.as_deref() {
        config.insert("on_error".to_string(), json!(on_error));
    }
    if let Some(retry) = &def.retry {
        config.insert("retry".to_string(), retry_config(retry));
    }
    if let Some(requires_approval) = def.requires_approval {
        config.insert("requires_approval".to_string(), json!(requires_approval));
    }
    // Issue #1866: the deterministic postcondition, lowered the same way
    // `retry` is — a typed model field becomes the exact config key
    // `HarnessAgentRunner::run_turn` reads (`caps::postcondition`).
    if let Some(postcondition) = &def.postcondition {
        config.insert(
            "postcondition".to_string(),
            serde_json::to_value(postcondition)
                .expect("WorkflowPostconditionDef always serializes"),
        );
    }

    Node {
        id: def.id.clone(),
        kind: tinyflows_kind(def.kind),
        type_version: 1,
        name: def.name.clone(),
        config: Value::Object(config),
        ports: Vec::new(),
        position: None,
    }
}

/// Builds the engine's `retry` config object from the typed policy, emitting the
/// exact keys the tinyflows engine reads (`max_attempts` / `backoff_ms` /
/// `backoff`) and omitting any the author left unset (the engine defaults them).
fn retry_config(retry: &crate::company::WorkflowRetryDef) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(max_attempts) = retry.max_attempts {
        map.insert("max_attempts".to_string(), json!(max_attempts));
    }
    if let Some(backoff_ms) = retry.backoff_ms {
        map.insert("backoff_ms".to_string(), json!(backoff_ms));
    }
    if let Some(backoff) = &retry.backoff {
        map.insert("backoff".to_string(), json!(backoff));
    }
    Value::Object(map)
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

/// Maps one OpenCompany edge to a tinyflows [`Edge`]. An "error"-labeled edge
/// leaving an `on_error = "route"` node carries the `error` port (the engine
/// emits the failure item there); edges leaving a `condition` node carry their
/// branch on `from_port` (`true`/`false`, mapped from the label); every other
/// edge stays on the default `main` port.
///
/// The error-port check runs **first** so it takes precedence for a node that is
/// both a `condition` and `on_error = "route"`: without it, the condition branch
/// would map the `"error"` label through [`condition_port`] onto the `true`
/// port, silently misrouting the failure item onto the truthy branch instead of
/// the error edge.
fn translate_edge(
    edge: &WorkflowEdgeDef,
    condition_ids: &HashSet<&str>,
    route_ids: &HashSet<&str>,
    switch_ids: &HashSet<&str>,
) -> Edge {
    let from_port =
        if route_ids.contains(edge.from.as_str()) && edge.label.as_deref() == Some("error") {
            // The error-port check stays FIRST so a node that is both a routing
            // node and a condition/switch routes its `"error"` edge to the error
            // port rather than through the branch mapping below.
            "error".to_string()
        } else if switch_ids.contains(edge.from.as_str()) {
            // A switch edge's label is the case name, carried verbatim; an
            // unlabeled edge falls to the engine's `default` fallback port.
            edge.label
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "default".to_string())
        } else if condition_ids.contains(edge.from.as_str()) {
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
        // `publish` is an agent assembly step (#530) — there is no CMS to POST to.
        assert_eq!(kind("publish"), Some(NodeKind::Agent));
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

    /// **Issue #782.** Every agent node carries an `input = "=items"` binding so
    /// the engine resolves the full set of upstream node outputs into its config
    /// at run time — the only channel an upstream step's output has to the next
    /// agent's turn. `=items` (not `=item`) is deliberate: it is the whole
    /// predecessor set, so a fan-in (`merge -> agent`) delivers every predecessor
    /// rather than only the first.
    #[test]
    fn agent_node_binds_the_full_upstream_output() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);
        // Every translated agent node carries the binding…
        for node in graph.nodes.iter().filter(|n| n.kind == NodeKind::Agent) {
            assert_eq!(
                node.config["input"], "=items",
                "agent node {} must bind the full upstream output set",
                node.id
            );
        }
        // …and a non-agent node does not (the binding is agent-specific).
        let research = graph.nodes.iter().find(|n| n.id == "research").unwrap();
        assert!(
            research.config.get("input").is_none(),
            "a tool_call node carries no upstream-output binding"
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

    // --- P1: config overlay + error/retry/approval + error routing ---------

    /// A snapshot pinning the shipped campaign pipeline's translated config. Most
    /// nodes carry only kind-derived config; the `research` `tool_call` binds the
    /// metered `web_search` slug + args and an `on_error = "continue"` policy
    /// (#530), and `publish` is an `agent` assembly step (there is no CMS to POST
    /// to), so each carries exactly the config those choices imply.
    #[test]
    fn campaign_translation_lowers_to_the_expected_engine_config() {
        let file = parse_workflow(CAMPAIGN).expect("campaign parses");
        let graph = translate(&file);
        let config = |id: &str| {
            graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.config.clone())
                .unwrap()
        };
        assert_eq!(config("brief"), json!({}));
        assert_eq!(
            config("strategist"),
            json!({
                "agent_ref": "brand_strategist",
                "prompt": "Turns the brief into an angle + outline.",
                // Issue #782: the upstream-output binding every agent node carries.
                "input": "=items",
                // Issue #881: the node's own id, so the agent capability can say
                // WHICH node blocked when its turn parks an approval.
                "node_id": "strategist"
            })
        );
        // The gate carries its boolean discriminant (issue #661): a condition
        // node must name the `field` it branches on.
        assert_eq!(config("gate"), json!({ "field": "=item.needs_research" }));
        assert_eq!(
            config("research"),
            json!({
                "slug": "web_search",
                "args": { "query": "=item.text", "max_results": 5 },
                "on_error": "continue"
            })
        );
        assert_eq!(
            config("publish"),
            json!({
                "agent_ref": "copywriter",
                "prompt": "Assemble the publish-ready post and hero-image reference, then hand off for operator sign-off.",
                // Issue #782: the upstream-output binding every agent node carries.
                "input": "=items",
                // Issue #881: as above.
                "node_id": "publish"
            })
        );
        assert_eq!(config("done"), json!({}));
    }

    /// A `tool_call` node's config `slug` is carried into the engine config
    /// verbatim (issue #661 removed the node-id placeholder fallback; author-time
    /// validation now guarantees a real slug is always present).
    #[test]
    fn config_slug_is_carried_into_engine_config() {
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
            [node.config]
            slug = "csv_export"
            [node.config.args]
            filename = "out.csv"
            [[edge]]
            from = "start"
            to = "call"
        "#;
        let graph = translate(&parse_workflow(src).expect("parses"));
        let call = graph.nodes.iter().find(|n| n.id == "call").unwrap();
        assert_eq!(call.config["slug"], "csv_export");
        assert_eq!(call.config["args"]["filename"], "out.csv");
    }

    /// A config that tries to spoof `agent_ref` cannot win: the first-class
    /// `agent` field is written last, so the roster binding is authoritative.
    /// (The model would reject this config at parse time; here we build the node
    /// directly to prove the layering order in `translate` itself.)
    #[test]
    fn agent_ref_survives_a_spoofing_config() {
        use crate::company::{WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};
        let file = WorkflowFile {
            global: false,
            id: "wf".into(),
            name: "WF".into(),
            description: None,
            owner_desk: None,
            nodes: vec![WorkflowNodeDef {
                id: "worker".into(),
                kind: WorkflowNodeKind::Agent,
                name: "Worker".into(),
                summary: None,
                agent: Some("real".into()),
                schedule: None,
                config: Some(json!({ "agent_ref": "impostor" })),
                on_error: None,
                retry: None,
                requires_approval: None,
                repeatable: None,
                destination: None,
                postcondition: None,
            }],
            edges: Vec::new(),
        };
        let graph = translate(&file);
        assert_eq!(graph.nodes[0].config["agent_ref"], "real");
    }

    /// `on_error` / `retry` / `requires_approval` land as the exact config keys
    /// the tinyflows engine reads.
    #[test]
    fn error_retry_approval_land_as_engine_config_keys() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            requires_approval = true
            [[node]]
            id = "call"
            kind = "tool_call"
            name = "Call"
            on_error = "continue"
            [node.config]
            slug = "csv_export"
            [node.retry]
            max_attempts = 3
            backoff_ms = 250
            backoff = "exponential"
            [[edge]]
            from = "start"
            to = "call"
        "#;
        let graph = translate(&parse_workflow(src).expect("parses"));
        let start = graph.nodes.iter().find(|n| n.id == "start").unwrap();
        assert_eq!(start.config["requires_approval"], true);
        let call = graph.nodes.iter().find(|n| n.id == "call").unwrap();
        assert_eq!(call.config["on_error"], "continue");
        assert_eq!(call.config["retry"]["max_attempts"], 3);
        assert_eq!(call.config["retry"]["backoff_ms"], 250);
        assert_eq!(call.config["retry"]["backoff"], "exponential");
    }

    /// Issue #1866: an agent node's declared `postcondition` lands as the
    /// exact config key `HarnessAgentRunner::run_turn` reads it back from —
    /// the same first-class-field-becomes-config-key contract `retry` and
    /// `requires_approval` already have, pinned above.
    #[test]
    fn postcondition_lands_as_an_engine_config_key() {
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
            agent = "researcher"
            [node.postcondition]
            require = "field_present"
            field = "json.items"
            [[edge]]
            from = "start"
            to = "worker"
        "#;
        let graph = translate(&parse_workflow(src).expect("parses"));
        let worker = graph.nodes.iter().find(|n| n.id == "worker").unwrap();
        assert_eq!(worker.config["postcondition"]["require"], "field_present");
        assert_eq!(worker.config["postcondition"]["field"], "json.items");
    }

    /// The other half of the same contract: a node with no `postcondition`
    /// declared carries no `postcondition` key at all — translation of an
    /// unchanged (pre-#1866) file stays byte-identical, matching every other
    /// first-class field's `Option<T>` -> `if let Some` -> config-key
    /// pattern above.
    #[test]
    fn a_node_without_a_postcondition_carries_no_postcondition_key() {
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
            agent = "researcher"
            [[edge]]
            from = "start"
            to = "worker"
        "#;
        let graph = translate(&parse_workflow(src).expect("parses"));
        let worker = graph.nodes.iter().find(|n| n.id == "worker").unwrap();
        assert!(
            worker.config.get("postcondition").is_none(),
            "an undeclared postcondition must not appear in the lowered config at all: {:?}",
            worker.config
        );
    }

    /// An "error"-labeled edge leaving a routing node maps onto the engine's
    /// `error` port; a non-error edge from the same node stays on `main`.
    #[test]
    fn error_label_maps_to_error_port() {
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
            [node.config]
            slug = "csv_export"
            [[node]]
            id = "ok"
            kind = "output"
            name = "OK"
            [[node]]
            id = "recover"
            kind = "output"
            name = "Recover"
            [[edge]]
            from = "start"
            to = "call"
            [[edge]]
            from = "call"
            to = "ok"
            [[edge]]
            from = "call"
            to = "recover"
            label = "error"
        "#;
        let graph = translate(&parse_workflow(src).expect("parses"));
        let port = |to: &str| {
            graph
                .edges
                .iter()
                .find(|e| e.from_node == "call" && e.to_node == to)
                .map(|e| e.from_port.clone())
        };
        assert_eq!(port("recover").as_deref(), Some("error"));
        assert_eq!(port("ok").as_deref(), Some("main"));
    }

    /// A node that is BOTH a `condition` and `on_error = "route"` must route its
    /// `"error"`-labeled edge onto the `error` port — not the `true` port. The
    /// error-port check runs before the condition branch precisely so the
    /// `"error"` label is never funneled through `condition_port` (which maps any
    /// non-negative label, including `"error"`, to `true`). Its `yes`/`no` branch
    /// edges must still map to `true`/`false`.
    #[test]
    fn condition_node_with_route_sends_error_edge_to_error_port() {
        use crate::company::{WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};
        let file = WorkflowFile {
            global: false,
            id: "wf".into(),
            name: "WF".into(),
            description: None,
            owner_desk: None,
            nodes: vec![
                WorkflowNodeDef {
                    id: "gate".into(),
                    kind: WorkflowNodeKind::Condition,
                    name: "Gate".into(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: Some("route".into()),
                    retry: None,
                    requires_approval: None,
                    repeatable: None,
                    destination: None,
                    postcondition: None,
                },
                node_stub("yes_path"),
                node_stub("no_path"),
                node_stub("recover"),
            ],
            edges: vec![
                edge_stub("gate", "yes_path", Some("yes")),
                edge_stub("gate", "no_path", Some("no")),
                edge_stub("gate", "recover", Some("error")),
            ],
        };
        let graph = translate(&file);
        let port = |to: &str| {
            graph
                .edges
                .iter()
                .find(|e| e.from_node == "gate" && e.to_node == to)
                .map(|e| e.from_port.clone())
        };
        // The error edge wins the error port; the branch edges keep true/false.
        assert_eq!(port("recover").as_deref(), Some("error"));
        assert_eq!(port("yes_path").as_deref(), Some("true"));
        assert_eq!(port("no_path").as_deref(), Some("false"));
    }

    // --- P2: the twelve-kind map + switch ports ----------------------------

    /// Every OpenCompany kind lowers to its tinyflows counterpart; the P2 kinds
    /// map one-to-one and `output` still lowers to a pass-through `transform`.
    #[test]
    fn twelve_kind_map_is_total() {
        use crate::company::{WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};
        let kinds = [
            (WorkflowNodeKind::Trigger, NodeKind::Trigger),
            (WorkflowNodeKind::Agent, NodeKind::Agent),
            (WorkflowNodeKind::ToolCall, NodeKind::ToolCall),
            (WorkflowNodeKind::HttpRequest, NodeKind::HttpRequest),
            (WorkflowNodeKind::Condition, NodeKind::Condition),
            (WorkflowNodeKind::Output, NodeKind::Transform),
            (WorkflowNodeKind::Switch, NodeKind::Switch),
            (WorkflowNodeKind::Merge, NodeKind::Merge),
            (WorkflowNodeKind::SplitOut, NodeKind::SplitOut),
            (WorkflowNodeKind::Transform, NodeKind::Transform),
            (WorkflowNodeKind::OutputParser, NodeKind::OutputParser),
            (WorkflowNodeKind::SubWorkflow, NodeKind::SubWorkflow),
        ];
        for (oc, tf) in kinds {
            let file = WorkflowFile {
                global: false,
                id: "wf".into(),
                name: "WF".into(),
                description: None,
                owner_desk: None,
                nodes: vec![WorkflowNodeDef {
                    id: "n".into(),
                    kind: oc,
                    name: "N".into(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    repeatable: None,
                    destination: None,
                    postcondition: None,
                }],
                edges: Vec::new(),
            };
            assert_eq!(translate(&file).nodes[0].kind, tf, "{oc:?} → {tf:?}");
        }
    }

    /// An edge leaving a `switch` carries its label VERBATIM as the branch port;
    /// an unlabeled switch edge falls to the engine's `default` fallback port.
    #[test]
    fn switch_labels_map_to_verbatim_ports() {
        use crate::company::{WorkflowFile, WorkflowNodeDef, WorkflowNodeKind};
        let file = WorkflowFile {
            global: false,
            id: "wf".into(),
            name: "WF".into(),
            description: None,
            owner_desk: None,
            nodes: vec![
                WorkflowNodeDef {
                    id: "sw".into(),
                    kind: WorkflowNodeKind::Switch,
                    name: "Switch".into(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    repeatable: None,
                    destination: None,
                    postcondition: None,
                },
                node_stub("paid"),
                node_stub("error_case"),
                node_stub("fallthrough"),
            ],
            edges: vec![
                edge_stub("sw", "paid", Some("paid")),
                // `error` is a legitimate case name on a switch — carried verbatim,
                // NOT mapped onto the engine's `error` port.
                edge_stub("sw", "error_case", Some("error")),
                edge_stub("sw", "fallthrough", None),
            ],
        };
        let graph = translate(&file);
        let port = |to: &str| {
            graph
                .edges
                .iter()
                .find(|e| e.from_node == "sw" && e.to_node == to)
                .map(|e| e.from_port.clone())
        };
        assert_eq!(port("paid").as_deref(), Some("paid"));
        assert_eq!(port("error_case").as_deref(), Some("error"));
        assert_eq!(port("fallthrough").as_deref(), Some("default"));
    }

    /// **The delivery invariant (issue #170).** An `output` node's
    /// `destination` is host-side routing, not engine config: it must NOT reach
    /// the compiled graph. A destination-bearing output node lowers to exactly
    /// the same bare pass-through `Transform` it lowered to before the field
    /// existed — so translation of a graph is unaffected by where its report
    /// goes, and the engine never gains an inert key it does not understand.
    #[test]
    fn an_output_destination_never_reaches_the_engine_graph() {
        let with_destination = crate::company::parse_workflow(
            r#"
id = "wf"
name = "WF"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Report"
[node.destination]
kind = "email"
target = "ada@example.com"
[[edge]]
from = "start"
to = "done"
"#,
        )
        .expect("parses");
        let without = crate::company::parse_workflow(
            r#"
id = "wf"
name = "WF"
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
"#,
        )
        .expect("parses");

        let done = |file: &crate::company::WorkflowFile| {
            translate(file)
                .nodes
                .into_iter()
                .find(|n| n.id == "done")
                .expect("output node lowered")
        };
        let with = done(&with_destination);
        let plain = done(&without);

        assert_eq!(with.kind, NodeKind::Transform, "output lowers to transform");
        // A bare pass-through: no `set` bindings, and above all no destination.
        assert_eq!(with.config, json!({}));
        assert_eq!(
            with.config, plain.config,
            "a destination must not change the engine config"
        );
    }

    fn node_stub(id: &str) -> crate::company::WorkflowNodeDef {
        crate::company::WorkflowNodeDef {
            id: id.into(),
            kind: crate::company::WorkflowNodeKind::Output,
            name: id.into(),
            summary: None,
            agent: None,
            schedule: None,
            config: None,
            on_error: None,
            retry: None,
            requires_approval: None,
            repeatable: None,
            destination: None,
            postcondition: None,
        }
    }

    fn edge_stub(from: &str, to: &str, label: Option<&str>) -> crate::company::WorkflowEdgeDef {
        crate::company::WorkflowEdgeDef {
            from: from.into(),
            to: to.into(),
            label: label.map(Into::into),
        }
    }
}
