//! Workflow graph files: `companies/<name>/workflows/<id>.toml`.
//!
//! Each enabled workflow is a data-only node/edge graph edited by the Workflow
//! canvas and referenced by `[workflows].enabled` in the manifest. This module
//! parses those files into a validated [`WorkflowFile`], reporting every problem
//! at once in prosumer language, matching [`super::manifest`].
//!
//! A `trigger` node may carry a `schedule`: a standard 5-field cron expression,
//! **always interpreted in UTC**, in the same dialect as the manifest's
//! `[[schedule]]` entries. It is validated here with
//! [`CronExpr`](crate::runtime::cron::CronExpr) and driven at runtime by
//! [`WorkflowScheduler`](crate::runtime::workflow_scheduler::WorkflowScheduler),
//! so a saved schedule actually fires instead of sitting in prose. No other node
//! kind may carry one, and a graph may carry at most one scheduled trigger — a
//! schedule says when the whole workflow runs, so two would double-run it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{OpenCompanyError, Result};

/// The node kinds a workflow graph may use — the OpenCompany authoring
/// contract. The first six are the original set; the trailing six (P2) add the
/// data-shape nodes (`switch` / `merge` / `split_out` / `transform` /
/// `output_parser`) and `sub_workflow` composition. Each string is tinyflows'
/// snake_case wire kind verbatim, but this set is deliberately *narrower* than
/// tinyflows' full engine catalog (`tinyflows`'s `NODE_KINDS`): the engine-only
/// kinds are refused at parse. The accepted-vs-rejected contract, and why each
/// engine-only kind is left out, is documented in
/// `docs/spec/runtime/workflow-vocabulary.md`.
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

/// The destination kinds an `output` node may route its report to.
///
/// Deliberately closed: each kind has its own server-side resolution and its
/// own policy gate (see [`crate::workflows::delivery`]), so a new kind is a
/// deliberate addition, never a free-form string an author can invent.
pub const WORKFLOW_DESTINATION_KINDS: &[&str] = &["owner", "email", "channel"];

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
    /// Whether this graph came from the global baseline ([`crate::globals`])
    /// rather than the company's own `workflows/` directory or its saved
    /// overlays.
    ///
    /// Provenance, for a console that has to say where a graph a company never
    /// wrote came from. It changes no behaviour here: precedence is decided by
    /// id, in [`load_workflow_union`] and [`list_workflows_union`].
    pub global: bool,
}

impl WorkflowFile {
    /// The cron this graph fires on, if any — the first `trigger` node carrying
    /// a `schedule`.
    ///
    /// This is the single definition of "is this workflow automatic", shared by
    /// [`WorkflowScheduler::tick`](crate::runtime::WorkflowScheduler), which uses
    /// it to decide what to fire, and by the disarm rule in `workflow_create.rs`,
    /// which uses it to decide what must not fire unreviewed. Two copies of this
    /// predicate that disagreed would mean a workflow the host considers manual
    /// and the scheduler considers armed — silently the exact hole issue #276
    /// exists to close.
    ///
    /// Reads the **trigger** only. A `schedule` on any other node kind is inert
    /// (validation permits the field on the node struct; only a trigger's is
    /// load-bearing), so honouring one elsewhere would arm a workflow the canvas
    /// does not show as scheduled.
    pub fn trigger_schedule(&self) -> Option<&str> {
        self.nodes
            .iter()
            .find(|node| node.kind == WorkflowNodeKind::Trigger && node.schedule.is_some())
            .and_then(|node| node.schedule.as_deref())
    }

    /// Whether this graph has any node that could actually do something
    /// (issue #976).
    ///
    /// A `trigger` says *when* a workflow runs, never *what* it does, so a graph
    /// whose nodes are all triggers is stage-less: the engine executes it
    /// happily, no stage fails because there is no stage, and it settles as an
    /// ordinary finished run. On staging that produced `QA Test Pipeline` with
    /// six recorded runs that could not have done anything, and `campaign`
    /// holding a schedule it cannot keep.
    ///
    /// Expressed as "is there a non-trigger node" rather than a node **count**,
    /// which is the tempting shortcut and is wrong twice: a graph of three
    /// triggers has three nodes and still does nothing, and a legitimate
    /// one-stage graph (trigger → agent) has only two. What matters is whether
    /// any node kind *executes*, and every kind except `Trigger` does.
    ///
    /// The single definition, shared by the arming refusal in
    /// `workflow_create.rs` and the run notice in
    /// [`workflows::runner`](crate::workflows::runner) — for the reason spelled
    /// out on [`trigger_schedule`](Self::trigger_schedule) just above: two copies
    /// of a predicate that disagreed would let a graph be refused a schedule and
    /// still run silently, or the reverse.
    pub fn has_runnable_node(&self) -> bool {
        self.nodes
            .iter()
            .any(|node| node.kind != WorkflowNodeKind::Trigger)
    }
}

/// What a run of a stage-less graph records (issue #976).
///
/// Carried as a run **notice**, not an error: an empty graph is not a failure.
/// Nothing broke and nothing was attempted, so marking the run failed would put
/// a half-authored stub into the failure count beside runs that genuinely went
/// wrong — the same call [`DeliveryReason::NoDestinationConfigured`] makes one
/// level down (issue #925), and the same one #638 already made for the
/// approval-overflow notice this rides alongside.
///
/// A literal, like every other notice: nothing runtime-supplied reaches an
/// operator surface through it.
///
/// [`DeliveryReason::NoDestinationConfigured`]: crate::ports::DeliveryReason::NoDestinationConfigured
pub const STAGELESS_WORKFLOW_NOTICE: &str = "This workflow has no stage to run — its only node is the trigger that starts it — so this \
     run did nothing. Add at least one node after the trigger and run it again.";

/// Why arming a stage-less workflow's schedule is refused (issue #976).
///
/// Says what is wrong, why it matters, and the one thing the operator can do —
/// the shape [`DrainedRequests::overflow_notice`](crate::harness::policy::DrainedRequests::overflow_notice)
/// established for telling somebody their work will not happen.
pub const STAGELESS_SCHEDULE_REFUSAL: &str = "This workflow has no stage to run — its only node is the trigger that starts it — so a \
     schedule would fire on time and do nothing. Add at least one node after the trigger, then \
     switch it on.";

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
    /// A standard 5-field cron expression saying *when* this workflow starts on
    /// its own — only meaningful on `trigger` nodes, and always **UTC**.
    ///
    /// Same dialect as the manifest's `[[schedule]]` crons: it is parsed by
    /// [`CronExpr`](crate::runtime::cron::CronExpr) at validation, so a
    /// malformed expression is rejected with the parser's own prosumer message
    /// rather than being persisted as inert prose. `None` (the default) means
    /// the workflow only runs when something else starts it — an operator
    /// clicking Run, the REST run route, or another workflow.
    pub schedule: Option<String>,
    /// Free-form, kind-specific node configuration ([`tool_call`] slug/args,
    /// [`http_request`] descriptor, …). Layered under the derived defaults and
    /// the first-class fields below by [`translate`](crate::workflows::translate)
    /// before it reaches the engine. Reserved keys (`on_error` / `retry` /
    /// `requires_approval` / `schedule`, plus `agent_ref` on `agent` nodes) are
    /// rejected at validation so they cannot silently shadow a first-class
    /// field.
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
    /// Where this node's report is delivered once the run finishes — `output`
    /// nodes only. `None` (every legacy graph) keeps the pre-#170 behaviour: the
    /// value surfaces in the run-result drawer and goes nowhere else.
    pub destination: Option<WorkflowDestinationDef>,
}

/// Where an `output` node's report goes when the run completes.
///
/// **The engine never sees this.** Delivery executes host-side, after
/// `tinyflows::engine::run` returns, in
/// [`deliver_outputs`](crate::workflows::delivery::deliver_outputs) — so this is
/// not engine config and must not live in [`WorkflowNodeDef::config`], where it
/// would be an inert key silently riding into the engine graph. It is first-class
/// model data for the same reason [`WorkflowRetryDef`] is: the console and
/// validation see exactly one shape.
///
/// Each `kind` carries a different target contract, enforced by
/// [`validate`]:
///
/// | `kind`    | `target`                      | Who it reaches |
/// |-----------|-------------------------------|----------------|
/// | `owner`   | must be **absent**            | the company's active Admin users, else the operator channel |
/// | `email`   | required, must contain `@`    | that address — **only** if the company grants `email` and the recipient is an established thread |
/// | `channel` | required, a wired channel id  | that [`ChannelAdapter`](crate::ports::ChannelAdapter) |
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct WorkflowDestinationDef {
    /// One of [`WORKFLOW_DESTINATION_KINDS`].
    /// `#[serde(default)]` like every other field on the raw shapes: an omitted
    /// `kind` becomes a prosumer-language validation problem rather than a raw
    /// serde trace out of the TOML parser or the create route.
    #[serde(default)]
    pub kind: String,
    /// The recipient address (`email`) or channel id (`channel`). Absent for
    /// `owner`, which the host resolves from the company's own directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
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
    /// The trigger node's 5-field UTC cron. Declared before `config` so the
    /// rendered TOML keeps every scalar above the `[node.config]` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) schedule: Option<String>,
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
    /// Kept LAST in the struct: `toml::to_string` refuses to emit a scalar after
    /// a table, so a table-valued field must not be followed by a scalar one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) destination: Option<WorkflowDestinationDef>,
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

/// Parses stored workflow TOML back into an editable [`RawWorkflow`] draft — the
/// inverse of [`render_workflow`], and the seam a rollback (issue #274) uses to
/// re-feed a captured revision through the ordinary update path.
///
/// `RawWorkflow` is the same serde shape both directions (it is what
/// [`render_workflow`] emits and what [`parse_workflow`] validates), so a body
/// that was persisted through the create/update path round-trips here verbatim.
/// A parse failure is an [`InvalidRequest`](OpenCompanyError::InvalidRequest)
/// (400) rather than the 500 a malformed on-disk file gets, because the only
/// caller feeds it straight back into the validating update path.
pub(crate) fn raw_workflow_from_toml(toml_src: &str) -> Result<RawWorkflow> {
    toml::from_str(toml_src).map_err(|err| {
        OpenCompanyError::InvalidRequest(format!("workflow revision is unreadable: {err}"))
    })
}

/// A stored graph split into the part an *agent* authoring schema can express
/// and the part it cannot (issue #661, M7).
///
/// The agent-facing `create_workflow` / `update_workflow` tools deliberately
/// carry a narrower node shape than the REST body: `schedule`, `on_error`,
/// `retry` and `requires_approval` are unattended-run **policy**, reserved for
/// an operator (see `CreateWorkflowArgs` in
/// [`crate::harness::orchestrator`]). That narrowing is safe on *create* — a
/// graph with no policy fields is simply authored without them — but on a
/// full-replacement *edit* it is a hazard: replaying a read graph back through
/// the narrow schema would silently drop whatever policy an operator had put on
/// it, and dropping a `requires_approval` is removing a gate rather than
/// forgetting a field.
///
/// So the projection is explicit about both halves rather than emitting the
/// spec and hoping. [`Self::spec`] round-trips; [`Self::unexpressible`] is the
/// evidence the write tools refuse on and the read tool reports.
///
/// Gated with the agent tools that are its only consumer
/// (`crate::harness::workflow_admin`), the same way `courtesy_validate_draft`
/// is gated with the builder it serves: in a default build this would be dead
/// code.
#[cfg(feature = "openhuman")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowSpecProjection {
    /// `{id, name, description?, nodes[], edges[]}` — exactly the JSON the
    /// agent `update_workflow` tool accepts, so a graph read here can be edited
    /// and handed straight back without reshaping.
    pub(crate) spec: serde_json::Value,
    /// The trigger's cron, when the stored body carries one. Read-only: an
    /// agent can neither author nor preserve one.
    pub(crate) schedule: Option<String>,
    /// Every per-node policy field [`Self::spec`] cannot carry, in node order:
    /// `(node id, [(field name, rendered value)])`. Empty means the whole graph
    /// survives a round trip through the agent schema.
    pub(crate) unexpressible: Vec<(String, Vec<(&'static str, String)>)>,
}

#[cfg(feature = "openhuman")]
impl WorkflowSpecProjection {
    /// A one-line, agent-readable rendering of [`Self::unexpressible`], e.g.
    /// ``node `review` (requires_approval), node `fetch` (on_error, retry)``.
    /// Empty string when nothing is unexpressible.
    pub(crate) fn unexpressible_summary(&self) -> String {
        self.unexpressible
            .iter()
            .map(|(node, fields)| {
                let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
                format!("node `{node}` ({})", names.join(", "))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Projects a stored [`RawWorkflow`] onto the agent authoring schema, keeping
/// the residue (see [`WorkflowSpecProjection`]).
///
/// Additive and read-only: it builds a fresh JSON value and touches nothing.
/// It lives here, beside [`raw_workflow_from_toml`] whose output it consumes,
/// rather than in `workflow_create.rs` — the two are a read pair, and the
/// create module is the busier merge surface.
///
/// `config` is converted TOML → JSON, which cannot fail in this direction (TOML
/// has no shape JSON lacks; the lossy direction is the one
/// `raw_workflow_from_spec` guards).
#[cfg(feature = "openhuman")]
pub(crate) fn project_workflow_spec(raw: &RawWorkflow) -> WorkflowSpecProjection {
    let mut nodes = Vec::with_capacity(raw.nodes.len());
    let mut unexpressible = Vec::new();
    let mut schedule = None;

    for node in &raw.nodes {
        let mut entry = serde_json::Map::new();
        entry.insert("id".into(), serde_json::Value::String(node.id.clone()));
        entry.insert("kind".into(), serde_json::Value::String(node.kind.clone()));
        entry.insert("name".into(), serde_json::Value::String(node.name.clone()));
        if let Some(summary) = &node.summary {
            entry.insert("summary".into(), serde_json::Value::String(summary.clone()));
        }
        if let Some(agent) = &node.agent {
            entry.insert("agent".into(), serde_json::Value::String(agent.clone()));
        }
        if let Some(config) = &node.config
            && let Ok(json) = serde_json::to_value(config)
        {
            entry.insert("config".into(), json);
        }
        if let Some(destination) = &node.destination
            && let Ok(json) = serde_json::to_value(destination)
        {
            entry.insert("destination".into(), json);
        }
        nodes.push(serde_json::Value::Object(entry));

        // The residue. `schedule` is collected separately because it is a
        // property of the *workflow* (only a trigger's is load-bearing — see
        // [`WorkflowFile::trigger_schedule`]), not of the node an operator
        // would go edit.
        if node.kind == WorkflowNodeKind::Trigger.as_str()
            && let Some(cron) = &node.schedule
        {
            schedule = Some(cron.clone());
        }
        let mut fields: Vec<(&'static str, String)> = Vec::new();
        if let Some(on_error) = &node.on_error {
            fields.push(("on_error", on_error.clone()));
        }
        if let Some(retry) = &node.retry {
            fields.push((
                "retry",
                serde_json::to_string(retry).unwrap_or_else(|_| "set".to_string()),
            ));
        }
        if let Some(requires_approval) = node.requires_approval {
            fields.push(("requires_approval", requires_approval.to_string()));
        }
        if !fields.is_empty() {
            unexpressible.push((node.id.clone(), fields));
        }
    }

    let edges: Vec<serde_json::Value> = raw
        .edges
        .iter()
        .map(|edge| {
            let mut entry = serde_json::Map::new();
            entry.insert("from".into(), serde_json::Value::String(edge.from.clone()));
            entry.insert("to".into(), serde_json::Value::String(edge.to.clone()));
            if let Some(label) = &edge.label {
                entry.insert("label".into(), serde_json::Value::String(label.clone()));
            }
            serde_json::Value::Object(entry)
        })
        .collect();

    let mut spec = serde_json::Map::new();
    spec.insert("id".into(), serde_json::Value::String(raw.id.clone()));
    spec.insert("name".into(), serde_json::Value::String(raw.name.clone()));
    if let Some(description) = &raw.description {
        spec.insert(
            "description".into(),
            serde_json::Value::String(description.clone()),
        );
    }
    spec.insert("nodes".into(), serde_json::Value::Array(nodes));
    spec.insert("edges".into(), serde_json::Value::Array(edges));

    WorkflowSpecProjection {
        spec: serde_json::Value::Object(spec),
        schedule,
        unexpressible,
    }
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

    // Lenient (issue #682): the read/load path must not hard-fail a saved graph
    // on the new #661 author-time rules — those are enforced strictly on the
    // create/update draft path and the seed corpus test. Every structural check
    // still runs here; only the two #661 rules are skipped.
    let problems = validate(&raw, false);
    if !problems.is_empty() {
        return Err(OpenCompanyError::DataInvalid { path, problems });
    }

    Ok(WorkflowFile {
        id: raw.id,
        name: raw.name,
        description: raw.description,
        // Set by whoever merges the baseline in; this parser reads company
        // graphs and global ones through the same path.
        global: false,
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
                schedule: node.schedule,
                // Validation rejects TOML's non-finite floats before this
                // conversion because JSON cannot represent them.
                config: node.config.map(|value| {
                    serde_json::to_value(value)
                        .expect("validated TOML config always converts to JSON")
                }),
                on_error: node.on_error,
                retry: node.retry,
                requires_approval: node.requires_approval,
                destination: node.destination,
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

/// Loads one workflow graph by id from the **union** of a company's two graph
/// sources: the version-controlled seed file (`source_dir/workflows/<id>.toml`)
/// and the record's runtime-authored [`OverlayWorkflow`] bodies.
///
/// This is the single read path for "give me graph `<id>`" — the REST
/// `GET …/workflows/{wid}` and run routes, the GraphQL resolver, the
/// orchestrator's `run_workflow` tool, and the `sub_workflow` resolver all go
/// through it, so they can never disagree about which graphs exist.
///
/// **The seed file wins on an id collision.** An overlay body with the same id
/// as a committed file is shadowed, not destroyed — it stays on the record and
/// resurfaces if the file goes away. This matches the manifest-first convention
/// [`CompanyRecord::effective_desk_members`](crate::ports::types::CompanyRecord::effective_desk_members)
/// already uses: the version-controlled definition is authoritative.
///
/// `Ok(None)` means neither source has that id — the caller's clean 404. An
/// `Err` means the body that *was* found is malformed (the same error a
/// hand-authored file would give).
pub fn load_workflow_union(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
    id: &str,
) -> Result<Option<WorkflowFile>> {
    load_company_workflow_union(source_dir, overlays, id)
}

/// [`load_workflow_union`], honouring a company's `[globals].disable`.
///
/// The same read, with the one thing the two-source form cannot see: whether
/// this company opted out of the global graph it would otherwise resolve to.
/// Callers that hold the record pass `record.manifest.globals.disable`; the
/// shorter form exists for the ones that do not, and resolves globals as if
/// nothing were disabled — which is what an operator who wrote no opt-out gets
/// either way.
///
/// Precedence is seed file, then overlay, then global: the company's committed
/// graph wins, its console-authored graph wins over the baseline, and the
/// baseline fills what neither defines. A global graph is never *merged* into a
/// same-id company graph — nobody designed the half of each.
pub fn load_workflow_with_globals(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
    disable: &[String],
    id: &str,
) -> Result<Option<WorkflowFile>> {
    if let Some(file) = load_company_workflow_union(source_dir, overlays, id)? {
        return Ok(Some(file));
    }
    if crate::globals::disabled(disable, "workflow", id) {
        return Ok(None);
    }
    Ok(crate::globals::workflows()
        .iter()
        .find(|workflow| workflow.id == id)
        .cloned()
        .map(|mut workflow| {
            workflow.global = true;
            workflow
        }))
}

/// The company's own two sources — seed file, then overlay — with no baseline.
fn load_company_workflow_union(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
    id: &str,
) -> Result<Option<WorkflowFile>> {
    if let Some(dir) = source_dir {
        let path = dir.join("workflows").join(format!("{id}.toml"));
        // Only load ids that exist on disk, so a missing file falls through to
        // the overlay rather than becoming a `DataRead` error.
        if path.is_file() {
            let ids = [id.to_string()];
            return load_company_workflows(dir, &ids).map(|mut files| files.pop());
        }
    }

    let Some(overlay) = overlays.iter().find(|w| w.id == id) else {
        return Ok(None);
    };
    // Re-label parse/validation errors with the id, matching how the on-disk
    // loader re-labels them with the real path.
    let labelled = PathBuf::from(format!("{id}.toml"));
    match parse_workflow(&overlay.toml) {
        Ok(workflow) => Ok(Some(workflow)),
        Err(OpenCompanyError::DataInvalid { problems, .. }) => Err(OpenCompanyError::DataInvalid {
            path: labelled,
            problems,
        }),
        Err(OpenCompanyError::DataParse { message, .. }) => Err(OpenCompanyError::DataParse {
            path: labelled,
            message,
        }),
        Err(other) => Err(other),
    }
}

/// Every workflow graph a company has, from the **union** of its seed
/// `workflows/*.toml` files and its runtime-authored
/// [`OverlayWorkflow`](crate::ports::types::OverlayWorkflow) bodies.
///
/// The seed scan comes first (in stable id order, via
/// [`list_source_workflows`]), then overlay graphs the scan did not already
/// yield, in stable id order — so a seed file **wins** over an overlay of the
/// same id, the same precedence [`load_workflow_union`] applies. A malformed
/// overlay body skips only itself (logged), the same tolerance the seed scan
/// has, so one bad graph never hides the rest.
pub fn list_workflows_union(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
) -> Vec<WorkflowFile> {
    list_company_workflows_union(source_dir, overlays)
}

/// [`list_workflows_union`], honouring a company's `[globals].disable`.
///
/// Global graphs are listed **last**, after the company's own seeds and saved
/// overlays and only for ids neither of those already carries — the same
/// precedence [`load_workflow_with_globals`] applies, so the picker and the
/// loader can never disagree about which graph an id means.
pub fn list_workflows_with_globals(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
    disable: &[String],
) -> Vec<WorkflowFile> {
    let mut files = list_company_workflows_union(source_dir, overlays);
    // Reserved by *claim*, not by successful parse: a malformed seed file or
    // overlay still names an id the company owns, and `load_workflow_with_globals`
    // resolves that company definition first — parse error and all — before it
    // would ever fall through to a global. Using only `files`' ids here (the ones
    // that parsed) would let a same-id global slip into the list even though the
    // loader can never actually return it, exposing an entry this list cannot
    // back.
    let reserved = reserved_company_workflow_ids(source_dir, overlays);
    for workflow in crate::globals::workflows() {
        if reserved.contains(&workflow.id)
            || crate::globals::disabled(disable, "workflow", &workflow.id)
        {
            continue;
        }
        let mut workflow = workflow.clone();
        workflow.global = true;
        files.push(workflow);
    }
    files
}

/// Every workflow id a company's own two sources claim, whether or not the
/// file actually parses: `workflows/*.toml` file stems plus every saved
/// overlay's `id`. See [`list_workflows_with_globals`] for why a malformed
/// source must still reserve its id.
fn reserved_company_workflow_ids(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
) -> std::collections::HashSet<String> {
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(dir) = source_dir
        && let Ok(entries) = std::fs::read_dir(dir.join("workflows"))
    {
        ids.extend(
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
                .filter_map(|path| {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_string)
                }),
        );
    }
    ids.extend(overlays.iter().map(|overlay| overlay.id.clone()));
    ids
}

/// The company's own two sources — seed files, then saved overlays — and no
/// baseline. [`list_workflows_union`] is exactly this, named for its callers.
fn list_company_workflows_union(
    source_dir: Option<&Path>,
    overlays: &[crate::ports::types::OverlayWorkflow],
) -> Vec<WorkflowFile> {
    let mut files = list_source_workflows(source_dir);
    let mut seen: std::collections::HashSet<String> = files.iter().map(|f| f.id.clone()).collect();

    let mut extra: Vec<&crate::ports::types::OverlayWorkflow> = overlays
        .iter()
        .filter(|overlay| !seen.contains(&overlay.id))
        .collect();
    extra.sort_by(|a, b| a.id.cmp(&b.id));

    for overlay in extra {
        // Two overlay entries with the same id can only happen on a corrupted
        // record; keep the first and skip the rest rather than double-listing.
        if !seen.insert(overlay.id.clone()) {
            continue;
        }
        match parse_workflow(&overlay.toml) {
            Ok(file) => files.push(file),
            Err(err) => {
                tracing::warn!(workflow = %overlay.id, error = %err, "skipping malformed saved workflow")
            }
        }
    }

    files
}

/// Collects every validation problem in prosumer language. Empty means valid.
///
/// `strict` picks the severity surface (issue #682). The two NEW #661 author-time
/// rules — per-kind required `config` ([`required_config_problems`]) and the
/// `condition` branch `yes`/`no` label rule — run ONLY when `strict` is true.
/// The read/load path ([`parse_workflow`]) calls this with `strict = false` so a
/// graph persisted before #661 (a field-less condition, an off-vocabulary branch
/// label — shapes the console couldn't even emit until this change) still LOADS
/// and runs with today's behaviour, instead of hard-failing on every read and
/// vanishing from the console / silently halting a scheduled run. Author-time
/// enforcement lives on the create/update draft path
/// ([`validate_draft_against_record`](crate::company::workflow_create)), which
/// applies these same rules strictly, plus the seed corpus test which runs this
/// with `strict = true`. Every OTHER structural check here is unconditional.
pub(crate) fn validate(raw: &RawWorkflow, strict: bool) -> Vec<String> {
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
    // Ids of every `condition` node, collected the same way as `switch_nodes`.
    // Both are the branch kinds that can steer a run OUT of a loop, so the
    // inescapable-cycle check below treats them alike: an SCC with no edge
    // leaving it from a condition/switch is a trap the engine can never exit.
    let mut condition_nodes = std::collections::HashSet::new();
    // Ids of every `trigger` node (the graph's entry points), in file order.
    // The reachability check below does a BFS from all of them.
    let mut trigger_ids: Vec<&str> = Vec::new();
    // Ids of every `trigger` node carrying a `schedule`. More than one is
    // rejected below: the graph is ONE workflow, so two schedules on it would
    // double-run it on any minute both matched.
    let mut scheduled_triggers: Vec<&str> = Vec::new();
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
            Some(WorkflowNodeKind::Trigger) => {
                trigger_count += 1;
                if !node.id.trim().is_empty() {
                    trigger_ids.push(node.id.as_str());
                }
            }
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
        if kind == Some(WorkflowNodeKind::Condition) && !node.id.trim().is_empty() {
            condition_nodes.insert(node.id.as_str());
        }

        // Per-kind required config (issue #661): a hand-authored file that omits
        // a `condition` `field`, an `http_request` `method`/`url`, a `switch`
        // discriminant, or a `tool_call` `slug` translates into a graph whose
        // runtime behaviour is silently wrong. Enforced ONLY on the strict
        // author-time surface (issue #682): the read/load path must not hard-fail
        // a graph persisted before #661, or it would vanish from the console and
        // silently stop a scheduled run. The console-draft path applies the same
        // helper strictly, so the two AUTHOR surfaces reject the same shapes.
        if strict && let Some(kind) = kind {
            problems.extend(required_config_problems(kind, &label, node.config.as_ref()));
        }

        // `schedule` says *when* the workflow starts, so it is trigger-only —
        // anywhere else it would sit inert and mislead (the same footgun the
        // stray-`agent` check above prevents). On a trigger it must be a real
        // 5-field cron: the workflow scheduler parses it with the same
        // `CronExpr` the manifest `[[schedule]]` crons use, so an expression
        // that cannot fire is rejected here rather than silently never firing.
        if let Some(schedule) = node.schedule.as_deref() {
            match kind {
                Some(WorkflowNodeKind::Trigger) => {
                    if let Err(err) = crate::runtime::cron::CronExpr::parse(schedule) {
                        problems.push(format!(
                            "{label} has a `schedule` that is not a valid cron — {err}. Times are UTC."
                        ));
                    }
                    // Counted even when the cron is malformed, so a graph with
                    // two bad schedules reports both problems at once.
                    if !node.id.trim().is_empty() {
                        scheduled_triggers.push(node.id.as_str());
                    }
                }
                Some(kind) => problems.push(format!(
                    "{label} sets `schedule` but is a `{}` node — only `trigger` nodes carry a schedule.",
                    kind.as_str()
                )),
                // The unknown-kind problem is already reported above; do not
                // pile a second, confusing message onto the same node.
                None => {}
            }
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

        // `destination` routes an `output` node's report to a person or a
        // channel after the run finishes. Only `output` nodes report back, and
        // each kind has its own target contract — an author who gets this wrong
        // must hear about it here, not discover a silently undelivered report.
        if let Some(destination) = &node.destination {
            if kind != Some(WorkflowNodeKind::Output) {
                problems.push(format!(
                    "{label} sets `destination` but is a `{}` node — only `output` nodes route a report.",
                    node.kind
                ));
            }
            let target = destination.target.as_deref().map(str::trim).unwrap_or("");
            match destination.kind.trim() {
                "owner" => {
                    if !target.is_empty() {
                        problems.push(format!(
                            "{label} sets a `target` on an `owner` destination — the owner is resolved from the company's own admins, so leave it out."
                        ));
                    }
                }
                "email" => {
                    if !target.contains('@') {
                        problems.push(format!(
                            "{label} has an `email` destination whose `target` `{target}` is not an email address — give the recipient's full address."
                        ));
                    }
                }
                "channel" => {
                    if target.is_empty() {
                        problems.push(format!(
                            "{label} has a `channel` destination with no `target` — name the channel to post the report to."
                        ));
                    }
                }
                "" => problems.push(format!(
                    "{label} has a `destination` that names no `kind` — use one of {}.",
                    WORKFLOW_DESTINATION_KINDS.join(", ")
                )),
                other => problems.push(format!(
                    "{label} has an unknown `destination.kind` `{other}` — use one of {}.",
                    WORKFLOW_DESTINATION_KINDS.join(", ")
                )),
            }
        }

        // Reserved config keys: the first-class fields above are written into
        // the engine config LAST, so a `config` entry naming one would be
        // silently ignored — reject it as a footgun instead. `destination` is
        // reserved for a different reason: it is never engine config at all
        // (delivery runs host-side), so a `config.destination` would ride into
        // the engine graph as an inert key and deliver nothing. `agent_ref` is
        // reserved on `agent` nodes (translation binds it from `agent`).
        if let Some(toml::Value::Table(table)) = &node.config {
            for reserved in [
                "on_error",
                "retry",
                "requires_approval",
                "schedule",
                "destination",
            ] {
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

    // At most ONE scheduled trigger. Several triggers are fine — a graph may be
    // startable several ways — but a schedule says when the whole workflow runs,
    // so two of them would run it twice on any minute both matched. Rejecting is
    // better than picking one: silently honoring the first would drop a schedule
    // the operator saved, with nothing anywhere to say so.
    if scheduled_triggers.len() > 1 {
        let names: Vec<String> = scheduled_triggers
            .iter()
            .map(|id| format!("`{id}`"))
            .collect();
        problems.push(format!(
            "nodes {} each set a `schedule` — a workflow may carry at most one scheduled trigger, or it would run twice on the same minute.",
            names.join(", ")
        ));
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

        // A `condition` node's branches steer the run onto the engine's `true`
        // or `false` port, keyed off the edge label (issue #661). An unlabeled
        // or oddly-labeled branch would silently funnel through `condition_port`
        // onto the `true` port — a mislabeled branch that quietly runs the wrong
        // way — so require every condition branch to read `yes` or `no`. The
        // sole exception is the `error` recovery edge of a condition that is also
        // `on_error = "route"`, already validated by the routing-edge rule above.
        //
        // Enforced ONLY on the strict author-time surface (issue #682): the
        // read/load path must accept a graph persisted before #661 (a
        // console-authored condition could not carry `yes`/`no` labels until this
        // change), where a field-less/label-less condition routed `true` always —
        // wrong-but-working beats "gone and never fires". The draft path applies
        // this same rule strictly.
        //
        // Intentional asymmetry (do not "fix" one to match the other): the branch
        // label is lowercased + trimmed before matching `yes`/`no`, so ` YES `
        // passes — the label is compared, never persisted as a lookup key. By
        // contrast `validate_tool_call_node` REJECTS a padded `slug`, because that
        // string is stored and looked up at run time verbatim, so the validated
        // string must equal the persisted one.
        if strict && condition_nodes.contains(edge.from.as_str()) {
            let is_route_error =
                edge.label.as_deref() == Some("error") && route_nodes.contains(edge.from.as_str());
            let is_yes_no = edge
                .label
                .as_deref()
                .map(|l| l.trim().to_ascii_lowercase())
                .is_some_and(|l| matches!(l.as_str(), "yes" | "no"));
            if !is_route_error && !is_yes_no {
                let shown = edge
                    .label
                    .as_deref()
                    .map(|l| format!("`{l}`"))
                    .unwrap_or_else(|| "no label".to_string());
                problems.push(format!(
                    "{label} leaves condition node `{}` with {shown} — a condition's branches must be labeled `yes` or `no`.",
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

    // --- Graph traversal: inescapable cycles + reachability -----------------
    //
    // Everything above checks a node or an edge in isolation. These two checks
    // are about the SHAPE of the whole graph, so they run last, over a directed
    // graph built only from edges whose BOTH endpoints resolve to real nodes —
    // an unresolved endpoint already produced its own problem above, and a
    // self-loop already did too, so neither cascades a second, confusing message
    // here. A branch label (`yes` / `error` / a switch case) is an ordinary
    // directed edge for traversal; it steers WHICH way the run goes, not whether
    // the edge exists.
    let mut node_ids: Vec<&str> = Vec::new();
    let mut index_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for node in &raw.nodes {
        let id = node.id.as_str();
        if id.trim().is_empty() {
            continue;
        }
        // First occurrence wins, mirroring `seen`; a duplicate id is one vertex.
        index_of.entry(id).or_insert_with(|| {
            node_ids.push(id);
            node_ids.len() - 1
        });
    }
    let node_count = node_ids.len();
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for edge in &raw.edges {
        if let (Some(&from), Some(&to)) = (
            index_of.get(edge.from.as_str()),
            index_of.get(edge.to.as_str()),
        ) {
            adjacency[from].push(to);
        }
    }

    // Tarjan's strongly-connected-components, iteratively (a recursive DFS could
    // blow the stack on a pathological hand-authored graph). `scc_of[v]` is the
    // component index of node `v`; nodes in the same component are mutually
    // reachable — i.e. they form a cycle.
    let mut scc_of = vec![usize::MAX; node_count];
    let mut disc = vec![usize::MAX; node_count];
    let mut low = vec![0usize; node_count];
    let mut on_stack = vec![false; node_count];
    let mut tarjan_stack: Vec<usize> = Vec::new();
    let mut next_disc = 0usize;
    let mut scc_count = 0usize;
    for start in 0..node_count {
        if disc[start] != usize::MAX {
            continue;
        }
        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, child)) = call_stack.last() {
            if child == 0 {
                disc[v] = next_disc;
                low[v] = next_disc;
                next_disc += 1;
                tarjan_stack.push(v);
                on_stack[v] = true;
            }
            if child < adjacency[v].len() {
                let w = adjacency[v][child];
                call_stack.last_mut().unwrap().1 += 1;
                if disc[w] == usize::MAX {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(disc[w]);
                }
            } else {
                if low[v] == disc[v] {
                    loop {
                        let w = tarjan_stack.pop().unwrap();
                        on_stack[w] = false;
                        scc_of[w] = scc_count;
                        if w == v {
                            break;
                        }
                    }
                    scc_count += 1;
                }
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }

    // Inescapable-cycle check. A cycle is FINE as long as the run can choose to
    // leave it — that is what a guarded retry loop is (a `condition`/`switch`
    // inside the loop with a branch that exits it). So for every SCC bigger than
    // one node, require at least one `condition`/`switch` member with an edge
    // that leaves the SCC. An SCC with no such exit is a trap: once the run
    // enters, every path leads back in, and it never terminates.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); scc_count];
    for v in 0..node_count {
        members[scc_of[v]].push(v); // ascending v == file order
    }
    for component in &members {
        if component.len() < 2 {
            continue;
        }
        let scc = scc_of[component[0]];
        let has_exit = component.iter().any(|&v| {
            (condition_nodes.contains(node_ids[v]) || switch_nodes.contains(node_ids[v]))
                && adjacency[v].iter().any(|&w| scc_of[w] != scc)
        });
        if !has_exit {
            let names: Vec<String> = component
                .iter()
                .map(|&v| format!("`{}`", node_ids[v]))
                .collect();
            problems.push(format!(
                "nodes {} form a loop with no conditional way out — add a `condition`/`switch` branch that leaves the loop, or remove an edge.",
                names.join(", ")
            ));
        }
    }

    // Reachability check. Every node must sit on some path from a `trigger`, or
    // the engine will never execute it. SKIPPED entirely when no trigger
    // contributed a usable entry id — either there are no triggers, or every
    // trigger failed id validation (empty id). In both cases the real problem
    // ("needs at least one trigger" / "missing an `id`") already fired above, and
    // without a seed EVERY node is trivially unreachable, so reporting all of them
    // would just bury that problem in noise. Gate on `trigger_ids` (the entries
    // the BFS can actually seed from), NOT the raw `trigger_count`, so an id-less
    // trigger doesn't clear the count yet seed nothing.
    if !trigger_ids.is_empty() {
        let mut reached = vec![false; node_count];
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        for tid in &trigger_ids {
            if let Some(&i) = index_of.get(tid)
                && !reached[i]
            {
                reached[i] = true;
                queue.push_back(i);
            }
        }
        while let Some(v) = queue.pop_front() {
            for &w in &adjacency[v] {
                if !reached[w] {
                    reached[w] = true;
                    queue.push_back(w);
                }
            }
        }
        let unreached: Vec<String> = (0..node_count)
            .filter(|&v| !reached[v])
            .map(|v| format!("`{}`", node_ids[v]))
            .collect();
        if !unreached.is_empty() {
            let (subject, tail) = if unreached.len() == 1 {
                ("node", "it")
            } else {
                ("nodes", "them")
            };
            problems.push(format!(
                "{subject} {} cannot be reached from any `trigger` — connect an edge that leads to {tail}, or remove {tail}.",
                unreached.join(", ")
            ));
        }
    }

    problems
}

/// The per-kind required-`config` problems for one node, in prosumer language.
///
/// Shared by the on-disk [`validate`] pass and the console/builder draft path
/// ([`validate_draft_against_record`](crate::company::workflow_create)) so the
/// same missing config is rejected identically on BOTH author-time surfaces
/// (issue #661): a `condition` with no `field`, an `http_request` missing
/// `method`/`url`, a `switch` with no discriminant, or a `tool_call` with no
/// `slug`. Each of those still *translates* into a runnable graph, but the
/// node's behaviour is silently wrong — a field-less condition tests the whole
/// item, a slug-less `tool_call` used to fall back to the node id (masking which
/// tool would run) — so surfacing it at load/author time is the point.
pub(crate) fn required_config_problems(
    kind: WorkflowNodeKind,
    label: &str,
    config: Option<&toml::Value>,
) -> Vec<String> {
    let table = config.and_then(toml::Value::as_table);
    // A config key set to a non-empty, non-whitespace string.
    let non_empty = |key: &str| -> bool {
        table
            .and_then(|t| t.get(key))
            .and_then(toml::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    let mut problems = Vec::new();
    match kind {
        WorkflowNodeKind::Condition if !non_empty("field") => {
            problems.push(format!(
                "{label} is a condition node but sets no `config.field` — give it the boolean \
                 expression the branch tests (e.g. `field = \"=item.approved\"`)."
            ));
        }
        WorkflowNodeKind::HttpRequest => {
            if !non_empty("method") {
                problems.push(format!(
                    "{label} is an http_request node but sets no `config.method` — name the HTTP \
                     method (e.g. `method = \"GET\"`)."
                ));
            }
            if !non_empty("url") {
                problems.push(format!(
                    "{label} is an http_request node but sets no `config.url` — give the request \
                     URL (e.g. `url = \"https://…\"`)."
                ));
            }
        }
        // `field` OR `expression` both satisfy a switch — the tinyflows engine
        // reads `config.expression` first and falls back to `config.field`
        // (`vendor/openhuman/vendor/tinyflows/.../switch.rs`), so either is
        // honoured downstream and requiring only that ONE is present matches the
        // runtime.
        WorkflowNodeKind::Switch if !non_empty("field") && !non_empty("expression") => {
            problems.push(format!(
                "{label} is a switch node but names no discriminant — set `config.field` or \
                 `config.expression` to the value that selects the branch."
            ));
        }
        WorkflowNodeKind::ToolCall if !non_empty("slug") => {
            problems.push(format!(
                "{label} is a tool_call but sets no `config.slug` — set `config.slug` to the \
                 tool to run."
            ));
        }
        _ => {}
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
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
                RawNode {
                    id: "worker".to_string(),
                    kind: "agent".to_string(),
                    name: "Worker".to_string(),
                    summary: Some("Does the thing.".to_string()),
                    agent: Some("ceo".to_string()),
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
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
                schedule: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
                destination: None,
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
            [[edge]]
            from = "start"
            to = "call"
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
            [node.config]
            slug = "csv_export"
            [node.retry]
            max_attempts = 3
            backoff_ms = 100
            backoff = "exponential"
            [[edge]]
            from = "start"
            to = "call"
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
            [node.config]
            slug = "csv_export"
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
            [node.config]
            field = "=item.kind"
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
            [[edge]]
            from = "start"
            to = "sw"
            [[edge]]
            from = "sw"
            to = "mg"
            [[edge]]
            from = "mg"
            to = "so"
            [[edge]]
            from = "so"
            to = "tf"
            [[edge]]
            from = "tf"
            to = "op"
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
            [node.config]
            field = "=item.kind"
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

    // --- Per-kind required config (issue #661) ------------------------------

    /// A `condition` node with no `config.field` still LOADS on the lenient
    /// read path (issue #682: pre-#661 saved graphs must keep loading), but the
    /// STRICT author-time pass reports it — without a field the engine tests the
    /// whole item and the branch is silently meaningless. This is the regression
    /// guard: a field-less-condition graph parses via `parse_workflow` yet is
    /// rejected by `validate(_, true)`.
    #[test]
    fn condition_without_field_loads_leniently_but_strict_rejects() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "gate"
            kind = "condition"
            name = "Gate"
            [[node]]
            id = "yes_out"
            kind = "output"
            name = "Yes"
            [[node]]
            id = "no_out"
            kind = "output"
            name = "No"
            [[edge]]
            from = "start"
            to = "gate"
            [[edge]]
            from = "gate"
            to = "yes_out"
            label = "yes"
            [[edge]]
            from = "gate"
            to = "no_out"
            label = "no"
        "#;
        // Lenient load path accepts it — a graph persisted before #661 still loads.
        assert!(parse_workflow(src).is_ok());
        // Strict author-time pass reports the missing field.
        let raw: RawWorkflow = toml::from_str(src).expect("the fixture is valid TOML");
        let problems = validate(&raw, true).join("\n");
        assert!(problems.contains("config.field"), "{problems}");
    }

    /// A `condition` branch labeled anything but `yes`/`no` loads leniently
    /// (issue #682) but is rejected by the STRICT author-time pass — an
    /// off-vocabulary label silently maps onto the `true` port.
    #[test]
    fn condition_branch_with_non_yes_no_label_loads_leniently_but_strict_rejects() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "gate"
            kind = "condition"
            name = "Gate"
            [node.config]
            field = "=item.ok"
            [[node]]
            id = "a"
            kind = "output"
            name = "A"
            [[node]]
            id = "b"
            kind = "output"
            name = "B"
            [[edge]]
            from = "start"
            to = "gate"
            [[edge]]
            from = "gate"
            to = "a"
            label = "pass"
            [[edge]]
            from = "gate"
            to = "b"
            label = "no"
        "#;
        // Lenient load path accepts the off-vocabulary label.
        assert!(parse_workflow(src).is_ok());
        // Strict author-time pass reports it.
        let raw: RawWorkflow = toml::from_str(src).expect("the fixture is valid TOML");
        let problems = validate(&raw, true).join("\n");
        assert!(problems.contains("labeled `yes` or `no`"), "{problems}");
    }

    /// A well-formed `condition` — a `field` plus `yes`/`no` branches — parses.
    #[test]
    fn condition_with_field_and_yes_no_labels_is_valid() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "gate"
            kind = "condition"
            name = "Gate"
            [node.config]
            field = "=item.approved"
            [[node]]
            id = "a"
            kind = "output"
            name = "A"
            [[node]]
            id = "b"
            kind = "output"
            name = "B"
            [[edge]]
            from = "start"
            to = "gate"
            [[edge]]
            from = "gate"
            to = "a"
            label = "yes"
            [[edge]]
            from = "gate"
            to = "b"
            label = "no"
        "#;
        assert!(parse_workflow(src).is_ok());
    }

    /// An `http_request` node missing `config.method` / `config.url` loads
    /// leniently (issue #682) but the STRICT pass reports BOTH missing keys.
    #[test]
    fn http_request_without_method_or_url_loads_leniently_but_strict_rejects() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "fetch"
            kind = "http_request"
            name = "Fetch"
            [[edge]]
            from = "start"
            to = "fetch"
        "#;
        // Lenient load path accepts it.
        assert!(parse_workflow(src).is_ok());
        // Strict author-time pass reports both missing config keys at once.
        let raw: RawWorkflow = toml::from_str(src).expect("the fixture is valid TOML");
        let message = validate(&raw, true).join("\n");
        assert!(message.contains("config.method"), "{message}");
        assert!(message.contains("config.url"), "{message}");
    }

    /// A `switch` node with neither `field` nor `expression` loads leniently
    /// (issue #682) but the STRICT pass reports the missing discriminant.
    #[test]
    fn switch_without_discriminant_loads_leniently_but_strict_rejects() {
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
            id = "case_a"
            kind = "output"
            name = "A"
            [[edge]]
            from = "start"
            to = "sw"
            [[edge]]
            from = "sw"
            to = "case_a"
            label = "a"
        "#;
        // Lenient load path accepts it.
        assert!(parse_workflow(src).is_ok());
        // Strict author-time pass reports the missing discriminant.
        let raw: RawWorkflow = toml::from_str(src).expect("the fixture is valid TOML");
        let problems = validate(&raw, true).join("\n");
        assert!(problems.contains("discriminant"), "{problems}");
    }

    /// Strict author-time parity (issue #661/#682): a `tool_call` with no `slug`
    /// loads leniently now, but the STRICT pass reports it — the same shape the
    /// console-draft path rejects — so `translate` never has to fall back to the
    /// node id as a placeholder slug once a graph reaches an author surface.
    #[test]
    fn tool_call_without_slug_loads_leniently_but_strict_rejects() {
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
            [[edge]]
            from = "start"
            to = "call"
        "#;
        // Lenient load path accepts it.
        assert!(parse_workflow(src).is_ok());
        // Strict author-time pass reports the missing slug.
        let raw: RawWorkflow = toml::from_str(src).expect("the fixture is valid TOML");
        let problems = validate(&raw, true).join("\n");
        assert!(problems.contains("config.slug"), "{problems}");
    }

    // --- G15: inescapable cycles + reachability (issue #540) ----------------

    /// A bare two-node cycle with no branch to leave it is a trap: once the run
    /// reaches `a` it loops `a → b → a` forever. Rejected, naming both nodes.
    /// (Negative control for the cycle check: delete the SCC-exit check and this
    /// graph — which has no condition/switch at all — would wrongly pass.)
    #[test]
    fn multi_node_cycle_with_no_exit_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "a"
            kind = "agent"
            name = "A"
            agent = "ceo"
            [[node]]
            id = "b"
            kind = "agent"
            name = "B"
            agent = "ceo"
            [[edge]]
            from = "start"
            to = "a"
            [[edge]]
            from = "a"
            to = "b"
            [[edge]]
            from = "b"
            to = "a"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("form a loop"), "{message}");
        assert!(message.contains("`a`"), "{message}");
        assert!(message.contains("`b`"), "{message}");
    }

    /// A miniature of the shipped `game_build_pipeline` shape: a `condition`
    /// guards the loop, with a `yes` branch that leaves it and a `no` branch
    /// that loops back. That is a legal bounded retry — it must parse clean.
    #[test]
    fn condition_guarded_loop_is_valid() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "work"
            kind = "agent"
            name = "Work"
            agent = "ceo"
            [[node]]
            id = "gate"
            kind = "condition"
            name = "Good enough?"
            [node.config]
            field = "=item.good_enough"
            [[node]]
            id = "done"
            kind = "output"
            name = "Ship"
            [[edge]]
            from = "start"
            to = "work"
            [[edge]]
            from = "work"
            to = "gate"
            [[edge]]
            from = "gate"
            to = "done"
            label = "yes"
            [[edge]]
            from = "gate"
            to = "work"
            label = "no"
        "#;
        assert!(
            parse_workflow(src).is_ok(),
            "a condition-guarded retry loop must stay valid"
        );
    }

    /// The shipped guarded-retry preset itself must stay valid — its loop
    /// (`gameplay → assets → balance → qa → gate → gameplay`) is escapable
    /// because `gate` is a `condition` whose `yes` branch leaves the loop.
    #[test]
    fn the_shipped_guarded_loop_preset_is_valid() {
        const GAME: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/companies/agentic_game_studio/workflows/game_build_pipeline.toml"
        ));
        parse_workflow(GAME).expect("the game-studio guarded loop is valid");
    }

    /// A cycle that DOES contain a `condition`, but whose only edges all stay
    /// inside the loop, is still inescapable — a branch that never leaves the SCC
    /// buys nothing. (Negative control: the exit test, not merely "contains a
    /// condition", is what this asserts.)
    #[test]
    fn inescapable_cycle_containing_condition_with_no_exit_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "a"
            kind = "agent"
            name = "A"
            agent = "ceo"
            [[node]]
            id = "gate"
            kind = "condition"
            name = "Gate"
            [[edge]]
            from = "start"
            to = "a"
            [[edge]]
            from = "a"
            to = "gate"
            [[edge]]
            from = "gate"
            to = "a"
            label = "again"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(err.to_string().contains("form a loop"), "{err}");
    }

    /// A node no edge ever reaches from the trigger would never execute. It is
    /// rejected, naming the orphan. (Negative control for reachability.)
    #[test]
    fn unreachable_node_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "reached"
            kind = "output"
            name = "Reached"
            [[node]]
            id = "orphan"
            kind = "output"
            name = "Orphan"
            [[edge]]
            from = "start"
            to = "reached"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("cannot be reached"), "{message}");
        assert!(message.contains("`orphan`"), "{message}");
        // The reached node must NOT be named — only the genuine orphan.
        assert!(!message.contains("`reached`"), "{message}");
    }

    /// With no trigger at all, the reachability check stays silent: the
    /// "needs at least one trigger" problem is the real one, and flagging every
    /// node as unreachable on top of it would just be noise.
    #[test]
    fn no_trigger_skips_reachability_noise() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "only"
            kind = "output"
            name = "Only"
            [[node]]
            id = "other"
            kind = "output"
            name = "Other"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("trigger"), "{message}");
        assert!(
            !message.contains("cannot be reached"),
            "reachability must be skipped with no trigger: {message}"
        );
    }

    /// A trigger with an EMPTY id already fails id-validation, and it seeds no
    /// entry into the reachability BFS. The gate keys off `trigger_ids` (usable
    /// entries), not the raw trigger count, so the run's one valid node is NOT
    /// piled with a bogus "cannot be reached" on top of the real "missing an
    /// `id`" problem. (Regression, #540.)
    #[test]
    fn empty_id_trigger_skips_reachability_noise() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = ""
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "work"
            kind = "output"
            name = "Work"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("missing an `id`"), "{message}");
        assert!(
            !message.contains("cannot be reached"),
            "an id-less trigger must not spawn reachability noise: {message}"
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
        assert!(start.destination.is_none());
    }

    // --- Output destination (issue #170) ------------------------------------

    /// A graph with one `output` node carrying `destination` of `kind`, plus an
    /// optional `target` line.
    fn with_destination(kind: &str, target: Option<&str>) -> String {
        let target_line = target
            .map(|t| format!("target = \"{t}\"\n"))
            .unwrap_or_default();
        format!(
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
name = "Report back"
[node.destination]
kind = "{kind}"
{target_line}
[[edge]]
from = "start"
to = "done"
"#
        )
    }

    /// Each of the three destination kinds parses onto the first-class field
    /// with its target contract intact.
    #[test]
    fn output_destinations_parse_for_every_kind() {
        let owner = parse_workflow(&with_destination("owner", None)).expect("owner parses");
        let dest = owner.nodes[1].destination.as_ref().expect("present");
        assert_eq!(dest.kind, "owner");
        assert_eq!(dest.target, None);

        let email = parse_workflow(&with_destination("email", Some("ada@example.com")))
            .expect("email parses");
        let dest = email.nodes[1].destination.as_ref().expect("present");
        assert_eq!(dest.kind, "email");
        assert_eq!(dest.target.as_deref(), Some("ada@example.com"));

        let channel =
            parse_workflow(&with_destination("channel", Some("operator"))).expect("channel parses");
        let dest = channel.nodes[1].destination.as_ref().expect("present");
        assert_eq!(dest.kind, "channel");
        assert_eq!(dest.target.as_deref(), Some("operator"));
    }

    #[test]
    fn unknown_destination_kind_is_rejected() {
        let err = parse_workflow(&with_destination("carrier_pigeon", Some("x"))).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown `destination.kind`"), "{message}");
        // The message names what IS supported, not just what isn't.
        assert!(message.contains("owner"), "{message}");
    }

    /// An `email` destination MUST name an address. This is the validation half
    /// of the security boundary: a workflow cannot mail "somebody" — the
    /// recipient is pinned in the graph, where a reviewer can see it.
    #[test]
    fn email_destination_without_an_address_is_rejected() {
        let err = parse_workflow(&with_destination("email", Some("ada"))).unwrap_err();
        assert!(err.to_string().contains("not an email address"), "{err}");
        let err = parse_workflow(&with_destination("email", None)).unwrap_err();
        assert!(err.to_string().contains("not an email address"), "{err}");
    }

    #[test]
    fn channel_destination_without_a_target_is_rejected() {
        let err = parse_workflow(&with_destination("channel", None)).unwrap_err();
        assert!(err.to_string().contains("no `target`"), "{err}");
    }

    /// `owner` resolves server-side, so a target on it is a mistake worth
    /// naming — otherwise an author writes an address there and quietly gets
    /// the admins instead.
    #[test]
    fn owner_destination_with_a_target_is_rejected() {
        let err = parse_workflow(&with_destination("owner", Some("ada@example.com"))).unwrap_err();
        assert!(err.to_string().contains("`owner` destination"), "{err}");
    }

    /// Only `output` nodes report back, so a `destination` anywhere else is a
    /// silent no-op waiting to happen.
    #[test]
    fn destination_on_a_non_output_node_is_rejected() {
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
            [node.destination]
            kind = "owner"
        "#;
        let err = parse_workflow(src).unwrap_err();
        assert!(
            err.to_string()
                .contains("only `output` nodes route a report"),
            "{err}"
        );
    }

    /// `destination` inside `config` would ride into the engine graph as an
    /// inert key and deliver nothing — reject it like the other reserved keys.
    #[test]
    fn destination_inside_config_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "done"
            kind = "output"
            name = "Done"
            [node.config]
            destination = "owner"
            [[edge]]
            from = "start"
            to = "done"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("destination"), "{message}");
        assert!(message.contains("inside `config`"), "{message}");
    }

    /// A destination-bearing graph renders back to TOML and re-parses to the
    /// same model — the create route's persist path depends on this.
    #[test]
    fn destination_round_trips_through_render_and_parse() {
        let raw = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
            nodes: vec![
                RawNode {
                    id: "start".to_string(),
                    kind: "trigger".to_string(),
                    name: "Start".to_string(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
                RawNode {
                    id: "done".to_string(),
                    kind: "output".to_string(),
                    name: "Report".to_string(),
                    summary: None,
                    agent: None,
                    schedule: None,
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: Some(WorkflowDestinationDef {
                        kind: "email".to_string(),
                        target: Some("ada@example.com".to_string()),
                    }),
                },
            ],
            edges: vec![RawEdge {
                from: "start".to_string(),
                to: "done".to_string(),
                label: None,
            }],
        };
        let toml_src = render_workflow(&raw).expect("renders");
        let file = parse_workflow(&toml_src).expect("re-parses");
        let dest = file.nodes[1].destination.as_ref().expect("present");
        assert_eq!(dest.kind, "email");
        assert_eq!(dest.target.as_deref(), Some("ada@example.com"));
    }

    /// A legacy graph (no `destination` anywhere) renders byte-identically to
    /// what it rendered before the field existed — `skip_serializing_if` is what
    /// keeps an unchanged file from churning on every re-save.
    #[test]
    fn a_graph_without_a_destination_renders_no_destination_key() {
        let raw = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
            nodes: vec![RawNode {
                id: "start".to_string(),
                kind: "trigger".to_string(),
                name: "Start".to_string(),
                summary: None,
                agent: None,
                schedule: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
                destination: None,
            }],
            edges: Vec::new(),
        };
        let toml_src = render_workflow(&raw).expect("renders");
        assert!(!toml_src.contains("destination"), "{toml_src}");
    }

    // --- trigger schedule (issue #169) --------------------------------------

    /// A trigger's `schedule` survives the render → parse round trip the create
    /// endpoint runs, and lands on the parsed node.
    #[test]
    fn trigger_schedule_round_trips_render_and_parse() {
        let raw = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: None,
            nodes: vec![
                RawNode {
                    id: "start".to_string(),
                    kind: "trigger".to_string(),
                    name: "Start".to_string(),
                    summary: None,
                    agent: None,
                    schedule: Some("0 * * * *".to_string()),
                    config: None,
                    on_error: None,
                    retry: None,
                    requires_approval: None,
                    destination: None,
                },
                RawNode {
                    id: "done".to_string(),
                    kind: "output".to_string(),
                    name: "Done".to_string(),
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
            edges: vec![RawEdge {
                from: "start".to_string(),
                to: "done".to_string(),
                label: None,
            }],
        };
        let toml_src = render_workflow(&raw).expect("renders");
        let file = parse_workflow(&toml_src).expect("re-parses");
        let start = file.nodes.iter().find(|n| n.id == "start").unwrap();
        assert_eq!(start.schedule.as_deref(), Some("0 * * * *"));
        let done = file.nodes.iter().find(|n| n.id == "done").unwrap();
        assert!(done.schedule.is_none());
    }

    /// A trigger schedule parses from hand-authored TOML too, including the
    /// named-weekday dialect the manifest `[[schedule]]` crons already accept.
    #[test]
    fn trigger_schedule_parses_from_toml() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            schedule = "0 9 * * MON"
        "#;
        let file = parse_workflow(src).expect("parses");
        assert_eq!(file.nodes[0].schedule.as_deref(), Some("0 9 * * MON"));
    }

    /// `schedule` says when the *workflow* starts, so it is trigger-only.
    #[test]
    fn schedule_on_a_non_trigger_node_is_rejected() {
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
            schedule = "0 * * * *"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("only `trigger` nodes carry a schedule"),
            "{message}"
        );
    }

    /// A malformed cron is rejected at validation with the parser's own
    /// message, so it can never be persisted as an expression that never fires.
    #[test]
    fn invalid_trigger_schedule_cron_is_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            schedule = "every hour"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not a valid cron"), "{message}");
        assert!(message.contains("needs 5 fields"), "{message}");
        assert!(message.contains("UTC"), "{message}");

        // An out-of-range field is caught by the same parser.
        let out_of_range = src.replace("every hour", "0 99 * * *");
        let err = parse_workflow(&out_of_range).unwrap_err();
        assert!(err.to_string().contains("not a valid cron"), "{err}");
    }

    /// Two scheduled triggers would double-run the workflow, and honoring only
    /// the first would silently drop a schedule the operator saved — so the
    /// graph is rejected, naming both offenders.
    #[test]
    fn two_scheduled_triggers_are_rejected() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "nightly"
            kind = "trigger"
            name = "Nightly"
            schedule = "0 2 * * *"
            [[node]]
            id = "hourly"
            kind = "trigger"
            name = "Hourly"
            schedule = "0 * * * *"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("at most one scheduled trigger"),
            "{message}"
        );
        assert!(message.contains("`nightly`"), "{message}");
        assert!(message.contains("`hourly`"), "{message}");
    }

    /// The at-most-one rule counts *schedules*, not triggers: a graph may still
    /// have several triggers, and one of them may be scheduled.
    #[test]
    fn multiple_triggers_are_still_allowed_when_at_most_one_is_scheduled() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "manual"
            kind = "trigger"
            name = "Manual"
            [[node]]
            id = "webhook"
            kind = "trigger"
            name = "Webhook"
            [[node]]
            id = "nightly"
            kind = "trigger"
            name = "Nightly"
            schedule = "0 2 * * *"
        "#;
        let file = parse_workflow(src).expect("several triggers stay legal");
        assert_eq!(file.nodes.len(), 3);
        let scheduled: Vec<&str> = file
            .nodes
            .iter()
            .filter(|n| n.schedule.is_some())
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(scheduled, vec!["nightly"]);

        // And with no schedules at all, unchanged from before this rule.
        let bare = src.replace("schedule = \"0 2 * * *\"", "");
        assert!(parse_workflow(&bare).is_ok());
    }

    /// Two *malformed* schedules report the bad crons AND the at-most-one
    /// problem together, matching the module's report-everything-at-once
    /// contract.
    #[test]
    fn two_bad_schedules_report_every_problem_at_once() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "a"
            kind = "trigger"
            name = "A"
            schedule = "nightly"
            [[node]]
            id = "b"
            kind = "trigger"
            name = "B"
            schedule = "hourly"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not a valid cron"), "{message}");
        assert!(
            message.contains("at most one scheduled trigger"),
            "{message}"
        );
    }

    /// `config.schedule` would be silently ignored (the first-class field wins),
    /// so it is a reserved key like the other first-class node fields.
    #[test]
    fn config_schedule_is_rejected_as_a_reserved_key() {
        let src = r#"
            id = "wf"
            name = "WF"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [node.config]
            schedule = "0 * * * *"
        "#;
        let err = parse_workflow(src).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("`schedule` inside `config`"), "{message}");
    }

    /// A graph authored before `schedule` existed parses with the field unset
    /// and re-renders byte-identically — the field is skipped when `None`, so
    /// adding it rewrites nothing on disk.
    #[test]
    fn legacy_graph_without_schedule_re_renders_byte_identically() {
        let raw = RawWorkflow {
            id: "wf".to_string(),
            name: "WF".to_string(),
            description: Some("Legacy.".to_string()),
            nodes: vec![RawNode {
                id: "start".to_string(),
                kind: "trigger".to_string(),
                name: "Start".to_string(),
                summary: Some("Kicks off.".to_string()),
                agent: None,
                schedule: None,
                config: None,
                on_error: None,
                retry: None,
                requires_approval: None,
                destination: None,
            }],
            edges: Vec::new(),
        };
        let first = render_workflow(&raw).expect("renders");
        assert!(
            !first.contains("schedule"),
            "an unset schedule must not be written: {first}"
        );

        let file = parse_workflow(&first).expect("parses");
        assert!(file.nodes[0].schedule.is_none());

        // Re-render the parsed graph through the same shape: byte-identical.
        let round_tripped = RawWorkflow {
            id: file.id.clone(),
            name: file.name.clone(),
            description: file.description.clone(),
            nodes: file
                .nodes
                .iter()
                .map(|n| RawNode {
                    id: n.id.clone(),
                    kind: n.kind.as_str().to_string(),
                    name: n.name.clone(),
                    summary: n.summary.clone(),
                    agent: n.agent.clone(),
                    schedule: n.schedule.clone(),
                    config: None,
                    on_error: n.on_error.clone(),
                    retry: n.retry.clone(),
                    requires_approval: n.requires_approval,
                    destination: n.destination.clone(),
                })
                .collect(),
            edges: Vec::new(),
        };
        assert_eq!(render_workflow(&round_tripped).expect("re-renders"), first);
    }

    // --- seed ∪ overlay union (issue #168) ----------------------------------

    use crate::ports::types::OverlayWorkflow;

    /// A minimal valid graph body with the given id and display name.
    fn body(id: &str, name: &str) -> String {
        format!(
            r#"
id = "{id}"
name = "{name}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#
        )
    }

    fn overlay(id: &str, name: &str) -> OverlayWorkflow {
        OverlayWorkflow {
            id: id.to_string(),
            toml: body(id, name),
        }
    }

    /// Writes a seed graph to `<dir>/workflows/<id>.toml`.
    fn seed(dir: &Path, id: &str, name: &str) {
        let workflows = dir.join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join(format!("{id}.toml")), body(id, name)).unwrap();
    }

    /// The hosted shape: no source directory at all, so the overlay is the only
    /// source. This is the read half of the #168 fix.
    #[test]
    fn load_union_falls_back_to_the_overlay_with_no_source_dir() {
        let overlays = vec![overlay("hosted", "Hosted flow")];
        let file = load_workflow_union(None, &overlays, "hosted")
            .expect("loads")
            .expect("present");
        assert_eq!(file.id, "hosted");
        assert_eq!(file.name, "Hosted flow");
        assert_eq!(file.nodes.len(), 2);
    }

    /// A source directory that simply has no file for the id also falls through.
    #[test]
    fn load_union_falls_back_when_the_seed_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "other", "Other");
        let overlays = vec![overlay("mine", "Mine")];
        let file = load_workflow_union(Some(dir.path()), &overlays, "mine")
            .expect("loads")
            .expect("present");
        assert_eq!(file.name, "Mine");
    }

    /// Documented precedence: the committed seed file wins over an overlay body
    /// with the same id. The overlay is shadowed, not destroyed.
    #[test]
    fn load_union_prefers_the_seed_file_on_an_id_collision() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "dup", "From seed");
        let overlays = vec![overlay("dup", "From overlay")];
        let file = load_workflow_union(Some(dir.path()), &overlays, "dup")
            .expect("loads")
            .expect("present");
        assert_eq!(file.name, "From seed");
    }

    /// An id neither source has is `Ok(None)` — the caller's clean 404, not an
    /// error.
    #[test]
    fn load_union_of_an_unknown_id_is_none() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "known", "Known");
        assert!(
            load_workflow_union(Some(dir.path()), &[], "ghost")
                .expect("no error")
                .is_none()
        );
        assert!(
            load_workflow_union(None, &[], "ghost")
                .expect("no error")
                .is_none()
        );
    }

    /// A malformed overlay body surfaces as an error labelled with its id — the
    /// same shape a malformed on-disk file gets.
    #[test]
    fn load_union_of_a_malformed_overlay_is_an_error() {
        let overlays = vec![OverlayWorkflow {
            id: "broken".to_string(),
            toml: "id = \"broken\"\nname = \"Broken\"\n".to_string(),
        }];
        let err = load_workflow_union(None, &overlays, "broken").unwrap_err();
        assert!(err.to_string().contains("trigger"), "{err}");
        assert!(err.to_string().contains("broken.toml"), "{err}");
    }

    /// The list union dedupes by id with the seed winning, and keeps a stable
    /// order (seed scan first, then overlays by id).
    #[test]
    fn list_union_dedupes_with_source_winning() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "dup", "From seed");
        seed(dir.path(), "aaa", "Seed A");
        let overlays = vec![
            overlay("zzz", "Overlay Z"),
            overlay("dup", "From overlay"),
            overlay("mmm", "Overlay M"),
        ];
        let files = list_workflows_union(Some(dir.path()), &overlays);
        let ids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "dup", "mmm", "zzz"]);
        let dup = files.iter().find(|f| f.id == "dup").unwrap();
        assert_eq!(dup.name, "From seed", "the seed file must win");
    }

    /// With no source directory, the list is exactly the overlay set.
    #[test]
    fn list_union_with_no_source_dir_is_the_overlay_set() {
        let overlays = vec![overlay("b", "B"), overlay("a", "A")];
        let files = list_workflows_union(None, &overlays);
        let ids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    /// One malformed overlay skips only itself — the same tolerance the seed
    /// scan has, so a single bad graph never empties the picker.
    #[test]
    fn list_union_skips_a_malformed_overlay() {
        let overlays = vec![
            overlay("good", "Good"),
            OverlayWorkflow {
                id: "bad".to_string(),
                toml: "id = \"bad\"\nname =".to_string(),
            },
        ];
        let files = list_workflows_union(None, &overlays);
        let ids: Vec<&str> = files.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["good"]);
    }

    /// A malformed overlay whose id collides with a global workflow must not
    /// let the global slip into the list: `load_workflow_with_globals`
    /// resolves the company's (broken) definition first and errors, so a list
    /// entry backed by the global instead would be one the loader can never
    /// actually return.
    #[test]
    fn list_with_globals_reserves_a_malformed_overlays_id_against_the_global() {
        let taken = crate::globals::workflows()[0].id.clone();
        let overlays = vec![OverlayWorkflow {
            id: taken.clone(),
            toml: "id = \"broken\"\nname =".to_string(),
        }];

        let listed = list_workflows_with_globals(None, &overlays, &[]);
        assert!(
            listed.iter().all(|f| f.id != taken),
            "the global must not appear in place of the company's malformed definition: {listed:?}"
        );

        let loaded = load_workflow_with_globals(None, &overlays, &[], &taken);
        assert!(
            loaded.is_err(),
            "the loader must surface the malformed overlay's error, not the global"
        );
    }

    // -----------------------------------------------------------------------
    // Console drift guard (issue #260)
    // -----------------------------------------------------------------------
    //
    // The console pre-flights the destination and schedule rules client-side so
    // a wrong target is caught without a round trip. That is worth keeping — it
    // is the difference between instant feedback and a save that bounces — but
    // it makes one rule live in two hand-written places, free to drift. Issue
    // #260 reports the drift that already happened: two different messages for
    // the same rule.
    //
    // These tests are the coupling. Each shared fragment is asserted TWICE —
    // once against this module's live `validate()` output, so a server rewording
    // fails here, and once against the console source, so a console rewording
    // fails here too. Neither side can be reworded alone.
    //
    // This is a tripwire, not a proof. The fragment only has to APPEAR in the
    // console source, so a stale copy left in a comment would false-pass, and
    // nothing here checks that the console's rule FIRES in the same cases the
    // host's does. What it does buy is that the specific failure #260 describes
    // — one side reworded, the other silently asserting the old contract — can
    // no longer happen quietly. Closing the rest means option 3 from the issue:
    // the host exposing the destination contract as data.

    /// The console's workflow creator, read at compile time so a file move is a
    /// build error naming the path rather than a silently-skipped test.
    const CONSOLE_DIALOG: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/frontend/src/views/WorkflowCreateDialog.tsx"
    ));
    const CONSOLE_DIALOG_PATH: &str = "frontend/src/views/WorkflowCreateDialog.tsx";

    /// The console's workflow API module, which declares the picker's
    /// destination kinds.
    const CONSOLE_API: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/frontend/src/api/workflows.ts"
    ));
    const CONSOLE_API_PATH: &str = "frontend/src/api/workflows.ts";

    /// How an `email` destination with a non-address target ends, on both sides.
    const EMAIL_TARGET_TAIL: &str = "is not an email address — give the recipient's full address.";
    /// How a `channel` destination with no target ends, on both sides.
    const CHANNEL_TARGET_TAIL: &str = "name the channel to post the report to.";

    /// A graph that trips both destination target rules at once.
    const BAD_DESTINATIONS: &str = r#"
        id = "wf"
        name = "WF"

        [[node]]
        id = "start"
        kind = "trigger"
        name = "Start"

        [[node]]
        id = "mailer"
        kind = "output"
        name = "Mailer"
        [node.destination]
        kind = "email"
        target = "nope"

        [[node]]
        id = "poster"
        kind = "output"
        name = "Poster"
        [node.destination]
        kind = "channel"
    "#;

    #[test]
    fn destination_messages_match_the_console() {
        let raw: RawWorkflow = toml::from_str(BAD_DESTINATIONS).expect("the fixture is valid TOML");
        // Destination checks are unconditional, so the load-path (`false`) form
        // surfaces them exactly as `parse_workflow` does.
        let problems = validate(&raw, false).join("\n");

        for tail in [EMAIL_TARGET_TAIL, CHANNEL_TARGET_TAIL] {
            assert!(
                problems.contains(tail),
                "the host stopped saying `{tail}` — if that rewording is deliberate, \
                 update this const AND the matching message in {CONSOLE_DIALOG_PATH}, \
                 so an author who trips the pre-flight and an author who trips the 400 \
                 are still told the same thing.\nhost said:\n{problems}"
            );
            assert!(
                CONSOLE_DIALOG.contains(tail),
                "{CONSOLE_DIALOG_PATH} no longer says `{tail}` — the console's \
                 client-side pre-flight has drifted from the host's rule (issue #260). \
                 Reword both sides together, or drop the pre-flight and surface the \
                 host's message on the failed save."
            );
        }
    }

    /// The picker's destination kinds, extracted from the console's own
    /// `DESTINATION_KINDS` block. A kind added on one side alone is either a
    /// picker option the host rejects or one the host accepts and the author
    /// can never choose.
    #[test]
    fn destination_kinds_match_the_console() {
        let start = CONSOLE_API.find("export const DESTINATION_KINDS").unwrap_or_else(|| {
            panic!("`DESTINATION_KINDS` is gone from {CONSOLE_API_PATH} — it is what this test reads")
        });
        // Slice from the array opener, NOT from the declaration: the type
        // annotation in between ends `WorkflowDestination["kind"];`, which
        // contains a literal `"];` and would close the block before the first
        // entry.
        let block = &CONSOLE_API[start..];
        let open = block.find("= [").unwrap_or_else(|| {
            panic!("`DESTINATION_KINDS` in {CONSOLE_API_PATH} is no longer an array literal")
        });
        let block = &block[open..];
        let end = block
            .find("];")
            .unwrap_or_else(|| panic!("`DESTINATION_KINDS` in {CONSOLE_API_PATH} has no `];`"));
        let block = &block[..end];

        // Scan for `value: "…"` entries. The type annotation on the same
        // declaration carries a bare `value:` with no string, so keying on the
        // opening quote is what keeps it out.
        let needle = "value: \"";
        let mut console = std::collections::BTreeSet::new();
        let mut rest = block;
        while let Some(at) = rest.find(needle) {
            rest = &rest[at + needle.len()..];
            let close = rest
                .find('"')
                .unwrap_or_else(|| panic!("unterminated `value:` in {CONSOLE_API_PATH}"));
            console.insert(&rest[..close]);
            rest = &rest[close..];
        }

        let host: std::collections::BTreeSet<&str> =
            WORKFLOW_DESTINATION_KINDS.iter().copied().collect();
        assert_eq!(
            console, host,
            "the console's DESTINATION_KINDS picker ({CONSOLE_API_PATH}) and the host's \
             WORKFLOW_DESTINATION_KINDS disagree — one side offers a kind the other \
             does not know (issue #260)"
        );
    }

    /// The console's `looksLikeCron` pre-flight counts whitespace-separated
    /// fields and accepts exactly five, so it is only correct while the host's
    /// parser draws the line in the same place. Relaxing the host to accept a
    /// 6-field (seconds) expression without touching the console would make the
    /// console reject input the host now takes — the drift direction #260 says
    /// bites, because the console is the stricter side by construction.
    #[test]
    fn cron_arity_matches_the_console_preflight() {
        use crate::runtime::cron::CronExpr;
        assert!(CronExpr::parse("0 9 * * MON").is_ok(), "5 fields");
        assert!(CronExpr::parse("0 9 * *").is_err(), "4 fields");
        assert!(CronExpr::parse("0 0 9 * * MON").is_err(), "6 fields");
        assert!(
            CONSOLE_DIALOG.contains("function looksLikeCron"),
            "{CONSOLE_DIALOG_PATH} no longer defines `looksLikeCron` — this test \
             exists to pin the arity that helper assumes"
        );
    }
}
