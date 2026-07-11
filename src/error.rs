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
