use std::path::PathBuf;

/// Crate-wide result type.
pub type Result<T> = std::result::Result<T, OpenCompanyError>;

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

    /// A tool was invoked outside the manifest grant.
    #[error("tool not granted: {0}")]
    ToolNotGranted(String),

    /// A spend would exceed the company's budget scope.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),

    /// An operation conflicts with the company's lifecycle state (e.g. the
    /// company is paused or archived).
    #[error("company is {0}")]
    LifecycleConflict(String),

    /// A write conflicts with a durable invariant that is not a lifecycle state
    /// (e.g. uninstalling a built-in skill, or deleting a manifest-defined
    /// agent). Renders as `409 Conflict`.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A request was malformed or internally inconsistent (e.g. an approval
    /// resolution that pairs a `deny` verdict with an amended payload).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

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
            Self::ToolNotGranted(_) => "tool_not_granted".to_string(),
            Self::BudgetExceeded(_) => "budget_exceeded".to_string(),
            Self::LifecycleConflict(_) => "lifecycle_conflict".to_string(),
            Self::Conflict(_) => "conflict".to_string(),
            Self::InvalidRequest(_) => "invalid_request".to_string(),
            Self::Config(_) => "config_error".to_string(),
            Self::Orchestration { code, .. } => code.clone(),
            Self::Tinyplace { code, .. } => format!("tinyplace_{code}"),
            Self::Unimplemented(_) => "unimplemented".to_string(),
            #[cfg(feature = "openhuman")]
            Self::Harness(_) => "harness_error".to_string(),
        }
    }
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
