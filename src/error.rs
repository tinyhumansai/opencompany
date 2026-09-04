use std::path::PathBuf;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, OpenCompanyError>;

/// One structured problem with a workflow graph draft (issue #1016).
///
/// Unlike the flat `Vec<String>` an [`OpenCompanyError::DataInvalid`] carries,
/// each problem names the node it belongs to and the config field at fault, so
/// the console can highlight the exact node + field instead of parsing a joined
/// sentence. `node_id` is the offending node's id — or, for an edge problem, the
/// dangling endpoint's id — and is `None` for a graph-level problem with no
/// single owner (an inescapable cycle names several nodes at once). `field` is
/// the config path at fault (`config.url`, `config.set`, `workflow_id`, `from`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkflowProblem {
    /// The node id this problem belongs to (or the dangling endpoint id for an
    /// edge problem); `None` for a graph-level problem with no single owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// The config field at fault (`config.url`, `config.set`, `workflow_id`,
    /// `from`/`to`); `None` when the problem is not about a single field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The human-readable problem, in the same prosumer language the flat
    /// validation messages use.
    pub message: String,
}

impl WorkflowProblem {
    /// A problem pinned to a specific node and config field.
    pub fn node_field(
        node_id: impl Into<String>,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let node_id = node_id.into();
        Self {
            node_id: (!node_id.trim().is_empty()).then_some(node_id),
            field: Some(field.into()),
            message: message.into(),
        }
    }
}

/// A graph-level problem with no single owner keeps its message and leaves both
/// `node_id` and `field` empty — the fallback for every validation string that
/// is not enriched with a node/field.
impl From<String> for WorkflowProblem {
    fn from(message: String) -> Self {
        Self {
            node_id: None,
            field: None,
            message,
        }
    }
}

impl From<&str> for WorkflowProblem {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

/// Errors returned by OpenCompany.
#[derive(Debug, thiserror::Error)]
pub enum OpenCompanyError {
    /// An OpenHuman checkout path was expected but not found.
    #[error("openhuman root does not exist: {0}")]
    MissingOpenHumanRoot(PathBuf),

    /// The OpenHuman process failed to start or wait.
    #[error("openhuman process error: {0}")]
    OpenHumanProcess(#[from] std::io::Error),

    /// An OpenHuman JSON-RPC call failed at the transport or protocol level.
    ///
    /// Carries the failure as an owned `code`/`message` pair rather than a
    /// `#[from] std::io::Error` so it never collides with the existing
    /// `OpenHumanProcess` conversion.
    #[error("openhuman rpc error ({code}): {message}")]
    OpenHuman {
        /// The JSON-RPC error code (or a synthetic transport code).
        code: i64,
        /// A human-readable description of the failure.
        message: String,
    },

    /// No manifest (`company.toml` or `agents.toml`) was found.
    #[error("no company.toml or agents.toml found in {0}")]
    MissingManifest(PathBuf),

    /// The manifest file could not be read from disk.
    #[error("could not read manifest {path}: {source}")]
    ManifestRead {
        /// The manifest path that failed to load.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The manifest is not valid TOML.
    #[error("{0} is not valid TOML: {1}")]
    ManifestParse(PathBuf, String),

    /// The manifest parsed but failed validation. Every message is written in
    /// prosumer language and lists all problems at once.
    #[error("{}", format_manifest_problems(.path, .problems))]
    ManifestInvalid {
        /// The manifest path that failed validation.
        path: PathBuf,
        /// One human-readable problem per line.
        problems: Vec<String>,
    },

    /// A company data file (workflow graph, skill doc, workspace note) could
    /// not be read from disk.
    #[error("could not read {path}: {source}")]
    DataRead {
        /// The data file path that failed to load.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A company data file could not be parsed (invalid TOML, or a malformed
    /// SKILL.md frontmatter block).
    #[error("{path} could not be parsed: {message}")]
    DataParse {
        /// The data file path (or synthetic label) that failed to parse.
        path: PathBuf,
        /// A human-readable description of the parse failure.
        message: String,
    },

    /// A company data file parsed but failed validation. Every message is
    /// written in prosumer language and lists all problems at once, mirroring
    /// [`Self::ManifestInvalid`].
    #[error("{}", format_manifest_problems(.path, .problems))]
    DataInvalid {
        /// The data file path that failed validation.
        path: PathBuf,
        /// One human-readable problem per line.
        problems: Vec<String>,
    },

    /// A persistence backend reported a failure that has no more specific
    /// variant.
    #[error("store error: {0}")]
    Store(String),

    /// A store file could not be read from or written to disk.
    #[error("could not read {path}: {source}")]
    StoreIo {
        /// The bundle path that failed I/O.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A value failed to (de)serialize through JSON.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// No company is registered under the requested id.
    #[error("company not found: {0}")]
    CompanyNotFound(String),

    /// An addressed resource other than a company does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// No MCP install exists in the addressed company's registry store.
    #[cfg(any(feature = "openhuman", feature = "mcp"))]
    #[error("MCP server not found: {0}")]
    McpServerNotFound(String),

    /// A tool was invoked outside the manifest grant.
    #[error("tool not granted: {0}")]
    ToolNotGranted(String),

    /// A spend would exceed the company's budget scope.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),

    /// A workspace write would exceed the per-file cap or the company's tree
    /// quota (issue #553). Refused before anything is stored, so the tree is
    /// exactly as it was.
    #[error("{0}")]
    WorkspaceQuota(String),

    /// A workflow run was refused because the company is already at its
    /// configured ceiling of concurrent in-flight runs (issue #401).
    ///
    /// Raised before the run is registered or spawned, so nothing is journaled
    /// and no run id is minted — the caller gets a `429` with no run to follow.
    /// The message is actionable because the operator has three real levers.
    #[error(
        "this company is already running its maximum of {limit} workflow runs at once; wait for one to finish, stop one from the runs view (POST …/workflows/runs/{{id}}/cancel), or raise `[workflows].max_in_flight_runs`"
    )]
    WorkflowRunLimit {
        /// The configured ceiling that was hit.
        limit: usize,
    },

    /// A workflow run failed after some of its nodes had already done durable
    /// work (issue #1008).
    ///
    /// # Why an error variant, rather than a message
    ///
    /// A run's outcome is journaled by the **caller**, off what
    /// [`WorkflowRunner::run`](crate::ports::WorkflowRunner::run) returned — and
    /// the error arm returned nothing but a string. So a run that opened two
    /// board cards, parked an approval and then broke at a later node journaled
    /// a `WorkflowRunFinished` with every one of those lists empty: the cards and
    /// the approval were sitting in front of the operator while no run admitted
    /// to opening them.
    ///
    /// This carries the partial run so the caller can list what did happen. It
    /// **wraps** the underlying failure rather than replacing it, so
    /// [`Display`](std::fmt::Display), [`code`](Self::code) and the HTTP status
    /// are exactly what they were — a caller that never asks for the partial
    /// cannot tell this variant apart from the error inside it, which is the
    /// property that keeps it additive.
    #[error("{source}")]
    WorkflowRunFailed {
        /// The failure as it would have been reported before this wrapper.
        #[source]
        source: Box<OpenCompanyError>,
        /// What the run had done by the time it broke: its per-node rows and
        /// output, and the durable board / approval / notice rows its nodes
        /// already produced.
        partial: Box<crate::ports::WorkflowRun>,
    },

    /// An operation conflicts with the company's lifecycle state (e.g. the
    /// company is paused or archived).
    #[error("company is {0}")]
    LifecycleConflict(String),

    /// A side-effecting effect was refused because the company's emergency stop
    /// is engaged (issue #86).
    ///
    /// Distinct from [`LifecycleConflict`](Self::LifecycleConflict), which is a
    /// *durable* lifecycle state: this vetoes new side-effecting work while the
    /// switch is down and clears the moment it is released. `EffectGroup::Other`
    /// (chat) stays exempt, exactly as in `ApprovalGate::evaluate`.
    #[error("emergency stop is engaged: {0}")]
    EmergencyStop(String),

    /// A write conflicts with a durable invariant that is not a lifecycle state
    /// (e.g. uninstalling a built-in skill, or deleting a manifest-defined
    /// agent). Renders as `409 Conflict`.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A surface this binary was compiled without. Renders as `409 Conflict`
    /// with the stable code `not_in_build`.
    ///
    /// Split out of [`Conflict`](Self::Conflict) because the two are the same
    /// status but opposite advice. `Conflict` is overloaded across a hundred
    /// call sites — a lost publish race, an optimistic-lock version skew, a
    /// duplicate desk id — most of which a caller clears by retrying or by
    /// sending something else. This one is a fact about the binary: nothing the
    /// operator does in this session changes it, and a console that cannot tell
    /// the two apart can only offer the recoverable reading of both.
    #[error("{0}")]
    NotInBuild(String),

    /// A surface this build has, that this company has not configured. Renders
    /// as `409 Conflict` with the stable code `not_configured`.
    ///
    /// The middle rung between [`NotInBuild`](Self::NotInBuild) and a genuine
    /// failure: retrying the same read never succeeds, but the operator can
    /// reach the control that clears it. The message names that control.
    #[error("{0}")]
    NotConfigured(String),

    /// The company's runtime is quiescing for a swap (issue #290): it has
    /// stopped accepting new cycles while the one in flight drains, and a
    /// successor is being built. Distinct from
    /// [`LifecycleConflict`](Self::LifecycleConflict), which is a *durable*
    /// state an operator chose; this one clears itself within a turn, so it
    /// renders as `503 Service Unavailable` and the caller should retry.
    #[error("company {0} is being rebuilt; retry shortly")]
    Quiescing(String),

    /// A request was malformed or internally inconsistent (e.g. an approval
    /// resolution that pairs a `deny` verdict with an amended payload).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// A workflow graph draft was rejected at author time with one or more
    /// structured problems (issue #1016). Distinct from [`Self::DataInvalid`]:
    /// each [`WorkflowProblem`] carries the node id and config field at fault so
    /// the console can highlight the exact node + field, while `Display` still
    /// joins every message so the human `error` string stays populated and every
    /// string-only reader keeps working. Renders as `400 Bad Request`; the HTTP
    /// envelope additionally carries a `problems` array (see `server::error`).
    #[error("{}", format_workflow_problems(.problems))]
    WorkflowInvalid {
        /// Every problem found, each naming its node and config field where one
        /// applies.
        problems: Vec<WorkflowProblem>,
    },

    /// Runtime configuration could not be resolved (bad value, unreadable or
    /// malformed `config.toml`).
    #[error("configuration error: {0}")]
    Config(String),

    /// The hosted Medulla orchestrator (`/orchestration/v1`) reported a wire
    /// error. `code` is the verbatim `ORCH_*` string from the server envelope.
    #[error("orchestration error [{code}]: {message}")]
    Orchestration {
        /// The verbatim `ORCH_*` error code from the server envelope.
        code: String,
        /// The human-readable error message.
        message: String,
    },

    /// A tiny.place economy transport or protocol failure. `code` is a stable
    /// machine-readable token (e.g. `unreachable`, `http_502`); `message` is the
    /// human-readable detail.
    #[error("tinyplace error ({code}): {message}")]
    Tinyplace {
        /// A stable, machine-readable failure token.
        code: String,
        /// A human-readable description of the failure.
        message: String,
    },

    /// A TinyHumans backend transport or protocol failure. `code` is a stable
    /// machine-readable token (e.g. `unreachable`, `http_502`); `message` is the
    /// human-readable detail.
    #[error("tinyhumans error ({code}): {message}")]
    TinyHumans {
        /// A stable, machine-readable failure token.
        code: String,
        /// A human-readable description of the failure.
        message: String,
    },

    /// A Chargebee Billing API failure, or a tool argument this crate rejected
    /// before making the call (issue #788).
    ///
    /// Carries `status` alongside `code` because Chargebee reports business
    /// outcomes — a customer that does not exist, a currency the site has not
    /// enabled — as 4xx responses whose JSON body names the real problem. The
    /// agent needs that body, not the status, so both are preserved; a locally
    /// rejected argument uses `status: 0` and `code: invalid_arguments`.
    #[error("chargebee error ({code}): {message}")]
    Chargebee {
        /// The HTTP status, or `0` when the failure never reached the network.
        status: u16,
        /// Chargebee's `api_error_code`, or a local token.
        code: String,
        /// A human-readable description of the failure.
        message: String,
    },

    /// A PayPal REST API failure, or an argument rejected before the call
    /// (issue #789).
    #[error("paypal error ({code}): {message}")]
    Paypal {
        /// The HTTP status, or `0` when the failure never reached the network.
        status: u16,
        /// PayPal's `name`/`error` token, or a local one.
        code: String,
        /// A human-readable description of the failure.
        message: String,
    },

    /// A spawned background task the caller was waiting on panicked or was
    /// aborted, so its result never arrived (issue #383).
    ///
    /// Distinct from a failure *inside* that work, which reports itself through
    /// its own variant. This one means the work's outcome is unknown — the
    /// caller's wait ended without an answer. On the approval path the verdict
    /// it settled is already durable regardless; only the follow-up cycle is
    /// unaccounted for.
    #[error("background work did not complete: {0}")]
    BackgroundTask(String),

    /// A port method has no implementation in the current build.
    #[error("port not implemented: {0}")]
    Unimplemented(&'static str),

    /// The embedded openhuman harness failed to build or run an agent.
    #[cfg(feature = "openhuman")]
    #[error("harness error: {0}")]
    Harness(String),
}

impl OpenCompanyError {
    /// Builds an [`OpenCompanyError::Orchestration`] from a wire error code and
    /// message. `code` is stored verbatim and surfaced by [`Self::code`].
    pub fn orchestration(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Orchestration {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Builds an [`OpenCompanyError::Tinyplace`] from a failure token and
    /// message. `code` is stored verbatim and surfaced by [`Self::code`].
    pub fn tinyplace(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tinyplace {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The partial run a [`WorkflowRunFailed`](Self::WorkflowRunFailed) carries,
    /// if this is one (issue #1008).
    ///
    /// `None` for every other error, including a workflow failure raised before
    /// the engine ran — there is no partial run in that case, and an empty one
    /// would be a claim rather than an absence.
    pub fn partial_run(&self) -> Option<&crate::ports::WorkflowRun> {
        match self {
            Self::WorkflowRunFailed { partial, .. } => Some(partial),
            _ => None,
        }
    }

    /// The failure underneath a [`WorkflowRunFailed`](Self::WorkflowRunFailed)
    /// wrapper, or `self` when there is none (issue #1008).
    ///
    /// The wrapper is additive by design, so anything that classifies an error —
    /// HTTP status, [`code`](Self::code) — asks for this first and is otherwise
    /// unchanged.
    pub fn unwrapped(&self) -> &Self {
        match self {
            Self::WorkflowRunFailed { source, .. } => source.unwrapped(),
            other => other,
        }
    }

    /// A stable, machine-readable code for this error.
    ///
    /// Surfaced in the HTTP error envelope (`{ "error", "code" }`) so clients
    /// can branch on the code rather than parsing the human-readable message.
    ///
    /// Returns an owned `String` because [`Self::Orchestration`] carries a
    /// runtime `ORCH_*` code that is not `'static`; every other variant maps to
    /// a fixed string literal.
    pub fn code(&self) -> String {
        match self {
            Self::MissingOpenHumanRoot(_) => "openhuman_root_missing".to_string(),
            Self::OpenHumanProcess(_) => "openhuman_process".to_string(),
            Self::OpenHuman { .. } => "openhuman_rpc".to_string(),
            Self::MissingManifest(_) => "manifest_missing".to_string(),
            Self::ManifestRead { .. } => "manifest_read".to_string(),
            Self::ManifestParse(_, _) => "manifest_parse".to_string(),
            Self::ManifestInvalid { .. } => "manifest_invalid".to_string(),
            Self::DataRead { .. } => "data_read".to_string(),
            Self::DataParse { .. } => "data_parse".to_string(),
            Self::DataInvalid { .. } => "data_invalid".to_string(),
            Self::Store(_) => "store_error".to_string(),
            Self::StoreIo { .. } => "store_io".to_string(),
            Self::Serde(_) => "serialization_error".to_string(),
            Self::CompanyNotFound(_) => "company_not_found".to_string(),
            Self::NotFound(_) => "not_found".to_string(),
            #[cfg(any(feature = "openhuman", feature = "mcp"))]
            Self::McpServerNotFound(_) => "mcp_server_not_found".to_string(),
            Self::ToolNotGranted(_) => "tool_not_granted".to_string(),
            Self::BudgetExceeded(_) => "budget_exceeded".to_string(),
            Self::WorkspaceQuota(_) => "workspace_quota_exceeded".to_string(),
            Self::WorkflowRunLimit { .. } => "workflow_run_limit".to_string(),
            // Issue #1008: delegated, not its own code. The wrapper adds a
            // payload for the journal, never a new failure a client should
            // branch on differently.
            Self::WorkflowRunFailed { source, .. } => source.code(),
            Self::LifecycleConflict(_) => "lifecycle_conflict".to_string(),
            Self::EmergencyStop(_) => "emergency_stop".to_string(),
            Self::Conflict(_) => "conflict".to_string(),
            Self::NotInBuild(_) => "not_in_build".to_string(),
            Self::NotConfigured(_) => "not_configured".to_string(),
            Self::Quiescing(_) => "quiescing".to_string(),
            Self::InvalidRequest(_) => "invalid_request".to_string(),
            Self::WorkflowInvalid { .. } => "workflow_invalid".to_string(),
            Self::Config(_) => "config_error".to_string(),
            Self::Orchestration { code, .. } => code.clone(),
            Self::Tinyplace { code, .. } => format!("tinyplace_{code}"),
            Self::TinyHumans { code, .. } => format!("tinyhumans_{code}"),
            Self::Chargebee { code, .. } => format!("chargebee_{code}"),
            Self::Paypal { code, .. } => format!("paypal_{code}"),
            Self::BackgroundTask(_) => "background_task".to_string(),
            Self::Unimplemented(_) => "unimplemented".to_string(),
            #[cfg(feature = "openhuman")]
            Self::Harness(_) => "harness_error".to_string(),
        }
    }
}

/// Joins every workflow problem's message into one human-readable string, so a
/// [`OpenCompanyError::WorkflowInvalid`] renders the same flat sentence a
/// string-only caller expects even though it carries structured problems. The
/// per-node/field detail rides in the `problems` vec, surfaced by the HTTP
/// envelope; `Display` stays a plain join for logs and legacy readers.
fn format_workflow_problems(problems: &[WorkflowProblem]) -> String {
    problems
        .iter()
        .map(|problem| problem.message.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_manifest_problems(path: &std::path::Path, problems: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "{} has {} problem{}:",
        path.display(),
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for problem in problems {
        let _ = write!(out, "\n  • {problem}");
    }
    out
}
