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

/// Issue #337: the one guarded mover for the board's automatic edge. Advances a
/// settled attempt's card **only** from `in_progress`, so a card an operator
/// parked in Paused or In Review is never yanked back by a late settle. See
/// [`advance`].
pub mod advance;
/// Issue #372: the host-side projection of a parked effect's payload onto the
/// approval card — shown in full by default, with a credential-key denylist as
/// the safety net, and bounded so an agent-authored blob cannot reach a
/// browser unbounded. See [`approval_display`].
pub mod approval_display;
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
/// Issue #290: [`RuntimeHandover`] — the live, per-instance state a rebuilt
/// company runtime must inherit rather than construct a second copy of (the
/// journal, the approval gate, the grant set, the event log, the stores, the
/// harness pool, the MCP runtime, and the two serialising mutexes). See
/// [`handover`].
pub mod handover;
pub mod journal;
pub mod mailbox_poller;
/// Issue #290: replacing a registered company's runtime in place, so first-time
/// BYOK setup takes effect without a process restart. See [`rebuild`].
pub mod rebuild;
pub mod registry;
/// Issue #383: [`RunSupervisor`] — the live set of workflow runs an operator can
/// still stop, so `POST …/workflows/runs/{runId}/cancel` has something to reach.
/// Compiled in every build: it is a plain map of stop signals and touches no
/// engine. See [`run_supervisor`].
pub mod run_supervisor;
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
/// Issue #395: resuming a workflow run that paused on a `requires_approval`
/// node, once the operator has signed the gate off. See [`workflow_resume`].
pub mod workflow_resume;
pub mod workflow_scheduler;
/// Issue #395: the supervisor-registration + outcome-journalling discipline
/// every workflow entry point owes, in one place. See [`workflow_spawn`].
pub mod workflow_spawn;

pub use advance::{SYSTEM_ATTRIBUTION, advance_settled_card, append_result};
pub use builder::{RuntimeBuilder, company_id_from_name};
pub use channel::{OPERATOR_CHANNEL, OperatorChannel};
pub use cron::{CivilTime, CronExpr};
pub use cycle::CycleRunner;
pub use handover::RuntimeHandover;
pub use rebuild::{BootInputs, RebuildRequest, RuntimeRebuilder, rebuild_company};
pub use registry::CompanyRegistry;
pub use run_supervisor::{RunGuard, RunSupervisor};
pub use scheduler::{Clock, CompanyScheduler, FakeClock, SystemClock};
pub use tools::StubToolProvider;
pub use types::{ApprovalSummary, CompanyStatus, CycleReport};
pub use workflow_outcome::{record_run_finished, sweep_interrupted_runs};
pub use workflow_resume::WORKFLOW_APPROVE_KIND;
pub use workflow_scheduler::WorkflowScheduler;
pub use workflow_spawn::WorkflowSpawn;

// The assembly struct lives under `company/` to match the `ports.md` sketch
// (`src/company/runtime.rs`); re-export it here as the kernel's public surface.
pub use crate::company::runtime::CompanyRuntime;
