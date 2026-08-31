//! The [`CompanyRuntime`] assembly: one running company's wired-together ports.
//!
//! The struct matches the sketch in `docs/spec/runtime/ports.md` — the nine
//! ports, with `economy` the only optional one. Three runtime-internal fields
//! are added: the company `id`, a per-company serial lock so exactly one cycle
//! runs at a time, and the [`RuntimeJournal`] backing at-most-once effects and
//! the durable approval queue.
//!
//! The cycle logic itself lives in [`CycleRunner`](crate::runtime::CycleRunner);
//! the methods here are thin delegations so callers hold a single
//! `Arc<CompanyRuntime>`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::Result;
use crate::app::config::AuthMode;
use crate::error::OpenCompanyError;
use crate::feedback::board::{
    BoardComment, BoardDetail, BoardItem, BoardPage, BoardQuery, VoteValue,
};
use crate::feedback::service::{FeedbackFiler, FeedbackResponse};
use crate::feedback::store::FeedbackStore;
use crate::feedback::tinyhumans::TinyHumansClient;
use crate::feedback::types::{FeedbackInput, FeedbackItem, FeedbackSummary};
use crate::policy::ManifestApprovalGate;
use crate::ports::now_millis;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, EventSeq, Mention, Verdict,
};
use crate::ports::{
    AgentEconomy, ApprovalGate, ArtifactStore, Brain, ChannelAdapter, CompanyStore, ContextStore,
    EventLog, FactStore, InboxStore, LoginCodeStore, MemoryStore, NotificationStore,
    ReadStateStore, RunStore, SecretStore, SessionStore, SkillStateStore, TaskRecord, TaskStore,
    ToolProvider, UsageMeter, UserStore, WorkflowRevisionStore, WorkspaceStore,
};
// Separate line (#241) so this addition is a pure append, not a reflow of the
// grouped import that sibling store-seam branches (#274, #596) also edit.
use crate::ports::ScheduleFireStore;
// Separate line (#596) for the same reason.
use crate::ports::WorkflowRunOutputStore;
// Separate line, same reasons as above: `set_lifecycle` needs the
// per-company write lock (PR #1875 review finding, second round).
use crate::ports::store::company_write_lock;

/// The board column a task must enter to be dispatched to its assignee. Read
/// from the task port (#205) so this edge and the write boundary that validates
/// the column cannot drift onto two different literals.
use crate::ports::tasks::COLUMN_IN_PROGRESS as IN_PROGRESS;
/// The board column a task must enter to be planned (issue #337). Read from the
/// task port for the same reason the dispatch literal is.
use crate::ports::tasks::COLUMN_PLANNING as PLANNING;
/// The board column a bounced card lands in (issue #1865). Read from the task
/// port for the same reason the dispatch/planning literals above are — so the
/// clear-on-departure edge below and [`TaskRecord::bounced`]'s own doc cannot
/// drift onto two different literals for "todo".
use crate::ports::tasks::COLUMN_TODO as TODO;

/// Whether an upsert moves a card **into** `in_progress` (the dispatch edge).
/// A card already in `in_progress` re-saved is not a fresh dispatch.
fn task_enters_in_progress(prev_column: Option<&str>, next_column: &str) -> bool {
    next_column == IN_PROGRESS && prev_column != Some(IN_PROGRESS)
}

/// Whether an upsert moves a card **into** `planning` (the planning edge,
/// issue #337).
///
/// Edge-fired on the *transition*, exactly like the dispatch edge above, and
/// the shape is what gives the pass its "one per entry, no retry" property for
/// free: a card already in Planning that is re-saved — an edit, a re-title, a
/// note appended by the pass itself — is not a fresh entry, so it cannot start
/// a second pass or bill a second model call.
///
/// It is a **spend gate**, and it is the second one the board has. Before #337
/// only `in_progress` cost anything; a drag into Planning now buys one model
/// call. That is deliberate and it is opt-in — Todo → In Progress still
/// dispatches unplanned, so nobody is routed through planning who did not ask
/// to be.
fn task_enters_planning(prev_column: Option<&str>, next_column: &str) -> bool {
    next_column == PLANNING && prev_column != Some(PLANNING)
}

/// Whether an upsert moves a card **out of** `todo`, by any route (issue
/// #1865 Codex review on PR #1883).
///
/// [`TaskRecord::bounced`](crate::ports::tasks::TaskRecord::bounced)'s own doc
/// says the field is "cleared the instant the card leaves `todo` any other
/// way" — not only via the two edges above. `patch_task` accepts any board
/// column on a single write, so an operator can move a bounced To-do card
/// straight to `in_review` or `done` without ever passing through
/// `in_progress`/`planning`; `dispatch || plan` alone missed that departure,
/// so the stale chip rode along and could resurface if the card later came
/// back to `todo` — a manual transition that superseded the bounce, reporting
/// a reason that no longer applies.
fn task_leaves_todo(prev_column: Option<&str>, next_column: &str) -> bool {
    prev_column == Some(TODO) && next_column != TODO
}

/// Whether a company should come up with the emergency stop engaged, given what
/// replaying its event log produced (issue #86).
///
/// Free-standing and pure so the fail-safe direction can be pinned by a test
/// without standing up a runtime — see
/// [`CompanyRuntime::hydrate_emergency`] for the full argument. The one rule
/// worth restating here: **`Err` means stopped.** A log that cannot be read
/// tells us nothing about whether an operator pulled the switch, and of the two
/// available guesses only one is recoverable by a human noticing.
fn emergency_from_load(stopped: Result<Option<bool>>) -> bool {
    match stopped {
        Ok(Some(engaged)) => engaged,
        // Nothing known and nothing broken — a company with no such event in its
        // log was never stopped.
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Awaits a spawned follow-up cycle and flattens its two failure modes into one
/// (issue #383).
///
/// A [`JoinError`](tokio::task::JoinError) means the cycle task panicked or was
/// aborted — neither of which the cycle itself can report. Callers that want to
/// answer on the response body await through here; callers that have detached
/// simply drop the handle, which abandons only the waiting.
pub(crate) async fn join_follow_up(
    follow_up: JoinHandle<Result<CycleReport>>,
) -> Result<CycleReport> {
    match follow_up.await {
        Ok(report) => report,
        Err(err) => Err(OpenCompanyError::BackgroundTask(format!(
            "the follow-up cycle did not finish: {err}"
        ))),
    }
}
use crate::runtime::CycleRunner;
use crate::runtime::blocked_nodes::BlockedNodeQueue;
use crate::runtime::continuation::ContinuationQueue;
use crate::runtime::cycle::ResolveReceipt;
use crate::runtime::grants::{GRANT_TTL_MILLIS, GrantId, GrantScope, GrantSet, StandingGrant};
use crate::runtime::journal::{ApprovalOrigin, ExecutedEffect, ExpiryReason, RuntimeJournal};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::runtime::workflow_gates::WorkflowGateQueue;
use crate::server::ops::mailer::MailSender;
use crate::server::ops::smtp::SmtpCredentials;

/// The most parked approvals one maintenance tick retires (issue #971).
///
/// A cap, not a rate: the tick runs every minute for every company, so a
/// backlog of a few hundred drains in a handful of minutes and one of a few
/// thousand still drains the same day. What it buys is that the FIRST tick
/// after a shortened deadline ships — the one that meets an entire accumulated
/// queue at once — does not turn into one unbounded burst of journal appends,
/// event appends and released agent turns on the minute boundary every other
/// company in the process shares.
///
/// Deliberately generous rather than tuned. The failure this guards is a
/// stampede, and 50 retirements is nowhere near one; a number small enough to
/// need tuning would instead be a queue that visibly lags behind its own
/// deadline, which is the symptom issue #971 is about.
const MAX_RETIREMENTS_PER_TICK: usize = 50;

/// The WS3 console ports, bundled so the runtime constructor stays legible.
/// Each is an `Arc<dyn …>` keyed by [`CompanyId`], defaulting to the fs backend
/// and overridden together when a non-fs backend is selected.
#[derive(Clone)]
pub struct OpsStores {
    /// The durable task board.
    pub tasks: Arc<dyn TaskStore>,
    /// The company's declared ledgers and their append-only event logs.
    pub ledgers: Arc<dyn crate::ports::ledgers::LedgerStore>,
    /// The durable workspace file tree.
    pub workspace: Arc<dyn WorkspaceStore>,
    /// The durable memory-facts view.
    pub facts: Arc<dyn FactStore>,
    /// Versioned task artifacts and their human-edit history (#187).
    pub artifacts: Arc<dyn ArtifactStore>,
    /// First-class records of each task attempt: status, trace, cost (#242).
    pub runs: Arc<dyn RunStore>,
    /// The unredacted companion of a run's steps — reasoning text and raw tool
    /// I/O, kept beside the scrubbed skeleton in [`Self::runs`].
    pub deep_trace: Arc<dyn crate::ports::deep_trace::DeepTraceStore>,
    /// Per-workflow edit history, for rollback of an edited workflow (#274).
    pub workflow_revisions: Arc<dyn WorkflowRevisionStore>,
    /// Durable cross-replica scheduler fire claims (#241).
    pub schedule_fires: Arc<dyn ScheduleFireStore>,
    /// Durable, console-facing per-node run output snapshots (#596).
    pub workflow_run_outputs: Arc<dyn WorkflowRunOutputStore>,
    /// The usage meter (written by the WS4 cost hook, read by WS5).
    pub usage: Arc<dyn UsageMeter>,
    /// Operator deltas over the company's skills.
    pub skills: Arc<dyn SkillStateStore>,
    /// Per-person, per-channel read markers (#755).
    pub read_state: Arc<dyn ReadStateStore>,
    /// Durable notifications with per-person read state (#749).
    pub notifications: Arc<dyn NotificationStore>,
    /// The company's human collaborators and their outstanding invites.
    pub users: Arc<dyn UserStore>,
    /// Live browser sessions for those users.
    pub sessions: Arc<dyn SessionStore>,
    /// Pending magic-link login codes.
    pub login_codes: Arc<dyn LoginCodeStore>,
}

/// The company's own outbound-mail handle: a sender + its SMTP credentials
/// (the manager-injected per-tenant mailbox). `None` when email isn't wired.
#[derive(Clone)]
pub struct CompanyMail {
    pub sender: Arc<dyn MailSender>,
    pub smtp: SmtpCredentials,
}

/// A running company: its brain, stores, channels, and policy gate, wired
/// together behind a serial cycle loop.
pub struct CompanyRuntime {
    /// Whether this runtime has already said that it cannot dispatch
    /// (issue #1059). Latched so a board with many cards says it once.
    pub(crate) inert_board_reported: std::sync::atomic::AtomicBool,
    pub(crate) id: CompanyId,
    pub(crate) brain: Arc<dyn Brain>,
    pub(crate) store: Arc<dyn CompanyStore>,
    pub(crate) events: Arc<dyn EventLog>,
    pub(crate) memory: Arc<dyn MemoryStore>,
    pub(crate) context: Arc<dyn ContextStore>,
    /// The taint-stamping context port for external content (issue #1113);
    /// resolved at build time — same store as `context` when the engine
    /// cannot represent taint.
    pub(crate) inbound_context: Arc<dyn ContextStore>,
    /// Isolated provisional working context from a provider-backed overlay.
    pub(crate) scratch_context: Option<Arc<dyn ContextStore>>,
    /// Safe agent/desk partitions and archive reads from that overlay.
    pub(crate) memory_scopes: Option<Arc<dyn crate::store::MemoryScopes>>,
    pub(crate) tools: Arc<dyn ToolProvider>,
    pub(crate) channels: Vec<Arc<dyn ChannelAdapter>>,
    pub(crate) economy: Option<Arc<dyn AgentEconomy>>,
    pub(crate) approvals: Arc<dyn ApprovalGate>,
    /// The concrete gate, kept alongside the `dyn` port so the runtime can reach
    /// the amend and expiry-sweep methods that live outside the trait without a
    /// downcast.
    pub(crate) approval_gate: Arc<ManifestApprovalGate>,
    /// Whether `approval_gate` came from [`RuntimeBuilder::with_approvals`]
    /// (crate::runtime::RuntimeBuilder::with_approvals) — a test seam that
    /// carries its own policy/TTL on purpose — rather than from the manifest and
    /// the persisted record. Issue #1455 refreshes the live gate from the
    /// record's effective policy at safe turn boundaries; an injected gate must
    /// be exempt, or the refresh would clobber the fixture (e.g. a zero-TTL gate
    /// for expiry tests).
    pub(crate) gate_injected: bool,
    pub(crate) journal: Arc<RuntimeJournal>,
    /// Where this company's turns are reported, if anywhere (issue #1739).
    ///
    /// Always present and always compiled — the port and its no-op default live
    /// in the default build, exactly as `steer` and `grants` do. A desktop or
    /// self-hosted instance holds a
    /// [`NullTracker`](crate::analytics::NullTracker) here and nothing it does
    /// leaves the process; only a hosted tenant that resolved to
    /// [`Decision::Report`](crate::analytics::Decision::Report) holds anything
    /// else, and only a build compiled with `--features analytics` has anything
    /// else to hold.
    pub(crate) tracker: Arc<dyn crate::analytics::Tracker>,
    /// Per-company secrets, read by the feedback scrubber (and webhook HMAC
    /// verification, later).
    pub(crate) secrets: Arc<dyn SecretStore>,
    /// Install-wide default MCP servers (issue #527), already normalized by
    /// `company::mcp::normalize_default_servers` at the config boundary. Lives
    /// beside `secrets` because every reader needs both: the pair is what
    /// `company::mcp::resolve_effective` takes. Empty for an install that
    /// configures no defaults, which is the common case.
    pub(crate) default_mcp_servers: Vec<crate::company::McpServer>,
    /// Per-teammate email (inbound + outbound), backing the inbox surface.
    pub(crate) inbox: Arc<dyn InboxStore>,
    /// The company's own outbound-mail handle (sender + SMTP credentials),
    /// wired via [`RuntimeBuilder::with_mail`](crate::runtime::RuntimeBuilder::with_mail).
    /// `None` when email send isn't wired.
    pub(crate) mail: Option<CompanyMail>,
    /// The WS3 console ports (tasks, workspace, facts, usage, skills).
    pub(crate) ops: OpsStores,
    /// Durable store of feedback items (the "feedback family").
    pub(crate) feedback: Arc<FeedbackStore>,
    /// Filing configuration: the GitHub client, target repo, consent, limiter.
    pub(crate) filer: Arc<FeedbackFiler>,
    /// The company's on-disk source definition directory (`companies/<name>`),
    /// set on the `serve`/CLI path so read resolvers can find the committed
    /// `skills/` and `workflows/` content. `None` in platform-provisioned mode
    /// (no source dir), where those resolvers degrade to manifest-derived/empty.
    pub(crate) source_dir: Option<PathBuf>,
    /// How humans sign in to this company, resolved once at build from the
    /// host-wide override and the manifest's `[users].mode`. Cached because it
    /// is read on the request path — see [`Self::auth_mode`].
    pub(crate) auth_mode: AuthMode,
    /// Issue #29: the workflow runner, when wired. Executes a company's workflow
    /// graphs on the embedded `tinyflows` engine (agent nodes on the harness
    /// pool). The port trait is default-compiled, so this field is always
    /// present; only the concrete `HarnessWorkflowRunner` is `openhuman`-gated,
    /// so the default build simply leaves it `None` and the run route reports
    /// "not wired".
    pub(crate) workflow_runner: Option<Arc<dyn crate::ports::WorkflowRunner>>,
    /// Issue #111: the registry of in-flight, steerable runs. The operator steer
    /// routes (`GET …/tasks/inflight`, `POST …/tasks/{key}/steer`) read and write
    /// it; the harness brain registers a dispatched task / desk delegation here
    /// before running it. Always present (the type is openhuman-free) — the
    /// default build simply never registers anything, so the strip is empty and
    /// every steer is `not in flight`. On the harness path the
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) wires in the same handle
    /// the harness deps hold via [`set_steer`](Self::set_steer).
    pub(crate) steer: crate::company::steer::InflightRegistry,
    /// Issue #383: the live set of workflow runs an operator can still stop.
    ///
    /// Always present and always compiled, like [`steer`](Self::steer) — a
    /// [`RunSupervisor`](crate::runtime::RunSupervisor) is a map of stop signals
    /// and touches no engine. The default build wires no runner, so nothing ever
    /// registers and every cancel is a clean `404`.
    ///
    /// Every entry point that can start a run mints its context here rather than
    /// through [`WorkflowRunContext::new`](crate::ports::WorkflowRunContext::new),
    /// which is what makes a wedged cron fire and an agent-initiated run as
    /// stoppable from the console as a Run-button one.
    pub(crate) run_supervisor: crate::runtime::RunSupervisor,
    /// Issue #243: the live single-use grants minted when an operator approves a
    /// tool call an agent was blocked from making.
    ///
    /// Always present, like [`steer`](Self::steer) — [`GrantSet`] is
    /// openhuman-free and the journal records replay in every build, so a
    /// company that ran under the harness stays replayable by one without it. On
    /// the harness path the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder)
    /// hands the SAME set to the agents' `ApprovalRequestQueue`, which is what
    /// lets a grant minted here be redeemed there. On the default build nothing
    /// ever mints, so the set stays empty and every approval keeps its
    /// pre-#243 native-execute behaviour.
    pub(crate) grants: GrantSet,
    /// Issue #469: how many of each turn's parked approvals are still
    /// undecided, so a turn that raised several sign-offs is continued **once**
    /// — after the last of them lands — instead of once per decision.
    ///
    /// Live per-instance state, like [`grants`](Self::grants), and inherited by
    /// a rebuilt runtime through [`RuntimeHandover`](crate::runtime::handover::RuntimeHandover)
    /// for the same reason: a swap in the middle of a partly-decided turn must
    /// not forget that the turn is blocked, or the next decision continues it as
    /// though the others had never been owed.
    pub(crate) continuations: ContinuationQueue,
    /// Issue #978: which gate node each parked **workflow** approval is
    /// deciding, and the trigger input its run paused with.
    ///
    /// The run-scoped companion to [`continuations`](Self::continuations): that
    /// queue counts a run's outstanding decisions, and this one holds the facts
    /// the release needs to actually re-dispatch it. Live per-instance state and
    /// inherited across a rebuild for the same reason both its neighbours are —
    /// a swap mid-decision that forgot a run's parked gates would re-ask about
    /// every one of them.
    pub(crate) workflow_gates: WorkflowGateQueue,
    /// Issue #899 (Stage 1): the workflow id and trigger input each **blocked
    /// agent node** needs to re-dispatch its run when the operator approves the
    /// gated call parked inside its tool loop.
    ///
    /// The agent-node companion to [`workflow_gates`](Self::workflow_gates): both
    /// hold the facts a released [`continuations`](Self::continuations) batch
    /// cannot carry, but for the two structurally different ways a run blocks —
    /// a `requires_approval` gate node (there) versus a policy-gated call inside
    /// an agent node's own tool loop (here). Live per-instance state; unlike its
    /// neighbours it is **not** rebuilt from the journal on a swap, because the
    /// parked tool-call effect carries no workflow lineage to rebuild it from —
    /// see [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue).
    pub(crate) blocked_nodes: BlockedNodeQueue,
    /// Held for the duration of a cycle so cycles never interleave per company.
    ///
    /// `Arc`-shared rather than owned so a rebuilt runtime can inherit the *same*
    /// lock (issue #290). Two runtimes for one company each holding their own
    /// mutex would mean two cycles running at once against a store whose `save`
    /// writes the whole record, which is exactly the invariant this exists to
    /// hold. Handing the lock over is also what makes
    /// [`quiesce`](Self::quiesce)'s drain meaningful across the swap.
    pub(crate) serial: Arc<TokioMutex<()>>,
    /// One lock slot per addressed agent, so two operators talking to two
    /// different agents in the same company do not serialize behind each other.
    ///
    /// [`serial`](Self::serial) is held for a whole cycle — a live agent turn —
    /// so with only that lock, three messages to three agents in one company run
    /// strictly one after another even though nothing they touch is shared: each
    /// agent has its own conversation history in the harness pool, and the state
    /// they *do* share (the task board, the event-log `seq`) already has its own
    /// finer lock. This map hands each addressed agent its own slot so their
    /// turns overlap while a whole-company cycle still serializes against all of
    /// them.
    ///
    /// A cycle with no single addressee — a scheduler tick, an unaddressed
    /// message routed to the orchestrator, or a batch naming more than one agent
    /// — falls back to [`serial`](Self::serial) and so still serializes against
    /// everything. That is deliberate: such a cycle may touch the whole company.
    ///
    /// `Arc`-shared for the same reason as `serial`: a rebuilt runtime must
    /// inherit the *same* per-agent slots (issue #290), or an agent mid-turn
    /// could start a second turn beside itself across the swap.
    pub(crate) per_agent: Arc<TokioMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
    /// Held across a REST board write's read → validate → write, so two
    /// concurrent edits cannot each validate against a snapshot that predates
    /// the other's edge (issue #185 review).
    ///
    /// Deliberately **not** [`serial`](Self::serial): that lock is held for a
    /// whole cycle, which is a live agent turn, so reusing it would park every
    /// board edit behind an LLM call. This one is only ever held across a
    /// couple of store round-trips.
    ///
    /// `Arc`-shared for the same reason as [`serial`](Self::serial): a rebuild
    /// inherits it rather than minting a second one.
    pub(crate) task_writes: Arc<TokioMutex<()>>,
    /// Set while this runtime is being replaced (issue #290). Once set, every
    /// cycle entry point refuses with [`OpenCompanyError::Quiescing`] so the
    /// in-flight turn can drain and the successor takes over at a point with no
    /// live cycle. Never cleared by the runtime itself: either the successor
    /// replaces it in the registry, or the rebuild failed and
    /// [`resume`](Self::resume) puts this one back to work.
    pub(crate) quiesced: Arc<AtomicBool>,
    /// Set by a cold build when replay found explicit decision continuations;
    /// consumed once when the runtime enters the production registry.
    replay_continuations_on_register: AtomicBool,
    /// WS4: the embedded openhuman harness pool, when wired via
    /// [`RuntimeBuilder::with_harness`](crate::runtime::RuntimeBuilder::with_harness).
    /// Feature-gated so the default build is unaffected.
    #[cfg(feature = "openhuman")]
    pub(crate) harness: Option<Arc<crate::harness::HarnessPool>>,
    /// Issue #337: the company's planning station, when wired. Mirrors
    /// [`harness`](Self::harness) — same feature gate, same
    /// `None`-means-inert contract, wired by the
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) from the same
    /// `Arc<dyn HarnessModel>` the roster runs on.
    ///
    /// `None` (the default build, or any runtime built without a harness)
    /// leaves the planning edge inert: a card dragged into Planning simply
    /// rests there, exactly as it did before #337, and the boot sweep returns
    /// it at the next start.
    #[cfg(feature = "openhuman")]
    pub(crate) planner: Option<Arc<crate::harness::planning::TaskPlanner>>,
    /// Issue #580: the company's workflow builder, attached the same way as the
    /// planner. `None` leaves the `workflow`-deliverable dispatch branch inert —
    /// a card entering In Progress dispatches as a one-off exactly as before #580,
    /// and the boot reaper settles any run left mid-build.
    #[cfg(feature = "openhuman")]
    pub(crate) builder: Option<Arc<crate::harness::workflow_build::WorkflowBuilder>>,
    #[cfg(feature = "openhuman")]
    pub(crate) workflow_harness_deps: Option<crate::harness::HarnessDeps>,
    /// The company's first-run setup polish pass, attached the same way as the
    /// planner and the workflow builder. `None` is not a degraded state here:
    /// the setup route then returns the curated template unpolished, which is a
    /// real roster — see `docs/spec/runtime/company-setup.md`.
    #[cfg(feature = "openhuman")]
    pub(crate) roster_builder: Option<Arc<crate::harness::roster_build::RosterBuilder>>,
    /// MCP installs and live connections for this runtime. The wrapper owns a
    /// company-home-scoped OpenHuman config while the live registry remains
    /// shared in-process with harness agents.
    #[cfg(feature = "mcp")]
    pub(crate) mcp: Option<Arc<crate::harness::mcp::McpRuntime>>,
}

/// The event the runtime appends when a continuation could not be picked back
/// up (issue #469, defect 4).
///
/// Named so its **author** can be asserted (issue #966). This site writes the
/// `AgentReply` directly rather than going through `OutboundMessage`, so it does
/// not get the `agent` field's fallback and has to name the author itself. It
/// used to store `OPERATOR_CHANNEL`, which made a correct system row
/// indistinguishable on disk from a reply whose author the pre-#885 defect
/// overwrote.
fn continuation_failure_notice(thread: String, parent: Option<EventSeq>) -> CompanyEvent {
    CompanyEvent::AgentReply {
        parent,
        chat_id: thread,
        agent_id: crate::ports::SYSTEM_AUTHOR.to_string(),
        text: "Your approval was recorded, but the agent could not pick the work back up. \
               Nothing was half-done — approving again is safe and will retry it."
            .to_string(),
        steps: Vec::new(),
        task_id: None,
        // A runtime notice addressed to whoever is reading it. It names no
        // teammate and no person, so there is nothing to chip and nobody to
        // ping.
        mentions: Vec::new(),
        mention_depth: 0,
    }
}

impl CompanyRuntime {
    /// Assembles a runtime from its ports. Most callers use
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) instead.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: CompanyId,
        brain: Arc<dyn Brain>,
        store: Arc<dyn CompanyStore>,
        events: Arc<dyn EventLog>,
        memory: Arc<dyn MemoryStore>,
        context: Arc<dyn ContextStore>,
        inbound_context: Arc<dyn ContextStore>,
        tools: Arc<dyn ToolProvider>,
        channels: Vec<Arc<dyn ChannelAdapter>>,
        economy: Option<Arc<dyn AgentEconomy>>,
        approval_gate: Arc<ManifestApprovalGate>,
        journal: Arc<RuntimeJournal>,
        secrets: Arc<dyn SecretStore>,
        inbox: Arc<dyn InboxStore>,
        mail: Option<CompanyMail>,
        ops: OpsStores,
        feedback: Arc<FeedbackStore>,
        filer: Arc<FeedbackFiler>,
        grants: GrantSet,
    ) -> Self {
        let approvals: Arc<dyn ApprovalGate> = approval_gate.clone();
        Self {
            inert_board_reported: std::sync::atomic::AtomicBool::new(false),
            // Install-wide, not per-company, so it is set by the builder from
            // resolved config (`set_default_mcp_servers`) rather than taken as a
            // 19th positional argument here.
            default_mcp_servers: Vec::new(),
            id,
            brain,
            store,
            events,
            memory,
            context,
            inbound_context,
            scratch_context: None,
            memory_scopes: None,
            tools,
            channels,
            economy,
            approvals,
            approval_gate,
            gate_injected: false,
            journal,
            tracker: crate::analytics::null_tracker(),
            secrets,
            inbox,
            mail,
            ops,
            feedback,
            filer,
            source_dir: None,
            auth_mode: AuthMode::default(),
            workflow_runner: None,
            steer: crate::company::steer::InflightRegistry::new(),
            run_supervisor: crate::runtime::RunSupervisor::new(),
            grants,
            continuations: ContinuationQueue::default(),
            workflow_gates: WorkflowGateQueue::default(),
            blocked_nodes: BlockedNodeQueue::default(),
            serial: Arc::new(TokioMutex::new(())),
            per_agent: Arc::new(TokioMutex::new(HashMap::new())),
            task_writes: Arc::new(TokioMutex::new(())),
            quiesced: Arc::new(AtomicBool::new(false)),
            replay_continuations_on_register: AtomicBool::new(false),
            #[cfg(feature = "openhuman")]
            harness: None,
            #[cfg(feature = "openhuman")]
            planner: None,
            #[cfg(feature = "openhuman")]
            builder: None,
            #[cfg(feature = "openhuman")]
            workflow_harness_deps: None,
            #[cfg(feature = "openhuman")]
            roster_builder: None,
            #[cfg(feature = "mcp")]
            mcp: None,
        }
    }

    /// Records the company's on-disk source directory (`companies/<name>`), set
    /// by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) on the serve
    /// path so read resolvers can resolve committed skills/workflows content.
    pub fn set_source_dir(&mut self, dir: Option<PathBuf>) {
        self.source_dir = dir;
    }

    /// Installs the provider-backed memory decorators selected at boot.
    ///
    /// These are optional because the base store and the legacy embedded engine
    /// do not have the provider contract's isolated partitions or archive tier.
    pub(crate) fn set_memory_decorators(
        &mut self,
        scratch_context: Option<Arc<dyn ContextStore>>,
        memory_scopes: Option<Arc<dyn crate::store::MemoryScopes>>,
    ) {
        self.scratch_context = scratch_context;
        self.memory_scopes = memory_scopes;
    }

    /// The isolated working-memory partition, when the selected engine serves
    /// the provider-backed decorator contract.
    pub fn scratch_context(&self) -> Option<Arc<dyn ContextStore>> {
        self.scratch_context.clone()
    }

    /// One agent's private context partition, without exposing namespaces.
    pub fn agent_context(&self, agent_id: &str) -> Option<Arc<dyn ContextStore>> {
        self.memory_scopes
            .as_ref()
            .map(|scopes| scopes.agent_context(agent_id))
    }

    /// One desk's shared context partition, without exposing namespaces.
    pub fn desk_context(&self, desk_id: &str) -> Option<Arc<dyn ContextStore>> {
        self.memory_scopes
            .as_ref()
            .map(|scopes| scopes.desk_context(desk_id))
    }

    /// Traces preserved by the provider decorator's archive-on-evict policy.
    pub async fn archived_traces(&self) -> Result<Option<Vec<crate::ports::CompressedTrace>>> {
        match &self.memory_scopes {
            Some(scopes) => scopes.archived_traces(&self.id).await.map(Some),
            None => Ok(None),
        }
    }

    /// The company's on-disk source directory, when built on the serve path.
    /// `None` in platform-provisioned mode.
    pub fn source_dir(&self) -> Option<&Path> {
        self.source_dir.as_deref()
    }

    /// Records how humans sign in to this company, resolved once by the
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) from the host override
    /// and the manifest's `[users].mode`.
    pub(crate) fn set_auth_mode(&mut self, mode: AuthMode) {
        self.auth_mode = mode;
    }

    /// Points this company's turn reporting at `tracker` (issue #1739). Wired
    /// once by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) from the
    /// process-wide decision; the default is a
    /// [`NullTracker`](crate::analytics::NullTracker).
    pub(crate) fn set_tracker(&mut self, tracker: Arc<dyn crate::analytics::Tracker>) {
        self.tracker = tracker;
    }

    /// How humans sign in to this company.
    ///
    /// Read on the request path — by the login routes, by the user-administration
    /// routes, and by principal resolution — so it is a cached field rather than
    /// a manifest read. It cannot change without a rebuild, which is what makes
    /// caching it honest.
    pub fn auth_mode(&self) -> AuthMode {
        self.auth_mode
    }

    /// Issue #29: attach the workflow runner after construction. Wired by the
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) under the `openhuman`
    /// feature; without it the run route reports "not wired".
    pub fn set_workflow_runner(&mut self, runner: Arc<dyn crate::ports::WorkflowRunner>) {
        self.workflow_runner = Some(runner);
    }

    /// The workflow runner, if one is wired. `None` in the default build (and on
    /// any runtime built without a harness), where workflow execution is inert.
    pub fn workflow_runner(&self) -> Option<&Arc<dyn crate::ports::WorkflowRunner>> {
        self.workflow_runner.as_ref()
    }

    /// WS4: attach an embedded harness pool after construction (called by the
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder)).
    #[cfg(feature = "openhuman")]
    pub fn set_harness(&mut self, harness: Arc<crate::harness::HarnessPool>) {
        self.harness = Some(harness);
    }

    /// WS4: the embedded harness pool, if one is wired. The chat layer (WS3)
    /// routes desk turns through this when present.
    #[cfg(feature = "openhuman")]
    pub fn harness(&self) -> Option<&Arc<crate::harness::HarnessPool>> {
        self.harness.as_ref()
    }

    /// Issue #337: attach the company's planning station after construction
    /// (called by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder)).
    #[cfg(feature = "openhuman")]
    pub fn set_planner(&mut self, planner: Arc<crate::harness::planning::TaskPlanner>) {
        self.planner = Some(planner);
    }

    /// The company's planning station, if one is wired. `None` leaves the
    /// planning edge inert — the card rests in Planning and the boot sweep
    /// returns it.
    #[cfg(feature = "openhuman")]
    pub fn planner(&self) -> Option<&Arc<crate::harness::planning::TaskPlanner>> {
        self.planner.as_ref()
    }

    /// Issue #580: attach the company's workflow builder after construction
    /// (called by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder)),
    /// mirroring [`set_planner`](Self::set_planner).
    #[cfg(feature = "openhuman")]
    pub fn set_builder(&mut self, builder: Arc<crate::harness::workflow_build::WorkflowBuilder>) {
        self.builder = Some(builder);
    }

    /// The company's workflow builder, if one is wired. `None` leaves the
    /// `workflow`-deliverable dispatch branch inert — the card dispatches as a
    /// one-off.
    #[cfg(feature = "openhuman")]
    pub fn builder(&self) -> Option<&Arc<crate::harness::workflow_build::WorkflowBuilder>> {
        self.builder.as_ref()
    }

    /// This deployment's workflow-tool wiring for `company`: the namespaces a
    /// `tool_call` can actually reach here, **and** why each of the others
    /// cannot — the same
    /// [`WorkflowToolWiring`](crate::workflows::caps::WorkflowToolWiring) the
    /// run-time gate reads, so what a caller is told is available and what
    /// `refusal_for` says at run time come from one computation.
    ///
    /// `None` means the wiring is not knowable — no harness deps are attached,
    /// so there is no deployment to ask. Callers must treat that as "cannot
    /// say" and fall back to the grant-only answer rather than reporting
    /// everything as unwired.
    ///
    /// The capability filter is resolved per call because a budget plan makes it
    /// a function of *current* spend (issue #661): a tier that is open now can
    /// be filtered an hour later, and a cached set would advertise a namespace
    /// the run would refuse.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn workflow_tool_wiring(
        &self,
        company: &crate::ports::CompanyRecord,
    ) -> Option<crate::workflows::caps::WorkflowToolWiring> {
        let deps = self.workflow_harness_deps.as_ref()?;
        let mut resolved = deps.clone();
        if let Some(plan) = &resolved.plan {
            resolved.capabilities = crate::harness::capability_budget::resolve_filter(
                plan,
                resolved.meter.as_deref(),
                &company.id,
                crate::ports::now_millis(),
            )
            .await;
        }
        Some(crate::workflows::caps::workflow_tool_wiring(&resolved))
    }

    #[cfg(feature = "openhuman")]
    pub async fn wired_workflow_namespaces(
        &self,
        company: &crate::ports::CompanyRecord,
    ) -> Option<std::collections::BTreeSet<&'static str>> {
        Some(self.workflow_tool_wiring(company).await?.wired_namespaces)
    }

    #[cfg(feature = "openhuman")]
    pub fn set_workflow_harness_deps(&mut self, deps: crate::harness::HarnessDeps) {
        self.workflow_harness_deps = Some(deps);
    }

    /// Attaches the company's first-run setup pass after construction, mirroring
    /// [`set_builder`](Self::set_builder).
    #[cfg(feature = "openhuman")]
    pub fn set_roster_builder(
        &mut self,
        roster_builder: Arc<crate::harness::roster_build::RosterBuilder>,
    ) {
        self.roster_builder = Some(roster_builder);
    }

    /// The company's first-run setup pass, if one is wired. `None` means setup
    /// answers a proposal from the curated template alone — a supported path,
    /// not a broken one.
    #[cfg(feature = "openhuman")]
    pub fn roster_builder(&self) -> Option<&Arc<crate::harness::roster_build::RosterBuilder>> {
        self.roster_builder.as_ref()
    }

    /// The pass that drafts one teammate's mandate or persona (issue #1776).
    ///
    /// Built on demand from the same harness deps the workflow builder holds —
    /// the same provider and model override — so a console BYOK switch reaches
    /// drafting with no second credential path and no second wiring site. It is
    /// two `Arc` clones and carries no state between calls, so there is nothing
    /// to attach at boot and nothing to rebuild.
    ///
    /// `None` means this company has no harness path, which is a supported
    /// configuration: the route answers `no_model` and the console says so,
    /// rather than offering a control that can only fail.
    #[cfg(feature = "openhuman")]
    pub(crate) fn profile_drafter(&self) -> Option<crate::harness::profile_draft::ProfileDrafter> {
        Some(crate::harness::profile_draft::ProfileDrafter::from_deps(
            self.workflow_harness_deps.as_ref()?,
        ))
    }

    /// Attaches the embedded MCP runtime used by REST and harness agents.
    #[cfg(feature = "mcp")]
    pub fn set_mcp(&mut self, mcp: Arc<crate::harness::mcp::McpRuntime>) {
        self.mcp = Some(mcp);
    }

    /// Returns this company's embedded MCP runtime when the feature is enabled.
    #[cfg(feature = "mcp")]
    pub fn mcp(&self) -> Option<&Arc<crate::harness::mcp::McpRuntime>> {
        self.mcp.as_ref()
    }

    /// Issue #111: replaces this runtime's in-flight steer registry with a shared
    /// handle (wired by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) to
    /// the one the harness deps hold, so the operator routes and the brain see the
    /// same runs).
    pub fn set_steer(&mut self, steer: crate::company::steer::InflightRegistry) {
        self.steer = steer;
    }

    /// This company's in-flight steer registry — the operator control plane for
    /// pausing / cancelling / redirecting live runs.
    pub fn steer(&self) -> &crate::company::steer::InflightRegistry {
        &self.steer
    }

    /// Issue #383: replaces this runtime's run supervisor with a shared handle
    /// (wired by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) to the one
    /// the harness deps hold, so the orchestrator's `run_workflow` tool registers
    /// into the map the cancel route reads).
    pub fn set_run_supervisor(&mut self, supervisor: crate::runtime::RunSupervisor) {
        self.run_supervisor = supervisor;
    }

    /// This company's live set of cancellable workflow runs (issue #383).
    /// Whether this company is doing anything the platform must not interrupt.
    ///
    /// Three sources, because no one of them sees all the work — the first
    /// version of this shipped only the third and missed the case
    /// opencompany-microservice#22 actually measured.
    ///
    /// - **[`serial`](Self::serial)**, the per-company cycle lock. This is the
    ///   broad one: a top-level operator chat turn takes it and registers
    ///   nothing else, and `chat_and_emit` detaches that turn onto its own task
    ///   precisely because it outlives reverse-proxy timeouts. Webhook, telegram
    ///   and mailbox-poller cycles take it too. A `tokio::Mutex`, so `try_lock`
    ///   is free and never blocks the caller.
    /// - **[`run_supervisor`](Self::run_supervisor)**, covering workflow runs —
    ///   the manual run route, the cron scheduler, approved-gate continuations
    ///   and the orchestrator's `run_workflow` tool. It is a separate registry
    ///   and the other two never see it.
    /// - **[`steer`](Self::steer)**, the in-flight registry, for dispatched board
    ///   cards and desk delegations, which run *inside* a cycle and so would
    ///   otherwise be covered — it is kept for the case where a turn's steerable
    ///   run outlives the cycle that started it.
    ///
    /// Every arm fails closed: the two registries report **busy** on a poisoned
    /// mutex rather than panicking, because a panic here reaches an axum handler
    /// with no `CatchPanicLayer` and the manager's reading of a reset connection
    /// is to park (issues #1133, #1239).
    ///
    /// Cheap by construction: a non-blocking `try_lock` and at most two
    /// `std::sync::Mutex` acquisitions — one for the run supervisor's map (the
    /// emptiness check *is* a lock) and one for the steer registry. `||`
    /// short-circuits, so a company already holding its cycle lock takes
    /// neither. The platform calls this once per idle tenant per reconcile scan
    /// against a short timeout, so anything that could block would turn a slow
    /// company into a stalled sweep.
    pub fn is_busy(&self) -> bool {
        self.serial.try_lock().is_err()
            || !self.run_supervisor.is_empty()
            || self.steer.any_inflight()
    }

    pub fn run_supervisor(&self) -> &crate::runtime::RunSupervisor {
        &self.run_supervisor
    }

    /// This company's id.
    pub fn id(&self) -> &CompanyId {
        &self.id
    }

    /// This company's secret store (SMTP creds, OAuth tokens, domain config).
    /// Sets the install-wide default MCP servers (issue #527). Called by
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) from resolved config.
    pub fn set_default_mcp_servers(&mut self, servers: Vec<crate::company::McpServer>) {
        self.default_mcp_servers = servers;
    }

    /// The install-wide default MCP servers (issue #527), for passing to
    /// [`company::mcp::resolve_effective`](crate::company::mcp::resolve_effective).
    pub fn default_mcp_servers(&self) -> &[crate::company::McpServer] {
        &self.default_mcp_servers
    }

    pub fn secrets(&self) -> &Arc<dyn SecretStore> {
        &self.secrets
    }

    /// This company's event log (append-only audit trail).
    pub fn events(&self) -> &Arc<dyn EventLog> {
        &self.events
    }

    /// This company's runtime journal — the at-most-once effect log and the
    /// durable approval queue.
    ///
    /// Exposed so a rebuild can prove it handed the *same* journal to the
    /// successor: a second `RuntimeJournal` over one path is the corruption
    /// hazard [`RuntimeHandover`](crate::runtime::RuntimeHandover) exists to
    /// prevent, and "we passed it along" is only checkable if it is readable.
    /// The run-scoped workflow gate batches this runtime is holding (issue
    /// #978). Delegated to rather than exposed as a field so the approve path in
    /// `workflow_resume` can fork on whether a card's run is armed.
    pub fn workflow_gates(&self) -> &WorkflowGateQueue {
        &self.workflow_gates
    }

    pub fn journal(&self) -> &Arc<RuntimeJournal> {
        &self.journal
    }

    /// This company's durable record store.
    pub fn store(&self) -> &Arc<dyn CompanyStore> {
        &self.store
    }

    /// The ids of this running company's channels a workflow may actually
    /// deliver to — exactly what an `output` node's `channel` destination may
    /// target (issues #813, #981, #1757). Desk channels (one per `[[group_chat]]`
    /// and per operator-created desk), enabled OpenHuman-provider manifest
    /// channels, **and** the always-present `operator` channel — which is now a
    /// durable, journal-backed surface (issue #1757), so it is a real target the
    /// console offers like any other.
    ///
    /// The console reads this to offer a picker of real targets, and the
    /// workflow write routes reject a channel destination outside it, instead
    /// of a free-text box that only fails at delivery time with
    /// `ChannelNotWired`.
    ///
    /// The set is empty only when a company somehow wires no channels at all —
    /// normally it holds at least `operator`, which every company has.
    ///
    /// This was `wired_channel_ids`; the rename survives because every call site
    /// is still worth re-reading against the delivery rule — but the rule no
    /// longer excludes `operator`, whose report now lands durably.
    ///
    /// Deduplicated, first-occurrence order preserved (issue #1781 review,
    /// Codex P2 follow-up). A grandfathered manifest desk at the literal id
    /// `operator` predates the "operator is reserved" manifest validation
    /// (`company/manifest.rs`, checked only at upload/create time, never at
    /// boot) and still wires **both** the built-in `OperatorChannel` and a
    /// `DeskChannel("operator")` into `self.channels` — the desk-wiring loop in
    /// `RuntimeBuilder::build` dedupes desk ids against each other but has no
    /// way to know the built-in channel already claimed the same id. Left
    /// unfiltered, `operator` would surface twice in `/workflows/wired-channels`
    /// and `WorkflowCreateDialog` would render two `SelectItem`s with the same
    /// key and value.
    pub fn deliverable_channel_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.channels
            .iter()
            .map(|channel| channel.channel_id().to_string())
            .filter(|id| seen.insert(id.clone()))
            .collect()
    }

    /// The workflow ids declared in this company's manifest
    /// (`[workflows].enabled`), read from the persisted record. Empty when the
    /// record hasn't been saved yet.
    ///
    /// This is the source of truth for *which* workflows exist on a
    /// platform-provisioned tenant (no `source_dir`, so nothing to scan on
    /// disk) — see [`Self::source_dir`]. Both the REST `list_workflows` route
    /// and the GraphQL `Company.workflows` resolver read it so the two
    /// surfaces agree on what the company has enabled.
    pub async fn enabled_workflow_ids(&self) -> Result<Vec<String>> {
        let record = self.store.load(&self.id).await?;
        Ok(record
            .map(|record| record.manifest.workflows.enabled)
            .unwrap_or_default())
    }

    /// This company's inbox store (inbound + outbound email).
    pub fn inbox(&self) -> &Arc<dyn InboxStore> {
        &self.inbox
    }

    /// This company's outbound-mail handle (sender + SMTP credentials), when
    /// wired. `None` when email send isn't configured.
    pub fn mail(&self) -> Option<&CompanyMail> {
        self.mail.as_ref()
    }

    /// This company's task board.
    pub fn tasks(&self) -> &Arc<dyn TaskStore> {
        &self.ops.tasks
    }

    /// The company's ledger store.
    pub fn ledgers(&self) -> &Arc<dyn crate::ports::ledgers::LedgerStore> {
        &self.ops.ledgers
    }

    /// Upserts a board task and edge-fires the board's two automatic entries:
    /// a **dispatch** when the write moves the card into `in_progress`, and a
    /// **planning pass** when it moves the card into `planning` (issue #337).
    ///
    /// The single write site for REST task mutations, so neither trigger can be
    /// bypassed by writing straight to the store — and, just as importantly,
    /// the planning pass's own settle routes back through here, so a plan hands
    /// its card on through exactly the edge a human drag would fire rather than
    /// through a second copy of the dispatch gate.
    ///
    /// Both are edge-fired on the *transition*, never on the state: a card
    /// re-saved in the column it is already in is an edit, not a fresh entry,
    /// and must not spend a second time. The two are also mutually exclusive by
    /// construction — one write names one target column — so a card cannot be
    /// planned and dispatched by the same upsert.
    ///
    /// Both are detached (`tokio::spawn`), so the HTTP write returns at once and
    /// the result lands on the card asynchronously. Without an attached harness
    /// both are no-ops and the board stays inert — the card simply rests where
    /// it was put.
    ///
    /// Returns the record actually persisted, not necessarily `task` itself:
    /// when a stale `bounced` chip is cleared (above), the clone that carries
    /// the clear is what lands in the store, and a caller that went on to
    /// serialize its own `task` back to a client (`PATCH /tasks/{id}`'s REST
    /// handler) would otherwise hand back a `bounced` reason the stored card no
    /// longer has (Codex review, PR #1883).
    pub async fn upsert_task(self: &Arc<Self>, task: &TaskRecord) -> Result<TaskRecord> {
        let prev_column = self
            .ops
            .tasks
            .list(&self.id)
            .await?
            .into_iter()
            .find(|t| t.id == task.id)
            .map(|t| t.column);
        let dispatch = task_enters_in_progress(prev_column.as_deref(), &task.column);
        let plan = task_enters_planning(prev_column.as_deref(), &task.column);
        // Issue #1865: a card re-entering In Progress **or** Planning is a
        // fresh attempt, so any bounce chip left over from a *previous* failed
        // attempt is stale the moment this one starts — a card mid-retry must
        // not go on advertising the reason its last try came back. Planning
        // included (Codex review): "Plan first" on a bounced card is exactly
        // as much a fresh attempt as a direct re-dispatch, and the planning
        // pass's own settle paths (`settle_blocked`/`settle_failed` in
        // `harness::built_in::planning`) write back to To-do through the plain
        // `TaskStore::upsert` port, not through here — so if this call sat out
        // the planning edge, the stale chip would ride the card all the way
        // through the pass and reappear on a To-do that has nothing to do with
        // the dispatch failure it names.
        //
        // Codex review (PR #1883): gated on `task_leaves_todo`, not
        // `dispatch || plan` — `patch_task` accepts every board column on one
        // write, so a bounced card can leave `todo` straight for `in_review`
        // or `done` without ever touching `in_progress`/`planning`. That
        // manual transition supersedes the bounce exactly as much as a
        // re-dispatch does, and the field's own doc promises it clears "the
        // instant the card leaves `todo` any other way" — not only these two.
        // Cloned rather than mutating the caller's `task` in place: this is
        // the single write site for REST mutations and the caller may hold or
        // re-render its own copy afterwards.
        let write: std::borrow::Cow<'_, TaskRecord> =
            if task_leaves_todo(prev_column.as_deref(), &task.column) && task.bounced.is_some() {
                let mut cleared = task.clone();
                cleared.bounced = None;
                std::borrow::Cow::Owned(cleared)
            } else {
                std::borrow::Cow::Borrowed(task)
            };
        self.ops.tasks.upsert(&self.id, &write).await?;
        if dispatch {
            self.dispatch_task(task).await;
        }
        if plan {
            self.plan_task(task);
        }
        Ok(write.into_owned())
    }

    /// Fires the detached planning pass for a card that just entered
    /// `planning` (issue #337). In the default build — and on any runtime built
    /// without a harness — this is a no-op, keeping the column inert.
    ///
    /// Detached for the same reason [`dispatch_task`](Self::dispatch_task) is:
    /// a pass makes a model call, and the board write that triggered it is an
    /// HTTP request an operator is waiting on.
    ///
    /// Synchronous (no `async fn`) because there is nothing to await before the
    /// spawn. Unlike a dispatch, a pass mints **no** attempt row — there is no
    /// run, so there is nothing to write ahead of the spawn and nothing for a
    /// boot reaper to find. The card's own presence in `planning` is what
    /// records that a pass was started, and
    /// [`sweep_stranded_planning`](crate::runtime::advance::sweep_stranded_planning)
    /// is what recovers it if this process dies before the pass settles.
    #[allow(unused_variables)]
    fn plan_task(self: &Arc<Self>, task: &TaskRecord) {
        #[cfg(feature = "openhuman")]
        if self.planner.is_some() {
            let task_id = task.id.clone();
            let runtime = Arc::clone(self);
            tokio::spawn(async move {
                crate::harness::planning::run_planning_pass(runtime, task_id).await
            });
        }
    }

    /// Fires the detached [`TaskDispatched`] cycle for a task when a harness is
    /// attached. Detached (`tokio::spawn`) so the board write returns at once;
    /// the cycle writes its outcome back onto the card. In the default build (no
    /// harness) this is a no-op, keeping the board inert.
    ///
    /// The one **choke point** every dispatch passes through, which is why issue
    /// #242 mints the attempt's [`RunRecord`](crate::ports::runs::RunRecord)
    /// here — see [`open_run`](Self::open_run) for why it is minted *before* the
    /// spawn rather than inside the cycle.
    ///
    /// [`TaskDispatched`]: crate::ports::types::CompanyEvent::TaskDispatched
    async fn dispatch_task(self: &Arc<Self>, task: &TaskRecord) {
        // Issue #580: a `workflow`-deliverable card does not dispatch to its
        // assignee — building the workflow IS its In-Progress work. It routes
        // through the builder pass, which mints the same attempt row (so #339's
        // link stays honest and the spend is attributed) and settles the card to
        // In Review with a proposal, or back to To-do with the reason. Without a
        // wired builder the branch is inert and the card falls through to an
        // ordinary dispatch, exactly as a `once` card does.
        #[cfg(feature = "openhuman")]
        if task.deliverable == crate::ports::tasks::TaskDeliverable::Workflow
            && self.harness.is_some()
            && self.builder.is_some()
        {
            let task_id = task.id.clone();
            let run_id = self.open_run(task).await;
            let runtime = Arc::clone(self);
            tokio::spawn(async move {
                crate::harness::workflow_build::run_workflow_build_pass(runtime, task_id, run_id)
                    .await
            });
            return;
        }
        #[cfg(feature = "openhuman")]
        if self.harness.is_some() {
            let task_id = task.id.clone();
            let run_id = self.open_run(task).await;
            let runtime = Arc::clone(self);
            tokio::spawn(async move { runtime.run_dispatch_cycle(task_id, run_id).await });
            return;
        }
        // Default build / no harness: the board stays inert. The card rests in
        // `in_progress` until a harness cycle (or a human) advances it. No run is
        // minted either — nothing is attempting the card, so an attempt row would
        // be a fiction.
        //
        // Issue #1059: say so, once. Dispatching is where the intent shows —
        // somebody dragged a card into In Progress and is waiting for work — and
        // until now this returned in silence, so the card simply sat there with
        // no run, no timeline and nothing in the log to grep for. The builder is
        // the wrong place to say it: ~200 callers build a runtime with no harness
        // on purpose and never dispatch, so a warning there is noise on every one
        // of them and absent from the only case that is a mistake.
        //
        // Latched, because an inert board with fifty cards has one problem, not
        // fifty.
        if !self
            .inert_board_reported
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            // The remedy is per build, because there are two different causes
            // and only one of them is "nobody called `with_harness`" (issue
            // #1059 review). `RuntimeBuilder::with_harness` is itself
            // `#[cfg(feature = "openhuman")]`, so in a default-feature build
            // naming it sends the operator looking for a method that is not
            // compiled into their binary — and that build is not hypothetical:
            // `Dockerfile`'s `ARG FEATURES=""` ships it as a first-class
            // configuration, and Cargo.toml describes the default as offline
            // and echo-brained. There the thing that would actually help is to
            // rebuild with the feature.
            //
            // Only the remedy is split. The symptom stays one literal shared by
            // both builds, so the half that describes what happened cannot
            // drift between them while the half that says what to do about it
            // is the only thing the cfg decides.
            #[cfg(feature = "openhuman")]
            const REMEDY: &str = "Wire one with `RuntimeBuilder::with_harness(...)` (see \
                 `src/bin/opencompany.rs`), or move the card by hand.";
            #[cfg(not(feature = "openhuman"))]
            const REMEDY: &str = "This binary was built without the `openhuman` feature, so it \
                 has no harness to wire — rebuild with `--features openhuman` (the `FEATURES` \
                 build arg in `Dockerfile`), or move the card by hand.";
            tracing::warn!(
                company = %self.id,
                task = %task.id,
                "[board] a card was dispatched but this runtime has no agent pool, so nothing \
                 will work it: the card stays in `in_progress` with no attempt row. {REMEDY} \
                 Reported once per runtime."
            );
        }
        let _ = task;
    }

    /// The body of a dispatch's detached cycle (issue #242), split out of the
    /// `tokio::spawn` so the quiesce path below is reachable from a test.
    ///
    /// Owns the settle for the one dispatch failure the cycle's own terminality
    /// backstop cannot see — see [`abandon_run`](Self::abandon_run).
    #[cfg(feature = "openhuman")]
    async fn run_dispatch_cycle(self: Arc<Self>, task_id: String, run_id: Option<String>) {
        let report = match self
            .run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: task_id.clone(),
                run_id: run_id.clone(),
            }])
            .await
        {
            Ok(report) => report,
            Err(err) => {
                // Issue #290 meets issue #242. `ensure_accepting` refuses
                // *before* `CycleRunner` takes the serial lock, so a dispatch
                // that lands in the window while this runtime is being
                // replaced never reaches `begin_run` — and the backstop
                // inside the cycle only settles rows that cycle started.
                // Every other dispatch failure is already covered in there.
                // Left alone, the row minted a moment ago would sit `Pending`
                // for the rest of the process's life: a card reading as under
                // way by an attempt that never began, which nothing
                // re-drives, and which the rebuild deliberately does *not*
                // run the boot reaper to clean up.
                if let Some(id) = run_id.as_deref()
                    && matches!(err, OpenCompanyError::Quiescing(_))
                {
                    self.abandon_run(id, &task_id).await;
                }
                tracing::warn!(
                    company = %self.id,
                    task = %task_id,
                    error = %err,
                    "task dispatch cycle failed"
                );
                return;
            }
        };
        // Issue #1852 Part 1: `run_task`/`refuse_dispatch` already build the
        // right relay via `relay_reply` — it rides home in this report's
        // responses — but until now nothing wrote it down. Unlike the
        // chat-POST path (`journal_chat_replies`) and the approval path
        // (`publish_continuation`), this dispatch path had no journaling step
        // at all, so the answer never reached the thread it was spawned from,
        // live or on reload.
        self.journal_dispatch_replies(&report).await;
    }

    /// Settles an attempt whose cycle was refused before it could start
    /// (issue #290).
    ///
    /// `Pending` → terminal is exactly the move the transition table names for
    /// "a dispatch that failed before the first turn"
    /// ([`RunStatus::can_transition_to`](crate::ports::runs::RunStatus::can_transition_to)),
    /// so this needs no new state — only a caller willing to use it.
    ///
    /// Recorded as [`RunStatus::Failed`](crate::ports::runs::RunStatus::Failed)
    /// rather than `Cancelled`, for the same reason the boot reaper picks
    /// `Failed`: the runtime went away underneath a card an operator had just
    /// dispatched, and that is something they need to see and re-drive, not an
    /// intentional stop filed quietly away. The reason string is its own
    /// constant ([`RUNTIME_REPLACED_ERROR`](crate::ports::runs::RUNTIME_REPLACED_ERROR))
    /// so a run list can tell "we swapped your runtime" apart from "the host
    /// died".
    ///
    /// **Issue #337: the card comes back too.** This used to settle the row and
    /// deliberately leave the card in `in_progress`, "which is where every other
    /// failed dispatch cycle leaves it". That was true and it was the bug: the
    /// dispatch edge fires on the *transition* into In Progress, which has
    /// already happened, so nothing re-drives the card and an operator is left
    /// staring at work that is provably not being done. It now returns to To-do
    /// carrying the reason, through the guarded mover — so a card that has since
    /// been dragged, parked or landed by a later attempt is untouched.
    ///
    /// Best-effort and logged, never propagated: the dispatch has already
    /// failed, and a bookkeeping write cannot make that better or worse.
    #[cfg(feature = "openhuman")]
    async fn abandon_run(&self, run_id: &str, task_id: &str) {
        let outcome = crate::ports::runs::RunOutcome::new(crate::ports::runs::RunStatus::Failed)
            .with_error(crate::ports::runs::RUNTIME_REPLACED_ERROR);
        if let Err(err) = self.ops.runs.finish_run(&self.id, run_id, outcome).await {
            tracing::warn!(
                company = %self.id,
                run = %run_id,
                error = %err,
                "[runs] could not settle an attempt refused by a quiescing runtime; it stays \
                 Pending until the next boot reaps it"
            );
            // The row is still Pending, so the card is still truthfully claimed
            // by an attempt. Leave it for the boot reaper to settle both.
            return;
        }
        match crate::runtime::advance::advance_settled_card(
            self.ops.tasks.as_ref(),
            &self.id,
            task_id,
            crate::ports::runs::RunStatus::Failed,
            crate::ports::runs::RUNTIME_REPLACED_ERROR,
        )
        .await
        {
            // Issue #1865: the card actually bounced to To-do — notify, the
            // same as the cycle's own terminality backstop does for the far
            // more common "the brain errored" shape of this failure.
            Ok(Some(crate::ports::tasks::COLUMN_TODO)) => {
                self.notify_dispatch_failed(task_id, crate::ports::runs::RUNTIME_REPLACED_ERROR)
                    .await;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    company = %self.id,
                    run = %run_id,
                    task = %task_id,
                    error = %err,
                    "[runs] settled an attempt refused by a quiescing runtime but could not \
                     return its card; it stays in progress until the next boot"
                );
            }
        }
    }

    /// Parks a blocker on the approval gate from **outside a cycle** (issue
    /// #1861).
    ///
    /// The planning pass runs in a detached `tokio::spawn` with no cycle around
    /// it and no attempt row of its own, so it cannot reach
    /// `CycleRunner::park` and must not stage onto the harness's
    /// approval-request queue: nothing would drain that until some later,
    /// unrelated chat cycle happened to run, and the park would then be
    /// attributed to that turn's thread rather than to this card.
    ///
    /// So this is `CycleRunner::park`'s journal-and-announce half, minus the
    /// two things only a cycle can honestly supply:
    ///
    /// * **No continuation is armed.** There is no turn suspended on this
    ///   answer — the pass has already finished. Arming one would leave a
    ///   counter against a cycle that will never run again. Resuming a planning
    ///   blocker means re-dispatching the card, which is #1863's work.
    /// * **No grant is marked pending.** `mark_pending` protects a live
    ///   checkout from another turn's orphan sweep; a finished pass holds none.
    ///
    /// Ordering matches the cycle's exactly: gate, then the journal write that
    /// binds it, then the advisory event. A crash between the journal and the
    /// event replays as "still parked" and the console picks it up on its next
    /// feed refresh, which is the same trade `CycleRunner::park` documents.
    ///
    /// # Why this is feature-gated and its expiry half is not
    ///
    /// Its only caller is the planning pass, which is
    /// `#[cfg(feature = "openhuman")]`, so on a default build this is a method
    /// nobody can reach — and `-D dead_code` is right to say so.
    ///
    /// The *expiry* half of the same story — `unanswered_blocker` and the card
    /// return it drives — stays ungated on purpose: the TTL sweep that runs it
    /// is ungated, and a blocker parked by a gated build still has to expire
    /// correctly on any build that later loads the same journal.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn park_blocker(
        &self,
        payload: &crate::ports::blockers::BlockerPayload,
        task_id: &str,
    ) -> Result<ApprovalId> {
        use crate::ports::types::{Effect, EffectGroup};
        use crate::runtime::journal::{ApprovalConversation, TaskLink};

        let effect = Effect {
            kind: payload.effect_kind(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
            // Not an agent's blocked tool call — see `Effect::agent`. Approving
            // one is inert until #1863 carries the answer back.
            agent: None,
            // A pass mints no attempt row, so there is no run waiting on this.
            run_id: None,
        };
        let approval_id = self.approvals.park(&self.id, effect.clone()).await?;
        // The gate park is already live at this point, so a failing journal
        // write cannot simply `?` out: the caller would read the park as failed
        // and return the card to To-do while the gate still held a decidable
        // approval against it — an operator shown a question for a card nobody
        // paused, the same inconsistency `unpark_blocker` exists to prevent on
        // the other side of this pair.
        //
        // Undone in memory only, and that is the whole point: the durable write
        // is the thing that failed, so a compensating *record* would go down
        // the same broken path. `resolve_outcome` with a `Deny` drops the
        // parked entry and mints nothing — `GrantedCall` exists only on the
        // `Approved` arm — and `discard_unrecorded_park` clears the projection
        // rows `record_parked` inserted before its append. Nothing was durably
        // parked, so nothing is durably retired; the error propagates and the
        // caller returns the card exactly as it does for a refused park.
        if let Err(err) = self
            .journal
            .record_parked(
                &approval_id,
                &effect,
                now_millis(),
                TaskLink::from_task_id(Some(task_id)),
                // No conversation: a planning pass is not anybody's turn, so
                // there is no thread to thread the answer back into.
                ApprovalConversation {
                    thread: None,
                    parent: None,
                },
                None,
            )
            .await
        {
            self.approval_gate.resolve_outcome(
                &approval_id,
                Verdict::Deny,
                Actor {
                    kind: ActorKind::System,
                    id: "park-rollback".into(),
                },
                now_millis(),
            );
            self.journal.discard_unrecorded_park(&approval_id);
            tracing::warn!(
                company = %self.id,
                task = %task_id,
                %approval_id,
                error = %err,
                "[blockers] a blocker could not be journaled; its gate entry was rolled back so \
                 the card returns rather than leaving an undecidable question"
            );
            return Err(err);
        }
        if let Err(err) = self
            .events
            .append(
                &self.id,
                CompanyEvent::ApprovalParked {
                    approval_id: approval_id.clone(),
                    effect_kind: effect.kind.clone(),
                    thread: None,
                },
            )
            .await
        {
            tracing::warn!(
                company = %self.id,
                approval_id = %approval_id,
                error = %err,
                "blocker parked and journaled, but its event-log entry failed",
            );
        }
        Ok(approval_id)
    }

    /// Withdraws a blocker this pass just parked, because the card write that
    /// was supposed to follow it failed (issue #1861).
    ///
    /// [`park_blocker`](Self::park_blocker) deliberately runs **before** the
    /// card is written, so an operator can never be shown a `paused` column
    /// with nothing in the queue to release it. This is the other half of that
    /// trade. Without it the failing write leaves the opposite inconsistency —
    /// a live blocker naming a card still in Planning — and nothing repairs it:
    /// [`return_expired_blocker_card`](crate::runtime::advance::return_expired_blocker_card)
    /// only moves cards already in `paused`, so the TTL sweep would retire the
    /// approval and leave the card exactly where it was stuck.
    ///
    /// Routed through [`retire_approval`](Self::retire_approval), the single
    /// retirement primitive, so this leaves the same durable trail as every
    /// other retirement: an `ApprovalExpired` line and a `Deny` with the system
    /// named, never a grant. Only the recorded
    /// [`ExpiryReason`] differs, and it differs on purpose — see
    /// [`ExpiryReason::CardUnwritable`].
    ///
    /// Feature-gated for the reason `park_blocker` is: the planning pass is its
    /// only caller.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn unpark_blocker(self: &Arc<Self>, id: &ApprovalId) -> Result<()> {
        self.retire_approval(id, ExpiryReason::CardUnwritable, now_millis())
            .await
    }

    /// Thin `&self` wrapper around
    /// [`advance::notify_dispatch_failed`](crate::runtime::advance::notify_dispatch_failed)
    /// for the two callers that already hold a live [`CompanyRuntime`]:
    /// [`abandon_run`](Self::abandon_run) and the cycle's terminality
    /// backstop. The boot reaper's card sweep runs before a `CompanyRuntime`
    /// exists, so it calls the shared function directly — see that doc for
    /// the full three-caller picture.
    pub(crate) async fn notify_dispatch_failed(&self, task_id: &str, reason: &str) {
        crate::runtime::advance::notify_dispatch_failed(
            self.notifications().as_ref(),
            &self.id,
            task_id,
            reason,
        )
        .await;
    }

    /// Mints this dispatch's [`RunStatus::Pending`] attempt row and returns its
    /// id, or `None` when the row could not be written (issue #242).
    ///
    /// **Before the spawn, deliberately.** The cycle is a detached
    /// `tokio::spawn`, so a host that dies in the gap between this write and the
    /// cycle's first turn used to leave *nothing at all* behind: the card sat in
    /// `in_progress` with no record that anything had ever tried it. Writing the
    /// row first turns that silent loss into a visible orphan the boot reaper
    /// ([`reap_orphaned_runs`](crate::ports::runs::reap_orphaned_runs)) fails on
    /// the next start.
    ///
    /// **A failed write never blocks the dispatch.** Record-keeping does not get
    /// to fail the work it records — the same invariant the workflow-outcome
    /// journal, the inference meter and the grant-consumption record already
    /// hold. The dispatch proceeds with `run_id: None`, which every downstream
    /// reader treats as "this attempt is untracked", and the failure is logged at
    /// `warn` rather than swallowed.
    ///
    /// [`RunStatus::Pending`]: crate::ports::runs::RunStatus::Pending
    #[cfg(feature = "openhuman")]
    async fn open_run(&self, task: &TaskRecord) -> Option<String> {
        let spec = crate::ports::runs::NewRun::for_task(
            crate::ports::generate_id(),
            task.id.clone(),
            task.assignee.clone(),
        );
        match self.ops.runs.create_run(&self.id, spec).await {
            Ok(run) => {
                tracing::debug!(
                    company = %self.id,
                    task = %task.id,
                    run = %run.id,
                    attempt = run.attempt,
                    "[runs] opened an attempt for a dispatched card"
                );
                Some(run.id)
            }
            Err(err) => {
                tracing::warn!(
                    company = %self.id,
                    task = %task.id,
                    error = %err,
                    "[runs] could not open an attempt row; dispatching anyway — the work runs \
                     untracked rather than not at all"
                );
                None
            }
        }
    }

    /// This company's workspace file tree.
    pub fn workspace(&self) -> &Arc<dyn WorkspaceStore> {
        &self.ops.workspace
    }

    /// This company's durable memory-facts view.
    pub fn facts(&self) -> &Arc<dyn FactStore> {
        &self.ops.facts
    }

    /// This company's versioned task artifacts (#187).
    pub fn artifacts(&self) -> &Arc<dyn ArtifactStore> {
        &self.ops.artifacts
    }

    /// This company's task-run records (#242): one row per attempt at a card,
    /// with its status, step trace and cost.
    pub fn runs(&self) -> &Arc<dyn RunStore> {
        &self.ops.runs
    }

    /// The unredacted companion of this company's run steps: reasoning text and
    /// raw tool I/O, kept beside the scrubbed skeleton in [`Self::runs`].
    pub fn deep_trace(&self) -> &Arc<dyn crate::ports::deep_trace::DeepTraceStore> {
        &self.ops.deep_trace
    }

    /// This company's per-workflow edit history (#274), the snapshot ring a
    /// workflow rollback reads and writes.
    pub fn workflow_revisions(&self) -> &Arc<dyn WorkflowRevisionStore> {
        &self.ops.workflow_revisions
    }

    /// This company's durable scheduler fire claims (#241): one row per
    /// `(schedule, minute)` the schedulers use to dedup fires across replicas and
    /// restarts.
    pub fn schedule_fires(&self) -> &Arc<dyn ScheduleFireStore> {
        &self.ops.schedule_fires
    }

    /// This company's durable per-node run output snapshots (#596): one record
    /// per settled run, read by the console run inspector to show what each node
    /// produced on any past run.
    pub fn workflow_run_outputs(&self) -> &Arc<dyn WorkflowRunOutputStore> {
        &self.ops.workflow_run_outputs
    }

    /// This company's usage meter (written by the cost hook, read by WS5).
    pub fn usage(&self) -> &Arc<dyn UsageMeter> {
        &self.ops.usage
    }

    /// Which cognition path this company actually booted onto, and where that
    /// path's inference usage is metered.
    ///
    /// The console's inference-status route surfaces this so an operator can tell
    /// "no inference source resolved, so the company fell back to a path that
    /// spends nothing" from "inference ran but the meter never saw it" — the
    /// silent degradation that made issue #174 hard to read.
    pub fn cognition(&self) -> crate::ports::Cognition {
        self.brain.cognition()
    }

    /// This company's skill-state deltas.
    pub fn skills(&self) -> &Arc<dyn SkillStateStore> {
        &self.ops.skills
    }

    /// Where each person has read to, per channel (#755).
    pub fn read_state(&self) -> &Arc<dyn ReadStateStore> {
        &self.ops.read_state
    }

    /// Durable notifications with per-person read state (#749).
    pub fn notifications(&self) -> &Arc<dyn NotificationStore> {
        &self.ops.notifications
    }

    /// This company's human collaborators and their invites.
    pub fn users(&self) -> &Arc<dyn UserStore> {
        &self.ops.users
    }

    /// This company's live browser sessions.
    pub fn sessions(&self) -> &Arc<dyn SessionStore> {
        &self.ops.sessions
    }

    /// This company's pending magic-link login codes.
    pub fn login_codes(&self) -> &Arc<dyn LoginCodeStore> {
        &self.ops.login_codes
    }

    /// Whether an agent economy (tiny.place) is wired in.
    pub fn has_economy(&self) -> bool {
        self.economy.is_some()
    }

    /// Stops this runtime accepting new cycles, then waits for the one in flight
    /// to finish (issue #290).
    ///
    /// The two halves are both necessary and neither is sufficient. Setting the
    /// flag alone leaves a turn mid-cycle, and a successor that started writing
    /// the same journal underneath it would interleave two records onto one line
    /// — a parse failure that bricks the *next* boot, not just this one. Waiting
    /// on [`serial`](Self::serial) alone would race: a cycle queued behind the
    /// one in flight would acquire the lock the moment it dropped and the drain
    /// would return to a runtime that is busy again.
    ///
    /// The `serial` guard is deliberately released before returning. It is
    /// handed to the successor by [`RuntimeHandover`](crate::runtime::RuntimeHandover),
    /// so holding it here would park the successor's first cycle behind a guard
    /// this runtime no longer has any reason to own.
    ///
    /// What this does **not** drain: work that never takes `serial`. Detached
    /// task dispatches and scheduled workflow runs each clone an
    /// `Arc<CompanyRuntime>` and run a cycle, so they *are* covered — they either
    /// completed before the flag was set or they are the turn being waited on.
    /// A harness tool call already in flight inside that turn finishes on the old
    /// pool, which the successor then inherits.
    pub async fn quiesce(&self) {
        self.quiesced.store(true, Ordering::SeqCst);
        // Acquiring proves the in-flight cycle (if any) has finished.
        let _drained = self.serial.lock().await;
    }

    /// Marks this runtime quiesced **without** draining it (issue #986).
    ///
    /// The drain half of [`quiesce`](Self::quiesce) proves the in-flight cycle
    /// finished. This is for a runtime that cannot have one: the registry calls
    /// it while a company is being registered during shutdown, before anything
    /// can reach the runtime to start a cycle on it. There is nothing to wait
    /// for, and waiting would mean taking `serial` — which on a rebuild
    /// successor is the *predecessor's* lock, so this would park behind the very
    /// turn the swap is handing over.
    ///
    /// Not a substitute for `quiesce` anywhere a cycle could already be running.
    pub(crate) fn mark_quiesced(&self) {
        self.quiesced.store(true, Ordering::SeqCst);
    }

    /// Puts a quiesced runtime back to work.
    ///
    /// Called when a rebuild fails: a company left quiesced would refuse every
    /// cycle forever, which is a far worse outcome than the stale brain the
    /// rebuild was trying to replace.
    pub fn resume(&self) {
        self.quiesced.store(false, Ordering::SeqCst);
    }

    /// Whether this runtime has stopped accepting cycles pending a swap.
    pub fn is_quiesced(&self) -> bool {
        self.quiesced.load(Ordering::SeqCst)
    }

    /// Adopts the serialising mutexes of the runtime this one replaces
    /// (issue #290), so the cycle and board-write invariants span the swap
    /// instead of lapsing at it.
    ///
    /// Called by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) on a
    /// rebuild, before the successor is registered and therefore before anything
    /// can be holding either lock through *this* runtime.
    pub fn adopt_locks(
        &mut self,
        serial: Arc<TokioMutex<()>>,
        per_agent: Arc<TokioMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
        task_writes: Arc<TokioMutex<()>>,
    ) {
        self.serial = serial;
        self.per_agent = per_agent;
        self.task_writes = task_writes;
    }

    /// Installs the continuation queue the builder prepared (issue #469) —
    /// re-armed from the journal on a boot, inherited live on a rebuild.
    ///
    /// Set through the builder rather than [`new`](Self::new) so the ~30 direct
    /// `CompanyRuntime::new` call sites do not each have to learn about a queue
    /// only the approval path reads, matching how the locks are handed over.
    pub fn adopt_continuations(&mut self, continuations: ContinuationQueue) {
        self.continuations = continuations;
    }

    /// Installs the run-scoped workflow gate batches the builder prepared
    /// (issue #978) — rehydrated from the journal's still-parked gates on a
    /// boot, inherited live on a rebuild.
    ///
    /// Set through the builder for exactly
    /// [`adopt_continuations`](Self::adopt_continuations)' reason, and always
    /// alongside it: the two describe one run's decisions from opposite sides,
    /// and a runtime holding a fresh copy of one and an inherited copy of the
    /// other would release a batch it cannot re-dispatch.
    pub fn adopt_workflow_gates(&mut self, gates: WorkflowGateQueue) {
        self.workflow_gates = gates;
    }

    /// Installs the blocked-agent-node stash the builder prepared (issue #899,
    /// Stage 1) — inherited live on a rebuild, and empty on a boot (the parked
    /// tool-call effect carries nothing to rehydrate it from).
    ///
    /// Set through the builder for [`adopt_continuations`](Self::adopt_continuations)'
    /// reason, and shared with the workflow runner's `DeliveryParking` so the
    /// runner that arms a stash at block-settle and the `continue_turn` that
    /// releases it see one set.
    pub fn adopt_blocked_nodes(&mut self, blocked_nodes: BlockedNodeQueue) {
        self.blocked_nodes = blocked_nodes;
    }

    /// The blocked-agent-node stash, for the workflow-node continuation fork in
    /// [`continue_turn`](Self::continue_turn) (issue #899, Stage 1).
    pub fn blocked_nodes(&self) -> &BlockedNodeQueue {
        &self.blocked_nodes
    }

    /// Rejects a cycle on a runtime that is being replaced.
    ///
    /// Separate from [`ensure_running`](Self::ensure_running): that one reads a
    /// durable lifecycle an operator chose (paused, archived) and renders `409`;
    /// this one is a process-local window that clears itself within a turn and
    /// renders `503`.
    /// `pub(crate)` since issue #983 rather than private: a caller that journals
    /// its own input has to be able to ask this **before** it writes, since a
    /// refusal ordered after the append would leave a message in the transcript
    /// that no turn will ever answer. Every in-tree caller still goes through
    /// one of the cycle entry points below; this exists so the chat route can
    /// run the same check one step earlier.
    pub(crate) fn ensure_accepting(&self) -> Result<()> {
        if self.is_quiesced() {
            return Err(OpenCompanyError::Quiescing(self.id.as_ref().to_string()));
        }
        Ok(())
    }

    /// Refuses a write addressed to the read-only Operator system channel,
    /// unless a real desk or roster teammate already owns that literal id
    /// (the migration carve-out below).
    ///
    /// Issue #1757: the Operator channel is a **read-only** aggregation
    /// surface — a "what happened" feed of workflow reports, not a
    /// conversation. Every ingress that journals an `OperatorMessage` under a
    /// caller-chosen chat id has to run this same check before appending
    /// anything, or "read-only" is only true for whichever ingress remembered
    /// to ask. Per the PR #1781 review (Codex P1): the ACP `session/prompt`
    /// route used to journal straight past the REST route's inline version of
    /// this guard, because it never called `chat_and_emit` at all — it
    /// appends to `self.events()` directly. `ensure_accepting` above is the
    /// model this follows: a check the write route runs on *itself*,
    /// immediately before it appends, so a second ingress into the same
    /// journal cannot forget it either.
    ///
    /// Migration carve-out: `operator` was not reserved before issue #1757,
    /// so a company provisioned earlier can already have a real manifest or
    /// overlay desk (`from_stored_toml` deliberately never re-validates a
    /// stored manifest) or roster teammate (`ChatView` addresses a DM by bare
    /// id, issue #364) already using that id. A literal `desk_exists` check
    /// alone would miss two shapes: the teammate case — it only walks
    /// `group_chats` and `overlay_desks`, never the roster, so
    /// `is_roster_agent` is checked alongside it, the same carve-out applied
    /// to the other namespace `RESERVED_AGENT_IDS` reserves — and a desk
    /// grandfathered by **name** rather than id (issue #1781 review, Codex
    /// P1 follow-up): `{ id = "legacy_ops", name = "Operator" }` is exactly
    /// the collision `operator_feed_channel` diverts the system feed off of,
    /// but `desk_exists("operator")` only ever matches on id, so a chat or
    /// ACP send addressed through the desk's own supported case-insensitive
    /// `Operator` alias — which every *read* already resolves via
    /// `resolve_desk_id` — was refused here as if it named the fake system
    /// channel. Resolving `desk` (the actual selector, alias and all)
    /// through `resolve_desk_id` first is what makes this guard agree with
    /// the read path on which desk a caller meant.
    ///
    /// `OPERATOR_CHANNEL_COLLISION_FALLBACK` is the id `list_desks` hands the
    /// synthetic system desk when a roster teammate is the one grandfathered
    /// onto `operator` (see `CompanyRecord::operator_feed_channel`), and its
    /// **id** is unmintable by any real desk or agent (see the constant's
    /// doc). Its display **name** is not id-validated at all, though (issue
    /// #1781 review, Codex P2 follow-up): a pre-#1757 manifest desk such as
    /// `{ id = "ops", name = "operator-feed" }` predates every id-charset
    /// rule this reasoning leans on, `from_path_for_reload` never
    /// re-validates a stored manifest, and this can be true even without a
    /// *primary* `operator` collision at all. So this branch resolves the
    /// alias first too, the same as the literal `operator` case below — only
    /// refusing once nothing real actually claims it.
    ///
    /// The store load's `?` propagates a real store failure as itself, rather
    /// than collapsing it into "no real desk" — that would misreport a
    /// transient store error as the ordinary read-only refusal, for every
    /// company, and journal the failure nowhere.
    pub(crate) async fn ensure_desk_writable(&self, desk: &str) -> Result<()> {
        if desk.eq_ignore_ascii_case(crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK) {
            // `resolve_desk_id(desk)` — not an unconditional refusal — for the
            // identical reason the `OPERATOR_CHANNEL` branch below resolves
            // its alias first (issue #1781 review, Codex P2 follow-up):
            // `OPERATOR_CHANNEL_COLLISION_FALLBACK`'s id is unmintable by any
            // *new* desk (`is_valid_desk_id` rejects the hyphen), but its
            // display **name** is not id-validated at all, and
            // `from_path_for_reload` deliberately never re-validates a stored
            // manifest — so a pre-#1757 desk such as
            // `{ id = "ops", name = "operator-feed" }` can already exist,
            // stay listed and readable, and (unlike the id case) be true even
            // when there is no *primary* `operator` collision at all. Without
            // this, a send addressed through that desk's own supported
            // case-insensitive alias — the one every read already resolves
            // via `resolve_desk_id` — was refused here as if it named the
            // synthetic read-only system desk instead.
            let has_real_recipient = self
                .store()
                .load(&self.id)
                .await?
                .is_some_and(|record| record.resolve_desk_id(desk).is_some());
            if !has_real_recipient {
                return Err(OpenCompanyError::InvalidRequest(
                    "the Operator channel is a read-only feed of workflow reports and \
                     notifications — it cannot be posted to"
                        .to_string(),
                ));
            }
        }
        if desk.eq_ignore_ascii_case(crate::runtime::OPERATOR_CHANNEL) {
            let has_real_operator_recipient =
                self.store().load(&self.id).await?.is_some_and(|record| {
                    // `resolve_desk_id(desk)` — not `desk_exists(OPERATOR_CHANNEL)`
                    // — so a grandfathered desk claiming this alias only by
                    // **name** (`{ id: "legacy_ops", name: "Operator" }`) is
                    // recognised the same way the read path already resolves
                    // it, not just one claiming the literal id.
                    record.resolve_desk_id(desk).is_some()
                        || record.is_roster_agent(crate::runtime::OPERATOR_CHANNEL)
                });
            if !has_real_operator_recipient {
                return Err(OpenCompanyError::InvalidRequest(
                    "the Operator channel is a read-only feed of workflow reports and \
                     notifications — it cannot be posted to"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Runs one cycle over a batch of events, returning what happened.
    pub async fn run_cycle(&self, events: Vec<CompanyEvent>) -> Result<CycleReport> {
        self.ensure_accepting()?;
        CycleRunner::new(self).run(events).await
    }

    /// [`run_cycle`](Self::run_cycle), for inputs the caller has **already**
    /// appended to the journal (issue #983).
    ///
    /// The chat route journals the operator's message the instant the request
    /// is accepted, so the transcript is correct from acceptance rather than
    /// from whenever the cycle wins the per-company serial lock — behind a busy
    /// company, an unbounded time later. Handing the seq over here is what stops
    /// the same message being appended a second time.
    ///
    /// `run_id` is a run row moved `Pending` → `Running` once that lock is
    /// actually held; see [`CycleRunner::run_journaled`].
    ///
    /// Deliberately a second entry point rather than a parameter on the first:
    /// every other trigger — scheduler, cron, webhooks, telegram, delegation,
    /// approval follow-ups — keeps `run_cycle` byte-unchanged, so the append
    /// they rely on cannot be turned off by a mistake at a call site.
    pub async fn run_journaled_cycle(
        &self,
        events: Vec<(EventSeq, CompanyEvent)>,
        run_id: Option<String>,
    ) -> Result<CycleReport> {
        self.ensure_accepting()?;
        CycleRunner::new(self).run_journaled(events, run_id).await
    }

    /// Resolves a parked approval and runs a follow-up cycle so the brain learns
    /// the verdict. Returns the follow-up cycle's report.
    ///
    /// The verdict is settled inline and the follow-up cycle runs on a **spawned
    /// task** this then awaits — so the report is the same one callers always
    /// got, but the cycle producing it no longer lives inside the caller's
    /// future. See [`resolve_approval_spawned`](Self::resolve_approval_spawned)
    /// for why that matters.
    pub async fn resolve_approval(
        self: &Arc<Self>,
        id: &ApprovalId,
        verdict: Verdict,
        by: Actor,
    ) -> Result<CycleReport> {
        let (_, follow_up) = self
            .resolve_approval_spawned(id, verdict, by, GrantScope::Once)
            .await?;
        join_follow_up(follow_up).await
    }

    /// Settles a parked approval's verdict **inline** and runs its follow-up
    /// cycle on a spawned task, handing back both the receipt and the task's
    /// handle (issue #383).
    ///
    /// The point of the split is drop-safety. This host is plain
    /// `axum::serve(listener, router(state))`; hyper drops a handler's future the
    /// moment the peer closes the connection, and a reverse proxy in front of a
    /// hosted tenant closes it the moment it decides the upstream is too slow.
    /// Nothing on the old resolve path was spawned, so the follow-up agent turn
    /// lived inside that future and died with it — after the verdict was
    /// journaled and after the single-use grant was minted. The operator's
    /// approval was spent on a re-dispatch that never happened, and the
    /// conversation never resumed (issue #380, defect 3).
    ///
    /// Awaiting a [`JoinHandle`] is drop-safe: dropping the handle abandons the
    /// *waiting*, not the work. So every caller — including the one that just
    /// awaits it and answers exactly as before — survives a disconnect, with no
    /// wire change required to get that.
    ///
    /// Spawned cycles still serialise on the runtime's per-company cycle lock
    /// exactly as inline ones did, so this adds no concurrency. What changes is
    /// that a burst of resolves all *complete* rather than some being shed by
    /// dropped connections.
    ///
    /// `ensure_accepting` is checked before the gate is touched, not inside the
    /// follow-up cycle: a resolution that journaled the verdict and then failed
    /// to run its follow-up would leave the brain permanently unaware of an
    /// approval the operator had already granted.
    /// `scope` is what the operator's approve buys (issue #374):
    /// [`GrantScope::Once`] is the default and the pre-#374 behaviour byte for
    /// byte; [`GrantScope::Tool`] mints a standing permission instead. A scope
    /// the runtime must not honour is refused **before** the gate is touched, so
    /// the approval stays parked and no verdict is journaled.
    pub async fn resolve_approval_spawned(
        self: &Arc<Self>,
        id: &ApprovalId,
        verdict: Verdict,
        by: Actor,
        scope: GrantScope,
    ) -> Result<(ResolveReceipt, JoinHandle<Result<CycleReport>>)> {
        self.ensure_accepting()?;
        let receipt = CycleRunner::new(self)
            .settle_approval(id, verdict, by, scope)
            .await?;
        self.retire_if_expired(id, &receipt).await?;
        Ok((receipt.clone(), self.spawn_follow_up(receipt)))
    }

    /// Finishes the retirement a [`ResolveReceipt::Expired`] owes (issue #1449).
    ///
    /// The gate dropped the entry inside its own critical section — that is what
    /// `Expired` reports — and this is the rest of the transaction:
    /// [`retire_approval`](Self::retire_approval), the single retirement
    /// primitive, exactly as the sweeper reaches it. So a deadline that passes
    /// unnoticed and a deadline that passes one second before the operator
    /// clicks now leave **the same** durable trail: an `ApprovalExpired` line
    /// and an `ApprovalResolved { verdict: Deny, by: System }` event, with no
    /// human's name attached to an approval that did not happen.
    ///
    /// It runs **here**, inline, rather than inside the spawned follow-up: the
    /// detached resolve answers `recorded: true` the moment this returns, and a
    /// receipt that claims durability while its journal write is still queued on
    /// another task is the same class of untrue statement as the one being fixed.
    ///
    /// A no-op for every other receipt.
    ///
    /// Also files the same `approval_expired` notification
    /// [`sweep_expired_approvals`](Self::sweep_expired_approvals) files when
    /// *it* is the one to discover the deadline (issue #1865, Codex review on
    /// PR #1883). Both callers reach the identical outcome — a parked
    /// approval that ran out unanswered — and `notify_approval_expired` is
    /// invoked from nowhere else, so before this an expiry notified when the
    /// sweeper found it first and stayed silent when a late resolve found it
    /// instead. Best-effort and after the retirement, same ordering as the
    /// sweep: a notification that could not be filed must not undo a
    /// default-deny that already happened.
    async fn retire_if_expired(
        self: &Arc<Self>,
        id: &ApprovalId,
        receipt: &ResolveReceipt,
    ) -> Result<()> {
        if !receipt.expired() {
            return Ok(());
        }
        // Both read BEFORE the retirement, for the reason
        // `sweep_expired_approvals` gives at its own call: retiring is what
        // removes the approval from the journal's pending set, and after that
        // there is no way back to what was being asked. Main's #1883 added
        // this call site against the one-argument signature that predated
        // #1861's blocker/approval distinction; without the two flags the
        // notice would tell an operator a question was "denied by default"
        // when nothing was ever decided.
        //
        // `finish_expiry` carries the rest, so a deadline the sweeper finds
        // first and one a late resolve finds instead leave the same board and
        // the same badge behind.
        let unanswered = self.unanswered_blocker(id);
        let is_blocker = self.is_blocker(id);
        self.retire_approval(id, ExpiryReason::Ttl, now_millis())
            .await?;
        self.finish_expiry(id, is_blocker, unanswered).await;
        Ok(())
    }

    /// Pushes a parked approval's deadline out to a fresh full TTL window,
    /// giving the operator more time before it default-denies (issue #1805).
    ///
    /// Returns the approval's **new** deadline (epoch-millis: the extension
    /// instant plus the gate's current TTL), the same number the card's
    /// countdown will now project. Errors with
    /// [`OpenCompanyError::NotFound`] when no such approval is parked — an
    /// unknown id, or one already resolved or expired — so a caller answers 404
    /// rather than reporting an extension of nothing.
    ///
    /// # Why a full window rather than "+N hours"
    ///
    /// Re-anchoring the TTL to now reuses the single deadline the sweeper and
    /// the console already agree on (`parked_at + ttl`), so there is no second
    /// stored offset for a projection to compute differently. Extend is the
    /// mirror of the shortening path that made this issue matter: an approval
    /// that vanishes on a deadline is only acceptable if the operator can also
    /// keep it alive.
    ///
    /// The move is made durable in two places kept in step exactly as the park
    /// instant already is: the live gate the sweeper reads, and the journal the
    /// projection reads and the next boot rehydrates the gate from — so an
    /// extension survives a redeploy instead of reverting.
    pub async fn extend_approval(&self, id: &ApprovalId, by: Actor) -> Result<u64> {
        self.ensure_accepting()?;
        let now = now_millis();
        // The gate is the existence check: `false` means nothing is parked under
        // this id, so nothing is extended and the caller owes a 404.
        if !self.approval_gate.extend(id, now) {
            return Err(OpenCompanyError::NotFound(format!(
                "no parked approval {id} to extend"
            )));
        }
        // Durable half: the journal both projects the new deadline (its in-memory
        // queue moved here) and replays it on the next boot.
        self.journal.record_extended(id, now, by.clone()).await?;
        // Audit half: who kept this alive, and when.
        self.events
            .append(
                &self.id,
                CompanyEvent::ApprovalExtended {
                    approval_id: id.clone(),
                    by,
                },
            )
            .await?;
        Ok(now.saturating_add(self.approval_gate.ttl_millis()))
    }

    /// How many **other** decisions the turn behind `id` is still blocked on
    /// (issue #561).
    ///
    /// The console asks so it can say what is actually about to happen. Since
    /// issue #469 a turn continues once, when the last decision it parked
    /// lands — so approving one of four tells the operator's agent nothing yet,
    /// and a confirmation reading "the agent is completing the action" is false
    /// for three of those four clicks. It was measured false for minutes at a
    /// time on staging, which is worse than silence: the operator waits for work
    /// that no decision has released.
    ///
    /// `0` means this decision releases the turn (or the turn was never gated,
    /// which continues on its own the same way).
    ///
    /// A **snapshot**, deliberately: the count is read after the verdict is
    /// durable and before the follow-up cycle decrements it, so this approval is
    /// still included and is subtracted here. A concurrent resolve on the same
    /// turn can land between the read and the render, which makes the number
    /// advisory — it is confirmation copy, not a control, and the continuation
    /// itself is decided under the queue's own lock where no such race exists.
    pub fn decisions_still_awaited(&self, id: &ApprovalId) -> usize {
        let Some(turn) = self.journal.approval_cycle(id).flatten() else {
            return 0;
        };
        self.continuations.outstanding(&turn).saturating_sub(1)
    }

    /// Resolves a parked approval to an operator-amended effect
    /// (approve-with-edit): the operator's `amended_payload` is overlaid onto
    /// the parked effect, which is then executed. Runs a follow-up cycle so the
    /// brain learns the resolution; the immutable journal records both the
    /// original (parked) and amended effects.
    ///
    /// Drop-safe on the same terms as [`resolve_approval`](Self::resolve_approval).
    pub async fn resolve_approval_amended(
        self: &Arc<Self>,
        id: &ApprovalId,
        amended_payload: serde_json::Value,
        by: Actor,
    ) -> Result<CycleReport> {
        let (_, follow_up) = self
            .resolve_approval_amended_spawned(id, amended_payload, by)
            .await?;
        join_follow_up(follow_up).await
    }

    /// The amend counterpart to
    /// [`resolve_approval_spawned`](Self::resolve_approval_spawned).
    pub async fn resolve_approval_amended_spawned(
        self: &Arc<Self>,
        id: &ApprovalId,
        amended_payload: serde_json::Value,
        by: Actor,
    ) -> Result<(ResolveReceipt, JoinHandle<Result<CycleReport>>)> {
        self.ensure_accepting()?;
        let receipt = CycleRunner::new(self)
            .settle_approval_amended(id, amended_payload, by)
            .await?;
        self.retire_if_expired(id, &receipt).await?;
        Ok((receipt.clone(), self.spawn_follow_up(receipt)))
    }

    /// Spawns the follow-up cycle a settled verdict owes, on a task that owns
    /// its own `Arc<CompanyRuntime>` and so outlives whatever asked for it.
    ///
    /// [`ResolveReceipt::AlreadyResolved`] owes no cycle, but still spawns —
    /// answering with the same synthetic report on a handle of the same shape
    /// keeps every caller on one path instead of branching on a case that only
    /// arises from a double-click.
    ///
    /// A failed follow-up is **told to the operator**, not only logged
    /// (issue #469, defect 4). It is genuinely recoverable: the verdict and the
    /// grant are already durable, and re-approving is a safe no-op
    /// (`ResolveOutcome::NotParked` → the already-resolved report, per issue
    /// #243), so the operator can retry without minting a second grant. But
    /// that is only useful to somebody who knows it happened, and before this
    /// the whole report was one `tracing::error!` on a stream nobody watches:
    /// the agent was not told the outcome and neither was the person waiting
    /// for it.
    fn spawn_follow_up(
        self: &Arc<Self>,
        receipt: ResolveReceipt,
    ) -> JoinHandle<Result<CycleReport>> {
        let rt = Arc::clone(self);
        tokio::spawn(async move {
            let event = match receipt {
                ResolveReceipt::AlreadyResolved => {
                    return Ok(CycleRunner::new(&rt).already_resolved_report());
                }
                // Issue #1449: an expiry owes no continuation *from here*.
                // `retire_approval` — already run inline by `retire_if_expired`
                // — released the turn itself, banking the expiry as the deny it
                // is. Running one here too would decide the same approval twice
                // against the continuation queue.
                ResolveReceipt::Expired => {
                    return Ok(CycleRunner::new(&rt).expired_report());
                }
                ResolveReceipt::Settled(event) => *event,
            };
            rt.continue_turn(event).await
        })
    }

    /// Durably banks a blocked-node approval the moment its verdict is known,
    /// for whichever caller reaches it first (issue #1816 / #1825).
    ///
    /// Extracted so `settle_approval` and `settle_approval_amended`
    /// (`runtime/cycle.rs`) can call it **inline, before returning the
    /// receipt** — closing a window `continue_turn`'s own call alone could
    /// not: `resolve_approval_spawned` settles the verdict durably and only
    /// then spawns the detached follow-up task that used to be the sole
    /// caller of this bank. A restart between that spawn and the task's first
    /// poll left the settle durable but the bank never run, and boot rehydrate
    /// only rearms `blocked_nodes` from `journal.blocked_node_approvals()` —
    /// so a stash whose approval never reached this call is invisible to
    /// `reconcile_stranded_blocked_nodes` and stranded exactly as before
    /// #1816's original fix, just through a different crash window.
    ///
    /// Idempotent by construction (`mark_approved` is a flag flip,
    /// `record_blocked_node_approved` is a journal-backed set insert), so
    /// every caller — the inline settle paths and `continue_turn`'s own
    /// defense-in-depth call — can run it unconditionally without needing to
    /// coordinate who "owns" the bank. A no-op for a denial, an id this
    /// journal never parked, or a turn that is not a blocked agent node's.
    ///
    /// # Why the durable write retries inline, then in the background (P1, then P2)
    ///
    /// Both inline callers reach this only after
    /// `approval_gate.resolve_outcome` has already popped `id` from the
    /// parked set (issue #243's double-submit guard) — that is what makes the
    /// call idempotent-safe rather than a second decision. It also means a
    /// re-click of "approve" on the same id short-circuits to
    /// `ResolveReceipt::AlreadyResolved` upstream and never reaches this
    /// function again: unlike `spawn_blocked_node_continuation`'s dispatch
    /// write (which a caller-visible `Err` lets `resume_blocked_agent_node`
    /// retry, because that stash and approval are still sitting there to
    /// retry from), there is no external *caller* who can retry *this* write
    /// — the operator's click already happened, and clicking again is a
    /// no-op past this point. A single failed attempt on this node's last
    /// decision is invisible to `reconcile_stranded_blocked_nodes` (see this
    /// function's doc above) and strands the grant permanently, not until the
    /// next transient blip clears — so a bounded, synchronous retry runs
    /// before this call returns to the caller, rather than warning once and
    /// moving on.
    ///
    /// That bounded loop (P1) still gives up after three quick attempts,
    /// which is exactly as blind to an outage lasting any longer as no retry
    /// at all — the caller sees success either way, since the grant and the
    /// resolved-journal line are already committed by the time this runs. P2
    /// hands an exhausted write to [`spawn_background_approval_bank_retry`](Self::spawn_background_approval_bank_retry)
    /// instead of only logging it: not a caller retrying, but this function
    /// retrying itself on borrowed time, for as long as the process backing
    /// this boot survives the outage.
    pub(crate) async fn bank_blocked_node_approval(&self, id: &ApprovalId, verdict: Verdict) {
        if verdict != Verdict::Approve {
            return;
        }
        let Some(turn) = self.journal.approval_cycle(id).flatten() else {
            return;
        };
        if !crate::runtime::workflow_resume::is_node_turn(&turn) {
            return;
        }
        self.blocked_nodes.mark_approved(&turn);
        const ATTEMPTS: u32 = 3;
        let mut last_error = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(u64::from(attempt) * 50)).await;
            }
            match self.journal.record_blocked_node_approved(&turn).await {
                Ok(()) => return,
                Err(error) => last_error = Some(error),
            }
        }
        // Every bounded, inline attempt failed. `error!`, not `warn!`: this is
        // the only synchronous record of the fact.
        if let Some(error) = last_error {
            tracing::error!(
                company = %self.id,
                %turn,
                %error,
                "[approval] a blocked node's approval could not be durably banked after \
                 retrying inline; handing the write to a background retry rather than \
                 accepting the loss"
            );
        }
        // Issue #1825 (P2 follow-up): the bounded loop above used to be where
        // this gave up — three quick attempts (max ~150ms of backoff) and then
        // only a log line, so a journal outage lasting even a moment longer
        // than that fell through as a *successful* settlement: the caller
        // above sees no error, the grant is already live, and nothing downstream
        // ever tries this write again (see the doc above `bank_blocked_node_approval`
        // — there is no retryable caller for it, unlike `spawn_blocked_node_continuation`'s
        // dispatch write). A restart landing anywhere before the live follow-up
        // releases the turn then rehydrates the stash from `blocked_stashes`
        // with `approved: false` (that record is a separate write, made earlier
        // at park time, and did land), and boot's `reconcile_stranded_blocked_nodes`
        // reads it off `stashed_turns()` as unapproved — indistinguishable from a
        // stash nobody ever decided — and retires it, discarding a real approval.
        //
        // # Why detached rather than propagated as an error
        //
        // Returning `Err` from here instead was considered and rejected. By
        // this point `ApprovalGate::resolve_outcome` (`settle_approval`,
        // `runtime/cycle.rs`) has already popped `id` from the parked set —
        // synchronously, unconditionally, on every path, not something this
        // function can gate — and `record_resolved` plus (for an approval)
        // the grant mint have already durably committed. An `Err` here would
        // misrepresent an approval that already took effect elsewhere as
        // failed, AND would abort `resolve_approval_spawned` before
        // `spawn_follow_up` runs — the continuation would then never dispatch
        // even in the ordinary same-process case, trading a rare cross-restart
        // gap for a routine same-process failure on any multi-second journal
        // hiccup. Moving this write earlier, before `resolve_outcome`, so an
        // abort *would* be clean, is not safe on its own either:
        // `resolve_outcome` is what tells an `Approve` apart from an `Expired`
        // default-deny, and writing "this node's continuation is approved"
        // before that classification risks banking a decision that turns out
        // to be a deny. Doing that safely needs a peek-then-commit split on
        // `ApprovalGate::resolve_outcome`'s pop, out of scope for this finding.
        //
        // So the accepted trade-off is a *bounded* background retry, not a
        // synchronous or an unbounded one — the same best-effort-plus-boot-
        // reconciliation pattern this feature already uses for its other
        // post-commit durable writes (`record_blocked_node_stashed`,
        // `BlockedNodeDispatched`). It does not make the crash-during-retry
        // window zero — nothing single-process can, once the decision is
        // already irreversible — it shrinks the window from "gone the
        // instant the third inline attempt fails" to "gone only if the
        // process dies during the several-second background retry", and
        // keeps the operator's HTTP response exactly as fast as before:
        // `settle_approval` has already returned by the time this task
        // starts, so nothing here adds to the caller's wait.
        self.spawn_background_approval_bank_retry(turn);
    }

    /// Keeps retrying [`RuntimeJournal::record_blocked_node_approved`] in the
    /// background after [`bank_blocked_node_approval`](Self::bank_blocked_node_approval)'s
    /// bounded inline loop exhausts (issue #1825, P2 follow-up).
    ///
    /// Detached on its own clone of the journal handle and the company id —
    /// not `Arc<Self>` — because nothing else this turn's continuation needs
    /// lives here: `mark_approved` already flipped the in-process flag this
    /// same call started with, so a same-process redemption is unaffected
    /// either way. This task's only job is to keep trying the one durable
    /// write a restart depends on, until it lands.
    ///
    /// Backs off exponentially (200ms doubling, capped at 2s) for up to 8
    /// further attempts — worst case ~11s of total backoff, not the ~80s a
    /// wider bound would allow. Deliberately tight: every second this task is
    /// still retrying is a second in which a process crash loses the
    /// approval for good (see the doc above), and a real transient blip —
    /// brief disk contention, a momentary lock — clears in low single-digit
    /// seconds, not tens of them. Widening this bound trades a smaller
    /// crash-loss window for catching a longer outage, which is not a trade
    /// this function should make silently; a store down for longer than ~11s
    /// needs an operator's attention regardless; a bounded window, not
    /// forever, so a journal that is down for good does not leak one task
    /// per stranded approval for the life of the process.
    /// `record_blocked_node_approved` is idempotent (a journal-backed set
    /// insert, per its own doc), so a write that lands after the bounded
    /// inline loop's own attempts already partially failed cannot
    /// double-record anything.
    ///
    /// # Retires a write that lands after its own stash was already released
    /// (P2, found by chatgpt-codex-connector)
    ///
    /// `bank_blocked_node_approval` runs twice per resolve — once inline from
    /// `settle_approval`, once again as `continue_turn`'s own
    /// defense-in-depth call — so an inline-exhausted write here can race a
    /// **second** background retry task for the same turn, spawned by the
    /// other call site, while the run's own dispatch (which does not wait on
    /// either) is already releasing the stash
    /// (`record_blocked_node_released`, which removes `turn` from
    /// `blocked_node_approvals` along with everything else). A retry that
    /// lands afterward re-inserts `turn` into `blocked_node_approvals` with
    /// nothing left to release it — the set is no longer idempotent with
    /// respect to its own terminal record, and a replay of this exact
    /// sequence on a future boot reaches the same state: the durable key
    /// accumulates forever, because nothing ever appends a second
    /// `BlockedNodeReleased` to retire it. This function's earlier revision
    /// tried closing the race by checking `blocked_nodes.is_armed` *before*
    /// each attempt and abandoning the retry outright — proven wrong by
    /// `a_recovered_approval_bank_failure_lands_via_the_background_retry`,
    /// which fails every one of the outaged fixture's exhausted appends
    /// specifically *because* dispatch (which never waits on this write)
    /// reliably releases the stash before the first 200ms backoff elapses;
    /// bailing there would make the retry a no-op in the exact outage it
    /// exists to recover, not only in the race this section closes. So the
    /// write still runs unconditionally — the fact is real audit history
    /// either way — and only the resurrected mirror key gets swept: on
    /// success, if the stash is no longer armed, one more
    /// `record_blocked_node_released` for the same turn is appended.
    /// `record_blocked_node_released`'s in-memory removal is already a no-op
    /// on an absent turn, and on replay this second line reorders correctly
    /// behind the stray `BlockedNodeApproved` it is retiring, so a future
    /// boot's replay ends exactly where this process does: nothing left
    /// behind.
    fn spawn_background_approval_bank_retry(&self, turn: String) {
        let journal = self.journal.clone();
        let blocked_nodes = self.blocked_nodes.clone();
        let company = self.id.clone();
        tokio::spawn(async move {
            const MAX_ATTEMPTS: u32 = 8;
            const MAX_BACKOFF_MS: u64 = 2_000;
            let mut backoff_ms: u64 = 200;
            for attempt in 0..MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                match journal.record_blocked_node_approved(&turn).await {
                    Ok(()) => {
                        tracing::info!(
                            company = %company,
                            %turn,
                            attempt,
                            "[approval] a blocked node's approval bank landed on a background \
                             retry, after the inline bounded loop exhausted"
                        );
                        if !blocked_nodes.is_armed(&turn)
                            && let Err(error) = journal.record_blocked_node_released(&turn).await
                        {
                            tracing::warn!(
                                company = %company,
                                %turn,
                                %error,
                                "[approval] this write landed after its own stash was already \
                                 released by another retry or the inline attempt; retiring the \
                                 resurrected key failed, so a stale entry may linger in \
                                 blocked_node_approvals until a manual sweep"
                            );
                        }
                        return;
                    }
                    Err(error) => {
                        backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                        tracing::warn!(
                            company = %company,
                            %turn,
                            attempt,
                            %error,
                            "[approval] background retry of a blocked node's approval bank \
                             failed again"
                        );
                    }
                }
            }
            // Issue #1825 (P2 follow-up): every extended attempt failed too.
            // This is now the loudest record there is — the grant is live, the
            // decision is durable in the audit trail, but this specific
            // `BlockedNodeApproved` fact never reached the journal, so a
            // restart from here on will rehydrate this stash unapproved and
            // `reconcile_stranded_blocked_nodes` will retire it. There is no
            // further retry left on this boot; recovering from this point on
            // needs an operator to re-run the workflow.
            tracing::error!(
                company = %company,
                %turn,
                attempts = MAX_ATTEMPTS,
                "[approval] a blocked node's approval could not be durably banked after an \
                 extended background retry; a restart from here will strand this grant with \
                 no further automatic recovery — the workflow needs a manual re-run"
            );
        });
    }

    /// Runs the continuation a settled verdict owes — **once per turn, not once
    /// per approval** (issue #469).
    ///
    /// A turn that parked four calls is blocked on four decisions. Before this,
    /// each decision spawned its own cycle, so approving all four re-ran the
    /// same turn four times: four full agent turns over one turn's work, each
    /// told about one decision and blind to the other three, with the later ones
    /// finding the grants the earlier ones had already redeemed and quietly
    /// producing nothing. The operator approved four times and got silence.
    ///
    /// So the decision is banked instead, and the cycle runs when the **last**
    /// one lands, carrying every `ApprovalResolved` the turn accumulated. The
    /// trigger is the last decision rather than a window, which is what makes
    /// approving four at once and approving them one at a time over a minute end
    /// in the same place. An approval whose journal line predates the turn key
    /// is not gated and continues on its own, exactly as it used to.
    async fn continue_turn(&self, event: CompanyEvent) -> Result<CycleReport> {
        let CompanyEvent::ApprovalResolved { approval_id, .. } = &event else {
            // Not a resolution, so no turn owns it. Run it as its own cycle.
            return CycleRunner::new(self).run(vec![event]).await;
        };
        let approval_id = approval_id.clone();
        let verdict = match &event {
            CompanyEvent::ApprovalResolved { verdict, .. } => *verdict,
            _ => unreachable!("matched ApprovalResolved above"),
        };

        // `Some(None)` is a park recorded before the turn key existed; `None` is
        // an id this journal never parked. Neither is gated.
        let turn = self.journal.approval_cycle(&approval_id).flatten();
        // Issue #978: bank the verdict against the run's gate batch BEFORE the
        // continuation queue is told, so that whichever decision turns out to be
        // the last finds every sibling's verdict already recorded. Ordering is
        // what makes that safe rather than lucky: each caller banks then counts,
        // and the release is handed to the caller whose count reaches zero — by
        // which time the other N-1 have necessarily banked. A no-op for a turn
        // that is not a workflow run.
        if let Some(turn) = turn.as_deref() {
            self.workflow_gates.decide(turn, &approval_id, verdict);
        }
        // Issue #1816/#1825: this is the second place a blocked-node approval
        // gets banked — `settle_approval`/`settle_approval_amended` (issue
        // #1825) already did it inline, durably, before this detached follow-up
        // task was even spawned. Calling it again here is a deliberate,
        // harmless no-op (`mark_approved` and `record_blocked_node_approved`
        // are both idempotent) kept as defense-in-depth for exactly the
        // scenario the first bank exists to close: a crash between the settle
        // returning and this task's first poll would otherwise leave nobody
        // having banked the decision at all.
        self.bank_blocked_node_approval(&approval_id, verdict).await;
        let batch = match &turn {
            Some(turn) => match self.continuations.decide(turn, Some(event)) {
                Some(batch) => batch,
                None => {
                    tracing::debug!(
                        company = %self.id,
                        approval_id = %approval_id,
                        turn = %turn,
                        outstanding = self.continuations.outstanding(turn),
                        "[approval] decision recorded; the turn is still waiting on another"
                    );
                    return Ok(self.still_waiting_report(turn));
                }
            },
            None => vec![event],
        };
        // Issue #978: a workflow run is not a brain turn, so it is not continued
        // like one. The fork is read off the turn key itself — see
        // `continuation_target` — rather than from a side lookup that could
        // disagree with the key the park wrote.
        if let Some(turn) = turn.as_deref()
            && crate::runtime::workflow_resume::run_id_from_turn(turn).is_some()
        {
            return self.resume_workflow_run(&approval_id, turn, batch).await;
        }
        // An explicit question raised inside a workflow agent node is still a
        // conversation continuation for that agent, not authority to replay the
        // workflow node. The ordinary blocked-node path intentionally drops an
        // all-denied batch; doing that here would swallow the operator's answer
        // and leave the durable ApprovalContinuation live until expiry. The
        // request tool is a turn boundary, so this batch is all-explicit by
        // construction; keep the `all` guard fail-closed if a legacy mixed
        // batch is ever replayed.
        if let Some(turn) = turn.as_deref()
            && crate::runtime::workflow_resume::is_node_turn(turn)
            && batch.iter().all(|event| {
                let CompanyEvent::ApprovalResolved { approval_id, .. } = event else {
                    return false;
                };
                self.grants.peek_continuation(approval_id).is_some()
            })
        {
            self.retire_blocked_stash(turn).await;
            return self.run_continuation(&approval_id, batch).await;
        }
        // Issue #899 (Stage 1): a blocked agent node, likewise not a brain turn.
        // Its gated calls parked under a `workflow-node:` key (disjoint from the
        // `workflow-run:` gate key above), so the same batch counting releases
        // them together, and this re-dispatches the run once — the auto-continue
        // that used to be missing. Deny/expire-only spawns nothing.
        if let Some(turn) = turn.as_deref()
            && crate::runtime::workflow_resume::is_node_turn(turn)
        {
            return self
                .resume_blocked_agent_node(&approval_id, turn, batch)
                .await;
        }
        if batch.is_empty() {
            // Every approval the turn raised expired rather than being decided.
            // The sweep already appended each `ApprovalResolved` itself, so
            // there is nothing left to tell the brain.
            return Ok(CycleRunner::new(self).already_resolved_report());
        }
        self.run_continuation(&approval_id, batch).await
    }

    /// Re-dispatches the workflow run a released batch belongs to — **once**
    /// (issue #978).
    ///
    /// The workflow arm of [`continue_turn`](Self::continue_turn), and the
    /// counterpart to [`run_continuation`](Self::run_continuation): a run has no
    /// agent turn to resume, so there is no cycle to run. What it owes is one
    /// replay of the graph carrying every gate the batch approved.
    ///
    /// The decisions are still appended to the event log, because they happened
    /// and the timeline should say so; what is deliberately **not** run is a
    /// brain cycle per decision. Before this, every workflow gate approval spent
    /// a full agent turn telling the brain about a resolution it can do nothing
    /// with, on top of the duplicate run it started.
    ///
    /// A refused spawn is announced rather than only logged. Issue #401's
    /// concurrency ceiling was survivable when each approval had its own spawn
    /// attempt — one refusal left the other cards to retry with. A batch gets one
    /// attempt and consumes every card, so a silent refusal loses the run with
    /// nothing left to click; the operator has to be told to re-run it. Same
    /// stance as issue #469 defect 4.
    async fn resume_workflow_run(
        &self,
        approval_id: &ApprovalId,
        turn: &str,
        batch: Vec<CompanyEvent>,
    ) -> Result<CycleReport> {
        for event in batch {
            if let Err(error) = self.events.append(&self.id, event).await {
                tracing::warn!(
                    company = %self.id,
                    %approval_id,
                    %error,
                    "[approval] a workflow gate's resolution could not be appended to the event \
                     log; the journal remains the binding record"
                );
            }
        }
        if let Err(error) = crate::runtime::workflow_resume::resume_run(self, turn).await {
            tracing::error!(
                company = %self.id,
                %turn,
                %error,
                "[approval] the workflow run released by this decision could not be continued"
            );
            self.announce_to_operator(&format!(
                "Every sign-off on that workflow step is in, but the run could not be \
                 restarted: {error}. Nothing else is waiting on you — re-run the workflow to \
                 pick it back up."
            ))
            .await;
            return Err(error);
        }
        Ok(CycleRunner::new(self).already_resolved_report())
    }

    /// Re-dispatches the run a **blocked agent node** belonged to — once, when
    /// its gated calls are all decided and at least one was approved (issue #899,
    /// Stage 1).
    ///
    /// The agent-node counterpart to
    /// [`resume_workflow_run`](Self::resume_workflow_run). The difference is what
    /// a continuation needs: a gate threads its node id into the trigger's
    /// `approvals` array, but a call gated *inside* an agent node's tool loop is
    /// not a graph node — the re-run just runs the graph again, and the grant the
    /// approve minted (a shared [`GrantSet`](crate::runtime::grants::GrantSet))
    /// lets the identical call pass. So this spawns from the stashed workflow id
    /// and trigger input, unchanged.
    ///
    /// Three outcomes, all ending with the decisions appended to the timeline:
    ///
    /// * **at least one approved** — spawn one continuation run. A diverging
    ///   re-run may re-ask (Stage 2 closes that); a failed spawn is announced,
    ///   not swallowed, on [`resume_workflow_run`](Self::resume_workflow_run)'s
    ///   reasoning — the cards are already consumed.
    /// * **all denied or expired** — spawn nothing. The block is final; there is
    ///   nothing to continue, exactly as `resume_run` starts no run for a wholly
    ///   refused batch.
    /// * **approved but the stash is gone** — the last-resort branch. Since
    ///   issue #1816 a restart no longer lands here: the workflow id and trigger
    ///   input are stashed durably at park time and the boot builder re-arms the
    ///   [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue)
    ///   from them, so `release` above finds the run. This branch remains only for
    ///   the genuine no-lineage case (a park whose durable stash write also
    ///   failed, or one written before #1816): the operator is told to re-run
    ///   rather than left waiting.
    async fn resume_blocked_agent_node(
        &self,
        approval_id: &ApprovalId,
        turn: &str,
        batch: Vec<CompanyEvent>,
    ) -> Result<CycleReport> {
        // Issue #1816: the released batch only names the verdicts this process
        // held in memory, which a restart between two decisions on the same
        // node can leave short of an earlier approve. `stashed.approved` (below)
        // is the durable backstop for exactly that gap — read alongside this,
        // not instead of it, since the common in-process case never needs it.
        let batch_approved = batch.iter().any(|event| {
            matches!(
                event,
                CompanyEvent::ApprovalResolved {
                    verdict: Verdict::Approve,
                    ..
                }
            )
        });
        for event in batch {
            if let Err(error) = self.events.append(&self.id, event).await {
                tracing::warn!(
                    company = %self.id,
                    %approval_id,
                    %error,
                    "[approval] a blocked node's resolution could not be appended to the event \
                     log; the journal remains the binding record"
                );
            }
        }
        // Issue #1816 (Stage 4): read the stash without taking it yet. Retiring
        // it — both the in-memory fast path and the durable journal record
        // beneath it — used to happen right here, unconditionally, before the
        // spawn attempt below was even made. A crash strictly during that
        // awaited spawn then left the release already recorded and the stash
        // already gone, so a restart rehydrated neither: no pending decision
        // (already resolved), no stash (already released), nothing left to
        // retry — exactly the stranding this queue exists to prevent, just
        // moved one step later. Each branch below now calls
        // `retire_blocked_stash` itself, only once its own outcome (spawned,
        // refused, or genuinely un-continuable) is actually final.
        let stashed = self.blocked_nodes.peek(turn);
        // The stash's own flag (banked at decide time, and rehydrated across a
        // restart alongside the stash itself) carries an earlier approve the
        // batch above may have lost. Either source is enough — the point is
        // that a genuine approve on this node is never overruled by what this
        // particular process happened to still be holding.
        let approved = batch_approved || stashed.as_ref().is_some_and(|s| s.approved);
        if !approved {
            tracing::info!(
                company = %self.id,
                %turn,
                "[approval] every gated call on this blocked node was refused or expired, so no \
                 continuation runs"
            );
            self.retire_blocked_stash(turn).await;
            return Ok(CycleRunner::new(self).already_resolved_report());
        }
        let Some(stashed) = stashed else {
            tracing::error!(
                company = %self.id,
                %turn,
                "[approval] a blocked node's calls were approved, but this host no longer holds \
                 the run's stash (a restart drops it), so there is nothing to continue"
            );
            self.announce_to_operator(
                "That workflow step's approval is in, but this host no longer has the run to \
                 continue — re-run the workflow to pick it back up.",
            )
            .await;
            // Nothing was in the queue to take, but the durable side may still
            // hold a record naming this turn (the genuine no-lineage case is
            // driven by the in-memory queue being empty, not necessarily the
            // journal) — retire it so a later boot does not rehydrate it.
            self.retire_blocked_stash(turn).await;
            return Ok(CycleRunner::new(self).already_resolved_report());
        };
        // Issue #1825 (finding `3877718169`, chatgpt-codex-connector): a ghost
        // decision reaching this **live** path must not repeat a dispatch
        // that already landed. `reconcile_stranded_blocked_nodes` only ever
        // calls this function once its own `already_dispatched` check has
        // already ruled that out — but that check runs once per boot, over
        // turns with nothing left parked. `continue_turn` routes here off
        // nothing but a turn key and a fresh `ApprovalResolved`, with no such
        // filter, and a ghost card supplies that event exactly as faithfully
        // as a genuine one.
        //
        // `ApprovalResolved` is `Durability::Process` by design —
        // `journal.rs`'s own doc on it: "a ghost approval that is approved a
        // second time cannot duplicate the effect, because the effect's own
        // commit is host-durable and `is_executed` skips it." True for the
        // gated call's own effect, replayed through the same park. Not true
        // for this node's *continuation dispatch*, which sits behind no such
        // guard — `spawn_blocked_node_continuation` below launches on nothing
        // but `stashed.is_some() && approved`. A host crash that loses only
        // the resolution leaves the card's own `ApprovalParked` line intact
        // (`Durability::Host`, same tier as `BlockedNodeApproved`/
        // `BlockedNodeDispatched`) — visible to the operator and decidable
        // again — while `reconcile_stranded_blocked_nodes`, seeing the turn
        // still parked, reads that as "waiting on a sibling decision" and
        // deliberately leaves it alone (see that function's own `continue`
        // above `already_dispatched`'s check). So the operator's second
        // click on the reopened card is the only thing left standing between
        // an already-dispatched continuation and a duplicate one: same model
        // spend, same external side effects, run twice.
        if self.journal.is_blocked_node_dispatched(turn) {
            tracing::warn!(
                company = %self.id,
                %turn,
                "[approval] a decision landed on a blocked node whose continuation was \
                 already dispatched; recording it and retiring the stash without launching \
                 a second continuation"
            );
            self.retire_blocked_stash(turn).await;
            return Ok(CycleRunner::new(self).already_resolved_report());
        }
        match crate::runtime::workflow_resume::spawn_blocked_node_continuation(
            self,
            turn,
            &stashed.workflow_id,
            stashed.input,
            stashed.started_by,
        )
        .await
        {
            Ok(()) => {
                // Issue #1825: `spawn_blocked_node_continuation` itself banks
                // `BlockedNodeDispatched` now, between admitting the run and
                // launching its detached task — see that function's doc for
                // why the marker moved off this side of the call. `Ok(())`
                // only reaches here once that write has actually landed (a
                // P1 follow-up made a failed write abort the launch and
                // propagate instead of warning and proceeding unmarked — see
                // that call site), so by the time this arm runs the dispatch
                // is durable *and* the launch happened; nothing left to do on
                // that front. Only now — spawn has actually taken hold — is
                // the stash truly spent. Retiring it here rather than up
                // front is what lets a crash mid-spawn rehydrate the very
                // stash it needs to retry from, instead of finding both
                // halves already gone.
                self.retire_blocked_stash(turn).await;
                Ok(CycleRunner::new(self).already_resolved_report())
            }
            Err(error) => {
                tracing::error!(
                    company = %self.id,
                    %turn,
                    %error,
                    "[approval] the workflow run released by a blocked node's approval could \
                     not be continued"
                );
                // Issue #1825 (P2 follow-up): a refusal at the concurrency
                // ceiling (or, per `spawn_blocked_node_continuation`, any
                // other failure reached before `RunSupervisor::begin` even
                // ran, e.g. a transient store read) means nothing was
                // admitted and nothing was marked dispatched — the stash and
                // its approval are exactly as durably recoverable as they
                // were before this attempt. Retiring them here would discard
                // an approval with real durable state still able to resume
                // it, leaving nothing to redeem it once capacity frees up.
                // Keep it stashed and approved so a later boot's
                // `reconcile_stranded_blocked_nodes` finds it and tries
                // again, exactly as if this attempt had never run.
                if Self::is_retryable_dispatch_failure(&error) {
                    // CodeRabbit (review 5038258829): the only retry path for
                    // a kept stash is `reconcile_stranded_blocked_nodes`, which
                    // runs once per boot (`RuntimeBuilder::build`, gated on
                    // `handover.is_none()`) — not an in-process retry inside
                    // minutes, which "will retry it automatically" led an
                    // operator to expect.
                    self.announce_to_operator(&format!(
                        "That workflow step's approval is in, but the run could not start \
                         right now: {error}. The approval stays recorded and is picked back \
                         up automatically the next time this company starts."
                    ))
                    .await;
                } else {
                    self.announce_to_operator(&format!(
                        "That workflow step's approval is in, but the run could not be \
                         restarted: {error}. Nothing else is waiting on you — re-run the \
                         workflow to pick it back up."
                    ))
                    .await;
                    // A handled, permanent, in-process failure (as opposed to
                    // a crash, and as opposed to a retryable refusal above) —
                    // the operator has already been told to re-run manually,
                    // so the stash is spent exactly as it was on `main`.
                    self.retire_blocked_stash(turn).await;
                }
                Err(error)
            }
        }
    }

    /// Whether a [`spawn_blocked_node_continuation`](crate::runtime::workflow_resume::spawn_blocked_node_continuation)
    /// failure means nothing was admitted, so the stash it failed to dispatch
    /// is still worth keeping for a later attempt (issue #1825, P2 follow-up).
    ///
    /// [`OpenCompanyError::WorkflowRunLimit`] is the reconciliation-specific
    /// case the finding names directly: the boot reconciler can rehydrate
    /// more approved stashes than `[workflows].max_in_flight_runs` admits at
    /// once, and every one past the ceiling must survive to be retried once
    /// capacity frees up rather than being discarded on the first refusal.
    /// [`OpenCompanyError::Store`] and [`OpenCompanyError::StoreIo`] are the
    /// other case the finding names — `spawn_blocked_node_continuation`'s
    /// `store().load(...)` for overlay workflows can fail on a host hiccup
    /// with nothing wrong with the approval or the graph it names. A P1
    /// follow-up added a second source of the same two variants: a failed
    /// `record_blocked_node_dispatched` write now aborts the launch instead
    /// of warning and proceeding unmarked, so that failure reaches here too
    /// — `begin` already admitted (and this arm's guard already dropped,
    /// freeing the slot) but nothing launched, which is exactly the shape
    /// this function exists to keep retryable.
    ///
    /// Every other variant reaching this call site — `CompanyNotFound` (the
    /// graph was deleted) or `InvalidRequest` (no workflow runner wired) — is
    /// a fact about the company that a retry cannot change, so those stay
    /// permanent: retire the stash and tell the operator to re-run by hand,
    /// exactly as `main` already does for them.
    fn is_retryable_dispatch_failure(error: &OpenCompanyError) -> bool {
        matches!(
            error,
            OpenCompanyError::WorkflowRunLimit { .. }
                | OpenCompanyError::Store(_)
                | OpenCompanyError::StoreIo { .. }
        )
    }

    /// Retires a blocked-node stash — the in-memory fast path and the durable
    /// journal record beneath it — once its outcome is truly final (issue
    /// #1816, Stage 4).
    ///
    /// Split out of [`resume_blocked_agent_node`](Self::resume_blocked_agent_node)
    /// so every one of its terminal branches retires the stash at the same,
    /// late point rather than up front: see that function's doc comment for
    /// why firing this before the spawn attempt is the gap this exists to
    /// close. Best-effort on the durable clear, matching the park's own
    /// stance — the in-memory drop is what this cycle acts on, and a lost
    /// release record at worst rehydrates a stash whose approvals are already
    /// resolved, which no resolve event will ever release again.
    async fn retire_blocked_stash(&self, turn: &str) {
        self.blocked_nodes.release(turn);
        if let Err(error) = self.journal.record_blocked_node_released(turn).await {
            tracing::warn!(
                company = %self.id,
                %turn,
                %error,
                "[approval] a blocked node's durable stash could not be retired; a boot may \
                 rehydrate an already-resolved block. `reconcile_stranded_blocked_nodes` won't \
                 re-dispatch it a second time when this call reached the spawn (issue #1825's \
                 `BlockedNodeDispatched` survives this write's failure), but for the \
                 all-denied/no-stash callers this remains a genuinely stale record: harmless, \
                 not re-released, just left sitting in the journal until a future replay"
            );
        }
    }

    /// Boot-time reconciliation for issue #1816's narrowest gap (Stage 3): a
    /// restart landing between the durable approval bank
    /// (`record_blocked_node_approved`) and the in-memory decision that would
    /// have released the block (`ContinuationQueue::decide`) rehydrates an
    /// approved stash that nothing then triggers.
    ///
    /// `record_blocked_node_approved` exists precisely to survive a restart
    /// that lands on a decision that is *not* the turn's last (its own doc
    /// comment). What it does not cover on its own is the case where the
    /// crash lands on the decision that *was* the last one: the journal's
    /// `parked_turns()` has already dropped every approval for this turn —
    /// there was nothing left to be parked — so the boot rearm gives
    /// `ContinuationQueue` nothing to fire on, and only a *future* decision on
    /// the same turn used to notice the gap. A node whose last call is also
    /// its only (or final) call has no future decision coming, so the run
    /// sits stranded rather than resuming the moment the operator's approval
    /// — which they already gave — should have redeemed it.
    ///
    /// Run once per boot, after every other queue is rearmed: a turn whose
    /// stash is durably marked `approved` and has nothing left parked in the
    /// journal is exactly that stranded case, resumed the same way a live
    /// release would resume it, with an empty batch — its decision is already
    /// durable in the event log and owes nothing further there.
    ///
    /// Excludes turns already durably marked `BlockedNodeDispatched` (issue
    /// #1825): that pair of facts — approved, nothing left parked — also
    /// describes a stash that *was* already resumed once, if the
    /// `resume_blocked_agent_node` call that did it got as far as spawning
    /// the continuation but then lost its `BlockedNodeReleased` write to a
    /// transient failure. Without the dispatched check this function cannot
    /// tell that case apart from a genuine strand and would re-dispatch a
    /// continuation that already ran.
    ///
    /// # Unapproved stashes (issue #1825, P2 follow-up)
    ///
    /// This used to scan only [`approved_turns`](crate::runtime::blocked_nodes::BlockedNodeQueue::approved_turns),
    /// on the reasoning that an unapproved turn has nothing worth resuming —
    /// true, but incomplete: `resume_blocked_agent_node`'s own all-denied
    /// branch retires a resolved-with-no-approval stash the moment it sees
    /// one *live*, and a restart landing between that resolution and the
    /// retirement it owes (or a retirement whose durable write itself fails)
    /// strands the identical shape this function exists to clean up — just
    /// unapproved instead of approved. `approved_turns` cannot see it, so on
    /// `main` it rehydrates on every boot's rearm and is never retired: one
    /// stale stash held in memory (and in the durable journal beneath it) per
    /// restart that races this window, accumulating indefinitely. Scanning
    /// [`stashed_turns`](crate::runtime::blocked_nodes::BlockedNodeQueue::stashed_turns)
    /// instead and branching on the stash's own `approved` flag lets this
    /// function retire that case the same way the live path does, rather than
    /// only ever dispatching.
    pub(crate) async fn reconcile_stranded_blocked_nodes(&self) {
        let still_parked: std::collections::HashSet<String> =
            self.journal.parked_turns().into_iter().collect();
        // Issue #1825: a turn already durably marked dispatched has already
        // been resumed once — it is not stranded, it is a `BlockedNodeStashed`
        // + `BlockedNodeApproved` pair whose paired `BlockedNodeReleased`
        // write failed after the spawn it retires had already succeeded.
        // Re-dispatching it here would spawn the same continuation a second
        // time. See `BlockedNodeDispatched`'s doc comment for the full window
        // this closes.
        let already_dispatched: std::collections::HashSet<String> =
            self.journal.blocked_node_dispatched().into_iter().collect();
        for turn in self.blocked_nodes.stashed_turns() {
            if still_parked.contains(&turn) {
                // Still waiting on a sibling decision — not stranded, just
                // mid-turn; the eventual last decision will release it.
                continue;
            }
            // Issue #1825 (P2 follow-up): nothing left parked and never
            // approved is the same all-denied/expired shape
            // `resume_blocked_agent_node`'s own no-approval branch retires
            // the moment it sees it live — just reached here because the
            // crash landed before that retirement (or its durable write)
            // could run. There is no approval to redeem, only a stash to
            // stop holding; retire it the same way that branch does and move
            // on, without touching the dispatched check below, which exists
            // solely to guard the *approved* replay path.
            if !self
                .blocked_nodes
                .peek(&turn)
                .is_some_and(|stashed| stashed.approved)
            {
                tracing::info!(
                    company = %self.id,
                    %turn,
                    "[approval] a restart stranded a blocked node whose last decision resolved \
                     with nothing approved, before its retirement could run; retiring the stash \
                     now instead of leaving it to rehydrate on every future boot"
                );
                self.retire_blocked_stash(&turn).await;
                continue;
            }
            if already_dispatched.contains(&turn) {
                tracing::warn!(
                    company = %self.id,
                    %turn,
                    "[approval] a blocked node's continuation was already dispatched before a \
                     restart, but its retirement never durably landed; skipping a second \
                     dispatch and retrying the retirement instead"
                );
                // Best-effort retry of the write that failed last time — a
                // second attempt on a fresh boot has every chance of a
                // transient failure (disk pressure, a mid-roll host) having
                // cleared. If it fails again, this boot's warning fires once
                // more next restart, which is a stale-record annoyance, not a
                // repeat of the double-dispatch this branch exists to avoid.
                self.retire_blocked_stash(&turn).await;
                continue;
            }
            let placeholder = ApprovalId::new(format!("boot-reconcile:{turn}"));
            if let Err(error) = self
                .resume_blocked_agent_node(&placeholder, &turn, Vec::new())
                .await
            {
                tracing::error!(
                    company = %self.id,
                    %turn,
                    %error,
                    "[approval] a blocked node stranded by a restart between its last \
                     approval and its release could not be resumed at boot"
                );
            }
        }
    }

    /// Runs one turn's continuation over the decisions it was blocked on, and
    /// makes sure its answer — or its failure — reaches the operator
    /// (issue #469).
    ///
    /// Split from [`continue_turn`](Self::continue_turn) because the release can
    /// also come from the TTL sweep, and a turn released by an expiry owes
    /// exactly the same continuation, delivered exactly the same way, as one
    /// released by the operator's last click.
    async fn run_continuation(
        &self,
        approval_id: &ApprovalId,
        batch: Vec<CompanyEvent>,
    ) -> Result<CycleReport> {
        let claims = batch
            .iter()
            .filter_map(|event| match event {
                CompanyEvent::ApprovalResolved { approval_id, .. } => {
                    self.grants.peek_continuation(approval_id)
                }
                _ => None,
            })
            .collect();
        match CycleRunner::new(self).run_continuation(batch, claims).await {
            Ok(mut report) => {
                self.publish_continuation(approval_id, &mut report).await;
                Ok(report)
            }
            Err(error) => {
                tracing::error!(
                    company = %self.id,
                    %error,
                    "[approval] the follow-up cycle after a resolved approval failed; \
                     the verdict and the grant are already durable, so the agent was \
                     not told the outcome — re-approving is a safe no-op and will \
                     re-run it"
                );
                self.announce_continuation_failure(approval_id).await;
                Err(error)
            }
        }
    }

    /// Journals the continuation's replies into the conversation the sign-off
    /// was asked in, so the agent's answer actually reaches the operator
    /// (issue #469, defect 1).
    ///
    /// **This is where the answer used to be lost.** The chat route journals
    /// every reply a cycle produces as an
    /// [`AgentReply`](CompanyEvent::AgentReply), which is what the console's
    /// event stream projects as an `agent_reply` frame and what a reload
    /// rebuilds the transcript from. The resolve route never did. It emitted
    /// webhooks and, on the un-detached path, handed the replies back on the
    /// response body — but the console's inline approval card resolves with
    /// `detach: true`, so the body is a receipt and the replies went nowhere at
    /// all. Nothing was broken about the cycle; its answer simply had no way to
    /// become visible.
    ///
    /// The thread is the one the approval was **raised** in, not the answering
    /// agent: a desk channel's request and a direct message to that channel's
    /// lead are answered by the same teammate, so keying on the agent delivers a
    /// channel's continuation into a private line nobody is watching
    /// (issue #379's lesson, applied to the reply as well as the re-park). Every
    /// approval in a batch came from one cycle, and that cycle had one thread,
    /// so one lookup answers for the whole batch. Falling back to the responding
    /// agent when the turn had no conversation behind it is the pre-existing
    /// behaviour for exactly the cases it was already right for.
    ///
    /// Best-effort per reply, exactly as on the chat route: a journal failure
    /// must not sink an answer the operator can already read on the response
    /// body. It costs the bubble its durable id, which the console reads as "not
    /// saved" and refuses to thread or react on — the honest degradation.
    ///
    /// **And the thread within that channel** (issue #435). A channel was the
    /// finest conversation that existed when the above was written; threads are
    /// persisted now, so answering into the channel alone drops a threaded
    /// conversation's own conclusion out of it. The continuation is parented to
    /// the same root the question hung off — the sibling rule the chat route
    /// already follows for an ordinary answer (issue #364) — so it renders in
    /// the thread rather than flat in the channel.
    ///
    /// A parent that no longer resolves **degrades to the channel** rather than
    /// being dropped: see [`resolvable_parent`](Self::resolvable_parent) for
    /// why that guard is load-bearing rather than defensive.
    /// A recorded thread root, but only if it still resolves to a message in
    /// `chat_id` (issue #435). `None` otherwise, which answers in the channel.
    ///
    /// **Why this is not defensive coding.** The console folds a transcript
    /// exactly one level deep and *drops* a reply whose parent it cannot find
    /// in the channel — it does not fall back to rendering it flat. So a stale
    /// parent here does not produce a slightly-misplaced bubble; it produces a
    /// continuation that renders nowhere at all. That is strictly worse than
    /// the bug this issue fixes, because today's answer at least reaches the
    /// channel. The issue names the requirement directly: a remembered parent
    /// that no longer resolves degrades to the channel rather than being
    /// dropped.
    ///
    /// Two ways it fails to resolve, and both must degrade:
    ///
    /// * **Gone.** [`read_from`](crate::ports::events::EventLog::read_from)
    ///   returns events with sequence `>= seq`, so a pruned root comes back as
    ///   whatever followed it. Comparing the returned sequence to the one asked
    ///   for is what tells "found it" from "found its successor" — without that
    ///   check a pruned root silently reparents the answer onto an unrelated
    ///   message.
    /// * **Elsewhere.** A root that resolves but lives in another channel is
    ///   just as unrenderable, and the mismatch means the recorded pair was
    ///   already inconsistent. Both are the same fact to the reader.
    ///
    /// One event read, only when a parent was recorded — a threaded approval,
    /// not the common case. A read failure degrades to the channel too: the
    /// answer must not be sunk by a lookup.
    async fn resolvable_parent(&self, parent: Option<EventSeq>, chat_id: &str) -> Option<EventSeq> {
        let parent = parent?;
        let stored = self.events.read_from(&self.id, parent, 1).await.ok()?;
        let stored = stored.into_iter().next()?;
        // `>= seq`, so an exact match is the only proof the root itself is
        // still there rather than its successor.
        if stored.seq != parent {
            return None;
        }
        let channel = match &stored.event {
            CompanyEvent::OperatorMessage { chat, .. } => chat.clone(),
            CompanyEvent::AgentReply { chat_id, .. } => Some(chat_id.clone()),
            // Only a chat message can root a thread; anything else at that
            // sequence means the recorded parent was never a valid root.
            _ => return None,
        };
        // Compared through the same rule the console renders by
        // ([`same_conversation`](crate::server::chat_history::same_conversation)),
        // never as raw strings. The General desk has four spellings — `None`
        // from an unaddressed chat post, `""` from older events, the console's
        // `"main"`, and `"General"` itself — and a raw compare rejects the pair
        // it is *most* likely to be handed: an unaddressed message is journaled
        // with `chat: None` and rendered under General, so a reply to it arrives
        // here as `None` vs `"General"`. That mismatch dropped the parent and
        // resumed in the channel — issue #435's own symptom, surviving inside
        // its fix.
        crate::server::chat_history::same_conversation(channel.as_deref(), Some(chat_id))
            .then_some(parent)
    }

    /// The console channel id a mention in `desk` belongs to.
    ///
    /// A desk channel's id is its own thread id, so the context is the desk id
    /// unchanged. A DM's thread id is the bare roster teammate id, while the
    /// console's channel id for the same DM is `dm:<teammate-id>` — and the
    /// console addresses a DM with that bare id (ChatView sends
    /// `active.member.id`). So a mention in a DM has to be re-keyed into the
    /// console's channel-id space or the rail has no row to badge, and opening
    /// the DM can never match or clear the notification.
    ///
    /// The roster check goes through [`crate::runtime::assignee::resolve`] for
    /// its desk-first ordering: the same one `responder_for` uses, so a desk
    /// whose id happens to match a teammate id still stores the desk id, and a
    /// desk literally named `dm:<…>` keeps that id instead of being displaced
    /// by the `dm:`-stripped retry. The human user directory is deliberately
    /// consulted **only** when the store will not answer — never ahead of that
    /// resolution, or a desk id matching a human id would be misclassified as
    /// `dm:<id>`. The resolution carries the **canonical** id (issue #214), so
    /// a key typed as a display name — `chat: "Engineering"` for a desk whose
    /// id is `engineering` — stores the canonical id, which is what the rail's
    /// channel ids are built from. A `dm:`-prefixed key is tried **as sent**
    /// first and only split for the retry when it names nothing — so a
    /// noncanonical address — `dm:BACKEND_ENGINEER`, `dm:<display name>` —
    /// still stores `dm:<canonical-agent-id>` and badges the rail's real DM
    /// channel rather than one that does not exist.
    pub(crate) async fn mention_context(
        &self,
        id: &CompanyId,
        users: &[crate::ports::users::UserRecord],
        desk: &str,
    ) -> String {
        // The key is tried **as sent** first, exactly as the routing does: a
        // desk or teammate literally named `dm:x` resolves today, and an
        // unconditional prefix-strip would let `dm:x` claim it
        // ([`crate::runtime::assignee::dm_key`] documents that ordering). The
        // stripped retry below is only for a `dm:`-prefixed key that names
        // nothing as sent.
        let Ok(Some(record)) = self.store().load(id).await else {
            // Store will not answer; best-effort, same as the callers. A
            // canonical `dm:<teammate-id>` still badges through the raw key,
            // and a *noncanonical* roster key is re-keyed through the
            // directory. This runs only on the store-down path, never ahead of
            // `assignee::resolve`: a desk id that happens to match a human id
            // must still file under the desk when the store answers, or a
            // mention aimed at that desk would badge a nonexistent `dm:<id>`
            // channel.
            if users.iter().any(|u| u.id == desk) {
                return format!("dm:{desk}");
            }
            if let Some(bare) = crate::runtime::assignee::dm_key(desk)
                && users.iter().any(|u| u.id == bare)
            {
                return format!("dm:{bare}");
            }
            return desk.to_string();
        };
        let bare = crate::runtime::assignee::dm_key(desk);
        match crate::runtime::assignee::resolve(&record, desk) {
            // A bare teammate key files under the console's DM channel id,
            // canonicalized (issue #214) — as does a teammate literally named
            // `dm:<…>`, whose DM channel id is `dm:dm:<…>` in the same space.
            crate::runtime::assignee::AssigneeResolution::Agent(agent) => format!("dm:{agent}"),
            // A desk with no member to work it is still a real desk with a real
            // rail channel, so it files under the same canonical id as one with
            // a lead — a memberless `"Sales"` still has to badge `#sales`.
            crate::runtime::assignee::AssigneeResolution::Desk { desk: desk_id, .. }
            | crate::runtime::assignee::AssigneeResolution::EmptyDesk(desk_id) => desk_id,
            // Unassigned, unknown, or ambiguous. A `dm:`-prefixed key that
            // names nothing as sent can still be the console's DM channel for a
            // *noncanonical* address — `dm:BACKEND_ENGINEER`,
            // `dm:<display name>` — which the routing resolves
            // case-insensitively, so the stored context has to carry the
            // canonical agent id the rail's channel ids are keyed by. Storing
            // the raw key files the badge under a channel that does not exist,
            // and opening the actual DM can never clear it. Split the prefix
            // off and run the bare half through the same resolution as an
            // un-prefixed desk, re-applying the prefix only when it names a
            // teammate.
            _ => {
                if let Some(bare) = bare {
                    match crate::runtime::assignee::resolve(&record, bare) {
                        crate::runtime::assignee::AssigneeResolution::Agent(agent) => {
                            return format!("dm:{agent}");
                        }
                        crate::runtime::assignee::AssigneeResolution::Desk {
                            desk: desk_id,
                            ..
                        }
                        | crate::runtime::assignee::AssigneeResolution::EmptyDesk(desk_id) => {
                            return desk_id;
                        }
                        _ => {}
                    }
                }
                // A general-chat spelling — `"General"` (the default for an
                // unaddressed message), `"main"`, or `""` — still names the
                // General desk, the console's default thread, so it has to file
                // under the console's canonical main-thread id, which the rail
                // aliases onto its first rendered desk channel
                // ([`crate::server::chat_history::is_general_chat`], issue #65).
                // Anything else is honestly the string as written: it may badge
                // nowhere, but it is not a lie.
                let probe = bare.unwrap_or(desk);
                if crate::server::chat_history::is_general_chat(Some(probe)) {
                    crate::server::chat_history::MAIN_THREAD_ID.to_string()
                } else {
                    desk.to_string()
                }
            }
        }
    }

    /// Files a durable mention notification for the people `mentions` names in
    /// `desk` (the console's channel-id space), for the journaled message at
    /// `message_seq`.
    ///
    /// **One row, many recipients** — not one row each. Read state is already
    /// per `(company, user, notification)`, so a single row carrying an
    /// audience gives every recipient independent read state for free, and the
    /// feed does not grow by the size of the room every time somebody types
    /// `@everyone`. Teammates produce no notification: an agent has no inbox to
    /// badge and no person to interrupt; a mention of one is already handled by
    /// routing.
    ///
    /// Shared by the operator `/chat` path and the approval-continuation path,
    /// so an `@user` an agent types back badges and notifies whoever it names
    /// whichever journaling surface wrote the reply. Without this, a
    /// continuation's mentions rendered as chips and nothing else — the badge
    /// and the notification both silently missing for exactly the person they
    /// are meant to reach: offline when the reply lands.
    pub(crate) async fn notify_mentions(
        &self,
        id: &CompanyId,
        mentions: &[Mention],
        message_seq: &EventSeq,
        by: Option<&Actor>,
        desk: &str,
    ) {
        let users = match self.users().list_users(id).await {
            Ok(users) => users,
            Err(err) => {
                tracing::warn!(
                    company = %id,
                    error = %err,
                    "[mentions] the user directory could not be read; this message badges nobody"
                );
                return;
            }
        };
        let users: Vec<_> = users
            .into_iter()
            .filter(|u| u.status == crate::ports::users::UserStatus::Active)
            .collect();
        let mut audience = crate::runtime::mentions::mentioned_users(&users, mentions);
        // Never notify the author, even when they wrote `@everyone`. `normalize`
        // already drops a direct self-mention, but a broadcast expands to the
        // whole company *after* that, so this is the only place the author can
        // be removed from one.
        if let Some(Actor {
            kind: ActorKind::User,
            id: author,
        }) = by
        {
            audience.retain(|u| u != author);
        }
        if audience.is_empty() {
            return;
        }

        let who = by
            .filter(|a| a.kind == ActorKind::User)
            .and_then(|a| users.iter().find(|u| u.id == a.id))
            .map(crate::runtime::mentions::user_label)
            .unwrap_or_else(|| "Someone".to_string());
        let note = crate::ports::notifications::Notification {
            id: crate::ports::generate_id(),
            kind: "mention".to_string(),
            subject: crate::ports::notifications::Subject {
                kind: crate::ports::notifications::SubjectKind::Message,
                id: message_seq.value().to_string(),
            },
            created_at: crate::ports::now_millis(),
            title: format!("{who} mentioned you in {desk}"),
            audience: Some(audience),
            // The console's channel-id space, so a badge lands without the
            // browser having loaded that transcript. Whether the thread is a DM
            // is a question about the roster, not the human user directory —
            // see [`Self::mention_context`].
            context: Some(self.mention_context(id, &users, desk).await),
        };
        if let Err(err) = self.notifications().append(id, &note).await {
            tracing::warn!(
                company = %id,
                error = %err,
                "[mentions] a mention could not be recorded; the message still lands and \
                 still renders, but nobody is badged for it"
            );
        }
    }

    /// Journals a dispatched card's relay into the conversation it was
    /// spawned from (issue #1852, Part 1).
    ///
    /// [`relay_reply`](crate::harness::built_in::lifecycle::relay_reply)
    /// already builds the right [`OutboundMessage`] — it carries the origin
    /// thread in `reply_to` — but `route_response`'s channel lookup finds no
    /// adapter for an agent id and falls back to the in-memory
    /// `OperatorChannel` (`runtime::channel`, a "response spy with no durable
    /// reader"), and until [`run_dispatch_cycle`](Self::run_dispatch_cycle)
    /// started calling this, nothing wrote the reply down at all. Modeled on
    /// [`publish_continuation`](Self::publish_continuation): the one
    /// difference is the destination comes from **each response's own**
    /// `reply_to.chat_id` — already the origin thread, courtesy of
    /// `relay_reply` — rather than one conversation recorded for the whole
    /// report, because a dispatch cycle answers exactly the one card it ran.
    ///
    /// Gated on `reply_to` being present, not on its `chat_id` being
    /// non-empty. That is the one field `relay_reply` sets that no other
    /// `OutboundMessage` producer does — the synchronous chat-turn cycle that
    /// `journal_chat_replies` (`server::operator`) journals leaves it `None`
    /// — so this can never re-journal a bubble that path already wrote, and a
    /// board-created card (no `origin_chat_id`, so `run_task`/
    /// `refuse_dispatch` return no relay at all) contributes nothing here
    /// either. An **empty** `chat_id` is still a real destination, not an
    /// absent one: `origin_chat_id` preserves `Some("")` for a card spawned
    /// from General, and `chat_history::same_conversation` treats `""` as an
    /// alias for General — so it must be journaled, not discarded.
    ///
    /// Best-effort, like `HarnessBrain::journal_task_outcome`'s own writes: a
    /// failure here is logged, never propagated. By the time this runs the
    /// card is already settled and persisted — its terminal column, its
    /// `journal_task_outcome` timeline record — so failing the cycle over
    /// this write would abandon that anchor for a dispatch that has, in fact,
    /// landed.
    #[cfg(feature = "openhuman")]
    async fn journal_dispatch_replies(&self, report: &CycleReport) {
        for response in &report.responses {
            let Some(chat_id) = response
                .reply_to
                .as_ref()
                .map(|reply_to| reply_to.chat_id.as_str())
            else {
                continue;
            };
            // Scanned host-side from the reply text, same as
            // `publish_continuation` and `journal_chat_replies` — the
            // console's picker never touched this message.
            let reply_mentions = self
                .resolve_mentions(
                    &response.text,
                    None,
                    response
                        .agent
                        .as_deref()
                        .map(|id| Actor {
                            kind: ActorKind::Agent,
                            id: id.to_string(),
                        })
                        .as_ref(),
                )
                .await;
            match self
                .events
                .append(
                    &self.id,
                    CompanyEvent::AgentReply {
                        parent: None,
                        chat_id: chat_id.to_string(),
                        // Issue #885: the author, falling back to the
                        // destination only when the producer named none —
                        // `relay_reply` always names none, so this is the
                        // orchestrator answering for its own roster.
                        agent_id: response
                            .agent
                            .clone()
                            .unwrap_or_else(|| response.channel.clone()),
                        text: response.text.clone(),
                        steps: response.steps.clone(),
                        // Dropped, deliberately — unlike `publish_continuation`
                        // and `journal_chat_replies`, which carry it through.
                        // `response.task_id` here always names the very card
                        // `journal_task_outcome` (`HarnessBrain`) just settled
                        // and already marked with a `DeskTaskCompleted` pointed
                        // at this same `chat_id` (issue #377's "finished → …"
                        // pill, `chat_history::owns`). That pill is already the
                        // origin thread's card link for this settle; setting
                        // `task_id` here too would additionally render this
                        // bubble's own "Card opened" chip (`CardChip`,
                        // `MessageRow`) — a second link to a card that, by the
                        // time this prose lands, is not "opened" at all. Two
                        // links for one settle is `journal_task_outcome`'s own
                        // "one run's words into one conversation twice" mistake
                        // (see its doc comment), aimed at a link instead of the
                        // text — and it is exactly what doubled the e2e
                        // `chat-dispatch-marker` reload count.
                        task_id: None,
                        mentions: reply_mentions.clone(),
                        // Zero, and stays zero: no reply's mentions reach
                        // dispatch, so no reply is ever a mention hop.
                        mention_depth: 0,
                    },
                )
                .await
            {
                Ok(seq) => {
                    if !reply_mentions.is_empty() {
                        self.notify_mentions(&self.id, &reply_mentions, &seq, None, chat_id)
                            .await;
                    }
                }
                Err(err) => tracing::warn!(
                    company = %self.id,
                    chat_id = %chat_id,
                    error = %err,
                    "[dispatch] a card's relay reply could not be journaled; the origin \
                     thread will not see it"
                ),
            }
        }
    }

    async fn publish_continuation(&self, approval_id: &ApprovalId, report: &mut CycleReport) {
        let conversation = self
            .journal
            .approval_conversation(approval_id)
            .unwrap_or_default();
        let thread = conversation.thread;
        // Where the reply goes when the approval was raised in no conversation
        // at all — a workflow node's parked tool call, a scheduler tick. Read
        // once for the whole report: every response of one continuation answers
        // the same approval, so they cannot land in two places.
        let nowhere =
            continuation_fallback_chat_id(self.journal.approval_origin(approval_id).as_ref());
        for response in &mut report.responses {
            let chat_id = thread.clone().unwrap_or_else(|| nowhere.clone());
            // Checked against the channel actually being answered into, not
            // against the recorded thread: when `thread` is absent the reply
            // goes to the run or card the work belongs to, and a root belonging
            // to some other channel must not follow it there.
            let parent = self.resolvable_parent(conversation.parent, &chat_id).await;
            // Scanned host-side from the reply text. The author is passed so a
            // teammate naming itself in its own answer does not chip itself.
            let reply_mentions = self
                .resolve_mentions(
                    &response.text,
                    None,
                    response
                        .agent
                        .as_deref()
                        .map(|id| Actor {
                            kind: ActorKind::Agent,
                            id: id.to_string(),
                        })
                        .as_ref(),
                )
                .await;
            match self
                .events
                .append(
                    &self.id,
                    CompanyEvent::AgentReply {
                        parent,
                        chat_id: chat_id.clone(),
                        // Issue #885: the author, not the destination. Same
                        // fallback as the `/chat` path — a producer that names
                        // no agent keeps the pre-#885 behaviour exactly.
                        agent_id: response
                            .agent
                            .clone()
                            .unwrap_or_else(|| response.channel.clone()),
                        text: response.text.clone(),
                        steps: response.steps.clone(),
                        task_id: response.task_id.clone(),
                        mentions: reply_mentions.clone(),
                        // Zero, and stays zero: no reply's mentions reach
                        // dispatch, so no reply is ever a mention hop.
                        mention_depth: 0,
                    },
                )
                .await
            {
                Ok(seq) => {
                    response.message_id = Some(seq.value().to_string());
                    // The durable half of a reply's mention, same as an operator
                    // message's and the `/chat` path's. Without this an `@user`
                    // the agent types back renders as a chip and nothing else —
                    // the badge and the notification both silently missing for
                    // whoever it named, which is worst for exactly the person it
                    // is meant to reach: offline when the reply lands.
                    if !reply_mentions.is_empty() {
                        self.notify_mentions(&self.id, &reply_mentions, &seq, None, &chat_id)
                            .await;
                    }
                }
                Err(err) => tracing::warn!(
                    company = %self.id,
                    approval_id = %approval_id,
                    error = %err,
                    "[approval] the continuation answered but its reply could not be \
                     journaled; the bubble has no durable id"
                ),
            }
        }
    }

    /// Tells the operator, in the conversation they are waiting in, that the
    /// continuation failed (issue #469, defect 4).
    ///
    /// Journaled as an [`AgentReply`](CompanyEvent::AgentReply) rather than sent
    /// through [`announce_to_operator`](Self::announce_to_operator), because the
    /// latter is a bare channel send with no event behind it — it reaches an
    /// adapter, not the console's event stream, and not a transcript reload. The
    /// person this is for is watching the thread they approved in.
    ///
    /// The wording says what is true and what to do: the decision stuck, the
    /// work did not, and re-approving is safe.
    async fn announce_continuation_failure(&self, approval_id: &ApprovalId) {
        let conversation = self
            .journal
            .approval_conversation(approval_id)
            .unwrap_or_default();
        let thread = conversation
            .thread
            .unwrap_or_else(|| crate::runtime::channel::OPERATOR_CHANNEL.to_string());
        // Issue #435: the bad news belongs in the same place the good news
        // would have gone. A failure notice left flat in the channel while the
        // question sits in a thread is the same lost-conclusion bug wearing a
        // different hat — and this is the message the operator is most likely
        // to be waiting on.
        let parent = self.resolvable_parent(conversation.parent, &thread).await;
        if let Err(err) = self
            .events
            .append(&self.id, continuation_failure_notice(thread, parent))
            .await
        {
            tracing::warn!(
                company = %self.id,
                approval_id = %approval_id,
                error = %err,
                "[approval] a failed continuation could not be reported to the operator"
            );
        }
    }

    /// The answer to a decision that lands while its turn is still blocked on
    /// another (issue #469).
    ///
    /// Synthetic, like [`already_resolved_report`](CycleRunner::already_resolved_report),
    /// and for the same reason: nothing ran, so there is nothing to report but
    /// the fact itself. It carries a line rather than an empty body because the
    /// un-detached caller — the Approvals page — renders the response, and
    /// "recorded, still waiting on the rest" is the honest thing to show
    /// somebody who has just approved one of four and would otherwise be told
    /// nothing at all.
    ///
    /// Deliberately **not** journaled: it is a receipt for one request, not an
    /// agent's reply, and four of them in a conversation would be noise over a
    /// state the approval cards already show.
    fn still_waiting_report(&self, turn: &str) -> CycleReport {
        let outstanding = self.continuations.outstanding(turn);
        CycleReport {
            cycle_id: crate::ports::generate_id(),
            responses: vec![crate::ports::types::OutboundMessage {
                message_id: None,
                task_id: None,
                channel: crate::runtime::channel::OPERATOR_CHANNEL.to_string(),
                agent: None,
                text: format!(
                    "Recorded. The agent picks this back up once the remaining {outstanding} \
                     sign-off{} on this step {} decided.",
                    if outstanding == 1 { "" } else { "s" },
                    if outstanding == 1 { "is" } else { "are" },
                ),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
            }],
            executed_effects: Vec::new(),
            parked: Vec::new(),
            persisted_seq: None,
            input_seqs: Vec::new(),
        }
    }

    /// Sweeps every parked approval past its TTL, resolving each to a
    /// default-deny and writing an `ApprovalExpired` audit entry to the journal.
    /// Returns the ids that expired.
    ///
    /// **Driven by [`MaintenanceTicker`](crate::runtime::maintenance::MaintenanceTicker)**
    /// — a process-wide ticker over the registry, not the per-company cron
    /// scheduler. Until issue #971 the only production caller was
    /// `CompanyScheduler::tick_maintenance`, and that scheduler is only spawned
    /// for a company whose manifest declares a `[[schedule]]`. So a company with
    /// no manifest cron — including one whose work is driven entirely by
    /// *workflow* schedules, which run on a different loop — parked approvals
    /// forever and swept none of them, at any age. Maximal minting, zero
    /// sweeping, and a cold boot faithfully re-parked the backlog from the
    /// journal with its original park instants.
    ///
    /// Capped at [`MAX_RETIREMENTS_PER_TICK`] per call, oldest first — see
    /// [`sweep_expired_capped`](crate::policy::ManifestApprovalGate::sweep_expired_capped).
    /// A host that has been accumulating for days meets its whole backlog on
    /// the first tick after this ships, and each retirement is a journal
    /// append, a grant clear, an event append and possibly a released turn.
    /// Uncapped, that is one burst on the minute tick every other company in
    /// the process shares.
    pub async fn sweep_expired_approvals(self: &Arc<Self>) -> Result<Vec<ApprovalId>> {
        let now = now_millis();
        let expired = self
            .approval_gate
            .sweep_expired_capped(now, MAX_RETIREMENTS_PER_TICK);
        for id in &expired {
            // Issue #1861: read the blocker's question BEFORE retiring, because
            // retiring is what removes it from the journal's pending set. After
            // that there is no way back to what was being asked, and a card
            // returned without its question is a card nobody can act on.
            let unanswered = self.unanswered_blocker(id);
            // Issue #1861: detect blockers independently of task linkage,
            // so unlinked blockers (from workflow nodes or chat) are recognized
            // as blockers, not ordinary approvals, even though unanswered
            // returns None for them.
            let is_blocker = self.is_blocker(id);
            self.retire_approval(id, ExpiryReason::Ttl, now).await?;
            self.finish_expiry(id, is_blocker, unanswered).await;
        }
        Ok(expired)
    }

    /// The board write and the badge a retirement owes, shared by the two paths
    /// that retire an expired approval: the sweeper that finds the deadline
    /// first, and [`retire_if_expired`](Self::retire_if_expired) when a late
    /// resolve finds it instead.
    ///
    /// Shared because it was not, and the two disagreed (CodeRabbit review on
    /// #1905). The sweeper returned the card; the late-expiry path did not, so
    /// a task-linked blocker discovered that way sat in `paused` forever with
    /// nothing left to release it — the approval it was waiting on had just
    /// been retired, and the next sweep will never see that id again. Both
    /// callers now reach the identical outcome, which is the same property
    /// `retire_approval` exists to give the retirement itself.
    ///
    /// **The board first, then the badge**, so the badge can tell the truth:
    /// its "its card is back in To-do" copy is now gated on a move that
    /// actually landed rather than on the blocker merely naming a card.
    ///
    /// Everything here is best-effort and everything here runs *after* the
    /// retirement, which has already propagated its own error. A notification
    /// that cannot be filed, or a board write that fails, must not undo a
    /// default-deny that already happened — the card stays `paused` with the
    /// question on it and the log line is the trace.
    async fn finish_expiry(
        &self,
        id: &ApprovalId,
        was_blocker: bool,
        unanswered: Option<(String, String)>,
    ) {
        let mut card_returned = false;
        if let Some((task_id, question)) = unanswered {
            match crate::runtime::advance::return_expired_blocker_card(
                self.tasks().as_ref(),
                &self.id,
                &task_id,
                &question,
            )
            .await
            {
                Ok(true) => {
                    card_returned = true;
                    tracing::info!(
                        company = %self.id,
                        task = %task_id,
                        approval = %id.as_ref(),
                        "[approvals] an unanswered blocker returned its card to To-do"
                    );
                }
                // The card moved on without us — an operator dragged it, or it
                // was already re-dispatched. Theirs, not ours, and not a return
                // this notification may claim.
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    company = %self.id,
                    task = %task_id,
                    error = %err,
                    "[approvals] a blocker expired but its card could not be returned; it \
                     stays paused with the question on it"
                ),
            }
        }
        // Issue #1865: a blocker nobody answered is exactly the silent failure
        // this notification exists for — "awaiting approval" forever with
        // nothing telling anybody it timed out.
        self.notify_approval_expired(id, was_blocker, card_returned)
            .await;
    }

    /// The card and question behind a parked **blocker**, or `None` for an
    /// ordinary approval (issue #1861).
    ///
    /// Read from the journal's pending set, which is why every caller has to
    /// call it *before* retiring: retirement is what empties that set.
    ///
    /// The task comes from the approval's own [`TaskLink`], not from the
    /// payload's `step`. Both can name a card, and the link is the one the
    /// journal has always maintained — a payload written by a future producer
    /// that forgot the field would silently strand the card, whereas a missing
    /// link is a case this already handles by returning `None`. A workflow
    /// node's blocker is parked `Unlinked` and so lands here as `None`, which
    /// is right: there is no card to return, and #1864 owns what a stalled run
    /// does next.
    ///
    /// [`TaskLink`]: crate::runtime::journal::TaskLink
    /// Whether a pending approval is a blocker (question the operator must answer).
    /// Unlike `unanswered_blocker`, this returns true regardless of task linkage,
    /// so unlinked blockers (from workflow nodes or chat) are recognized.
    fn is_blocker(&self, id: &ApprovalId) -> bool {
        let pending = match self.journal.pending().into_iter().find(|p| &p.id == id) {
            Some(p) => p,
            None => return false,
        };
        let prefix = format!("{}.", crate::ports::blockers::BLOCKER_EFFECT_PREFIX);
        pending.effect.kind.starts_with(&prefix)
    }

    fn unanswered_blocker(&self, id: &ApprovalId) -> Option<(String, String)> {
        use crate::runtime::journal::TaskLink;

        let pending = self.journal.pending().into_iter().find(|p| &p.id == id)?;
        let prefix = format!("{}.", crate::ports::blockers::BLOCKER_EFFECT_PREFIX);
        if !pending.effect.kind.starts_with(&prefix) {
            return None;
        }
        let task_id = match pending.task {
            Some(TaskLink::Task { id }) => id,
            _ => return None,
        };
        let payload: crate::ports::blockers::BlockerPayload =
            serde_json::from_value(pending.effect.payload.clone()).ok()?;
        // The question, then what would answer it. An operator reading this off
        // a To-do card has neither the thread nor the approvals page in front
        // of them any more, so both halves have to be on the card.
        Some((task_id, format!("{} ({})", payload.reason, payload.needed)))
    }

    /// Files a durable notification that a parked approval expired unanswered
    /// (issue #1865) — one row, whole company, since expiry has no single
    /// decider the way a mention has a mentioned user.
    ///
    /// `was_blocker` picks the copy (issue #1861). The two expiries are not the
    /// same event: an approval that times out **is** decided — denied by
    /// default — while a blocker that times out was never a decision at all,
    /// and telling an operator their unanswered question was "denied" would
    /// describe a judgement nobody made about work that is still perfectly
    /// possible.
    ///
    /// `card_returned` is the **outcome of the board write**, not the presence
    /// of a link (CodeRabbit review on #1905). It used to be
    /// `unanswered.is_some()` — "this blocker names a card" — which claimed the
    /// card was back in To-do before anything had tried to move it, and on the
    /// late-expiry path where nothing moved it at all. Only
    /// [`finish_expiry`](Self::finish_expiry) sets it, and only from a move
    /// that actually landed.
    async fn notify_approval_expired(
        &self,
        id: &ApprovalId,
        was_blocker: bool,
        card_returned: bool,
    ) {
        let note = crate::ports::notifications::Notification {
            id: crate::ports::generate_id(),
            kind: "approval_expired".to_string(),
            subject: crate::ports::notifications::Subject {
                kind: crate::ports::notifications::SubjectKind::Approval,
                id: id.as_ref().to_string(),
            },
            created_at: now_millis(),
            // Issue #1861: a question nobody answered is not "denied by
            // default" — there was nothing to deny. Saying so would tell an
            // operator a decision was made against work that is simply still
            // waiting to be explained. Only claim a card came back if the
            // blocker was actually linked to a task (has_linked_task).
            title: if was_blocker && card_returned {
                "A question nobody answered timed out; its card is back in To-do".to_string()
            } else if was_blocker {
                "A question nobody answered timed out".to_string()
            } else {
                "An approval expired unanswered and was denied by default".to_string()
            },
            audience: None,
            context: None,
        };
        if let Err(err) = self.notifications().append(&self.id, &note).await {
            tracing::warn!(
                company = %self.id,
                approval = %id.as_ref(),
                error = %err,
                "[approvals] an expiry notification could not be recorded; the default-deny \
                 still lands, but nobody is badged for it"
            );
        }
    }

    /// Retires one approval the operator never decided: the whole default-deny
    /// transaction, in one place (issue #971).
    ///
    /// **The single retirement primitive.** The entry is already out of
    /// [`ManifestApprovalGate`](crate::policy::ManifestApprovalGate)'s map by
    /// the time this runs — removal happens inside the gate's own critical
    /// section, in `sweep_expired_capped` or a `resolve_*`, and nothing else
    /// may remove from it. That ordering is what makes an operator clicking
    /// Approve as a sweep retires the same entry get either a real approval or
    /// [`ResolveOutcome::NotParked`](crate::policy::ResolveOutcome::NotParked),
    /// never a silent double execution. This function is everything that has to
    /// happen *after* that removal, and it exists as one function so a second
    /// retirement rule cannot ship with three of the four steps.
    ///
    /// The four steps, none of which is optional:
    ///
    /// 1. The **journal** record — the binding audit entry for a default-deny.
    ///    This one propagates its error; the rest are best-effort, because a
    ///    retirement that has already happened in memory must not be undone by
    ///    a write that failed after it.
    /// 2. **Clearing the pending mark** (issue #796): the parked approval is
    ///    gone, so its work unit is no longer awaiting a resume and the
    ///    checkout it held across the park becomes sweepable.
    /// 3. **Releasing the #469 continuation.** A retirement is a *decision* as
    ///    far as the continuation gate is concerned and has to be, or a turn
    ///    that raised four sign-offs and only ever got three waits for a fourth
    ///    that is never coming. Spawned rather than awaited: the continuation
    ///    is a full agent turn behind the per-company cycle lock, and this runs
    ///    on a minute boundary shared by every company.
    /// 4. The **`ApprovalResolved` event**. Expiry *is* a resolution — a
    ///    default-deny on silence — and before #305 it wrote only the journal
    ///    record, so a wait that ended in a timeout produced no event at all
    ///    and was invisible to every event-log reader including the task
    ///    timeline. `by` is `System`, which is what lets the operator SSE feed
    ///    say "expired" rather than attributing the deny to whoever is looking.
    ///
    /// **No grant is minted here, and none can be.** A
    /// [`GrantedCall`](crate::runtime::grants::GrantedCall) exists only on
    /// `resolve_outcome`'s `Approved` arm; this function takes no verdict and
    /// records `Deny`. That is the safety property the whole change rests on:
    /// an approval disappearing from the queue must never read as one that was
    /// granted.
    async fn retire_approval(
        self: &Arc<Self>,
        id: &ApprovalId,
        reason: ExpiryReason,
        at_millis: u64,
    ) -> Result<()> {
        let explicit_request = self
            .approval_gate
            .take_expired_effect(id)
            .filter(|effect| effect.kind == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND)
            .and_then(|effect| effect.agent.clone().map(|agent| (agent, effect)));
        self.journal.record_expired(id, at_millis, reason).await?;
        // Issue #796: the parked approval is gone, so its work unit is no
        // longer awaiting a resume — drop the pending mark so the checkout it
        // was holding across the park becomes sweepable.
        self.grants.clear_pending(id);
        if let Some((agent, effect)) = explicit_request {
            let by = Actor {
                kind: ActorKind::System,
                id: "expiry".into(),
            };
            if let Err(error) = CycleRunner::new(self)
                .mint_approval_continuation(id, agent, effect, Verdict::Deny, by.clone())
                .await
            {
                tracing::error!(
                    approval_id = %id,
                    %error,
                    "[approval] an expired explicit request could not queue its denial \
                     continuation; continuing the retirement sweep"
                );
            } else {
                // Route through the ordinary settled-verdict path so chat and
                // workflow-node requests both reach the asking agent.
                drop(self.spawn_follow_up(ResolveReceipt::Settled(Box::new(
                    CompanyEvent::ApprovalResolved {
                        approval_id: id.clone(),
                        verdict: Verdict::Deny,
                        by,
                    },
                ))));
                return Ok(());
            }
        }
        // Issue #469: releasing the turn this approval was blocking, and
        // running its continuation when this expiry was the last thing it
        // waited on. Spawned rather than awaited: the continuation is a full
        // agent turn behind the per-company cycle lock, and the maintenance
        // tick this runs on fires on a minute boundary for every company.
        if let Some(turn) = self.journal.approval_cycle(id).flatten() {
            // Issue #978: an expiry is a default-DENY, and the run's batch
            // has to hear it as one. Banked before the count is decremented,
            // exactly as an operator's verdict is in `continue_turn` — an
            // expired gate left in neither ledger would be replayed into,
            // pause the continuation, and park a brand-new card for a
            // decision that has already been made.
            self.workflow_gates.decide(&turn, id, Verdict::Deny);
            if let Some(batch) = self.continuations.decide(&turn, None) {
                let workflow_run =
                    crate::runtime::workflow_resume::run_id_from_turn(&turn).is_some();
                // A workflow run releases even on an empty batch: every
                // decision may have been an expiry (which appends its own
                // event), and the run still has to be told so its approved
                // siblings are not stranded. A brain turn with nothing to
                // report owes no cycle, exactly as before.
                if workflow_run || !batch.is_empty() {
                    let rt = Arc::clone(self);
                    let released = id.clone();
                    let turn = turn.clone();
                    tokio::spawn(async move {
                        let outcome = if workflow_run {
                            rt.resume_workflow_run(&released, &turn, batch).await
                        } else {
                            rt.run_continuation(&released, batch).await
                        };
                        if let Err(error) = outcome {
                            tracing::error!(
                                company = %rt.id,
                                %error,
                                "[approval] the continuation released by an expiry failed"
                            );
                        }
                    });
                }
            }
        }
        if let Err(e) = self
            .events
            .append(
                &self.id,
                CompanyEvent::ApprovalResolved {
                    approval_id: id.clone(),
                    verdict: Verdict::Deny,
                    by: Actor {
                        kind: ActorKind::System,
                        id: "expiry".into(),
                    },
                },
            )
            .await
        {
            tracing::warn!(
                approval_id = %id,
                error = %e,
                "approval expiry journaled but its event-log entry failed",
            );
        }
        Ok(())
    }

    /// Expires every single-use grant the agent never redeemed, and tells the
    /// operator (issue #243). Returns the ids that expired.
    ///
    /// An approval is consent to an action *now*, not a standing authorisation.
    /// Without this, a grant minted today would still admit the call if the same
    /// tool surfaced next month — the operator would have authorised something
    /// they had long since forgotten, at a moment they knew nothing about.
    ///
    /// The expiry is announced rather than silent, and that is the point. The
    /// failure this guards is the operator approving, seeing nothing happen, and
    /// having no way to tell whether the work is in flight, already done, or
    /// quietly dead. A line on the operator channel makes re-approving an
    /// informed choice.
    ///
    /// The journal write is the binding record and propagates; the operator line
    /// is best-effort, matching
    /// [`sweep_expired_approvals`](Self::sweep_expired_approvals) — a delivery
    /// fault must not undo an expiry that has already happened in memory.
    pub async fn sweep_expired_grants(&self) -> Result<Vec<ApprovalId>> {
        let now = now_millis();
        let expired = self.grants.sweep(now, GRANT_TTL_MILLIS);
        let mut ids = Vec::with_capacity(expired.len());
        for grant in expired {
            self.journal
                .record_grant_expired(&grant.approval_id, now)
                .await?;
            let text = format!(
                "Approved `{}` for `{}`, but the agent didn't act within 15 minutes — \
                 re-approve to retry.",
                grant.tool, grant.agent
            );
            for channel in &self.channels {
                if channel.channel_id() == crate::runtime::channel::OPERATOR_CHANNEL {
                    if let Err(e) = channel
                        .send(crate::ports::types::OutboundMessage {
                            message_id: None,
                            task_id: None,
                            channel: crate::runtime::channel::OPERATOR_CHANNEL.to_string(),
                            agent: None,
                            text: text.clone(),
                            steps: Vec::new(),
                            reply_to: None,
                            mentions: Vec::new(),
                        })
                        .await
                    {
                        tracing::warn!(
                            approval_id = %grant.approval_id,
                            error = %e,
                            "grant expiry journaled but the operator notice failed to send",
                        );
                    }
                    break;
                }
            }
            ids.push(grant.approval_id);
        }

        for continuation in self.grants.sweep_continuations(now, GRANT_TTL_MILLIS) {
            let id = continuation.call.approval_id;
            self.journal
                .record_approval_continuation_expired(&id, now)
                .await?;
            self.announce_to_operator(&format!(
                "The `{}` approval decision for `{}` could not be delivered within 15 minutes — \
                 ask the agent again if the work still matters.",
                continuation.call.tool, continuation.call.agent
            ))
            .await;
            ids.push(id);
        }

        // Issue #374: standing grants lapse on the same maintenance tick.
        //
        // The sweep is housekeeping and an operator notice, never the
        // enforcement — `GrantSet::match_standing` refuses an expired grant
        // under the redemption lock, so "for one hour" means one hour and not
        // "until the next tick after one hour". What this adds is the durable
        // record and the line telling the operator a permission they granted has
        // run out, so its silent return to asking is explained rather than
        // mysterious.
        for grant in self.grants.sweep_standing(now) {
            self.journal.record_standing_expired(&grant.id, now).await?;
            self.announce_to_operator(&format!(
                "The standing permission for `{}` on `{}` has expired — it will ask for approval \
                 again from now on.",
                grant.tool, grant.agent
            ))
            .await;
        }
        Ok(ids)
    }

    /// Every live standing permission, newest first (issue #374) — what the
    /// console's "Standing permissions" section lists.
    pub fn standing_grants(&self) -> Vec<StandingGrant> {
        self.grants.standing()
    }

    /// Revokes a standing permission (issue #374), journaling who took it back
    /// and when. `false` when there was nothing to revoke — already gone, swept,
    /// or revoked from another browser — which the route answers as a 404 rather
    /// than claiming to have done something.
    ///
    /// Takes effect on the **next** policy check. An already-admitted call is
    /// not aborted: there is no abort lever inside an agent's turn, and killing
    /// one mid-call is the lifecycle anti-pattern. The next check finds nothing
    /// and re-parks.
    pub async fn revoke_standing_grant(&self, id: &GrantId, by: Actor) -> Result<bool> {
        let Some(grant) = self.grants.revoke_standing(id) else {
            return Ok(false);
        };
        // Live-set removal first here, unlike minting. The orders are opposite on
        // purpose and both fail safe: a crash while minting must not leave a
        // permission live but unrecorded, and a crash while revoking must not
        // leave one live that the operator has already been told is gone.
        self.journal
            .record_standing_revoked(id, by, now_millis())
            .await?;
        tracing::debug!(
            grant_id = %id,
            tool = %grant.tool,
            agent = %grant.agent,
            "[approval] revoked a standing grant; the next call re-parks"
        );
        Ok(true)
    }

    /// Best-effort one-liner on the operator channel.
    ///
    /// Best-effort by design, matching the approval and grant sweeps: a delivery
    /// fault must not undo a state change that has already happened in memory
    /// and in the journal.
    async fn announce_to_operator(&self, text: &str) {
        for channel in &self.channels {
            if channel.channel_id() == crate::runtime::channel::OPERATOR_CHANNEL {
                if let Err(e) = channel
                    .send(crate::ports::types::OutboundMessage {
                        // A channel send, not a journaled chat reply: there is
                        // no `AgentReply` behind this line and so no sequence
                        // position to name (issue #364). Same as the grant-expiry
                        // notice above, which this generalizes.
                        message_id: None,
                        task_id: None,
                        channel: crate::runtime::channel::OPERATOR_CHANNEL.to_string(),
                        agent: None,
                        text: text.to_string(),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    })
                    .await
                {
                    tracing::warn!(error = %e, "an operator notice failed to send");
                }
                break;
            }
        }
    }

    /// Replays the journal to rebuild the executed-key set, the approval queue,
    /// and the live single-use grants (issue #243).
    pub async fn recover(self: &Arc<Self>) -> Result<()> {
        CycleRunner::new(self).recover().await?;
        self.arm_replayed_continuation_recovery();
        self.schedule_replayed_continuations();
        Ok(())
    }

    /// Arms cold-boot delivery when replay found an explicit decision whose
    /// detached follow-up had not yet been dispatch-claimed.
    pub(crate) fn arm_replayed_continuation_recovery(&self) {
        if !self.journal.replayed_approval_continuations().is_empty() {
            self.replay_continuations_on_register
                .store(true, Ordering::Release);
        }
    }

    /// Detaches replayed decision follow-ups once the runtime is addressable.
    /// The atomic makes a duplicate registration or rebuild swap a no-op.
    pub(crate) fn schedule_replayed_continuations(self: &Arc<Self>) {
        if !self
            .replay_continuations_on_register
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        for continuation in self.journal.replayed_approval_continuations() {
            drop(self.spawn_follow_up(ResolveReceipt::Settled(Box::new(
                CompanyEvent::ApprovalResolved {
                    approval_id: continuation.call.approval_id,
                    verdict: continuation.verdict,
                    by: continuation.by,
                },
            ))));
        }
    }

    /// What every approval ever parked was, keyed by id — including approvals
    /// already resolved or expired.
    ///
    /// The Task Detail read joins this against the event log's
    /// `ApprovalResolved` to recover how long the company was waiting on an
    /// operator (issue #305) and which card the sign-off belonged to
    /// (issue #333). Delegates to the journal so the `pub(crate)` field stays
    /// encapsulated, mirroring [`pending_approvals`](Self::pending_approvals).
    /// What one approval was when it parked (issues #305 + #333).
    ///
    /// The per-id form of [`approval_origins`](Self::approval_origins), and what
    /// the Task Detail read actually uses: that index is unbounded and never
    /// pruned, so cloning it per request would cost the company's whole approval
    /// history on every poll of a route the console polls.
    pub fn approval_origin(&self, id: &ApprovalId) -> Option<ApprovalOrigin> {
        self.journal.approval_origin(id)
    }

    pub fn approval_origins(&self) -> std::collections::HashMap<ApprovalId, ApprovalOrigin> {
        self.journal.approval_origins()
    }

    /// The irreversible effects a task has already executed, oldest first
    /// (issue #351).
    ///
    /// What the retry dialog names. Read from the journal's executed record —
    /// the same append-only set that makes effects at-most-once — so it reports
    /// what was committed to run rather than what a timeline label says an agent
    /// intended.
    pub fn irreversible_effects(&self, task_id: &str) -> Vec<ExecutedEffect> {
        self.journal.irreversible_effects(task_id)
    }

    /// Whether this company's journal holds executed history it cannot describe
    /// (issue #351) — a record written before descriptions existed.
    ///
    /// The companion to [`irreversible_effects`](Self::irreversible_effects):
    /// an empty list only means "nothing irreversible for this card" while this
    /// is `false`. When it is `true` the console confirms a retry regardless and
    /// says so, rather than showing an all-clear it cannot stand behind.
    pub fn has_undescribed_history(&self) -> bool {
        self.journal.has_undescribed_history()
    }

    /// The approvals queue as it is right now, in the two shapes a workflow run
    /// is joined against it by (issue #1189).
    ///
    /// One pass over the same parked effects [`pending_approvals`](Self::pending_approvals)
    /// projects, collecting both keys at once: every live approval id, and every
    /// live `(run, gate node)` pair. See
    /// [`LiveApprovals`](crate::ports::workflow_verdict::LiveApprovals) for why
    /// one key cannot answer for both shapes.
    ///
    /// It reads the **raw** effects rather than the projected summaries on
    /// purpose. `ApprovalSummary::payload` is `display_payload` — redacted and
    /// node-budget-bounded — so recovering a gate's node id from it would be
    /// reading a rendering of the fact instead of the fact, and would break
    /// silently the day the redaction rules change. Building the answer here
    /// also keeps raw parked effects out of the HTTP layer, which is the whole
    /// reason the projection exists.
    ///
    /// No task-link discrimination is needed for the gate half, unlike
    /// [`workflow_run_of`]: `gate_node_id` kind-checks `workflow.approve`, a
    /// kind only `park_pending_gates` ever mints, and on that effect `run_id` is
    /// always the workflow run that paused.
    pub fn live_approvals(&self) -> crate::ports::workflow_verdict::LiveApprovals {
        let mut live = crate::ports::workflow_verdict::LiveApprovals::default();
        for parked in self.journal.pending() {
            live.insert_id(parked.id.as_ref());
            if let (Some(run_id), Some(node_id)) = (
                parked.effect.run_id.as_deref(),
                crate::runtime::workflow_resume::gate_node_id(&parked.effect),
            ) {
                live.insert_gate(run_id, node_id);
            }
        }
        live
    }

    /// The parked queue, with each approval's **owning card** resolved (#1891).
    ///
    /// What every HTTP reader of the queue should call. [`Self::pending_approvals`]
    /// projects `task` as the raw link the park stamped, and that link is only
    /// the *fallback* half of the ownership rule the task detail read applies:
    /// the attempt (`Effect::run_id`) outranks it wherever there is one, which
    /// `the_attempt_id_outranks_the_card_link_when_both_are_present` pins. So an
    /// approval parked under one card's attempt while stamped with another
    /// card's link was handed out under the stamp, and a console joining on it
    /// put the row on the wrong card. Read-only that was a wrong label; once the
    /// board card grew Approve and Decline (#1891) it became an operator
    /// resolving somebody else's request, which is why the resolution belongs
    /// here rather than in a console that cannot see an attempt id at all.
    ///
    /// **Costs one store read per distinct attempt behind the queue**, not per
    /// approval and not per card — the ids are deduplicated first, and a queue
    /// whose parks name no attempt (a chat turn, a scheduler tick) does none.
    /// That is what keeps it affordable on a route the console polls: the
    /// alternative the board rejected in #883 was re-reading task detail per
    /// card per poll.
    pub async fn pending_approvals_resolved(&self) -> Vec<ApprovalSummary> {
        use std::collections::{HashMap, HashSet};

        let mut summaries = self.pending_approvals();
        // Approval id → the **task attempt** that parked it.
        //
        // `Effect::run_id` holds two id spaces — a task attempt (#242) and, on
        // the workflow path, a workflow run — and `generate_id` is only
        // process-locally unique, so the value alone cannot say which
        // ([`workflow_run_of`] says exactly this). Resolving a workflow run id
        // against the run store is therefore not merely useless but unsafe: a
        // collision with a persisted attempt id would find that attempt's card
        // and relabel a workflow approval onto it — inventing a card for a
        // request no card owns, on the surface that now decides.
        //
        // So the park *site* discriminates, through the one predicate that
        // already encodes the rule rather than a second copy of it: a park
        // `workflow_run_of` claims is a workflow park and is left alone. What
        // remains is a park linked to a card, where `run_id` is unambiguously
        // an attempt — which is exactly the misattribution case this exists to
        // correct, a park stamped with one card while its attempt belongs to
        // another.
        //
        // Conservative in the ambiguous direction, the same way
        // `workflow_run_of` is: the cost of under-claiming is a blocked row the
        // board does not draw, and of over-claiming is an operator deciding
        // another owner's request from this card. Those are not comparable.
        let attempts: HashMap<String, String> = self
            .journal
            .pending()
            .into_iter()
            .filter(|p| workflow_run_of(p).is_none())
            .filter_map(|p| {
                p.effect
                    .run_id
                    .clone()
                    .map(|run_id| (p.id.as_ref().to_string(), run_id))
            })
            .collect();
        if attempts.is_empty() {
            return summaries;
        }
        let distinct: HashSet<&str> = attempts.values().map(String::as_str).collect();
        let mut owners: HashMap<String, Option<String>> = HashMap::with_capacity(distinct.len());
        for run_id in distinct {
            // Only a **successful** read is recorded. An entry means the store
            // answered — `Some(card)` or a definite "no card" — and an absent
            // one means it could not be asked, which `resolve_owners` leaves
            // the stamped link alone for (#1895 review).
            //
            // The distinction is the whole of this arm. Folding a failed read
            // into "no owner" (an `.ok()` away) unlinks a still-parked
            // approval, `approvalsForTask` then drops the row, and the card
            // re-enables Resume while the approval is very much still parked —
            // a transient store blip handing the operator the re-dispatch this
            // PR exists to keep out of their hand. A stale link is a label that
            // may be wrong; a dropped blocker is a card that lies about being
            // free.
            match self.runs().get_run(self.id(), run_id).await {
                Ok(run) => {
                    owners.insert(run_id.to_string(), run.and_then(|run| run.task_id));
                }
                Err(err) => {
                    tracing::warn!(
                        run_id,
                        error = %err,
                        "could not resolve an approval's owning card; keeping its parked link",
                    );
                }
            }
        }
        crate::runtime::approval_ownership::resolve_owners(&mut summaries, &attempts, &owners);
        summaries
    }

    /// The approvals currently awaiting the operator.
    ///
    /// The single projection point for [`ApprovalSummary`], and therefore the
    /// single place issue #372's `agent` + `payload` are filled in. The payload
    /// is redacted and bounded **here**, before it is a summary at all, so no
    /// caller can accidentally serialize the raw effect.
    ///
    /// **`task` is the raw park link here.** Every reader that shows an
    /// approval *against a card* wants [`Self::pending_approvals_resolved`]
    /// instead — see there for why the stamp alone is not the ownership answer.
    pub fn pending_approvals(&self) -> Vec<ApprovalSummary> {
        self.journal
            .pending()
            .into_iter()
            .map(|p| ApprovalSummary {
                // Read before the field moves below (issue #880): the answer
                // needs the task link *and* the effect together.
                workflow_run_id: workflow_run_of(&p),
                // Issue #1098's gate id, projected as a fact rather than read
                // out of the display payload — role redaction (issue #618)
                // strips the payload from a member, and the run link (which
                // needs this id) has to survive for the member holding the
                // stalled workflow up.
                workflow_id: crate::runtime::workflow_resume::gate_workflow_id(&p.effect)
                    .map(str::to_owned),
                id: p.id,
                kind: p.effect.kind.clone(),
                amount_usd: p.effect.amount_usd,
                at_millis: p.at_millis,
                // Issue #971: the deadline, filled in at the single projection
                // point so every reader gets the same one. The TTL is read off
                // the gate rather than recomputed from `[policy]`, because the
                // gate is where the `None`-means-default rule resolves and a
                // second resolution of it is a second thing that can disagree —
                // the card would then promise a deadline the gate does not
                // enforce. Issue #1805: measured from the deadline anchor, not
                // `at_millis` — the two coincide until an operator extends, at
                // which point the anchor carries the pushed-out window and the
                // card's countdown moves with it (the same anchor the gate's
                // sweeper uses, so the two never disagree).
                expires_at_millis: Some(
                    p.deadline_anchor_millis
                        .saturating_add(self.approval_gate.ttl_millis()),
                ),
                // Issue #1024: the host's own classification, not the console's
                // guess. `kind` is the tool name for a harness call, so this is
                // the only field that distinguishes an outbound send from an
                // internal effect.
                group: p.effect.group,
                task: p.task,
                agent: p.effect.agent.clone(),
                payload: crate::runtime::approval_display::display_payload(&p.effect),
                thread: p.thread,
                // Issues #374, #444. Both halves matter: a native effect has no
                // teammate and no tool to grant, and a tool that can reach
                // further than a standing permission can describe stays a
                // per-call decision. Read off the parked effect, so the control
                // is offered on exactly the call the card itself is showing —
                // which matters for `composio_execute`, where the same tool is
                // grantable reading a repository and not grantable sending mail.
                // Issue #1098 replaced "is there a teammate" with "is there a
                // subject": a gate has no teammate but names the workflow it
                // belongs to, and that workflow can hold a permission. Decided by
                // the same `subject_of` the resolve route's 400 and the mint use,
                // so the control the card offers and the answer a resolve gets
                // cannot disagree.
                broadly_grantable: p.effect.kind
                    != crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
                    && crate::runtime::grants::subject_of(&p.effect).is_some()
                    && p.effect.may_be_granted_standing(),
                // Issue #1458: a standing **denial** is enforced only on the
                // agent turn path (`standing_deny_applies`); the workflow gate
                // does not honour `Deny`, and the resolve route's 400 refuses a
                // workflow standing denial before the gate is touched. So the
                // deny control is offered only where the runtime will actually
                // enforce it — an agent subject — while the grant half above
                // still covers a workflow, which can hold a standing permission.
                broadly_deniable: p.effect.kind
                    != crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
                    && matches!(
                        crate::runtime::grants::subject_of(&p.effect),
                        Some(crate::runtime::grants::GrantSubject::Agent(_))
                    ),
                // Always false here. Whether a *reader* may see the contents is
                // a property of who is asking, and this projection is
                // deliberately principal-free (issue #618) — the redaction
                // happens at the edge, in `server::approval_visibility`, so
                // per-role logic stays out of the domain layer.
                contents_hidden: false,
                // Issue #842: which turn asked for it, so the conversation can
                // ask about a turn's gated calls once. Projected, never
                // derived — grouping by "same agent, same thread, close
                // together" would guess at a fact the journal already records,
                // and would guess wrong exactly when two turns overlap.
                batch: p.batch,
            })
            .collect()
    }

    /// Captures a feedback item: persists it to the feedback family and logs a
    /// `FeedbackFiled` event. Nothing is filed — capture is always safe and
    /// local. Used by the built-in `feedback` tool and operator-chat intent.
    pub async fn capture_feedback(&self, input: FeedbackInput) -> Result<FeedbackItem> {
        let item = FeedbackItem::capture(input, crate::VERSION, self.filer.consent);
        self.feedback.append(&item).await?;
        self.events
            .append(
                &self.id,
                CompanyEvent::FeedbackFiled {
                    note: item.operator_words.clone(),
                },
            )
            .await?;
        Ok(item)
    }

    /// Captures feedback, then runs the scrub-then-preview gate and either
    /// previews the exact final issue body or files it (per consent). The
    /// scrubber fails closed, so a report that cannot be safely scrubbed is
    /// blocked rather than risked.
    ///
    /// `item_id` carries the previewed item on the confirm (Send-after-Preview)
    /// path: the same item is finalized — never a second capture — so the report
    /// appears once in the feedback family and the posted body is the exact
    /// previewed bytes (see [`crate::feedback::service::finalize`]).
    ///
    /// A confirm closes two gaps a bare `finalize` call would leave open:
    ///
    /// * **Idempotent** — an item that already left this machine (its
    ///   `issue_status` is recorded) returns the recorded result instead of
    ///   filing or forwarding again, so a retried or double-submitted Send does
    ///   not file a second issue or add a duplicate comment. A per-item lock
    ///   held across the whole confirm serialises concurrent confirms of the
    ///   same item, so the loser re-reads the winner's recorded result instead
    ///   of both sending.
    /// * **Preview-first** — an item captured by the feedback tool or the chat
    ///   intent was never previewed and its words are hidden from the reports
    ///   list, so confirming it by id would send a body nobody inspected.
    ///   Confirms of such items are refused; the operator must preview first.
    pub async fn submit_feedback(
        &self,
        input: FeedbackInput,
        preview: bool,
        item_id: Option<String>,
    ) -> Result<FeedbackResponse> {
        let manifest = self.store.load(&self.id).await?.map(|r| r.manifest);
        // Held until the end of the call for a confirm, so the check below and
        // the finalize that records the status are one critical section.
        let mut _confirm_guard = None;
        let item = match item_id {
            Some(id) => {
                // A nonexistent `item_id` is caller-supplied, so it must not
                // mint an entry in the process-wide confirm-lock registry,
                // which is never evicted. Validate existence before taking the
                // lock; the feedback family is append-only, so an id that
                // exists here still exists at the locked re-read below.
                if self.feedback.get(&id).await?.is_none() {
                    return Err(OpenCompanyError::NotFound(format!("feedback item {id}")));
                }
                _confirm_guard = Some(crate::feedback::store::confirm_lock(&id).lock_owned().await);
                let item = self.feedback.get(&id).await?.expect(
                    "feedback item exists: existence checked before taking the confirm lock",
                );
                if !preview {
                    if item.issue_status.is_some() {
                        return Ok(FeedbackResponse::recorded(&item));
                    }
                    if item.scrubbed_body.is_none() {
                        return Ok(FeedbackResponse::blocked(
                            &id,
                            "this report was not previewed; preview it before sending".to_string(),
                        ));
                    }
                }
                item
            }
            None => self.capture_feedback(input).await?,
        };
        crate::feedback::service::finalize(
            &self.feedback,
            self.secrets.as_ref(),
            &self.filer,
            &self.id,
            manifest.as_ref(),
            &item,
            // The `POST .../feedback` route is operator-driven; default to an
            // annoyance-severity operator filing.
            crate::feedback::Severity::Annoyance,
            crate::feedback::FeedbackSource::Operator,
            preview,
        )
        .await
    }

    /// Lists this company's captured feedback, newest first, as the
    /// HTTP-safe [`FeedbackSummary`] projection.
    ///
    /// The operator's raw words never appear: they are local-only by
    /// construction (see [`FeedbackItem::operator_words`]), so the reports list
    /// shows what was reported and where it went, not what was typed.
    pub async fn list_feedback(&self) -> Result<Vec<FeedbackSummary>> {
        let mut items = self.feedback.list().await?;
        items.sort_by_key(|item| std::cmp::Reverse(item.at_millis));
        Ok(items.iter().map(FeedbackSummary::from_item).collect())
    }

    /// The shared feedback board, one page at a time.
    ///
    /// The board is the hub's, not this runtime's: these four methods are a
    /// proxy that lends the console the instance credential without ever
    /// putting it in a browser. An instance provisioned with no credential has
    /// no board — that is a `no_board` refusal, not an empty page, so the
    /// console can hide the surface instead of rendering "nobody has asked for
    /// anything yet" to every unprovisioned operator.
    pub async fn feedback_board(&self, query: BoardQuery) -> Result<BoardPage> {
        self.hub()?.list_board(query).await
    }

    /// One board item with its comments.
    pub async fn feedback_board_item(&self, id: &str) -> Result<BoardDetail> {
        self.hub()?.board_item(id).await
    }

    /// Casts (or retracts) this instance's vote on a board item.
    pub async fn vote_feedback_board(&self, id: &str, value: VoteValue) -> Result<BoardItem> {
        self.hub()?.vote_board_item(id, value).await
    }

    /// Comments on a board item as this instance's hub account.
    pub async fn comment_feedback_board(&self, id: &str, body: &str) -> Result<BoardComment> {
        self.hub()?.comment_board_item(id, body).await
    }

    /// The hub client, or the refusal an unprovisioned instance owes the caller.
    fn hub(&self) -> Result<&dyn TinyHumansClient> {
        self.filer
            .tinyhumans
            .as_deref()
            .ok_or_else(|| crate::error::OpenCompanyError::TinyHumans {
                code: "no_board".to_string(),
                message: "this instance is not connected to a TinyHumans account".to_string(),
            })
    }

    /// The company's display name — what the manifest calls it, falling back to
    /// its id.
    ///
    /// Split out of [`Self::status`] for the one caller that needs the name
    /// *before* anybody has signed in: `GET …/auth/config`, which draws the
    /// sign-in heading. That route is public, so it must not be handed a status
    /// snapshot — the pending-approval count alone is a fact about the company's
    /// work, and the name is the only field on it a stranger may see.
    ///
    /// A store failure yields the id rather than an error: the name decorates a
    /// screen whose real payload is the mode, and a heading is not worth
    /// refusing to tell the console how this company signs people in.
    pub async fn display_name(&self) -> String {
        let named = self
            .store
            .load(&self.id)
            .await
            .ok()
            .flatten()
            .map(|record| record.manifest.company.name);
        match named {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => self.id.to_string(),
        }
    }

    /// Resolve the mentions in one chat message body.
    ///
    /// The single seam both journal sites go through, so an operator message
    /// and an agent reply cannot end up obeying different rules about who
    /// `@ada` is. Loads the record and the user directory and hands them to
    /// [`crate::runtime::mentions::resolve`], which does the rest without
    /// touching IO.
    ///
    /// **Never fails a send.** A store that cannot answer means mentions cannot
    /// be resolved, not that the message cannot be delivered — so a read error
    /// yields an empty list and is logged. The message still lands; it simply
    /// draws no chips and pings nobody, which is the same state every message
    /// journaled before this feature existed is in.
    pub async fn resolve_mentions(
        &self,
        text: &str,
        supplied: Option<Vec<Mention>>,
        sender: Option<&Actor>,
    ) -> Vec<Mention> {
        // Issue: on the operator-message path this runs BEFORE the journal
        // append (`mention_responder` reads the resolved mentions off the
        // journaled event, so the append cannot go first), which puts these
        // two store reads in front of every chat POST's accept latency. Run
        // together rather than sequentially — they read different stores and
        // neither depends on the other's result — to keep that addition close
        // to the cost of the slower read alone rather than the sum of both.
        let (record, user_list) =
            tokio::join!(self.store.load(&self.id), self.users().list_users(&self.id));
        let record = match record {
            Ok(Some(record)) => record,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    company = %self.id,
                    error = %err,
                    "[mentions] the company record could not be read; this message is \
                     journaled with no mentions"
                );
                return Vec::new();
            }
        };
        let mut users = user_list.unwrap_or_else(|err| {
            tracing::warn!(
                company = %self.id,
                error = %err,
                "[mentions] the user directory could not be read; only teammates and \
                 desks are resolvable on this message"
            );
            Vec::new()
        });
        // Suspended users are retained only for attribution and are refused on
        // every request — they must not be a live mention target here either.
        users.retain(|u| u.status == crate::ports::users::UserStatus::Active);
        // Sorted by the same stable key `GET .../chat/mentionables` uses before
        // it mints slugs (`user_slugs`), so a collision between two same-named
        // users gets the same `-2`/`-3` suffix here that the picker advertised —
        // an unsorted `UserStore` order (most-recently-created first) could
        // otherwise resolve `@sam-2` to a different person than the one the
        // picker showed under that label.
        users.sort_by(|a, b| a.id.cmp(&b.id));
        crate::runtime::mentions::resolve(text, supplied, sender, &record, &users)
    }

    /// A status snapshot, loading the company record for name and lifecycle.
    pub async fn status(&self) -> Result<CompanyStatus> {
        let record = self.store.load(&self.id).await?;
        let (name, logo_url, lifecycle, template_provenance) = match record {
            Some(record) => (
                record.manifest.company.name,
                record.manifest.company.logo_url,
                record.lifecycle,
                record.template_provenance,
            ),
            None => (self.id.to_string(), None, "running".to_string(), None),
        };
        Ok(CompanyStatus {
            id: self.id.clone(),
            name,
            logo_url,
            lifecycle,
            pending_approvals: self.journal.pending().len(),
            template_provenance,
            emergency_paused: self.is_emergency_paused(),
        })
    }

    /// Transitions the company's lifecycle to `to`, persisting the new state and
    /// appending a [`CompanyEvent::LifecycleChanged`] audit event stamped with
    /// the acting `by` actor. Returns the previous lifecycle string.
    ///
    /// Powers the platform pause/resume/suspend/archive controls. A company with
    /// no durable record yet is a [`OpenCompanyError::CompanyNotFound`].
    pub async fn set_lifecycle(&self, to: impl Into<String>, by: Actor) -> Result<String> {
        let to = to.into();
        // Held across the whole load-modify-save cycle (PR #1875 review
        // finding, second round): `server/provision.rs` calls this directly
        // for pause/resume, with no lock of its own, so without this a
        // `PATCH {scope}` name-confirm racing this transition could load
        // before the rename's `save` lands and save after it, silently
        // writing the confirmed rename's manifest and `name_confirmed` back
        // to their pre-rename values — undoing a write that already returned
        // success and potentially reopening the onboarding name step. Every
        // other `CompanyStore` load-modify-save cycle in the console
        // (`company_profile.rs`, `company_logo.rs`, `activation.rs`, …)
        // already serializes on this same per-company lock.
        let write_lock = company_write_lock(&self.id);
        let _lock = write_lock.lock().await;
        let mut record = self
            .store
            .load(&self.id)
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(self.id.to_string()))?;
        let from = record.lifecycle.clone();
        record.lifecycle = to.clone();
        // `save_importing`, not `save` (PR #1875 review finding): a bare
        // lifecycle flip is not `RuntimeBuilder::build`'s activation-aware
        // migration deciding this record has been seen — it is the console's
        // pause/resume/suspend/archive control, which can fire on a legacy
        // pre-#1843 record `build`'s "existing but not running" arm has
        // deliberately left un-migrated. `save`'s unconditional `true` would
        // poison that record's gate-seen marker while it is still
        // unmigrated, permanently blocking the grandfather arm on every
        // later `running` boot. Forward whatever the marker already is,
        // unless the grandfather back-fill below fires — that is the one
        // case this method itself decides the migration, so it persists
        // `true` for the same reason every deciding arm in `builder.rs` does.
        let gate_seen = self.store.activation_gate_seen(&self.id).await?;
        // Grandfather an unmigrated legacy record the moment an in-place
        // resume (PR #1875 review finding, third round) puts it back to
        // `running` without going through another `RuntimeBuilder::build` —
        // the only other place this back-fill runs (`builder.rs`'s own
        // "running and unlatched" arm). A company already registered in
        // `state.registry()` never rebuilds across pause/resume (`transition`
        // in `server/provision.rs` calls straight into this method on the
        // live runtime), so a legacy pre-#1843 company — never seen by
        // activation-aware code — that gets paused and resumed by the same
        // long-lived process would otherwise keep reading as
        // unconfirmed/unactivated, and the onboarding gate would wrongly
        // reappear for an established operator, until the process eventually
        // restarts and `build` finally applies the migration. Gated on
        // `!gate_seen` and an unset latch exactly like the builder's own arm,
        // so a genuinely new company still mid-onboarding (whose first save
        // already stamped the marker `true`) is never falsely grandfathered
        // by a resume.
        let gate_seen_to_persist =
            if to == "running" && !gate_seen && record.activation_completed_at.is_none() {
                record.name_confirmed = true;
                record.activation_completed_at = Some(crate::ports::now_millis());
                true
            } else {
                gate_seen
            };
        self.store
            .save_importing(&record, gate_seen_to_persist)
            .await?;
        self.events
            .append(
                &self.id,
                CompanyEvent::LifecycleChanged {
                    from: from.clone(),
                    to,
                    by,
                },
            )
            .await?;
        Ok(from)
    }

    // -- Emergency stop (issue #86) -----------------------------------------

    /// Whether the emergency stop is engaged.
    ///
    /// Reads the gate's in-memory flag, which is the enforcement path's own
    /// source of truth — so this can never disagree with what `evaluate` will
    /// actually do, which a re-read of the event log could.
    pub fn is_emergency_paused(&self) -> bool {
        self.approval_gate.is_emergency()
    }

    /// Seeds the gate's emergency flag at boot from the event-log replay in
    /// [`replayed_emergency`](crate::policy::gate::replayed_emergency).
    ///
    /// **Fails safe.** A company whose log cannot be read comes up *stopped*,
    /// not running. The alternative — assume `false` on error — means a store
    /// blip silently un-pauses a company an operator deliberately stopped, and
    /// nothing would surface that: the console would show a healthy company
    /// quietly executing the effects the kill switch was pulled to prevent. A
    /// company wrongly stopped is a visible, one-request problem; a company
    /// wrongly running is the failure this whole endpoint exists to prevent.
    ///
    /// Takes the read *result* rather than doing the read, so this stays
    /// synchronous and directly testable in every direction. The cases are
    /// deliberately distinct and must not be collapsed:
    ///
    /// * `Ok(Some(true))` — the log says stopped. Come up stopped.
    /// * `Ok(Some(false))` — the log says running. Come up running.
    /// * `Ok(None)` — nothing known, and no failure either. Come up running.
    /// * `Err(_)` — the state could not be read. Come up **stopped**.
    ///
    /// Flattening `Ok(None)` and `Err(_)` together is the bug this signature
    /// exists to make hard to write: they look alike at the call site and mean
    /// opposite things.
    pub fn hydrate_emergency(&self, stopped: Result<Option<bool>>) {
        if let Err(ref err) = stopped {
            tracing::error!(
                company = %self.id,
                error = %err,
                "could not replay the emergency-stop state at boot; \
                 failing safe and coming up STOPPED — release it with \
                 POST /api/v1/companies/{{id}}/emergency-resume once the \
                 event log is healthy"
            );
        }
        self.approval_gate
            .set_emergency(emergency_from_load(stopped));
    }

    /// Engages the emergency stop: every new effect outside
    /// [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other) is denied
    /// until an operator releases it.
    ///
    /// **Order is load-bearing: the flag flips before the event is appended.**
    /// Stopping is the safe direction, so enforcement must not wait on I/O that
    /// can fail. If the append then fails the company is stopped *now* — which is
    /// what the operator asked for — and the error tells them it may not survive
    /// a restart. Appending first would leave a window in which a running company
    /// is executing effects the operator believes they have already stopped.
    ///
    /// The flag flip is the gate's atomic `set_emergency`, which returns the
    /// *previous* value: exactly the caller that observed `false` (a real
    /// engage) journals the event, so two concurrent presses append one line,
    /// not two.
    ///
    /// Returns `true` when this call engaged the stop, `false` when it was
    /// already engaged — idempotent, because the second press of a panic button
    /// must not be an error.
    pub async fn emergency_pause(&self, by: Actor, reason: Option<String>) -> Result<bool> {
        // `set_emergency` returns what the switch was *before* this call; a
        // previous `false` is the only case that is a real transition.
        if self.approval_gate.set_emergency(true) {
            return Ok(false);
        }
        self.events
            .append(
                &self.id,
                CompanyEvent::EmergencyPauseChanged {
                    engaged: true,
                    by,
                    reason,
                },
            )
            .await?;
        Ok(true)
    }

    /// Releases the emergency stop, restoring normal policy evaluation.
    ///
    /// **The mirror image of [`emergency_pause`](Self::emergency_pause): the
    /// release is journaled first, and only a successful append clears the
    /// flag.** A failed append leaves the company stopped, so the unsafe
    /// direction is never taken on a best-effort basis. There is no timeout, no
    /// TTL and no auto-release anywhere in this path — releasing is always an
    /// explicit act by an identified operator, recorded as one.
    ///
    /// Releasing is inherently racy in the *opposite* direction from pausing:
    /// two concurrent releases can both observe `true` before either appends,
    /// so the event-log replay (which takes the last event) settles it. The
    /// in-memory flip at the end is the gate's atomic `set_emergency`, and the
    /// returned previous value lets the caller answer with the real outcome —
    /// `true` for the release that actually cleared the switch, `false` for a
    /// second release that raced it.
    ///
    /// Returns `true` when this call released the stop, `false` when it did
    /// not (it was not engaged, or a concurrent release already cleared it).
    pub async fn emergency_resume(&self, by: Actor, reason: Option<String>) -> Result<bool> {
        if !self.approval_gate.is_emergency() {
            return Ok(false);
        }

        self.events
            .append(
                &self.id,
                CompanyEvent::EmergencyPauseChanged {
                    engaged: false,
                    by,
                    reason,
                },
            )
            .await?;
        // Only now, with the release durably recorded, does enforcement stop.
        // A restart between the append and this line comes up running, which
        // matches what the log says the operator decided.
        Ok(self.approval_gate.set_emergency(false))
    }

    /// Rejects operation on a company that is not accepting work.
    ///
    /// Returns [`OpenCompanyError::LifecycleConflict`] when the loaded record's
    /// lifecycle is anything other than `running`.
    pub async fn ensure_running(&self) -> Result<()> {
        if let Some(record) = self.store.load(&self.id).await?
            && record.lifecycle != "running"
        {
            return Err(OpenCompanyError::LifecycleConflict(record.lifecycle));
        }
        Ok(())
    }
}

/// The **workflow** run waiting on a parked approval, if any (issue #880).
///
/// `Effect::run_id` holds two different id spaces — issue #242's task-attempt id
/// and, on the workflow path, a workflow run id — and `generate_id` is only
/// process-locally unique, so the value alone cannot say which it is. The park
/// *site* can, and it is recorded: a task attempt parks inside its dispatch
/// cycle and is linked to that card, while every workflow park goes through
/// `park_and_journal` and is recorded explicitly `Unlinked` (#333). A chat turn
/// is unlinked too but stamps no run id at all, so requiring both is exact.
///
/// Deliberately conservative in the ambiguous direction: an approval with no
/// recorded link (`None`, i.e. a pre-#333 journal line) reports nothing rather
/// than guessing, which is the same fallback rule #333 set for the task link.
fn workflow_run_of(parked: &crate::runtime::journal::PendingApproval) -> Option<String> {
    matches!(
        parked.task,
        Some(crate::runtime::journal::TaskLink::Unlinked)
    )
    .then(|| parked.effect.run_id.clone())
    .flatten()
}

/// Where a continuation's reply is journaled when the approval it resumes was
/// raised in **no conversation** (issue #1092), read off the park's own origin.
///
/// `publish_continuation` answers in the thread the approval came from. When
/// there is none it used to fall back to the answering agent's own id — which
/// `chat_history::owns` resolves as that teammate's DM, so a workflow node's
/// parked `web_fetch`, once approved, posted the re-issued turn's narration
/// into the operator's direct messages as though the teammate had written to
/// them unprompted. `GrantedCall::origin_thread` documents that fallback as
/// "right for a DM and never right for a desk channel"; a workflow run is a
/// third case, and it is the one that reaches here.
///
/// Every arm below names something that **matches no desk**, so the reply stays
/// on the event stream and inside the run or card timeline it belongs to
/// instead of appearing in a chat nobody opened. That is the same device — and
/// the same reasoning — `HarnessBrain::journal_task_outcome` already uses when
/// it journals a dispatch reply under the card id.
///
/// The order is by specificity, and the workflow arm reuses
/// [`workflow_run_of`]'s discrimination rather than restating it:
/// `Effect::run_id` carries two id spaces, and only an explicitly `Unlinked`
/// park with a run id on it is a workflow run. A park with neither a card nor a
/// run came from an unaddressed conversation, so it answers in General — the
/// same reading `chat_history::owns` gives a message journaled with no chat.
fn continuation_fallback_chat_id(
    origin: Option<&crate::runtime::journal::ApprovalOrigin>,
) -> String {
    // An unaddressed operator message is journaled with no chat on it, and
    // `chat_history::owns` reads that absence as the General desk — so a park
    // that carries no run and no card came from a conversation after all, and
    // General is where its answer is read. It is the destination for the
    // unknown case too (a pre-#333 line with no recorded link): a reply in the
    // operator's own line is recoverable, while one in a teammate's DM reads as
    // a message that teammate never sent.
    let general = || crate::server::ops::language::DEFAULT_DESK.to_string();
    let Some(origin) = origin else {
        return general();
    };
    match &origin.task {
        // A board task's dispatch cycle parked this: the card owns the work,
        // and its timeline is where the answer is already read.
        Some(crate::runtime::journal::TaskLink::Task { id }) => id.clone(),
        // Explicitly unlinked *and* carrying a run id is a workflow park — the
        // case this issue exists for. The run id matches no desk, so the answer
        // stays on the run rather than arriving as a teammate's DM.
        Some(crate::runtime::journal::TaskLink::Unlinked) => {
            origin.run_id.clone().unwrap_or_else(general)
        }
        None => general(),
    }
}

impl std::fmt::Debug for CompanyRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanyRuntime")
            .field("id", &self.id)
            .field("channels", &self.channels.len())
            .field("has_economy", &self.economy.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    /// A [`JournalStore`](crate::ports::journal::JournalStore) that refuses
    /// every append once armed — a full or read-only data volume, which is the
    /// failure mode `park_blocker`'s rollback exists for.
    ///
    /// Armed by the test rather than from birth, so boot's own journal writes
    /// still land and the runtime under test is an ordinary one that lost its
    /// volume mid-life.
    #[cfg(feature = "openhuman")]
    #[derive(Default)]
    struct RefusingJournalStore {
        inner: crate::ports::journal::MemoryJournalStore,
        armed: std::sync::atomic::AtomicBool,
    }

    #[cfg(feature = "openhuman")]
    impl RefusingJournalStore {
        fn arm(&self) {
            self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[cfg(feature = "openhuman")]
    #[async_trait::async_trait]
    impl crate::ports::journal::JournalStore for RefusingJournalStore {
        async fn append_journal(
            &self,
            id: &crate::ports::types::CompanyId,
            line: &str,
            durability: crate::ports::journal::Durability,
        ) -> crate::Result<()> {
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::error::OpenCompanyError::Store(
                    "RefusingJournalStore: the volume is full".to_string(),
                ));
            }
            self.inner.append_journal(id, line, durability).await
        }

        async fn read_journal(
            &self,
            id: &crate::ports::types::CompanyId,
        ) -> crate::Result<Vec<String>> {
            self.inner.read_journal(id).await
        }

        async fn journal_imported(
            &self,
            id: &crate::ports::types::CompanyId,
        ) -> crate::Result<bool> {
            self.inner.journal_imported(id).await
        }

        async fn complete_import(
            &self,
            id: &crate::ports::types::CompanyId,
            lines: Vec<String>,
        ) -> crate::Result<()> {
            self.inner.complete_import(id, lines).await
        }
    }

    use super::{
        CompanyEvent, continuation_failure_notice, emergency_from_load, task_enters_in_progress,
        task_enters_planning,
    };

    /// Issue #880: which parked approvals name a workflow run, and which must
    /// not.
    ///
    /// The discrimination is the whole content of the change, because
    /// `Effect::run_id` carries two id spaces — issue #242's task attempt and
    /// the workflow run — and `generate_id` is only process-locally unique, so
    /// the value cannot be inspected to tell them apart. Getting this wrong in
    /// the permissive direction would print a task-attempt id on an approvals
    /// card as though it were a workflow run.
    #[test]
    fn only_an_unlinked_park_with_a_run_id_names_a_workflow_run() {
        use crate::ports::types::{ApprovalId, Effect, EffectGroup};
        use crate::runtime::journal::{PendingApproval, TaskLink};

        let parked = |task: Option<TaskLink>, run_id: Option<&str>| PendingApproval {
            id: ApprovalId::new("appr-1"),
            effect: Effect {
                kind: "publish_artifact".to_string(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({}),
                agent: Some("ceo".to_string()),
                run_id: run_id.map(str::to_string),
            },
            at_millis: 1,
            deadline_anchor_millis: 1,
            task,
            thread: None,
            batch: None,
        };

        // A workflow park: `park_and_journal` records it explicitly unlinked
        // (#333) and the run stamps its id.
        assert_eq!(
            super::workflow_run_of(&parked(Some(TaskLink::Unlinked), Some("run-1"))),
            Some("run-1".to_string())
        );
        // A board task's attempt: same field, different id space. Must NOT be
        // reported as a workflow run.
        assert_eq!(
            super::workflow_run_of(&parked(
                Some(TaskLink::Task {
                    id: "card-1".to_string()
                }),
                Some("attempt-1")
            )),
            None
        );
        // A chat turn: unlinked, but nothing stamped a run onto it.
        assert_eq!(
            super::workflow_run_of(&parked(Some(TaskLink::Unlinked), None)),
            None
        );
        // A pre-#333 line records no link at all, so the park site is unknown.
        // Conservative rather than guessing — the same fallback rule #333 set.
        assert_eq!(super::workflow_run_of(&parked(None, Some("run-1"))), None);
    }

    /// Issue #1092: a continuation whose approval was raised in no conversation
    /// must never be journaled into the answering teammate's DM.
    ///
    /// The fallback is the whole content of the fix, so it is asserted per park
    /// site rather than through one happy path: the id it returns is what
    /// `chat_history::owns` will (or will not) resolve to a chat thread.
    #[test]
    fn a_continuation_with_no_conversation_answers_outside_every_chat() {
        use crate::runtime::journal::{ApprovalOrigin, TaskLink};

        let origin = |task: Option<TaskLink>, run_id: Option<&str>| ApprovalOrigin {
            at_millis: 1,
            kind: "web_fetch".to_string(),
            task,
            run_id: run_id.map(str::to_string),
            thread: None,
            parent: None,
            cycle: None,
        };

        // A workflow node's parked call: unlinked, with the run stamped on it.
        // The run id is the destination — the timeline the operator was already
        // watching, and a value no desk answers to.
        assert_eq!(
            super::continuation_fallback_chat_id(Some(&origin(
                Some(TaskLink::Unlinked),
                Some("run-9")
            ))),
            "run-9",
        );
        // A board card's dispatch: the card owns the work, exactly as
        // `journal_task_outcome` already records it.
        assert_eq!(
            super::continuation_fallback_chat_id(Some(&origin(
                Some(TaskLink::Task {
                    id: "card-3".to_string()
                }),
                Some("attempt-4"),
            ))),
            "card-3",
        );
        // Unlinked with nothing stamped is an unaddressed operator turn, and a
        // pre-#333 line with no link at all is unknown. Both answer in General
        // — visible to the person who approved, and never a teammate's DM.
        assert_eq!(
            super::continuation_fallback_chat_id(Some(&origin(Some(TaskLink::Unlinked), None))),
            "General",
        );
        assert_eq!(
            super::continuation_fallback_chat_id(Some(&origin(None, Some("run-9")))),
            "General",
        );
        assert_eq!(super::continuation_fallback_chat_id(None), "General");
    }

    /// Issue #1092, the property that actually matters: a workflow park's
    /// continuation must not resolve to a teammate's DM or to a desk.
    ///
    /// Asserted through `chat_history::owns` itself rather than by eyeballing
    /// the string, so a change on either side fails here instead of silently
    /// re-opening the leak. The General arm is asserted the other way round in
    /// the same breath — it is *supposed* to be readable — because a fallback
    /// that hid every continuation would pass a one-directional test and lose
    /// the operator's answer.
    #[test]
    fn a_workflow_parks_continuation_owns_no_desk_and_no_dm() {
        use crate::ports::types::CompanyEvent;
        use crate::runtime::journal::{ApprovalOrigin, TaskLink};
        use crate::server::chat_history::owns;

        let reply = |chat_id: String| CompanyEvent::AgentReply {
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id,
            agent_id: "copywriter".to_string(),
            text: "re-issued".to_string(),
            steps: Vec::new(),
            task_id: None,
        };
        let origin = |task: Option<TaskLink>, run_id: Option<&str>| ApprovalOrigin {
            at_millis: 1,
            kind: "web_fetch".to_string(),
            task,
            run_id: run_id.map(str::to_string),
            thread: None,
            parent: None,
            cycle: None,
        };

        // The leak: a workflow node's park, answered into the copywriter's DM.
        let workflow = super::continuation_fallback_chat_id(Some(&origin(
            Some(TaskLink::Unlinked),
            Some("run-9"),
        )));
        for (desk_id, desk_name) in [
            ("copywriter", "Copywriter"),
            ("creative", "Creative studio"),
        ] {
            assert!(
                !owns(desk_id, desk_name, &reply(workflow.clone())),
                "`{workflow}` must not be read as the `{desk_id}` conversation",
            );
        }

        // And the other direction: an unaddressed operator turn still answers
        // somewhere the person who approved is looking.
        let unaddressed =
            super::continuation_fallback_chat_id(Some(&origin(Some(TaskLink::Unlinked), None)));
        assert!(
            owns("main", "General", &reply(unaddressed.clone())),
            "`{unaddressed}` must still be read as the operator's General line",
        );
    }

    #[cfg(feature = "openhuman")]
    use std::sync::{Arc, Mutex};

    #[cfg(feature = "openhuman")]
    use async_trait::async_trait;

    #[cfg(feature = "openhuman")]
    #[derive(Default)]
    struct RecordingMeter {
        queried_companies: Mutex<Vec<crate::ports::types::CompanyId>>,
    }

    #[cfg(feature = "openhuman")]
    #[async_trait]
    impl crate::ports::UsageMeter for RecordingMeter {
        async fn record(
            &self,
            _company: &crate::ports::types::CompanyId,
            _sample: &crate::ports::UsageSample,
        ) -> crate::Result<()> {
            Ok(())
        }

        async fn query(
            &self,
            company: &crate::ports::types::CompanyId,
            _since_millis: u64,
        ) -> crate::Result<Vec<crate::ports::UsageSample>> {
            self.queried_companies.lock().unwrap().push(company.clone());
            Ok(Vec::new())
        }
    }

    /// `is_busy` must see **all three** sources, not just the steer registry.
    ///
    /// The first version of the busy endpoint read only `steer.any_inflight()`,
    /// which covers dispatched board cards and desk delegations. A top-level
    /// operator chat turn registers none of those — it takes `serial` and
    /// nothing else — and workflow runs live in `run_supervisor`, a separate
    /// registry. So the 15-minute turn opencompany-microservice#22 measured
    /// reported `busy: false` and got parked mid-flight, which is exactly the
    /// failure the endpoint exists to prevent.
    ///
    /// Each source is exercised idle → busy → idle independently, so dropping
    /// any one of them from `is_busy` fails here rather than silently in
    /// production. Deliberately outside any feature gate: the steer registry is
    /// only wired under `openhuman`, so a test that relied on it alone would not
    /// run in the default build at all.
    #[tokio::test]
    async fn is_busy_sees_every_source_of_work() {
        let (runtime, _record, _home) = runtime_and_record().await;
        assert!(!runtime.is_busy(), "an idle runtime must not report busy");

        // 1. The cycle lock — the operator-chat case the steer registry misses.
        {
            let _cycle = runtime.serial.lock().await;
            assert!(
                runtime.is_busy(),
                "a turn holding the cycle lock must report busy"
            );
        }
        assert!(!runtime.is_busy(), "releasing the cycle lock must clear it");

        // 2. A workflow run — tracked in its own registry, invisible to both
        //    the cycle lock and the steer registry.
        {
            let (_ctx, _run) = runtime
                .run_supervisor()
                .begin("wf-1", false)
                .expect("begin a workflow run");
            assert!(runtime.is_busy(), "a live workflow run must report busy");
        }
        assert!(
            !runtime.is_busy(),
            "the run guard must clear it on drop, or the tenant never parks again"
        );

        // 3. A steerable in-flight run — the original signal, kept because a
        //    dispatched card can outlive the cycle that started it.
        {
            let _guard = runtime.steer().register(
                runtime.id(),
                crate::company::steer::InflightEntry {
                    key: "run-1".to_string(),
                    task_id: Some("run-1".to_string()),
                    kind: crate::company::steer::InflightKind::Task,
                    title: "Ship the thing".to_string(),
                    agent_id: "ceo".to_string(),
                    started_at_millis: 0,
                    pending_action: None,
                },
            );
            assert!(runtime.is_busy(), "a registered steer run must report busy");
        }
        assert!(!runtime.is_busy(), "the steer guard must clear it on drop");
    }

    /// A poisoned run supervisor must make `is_busy` report **busy**.
    ///
    /// The predicate's advertised invariant is that it fails closed, and #1133
    /// only delivered that for two of its three sources: `steer.any_inflight`
    /// was made poison-tolerant, but the `run_supervisor` arm still reached a
    /// `.expect` through `len`. `GET /healthz/busy` has no `CatchPanicLayer`, so
    /// that panic reset the connection, the manager read it as "cannot tell",
    /// and its default is to park — losing the work the endpoint exists to
    /// protect (issue #1239).
    ///
    /// Outside any feature gate on purpose, matching
    /// `is_busy_sees_every_source_of_work`: the run supervisor is wired on the
    /// default build, and this must not be a test that only CI's `openhuman`
    /// lane runs.
    #[tokio::test]
    async fn is_busy_fails_closed_on_a_poisoned_run_supervisor() {
        let (runtime, _record, _home) = runtime_and_record().await;
        assert!(!runtime.is_busy(), "an idle runtime must not report busy");

        runtime.run_supervisor().poison_for_test();

        assert!(
            runtime.is_busy(),
            "a poisoned run supervisor must report busy rather than panic in the handler"
        );
    }

    /// Codex review (#1865): "Plan first" on a bounced card is a fresh
    /// attempt exactly like a re-dispatch, so the stale bounce chip must not
    /// survive the To-do → Planning edge either.
    ///
    /// No harness/planner wired — the default shape ~200 callers use — so
    /// `plan_task`'s spawn is a no-op and this exercises only the synchronous
    /// clearing `upsert_task` does before it, matching the inert-board
    /// pattern `runtime::builder::test` already uses for the sibling
    /// dispatch edge.
    #[tokio::test]
    async fn planning_first_clears_a_stale_bounce_chip_same_as_a_redispatch() {
        use crate::ports::tasks::{COLUMN_PLANNING, COLUMN_TODO, TaskRecord};

        let home = tempfile::tempdir().expect("tempdir");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n",
        )
        .expect("manifest");
        let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(crate::ports::types::CompanyId::new("acme"))
            .build()
            .await
            .expect("runtime");
        let runtime = std::sync::Arc::new(runtime);

        let card = TaskRecord {
            id: "card-1".to_string(),
            title: "Draft the spec".to_string(),
            note: None,
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 1,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            // A stale chip from a dispatch attempt that already bounced.
            bounced: Some("a previous run's dispatch failed".to_string()),
        };
        runtime
            .upsert_task(&card)
            .await
            .expect("seed the bounced card in To-do");

        let mut planned = card.clone();
        planned.column = COLUMN_PLANNING.to_string();
        runtime
            .upsert_task(&planned)
            .await
            .expect("drag it into Planning");

        let after = runtime
            .tasks()
            .list(runtime.id())
            .await
            .expect("list")
            .into_iter()
            .find(|t| t.id == "card-1")
            .expect("card survives");
        assert_eq!(
            after.bounced, None,
            "entering Planning must clear the previous dispatch's bounce chip, not carry it \
             through to whatever the planning pass settles next"
        );
    }

    /// Codex review on PR #1883 (comment 3874654383): `patch_task` accepts
    /// any board column on one write, so an operator can move a bounced
    /// To-do card straight to `done` — a departure that touches neither the
    /// dispatch nor the planning edge. The manual move supersedes the bounce
    /// exactly as much as a re-dispatch does, and
    /// [`crate::ports::tasks::TaskRecord::bounced`]'s own doc promises it
    /// clears "the instant the card leaves `todo` any other way" — this
    /// proves the "any other way" case, not just the two edge-fired ones the
    /// sibling test above covers.
    ///
    /// Before the fix, `upsert_task` only cleared `bounced` when
    /// `dispatch || plan`, so this direct To-do → Done transition left the
    /// stale chip in place — and it would have resurfaced if the card later
    /// came back to To-do, naming a failure the intervening manual move had
    /// already superseded.
    #[tokio::test]
    async fn a_direct_move_to_done_clears_a_stale_bounce_chip() {
        use crate::ports::tasks::{COLUMN_DONE, COLUMN_TODO, TaskRecord};

        let home = tempfile::tempdir().expect("tempdir");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n",
        )
        .expect("manifest");
        let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(crate::ports::types::CompanyId::new("acme"))
            .build()
            .await
            .expect("runtime");
        let runtime = std::sync::Arc::new(runtime);

        let card = TaskRecord {
            id: "card-2".to_string(),
            title: "Draft the spec".to_string(),
            note: None,
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 1,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            // A stale chip from a dispatch attempt that already bounced.
            bounced: Some("a previous run's dispatch failed".to_string()),
        };
        runtime
            .upsert_task(&card)
            .await
            .expect("seed the bounced card in To-do");

        let mut done = card.clone();
        done.column = COLUMN_DONE.to_string();
        runtime
            .upsert_task(&done)
            .await
            .expect("drag it straight to Done");

        let after = runtime
            .tasks()
            .list(runtime.id())
            .await
            .expect("list")
            .into_iter()
            .find(|t| t.id == "card-2")
            .expect("card survives");
        assert_eq!(
            after.bounced, None,
            "a direct To-do → Done move must clear the stale bounce chip too — the operator's \
             manual transition supersedes the reason it named, and the chip must not resurface \
             if the card ever comes back to To-do"
        );
    }

    async fn runtime_and_record() -> (
        super::CompanyRuntime,
        crate::ports::CompanyRecord,
        tempfile::TempDir,
    ) {
        let home = tempfile::tempdir().expect("tempdir");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
            "#,
        )
        .expect("manifest");
        let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(crate::ports::types::CompanyId::new("acme"))
            .build()
            .await
            .expect("runtime");
        let record = runtime
            .store()
            .load(runtime.id())
            .await
            .expect("load")
            .expect("record");
        (runtime, record, home)
    }

    /// `set_lifecycle` must serialize its load-modify-save cycle against
    /// `company_write_lock`, exactly like every other console load-modify-save
    /// (PR #1875 review finding, second round). Proven the same way
    /// `put_logo_serializes_against_the_company_write_lock`
    /// (`server/ops/company_logo.rs`) proves it for that handler: hold the
    /// lock externally, drive the real method, and demand it cannot finish
    /// while the lock is held.
    #[tokio::test]
    async fn set_lifecycle_serializes_against_the_company_write_lock() {
        let (runtime, _record, _home) = runtime_and_record().await;
        let runtime = std::sync::Arc::new(runtime);
        let id = runtime.id().clone();

        let lock = crate::ports::store::company_write_lock(&id);
        let guard = lock.lock().await;

        let runtime_for_task = runtime.clone();
        let mut task = tokio::spawn(async move {
            runtime_for_task
                .set_lifecycle(
                    "paused",
                    crate::ports::types::Actor {
                        kind: crate::ports::types::ActorKind::Operator,
                        id: "op".to_string(),
                    },
                )
                .await
        });

        // The method must be blocked behind the held lock — give it every
        // chance to (wrongly) race ahead before declaring it stuck.
        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "set_lifecycle completed while company_write_lock was held \
             elsewhere — it is not serializing its load-modify-save cycle \
             against concurrent writers (e.g. a racing name-confirm PATCH)"
        );

        drop(guard);
        let from = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("set_lifecycle never resumed after the lock was released")
            .expect("task panicked")
            .expect("set_lifecycle failed");
        assert_eq!(from, "running", "the fixture starts running");
    }

    /// The shared workflow-wiring fixture, re-exported under the name these
    /// tests already use.
    #[cfg(feature = "openhuman")]
    use crate::harness::workflow_wiring_deps as wiring_deps;

    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn workflow_wiring_is_absent_without_harness_deps() {
        let (runtime, record, _home) = runtime_and_record().await;
        assert_eq!(runtime.wired_workflow_namespaces(&record).await, None);
    }

    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn workflow_wiring_keeps_the_static_capability_filter_without_a_plan() {
        let (mut runtime, record, _home) = runtime_and_record().await;
        runtime.set_workflow_harness_deps(wiring_deps(
            &runtime,
            None,
            crate::harness::toolbelt::CapabilityFilter::DenyNamespaces(
                ["web"].into_iter().collect(),
            ),
            None,
        ));
        let namespaces = runtime
            .wired_workflow_namespaces(&record)
            .await
            .expect("wiring");
        assert!(!namespaces.contains("web"));
        assert!(namespaces.contains("shell"));
    }

    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn workflow_wiring_resolves_the_plan_against_its_company_meter() {
        let (mut runtime, record, _home) = runtime_and_record().await;
        let meter = Arc::new(RecordingMeter::default());
        runtime.set_workflow_harness_deps(wiring_deps(
            &runtime,
            Some(meter.clone()),
            crate::harness::toolbelt::CapabilityFilter::AllowAll,
            Some(crate::harness::capability_budget::CapabilityPlan {
                period: crate::harness::capability_budget::BudgetPeriod::Daily,
                budgets: [("shell".to_string(), u64::MAX)].into_iter().collect(),
                total_budget: None,
            }),
        ));
        let namespaces = runtime
            .wired_workflow_namespaces(&record)
            .await
            .expect("wiring");
        assert!(namespaces.contains("shell"));
        assert!(!namespaces.contains("web"));
        assert!(!namespaces.contains("code"));
        assert_eq!(*meter.queried_companies.lock().unwrap(), vec![record.id]);
    }

    /// Issue #874: the wiring carries **why** a namespace is out, not just that
    /// it is — the two reasons `refusal_for` renders at run time, so a caller
    /// (the `tool-slugs` route) can tell an operator "no provider configured"
    /// apart from "your capability tier filtered it" before a run fails.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn workflow_wiring_names_why_each_namespace_is_unwired() {
        let (mut runtime, record, _home) = runtime_and_record().await;
        // `wiring_deps` leaves `search: None` — the staging shape in issue #874,
        // where `searchCredentialConfigured` was false — and we deny `web` on top
        // so both reasons appear in one map.
        runtime.set_workflow_harness_deps(wiring_deps(
            &runtime,
            None,
            crate::harness::toolbelt::CapabilityFilter::DenyNamespaces(
                ["web"].into_iter().collect(),
            ),
            None,
        ));
        let wiring = runtime.workflow_tool_wiring(&record).await.expect("wiring");
        assert_eq!(
            wiring.missing.get("search").copied(),
            Some(crate::workflows::caps::MissingReason::SearchBackendNotConfigured),
            "no search backend is configured: {:?}",
            wiring.missing
        );
        assert_eq!(
            wiring.missing.get("web").copied(),
            Some(crate::workflows::caps::MissingReason::CapabilityTierFiltered),
            "web is denied by the capability filter: {:?}",
            wiring.missing
        );
        assert!(
            !wiring.missing.contains_key("shell"),
            "a wired namespace carries no reason: {:?}",
            wiring.missing
        );
    }

    /// Issue #874, the staging repro at the layer the route reads: a company that
    /// explicitly grants `search` on a deployment with **no** search backend must
    /// not be offered `web_search` for grounding — it must be reported as granted
    /// but unwired instead, so the copilot cannot author a node that dies at the
    /// first run.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_granted_but_unwired_tool_is_reported_not_offered() {
        let (mut runtime, mut record, _home) = runtime_and_record().await;
        record.manifest.tools.allow.push("search".to_string());
        record.manifest.tools.allow.push("shell".to_string());
        runtime.set_workflow_harness_deps(wiring_deps(
            &runtime,
            None,
            crate::harness::toolbelt::CapabilityFilter::AllowAll,
            None,
        ));
        let wiring = runtime.workflow_tool_wiring(&record).await;
        let wired = wiring.as_ref().map(|w| &w.wired_namespaces);

        let effective = crate::company::workflow_effective_tool_slugs(&record, wired);
        let unwired = crate::company::workflow_granted_but_unwired_tool_slugs(&record, wired);
        assert!(
            !effective.iter().any(|slug| slug == "web_search"),
            "an unwired search tool is not offered for grounding: {effective:?}"
        );
        assert!(
            unwired.iter().any(|slug| slug == "web_search"),
            "…but it IS reported as granted-and-unwired: {unwired:?}"
        );
        assert!(
            effective.iter().any(|slug| slug == "shell"),
            "a granted AND wired tool is still offered: {effective:?}"
        );
        // The two lists partition the granted set: nothing may appear in both, or
        // a caller grounding on one and warning from the other contradicts itself.
        assert!(
            !effective.iter().any(|slug| unwired.contains(slug)),
            "effective {effective:?} and unwired {unwired:?} overlap"
        );
    }

    /// The other half of the honesty split: with no harness deps the wiring is
    /// *unknowable*, so every granted tool stays offered and nothing is claimed
    /// to be unwired. Reporting "all granted tools are broken" on a host that
    /// simply cannot say would be the worse failure.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn unknowable_wiring_offers_the_grant_only_set_and_reports_nothing_unwired() {
        let (runtime, mut record, _home) = runtime_and_record().await;
        record.manifest.tools.allow.push("search".to_string());
        let wiring = runtime.workflow_tool_wiring(&record).await;
        assert!(wiring.is_none(), "no harness deps means no wiring answer");
        let wired = wiring.as_ref().map(|w| &w.wired_namespaces);

        assert!(
            crate::company::workflow_effective_tool_slugs(&record, wired)
                .iter()
                .any(|slug| slug == "web_search"),
            "a granted tool is still offered when the deployment cannot be asked"
        );
        assert!(
            crate::company::workflow_granted_but_unwired_tool_slugs(&record, wired).is_empty(),
            "nothing is claimed unwired when the deployment cannot be asked"
        );
    }

    /// Issue #86: the kill switch's boot decision, including the direction it
    /// fails in.
    ///
    /// The `Err` arm is the whole point. An unreadable log must not un-pause a
    /// company an operator deliberately stopped: a company wrongly stopped is a
    /// visible problem someone fixes with one request, while a company wrongly
    /// running is exactly the outcome the endpoint exists to prevent, and
    /// nothing would surface it.
    #[test]
    fn an_unreadable_record_comes_up_stopped() {
        assert!(emergency_from_load(Err(
            crate::error::OpenCompanyError::CompanyNotFound("acme".into())
        )));
    }

    /// The other three arms, which must stay distinct from the error case.
    #[test]
    fn a_readable_record_is_taken_at_its_word() {
        // Stopped stays stopped across the restart.
        assert!(emergency_from_load(Ok(Some(true))));
        // Running stays running — the switch is not sticky by accident.
        assert!(!emergency_from_load(Ok(Some(false))));
        // Nothing known is not the same as a read failure.
        assert!(!emergency_from_load(Ok(None)));
    }

    /// Issue #337: the planning edge, on the same terms as the dispatch one.
    /// Entering the column fires; resting in it does not.
    #[test]
    fn planning_fires_only_on_entering_planning() {
        // The drag this feature exists for.
        assert!(task_enters_planning(Some("todo"), "planning"));
        // A card created straight into Planning is a genuine entry too.
        assert!(task_enters_planning(None, "planning"));
        // Already planning, re-saved — an edit, the pass's own note append, a
        // re-title. This is what makes "one pass per entry, no retry" a
        // property of the edge rather than a rule the planner has to remember,
        // and it is what stops the settle's own write re-triggering the pass.
        assert!(!task_enters_planning(Some("planning"), "planning"));
        // Leaving Planning never fires it — including the success settle.
        assert!(!task_enters_planning(Some("planning"), "in_progress"));
        assert!(!task_enters_planning(Some("planning"), "todo"));
        // No other column entry fires it.
        for column in ["todo", "in_progress", "paused", "in_review", "done"] {
            assert!(!task_enters_planning(Some("todo"), column), "{column}");
        }
    }

    /// Issue #576: a prompt-box card buys **exactly one** planning pass across
    /// its whole life — not zero, not two.
    ///
    /// The assertions above pin the edge one transition at a time. This walks
    /// the sequence a self-promoting card actually goes through and *counts*,
    /// because the two ways to get this wrong are both invisible to a
    /// single-transition test:
    ///
    /// * **Zero** — the card is created directly in `planning` rather than
    ///   moved there, so if entry required a previous column there would be no
    ///   transition to observe and the pass would never fire. The card would sit
    ///   in Planning forever, which is the one column that must never hold a
    ///   card at rest.
    /// * **Two** — the pass writes its plan back onto the card *while the card
    ///   is still in Planning* (`harness::planning`, via `upsert_task`). If
    ///   resting in the column counted as entering it, that write-back would
    ///   start a second pass, which would write back, and bill a model call each
    ///   time.
    ///
    /// A test that merely asserted "it planned" would pass in the second case.
    #[test]
    fn a_prompt_box_card_buys_exactly_one_planning_pass() {
        // The life of a card opened from the prompt box: created directly in
        // Planning, its plan written back while it rests there, then settled
        // onward by the pass itself.
        let life = [
            (None, "planning"),                // the prompt box opens it
            (Some("planning"), "planning"),    // the pass writes the plan back
            (Some("planning"), "in_progress"), // the success settle
        ];
        let fires = life
            .iter()
            .filter(|(prev, next)| task_enters_planning(*prev, next))
            .count();
        assert_eq!(
            fires, 1,
            "a prompt-box card must buy exactly one planning pass: {life:?}"
        );

        // And the failure exit, which returns the card to To-do, must not buy a
        // second one on the way out either.
        let returned = [(None, "planning"), (Some("planning"), "todo")];
        assert_eq!(
            returned
                .iter()
                .filter(|(prev, next)| task_enters_planning(*prev, next))
                .count(),
            1,
            "a pass that returned the card must still have cost exactly one"
        );
    }

    /// The two edges are mutually exclusive by construction: one write names
    /// one target column, so no upsert can both plan and dispatch a card. This
    /// is what makes the "planning happens BEFORE dispatch" ordering structural
    /// rather than a matter of which `if` runs first in `upsert_task`.
    #[test]
    fn no_single_write_both_plans_and_dispatches() {
        for prev in [None, Some("todo"), Some("planning"), Some("in_progress")] {
            for next in [
                "todo",
                "planning",
                "in_progress",
                "paused",
                "in_review",
                "done",
            ] {
                assert!(
                    !(task_enters_planning(prev, next) && task_enters_in_progress(prev, next)),
                    "{prev:?} → {next} fires both edges"
                );
            }
        }
    }

    /// The success settle's shape, pinned end to end: a pass that clears the
    /// card writes `planning → in_progress`, which is NOT a planning entry (so
    /// it cannot loop) and IS a dispatch entry (so the plan actually hands the
    /// work on). Both halves matter; either one alone would be a bug.
    #[test]
    fn a_cleared_plan_hands_the_card_on_without_replanning_it() {
        assert!(
            !task_enters_planning(Some("planning"), "in_progress"),
            "the settle must not re-enter the pass it is settling"
        );
        assert!(
            task_enters_in_progress(Some("planning"), "in_progress"),
            "the settle must fire the dispatch edge — that is why it routes \
             through upsert_task rather than the plain store port"
        );
    }

    #[test]
    fn dispatch_only_on_entering_in_progress() {
        // Fresh card created straight into `in_progress` → dispatch.
        assert!(task_enters_in_progress(None, "in_progress"));
        // The drag: todo → in_progress → dispatch.
        assert!(task_enters_in_progress(Some("todo"), "in_progress"));
        // Issue #301: planning sits before dispatch, so entering it must not
        // fire one — and leaving it for `in_progress` must.
        assert!(!task_enters_in_progress(Some("todo"), "planning"));
        assert!(task_enters_in_progress(Some("planning"), "in_progress"));
        // Already in_progress, re-saved (e.g. an edit) → no re-dispatch.
        assert!(!task_enters_in_progress(Some("in_progress"), "in_progress"));
        // Any non-in_progress target → no dispatch.
        assert!(!task_enters_in_progress(Some("in_progress"), "in_review"));
        assert!(!task_enters_in_progress(None, "todo"));
        assert!(!task_enters_in_progress(Some("in_review"), "done"));
    }

    /// Issue #246 spend gate. A card opened from chat goes through
    /// `POST …/tasks` with **no** `column`, so what stops it from spending
    /// money the operator never approved is that the server's default column is
    /// not the dispatch trigger. That is two independent facts — what the
    /// default is, and what the trigger is — living in two different modules,
    /// so a change to either alone silently opens the gate. This pins them
    /// together.
    ///
    /// The second assertion is the positive control: without it the first
    /// would still pass if `task_enters_in_progress` were broken to always
    /// return `false`, and the test would be guarding nothing.
    #[test]
    fn the_column_a_chat_created_card_defaults_to_does_not_dispatch() {
        use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_TODO};

        // `create_task` (src/server/ops/tasks.rs) defaults an omitted `column`
        // to this one.
        assert!(
            !task_enters_in_progress(None, COLUMN_TODO),
            "a chat-created card must not spend an agent turn on arrival — the \
             human drag into in_progress is the approval gate"
        );
        assert!(
            task_enters_in_progress(None, COLUMN_IN_PROGRESS),
            "positive control: the trigger this test relies on is still live"
        );
    }

    /// Issue #242: the attempt row exists **before** the cycle is spawned, in
    /// [`RunStatus::Pending`], carrying the assignee it was dispatched to and a
    /// 1-based ordinal that climbs per re-dispatch. This is the whole point of
    /// minting at the choke point rather than inside the cycle — a host that
    /// dies in the gap leaves a visible orphan instead of nothing.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_dispatch_opens_a_pending_attempt_before_the_cycle_spawns() {
        use crate::ports::TaskRecord;
        use crate::ports::runs::{RunFilter, RunStatus};
        use crate::ports::tasks::COLUMN_IN_PROGRESS;

        let home = tempfile::Builder::new()
            .prefix("opencompany-run-open-")
            .tempdir()
            .expect("tempdir");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = crate::ports::types::CompanyId::new("acme");
        let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("runtime");

        let card = TaskRecord {
            id: "t-1".to_string(),
            title: "Ship it".to_string(),
            note: None,
            column: COLUMN_IN_PROGRESS.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 0,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        };

        let first = runtime.open_run(&card).await.expect("an attempt is minted");
        let runs = runtime
            .runs()
            .list_runs(&id, &RunFilter::for_task("t-1"))
            .await
            .expect("list");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, first);
        assert_eq!(
            runs[0].status,
            RunStatus::Pending,
            "the row is written before anything runs, so it starts Pending"
        );
        assert_eq!(runs[0].attempt, 1, "the first attempt at a card is 1");
        assert_eq!(runs[0].agent_id, "ceo");
        assert!(
            runs[0].trigger_event_seq.is_none(),
            "the driving event has not been appended yet"
        );
        assert!(runs[0].started_at_millis.is_none());

        // A re-dispatch is a NEW attempt, never a resurrection of the first.
        let second = runtime.open_run(&card).await.expect("a second attempt");
        assert_ne!(second, first);
        let runs = runtime
            .runs()
            .list_runs(&id, &RunFilter::for_task("t-1"))
            .await
            .expect("list");
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs.iter()
                .find(|r| r.id == second)
                .expect("second")
                .attempt,
            2
        );
    }

    /// Issue #290 against issue #242's write path: a card dragged into
    /// `in_progress` while this runtime is being replaced must not leave an
    /// attempt row claiming to be pending forever.
    ///
    /// The board write is deliberately *not* gated on the quiesce — only cycles
    /// are — so this window is reachable, and the refusal happens before
    /// `CycleRunner` starts the run, which puts it out of reach of the cycle's
    /// own terminality backstop. A rebuild also skips the boot reaper by design,
    /// so nothing else would ever clean the row up.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_dispatch_refused_by_a_quiescing_runtime_settles_its_attempt() {
        use std::sync::Arc;

        use crate::ports::TaskRecord;
        use crate::ports::runs::{RUNTIME_REPLACED_ERROR, RunStatus};
        use crate::ports::tasks::COLUMN_IN_PROGRESS;

        let home = tempfile::Builder::new()
            .prefix("opencompany-run-quiesce-")
            .tempdir()
            .expect("tempdir");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = crate::ports::types::CompanyId::new("acme");
        let runtime = Arc::new(
            crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .expect("runtime"),
        );

        let card = TaskRecord {
            id: "t-1".to_string(),
            title: "Ship it".to_string(),
            note: None,
            column: COLUMN_IN_PROGRESS.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 0,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        };

        // Positive control: on a live runtime the cycle runs, so the row is
        // settled by the backstop inside it and never reaches the path below.
        let live = runtime.open_run(&card).await.expect("an attempt");
        Arc::clone(&runtime)
            .run_dispatch_cycle(card.id.clone(), Some(live.clone()))
            .await;
        let settled = runtime
            .runs()
            .get_run(&id, &live)
            .await
            .expect("read")
            .expect("row");
        assert!(
            settled.status.is_terminal(),
            "the ordinary dispatch path still settles its own row"
        );
        assert_ne!(
            settled.error.as_deref(),
            Some(RUNTIME_REPLACED_ERROR),
            "the live path must not be settled by the quiesce handler"
        );

        // The window this test exists for.
        let stranded = runtime.open_run(&card).await.expect("an attempt");
        runtime.quiesce().await;
        Arc::clone(&runtime)
            .run_dispatch_cycle(card.id.clone(), Some(stranded.clone()))
            .await;

        let abandoned = runtime
            .runs()
            .get_run(&id, &stranded)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(
            abandoned.status,
            RunStatus::Failed,
            "an attempt whose cycle was refused must not stay Pending"
        );
        assert_eq!(
            abandoned.error.as_deref(),
            Some(RUNTIME_REPLACED_ERROR),
            "and it must say the runtime was swapped, not that the host died"
        );
        assert!(
            abandoned.started_at_millis.is_none(),
            "it never started, so it has no start time"
        );
        assert!(abandoned.finished_at_millis.is_some());
    }

    /// Issue #1852 Part 1 — the discard bug and its fix, proven directly on
    /// `run_dispatch_cycle` rather than on any one `Brain`'s output shape.
    ///
    /// `RelayBrain` answers a `TaskDispatched` event with exactly the shape
    /// `relay_reply` (`harness::built_in::lifecycle`) produces: a bubble whose
    /// `reply_to` names the origin thread and whose `task_id` names the card
    /// — without standing up a real harness or LLM. Before this fix,
    /// `run_dispatch_cycle` discarded the `CycleReport` carrying it (`let
    /// Err(err) = self.run_cycle(...).await else { return; }`), which is the
    /// generic bug underneath #1852, independent of which `Brain` produced
    /// the relay: reverting `run_dispatch_cycle` to that shape reproduces the
    /// failure this test now guards — zero `AgentReply` events land in the
    /// origin thread, because nothing ever journals the discarded report.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_dispatched_cards_relay_is_journaled_into_its_origin_thread() {
        use std::sync::Arc;

        use crate::ports::Brain;
        use crate::ports::TaskRecord;
        use crate::ports::brain::CycleHost;
        use crate::ports::tasks::COLUMN_IN_PROGRESS;
        use crate::ports::types::{
            CycleRequest, CycleResult, EventSeq, OutboundMessage, ReplyTo, TokenUsage,
        };

        /// Answers a `TaskDispatched { task_id: "t-1" }` with a
        /// `relay_reply`-shaped bubble; silent on everything else, mirroring
        /// `EchoBrain`'s silence on `TaskDispatched`.
        struct RelayBrain;

        #[async_trait::async_trait]
        impl Brain for RelayBrain {
            async fn run_cycle(
                &self,
                req: CycleRequest,
                _host: &dyn CycleHost,
            ) -> crate::Result<CycleResult> {
                let mut channel_responses = Vec::new();
                for event in &req.events {
                    if let CompanyEvent::TaskDispatched { task_id, .. } = event
                        && task_id == "t-1"
                    {
                        channel_responses.push(OutboundMessage {
                            message_id: None,
                            task_id: Some("t-1".to_string()),
                            channel: "ceo".to_string(),
                            agent: None,
                            text: "\"Ship it\" is ready for review (ceo ran it).".to_string(),
                            mentions: Vec::new(),
                            reply_to: Some(ReplyTo {
                                chat_id: "strategy".to_string(),
                            }),
                            steps: Vec::new(),
                        });
                    }
                }
                Ok(CycleResult {
                    channel_responses,
                    new_traces: Vec::new(),
                    ledger_deltas: Vec::new(),
                    token_usage: TokenUsage::default(),
                })
            }
        }

        let home_dir = tempfile::Builder::new()
            .prefix("opencompany-relay-journal-")
            .tempdir()
            .expect("tempdir");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = crate::ports::types::CompanyId::new("acme");
        let runtime = Arc::new(
            crate::runtime::RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
                .with_id(id.clone())
                .with_brain(Arc::new(RelayBrain))
                .build()
                .await
                .expect("runtime"),
        );

        let card = TaskRecord {
            id: "t-1".to_string(),
            title: "Ship it".to_string(),
            note: None,
            column: COLUMN_IN_PROGRESS.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 0,
            // The field the whole bug turns on: without an origin thread,
            // `relay_reply` is never called at all (a board-created card).
            origin_chat_id: Some("strategy".to_string()),
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        };

        let run_id = runtime.open_run(&card).await;
        Arc::clone(&runtime)
            .run_dispatch_cycle(card.id.clone(), run_id)
            .await;

        let events = runtime
            .events
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .expect("read journal");
        let relays: Vec<_> = events
            .iter()
            .filter_map(|stored| match &stored.event {
                CompanyEvent::AgentReply { chat_id, .. } if chat_id == "strategy" => {
                    Some(&stored.event)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            relays.len(),
            1,
            "exactly one relay must land in the origin thread, found {relays:?}"
        );
        let CompanyEvent::AgentReply {
            agent_id, task_id, ..
        } = relays[0]
        else {
            unreachable!()
        };
        assert_eq!(
            agent_id, "ceo",
            "the orchestrator answers for its own roster (issue #885 fallback)"
        );
        assert_eq!(
            task_id, &None,
            "the settle already has its own card link — `DeskTaskCompleted`'s \
             \"finished → …\" pill (issue #377) — so this bubble must not carry \
             its own \"Card opened\" chip alongside it"
        );
        assert!(
            crate::server::chat_history::owns("strategy", "Strategy", relays[0]),
            "the origin desk's own history read must pick this reply up"
        );
    }

    /// Issue #1852: the gate that stops a dispatch relay from being posted
    /// twice.
    ///
    /// A response the ordinary chat-turn cycle already journals through
    /// `journal_chat_replies` (`server::operator`) never carries `reply_to` —
    /// [`relay_reply`](crate::harness::built_in::lifecycle::relay_reply) is
    /// the only producer that sets it — so gating on that field structurally
    /// cannot re-journal a bubble the inline work-card path already wrote.
    /// The same absence covers a board-created card (no `origin_chat_id`):
    /// `run_task`/`refuse_dispatch` return no relay for one at all, which is
    /// this exact "no `reply_to`" shape.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn journal_dispatch_replies_only_touches_relay_shaped_responses() {
        use crate::CycleReport;
        use crate::ports::types::{EventSeq, OutboundMessage, ReplyTo};

        let (rt, _home_dir) = runtime_with_events().await;

        let report = CycleReport {
            responses: vec![
                // An ordinary chat-turn bubble: no `reply_to`, exactly what
                // `journal_chat_replies` already owns. Must not be touched
                // here, or the inline work-card path would double-post.
                OutboundMessage {
                    message_id: None,
                    task_id: None,
                    channel: "operator".to_string(),
                    agent: Some("ceo".to_string()),
                    text: "already handled elsewhere".to_string(),
                    mentions: Vec::new(),
                    reply_to: None,
                    steps: Vec::new(),
                },
                // A `reply_to` naming an empty chat id — not degenerate:
                // `origin_chat_id` preserves `Some("")` for a card spawned
                // from General, and `chat_history::same_conversation` treats
                // "" as an alias for General, so this must still journal.
                OutboundMessage {
                    message_id: None,
                    task_id: Some("t-2".to_string()),
                    channel: "ceo".to_string(),
                    agent: None,
                    text: "General-chat relay".to_string(),
                    mentions: Vec::new(),
                    reply_to: Some(ReplyTo {
                        chat_id: String::new(),
                    }),
                    steps: Vec::new(),
                },
                // The one shape `relay_reply` actually produces.
                OutboundMessage {
                    message_id: None,
                    task_id: Some("t-1".to_string()),
                    channel: "ceo".to_string(),
                    agent: None,
                    text: "\"Ship it\" is ready for review.".to_string(),
                    mentions: Vec::new(),
                    reply_to: Some(ReplyTo {
                        chat_id: "strategy".to_string(),
                    }),
                    steps: Vec::new(),
                },
            ],
            ..Default::default()
        };

        rt.journal_dispatch_replies(&report).await;

        let events = rt
            .events
            .read_from(&rt.id, EventSeq::new(0), usize::MAX)
            .await
            .expect("read journal");
        let relays: Vec<_> = events
            .iter()
            .filter_map(|stored| match &stored.event {
                CompanyEvent::AgentReply { .. } => Some(&stored.event),
                _ => None,
            })
            .collect();
        assert_eq!(
            relays.len(),
            2,
            "both reply_to-shaped responses must be journaled — an empty \
             chat_id is General, not absent — found {relays:?}"
        );
        let CompanyEvent::AgentReply {
            chat_id, task_id, ..
        } = relays
            .iter()
            .find(|event| matches!(event, CompanyEvent::AgentReply { chat_id, .. } if chat_id == "strategy"))
            .expect("the named-thread relay must be present")
        else {
            unreachable!()
        };
        assert_eq!(chat_id, "strategy");
        // Not `Some("t-1")`, even though the response itself carries it:
        // `journal_task_outcome` already marked "t-1" settled with its own
        // `DeskTaskCompleted` card link into this same thread, so this bubble
        // must not add a second one. See the drop site's own comment.
        assert_eq!(task_id, &None);

        let CompanyEvent::AgentReply { chat_id, .. } = relays
            .iter()
            .find(|event| matches!(event, CompanyEvent::AgentReply { chat_id, .. } if chat_id.is_empty()))
            .expect("the empty-chat_id General relay must be present")
        else {
            unreachable!()
        };
        assert_eq!(
            chat_id, "",
            "General's own empty chat_id must be preserved verbatim"
        );
    }

    /// Issue #435: the guard that decides whether a remembered thread root is
    /// still usable, and the direction it fails in.
    ///
    /// Every arm here degrades to `None`, which means "answer in the channel".
    /// That is the issue's stated requirement and it is not merely tidy: the
    /// console drops a reply whose parent it cannot resolve in the channel
    /// rather than rendering it flat, so a stale root would make the
    /// continuation invisible — strictly worse than the bug being fixed, since
    /// today's answer at least reaches the channel.
    /// A runtime with a live event log, for the thread-root tests. Returns the
    /// tempdir too: dropping it deletes the log the runtime is reading.
    async fn runtime_with_events() -> (crate::company::runtime::CompanyRuntime, tempfile::TempDir) {
        let home_dir = tempfile::Builder::new()
            .prefix("opencompany-parent-")
            .tempdir()
            .expect("tempdir");
        let manifest: crate::company::types::CompanyManifest = toml::from_str(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
            "#,
        )
        .expect("manifest");
        let rt = crate::runtime::RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
            .build()
            .await
            .expect("runtime");
        (rt, home_dir)
    }

    /// A helper effect and a manifest for the extend tests.
    fn extend_test_effect() -> crate::ports::types::Effect {
        crate::ports::types::Effect {
            kind: "payment.send".into(),
            group: crate::ports::types::EffectGroup::Spend,
            amount_usd: Some(1_200.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "vendor@example.test" }),
            agent: Some("ceo".into()),
            run_id: None,
        }
    }

    /// Seeds one parked approval into BOTH the live gate and the durable journal
    /// under a fixed id at `at_millis`, exactly as a real park leaves them — the
    /// gate answers "is this live?" for extend/sweep, the journal projects the
    /// deadline and replays on boot.
    async fn seed_parked(
        rt: &crate::company::runtime::CompanyRuntime,
        id: &str,
        at_millis: u64,
    ) -> crate::ports::types::ApprovalId {
        use crate::ports::types::ApprovalId;
        use crate::runtime::journal::{ApprovalConversation, TaskLink};
        let approval = ApprovalId::new(id);
        let effect = extend_test_effect();
        rt.approval_gate
            .rehydrate(approval.clone(), effect.clone(), at_millis);
        rt.journal
            .record_parked(
                &approval,
                &effect,
                at_millis,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        approval
    }

    /// Issue #1865 (Codex review on PR #1883): a late resolve that discovers
    /// an approval already past its deadline owes the SAME "expired
    /// unanswered" notification the sweep loop files when it discovers the
    /// identical deadline first.
    ///
    /// `notify_approval_expired` used to be invoked from nowhere but
    /// `sweep_expired_approvals`, so `retire_if_expired` — the path a late
    /// `resolve_approval_spawned`/`resolve_approval_amended_spawned` takes
    /// when `settle_approval` answers `ResolveReceipt::Expired` — ran the
    /// whole four-step `retire_approval` transaction and never told anybody.
    /// The exact same expiry notified when the sweeper found it and stayed
    /// silent when an operator's late click found it instead.
    #[tokio::test]
    async fn a_late_resolve_that_discovers_an_expiry_files_the_same_notification_as_the_sweep() {
        use crate::ports::types::{Actor, ActorKind, Verdict};
        use crate::runtime::grants::GrantScope;
        use std::sync::Arc;

        let (rt, _home) = runtime_with_events().await;
        let rt = Arc::new(rt);
        // Parked at epoch 0 — unambiguously past any TTL, the same trick
        // `expired_approval_is_labelled_as_an_expiry_and_carries_its_wait`
        // (src/server/ops/write_test.rs) uses.
        let id = seed_parked(&rt, "appr-late", 0).await;

        let by = Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        };
        let (receipt, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, by, GrantScope::Once)
            .await
            .unwrap();
        assert!(
            receipt.expired(),
            "an epoch-0 park must read as expired, not approved: {receipt:?}"
        );
        super::join_follow_up(follow_up).await.unwrap();

        let notifications = rt.notifications().list(rt.id(), "owner").await.unwrap();
        assert!(
            notifications
                .iter()
                .any(|n| n.notification.kind == "approval_expired"
                    && n.notification.subject.id == id.as_ref()),
            "a late resolve that discovers an expiry must file the same \
             approval_expired notification the sweep files, got {notifications:?}"
        );
    }

    /// Issue #971 (the projection this issue builds on): a card's deadline is the
    /// deadline anchor plus the gate's TTL, resolved once at the single
    /// projection point.
    #[tokio::test]
    async fn pending_approvals_projects_deadline_as_anchor_plus_ttl() {
        let (rt, _home) = runtime_with_events().await;
        seed_parked(&rt, "appr-deadline", 5_000).await;
        let ttl = rt.approval_gate.ttl_millis();
        assert_eq!(
            rt.pending_approvals()[0].expires_at_millis,
            Some(5_000 + ttl),
            "a fresh card's deadline runs from when it was parked"
        );
    }

    /// **The load-bearing extend test (issue #1805).** Extending moves the live
    /// deadline, and — the half that a redeploy silently reverted before this —
    /// the move survives a rebuild of the runtime from the same journal, because
    /// the extension is replayed and the gate is rehydrated from the moved anchor.
    #[tokio::test]
    async fn extend_approval_moves_deadline_and_survives_replay() {
        use crate::ports::types::{Actor, ActorKind};

        let home_dir = tempfile::Builder::new()
            .prefix("opencompany-extend-replay-")
            .tempdir()
            .expect("tempdir");
        let manifest: crate::company::types::CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"supervised\"\n")
                .expect("manifest");

        // First boot: park an old approval, confirm its original deadline, extend.
        let rt1 =
            crate::runtime::RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest.clone())
                .build()
                .await
                .expect("runtime");
        let id = seed_parked(&rt1, "appr-replay", 1_000).await;
        let ttl = rt1.approval_gate.ttl_millis();
        assert_eq!(
            rt1.pending_approvals()[0].expires_at_millis,
            Some(1_000 + ttl),
            "the fresh deadline runs from the park instant"
        );

        let new_deadline = rt1
            .extend_approval(
                &id,
                Actor {
                    kind: ActorKind::User,
                    id: "operator".into(),
                },
            )
            .await
            .expect("extend");
        assert!(
            new_deadline > 1_000 + ttl,
            "the live deadline moved out: {new_deadline} vs {}",
            1_000 + ttl
        );
        assert_eq!(
            rt1.pending_approvals()[0].expires_at_millis,
            Some(new_deadline),
            "the live projection reflects the extension immediately"
        );
        drop(rt1);

        // Second boot from the SAME journal — the redeploy the extension has to
        // survive. Without the replayed `ApprovalExtended` the deadline would
        // revert to `1_000 + ttl`.
        let rt2 = crate::runtime::RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest)
            .build()
            .await
            .expect("runtime");
        let replayed = rt2.pending_approvals();
        assert_eq!(
            replayed.len(),
            1,
            "the approval is still parked after a redeploy"
        );
        assert_eq!(
            replayed[0].expires_at_millis,
            Some(new_deadline),
            "the extended deadline survived the rebuild instead of reverting to the park window"
        );
        // The rehydrated gate enforces the extended window too: a sweep one tick
        // before the new deadline leaves it parked.
        assert!(
            rt2.approval_gate.sweep_expired(new_deadline - 1).is_empty(),
            "the rehydrated gate must enforce the extension, not the original park"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_thread_root_degrades_to_the_channel() {
        use crate::ports::types::{Actor, ActorKind, CompanyEvent, EventSeq};

        let (rt, _home_dir) = runtime_with_events().await;

        // A real root in `desk-finance`, and a second message elsewhere.
        let root = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "pay the invoice".into(),
                    by: None,
                    chat: Some("desk-finance".into()),
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("append");
        let elsewhere = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "unrelated".into(),
                    by: None,
                    chat: Some("desk-ops".into()),
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("append");

        // The good case: a root that exists, in the channel being answered.
        assert_eq!(
            rt.resolvable_parent(Some(root), "desk-finance").await,
            Some(root),
        );

        // No root recorded at all — the overwhelmingly common case, and the
        // pre-#435 behaviour.
        assert_eq!(rt.resolvable_parent(None, "desk-finance").await, None);

        // A root that resolves but lives in another channel. Renderable
        // nowhere, and proof the recorded pair was already inconsistent.
        assert_eq!(
            rt.resolvable_parent(Some(elsewhere), "desk-finance").await,
            None,
            "a root in another channel must not follow the answer across",
        );

        // A root that is simply GONE, with a live message after it.
        //
        // This is the case the exact-sequence check exists for, and it has to
        // be built deliberately. `read_from` returns events with sequence >=
        // the one asked for, so a vanished root comes back as its *successor*.
        // Asking past the end of the log proves nothing — that read is empty
        // and every implementation returns `None`. A genuine gap is what
        // separates "found it" from "found the next one", and the only thing
        // that makes gaps is pruning, which the events module documents as
        // leaving them by design.
        //
        // So: a prunable frame, then a real message in the channel, then a
        // pass that removes the first. Without the sequence check the message
        // answers for the hole underneath it — and it is in the right channel,
        // so the channel check waves it through.
        let doomed = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::WorkspaceChanged {
                    node_id: "n-1".into(),
                    change: "updated".into(),
                },
            )
            .await
            .expect("append");
        let after_the_hole = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "and another thing".into(),
                    by: None,
                    chat: Some("desk-finance".into()),
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("append");
        rt.events
            .prune(
                &rt.id,
                &crate::ports::events::RetentionPolicy {
                    max_entries_per_kind: Some(0),
                    ..Default::default()
                },
            )
            .await
            .expect("prune");
        // The hole is real, and the next event is a same-channel message.
        let successor = rt
            .events
            .read_from(&rt.id, doomed, 1)
            .await
            .expect("read")
            .into_iter()
            .next()
            .expect("the message after the hole answers the read");
        assert_eq!(
            successor.seq, after_the_hole,
            "the pruned sequence must genuinely be absent, answered by its successor",
        );
        assert_eq!(
            rt.resolvable_parent(Some(doomed), "desk-finance").await,
            None,
            "a vanished root must not be answered by the message that follows it",
        );

        // And past the end of the log, where the read is simply empty.
        let beyond = EventSeq::new(after_the_hole.value() + 500);
        assert_eq!(
            rt.resolvable_parent(Some(beyond), "desk-finance").await,
            None
        );

        // A sequence that resolves to something that is not a chat message at
        // all cannot root a thread either.
        let not_a_message = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::LifecycleChanged {
                    from: "idle".into(),
                    to: "running".into(),
                    by: Actor {
                        kind: ActorKind::Operator,
                        id: "owner".into(),
                    },
                },
            )
            .await
            .expect("append");
        assert_eq!(
            rt.resolvable_parent(Some(not_a_message), "desk-finance")
                .await,
            None,
        );
    }

    /// The General desk answers to four spellings, and a thread rooted in any
    /// of them keeps its parent (issue #435).
    ///
    /// This is the case the fix was *most* likely to be handed and originally
    /// dropped: the chat route journals an unaddressed message as
    /// `chat: None` while the console renders it under `General` and replies to
    /// it there, so the comparison arrived as `None` vs `"General"`. A raw
    /// string compare rejected it, the parent was discarded, and the
    /// continuation resumed in the channel — #435's own symptom surviving
    /// inside #435's fix, on the default path rather than an exotic one.
    #[tokio::test]
    async fn a_root_in_any_spelling_of_the_general_desk_still_resolves() {
        use crate::ports::types::CompanyEvent;

        let (rt, _home_dir) = runtime_with_events().await;

        // Three roots, one desk: the unaddressed post, the console's own
        // thread id, and the desk named outright.
        let mut roots = Vec::new();
        for chat in [None, Some("main"), Some("General")] {
            roots.push(
                rt.events
                    .append(
                        &rt.id,
                        CompanyEvent::OperatorMessage {
                            mentions: Vec::new(),
                            text: "ship it".into(),
                            by: None,
                            chat: chat.map(str::to_string),
                            parent: None,
                            deliverable: None,
                            attachments: Vec::new(),
                        },
                    )
                    .await
                    .expect("append"),
            );
        }

        // Every root resolves against every spelling of the channel it is
        // answered into — including the pair that used to fail.
        for root in &roots {
            for channel in ["General", "main", "general"] {
                assert_eq!(
                    rt.resolvable_parent(Some(*root), channel).await,
                    Some(*root),
                    "root {root} must resolve when answered into `{channel}`",
                );
            }
        }

        // …and the folding stops there. A real desk is still compared
        // verbatim, so this widening cannot pull an unrelated thread in.
        let elsewhere = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "unrelated".into(),
                    by: None,
                    chat: Some("desk-ops".into()),
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("append");
        assert_eq!(
            rt.resolvable_parent(Some(elsewhere), "General").await,
            None,
            "a named desk is not the General desk",
        );
        assert_eq!(
            rt.resolvable_parent(Some(roots[0]), "desk-ops").await,
            None,
            "and the General desk is not a named one",
        );
    }

    /// Issue #966: the failed-continuation report is authored by the runtime.
    ///
    /// This site appends the `AgentReply` itself, so it never sees
    /// `OutboundMessage::agent` or its `channel` fallback — it has to name the
    /// author, and it used to name `OPERATOR_CHANNEL`. That made a correct
    /// system row byte-identical on disk to a reply the pre-#885 defect had
    /// damaged, which is the finding recorded on #966.
    #[test]
    fn a_failed_continuation_report_is_authored_by_the_runtime_not_the_operator() {
        let event = continuation_failure_notice("desk-general".to_string(), None);
        let CompanyEvent::AgentReply {
            agent_id, chat_id, ..
        } = event
        else {
            panic!("the notice must stay an AgentReply — the console renders it from that arm");
        };
        assert_eq!(agent_id, crate::ports::SYSTEM_AUTHOR);
        assert_ne!(
            agent_id,
            crate::runtime::channel::OPERATOR_CHANNEL,
            "a notice must not store the author a destination-overwrite produces"
        );
        assert_eq!(
            chat_id, "desk-general",
            "it still lands in the thread it answers"
        );
    }

    /// Issue #1861 (found by Codex on #1905): a gate park that lands and then
    /// fails to journal must not leave the approval decidable.
    ///
    /// # The window
    ///
    /// `park_blocker` parks on the gate first and journals second. A `?` on the
    /// journal write reported the park as failed — so `settle_blocked` returned
    /// the card to To-do — while the gate still held a live, decidable entry
    /// against it. The operator is then shown a question for a card nobody
    /// paused, which is the exact inconsistency `unpark_blocker` exists to
    /// prevent on the other side of this pair.
    ///
    /// `record_parked` also populates the projection *before* its append, so
    /// the same failure left a pending approval that no journal line would ever
    /// replay: visible until the process exits, gone after a boot.
    ///
    /// Both are asserted here, because clearing one without the other just
    /// moves the disagreement.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_blocker_that_cannot_be_journaled_leaves_no_decidable_approval() {
        let home = tempfile::tempdir().expect("home");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
            "#,
        )
        .expect("manifest");
        let journal = std::sync::Arc::new(RefusingJournalStore::default());
        let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(crate::ports::types::CompanyId::new("acme"))
            .with_journal_store(journal.clone())
            .build()
            .await
            .expect("runtime");

        // The volume goes away *after* boot, so this is an ordinary runtime.
        journal.arm();

        let payload = crate::ports::blockers::BlockerPayload {
            kind: crate::ports::blockers::BlockerKind::Infrastructure,
            source: crate::ports::blockers::BlockerSource::Provider,
            step: Some(crate::ports::blockers::BlockerStep::Task {
                task_id: "t-1".to_string(),
            }),
            reason: "the model `gpt-nonexistent` was rejected".to_string(),
            needed: "a model id this provider serves".to_string(),
        };

        let parked = runtime.park_blocker(&payload, "t-1").await;
        assert!(
            parked.is_err(),
            "an unjournaled park is reported as a failed park, so the caller returns the card"
        );

        assert!(
            runtime.approval_gate.parked_ids().is_empty(),
            "the gate entry must be rolled back — otherwise the operator can decide a blocker \
             for a card that was handed straight back to To-do"
        );
        assert!(
            runtime.pending_approvals().is_empty(),
            "and the projection row `record_parked` inserted before its append must go with it"
        );
    }
}
