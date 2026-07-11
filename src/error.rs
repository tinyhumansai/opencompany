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

    /// A port method has no implementation in the current build.
    #[error("port not implemented: {0}")]
    Unimplemented(&'static str),
}

impl OpenCompanyError {
    /// A stable, machine-readable code for this error.
    ///
    /// Surfaced in the HTTP error envelope (`{ "error", "code" }`) so clients
    /// can branch on the code rather than parsing the human-readable message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingOpenHumanRoot(_) => "openhuman_root_missing",
            Self::OpenHumanProcess(_) => "openhuman_process",
            Self::MissingManifest(_) => "manifest_missing",
            Self::ManifestRead { .. } => "manifest_read",
            Self::ManifestParse(_, _) => "manifest_parse",
            Self::ManifestInvalid { .. } => "manifest_invalid",
            Self::Store(_) => "store_error",
            Self::StoreIo { .. } => "store_io",
            Self::Serde(_) => "serialization_error",
            Self::CompanyNotFound(_) => "company_not_found",
            Self::ToolNotGranted(_) => "tool_not_granted",
            Self::BudgetExceeded(_) => "budget_exceeded",
            Self::LifecycleConflict(_) => "lifecycle_conflict",
            Self::Unimplemented(_) => "unimplemented",
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
