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
        let problems = validate(&raw).join("\n");

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
