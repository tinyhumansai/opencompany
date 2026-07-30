//! The company kernel: assembling ports into a running company and driving the
//! cycle loop.
//!
//! - [`CompanyRuntime`] (defined in [`crate::company::runtime`]) is the wired
//!   assembly of the nine ports.
//! - [`RuntimeBuilder`] wires one from filesystem defaults.
//! - [`CycleRunner`] runs the serial drain → load → think → gate → persist loop.
//! - [`CompanyRegistry`] maps ids to running runtimes for both the single- and
//!   multi-company cases.
//! - The [`journal`] backs at-most-once effects and the durable approval queue.

pub mod builder;
pub mod channel;
pub mod cron;
pub mod cycle;
/// Brain-agnostic delegation seam (issue #176): the [`RunTurn`] trait +
/// [`DelegationRunner`] the harness brain drives. Compiled only under
/// `openhuman` — it drains the harness delegation queue and yields harness
/// [`TurnOutcome`]s. See [`delegation`].
///
/// [`RunTurn`]: delegation::RunTurn
/// [`DelegationRunner`]: delegation::DelegationRunner
/// [`TurnOutcome`]: crate::harness::TurnOutcome
#[cfg(feature = "openhuman")]
pub mod delegation;
/// Brain-agnostic delegation-tool primitives (issue #176): the tool names,
/// argument schemas, hosted [`ToolManifestEntry`](crate::brain::medulla::wire::ToolManifestEntry)
/// catalog, and desk-lead resolver shared by BOTH the harness and hosted paths.
/// Compiled in every build (the hosted brain ships in the default build).
pub mod delegation_tools;
pub mod journal;
pub mod mailbox_poller;
pub mod registry;
pub mod scheduler;
pub mod tools;
pub mod types;

pub use builder::{RuntimeBuilder, company_id_from_name};
pub use channel::{OPERATOR_CHANNEL, OperatorChannel};
pub use cron::{CivilTime, CronExpr};
pub use cycle::CycleRunner;
pub use registry::CompanyRegistry;
pub use scheduler::{Clock, CompanyScheduler, FakeClock, SystemClock};
pub use tools::StubToolProvider;
pub use types::{ApprovalSummary, CompanyStatus, CycleReport};

// The assembly struct lives under `company/` to match the `ports.md` sketch
// (`src/company/runtime.rs`); re-export it here as the kernel's public surface.
pub use crate::company::runtime::CompanyRuntime;
