//! OpenCompany Rust core.
//!
//! This crate is intentionally shaped as a light host around OpenHuman and the
//! TinyHumans Rust modules. The default build stays small; enable the `tiny`
//! feature to compile against the sibling `tiny*` crates.

pub mod app;
pub mod company;
pub mod error;
pub mod openhuman;
pub mod server;
pub mod tiny;

pub use app::{AppConfig, AppState};
pub use company::{CompanyManifest, run_company};
pub use error::{OpenCompanyError, Result};

/// Current crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
