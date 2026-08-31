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
/// Which card owns a parked approval on the queue read (#1891).
pub mod approval_ownership;
/// Brain-agnostic resolution of a task card's `assignee` against the full
/// roster — teammates, overlay teammates and desks (issue #205). Shared by the
/// harness dispatch path and the REST write boundary so the board's assignee
/// means one thing. See [`assignee`].
pub mod assignee;
/// Issue #899 (Stage 1): the workflow id and trigger input each **blocked agent
/// node** needs to re-dispatch its run when the operator approves a call gated
/// inside its tool loop — the agent-node companion to [`workflow_gates`]. See
/// [`blocked_nodes`].
pub mod blocked_nodes;
/// Issue #464: [`BoardAnnouncer`] — the [`TaskStore`](crate::ports::tasks::TaskStore)
/// decorator that announces a board write on the company event log, so a card
/// opened by *anything* reaches a watching console without a reload. Emitted at
/// the store rather than at the callers, which is what stops the next writer
/// from silently announcing nothing. See [`board_events`].
pub mod board_events;
pub mod builder;
pub mod channel;
/// Issue #469: one turn, one continuation. Tracks how many of a turn's parked
/// approvals are still undecided, so resolving several sign-offs raised by the
/// same turn re-runs that turn **once** — after the last decision — rather than
/// once per decision. See [`continuation`].
pub mod continuation;
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
/// The write guard on the `derived/` folder: a
/// [`WorkspaceStore`](crate::ports::WorkspaceStore) decorator that refuses a
/// hand-written edit to a file a ledger renders, and names the tool that
/// actually writes the row.
pub mod derived_guard;
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
/// Issue #1845: [`LifecycleScheduler`] — the process-wide daily tick that
/// nudges a signup who hit their day-7 boundary without saving a workflow,
/// by email and by a durable in-app [`Notification`](crate::ports::notifications::Notification)
/// row. See [`lifecycle_scheduler`].
pub mod lifecycle_scheduler;
pub mod mailbox_poller;
/// Issue #971: [`MaintenanceTicker`] — the process-wide minute loop that retires
/// expired approvals, expired grants and stale fire claims for EVERY registered
/// company, not only those with a manifest `[[schedule]]`. See [`maintenance`].
pub mod maintenance;
/// Resolving `@name` in chat to a teammate, a person, a desk, or the whole
/// room — and deciding what that addresses. Pure and brain-agnostic, for the
/// same reason [`delegation_tools`] is. See [`mentions`].
pub mod mentions;
/// Issue #290: replacing a registered company's runtime in place, so first-time
/// BYOK setup takes effect without a process restart. See [`rebuild`].
pub mod rebuild;
pub mod registry;
/// Issue #383: [`RunSupervisor`] — the live set of workflow runs an operator can
/// still stop, so `POST …/workflows/runs/{runId}/cancel` has something to reach.
/// Compiled in every build: it is a plain map of stop signals and touches no
/// engine. See [`run_supervisor`].
pub mod run_events;
pub mod run_supervisor;
pub mod scheduler;
pub mod tools;
/// Issue #983: settling chat turns a previous host process left open, the
/// transcript-side twin of the run reaper. See [`turn_sweep`].
pub mod turn_sweep;
pub mod types;
/// Issue #978: which gate node each parked workflow approval is deciding, and
/// the trigger input its run paused with — the two facts a run-scoped
/// continuation needs and the journal cannot give back once an approval has
/// resolved. See [`workflow_gates`].
pub mod workflow_gates;
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
/// Issue #327: [`WorkspaceAnnouncer`] — [`BoardAnnouncer`]'s counterpart for the
/// note tree. A note created, overwritten, moved or deleted by *anything* — the
/// console, an agent tool, the publish drain, the seeder — reaches a watching
/// console without a reload. See [`workspace_events`].
pub mod workspace_events;
/// Issue #553: [`QuotaEnforcedWorkspace`] — the one place a workspace write is
/// measured. Wrapped in beside [`WorkspaceAnnouncer`] so every writer is held
/// to the company's byte limits without knowing it is. See [`workspace_quota`].
pub mod workspace_quota;

pub use advance::{SYSTEM_ATTRIBUTION, advance_settled_card, append_result};
pub use board_events::{BoardAnnouncer, CHANGE_OPENED, CHANGE_REMOVED, CHANGE_UPDATED};
pub use builder::{RuntimeBuilder, company_id_from_name};
pub use channel::{
    DeskChannel, DurableOperatorChannel, OPERATOR_CHANNEL, OPERATOR_CHANNEL_COLLISION_FALLBACK,
    OWNER_FALLBACK_REPORT_AUTHOR, OperatorChannel, WORKFLOW_REPLY_AUTHOR,
    undeliverable_channel_message,
};
pub use cron::{CivilTime, CronExpr};
pub use cycle::CycleRunner;
pub use derived_guard::DerivedGuardWorkspace;
pub use handover::RuntimeHandover;
pub use lifecycle_scheduler::LifecycleScheduler;
pub use maintenance::MaintenanceTicker;
pub use rebuild::{BootInputs, RebuildRequest, RuntimeRebuilder, rebuild_company};
pub use registry::CompanyRegistry;
pub use run_supervisor::{RunGuard, RunSupervisor};
pub use scheduler::{
    CATCHUP_WINDOW_MINUTES, Clock, CompanyScheduler, FakeClock, PRUNE_CUTOFF_MINUTES, SystemClock,
    missed_instant,
};
pub use tools::StubToolProvider;
pub use turn_sweep::{TURN_INTERRUPTED_BY_RESTART, sweep_interrupted_turns};
pub use types::{ApprovalSummary, CompanyStatus, CycleReport};
pub use workflow_outcome::{
    FailedRun, delivered_by_unsettled_runs, record_run_finished, sweep_interrupted_runs,
};
pub use workflow_resume::WORKFLOW_APPROVE_KIND;
pub use workflow_scheduler::WorkflowScheduler;
pub(crate) use workflow_scheduler::workflow_schedule_id;
pub use workflow_spawn::WorkflowSpawn;
// Issue #1865: only referenced outside this module by the orchestrator's
// `run_workflow` tool path, which is `openhuman`-only — see
// `harness::built_in::orchestrator::RunWorkflowTool::execute`. The default
// build has no other consumer of the re-export, so an ungated `pub(crate) use`
// here is flagged unused by that build's own lint pass.
#[cfg(feature = "openhuman")]
pub(crate) use workflow_spawn::{
    PANICKED_BEFORE_FINISH, RUN_FAILED_DETAIL, file_run_unhealthy_notification,
};
pub use workspace_events::WorkspaceAnnouncer;
pub use workspace_quota::{
    DEFAULT_MAX_BLOB_BYTES, QuotaEnforcedWorkspace, UPLOAD_BODY_LIMIT_BYTES, WorkspaceQuota,
};

// The assembly struct lives under `company/` to match the `ports.md` sketch
// (`src/company/runtime.rs`); re-export it here as the kernel's public surface.
pub use crate::company::runtime::CompanyRuntime;
