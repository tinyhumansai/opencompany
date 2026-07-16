//! OpenCompany Rust core.
//!
//! This crate is intentionally shaped as a light host around OpenHuman and the
//! TinyHumans Rust modules. The default build stays small; enable the `tiny`
//! feature to compile against the sibling `tiny*` crates.

pub mod app;
pub mod brain;
pub mod company;
pub mod economy;
pub mod error;
pub mod feedback;
/// WS4: openhuman embedded as a library (the harness). Compiled only under the
/// `openhuman` feature; the default build links none of it and keeps the
/// echo-brained, offline behaviour unchanged.
#[cfg(feature = "openhuman")]
pub mod harness;
pub mod openhuman;
pub mod policy;
pub mod ports;
pub mod runtime;
pub mod server;
pub mod store;
pub mod tiny;

pub use app::{AppConfig, AppState};
pub use brain::EchoBrain;
pub use company::{CompanyManifest, run_company};
pub use economy::{build_agent_card, render_skill_md};
pub use error::{OpenCompanyError, Result};
pub use feedback::{
    ConsentMode, FeedbackCategory, FeedbackInput, FeedbackItem, FeedbackResponse, FeedbackStore,
};
pub use policy::ManifestApprovalGate;
pub use ports::{CompanyEvent, CompanyId, Effect, EffectDisposition, PolicyDecision, Verdict};
pub use runtime::{CompanyRegistry, CompanyRuntime, CycleReport, RuntimeBuilder};
pub use store::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore, FsSecretStore};

/// Current crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
