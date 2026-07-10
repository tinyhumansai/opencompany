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
}
