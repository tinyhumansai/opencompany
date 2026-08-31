//! OpenCompany Rust core.
//!
//! This crate is intentionally shaped as a light host around OpenHuman and the
//! TinyHumans Rust modules. The default build stays small; enable the `tiny`
//! feature to compile against the sibling `tiny*` crates.

pub mod analytics;
pub mod app;
pub mod brain;
/// How `build.rs` picks the value behind [`BUILD_COMMIT`]. Compiled here only
/// under `cfg(test)`.
///
/// The build script pulls the same file in with `include!`, so the code these
/// tests execute is the code that stamps the binary. The runtime reads the
/// finished constant and never calls the resolver, so outside a test build
/// this module would be dead code rather than a surface.
#[cfg(test)]
mod build_stamp;
/// Chargebee billing (issue #788): the REST client and the billing operations
/// the agent's tools call. The toolbelt bridge lives in `harness::chargebee`.
#[cfg(feature = "chargebee")]
pub mod chargebee;
pub mod company;
/// Local-only runtime host used by the packaged Tauri desktop application.
/// It embeds the existing operator API and ships the curated company presets;
/// it deliberately does not pull OpenHuman's local-AI configuration into the
/// OpenCompany product.
pub mod desktop;
pub mod economy;
pub mod error;
pub mod feedback;
/// The global baseline every company gets, whichever vertical it started from:
/// a small roster, the workflow graphs that are not vertical-specific, the
/// always-installed skills, and the tool namespaces a company with no
/// `[tools]` section of its own starts with. Authored in `globals/`, embedded
/// at build time.
pub mod globals;
/// WS4: openhuman embedded as a library (the harness). Compiled only under the
/// `openhuman` feature; the default build links none of it and keeps the
/// echo-brained, offline behaviour unchanged.
#[cfg(feature = "openhuman")]
pub mod harness;
/// Turning dropped files and links into memory: extraction, then chunking.
/// The console's Brain drop zone is the caller; the ports are unchanged.
pub mod ingest;
/// Dynamic ledgers: the company's own record — goals, decisions, and whatever
/// axis a workspace declares — as a folded append-only log rendered into the
/// `derived/` folder. The task board is registered here as a native ledger so
/// one discovery surface reaches every one of them.
pub mod ledger;
/// WS5: pure Usage & Finances projections over the runtime's accounting data
/// (usage samples, ledger, `[budget]`). No I/O; WS2 wraps these in GraphQL.
pub mod metering;
pub mod openhuman;
/// PayPal wallet + transaction visibility (issue #789).
#[cfg(feature = "paypal")]
pub mod paypal;
pub mod policy;
pub mod ports;
/// The `x-sdk-name: opencompany` identity attached to this crate's own
/// backend HTTP clients. Ungated (no `openhuman` feature requirement) because
/// `brain/` and `feedback/` need it and neither is feature-gated.
pub mod product;
/// Machines that dial out to execute this host's work (the `runner` feature).
#[cfg(feature = "runner")]
pub mod runner;
pub mod runtime;
pub mod server;
pub mod store;
/// The process-wide environment lock every env-mutating unit test in this crate
/// serialises on. Test-only: it compiles into the lib test binary and nowhere
/// else.
#[cfg(test)]
pub(crate) mod test_support;
pub mod tiny;
/// Transient per-turn progress bus — the live tool-call timeline that rides the
/// company SSE feed while a turn runs. Separate from the durable event log; the
/// `openhuman` harness publishes, the operator SSE route subscribes. Always
/// compiled (no `openhuman` types); no publishers in the default build.
pub mod turn_stream;
/// Issue #29 (epic #26): run a company's workflows on the embedded `tinyflows`
/// engine, with agent nodes executing on the harness pool. Compiled only under
/// the `openhuman` feature; the default build links none of it.
#[cfg(feature = "openhuman")]
pub mod workflows;

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

/// The Git commit this binary was built from.
///
/// A short object id (`d31e532f7c8a`), suffixed `-dirty` when tracked files
/// differed from that commit at build time, or the literal `"unknown"` when
/// no source could name one. `build.rs` decides; `src/build_stamp.rs` records
/// why, including what a build with no repository beside it does.
///
/// A compile-time constant, like [`VERSION`] beside it, and for the reason
/// that motivates it: [`VERSION`] has read `0.1.0` for thousands of commits,
/// so "a user on 0.1.0 hit this" and "a user on 0.1.0 at `d31e532f` hit this"
/// are the same sentence to everything that reports it today.
pub const BUILD_COMMIT: &str = env!("OPENCOMPANY_BUILD_COMMIT");
