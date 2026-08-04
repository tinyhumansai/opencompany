//! The company kernel: assembling ports into a running company and driving the
//! cycle loop.
//!
//! - [`CompanyRuntime`] (defined in [`crate::company::runtime`]) is the wired
//!   assembly of the nine ports.
//! - [`RuntimeBuilder`] wires one from filesystem defaults.
//! - [`CycleRunner`] runs the serial drain → load → think → gate → persist loop.
//! - [`CompanyRegistry`] maps ids to running runtimes for both the single- and
//!   multi-company cases.
//! - [`CompanyScheduler`] drives the manifest's `[[schedule]]` crons;
//!   [`WorkflowScheduler`] drives the crons authored on saved workflow graphs'
//!   trigger nodes (issue #169). Both share the [`cron`] matcher and [`Clock`].
//! - The [`journal`] backs at-most-once effects and the durable approval queue.

/// Brain-agnostic resolution of a task card's `assignee` against the full
/// roster — teammates, overlay teammates and desks (issue #205). Shared by the
/// harness dispatch path and the REST write boundary so the board's assignee
/// means one thing. See [`assignee`].
pub mod assignee;
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
/// Single-use grants minted when an operator approves a blocked tool call
/// (issue #243). Compiled in every build: the journal records and their replay
/// are feature-independent, so a company that ran under the harness stays
/// replayable by a build without it.
pub mod grants;
pub mod journal;
pub mod mailbox_poller;
pub mod registry;
pub mod scheduler;
/// Issue #203: the Telegram `getUpdates` long-polling listener — the inbound
/// path that needs no public URL, mirroring OpenHuman. See [`telegram_poller`].
pub mod telegram_poller;
pub mod tools;
pub mod types;
/// Issue #228: the single place a finished workflow run is journaled, shared by
/// the console's run route and the cron [`WorkflowScheduler`] so a run's history
/// is uniform no matter what started it. See [`workflow_outcome`].
pub mod workflow_outcome;
pub mod workflow_scheduler;

pub use builder::{RuntimeBuilder, company_id_from_name};
pub use channel::{OPERATOR_CHANNEL, OperatorChannel};
pub use cron::{CivilTime, CronExpr};
pub use cycle::CycleRunner;
pub use registry::CompanyRegistry;
pub use scheduler::{Clock, CompanyScheduler, FakeClock, SystemClock};
pub use tools::StubToolProvider;
pub use types::{ApprovalSummary, CompanyStatus, CycleReport};
pub use workflow_outcome::record_run_finished;
pub use workflow_scheduler::WorkflowScheduler;

// The assembly struct lives under `company/` to match the `ports.md` sketch
// (`src/company/runtime.rs`); re-export it here as the kernel's public surface.
pub use crate::company::runtime::CompanyRuntime;
