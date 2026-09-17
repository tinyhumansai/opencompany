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

/// Page size for the paged company-event scans in `settle_pill_before` and
/// `is_relay_bubble_for`, matching [`history_for_desk`](crate::server::chat_history::history_for_desk)'s
/// `EVENT_PAGE`.
#[cfg(feature = "openhuman")]
const RELAY_SCAN_PAGE: usize = 256;

/// The most pages either scan pages through before giving up — 2048 events
/// company-wide. Generous rather than tuned: a real reply-to-relay is at most
/// a handful of events away even in a busy company, so this is a safety cap
/// against a runaway scan (a desk that dispatched once and never again, or
/// never at all), not a bound expected to bite in practice.
#[cfg(feature = "openhuman")]
const RELAY_SCAN_MAX_PAGES: usize = 8;

/// Whether `card` is the review surface for `desk`: an `in_review`
/// dispatch-origin card whose origin conversation is `desk`. A board-created
/// card (no `origin_chat_id`) is excluded — it was never dispatched from a
/// thread and has no origin conversation to review it in.
#[cfg(feature = "openhuman")]
fn is_review_target(card: &TaskRecord, desk: &str) -> bool {
    card.column == crate::ports::tasks::COLUMN_IN_REVIEW
        && card.origin_chat_id().is_some_and(|origin| {
            crate::server::chat_history::same_conversation(Some(origin), Some(desk))
        })
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
    /// The platform managed default — see [`Self::platform_default`].
    pub(crate) platform_default: Option<crate::company::inference::EnvDefault>,
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
    /// Durable engine checkpoints for this company's workflow lineages.
    #[cfg(feature = "openhuman")]
    pub(crate) workflow_checkpoints:
        Option<Arc<crate::workflows::checkpoint_store::WorkflowCheckpointStore>>,
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
    /// Held across a blocker group's resolve loop so the group settles as a
    /// unit: two operators answering different members of one group cannot
    /// interleave their verdicts across it. The console's two-value path takes
    /// it too, so a legacy Approve/Deny and a four-way answer to the same
    /// blocker serialise rather than interleaving their claim and settle.
    ///
    /// **Not what makes a single blocker's answer safe.** That is
    /// `claim_blocker_resolution`, which arms the answer only into an empty
    /// slot and tells the caller whether it was the one to fill it, so a losing
    /// request returns having written neither the durable record nor the armed
    /// answer. Correctness per id lives in that one atomic step; this lock only
    /// decides how a *group* is batched, and dropping it would cost group
    /// atomicity rather than let two verdicts blur into one.
    ///
    /// Never acquired while [`task_writes`](Self::task_writes) is held, and
    /// never held across a resume: a member's follow-up is spawned, so it runs
    /// outside this lock and is free to take `task_writes` for the board edit
    /// its resume makes.
    ///
    /// `Arc`-shared for the same reason as [`serial`](Self::serial): a rebuild
    /// inherits it rather than minting a second one.
    pub(crate) blocker_resolutions: Arc<TokioMutex<()>>,
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
    /// The blocker twin of
    /// [`replay_continuations_on_register`](Self::replay_continuations_on_register):
    /// set when replay found a banked blocker answer whose approval is still
    /// parked, so the pair is driven once this runtime is addressable.
    replay_blockers_on_register: AtomicBool,
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
        audience: Vec::new(),
        parent,
        chat_id: thread,
        agent_id: crate::ports::SYSTEM_AUTHOR.to_string(),
        text: "Your approval was recorded, but the agent could not pick the work back up. \
               Nothing was half-done — approving again is safe and will retry it."
            .to_string(),
        steps: Vec::new(),
        task_id: None,
        outputs: Vec::new(),
        // A runtime notice addressed to whoever is reading it. It names no
        // teammate and no person, so there is nothing to chip and nobody to
        // ping.
        mentions: Vec::new(),
        mention_depth: 0,
    }
}

/// SPIKE (async hand-off): how many times one card may change hands before a
/// person is asked.
///
/// A hand-off chain terminates by construction rather than by nobody writing
/// one. `pub(crate)` so the harness tests can drive a chain the same number of
/// times production does, instead of hard-coding a number that drifts.
/// Both the cap's reader and the test that drives it are `openhuman`-only,
/// so the constant carries the same gate rather than reading as dead code
/// in a default build.
#[cfg(feature = "openhuman")]
pub(crate) const MAX_HAND_OFF_HOPS: usize = 3;

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
        let run_supervisor_gate = approval_gate.clone();
        let workflow_gates_gate = approval_gate.clone();
        Self {
            inert_board_reported: std::sync::atomic::AtomicBool::new(false),
            // Install-wide, not per-company, so it is set by the builder from
            // resolved config (`set_default_mcp_servers`) rather than taken as a
            // 19th positional argument here.
            default_mcp_servers: Vec::new(),
            // Likewise host-wide: `set_platform_default`.
            platform_default: None,
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
            #[cfg(feature = "openhuman")]
            workflow_checkpoints: None,
            steer: crate::company::steer::InflightRegistry::new(),
            run_supervisor: crate::runtime::RunSupervisor::new()
                .with_emergency_gate(run_supervisor_gate),
            grants,
            continuations: ContinuationQueue::default(),
            workflow_gates: WorkflowGateQueue::default().with_emergency_gate(workflow_gates_gate),
            blocked_nodes: BlockedNodeQueue::default(),
            serial: Arc::new(TokioMutex::new(())),
            per_agent: Arc::new(TokioMutex::new(HashMap::new())),
            task_writes: Arc::new(TokioMutex::new(())),
            blocker_resolutions: Arc::new(TokioMutex::new(())),
            quiesced: Arc::new(AtomicBool::new(false)),
            replay_continuations_on_register: AtomicBool::new(false),
            replay_blockers_on_register: AtomicBool::new(false),
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
        // `attach_harness` deliberately keeps an uncredentialed base Search
        // handle alive so a company key added later can share its ledger. The
        // handle's presence alone is therefore not proof that `web_search` is
        // usable. Resolve the effective company-or-deployment credential before
        // advertising workflow wiring.
        if resolved
            .search
            .as_ref()
            .is_some_and(|backend| !backend.credential.configured())
        {
            match crate::company::search::load_managed_key(&company.id, self.secrets().as_ref())
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => resolved.search = None,
                Err(err) => {
                    tracing::warn!(
                        company = %company.id,
                        "[search] could not read the managed company credential while resolving workflow wiring: {err}"
                    );
                    // Unknown is not unconfigured. Keep the base handle in the
                    // wiring verdict so callers do not misreport a transient
                    // secret-store outage as missing configuration.
                }
            }
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
    ///
    /// Re-gated on this runtime's emergency flag for
    /// [`adopt_workflow_gates`](Self::adopt_workflow_gates)' reason: the
    /// supervisor that reaches a runtime must refuse admission while the stop
    /// is engaged whether or not the site that built it remembered to say so.
    pub fn set_run_supervisor(&mut self, supervisor: crate::runtime::RunSupervisor) {
        self.run_supervisor = supervisor.with_emergency_gate(self.approval_gate.clone());
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

    /// The pass that names the work a card is opened for, when this company's
    /// brain has a model to name it with.
    ///
    /// `None` on an echo brain and in any build without the harness, which
    /// leaves [`mint_task_title`](crate::ports::tasks::mint_task_title) on the
    /// shortened request.
    pub fn titler(&self) -> Option<&dyn crate::ports::tasks::TitleSummariser> {
        self.brain.titler()
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

    /// The platform managed default this runtime resolves managed inference
    /// against: the endpoint on the platform this host was configured for
    /// (`api_url`), and the instance credential when the deployment has one.
    ///
    /// Set by the builder, which is also what hands the same value to the
    /// harness brain — so a console read and a turn cannot disagree about
    /// which platform "Managed" means. `None` only for a runtime assembled by
    /// hand around [`CompanyRuntime::new`]; every builder-made runtime has one.
    pub fn platform_default(&self) -> Option<&crate::company::inference::EnvDefault> {
        self.platform_default.as_ref()
    }

    /// See [`platform_default`](Self::platform_default).
    pub fn set_platform_default(&mut self, default: crate::company::inference::EnvDefault) {
        self.platform_default = Some(default);
    }

    /// This company's event log (append-only audit trail).
    /// The gate a [`JournalReferralQueue`](crate::runtime::hivemind::JournalReferralQueue)
    /// serialises its check-then-write under. One per company, so two referrals
    /// decided at once cannot both find the marker absent.
    #[cfg(feature = "hivemind")]
    pub(crate) fn referral_gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.task_writes.clone()
    }

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

    /// The company manifest's `[globals].disable` opt-outs, read from the
    /// persisted record. Empty when the record hasn't been saved yet.
    ///
    /// Read by both skill read paths so a manifest opt-out reaches them the way
    /// it reaches the harness — as a synthesized disabling delta, via
    /// [`globals_skill_disables`](crate::company::skill_effective::globals_skill_disables).
    pub async fn globals_disable(&self) -> Result<Vec<String>> {
        let record = self.store.load(&self.id).await?;
        Ok(record
            .map(|record| record.manifest.globals.disable)
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
            // `deploy/Dockerfile`'s `ARG FEATURES=""` ships it as a first-class
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
                 build arg in `deploy/Dockerfile`), or move the card by hand.";
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

    /// SPIKE (tinyhivemind P15): run a referred child turn on the target desk.
    ///
    /// A referral arrives on the target's desk channel as a MESSAGE authored by
    /// the agent that asked — which is what it is. That keeps one mechanism for
    /// "a turn happens because something arrived on this conversation" instead
    /// of a second, referral-only path, and it means the responder ladder picks
    /// the target exactly as it would for any other addressed message.
    ///
    /// Detached, like every other turn this runtime starts: the enqueue
    /// transaction has already committed its marker, and the caller must not
    /// wait on a model.
    #[cfg(feature = "hivemind")]
    pub(crate) fn spawn_referred_turn(
        self: Arc<Self>,
        desk: String,
        content: String,
        asker: String,
        // Where an answer goes home, on a crossing FORWARD. `None` on a return
        // — an answer that has arrived does not need carrying further.
        origin: Option<tinyhivemind_core::referral::ReferralOrigin>,
        // The forward this child is answering, by the journal sequence of its
        // marker — carried so the return can name it exactly. Two crossings
        // between the same desks to the same agent write geometrically
        // identical markers, so a return that searched for "the forward that
        // looks like mine" would pair one question with the other's answer.
        answers: Option<u64>,
        // How deep in the chain this turn sits. Its own replies are offered to
        // the referral pass at this depth, so a follow-up is one deeper than
        // the answer it follows and `max_hops` finally counts something.
        //
        // This was a hardcoded `1`, which read as "a referred turn is depth 1"
        // and is true only of the first one. Every later generation claimed
        // depth 1 as well, so the counter reset on each hop: two desks could
        // have passed a question back and forth forever without the policy ever
        // reaching its limit. Nothing drove that loop at the time — the asker
        // had no way to ask again — so it cost nothing until it would have cost
        // everything.
        hop: u32,
    ) {
        tokio::spawn(async move {
            let desk_for_replies = desk.clone();
            let event = CompanyEvent::OperatorMessage {
                text: content,
                // The asking AGENT, not the operator: a referral is a teammate
                // asking, and recording it as an operator message would put
                // words in a person's mouth.
                by: Some(crate::ports::types::Actor {
                    kind: crate::ports::types::ActorKind::Agent,
                    id: asker,
                }),
                chat: Some(desk),
                parent: None,
                // Never a card: a referral asks a question, it does not hand
                // work over. Ownership moving is the hand-off path's job.
                deliverable: Some(crate::ports::types::MessageIntent::Chat),
                mentions: Vec::new(),
                attachments: Vec::new(),
            };
            // `run_cycle` journals its INPUT events and returns the replies —
            // it does not write them down. The chat route journals its own
            // (`journal_chat_replies`) and the dispatch cycle journals its own
            // (`journal_dispatch_replies`); a third caller needs the same, or
            // the referred agent answers into a transcript nobody can read.
            match self.run_cycle(vec![event]).await {
                Ok(mut report) => {
                    let company = self.id.clone();
                    crate::server::operator::journal_chat_replies(
                        &self,
                        &company,
                        &desk_for_replies,
                        None,
                        &mut report,
                    )
                    .await;
                    // The back edge. This turn's reply is the ANSWER to the
                    // referral that caused it, so it is offered to the decision
                    // carrying the origin it must return to, at its own depth.
                    crate::server::operator::refer_committed_replies(
                        &self,
                        &company,
                        &desk_for_replies,
                        &report,
                        origin,
                        answers,
                        hop,
                    )
                    .await;
                }
                Err(err) => tracing::warn!(error = %err, "[referral] the referred turn failed"),
            }
        });
    }

    /// The body of a dispatch's detached cycle (issue #242), split out of the
    /// `tokio::spawn` so the quiesce path below is reachable from a test.
    ///
    /// Owns the settle for the one dispatch failure the cycle's own terminality
    /// backstop cannot see — see [`abandon_run`](Self::abandon_run).
    #[cfg(feature = "openhuman")]
    async fn run_dispatch_cycle(self: Arc<Self>, task_id: String, run_id: Option<String>) {
        let mut run_id = run_id;
        let mut hops = 0usize;
        let mut handed_back: Option<(String, String)> = None;
        loop {
            // Who owns the card going INTO this attempt. Read here and not after
            // the cycle, because by then a hand-off has already overwritten it —
            // which is exactly the value a rollback needs.
            // **A failed read is not an absent card.**
            //
            // `unwrap_or_default` was tolerable while this only fed
            // `owner_before`, where an empty owner is a survivable fallback.
            // The origin is different: a swallowed error silently omits the
            // conversation, and the dispatch then renders as the very silence
            // this change removes — indistinguishable from a board-created
            // card that genuinely has no thread (tinysweeper, #2369).
            //
            // Still not fatal, for the reason the rest of this path gives:
            // record-keeping does not fail the work it records. It is logged,
            // so an omitted origin has a cause a reader can find instead of
            // looking like a card that was never raised in a conversation.
            let cards = match self.ops.tasks.list(&self.id).await {
                Ok(cards) => cards,
                Err(error) => {
                    tracing::warn!(
                        company = %self.id,
                        task = %task_id,
                        error = %error,
                        "[dispatch] the board could not be read; this attempt runs without its \
                         owner or its originating conversation"
                    );
                    Vec::new()
                }
            };
            let card_before = cards.into_iter().find(|c| c.id == task_id);
            let owner_before = card_before
                .as_ref()
                .map(|c| c.assignee.clone())
                .unwrap_or_default();
            // **The conversation this attempt answers, carried onto the
            // dispatch itself.**
            //
            // `DeskTaskCompleted` already stamps this pair, so a finished run is
            // delivered into the thread that asked. The dispatch carried
            // neither, so the *start* of the work reached the console naming
            // only a card — and a thread that dispatched went silent from that
            // moment until the answer arrived, because the chat turn had
            // genuinely succeeded (it handed the work over) and its working row
            // settled with it. Minutes of a real agent turn rendered as nothing,
            // then a reply from nowhere.
            //
            // Read from the card rather than threaded through the call: the card
            // is where `TaskOrigin` is recorded, and a second place deciding
            // "which conversation is this?" is the drift #435 removed.
            let origin = card_before.as_ref().and_then(|c| c.origin.clone());
            let report = match self
                .run_cycle(vec![CompanyEvent::TaskDispatched {
                    task_id: task_id.clone(),
                    run_id: run_id.clone(),
                    origin_chat_id: origin.as_ref().map(|o| o.origin_chat_id.clone()),
                    origin_parent: origin.as_ref().and_then(|o| o.origin_parent),
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

            // ── SPIKE (async hand-off): re-dispatch for the card's new owner ────
            //
            // A settled dispatch that leaves its card in `in_progress` is the
            // hand-off shape and nothing else: every other ending lands the card in
            // a terminal column (`in_review`, `todo`, `paused`), and
            // `settled_landing_column(Delegated)` deliberately keeps it here
            // because "a hand-off is not an ending".
            //
            // The dispatch EDGE cannot fire on its own for this: it triggers on
            // `→ in_progress`, and the card never left. So the re-dispatch is
            // driven from here, where a fresh attempt row is minted for the
            // delegate — which is the whole point. One attempt per agent, the
            // serial cycle lock released between hops, and cost attributable to
            // whoever actually spent it.
            //
            // Bounded by the hand-offs already recorded on the card, so a chain
            // terminates by construction rather than by nobody writing one.
            // SPIKE: undo a hand-off whose delegate could not run.
            //
            // `refuse_dispatch` bounces the card to To-do with the reason, but it
            // leaves `assignee` naming the delegate — the state that makes the card
            // permanently unrunnable and the thread permanently misrouted. Rolling
            // the owner back is what makes the hand-off transactional: it either
            // moved the work or it did not.
            if let Some((delegate, prior)) = handed_back.take()
                && !prior.is_empty()
                && let Ok(cards) = self.ops.tasks.list(&self.id).await
                && let Some(mut card) = cards.into_iter().find(|c| c.id == task_id)
                && card.column == crate::ports::tasks::COLUMN_TODO
                && card.bounced.is_some()
                && card.assignee == delegate
            {
                tracing::warn!(
                    company = %self.id,
                    task = %task_id,
                    from = %delegate,
                    to = %prior,
                    "[hand-off] the delegate could not run; returning the card to its previous owner"
                );
                card.assignee = prior;
                card.updated_at_millis = crate::ports::now_millis();
                // The plain store port: this must not re-fire the dispatch edge.
                let _ = self.ops.tasks.upsert(&self.id, &card).await;
            }

            let handed_on = self
                .ops
                .tasks
                .list(&self.id)
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|card| card.id == task_id)
                .filter(|card| card.column == crate::ports::tasks::COLUMN_IN_PROGRESS);
            if let Some(card) = handed_on {
                if hops >= MAX_HAND_OFF_HOPS {
                    tracing::warn!(
                        company = %self.id,
                        task = %task_id,
                        hops,
                        "[hand-off] chain hit its cap; leaving the card for a person"
                    );
                    return;
                }
                hops += 1;
                // Who owned it before this hop, so a hand-off that cannot run can
                // be undone. Without this the card keeps an owner that never took
                // the work: it bounces to To-do assigned to an agent this build
                // cannot dispatch, every retry fails identically, and — since
                // `assignee` is what the thread's overseer is read from — the
                // conversation is redirected to them permanently.
                handed_back = Some((card.assignee.clone(), owner_before.clone()));
                tracing::info!(
                    company = %self.id,
                    task = %task_id,
                    assignee = %card.assignee,
                    hops,
                    "[hand-off] re-dispatching for the card's new owner"
                );
                // A FRESH attempt row for the delegate — this is what makes cost
                // attributable per agent instead of one figure spanning the chain.
                // Looping rather than recursing keeps the whole chain on this one
                // spawned task, and `run_cycle` takes and releases the per-company
                // serial lock per iteration, so the company is not parked for the
                // duration of the chain.
                run_id = self.open_run(&card).await;
                continue;
            }
            return;
        }
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
        signals: crate::company::blocker_sender::BlockerSenderSignals,
    ) -> Result<ApprovalId> {
        use crate::ports::types::{Effect, EffectGroup};
        use crate::runtime::journal::{ApprovalConversation, TaskLink};

        // Who this question is asked by, and therefore which DM it surfaces in.
        // Resolved before the park so the same thread stamps the journal, the
        // event and the notification — a card, a routed reply and a badge that
        // cannot land in three different places.
        let sender = self.resolve_blocker_sender(&signals).await;
        let thread = crate::company::blocker_sender::dm_thread(&sender);

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
                // The blocker's own conversation is the DM with the teammate it
                // is attributed to. There is no parking turn behind it, so there
                // is no message root to thread under — only the channel the
                // reply routes into.
                ApprovalConversation {
                    thread: Some(thread.clone()),
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
                    thread: Some(thread.clone()),
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
        self.notify_blocker_parked(&approval_id, &sender, payload)
            .await;
        Ok(approval_id)
    }

    /// The teammate a blocker is attributed to, resolved from the live company
    /// record (issue #1862). Falls back to the host identity when the record
    /// cannot be loaded — a blocker must always land in a real DM channel, and
    /// a transient store miss is not a reason to drop it on the floor.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn resolve_blocker_sender(
        &self,
        signals: &crate::company::blocker_sender::BlockerSenderSignals,
    ) -> String {
        match self.store().load(&self.id).await {
            Ok(Some(record)) => crate::company::blocker_sender::resolve_sender(&record, signals),
            _ => crate::company::blocker_sender::HOST_SENDER.to_string(),
        }
    }

    /// Files the durable "someone is blocked" notification beside the park
    /// (issue #1862), the sibling of
    /// [`notify_approval_expired`](Self::notify_approval_expired).
    ///
    /// **Title only, and deliberately no payload.** The one-line title is the
    /// operator-readable prose the blocker already carries in `reason`; the
    /// arguments that produced the stop stay redacted at their single point
    /// (`pending_approvals`), exactly as the thin `ApprovalParked` frame keeps
    /// them. `context` is the DM channel, so a badge lands on the right
    /// conversation without the console having loaded its transcript.
    ///
    /// Says a person is blocked, not that answering resumes anything: resume is
    /// #1863, and the copy must not promise it.
    #[cfg(feature = "openhuman")]
    async fn notify_blocker_parked(
        &self,
        id: &ApprovalId,
        sender: &str,
        payload: &crate::ports::blockers::BlockerPayload,
    ) {
        use crate::ports::blockers::BlockerStep;
        let step = match &payload.step {
            Some(BlockerStep::Task { task_id }) => task_id.clone(),
            Some(BlockerStep::Node { node_id, .. }) => node_id.clone(),
            None => "a question".to_string(),
        };
        let note = crate::ports::notifications::Notification {
            id: crate::ports::generate_id(),
            kind: "blocker_parked".to_string(),
            subject: crate::ports::notifications::Subject {
                kind: crate::ports::notifications::SubjectKind::Approval,
                id: id.as_ref().to_string(),
            },
            created_at: now_millis(),
            title: format!("{sender} is blocked on {step}: {}", payload.reason),
            audience: None,
            context: Some(crate::company::blocker_sender::dm_thread(sender)),
        };
        if let Err(err) = self.notifications().append(&self.id, &note).await {
            tracing::warn!(
                company = %self.id,
                approval = %id.as_ref(),
                error = %err,
                "[blockers] a blocker-parked notification could not be recorded; the park \
                 still stands, but nobody is badged for it"
            );
        }
    }

    /// Files the durable blocker-answer notification on the original DM thread.
    /// Best-effort: the resolution already stands if this write fails.
    #[cfg(feature = "openhuman")]
    async fn notify_blocker_resumed(
        &self,
        id: &ApprovalId,
        thread: Option<&str>,
        resolution: &crate::ports::blockers::BlockerResolution,
        step: Option<&crate::ports::blockers::BlockerStep>,
    ) {
        use crate::ports::blockers::{BlockerStep, BlockerVerdict};
        let Some(thread) = thread else {
            return;
        };
        let sender = thread.strip_prefix("dm:").unwrap_or(thread);
        let step_id = match step {
            Some(BlockerStep::Task { task_id }) => task_id.clone(),
            Some(BlockerStep::Node { node_id, .. }) => node_id.clone(),
            None => "a question".to_string(),
        };
        let title = if resolution.verdict == BlockerVerdict::Skip
            && matches!(step, Some(BlockerStep::Task { .. }))
        {
            format!("{sender}'s blocker on {step_id} was waived")
        } else {
            format!("{sender}'s blocker on {step_id} was answered — picking it back up")
        };
        let note = crate::ports::notifications::Notification {
            id: crate::ports::generate_id(),
            kind: "blocker_resumed".to_string(),
            subject: crate::ports::notifications::Subject {
                kind: crate::ports::notifications::SubjectKind::Approval,
                id: id.as_ref().to_string(),
            },
            created_at: now_millis(),
            title,
            audience: None,
            context: Some(thread.to_string()),
        };
        if let Err(err) = self.notifications().append(&self.id, &note).await {
            tracing::warn!(
                company = %self.id,
                approval = %id.as_ref(),
                error = %err,
                "[blockers] a blocker-resumed notification could not be recorded; the resume \
                 still stands, but nobody is badged for it"
            );
        }
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
    /// (issue #290), so the cycle, board-write and blocker-resolution
    /// invariants span the swap instead of lapsing at it.
    ///
    /// Called by the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) on a
    /// rebuild, before the successor is registered and therefore before anything
    /// can be holding either lock through *this* runtime.
    pub fn adopt_locks(
        &mut self,
        serial: Arc<TokioMutex<()>>,
        per_agent: Arc<TokioMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
        task_writes: Arc<TokioMutex<()>>,
        blocker_resolutions: Arc<TokioMutex<()>>,
    ) {
        self.serial = serial;
        self.per_agent = per_agent;
        self.task_writes = task_writes;
        self.blocker_resolutions = blocker_resolutions;
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
    ///
    /// The adopted queue is re-gated on this runtime's own emergency flag. A
    /// queue arrives here from two places that both have reason not to carry
    /// one — a boot builds a fresh queue to rehydrate parked gates into, and a
    /// rebuild clones the outgoing runtime's — so requiring each construction
    /// site to remember the gate makes the stop hold only where someone
    /// remembered. Re-gating on adoption is the one place that cannot be
    /// forgotten, because it is the only way a queue reaches a runtime.
    pub fn adopt_workflow_gates(&mut self, gates: WorkflowGateQueue) {
        self.workflow_gates = gates.with_emergency_gate(self.approval_gate.clone());
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

    #[cfg(feature = "openhuman")]
    pub fn set_workflow_checkpoints(
        &mut self,
        checkpoints: Arc<crate::workflows::checkpoint_store::WorkflowCheckpointStore>,
    ) {
        self.workflow_checkpoints = Some(checkpoints);
    }

    #[cfg(feature = "openhuman")]
    pub(crate) fn workflow_checkpoints(
        &self,
    ) -> Option<&Arc<crate::workflows::checkpoint_store::WorkflowCheckpointStore>> {
        self.workflow_checkpoints.as_ref()
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
        // The emergency stop is checked first because the two refusals mean
        // opposite things to a caller: `Quiescing` is a `503` that says retry in
        // a moment, and retrying is exactly wrong here.
        self.ensure_not_emergency_stopped()?;
        if self.is_quiesced() {
            return Err(OpenCompanyError::Quiescing(self.id.as_ref().to_string()));
        }
        Ok(())
    }

    /// Refuses work while the emergency stop is engaged.
    ///
    /// The effect gate
    /// ([`evaluate`](crate::ports::approvals::ApprovalGate::evaluate) /
    /// [`park`](crate::ports::approvals::ApprovalGate::park)) refuses the
    /// *effects* a turn asks for; this refuses the turn. Both are needed — a
    /// company whose effects are denied but whose turns keep running still
    /// executes tools and still bills inference, while reporting itself stopped.
    ///
    /// Enforced at the four doorways work enters through, each the sole
    /// entrance of its family:
    ///
    /// * [`ensure_accepting`](Self::ensure_accepting) — every ingress asking for
    ///   a cycle (chat, ACP, the scheduler, a rebuild, an approval resolution).
    /// * [`spawn_follow_up`](Self::spawn_follow_up) — every resume a settled or
    ///   expired verdict owes: a brain continuation, a released blocker, a
    ///   workflow replay, a blocked node.
    /// * [`reconcile_stranded_blocked_nodes`](Self::reconcile_stranded_blocked_nodes) —
    ///   the boot-time resume, which reaches a dispatch through neither of the
    ///   other two.
    /// * [`run_planning_pass`](crate::harness::built_in::planning::run_planning_pass) —
    ///   a task's own paid-model doorway. It never goes through `run_cycle`, and
    ///   a card reaches `Planning` through a plain board write
    ///   ([`upsert_task`](Self::upsert_task)), which leaves lifecycle `running`
    ///   even while stopped, so none of the other three ever see it.
    ///
    /// # Semantics
    ///
    /// This halts the **admission** of work, not work already executing. A turn
    /// running when the switch is pulled is not killed: it holds live model
    /// context and half-written state, and the effect gate above already denies
    /// the consequential actions it can still ask for, which is the containment
    /// that matters. What it cannot do is start anything new.
    ///
    /// A parked approval is likewise frozen rather than resolved: while stopped
    /// the queue takes no new parks, no verdicts and no extensions, so its cards
    /// run down the deadline they already had to the default-deny the TTL
    /// already promised.
    ///
    /// Nothing on the release path consults this — [`emergency_resume`](Self::emergency_resume)
    /// and [`status`](Self::status) are reachable while stopped — because a
    /// company that cannot resume is a worse failure than one that cannot stop.
    pub(crate) fn ensure_not_emergency_stopped(&self) -> Result<()> {
        if self.approval_gate.is_emergency() {
            return Err(OpenCompanyError::EmergencyStop(format!(
                "{} is stopped and will run no work until an operator releases it",
                self.id.as_ref()
            )));
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
        #[cfg(feature = "openhuman")]
        let _resolving = self.blocker_resolutions.lock().await;
        #[cfg(feature = "openhuman")]
        let armed = self.arm_console_blocker_resolution(id, verdict).await?;
        // A build with no blocker resume never arms one; the releases below are
        // no-ops there rather than a second code path to keep in step.
        #[cfg(not(feature = "openhuman"))]
        let armed = false;
        // Every exit below that leaves no resume behind it gives the claim back,
        // the compensation `settle_claimed_blocker` makes for the four-way path.
        // An error keeps the durable record, which is what the next boot re-arms
        // and drives; a receipt that settled nothing retires it, because there is
        // no park left for a boot to match it to.
        let receipt = match CycleRunner::new(self)
            .settle_approval(id, verdict, by, scope)
            .await
        {
            Ok(receipt) => receipt,
            Err(err) => {
                self.release_console_blocker_claim(id, armed);
                return Err(err);
            }
        };
        if let Err(err) = self.retire_if_expired(id, &receipt).await {
            self.release_console_blocker_claim(id, armed);
            return Err(err);
        }
        #[cfg(feature = "openhuman")]
        if armed && !matches!(receipt, ResolveReceipt::Settled(_)) {
            self.retire_unresumed_console_claim(id).await;
        }
        Ok((receipt.clone(), self.spawn_follow_up(receipt)))
    }

    /// Arms a parked blocker's resolution when the answer arrives through the
    /// console approvals path rather than a DM reply (issue #2008).
    ///
    /// [`apply_blocker_reply`](Self::apply_blocker_reply) is the only other place
    /// that banks a [`BlockerResolution`], and until this the console's
    /// `Approve`/`Deny` reached [`settle_approval`](crate::runtime::cycle::CycleRunner::settle_approval)
    /// with nothing armed: the blocker's effect settled inert (the #1861
    /// never-execute guard) and [`spawn_follow_up`](Self::spawn_follow_up)'s
    /// [`take_blocker_resolution`](crate::runtime::grants::GrantSet::take_blocker_resolution)
    /// found none, so the run fell through to a plain `continue_turn` and the card
    /// never left `paused`. Arming here routes it into the same resume fork a DM
    /// answer takes.
    ///
    /// Runs **before** the settle, the restart-durable ordering
    /// `apply_blocker_reply` keeps, and reads the step off the still-parked
    /// payload before the settle scrubs it. Two guards keep it from touching
    /// anything else:
    /// * an id that is not a genuinely-parked blocker arms nothing, so an
    ///   ordinary approval, an already-resolved id or an expired one is
    ///   untouched — and a resolution is never left banked for a settle that
    ///   returns `AlreadyResolved`/`Expired` and so never consumes it, which a
    ///   later boot would otherwise re-arm and resume a second time;
    /// * [`claim_blocker_resolution`](crate::runtime::grants::GrantSet::claim_blocker_resolution)
    ///   is what arms, so a slot the four-way path already filled — its richer
    ///   answer, since an amend carries the operator's words — is left alone.
    ///
    /// The claim is the same one `claim_and_settle_blocker` takes, and for the
    /// same reason: testing the slot and filling it must be one atomic step.
    /// Reading it empty, awaiting the journal write and then inserting
    /// unconditionally let a four-way request claim and settle inside that
    /// await, and this path overwrote the winner's resolution on the way out —
    /// so the approval event recorded one verdict while the resume executed
    /// another. A claim that loses returns having written nothing, and one that
    /// wins but cannot bank releases the slot rather than orphaning it.
    ///
    /// Answers **whether this call was the one to arm it**, so the caller
    /// releases only a claim it took: a slot the four-way path already filled,
    /// or an id that is no parked blocker at all, is none of its business.
    #[cfg(feature = "openhuman")]
    async fn arm_console_blocker_resolution(
        self: &Arc<Self>,
        id: &ApprovalId,
        verdict: Verdict,
    ) -> Result<bool> {
        use crate::ports::blockers::{BlockerPayload, BlockerResolution, BlockerVerdict};
        let Some(parked) = self.journal.pending().into_iter().find(|p| &p.id == id) else {
            return Ok(false);
        };
        if !crate::ports::blockers::is_blocker_effect(&parked.effect) {
            return Ok(false);
        }
        let step = serde_json::from_value::<BlockerPayload>(parked.effect.payload.clone())
            .ok()
            .and_then(|payload| payload.step);
        let verdict = match verdict {
            Verdict::Approve => BlockerVerdict::Retry,
            Verdict::Deny => BlockerVerdict::Cancel,
        };
        let resolution = BlockerResolution {
            verdict,
            answer: String::new(),
            step,
        };
        if !self.grants.claim_blocker_resolution(id, resolution.clone()) {
            return Ok(false);
        }
        if let Err(err) = self
            .journal
            .record_blocker_resolution(id, &resolution)
            .await
        {
            self.grants.take_blocker_resolution(id);
            return Err(err);
        }
        Ok(true)
    }

    /// Gives back a claim [`arm_console_blocker_resolution`](Self::arm_console_blocker_resolution)
    /// took, on a path that ends with no resume behind it.
    ///
    /// The durable record deliberately stays: the approval is still parked when
    /// a settle fails, so the next boot re-arms this answer and
    /// [`schedule_replayed_blocker_resolutions`](Self::schedule_replayed_blocker_resolutions)
    /// drives it. Erasing it here would turn a recoverable state into a lost
    /// decision.
    #[cfg(feature = "openhuman")]
    fn release_console_blocker_claim(&self, id: &ApprovalId, armed: bool) {
        if armed {
            self.grants.take_blocker_resolution(id);
        }
    }

    /// A build with no blocker resume arms nothing to give back.
    #[cfg(not(feature = "openhuman"))]
    fn release_console_blocker_claim(&self, _id: &ApprovalId, _armed: bool) {}

    /// Retires a console-armed blocker claim whose settle produced no resume.
    ///
    /// `AlreadyResolved` and `Expired` both leave `spawn_follow_up` returning
    /// early, so nothing will ever take the armed answer. The durable record
    /// goes with it: unlike a failed settle — which leaves the approval parked
    /// for [`schedule_replayed_blocker_resolutions`](Self::schedule_replayed_blocker_resolutions)
    /// to drive on the next boot — these receipts mean the park is gone, so a
    /// surviving `BlockerResolved` would be an orphan no boot could match.
    #[cfg(feature = "openhuman")]
    async fn retire_unresumed_console_claim(&self, id: &ApprovalId) {
        self.grants.take_blocker_resolution(id);
        if let Err(err) = self.journal.record_blocker_resumed(id).await {
            tracing::warn!(
                company = %self.id,
                approval = %id,
                error = %err,
                "could not retire a blocker answer whose settle found nothing to resume"
            );
        }
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
        let waiting_runs = self.parked_workflow_attempts(id).await;
        self.retire_approval(id, ExpiryReason::Ttl, now_millis())
            .await?;
        self.finish_expiry(id, is_blocker, unanswered, waiting_runs)
            .await;
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
            // Below this line every branch dispatches real work — a blocker
            // resume, a workflow replay, a brain continuation — so the emergency
            // stop is enforced once here rather than on each of them. The two
            // arms above return a synthetic report and start nothing, which is
            // why they sit on the other side of it.
            rt.ensure_not_emergency_stopped()?;
            // Issue #1863: a resolved blocker re-enters the stopped step rather
            // than redispatching a grant or running a brain continuation. The
            // answer was armed on the grant set's blocker side-channel by
            // `apply_blocker_reply`; taking it here both recognises the blocker
            // and consumes it, and every non-blocker resolution finds nothing
            // and falls through to `continue_turn` unchanged.
            #[cfg(feature = "openhuman")]
            if let CompanyEvent::ApprovalResolved { approval_id, .. } = &event
                && let Some(resolution) = rt.grants.take_blocker_resolution(approval_id)
            {
                return rt.resume_blocker(approval_id, resolution).await;
            }
            rt.continue_turn(event).await
        })
    }

    /// Applies a durable blocker verdict to its task, workflow node, or DM.
    /// Retry and amend re-dispatch task work; task skip and cancel settle it
    /// without another run. Node verdicts retain their workflow semantics.
    ///
    /// The step rides on the resolution itself — the journal scrubs a parked
    /// effect's payload, so it is captured at resolve time — while the DM thread
    /// is read off the approval's origin, which is not scrubbed. A blocker that
    /// carried no step of its own falls back to the card its approval is linked
    /// to, read from that same unscrubbed origin: see
    /// [`blocker_step_from_task_link`](Self::blocker_step_from_task_link). The
    /// answer is retired from the re-arm queue once re-entered, so the next boot
    /// does not resume it a second time.
    #[cfg(feature = "openhuman")]
    async fn resume_blocker(
        self: &Arc<Self>,
        approval_id: &ApprovalId,
        resolution: crate::ports::blockers::BlockerResolution,
    ) -> Result<CycleReport> {
        let conversation = self.journal.approval_conversation(approval_id);
        let thread = conversation
            .as_ref()
            .and_then(|conversation| conversation.thread.clone());
        let origin_parent = conversation
            .as_ref()
            .and_then(|conversation| conversation.parent);
        let step = match resolution.step.clone() {
            Some(step) => Some(step),
            None => self.blocker_step_from_task_link(approval_id).await?,
        };
        let outcome = self
            .drive_blocker_resume(&resolution, step.as_ref(), thread.as_deref(), origin_parent)
            .await;
        // Retire the armed answer whether or not the drive succeeded: a failed
        // resume is reported, not retried forever, and re-arming it would resume
        // twice on the next boot. Best-effort — the durable verdict already
        // stands.
        if let Err(err) = self.journal.record_blocker_resumed(approval_id).await {
            tracing::warn!(
                company = %self.id,
                %approval_id,
                error = %err,
                "[blockers] a blocker resumed but its consume-record failed; the next boot may \
                 re-arm the answer"
            );
        }
        outcome?;
        if resolution.resumes() {
            self.notify_blocker_resumed(approval_id, thread.as_deref(), &resolution, step.as_ref())
                .await;
        }
        Ok(CycleRunner::new(self).already_resolved_report())
    }

    /// The card a blocker stopped, when its own payload never named one.
    ///
    /// An agent's question parks with no
    /// [`BlockerStep`](crate::ports::blockers::BlockerStep): the tool holds
    /// neither a card nor a node. Where a board card's dispatch cycle raised it,
    /// the approval records that as its [`TaskLink`], the same key
    /// [`unanswered_blocker`](Self::unanswered_blocker) returns an expired
    /// blocker's card by — and for the same reason, that the journal has always
    /// maintained the link while the payload's step is a field a producer can
    /// omit.
    ///
    /// Read from the retained origins, never the pending set: the settle that
    /// precedes a resume is what empties that set, so by here the approval is no
    /// longer parked. An [`Unlinked`](TaskLink::Unlinked) record, a link written
    /// before the journal kept one, and an id the journal never saw are one
    /// answer — there is no card to re-enter, and the answer is carried back
    /// into the conversation instead.
    ///
    /// A link is weaker evidence than a step, so it is only followed to a card
    /// the board still holds and still has paused. A declared step names the
    /// thing that stopped; a link only says a card was in hand when the
    /// question was raised. Reading a link to a card that is gone — or to one
    /// an operator has since moved on, which both card resumes leave alone
    /// without a word — answers the operator with a report about a card, or
    /// with nothing at all, where what was asked for was an answer to a
    /// question.
    ///
    /// A board that cannot be read is not a board without the card: the error
    /// propagates, leaving the answer armed for the next attempt rather than
    /// retiring it against a resume that never re-entered anything.
    ///
    /// [`TaskLink`]: crate::runtime::journal::TaskLink
    #[cfg(feature = "openhuman")]
    async fn blocker_step_from_task_link(
        &self,
        id: &ApprovalId,
    ) -> Result<Option<crate::ports::blockers::BlockerStep>> {
        use crate::runtime::journal::TaskLink;

        let Some(Some(TaskLink::Task { id: task_id })) = self.journal.approval_task(id) else {
            return Ok(None);
        };
        let paused = self
            .ops
            .tasks
            .list(&self.id)
            .await?
            .into_iter()
            .any(|task| task.id == task_id && task.column == crate::ports::tasks::COLUMN_PAUSED);
        Ok(paused.then_some(crate::ports::blockers::BlockerStep::Task { task_id }))
    }

    /// Routes a resolved blocker to the right resume by its
    /// [`BlockerStep`](crate::ports::blockers::BlockerStep) (issue #1863).
    #[cfg(feature = "openhuman")]
    async fn drive_blocker_resume(
        self: &Arc<Self>,
        resolution: &crate::ports::blockers::BlockerResolution,
        step: Option<&crate::ports::blockers::BlockerStep>,
        thread: Option<&str>,
        origin_parent: Option<EventSeq>,
    ) -> Result<()> {
        use crate::ports::blockers::{BlockerStep, BlockerVerdict};

        match (step, resolution.verdict) {
            (Some(BlockerStep::Task { task_id }), BlockerVerdict::Skip) => {
                self.skip_task_card(task_id, thread, origin_parent).await
            }
            (Some(BlockerStep::Task { task_id }), BlockerVerdict::Cancel) => {
                self.cancel_task_card(task_id, thread, origin_parent).await
            }
            (
                Some(BlockerStep::Task { task_id }),
                BlockerVerdict::Retry | BlockerVerdict::Amend,
            ) => {
                self.resume_task_card(task_id, resolution, thread, origin_parent)
                    .await
            }
            (Some(BlockerStep::Node { run_id, node_id }), _) => {
                self.resume_node_blocker(run_id, node_id, resolution, thread, origin_parent)
                    .await
            }
            // A question with no card or node behind it — carrying the answer
            // back into its DM is the whole of the resume.
            (None, _) => {
                self.post_blocker_resume_note(
                    thread,
                    origin_parent,
                    &blocker_resume_note(resolution),
                )
                .await
            }
        }
    }

    /// Re-dispatches a board card a blocker had paused, moving it back into In
    /// Progress so its dispatch edge fires (issue #1863).
    ///
    /// The move rides through [`upsert_task`](Self::upsert_task), the one write
    /// site that carries the dispatch edge — the opposite choice from
    /// [`return_expired_blocker_card`](crate::runtime::advance::return_expired_blocker_card),
    /// which uses the plain port precisely so it does *not* re-dispatch. An
    /// [`Amend`](crate::ports::blockers::BlockerVerdict::Amend) carries the
    /// operator's answer onto the card note first, so the re-run reads the
    /// correction. A card an operator has since dragged out of `paused` is left
    /// alone — the same guard the expiry mover keeps.
    ///
    /// The re-dispatch is also pointed at the thread the blocker was answered in
    /// (issue #2008): the dispatch relay
    /// ([`journal_dispatch_replies`](Self::journal_dispatch_replies)) keys the
    /// resumed run's **output** on the card's origin, and a board- or
    /// planning-pass card carries none (the answer would relay nowhere) while a
    /// card raised elsewhere carries a different thread than the blocker
    /// conversation. Stamping the blocker's own thread here lands the output back
    /// where the operator answered — the delivery half `post_blocker_resume_note`
    /// only acknowledged before.
    #[cfg(feature = "openhuman")]
    async fn resume_task_card(
        self: &Arc<Self>,
        task_id: &str,
        resolution: &crate::ports::blockers::BlockerResolution,
        thread: Option<&str>,
        origin_parent: Option<EventSeq>,
    ) -> Result<()> {
        use crate::ports::blockers::BlockerVerdict;

        // The whole list-check-upsert below is one read-modify-write on the
        // board, so it holds `task_writes` for its duration — otherwise the
        // "already moved on" check reads a column another edit overwrites before
        // the upsert lands, and the resume yanks back a card an operator had
        // just dragged somewhere else.
        //
        // Lock order: `task_writes` is never taken while `blocker_resolutions`
        // is held. A resume runs on the follow-up task, which is spawned and so
        // outside the group lock the resolve loop holds.
        let _serialized = self.task_writes.lock().await;
        let Some(mut card) = self
            .ops
            .tasks
            .list(&self.id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id)
        else {
            drop(_serialized);
            return self
                .post_blocker_resume_note(
                    thread,
                    origin_parent,
                    "That card is no longer on the board, so there's nothing to pick back up.",
                )
                .await;
        };
        if card.column != crate::ports::tasks::COLUMN_PAUSED {
            // Someone has already moved it on; a resume must not yank it back.
            return Ok(());
        }
        if resolution.verdict == BlockerVerdict::Amend && !resolution.answer.trim().is_empty() {
            card.note = Some(crate::runtime::advance::append_result(
                card.note.as_deref(),
                "operator",
                &resolution.answer,
            ));
        }
        if let Some(thread) = thread {
            card.origin =
                crate::ports::tasks::TaskOrigin::new(Some(thread.to_string()), origin_parent);
        }
        card.column = IN_PROGRESS.to_string();
        card.updated_at_millis = now_millis();
        self.upsert_task(&card).await?;
        drop(_serialized);
        self.post_blocker_resume_note(thread, origin_parent, &blocker_resume_note(resolution))
            .await
    }

    #[cfg(feature = "openhuman")]
    async fn skip_task_card(
        self: &Arc<Self>,
        task_id: &str,
        thread: Option<&str>,
        origin_parent: Option<EventSeq>,
    ) -> Result<()> {
        let _serialized = self.task_writes.lock().await;
        let Some(mut card) = self
            .ops
            .tasks
            .list(&self.id)
            .await?
            .into_iter()
            .find(|task| task.id == task_id)
        else {
            return Ok(());
        };
        if card.column != crate::ports::tasks::COLUMN_PAUSED {
            return Ok(());
        }
        card.note = Some(crate::runtime::advance::append_result(
            card.note.as_deref(),
            "operator",
            BLOCKER_WAIVED,
        ));
        if let Some(thread) = thread {
            card.origin =
                crate::ports::tasks::TaskOrigin::new(Some(thread.to_string()), origin_parent);
        }
        card.column = crate::ports::tasks::COLUMN_IN_REVIEW.to_string();
        card.output = None;
        card.bounced = None;
        card.updated_at_millis = now_millis();
        self.ops.tasks.upsert(&self.id, &card).await?;
        drop(_serialized);
        self.post_blocker_resume_note(
            thread,
            origin_parent,
            "Okay — I've waived that blocker. The card is in review; nothing ran again.",
        )
        .await
    }

    /// Settles a blocked card the operator cancelled, moving it back to To-do
    /// carrying the reason and starting nothing (issue #1863).
    ///
    /// The plain [`TaskStore::upsert`] port, never
    /// [`upsert_task`](Self::upsert_task): a cancel must not fire a dispatch. The
    /// bounce chip marks it as not-fresh for a board scan, exactly as the expiry
    /// mover marks a card nobody answered.
    #[cfg(feature = "openhuman")]
    async fn cancel_task_card(
        self: &Arc<Self>,
        task_id: &str,
        thread: Option<&str>,
        origin_parent: Option<EventSeq>,
    ) -> Result<()> {
        // Held for the same read-modify-write reason, and in the same order, as
        // [`resume_task_card`](Self::resume_task_card).
        let _serialized = self.task_writes.lock().await;
        let Some(mut card) = self
            .ops
            .tasks
            .list(&self.id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id)
        else {
            return Ok(());
        };
        if card.column != crate::ports::tasks::COLUMN_PAUSED {
            return Ok(());
        }
        card.note = Some(crate::runtime::advance::append_result(
            card.note.as_deref(),
            "operator",
            BLOCKER_CANCELLED,
        ));
        card.column = TODO.to_string();
        card.bounced = Some(BLOCKER_CANCELLED.to_string());
        card.updated_at_millis = now_millis();
        self.ops.tasks.upsert(&self.id, &card).await?;
        drop(_serialized);
        self.post_blocker_resume_note(
            thread,
            origin_parent,
            "Okay — I've cancelled that. It's back in To-do if you want to pick it up later.",
        )
        .await
    }

    /// Re-enters the workflow node a resolved blocker stopped, carrying the
    /// operator's answer into the run (issues #1863, #2005).
    ///
    /// A [`Cancel`](crate::ports::blockers::BlockerVerdict::Cancel) settles the
    /// run and starts nothing — the short-circuit that runs before any cycle.
    ///
    /// A resuming verdict re-dispatches the run. A paused workflow run is
    /// *settled*, not suspended (see
    /// [`workflow_resume`](crate::runtime::workflow_resume)'s module docs), so
    /// re-entry means a fresh supervised run started from the blocked run's own
    /// trigger input with the answer threaded onto it under
    /// [`CONTINUATION_BLOCKER_KEY`](crate::runtime::workflow_resume::CONTINUATION_BLOCKER_KEY).
    /// The executing node reads it back and acts on the verdict: a retry runs
    /// again as it was, an amend runs again carrying the operator's words, a
    /// skip does not run at all and the branch proceeds past it.
    ///
    /// The facts that re-dispatch needs — workflow id, trigger input,
    /// attribution, checkpoint lineage — are the blocked-node stash the park
    /// armed (`stash_node_blocker_resume`), keyed per (run, node). Reusing that
    /// key rather than a second registry is what makes the at-most-once
    /// guarantee hold across both ways into this node: the gated-call resume and
    /// this one share
    /// [`is_blocked_node_dispatched`](crate::runtime::journal::RuntimeJournal::is_blocked_node_dispatched),
    /// so whichever arrives second records the decision and launches nothing.
    ///
    /// **Every failure is loud.** A stash this host no longer holds, a graph
    /// that has since been deleted, a build with no workflow execution wired —
    /// each returns `Err`, which
    /// [`resume_blocker`](Self::resume_blocker) propagates. Reporting a resume
    /// that did not happen as success is the silent drop the blocker family
    /// exists to close.
    ///
    /// Re-running from the trigger costs the upstream nodes again unless
    /// #1864's node-level checkpoint restart is available for this lineage;
    /// `spawn_blocked_node_continuation` picks the cheaper of the two. What it
    /// must never cost is a second report: the delivery (#438), outward-call
    /// (#846) and denial (#978) ledgers ride the same trigger input this
    /// threads onto, unchanged, and `deliver_outputs` skips a node either
    /// ledger already names.
    #[cfg(feature = "openhuman")]
    async fn resume_node_blocker(
        self: &Arc<Self>,
        run_id: &str,
        node_id: &str,
        resolution: &crate::ports::blockers::BlockerResolution,
        thread: Option<&str>,
        origin_parent: Option<EventSeq>,
    ) -> Result<()> {
        let turn = crate::runtime::workflow_resume::workflow_node_turn_key(run_id, node_id);
        if !resolution.resumes() {
            let outcome =
                crate::ports::runs::RunOutcome::new(crate::ports::runs::RunStatus::Cancelled)
                    .with_error(BLOCKER_CANCELLED);
            if let Err(err) = self.ops.runs.finish_run(&self.id, run_id, outcome).await {
                tracing::warn!(
                    company = %self.id,
                    run = %run_id,
                    error = %err,
                    "[blockers] a cancelled workflow-node blocker's run could not be settled"
                );
            }
            let stashed = self.blocked_nodes.peek(&turn);
            self.prune_checkpoint_lineage_of_stash(stashed.as_ref())
                .await;
            self.retire_blocked_stash(&turn).await;
            return self
                .post_blocker_resume_note(
                    thread,
                    origin_parent,
                    "Okay — I've cancelled that workflow step.",
                )
                .await;
        }
        // A continuation already launched for this node — by the gated-call
        // resume, or by a ghost decision replaying this one — must not launch a
        // second. Same guard, same reason as `resume_blocked_agent_node`: the
        // dispatch marker is host-durable and the run behind it is not
        // idempotent. Checked before the stash below is required: a node
        // parking more than one blocker card shares this turn's stash, and
        // resolving the first already retired it on dispatch — a second card's
        // answer must find the dispatch marker and be acknowledged, not read
        // the missing stash as "this host no longer holds the run".
        if self.journal.is_blocked_node_dispatched(&turn) {
            tracing::warn!(
                company = %self.id,
                %turn,
                "[blockers] a blocker was answered on a node whose continuation was already \
                 dispatched; recording the answer and retiring the stash without launching a \
                 second continuation"
            );
            self.retire_blocked_stash(&turn).await;
            return self
                .post_blocker_resume_note(thread, origin_parent, &blocker_resume_note(resolution))
                .await;
        }
        // Read without taking: the spawn below can still fail, and retiring the
        // stash first would leave a restart with no pending decision and no run
        // to continue — the stranding shape `resume_blocked_agent_node`'s own
        // Stage 4 comment describes.
        let Some(stashed) = self.blocked_nodes.peek(&turn) else {
            self.post_blocker_resume_note(
                thread,
                origin_parent,
                "I have your answer, but this host no longer holds that workflow run — re-run \
                 the workflow to pick it back up.",
            )
            .await?;
            self.retire_blocked_stash(&turn).await;
            return Err(OpenCompanyError::InvalidRequest(format!(
                "a blocker on workflow node `{node_id}` of run `{run_id}` was answered, but \
                 this host no longer holds the run's stash, so the node cannot be re-entered"
            )));
        };
        let input = crate::runtime::workflow_resume::blocker_continuation_input(
            stashed.input,
            node_id,
            resolution,
        )?;
        if let Err(error) = crate::runtime::workflow_resume::spawn_blocked_node_continuation(
            self,
            &turn,
            &stashed.workflow_id,
            input,
            stashed.started_by,
            stashed.thread_id,
            stashed.workflow_fingerprint,
        )
        .await
        {
            // The stash is deliberately left in place — nothing was admitted and
            // nothing was marked dispatched, so it stays recoverable, exactly as
            // `resume_blocked_agent_node`'s own failure arm keeps it. Said out
            // loud in the blocker's own DM as well as returned, because the
            // operator answered a question and is owed the news that the answer
            // did not land.
            self.post_blocker_resume_note(
                thread,
                origin_parent,
                &format!(
                    "I have your answer, but that workflow step could not be restarted right \
                     now: {error}"
                ),
            )
            .await?;
            return Err(error);
        }
        self.retire_blocked_stash(&turn).await;
        self.post_blocker_resume_note(thread, origin_parent, &blocker_resume_note(resolution))
            .await
    }

    /// Posts a resume acknowledgement into the DM the blocker was asked in
    /// (issue #1863), attributed to the teammate whose DM it is — the same
    /// durable [`AgentReply`](CompanyEvent::AgentReply) shape
    /// [`post_blocker_prompt`](Self::post_blocker_prompt) writes, so it threads
    /// and reloads like any transcript line. A no-op when the blocker was raised
    /// in no conversation.
    #[cfg(feature = "openhuman")]
    async fn post_blocker_resume_note(
        &self,
        thread: Option<&str>,
        parent: Option<EventSeq>,
        text: &str,
    ) -> Result<()> {
        let Some(thread) = thread else {
            return Ok(());
        };
        self.post_blocker_prompt(thread, parent, text).await
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
            // Issue #1991 review (`3903797619`): a wholly refused block is
            // terminal — nothing here or in `resume_run`'s twin arm ever comes
            // back for this lineage — so its checkpoint thread is prunable
            // exactly like the runner's own settle arms.
            self.prune_checkpoint_lineage_of_stash(stashed.as_ref())
                .await;
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
            stashed.thread_id,
            stashed.workflow_fingerprint,
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
    /// Prunes the checkpoint lineage a blocked-node stash names, when it has
    /// one and this build has a checkpoint store wired.
    ///
    /// The blocked-node counterpart to `workflow_resume::prune_checkpoint_lineage_for_effect`
    /// — the same idea, just reading the thread id off a
    /// [`StashedBlock`](crate::runtime::blocked_nodes::StashedBlock) instead
    /// of an [`Effect`](crate::ports::types::Effect)'s payload, because a
    /// blocked node's continuation facts live there rather than on a parked
    /// card.
    #[cfg(feature = "openhuman")]
    async fn prune_checkpoint_lineage_of_stash(
        &self,
        stashed: Option<&crate::runtime::blocked_nodes::StashedBlock>,
    ) {
        let Some(store) = self.workflow_checkpoints() else {
            return;
        };
        let Some(thread_id) = stashed.and_then(|s| s.thread_id.as_deref()) else {
            return;
        };
        if let Err(error) = store.prune_settled(thread_id).await {
            tracing::warn!(%thread_id, %error, "workflow: failed to prune settled checkpoints");
        }
    }

    #[cfg(not(feature = "openhuman"))]
    async fn prune_checkpoint_lineage_of_stash(
        &self,
        _stashed: Option<&crate::runtime::blocked_nodes::StashedBlock>,
    ) {
    }

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
        // A company that boots stopped resumes nothing. `RuntimeBuilder` seeds
        // the switch from the event log before it calls this, so the replayed
        // state is already in place; the stashes stay armed and this runs again
        // on the boot after the release.
        if self.ensure_not_emergency_stopped().is_err() {
            tracing::info!(
                company = %self.id,
                "[approval] the emergency stop is engaged; leaving stranded blocked nodes \
                 armed instead of resuming them at boot"
            );
            return;
        }
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
            let stashed = self.blocked_nodes.peek(&turn);
            if !stashed.as_ref().is_some_and(|stashed| stashed.approved) {
                tracing::info!(
                    company = %self.id,
                    %turn,
                    "[approval] a restart stranded a blocked node whose last decision resolved \
                     with nothing approved, before its retirement could run; retiring the stash \
                     now instead of leaving it to rehydrate on every future boot"
                );
                self.prune_checkpoint_lineage_of_stash(stashed.as_ref())
                    .await;
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
            // Codex review finding on PR #2140 (`3952230580`): this whole
            // function is one `await`-laden loop, so the single guard at the
            // top only proves the stop was clear when the loop *started* —
            // another owner can re-engage it while an earlier stash's own
            // awaits (the journal reads above, `resume_blocked_agent_node`'s
            // own writes) are still in flight. Rechecked immediately before
            // the one call in this loop that actually starts real work, the
            // same placement as `run_bracketed`'s post-lock recheck.
            if self.ensure_not_emergency_stopped().is_err() {
                tracing::info!(
                    company = %self.id,
                    %turn,
                    "[approval] the emergency stop re-engaged mid-reconciliation; leaving this \
                     stash armed instead of resuming it"
                );
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
        // Issue #1890 D/B: the conversation each relayed card was raised in.
        //
        // A dispatched card's relay used to land unparented, so a delegated
        // request produced an answer inside its thread and then one or two
        // loose bubbles about the same work beside it in the channel. That was
        // tolerable while only hand-opened threads existed and most channels
        // were flat; once every exchange is a thread it is simply the work
        // reporting back to the wrong place.
        //
        // The card already knows, since #1890 B records the thread it was
        // raised in — so this reads what the card recorded rather than deriving
        // it a second way, the same discipline the settle marker follows.
        //
        // One `list`, and only when something here actually names a card:
        // relays are the minority of responses and most cycles journal none.
        let origins: std::collections::HashMap<String, Option<EventSeq>> =
            if report.responses.iter().any(|r| r.task_id.is_some()) {
                self.tasks()
                    .list(&self.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|card| {
                        // The origin borrows the card, so read it before `id`
                        // moves out of it.
                        let origin_parent = card.origin_parent();
                        (card.id, origin_parent)
                    })
                    .collect()
            } else {
                std::collections::HashMap::new()
            };

        for response in &report.responses {
            let Some(chat_id) = response
                .reply_to
                .as_ref()
                .map(|reply_to| reply_to.chat_id.as_str())
            else {
                continue;
            };
            // `None` when the card names no thread, when it has been deleted,
            // or when this relay names no card at all — each of which is the
            // channel-level conversation, which is where these landed before.
            let parent = response
                .task_id
                .as_deref()
                .and_then(|id| origins.get(id).copied())
                .flatten();
            // Guarded the way `publish_continuation` and
            // `announce_continuation_failure` guard theirs, and for the reason
            // `resolvable_parent` documents: the console folds a transcript by
            // parent and *drops* a reply whose parent it cannot resolve in this
            // channel rather than rendering it flat. A dispatched card can
            // settle long after it was raised, so its recorded root may have
            // been pruned by now — and an unresolvable root would make the
            // delegate's answer vanish with nothing on screen to say so
            // (coderabbit on #1982). Falling back to the channel is the same
            // landing an unthreaded hand-off has always had.
            let parent = self.resolvable_parent(parent, chat_id).await;
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
                        audience: Vec::new(),
                        parent,
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
                        outputs: response.outputs.clone(),
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

    /// The board card a thread's review action targets: the `in_review`
    /// dispatch-origin card whose origin conversation is `desk`, anchored by
    /// the `parent` message the operator replied to — either that card's settle
    /// pill ([`CompanyEvent::DeskTaskCompleted`]) or the relay bubble that
    /// followed it.
    ///
    /// `Ok(None)` when `parent` names neither, when no such card is on the
    /// board, or when the card has already left `in_review` — each of which
    /// routes the message back to an ordinary chat turn rather than a review
    /// pass. `Err` when the task store failed to answer, so a storage hiccup
    /// surfaces as a server error rather than silently falling through to an
    /// ordinary chat turn with the operator's review note as its text.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn review_feedback_target(
        self: &Arc<Self>,
        desk: &str,
        parent: EventSeq,
    ) -> Result<Option<TaskRecord>> {
        let Some(task_id) = self.review_anchor_card(desk, parent).await? else {
            return Ok(None);
        };
        self.review_card_in_review(&task_id, desk).await
    }

    /// The card id a review `parent` anchors to. A settle pill names it
    /// directly; the relay bubble carries `task_id: None` by construction, so
    /// it is anchored to its settle pill only once [`is_relay_bubble_for`]
    /// confirms `parent` is that pill's own relay — proximity to *some*
    /// earlier pill is not enough, since an ordinary chat turn in the same
    /// desk carries the same `task_id: None` shape.
    ///
    /// Either way the anchoring pill must also be [`is_latest_settle_pill`]
    /// for that card: a card that settled, was revised, and is `in_review`
    /// again mints a fresh pill while the old one stays in history under the
    /// same `task_id`, so a stale client, a replayed request, or a direct API
    /// call replying to the earlier pill (or its relay) must not re-dispatch
    /// the card's latest attempt. The console applies the identical gate
    /// client-side (`isLatestSettlePill`,
    /// `frontend/src/views/chat/model.ts`); this is its server-side twin, per
    /// card rather than per thread.
    ///
    /// `Err` when the event log itself failed to answer — kept distinct from
    /// `Ok(None)` so a transient read failure surfaces to the caller instead
    /// of being read as "not a review anchor" and running the operator's note
    /// as an ordinary chat turn.
    #[cfg(feature = "openhuman")]
    async fn review_anchor_card(&self, desk: &str, parent: EventSeq) -> Result<Option<String>> {
        let stored = self.events.read_from(&self.id, parent, 1).await?;
        let Some(stored) = stored.into_iter().next() else {
            return Ok(None);
        };
        if stored.seq != parent {
            return Ok(None);
        }
        match stored.event {
            CompanyEvent::DeskTaskCompleted { task_id, .. } => {
                let is_latest = self.is_latest_settle_pill(desk, &task_id, parent).await?;
                Ok(is_latest.then_some(task_id))
            }
            CompanyEvent::AgentReply {
                task_id: None,
                chat_id,
                ..
            } if crate::server::chat_history::same_conversation(Some(&chat_id), Some(desk)) => {
                let Some((pill_seq, task_id)) = self.settle_pill_before(desk, parent).await? else {
                    return Ok(None);
                };
                let is_relay = self.is_relay_bubble_for(desk, pill_seq, parent).await?;
                if !is_relay {
                    return Ok(None);
                }
                let is_latest = self.is_latest_settle_pill(desk, &task_id, pill_seq).await?;
                Ok(is_latest.then_some(task_id))
            }
            _ => Ok(None),
        }
    }

    /// The seq and card id of the most recent settle pill before `before`
    /// whose origin conversation is `desk` — a *candidate* anchor for a relay
    /// bubble the operator replied to, still to be confirmed by
    /// [`is_relay_bubble_for`].
    ///
    /// The event log is company-wide, so the pill can sit arbitrarily far
    /// behind `before` once other desks are busy — a single fixed-size read
    /// used to cut this off at the first page and silently miss it. Pages
    /// backward instead, the same shape [`history_for_desk`](crate::server::chat_history::history_for_desk)
    /// uses to find a desk's messages amid a company-wide log, bounded by
    /// [`RELAY_SCAN_MAX_PAGES`] rather than one page — generous enough that a
    /// real reply-to-relay never hits it, but not the unbounded walk to
    /// genesis a desk that never dispatched anything would otherwise force.
    ///
    /// `Err` when the event log failed to answer a page read, threaded
    /// through rather than collapsed to "no pill found" for the same reason
    /// as [`review_anchor_card`].
    #[cfg(feature = "openhuman")]
    async fn settle_pill_before(
        &self,
        desk: &str,
        before: EventSeq,
    ) -> Result<Option<(EventSeq, String)>> {
        let mut cursor = Some(before);
        for _ in 0..RELAY_SCAN_MAX_PAGES {
            let page = self
                .events
                .read_before(&self.id, cursor, RELAY_SCAN_PAGE)
                .await?;
            if page.is_empty() {
                return Ok(None);
            }
            cursor = page.last().map(|stored| stored.seq);
            let found = page.into_iter().find_map(|stored| {
                let seq = stored.seq;
                match stored.event {
                    CompanyEvent::DeskTaskCompleted {
                        task_id,
                        origin_chat_id,
                        ..
                    } if origin_chat_id.as_deref().is_some_and(|origin| {
                        crate::server::chat_history::same_conversation(Some(origin), Some(desk))
                    }) =>
                    {
                        Some((seq, task_id))
                    }
                    _ => None,
                }
            });
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// Whether `parent` is the relay bubble `pill` actually produced: the
    /// first non-advisory `AgentReply` (`task_id: None`) posted to `desk`
    /// after `pill`, with no other settle pill for `desk` interleaved.
    ///
    /// Without this check, any later ordinary chat turn in the same desk —
    /// also an `AgentReply` with `task_id: None` — would satisfy the "reply
    /// targets the relay bubble" test just by being the nearest one before
    /// whatever the operator replied to, silently turning a normal reply into
    /// review feedback on a card the operator never looked at.
    ///
    /// **Skips the runtime's own advisories** — the B-101 mention-ambiguity
    /// note ([`Self::post_mention_ambiguity_note`]) also journals as an
    /// `AgentReply` with `task_id: None` in the same desk. A dispatch can
    /// append its `DeskTaskCompleted` and not yet reach
    /// `journal_dispatch_replies`, leaving a window in which another accepted
    /// chat's ambiguous `@name` interleaves that advisory between the pill
    /// and the genuine relay. Before this guard the advisory — not the relay —
    /// was "the first `AgentReply` after `pill`", so the scan returned
    /// `Ok(false)` for the real relay's own reply and a review pass silently
    /// ran as an ordinary chat turn instead (codex P2, PR #2052 fresh review
    /// round). The advisory can never itself be the relay bubble a review
    /// reply anchors to — nothing dispatches review feedback on a runtime
    /// notice — so skipping it costs nothing a real relay could need.
    ///
    /// **"The runtime's own" is asked of the roster, not assumed from the
    /// string.** [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR) is a
    /// [reserved agent id](crate::ports::types::RESERVED_AGENT_IDS), but the
    /// reservation is *grandfathered* on reload — `CompanyManifest`'s
    /// `from_path_for_reload` passes `enforce_reserved_agent_ids: false` — so a
    /// company declared before the reservation can still carry a roster agent
    /// literally called `system`, whose replies are ordinary teammate replies
    /// and whose relay bubble a blanket author filter would skip, silently
    /// losing that company's review pass instead of the one this guard exists
    /// to protect (codex P2, 2026-09-04). So the filter applies only when the
    /// live roster does **not** claim the id, resolved once per scan through
    /// [`Self::roster_declares_system_author`]. Read failure falls back to
    /// filtering, which is the pre-existing behaviour and the direction that
    /// cannot mistake an advisory for a relay.
    ///
    /// Pages forward past [`RELAY_SCAN_PAGE`] rather than giving up at one
    /// page, for the same company-wide-log reason as [`settle_pill_before`].
    /// Unlike that scan this one has a real same-desk boundary to stop at —
    /// the next `DeskTaskCompleted` for `desk` always ends it — so
    /// [`RELAY_SCAN_MAX_PAGES`] is a safety cap against a desk that never
    /// settles again, not the wall this search actually relies on.
    ///
    /// `Err` when the event log failed to answer a page read, threaded
    /// through rather than collapsed to "not the relay" for the same reason
    /// as [`review_anchor_card`].
    #[cfg(feature = "openhuman")]
    async fn is_relay_bubble_for(
        &self,
        desk: &str,
        pill: EventSeq,
        parent: EventSeq,
    ) -> Result<bool> {
        // Resolved once, outside the page loop: it is a property of the
        // company, not of any event, and the scan can read many pages.
        let system_is_a_teammate = self.roster_declares_system_author().await;
        let mut cursor = pill;
        for page_index in 0..RELAY_SCAN_MAX_PAGES {
            let forward = self
                .events
                .read_from(&self.id, cursor, RELAY_SCAN_PAGE)
                .await?;
            if forward.is_empty() {
                return Ok(false);
            }
            let skip = if page_index == 0 { 1 } else { 0 };
            for stored in forward.iter().skip(skip) {
                let seq = stored.seq;
                match &stored.event {
                    CompanyEvent::AgentReply {
                        task_id: None,
                        chat_id,
                        agent_id,
                        ..
                    } if (system_is_a_teammate || agent_id != crate::ports::SYSTEM_AUTHOR)
                        && crate::server::chat_history::same_conversation(
                            Some(chat_id.as_str()),
                            Some(desk),
                        ) =>
                    {
                        return Ok(seq == parent);
                    }
                    CompanyEvent::DeskTaskCompleted { origin_chat_id, .. }
                        if crate::server::chat_history::stamped_conversation_is(
                            origin_chat_id.as_deref(),
                            desk,
                        ) =>
                    {
                        return Ok(false);
                    }
                    _ => {}
                }
            }
            cursor = match forward.last() {
                Some(last) => EventSeq::new(last.seq.value().saturating_add(1)),
                None => return Ok(false),
            };
        }
        Ok(false)
    }

    /// Whether this company's live roster declares an agent whose id is
    /// [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR) — i.e. whether a reply
    /// attributed to that id is a *teammate* speaking here rather than the
    /// runtime reporting on itself.
    ///
    /// Normally `false`, and cheaply so: the id is reserved
    /// ([`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS)), so
    /// no company declared since that reservation can claim it. The reservation
    /// is grandfathered on reload, though (`from_path_for_reload` passes
    /// `enforce_reserved_agent_ids: false`), so an older bundle can, and a
    /// runtime guard that assumed otherwise would misread that company's
    /// ordinary replies as its own notices.
    ///
    /// A store that cannot answer reads as `false`. That is the pre-existing
    /// behaviour of every caller, and the safe direction for all of them: it
    /// keeps the runtime's advisories out of a scan that must not mistake one
    /// for an agent's reply, at the cost of a grandfathered `system` teammate
    /// losing a review anchor it could not have had before this method existed
    /// either.
    #[cfg(feature = "openhuman")]
    async fn roster_declares_system_author(&self) -> bool {
        matches!(
            self.store.load(&self.id).await,
            Ok(Some(record))
                if record
                    .resolve_roster_agent_id(crate::ports::SYSTEM_AUTHOR)
                    .is_some()
        )
    }

    /// Whether `pill` is the most recent settle pill for `task_id` in `desk`'s
    /// conversation — "most recent" in the same last-occurrence-wins sense the
    /// console's `latestSettlePillIdByTaskId`
    /// (`frontend/src/views/chat/model.ts`) uses to decide which pill's
    /// Approve control is live, computed here from the log's tail rather than
    /// an already-loaded message list.
    ///
    /// A card that finished, was revised, and returned to `in_review` mints a
    /// fresh `DeskTaskCompleted` for the same `task_id` while the old one
    /// stays in the log; this is `false` for that old pill so a reply
    /// anchored to it is rejected rather than re-dispatching the latest
    /// attempt. Scoped to `task_id` rather than "any settle for `desk`" so two
    /// different cards in the same desk each keep their own valid anchor.
    ///
    /// Pages backward from the tail, bounded by [`RELAY_SCAN_MAX_PAGES`] for
    /// the same company-wide-log reason as [`settle_pill_before`]. Unable to
    /// find any settle pill for `task_id` within that bound reads as `false`
    /// — the same fail-closed direction as every other outcome here, since
    /// `pill` came from a real event and its absence from the scan means the
    /// scan, not the anchor, is what gave out.
    #[cfg(feature = "openhuman")]
    async fn is_latest_settle_pill(
        &self,
        desk: &str,
        task_id: &str,
        pill: EventSeq,
    ) -> Result<bool> {
        let mut cursor = None;
        for _ in 0..RELAY_SCAN_MAX_PAGES {
            let page = self
                .events
                .read_before(&self.id, cursor, RELAY_SCAN_PAGE)
                .await?;
            if page.is_empty() {
                return Ok(false);
            }
            cursor = page.last().map(|stored| stored.seq);
            let found = page.iter().find_map(|stored| match &stored.event {
                CompanyEvent::DeskTaskCompleted {
                    task_id: found_id,
                    origin_chat_id,
                    ..
                } if found_id == task_id
                    && origin_chat_id.as_deref().is_some_and(|origin| {
                        crate::server::chat_history::same_conversation(Some(origin), Some(desk))
                    }) =>
                {
                    Some(stored.seq)
                }
                _ => None,
            });
            if let Some(found_seq) = found {
                return Ok(found_seq == pill);
            }
        }
        Ok(false)
    }

    /// The card with id `task_id`, but only when it is an `in_review`
    /// dispatch-origin card whose origin conversation is `desk`.
    ///
    /// `Err` when the task store itself failed to answer — kept distinct from
    /// `Ok(None)` (no such card, or one that is not a review target) so a
    /// transient storage error surfaces to the caller rather than being read
    /// as "not a review".
    #[cfg(feature = "openhuman")]
    pub(crate) async fn review_card_in_review(
        &self,
        task_id: &str,
        desk: &str,
    ) -> Result<Option<TaskRecord>> {
        let card = self
            .ops
            .tasks
            .list(&self.id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id);
        Ok(card.filter(|c| is_review_target(c, desk)))
    }

    /// Appends the operator's review feedback to `card`'s note as a
    /// `[reviewer]` block and re-dispatches it: the card moves
    /// `in_review → in_progress` through [`upsert_task`](Self::upsert_task),
    /// whose dispatch edge fires a fresh run that reads the appended note back
    /// through the card's task instruction.
    ///
    /// A blank `feedback` leaves the card untouched rather than re-dispatching
    /// it: nothing was appended for the fresh run to read, so a re-run would
    /// repeat the same attempt against the same instruction with nothing new
    /// to act on.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn apply_review_feedback(
        self: &Arc<Self>,
        card: &TaskRecord,
        feedback: &str,
        author: Option<&Actor>,
    ) -> Result<TaskRecord> {
        if feedback.trim().is_empty() {
            return Ok(card.clone());
        }
        tracing::debug!(
            company = %self.id,
            task = %card.id,
            reviewer = ?author.map(|a| a.id.as_str()),
            "[review] applying feedback; re-dispatching the card"
        );
        let mut next = card.clone();
        next.note = Some(crate::runtime::delegation::append_note(
            next.note.as_deref(),
            crate::harness::built_in::lifecycle::REVIEWER_ATTRIBUTION,
            feedback,
        ));
        next.column = IN_PROGRESS.to_string();
        next.updated_at_millis = now_millis();
        self.upsert_task(&next).await
    }

    /// Settles a reviewed `in_review` card per the operator's verdict:
    /// `Approve` finishes it to `done` with the verdict recorded in its note,
    /// `Revise` routes through [`apply_review_feedback`](Self::apply_review_feedback)
    /// for a fresh pass.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn apply_review_decision(
        self: &Arc<Self>,
        card: &TaskRecord,
        decision: crate::harness::built_in::lifecycle::ReviewDecision,
        note: Option<&str>,
        author: Option<&Actor>,
    ) -> Result<TaskRecord> {
        use crate::harness::built_in::lifecycle;
        match decision {
            lifecycle::ReviewDecision::Revise => {
                self.apply_review_feedback(card, note.unwrap_or_default(), author)
                    .await
            }
            lifecycle::ReviewDecision::Approve => {
                let mut next = card.clone();
                next.note = Some(crate::runtime::delegation::append_note(
                    next.note.as_deref(),
                    lifecycle::REVIEWER_ATTRIBUTION,
                    &lifecycle::review_note(decision, note),
                ));
                next.column = lifecycle::review_landing_column(decision).to_string();
                next.updated_at_millis = now_millis();
                self.upsert_task(&next).await
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
                        audience: Vec::new(),
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
                        outputs: response.outputs.clone(),
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
                outputs: Vec::new(),
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
            // Issue B-012: and which workflow run was waiting on it, for the
            // same before-the-retirement reason as the two above.
            let waiting_runs = self.parked_workflow_attempts(id).await;
            self.retire_approval(id, ExpiryReason::Ttl, now).await?;
            self.finish_expiry(id, is_blocker, unanswered, waiting_runs)
                .await;
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
        waiting_runs: Vec<crate::ports::runs::RunRecord>,
    ) {
        // **The run that was waiting stops claiming it still is** (issue B-012).
        //
        // A workflow that parks on a gate is recorded `WaitingApproval` — the
        // run is settled, nothing is executing, and that status is the row's
        // account of why it stopped. Expiry retired the approval and removed it
        // from the pending set, but nothing ever revisited the row, so it went
        // on naming an approval no sweep will see again: the Observatory showed
        // a run waiting for a decision that had already defaulted, and the
        // approvals list showed nothing to decide. Two screens, no way to tell
        // which was lying.
        //
        // `Cancelled` — "the attempt was cancelled before it could settle" — is
        // what a default-deny leaves behind. Not `Declined`, which is reserved
        // for work refused *by design* by the compiler or a step; here the
        // decision was made by the clock and nobody chose it. Not `Failed`:
        // nothing errored.
        //
        // **Whether or not the expiry released a continuation** (Codex on this
        // PR, third round). It is tempting to skip this when something was
        // released — a node whose *other* gated call the operator approved does
        // continue — but the continuation runs as a **new attempt**:
        // `RunAttempts` is an in-memory map built fresh per run
        // (`workflows/runner.rs`) and `caps` mints every attempt under
        // `generate_id()`. Nothing ever writes this row again. So skipping it
        // left exactly the stale `WaitingApproval` this issue exists to remove,
        // and — because the continuation cannot touch this id — there was never
        // a race here to avoid in the first place. This attempt stopped at a
        // gate that defaulted to denied, which is true either way.
        //
        // **Usage is carried, not reset** (Codex, same round). `finish_run`
        // assigns `run.usage` and `run.step_count` from the outcome
        // (`ports/runs.rs`), and `RunOutcome::new` zeroes both — so settling
        // from a bare outcome would silently erase the tokens and cost this
        // attempt really did spend, on a row the billing surfaces read.
        //
        // Best-effort and last, like everything else here: a row that cannot be
        // written must not undo a default-deny that already happened.
        for row in waiting_runs {
            let run_id = row.id;
            let outcome =
                crate::ports::runs::RunOutcome::new(crate::ports::runs::RunStatus::Cancelled)
                    .with_error(
                        "the approval this attempt was waiting on expired and defaulted to denied",
                    )
                    .with_usage(row.usage)
                    .with_step_count(row.step_count);
            if let Err(err) = self.runs().finish_run(&self.id, &run_id, outcome).await {
                tracing::warn!(
                    company = %self.id,
                    approval = %id,
                    run = %run_id,
                    %err,
                    "[approval] expired, but the waiting run's row could not be settled; \
                     it will keep reporting `waiting_approval`"
                );
            }
        }

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

    /// The **attempt rows** this approval left recorded `WaitingApproval`, read
    /// **before** the retirement for the reason its two siblings are (B-012).
    ///
    /// `workflow_run_of` needs the pending entry, and retiring is what removes
    /// it — so after `retire_approval` there is no way back to which run was
    /// waiting, exactly as there is no way back to what was being asked.
    ///
    /// # Why the cycle, and not `Effect::run_id` or the effect kind
    ///
    /// Two Codex findings, in sequence, both about reaching for the wrong
    /// handle. `Effect::run_id` on a `workflow.approve` gate is the **lineage**
    /// id, while `RunStore` rows are per-node *attempts* minted under
    /// `generate_id()` and merely linked to it (`NewRun::for_workflow_node`), so
    /// handing it to `finish_run` names no row at all. And the path that
    /// actually leaves an attempt reading `WaitingApproval` is not that gate: it
    /// is a **gated tool call** inside a workflow agent node
    /// (`caps::park_gated_calls` → the `!parked.is_empty()` settle), parked with
    /// `ApprovalPolicy::effect_for`'s own effect — whose `kind` is the tool name
    /// and whose `run_id` is `None`. So on the real path neither the kind test
    /// nor `workflow_run_of` can say anything, and a fix keyed on either is a
    /// no-op that a fixture combining a gate effect with a hand-made attempt row
    /// will nonetheless report as working.
    ///
    /// The **cycle recorded at park time** is the one correlation that survives
    /// both shapes, so it is what this reads:
    ///
    /// * `workflow-node:{run}:{node}` — a blocked node's gated calls. The real
    ///   case, and it names the run and the node outright.
    /// * `workflow-run:{run}` — a run-level gate, whose node is on its own
    ///   payload (`gate_node_id`).
    ///
    /// Narrowed to that node, never "every `WaitingApproval` attempt in the
    /// lineage": a graph can park two nodes on two gates and only one expired.
    /// Anything whose node cannot be resolved settles nothing — a stale row is
    /// the bug, but cancelling a sibling still waiting on a live decision would
    /// be a worse one.
    async fn parked_workflow_attempts(
        &self,
        id: &ApprovalId,
    ) -> Vec<crate::ports::runs::RunRecord> {
        use crate::runtime::workflow_resume::{gate_node_id, run_and_node_from_node_turn};

        let Some(pending) = self.journal.pending().into_iter().find(|p| &p.id == id) else {
            return Vec::new();
        };
        let cycle = self.journal.approval_cycle(id).flatten();
        let resolved = cycle
            .as_deref()
            .and_then(run_and_node_from_node_turn)
            .map(|(run, node)| (run.to_string(), node.to_string()))
            .or_else(|| {
                let run = workflow_run_of(&pending)?;
                let node = gate_node_id(&pending.effect)?;
                Some((run, node.to_string()))
            });
        let Some((lineage, node)) = resolved else {
            return Vec::new();
        };
        let filter = crate::ports::runs::RunFilter {
            workflow_run_id: Some(lineage),
            statuses: vec![crate::ports::runs::RunStatus::WaitingApproval],
            ..Default::default()
        };
        match self.runs().list_runs(&self.id, &filter).await {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| row.node_id.as_deref() == Some(node.as_str()))
                .collect(),
            Err(err) => {
                tracing::warn!(
                    company = %self.id,
                    approval = %id,
                    %err,
                    "[approval] could not resolve the attempts waiting on an expiring approval"
                );
                Vec::new()
            }
        }
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
                            outputs: Vec::new(),
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
                        outputs: Vec::new(),
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
        self.arm_replayed_blocker_recovery();
        self.schedule_replayed_blocker_resolutions();
        Ok(())
    }

    /// Arms boot recovery for a blocker answer replay left banked while its
    /// approval is still parked.
    ///
    /// That pair is the signature of a settle that never landed: the answer is
    /// durable, `record_resolved` is not, and replay therefore re-arms the
    /// claim over an approval it also still shows as pending. Left alone the
    /// two deadlock each other — `claim_blocker_resolution` refuses every later
    /// answer because the slot is full, and nothing ever empties the slot
    /// because settling is what would have.
    ///
    /// An answer whose approval *did* settle is skipped: its resume is the
    /// continuation queue's business, and driving it here would settle an
    /// approval twice.
    pub(crate) fn arm_replayed_blocker_recovery(&self) {
        if self.replayed_blockers_to_drive().is_empty() {
            return;
        }
        self.replay_blockers_on_register
            .store(true, Ordering::Release);
    }

    /// Every rehydrated blocker answer whose approval replay still shows
    /// parked, paired with the verdict to settle it under.
    #[cfg(feature = "openhuman")]
    fn replayed_blockers_to_drive(
        &self,
    ) -> Vec<(ApprovalId, crate::ports::blockers::BlockerVerdict)> {
        let pending = self.journal.pending();
        self.journal
            .replayed_blocker_resolutions()
            .into_iter()
            .filter(|(id, _)| {
                pending.iter().any(|parked| {
                    &parked.id == id && crate::ports::blockers::is_blocker_effect(&parked.effect)
                })
            })
            .map(|(id, resolution)| (id, resolution.verdict))
            .collect()
    }

    /// A build with no blocker resume has none of these to drive.
    #[cfg(not(feature = "openhuman"))]
    fn replayed_blockers_to_drive(&self) -> Vec<(ApprovalId, ())> {
        Vec::new()
    }

    /// Settles and resumes every blocker answer replay left banked but never
    /// settled, once the runtime is addressable.
    ///
    /// The blocker twin of
    /// [`schedule_replayed_continuations`](Self::schedule_replayed_continuations),
    /// and what makes `claim_and_settle_blocker`'s bank-before-settle ordering
    /// mean what its doc says: a crash between the two replays as "still armed"
    /// and is re-driven here, rather than stranding the blocker behind its own
    /// rehydrated claim.
    ///
    /// The claim is already held by the rehydration, so this enters at
    /// [`settle_claimed_blocker`](Self::settle_claimed_blocker) rather than
    /// re-claiming. The group lock is taken per id for the same reason the live
    /// paths take it — a boot and an operator can be answering at once.
    ///
    /// Settled under the operator channel, the same default a DM answer with no
    /// named actor takes: the durable record carries the verdict and the words,
    /// not who supplied them, and re-deriving an attribution the journal never
    /// stored would be a worse answer than the one the DM path already gives.
    ///
    /// Hands back the driving tasks so a caller that needs them finished can
    /// join them; the registry drops them, exactly as it drops a replayed
    /// continuation's.
    pub(crate) fn schedule_replayed_blocker_resolutions(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        if !self
            .replay_blockers_on_register
            .swap(false, Ordering::AcqRel)
        {
            return Vec::new();
        }
        self.drive_replayed_blockers()
    }

    #[cfg(feature = "openhuman")]
    fn drive_replayed_blockers(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        self.replayed_blockers_to_drive()
            .into_iter()
            .map(|(id, verdict)| {
                let rt = Arc::clone(self);
                tokio::spawn(async move {
                    let _resolving = rt.blocker_resolutions.lock().await;
                    let actor = Actor {
                        kind: ActorKind::Operator,
                        id: crate::runtime::channel::OPERATOR_CHANNEL.to_string(),
                    };
                    if let Err(err) = rt.settle_claimed_blocker(&id, verdict, &actor).await {
                        tracing::warn!(
                            company = %rt.id,
                            approval = %id,
                            error = %err,
                            "could not drive a blocker answer replay left banked; it stays \
                             durable for the next boot to retry"
                        );
                    }
                })
            })
            .collect()
    }

    #[cfg(not(feature = "openhuman"))]
    fn drive_replayed_blockers(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        Vec::new()
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
                // Issue #1862: the shared root cause, so the console folds every
                // card stalled on one broken integration into a single question.
                group_key: blocker_group_key_of(&p.effect),
                // So the console can word skip/cancel honestly per step kind
                // instead of promising a workflow node's behaviour on a card.
                blocker_step_kind: blocker_step_kind_of(&p.effect),
            })
            .collect()
    }

    /// Every pending blocker sharing both `group_key` and `step_kind` — the
    /// set one verdict fans across (issue #1862).
    ///
    /// Ten cards stalled on one broken OAuth grant are one question, so
    /// answering the card answers all of them — but a verdict does not mean
    /// the same thing to every kind of stopped step (Skip re-dispatches a
    /// board card; on a workflow node it produces nothing), so a root cause
    /// that happens to stop both a task and a node is two questions, not one.
    /// `step_kind` narrows the fan-out to `blocker_step_kind_of`'s own reading
    /// of the addressed member, so the two kinds never mix. A blocker with no
    /// `group_key` never reaches here (the caller resolves it alone), so a
    /// solo verdict can never fan by accident. Oldest-first, the order
    /// [`pending`](crate::runtime::journal::Journal::pending) already returns.
    #[cfg(feature = "openhuman")]
    pub(crate) fn blocker_group_members(
        &self,
        group_key: &str,
        step_kind: Option<&str>,
    ) -> Vec<ApprovalId> {
        self.journal
            .pending()
            .into_iter()
            .filter(|p| {
                blocker_group_key_of(&p.effect).as_deref() == Some(group_key)
                    && blocker_step_kind_of(&p.effect).as_deref() == step_kind
            })
            .map(|p| p.id)
            .collect()
    }

    /// The full group a single parked blocker belongs to — its root-cause
    /// siblings that stopped the same kind of step, or itself alone when it
    /// carries no group key.
    #[cfg(feature = "openhuman")]
    fn blocker_group_of(&self, id: &ApprovalId) -> Vec<ApprovalId> {
        let Some(parked) = self.journal.pending().into_iter().find(|p| &p.id == id) else {
            return vec![id.clone()];
        };
        match blocker_group_key_of(&parked.effect) {
            Some(key) => {
                self.blocker_group_members(&key, blocker_step_kind_of(&parked.effect).as_deref())
            }
            None => vec![id.clone()],
        }
    }

    /// The root-cause group `id` belongs to, or `None` when `id` is not a
    /// parked blocker at all.
    ///
    /// Kind-checked rather than payload-checked, the same reason
    /// [`is_blocker_effect`](crate::ports::blockers::is_blocker_effect) is: the
    /// kind is the part of a park that survives redaction.
    #[cfg(feature = "openhuman")]
    pub(crate) fn parked_blocker_group(&self, id: &ApprovalId) -> Option<Vec<ApprovalId>> {
        let parked = self.journal.pending().into_iter().find(|p| &p.id == id)?;
        if !crate::ports::blockers::is_blocker_effect(&parked.effect) {
            return None;
        }
        Some(self.blocker_group_of(id))
    }

    /// An idempotent "nothing left to resolve" answer for an id that WAS a
    /// parked blocker and no longer is (issue #2028) — settled by another
    /// tab, a plain double-click, or this very request racing a sibling's
    /// group fan-out. `None` when `id` was never a blocker at all, which the
    /// caller must still refuse: a `blocker_verdict` names an answer such an
    /// id never had a question for.
    ///
    /// [`parked_blocker_group`](Self::parked_blocker_group) cannot tell these
    /// two "not currently parked" cases apart by itself — it only kind-checks
    /// the *still-parked* effect, which is gone the instant the id resolves.
    /// This reads [`RuntimeJournal::approval_origin`] instead: an unbounded,
    /// never-pruned record of what every approval ever parked *was*, kind
    /// included, which survives resolution the live queue does not.
    #[cfg(feature = "openhuman")]
    pub(crate) fn already_resolved_blocker_receipt(
        self: &Arc<Self>,
        id: &ApprovalId,
    ) -> Option<(ResolveReceipt, JoinHandle<Result<CycleReport>>)> {
        let origin = self.journal.approval_origin(id)?;
        if !origin.kind.starts_with(&format!(
            "{}.",
            crate::ports::blockers::BLOCKER_EFFECT_PREFIX
        )) {
            return None;
        }
        let rt = Arc::clone(self);
        let handle =
            tokio::spawn(async move { Ok(CycleRunner::new(&rt).already_resolved_report()) });
        Some((ResolveReceipt::AlreadyResolved, handle))
    }

    /// The parked blockers pending in one DM, folded into root-cause groups
    /// (issue #1862), split further by step kind so a connection failure that
    /// stopped both a task and a workflow node never folds into one group —
    /// a verdict does not mean the same thing to each, so a group must never
    /// mix them. Oldest-first, the order `pending` already returns.
    ///
    /// Matched through
    /// [`stamped_conversation_is`](crate::server::chat_history::stamped_conversation_is)
    /// rather than `same_conversation`, so a blocker that names **no** thread is
    /// pending in no conversation rather than in General. `park_blocker` always
    /// stamps the sender's DM, but `cycle_conversation` hands back a thread-less
    /// `ApprovalConversation` for every park that came from no conversation at
    /// all — a planning pass, a scheduler tick, an unaddressed trigger — and
    /// through `same_conversation` each of those read as pending in `#general`,
    /// where the next top-level message was consumed as its answer.
    #[cfg(feature = "openhuman")]
    fn pending_blocker_groups(&self, desk: &str) -> Vec<PendingBlockerGroup> {
        let prefix = format!("{}.", crate::ports::blockers::BLOCKER_EFFECT_PREFIX);
        let mut groups: Vec<PendingBlockerGroup> = Vec::new();
        for p in self.journal.pending() {
            if !p.effect.kind.starts_with(&prefix) {
                continue;
            }
            if !crate::server::chat_history::stamped_conversation_is(p.thread.as_deref(), desk) {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<crate::ports::blockers::BlockerPayload>(
                p.effect.payload.clone(),
            ) else {
                continue;
            };
            let label: String = payload
                .reason
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            let step_kind = payload.step.as_ref().map(blocker_step_kind_str);
            match &payload.group_key {
                Some(key) => {
                    if let Some(group) = groups
                        .iter_mut()
                        .find(|g| g.key.as_deref() == Some(key) && g.step_kind == step_kind)
                    {
                        group.ids.push(p.id);
                    } else {
                        groups.push(PendingBlockerGroup {
                            key: Some(key.clone()),
                            step_kind,
                            ids: vec![p.id],
                            label,
                        });
                    }
                }
                None => groups.push(PendingBlockerGroup {
                    key: None,
                    step_kind,
                    ids: vec![p.id],
                    label,
                }),
            }
        }
        groups
    }

    /// The approval id of the parked blocker at `parent`, when the reply's
    /// parent is a blocker card (issue #1862) — the explicit reply tier.
    ///
    /// Reads the event at `parent` exactly as
    /// [`review_anchor_card`](Self::review_anchor_card) does, and answers `None`
    /// for anything that is not a still-pending `ApprovalParked` blocker. The
    /// two are disjoint: a review anchor is a settle pill or relay bubble, a
    /// blocker anchor is an `ApprovalParked` event, so neither steals the
    /// other's replies.
    #[cfg(feature = "openhuman")]
    async fn parked_blocker_at(&self, desk: &str, parent: EventSeq) -> Result<Option<ApprovalId>> {
        let stored = self.events.read_from(&self.id, parent, 1).await?;
        let Some(stored) = stored.into_iter().next() else {
            return Ok(None);
        };
        if stored.seq != parent {
            return Ok(None);
        }
        let prefix = format!("{}.", crate::ports::blockers::BLOCKER_EFFECT_PREFIX);
        match stored.event {
            CompanyEvent::ApprovalParked {
                approval_id,
                effect_kind,
                thread,
                ..
            } if effect_kind.starts_with(&prefix)
                // Same rule as `pending_blocker_groups`: a card stamped with no
                // thread was raised by no conversation, so no conversation's
                // reply anchors to it — General included.
                && crate::server::chat_history::stamped_conversation_is(thread.as_deref(), desk)
                && self.is_blocker(&approval_id) =>
            {
                Ok(Some(approval_id))
            }
            _ => Ok(None),
        }
    }

    /// Decides what an operator's reply in `desk` does to the blockers pending
    /// there (issue #1862) — the read-only half, so the caller journals the
    /// reply before anything settles.
    ///
    /// A reply parented to a blocker card resolves that card's whole group; an
    /// unparented reply in a DM with a single pending group resolves it; one in
    /// a DM with several groups is resolved only if the text names one,
    /// otherwise it asks which and settles nothing. A reply that is not a
    /// verdict — or a DM with no pending blocker — is
    /// [`NotBlocker`](BlockerReplyPlan::NotBlocker), and the caller runs it as an
    /// ordinary turn.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn plan_blocker_reply(
        &self,
        desk: &str,
        parent: Option<EventSeq>,
        text: &str,
    ) -> Result<BlockerReplyPlan> {
        use crate::company::task_intent::{BlockerReplyIntent, classify_blocker_reply};

        let explicit = match parent {
            Some(parent) => self
                .parked_blocker_at(desk, parent)
                .await?
                .map(|id| self.blocker_group_of(&id)),
            None => None,
        };
        let groups = self.pending_blocker_groups(desk);
        if explicit.is_none() && groups.is_empty() {
            return Ok(BlockerReplyPlan::NotBlocker);
        }
        let intent = classify_blocker_reply(text);
        if intent == BlockerReplyIntent::Unrelated {
            return Ok(BlockerReplyPlan::NotBlocker);
        }
        if let Some(ids) = explicit {
            return Ok(BlockerReplyPlan::Resolve { ids, intent });
        }
        if groups.len() == 1 {
            return Ok(BlockerReplyPlan::Resolve {
                ids: groups[0].ids.clone(),
                intent,
            });
        }
        let lower = text.to_lowercase();
        let mut named = groups.iter().filter(|group| {
            group
                .connection_hint()
                .is_some_and(|hint| lower.contains(hint))
        });
        match (named.next(), named.next()) {
            (Some(group), None) => Ok(BlockerReplyPlan::Resolve {
                ids: group.ids.clone(),
                intent,
            }),
            _ => Ok(BlockerReplyPlan::AskWhich {
                prompt: ask_which_prompt(&groups),
            }),
        }
    }

    /// Records the operator's verdict on a blocker group and **drives the
    /// resume** (issue #1863), fanning it to every member of the group.
    ///
    /// Each id is claimed on the grant set's blocker side-channel and banked as
    /// a durable
    /// [`BlockerResolution`](crate::ports::blockers::BlockerResolution) **before**
    /// the detached follow-up spawns, so a restart mid-resume replays the answer
    /// rather than dropping it — the same restart-durable ordering the
    /// blocked-node bank keeps. It is then resolved with the two-value event
    /// verdict the operator's answer lowers onto (Retry/Amend/Skip approve,
    /// Cancel denies), and
    /// [`spawn_follow_up`](Self::spawn_follow_up)'s blocker fork re-enters the
    /// stopped step carrying the resolution — a resuming verdict re-dispatches
    /// the work, a cancel settles it and starts nothing.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn apply_blocker_reply(
        self: &Arc<Self>,
        ids: &[ApprovalId],
        intent: crate::company::task_intent::BlockerReplyIntent,
        text: &str,
        by: Option<&Actor>,
    ) -> Result<()> {
        use crate::company::task_intent::BlockerReplyIntent;
        use crate::ports::blockers::BlockerVerdict;

        let verdict = match intent {
            BlockerReplyIntent::Retry => BlockerVerdict::Retry,
            BlockerReplyIntent::Amend => BlockerVerdict::Amend,
            BlockerReplyIntent::Skip => BlockerVerdict::Skip,
            BlockerReplyIntent::Cancel => BlockerVerdict::Cancel,
            // Not a verdict; the caller runs it as an ordinary turn.
            BlockerReplyIntent::Unrelated => return Ok(()),
        };
        // Only an amend carries the operator's words back into the step; the
        // other verdicts need none, so their answer stays empty.
        let answer = if verdict == BlockerVerdict::Amend {
            text
        } else {
            ""
        };
        if ids.is_empty() {
            return Ok(());
        }
        // No single "addressed" id here — a DM reply answers the whole group
        // at once, and the receipt is discarded rather than reported to a
        // particular request, so which member's receipt `head` names is moot.
        let (_, follow_up) = self
            .apply_blocker_reply_spawned(ids, &ids[0], verdict, answer, by)
            .await?;
        join_follow_up(follow_up).await?;
        Ok(())
    }

    /// Answers **one** parked blocker: claims it, banks the verdict, settles it
    /// and starts its resume — the only place a blocker reply writes a
    /// [`BlockerResolution`].
    ///
    /// The claim comes **first**, and it is the arming:
    /// [`claim_blocker_resolution`](crate::runtime::grants::GrantSet::claim_blocker_resolution)
    /// inserts only into an empty slot and says whether it was this caller that
    /// filled it. A caller that loses returns here having written nothing at
    /// all. Before, the sequence banked and armed the verdict and only then
    /// called `settle_approval` to discover it had lost, by which point it had
    /// already overwritten the winner's durable record and its armed answer —
    /// so a resume that had not yet consumed the entry executed the loser's
    /// verdict, and the journal disagreed with the approval event about what
    /// the operator had decided.
    ///
    /// Claiming is not on its own enough to write: an id that is not a blocker
    /// still parked in `pending` is released again and reported as
    /// already-resolved, so an unknown, expired or non-blocker id banks nothing
    /// — the guard [`arm_console_blocker_resolution`](Self::arm_console_blocker_resolution)
    /// keeps for the console's way in.
    ///
    /// Bank stays before settle, the journal-before-live ordering `mint_grant`
    /// keeps: the claim only reserves an in-memory slot, so a crash between the
    /// two still replays as "still armed" and re-resumes rather than losing the
    /// operator's decision. A settle that answers `AlreadyResolved` or `Expired`
    /// has no resume left to consume the entry, so both are compensated —
    /// the arming is taken back and the journal records the resolution as
    /// resumed — rather than left banked for a next boot to re-arm and replay.
    ///
    /// Hands back the receipt and, only for a settle that actually landed, the
    /// follow-up running its resume.
    #[cfg(feature = "openhuman")]
    async fn claim_and_settle_blocker(
        self: &Arc<Self>,
        id: &ApprovalId,
        parked: Option<&crate::runtime::journal::PendingApproval>,
        verdict: crate::ports::blockers::BlockerVerdict,
        answer: &str,
        actor: &Actor,
    ) -> Result<(ResolveReceipt, Option<JoinHandle<Result<CycleReport>>>)> {
        use crate::ports::blockers::{BlockerPayload, BlockerResolution};

        let step = parked
            .and_then(|parked| {
                serde_json::from_value::<BlockerPayload>(parked.effect.payload.clone()).ok()
            })
            .and_then(|payload| payload.step);
        let resolution = BlockerResolution {
            verdict,
            answer: answer.to_string(),
            step,
        };
        if !self.grants.claim_blocker_resolution(id, resolution.clone()) {
            return Ok((ResolveReceipt::AlreadyResolved, None));
        }
        let still_parked =
            parked.is_some_and(|parked| crate::ports::blockers::is_blocker_effect(&parked.effect));
        if !still_parked {
            self.grants.take_blocker_resolution(id);
            return Ok((ResolveReceipt::AlreadyResolved, None));
        }
        if let Err(err) = self
            .journal
            .record_blocker_resolution(id, &resolution)
            .await
        {
            self.grants.take_blocker_resolution(id);
            return Err(err);
        }
        self.settle_claimed_blocker(id, verdict, actor).await
    }

    /// Settles a blocker whose answer is **already claimed and banked**, and
    /// starts its resume.
    ///
    /// The tail of [`claim_and_settle_blocker`](Self::claim_and_settle_blocker),
    /// split out because a boot reaches exactly this point by a different road:
    /// [`schedule_replayed_blocker_resolutions`](Self::schedule_replayed_blocker_resolutions)
    /// rehydrates a durable answer whose settle never landed, so the claim is
    /// held and the record is banked before anything here runs. Re-claiming
    /// there would lose to the rehydrated entry and report `AlreadyResolved`,
    /// which is the whole bug that entry point exists to fix.
    ///
    /// A failure releases the live claim but deliberately leaves the durable
    /// record alone: that record is what the next boot re-arms and drives, and
    /// erasing it here would trade a retryable state for a lost decision.
    #[cfg(feature = "openhuman")]
    async fn settle_claimed_blocker(
        self: &Arc<Self>,
        id: &ApprovalId,
        verdict: crate::ports::blockers::BlockerVerdict,
        actor: &Actor,
    ) -> Result<(ResolveReceipt, Option<JoinHandle<Result<CycleReport>>>)> {
        let receipt = match CycleRunner::new(self)
            .settle_approval(id, verdict.event_verdict(), actor.clone(), GrantScope::Once)
            .await
        {
            Ok(receipt) => receipt,
            Err(err) => {
                self.grants.take_blocker_resolution(id);
                return Err(err);
            }
        };
        if let Err(err) = self.retire_if_expired(id, &receipt).await {
            self.grants.take_blocker_resolution(id);
            return Err(err);
        }
        if !matches!(receipt, ResolveReceipt::Settled(_)) {
            self.grants.take_blocker_resolution(id);
            if let Err(err) = self.journal.record_blocker_resumed(id).await {
                tracing::warn!(
                    company = %self.id,
                    approval = %id,
                    error = %err,
                    "could not retire a blocker answer whose settle found nothing to resume"
                );
            }
            return Ok((receipt, None));
        }
        let follow_up = self.spawn_follow_up(receipt.clone());
        Ok((receipt, Some(follow_up)))
    }

    /// [`apply_blocker_reply`](Self::apply_blocker_reply), handing back the
    /// addressed member's own receipt and a handle to the group's follow-ups
    /// rather than awaiting them — the blocker twin of
    /// [`resolve_approval_spawned`](Self::resolve_approval_spawned), and what
    /// lets a caller answer as soon as the verdicts are durable.
    ///
    /// Takes the four-way verdict and the operator's answer directly, so a
    /// surface that already knows which of the four was asked for does not have
    /// to round-trip through the free-text intent classifier.
    ///
    /// `addressed` names which member of `ids` the caller actually asked
    /// about — the id the operator clicked, not necessarily `ids[0]`.
    /// [`blocker_group_members`](Self::blocker_group_members) orders a group
    /// oldest-first, which need not be the id the request named: an older
    /// sibling can expire mid-loop while the clicked blocker settles the
    /// requested verdict just fine, and reporting the oldest member's outcome
    /// in that case would tell the operator their own decision failed when it
    /// did not. The returned receipt is always `addressed`'s own. Must be a
    /// member of `ids`, or the head receipt falls back to `ids[0]`'s — every
    /// caller passes an id it already sourced `ids` from, so this is a
    /// defensive fallback rather than a case any caller should hit.
    ///
    /// Every id is claimed, banked and settled **inline**, in order, before
    /// this returns; the returned handle runs each member's follow-up in that
    /// same order. A follow-up failing does not stop the rest from running —
    /// every member still gets its resume attempt, and the handle surfaces the
    /// first error once all of them have run.
    ///
    /// Each id goes through
    /// [`claim_and_settle_blocker`](Self::claim_and_settle_blocker), which owns
    /// the claim: a member another request already answered is skipped with an
    /// `AlreadyResolved` receipt and nothing written, so correctness here does
    /// not depend on the loop running alone.
    /// [`blocker_resolutions`](Self::blocker_resolutions) is still held across
    /// the loop, but only so a group settles as a unit rather than interleaving
    /// two operators' verdicts across its members.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn apply_blocker_reply_spawned(
        self: &Arc<Self>,
        ids: &[ApprovalId],
        addressed: &ApprovalId,
        verdict: crate::ports::blockers::BlockerVerdict,
        answer: &str,
        by: Option<&Actor>,
    ) -> Result<(ResolveReceipt, JoinHandle<Result<CycleReport>>)> {
        self.ensure_accepting()?;
        if ids.is_empty() {
            return Err(OpenCompanyError::InvalidRequest(
                "a blocker reply needs at least one approval to answer".to_string(),
            ));
        }
        let _resolving = self.blocker_resolutions.lock().await;
        // The still-parked entry each blocker re-enters from, snapshotted before
        // any resolve pops it: the journal scrubs a parked effect's payload the
        // moment the approval leaves the pending set, so the stopped step must be
        // read now and banked on the resolution, and a group's later members must
        // still be findable after the first is popped.
        let pending = self.journal.pending();
        let parked_of = |id: &ApprovalId| pending.iter().find(|parked| &parked.id == id);
        let actor = by.cloned().unwrap_or_else(|| Actor {
            kind: ActorKind::Operator,
            id: crate::runtime::channel::OPERATOR_CHANNEL.to_string(),
        });
        let mut head: Option<ResolveReceipt> = None;
        // Each member's follow-up is spawned the moment ITS settle lands, not
        // batched until every member in the group has settled. A later
        // member's `record_blocker_resolution`, `settle_approval` or
        // `retire_if_expired` can still fail and return via `?` below; when it
        // does, every earlier member is already durably settled AND its
        // follow-up is already running on its own task rather than sitting in
        // a `Vec` this early return would drop — the receipt would otherwise
        // be settled with nothing left to consume it, and retrying that id
        // would only find `AlreadyResolved` and never resume it.
        let mut handles: Vec<JoinHandle<Result<CycleReport>>> = Vec::with_capacity(ids.len());
        for id in ids {
            let (receipt, follow_up) = self
                .claim_and_settle_blocker(id, parked_of(id), verdict, answer, &actor)
                .await?;
            // The addressed member's own receipt, not the first one settled —
            // `ids` is oldest-first, and the id this request named need not be
            // the oldest. Falls back to the first receipt only if `addressed`
            // is somehow not a member of `ids` at all.
            if id == addressed || head.is_none() {
                head = Some(receipt);
            }
            handles.extend(follow_up);
        }
        let head = head.expect("ids is non-empty, so the loop above ran at least once");
        // One handle for the whole group: each member's follow-up was spawned
        // as it settled (above), so this only joins them, in that same order,
        // rather than deciding when they start.
        let follow_up = tokio::spawn(async move {
            let mut last = CycleReport::default();
            let mut first_err = None;
            for handle in handles {
                match join_follow_up(handle).await {
                    Ok(report) => last = report,
                    Err(err) => {
                        first_err.get_or_insert(err);
                    }
                }
            }
            match first_err {
                Some(err) => Err(err),
                None => Ok(last),
            }
        });
        Ok((head, follow_up))
    }

    /// Posts the ask-which question back into the DM (issue #1862) as a durable
    /// reply, so it survives a reload the way any transcript line does. Attributed
    /// to the teammate whose DM this is — the `dm:<agent>` thread names them.
    ///
    /// `parent` is the root the caller resolved for the message this reply
    /// answers, re-resolved here through
    /// [`resolvable_parent`](Self::resolvable_parent): a blocker's anchor can be
    /// days old by the time it is answered, and a root that is gone threads
    /// nothing. Required rather than defaulted so a caller holding an anchor
    /// has to decide about it.
    #[cfg(feature = "openhuman")]
    pub(crate) async fn post_blocker_prompt(
        &self,
        thread: &str,
        parent: Option<EventSeq>,
        prompt: &str,
    ) -> Result<()> {
        let agent_id = thread.strip_prefix("dm:").unwrap_or(thread).to_string();
        let parent = self.resolvable_parent(parent, thread).await;
        self.events
            .append(
                &self.id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    parent,
                    chat_id: thread.to_string(),
                    agent_id,
                    text: prompt.to_string(),
                    steps: Vec::new(),
                    task_id: None,
                    outputs: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await?;
        Ok(())
    }

    /// Says, in the conversation itself, that an `@name` reached more than one
    /// thing and therefore reached nobody (B-101).
    ///
    /// Attributed to [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR), which the
    /// console renders as a centred system pill rather than as a teammate
    /// speaking — the runtime is reporting its own refusal, and putting a
    /// roster face on that would be a small lie about who decided.
    ///
    /// **Journaled, not returned in the POST response.** The response reaches
    /// only the sender's own request, and this has to survive a reload and be
    /// readable by anyone who later reads the channel — including the person
    /// wondering why a reply is talking about them in the third person. It is
    /// also what makes the notice reach an API poster, who never renders a chip
    /// and for whom the old signal (a missing chip) did not exist at all.
    ///
    /// Never fatal: a message whose advisory line could not be appended is still
    /// a delivered message, so a failure is logged and swallowed on exactly the
    /// terms `resolve_mentions` already refuses to fail a send.
    ///
    /// Threaded the same way every real reply is: `parent` is the root the
    /// caller resolved for the message this note is about, re-resolved here
    /// through [`resolvable_parent`](Self::resolvable_parent) so a note about a
    /// threaded reply lands in that thread rather than falling to the channel
    /// timeline — the doc comment above promises "in the conversation itself",
    /// and a thread is part of that conversation.
    pub async fn post_mention_ambiguity_note(
        &self,
        desk: &str,
        parent: Option<EventSeq>,
        refused: &[crate::runtime::mentions::AmbiguousMention],
    ) {
        let Some(text) = crate::runtime::mentions::ambiguity_note(refused) else {
            return;
        };
        let parent = self.resolvable_parent(parent, desk).await;
        if let Err(err) = self
            .events
            .append(
                &self.id,
                CompanyEvent::AgentReply {
                    parent,
                    chat_id: desk.to_string(),
                    agent_id: crate::ports::SYSTEM_AUTHOR.to_string(),
                    text,
                    steps: Vec::new(),
                    task_id: None,
                    outputs: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    // Desk-visible, the ordinary case: the whole point of the
                    // notice is that everyone reading the channel — including
                    // whoever the ping failed to reach — can see it.
                    audience: Vec::new(),
                },
            )
            .await
        {
            tracing::warn!(
                company = %self.id,
                desk = %desk,
                error = %err,
                "[mentions] an ambiguous @name could not be reported in the channel; the ping \
                 still reached nobody and now nothing says so"
            );
        }
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
        self.resolve_mentions_reporting(text, supplied, sender)
            .await
            .mentions
    }

    /// [`resolve_mentions`](Self::resolve_mentions), also reporting every
    /// `@name` that matched more than one thing and therefore matched nobody
    /// (B-101).
    ///
    /// The refusal itself is correct and long-standing — see
    /// [`crate::runtime::mentions`], never guess a ping — but it used to be
    /// announced only by the *absence* of a chip. An absence is not a signal: it
    /// is invisible in a wall of text and completely invisible over the API, so
    /// a founder's `@Priya` reached neither the teammate nor the person of that
    /// name, the channel's catch-all answered, and the reply talked about her in
    /// the third person. Whoever refuses has to be the one who says so, which is
    /// why this is here and not a second guess in the console.
    pub async fn resolve_mentions_reporting(
        &self,
        text: &str,
        supplied: Option<Vec<Mention>>,
        sender: Option<&Actor>,
    ) -> crate::runtime::mentions::Extraction {
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
            Ok(None) => return Default::default(),
            Err(err) => {
                tracing::warn!(
                    company = %self.id,
                    error = %err,
                    "[mentions] the company record could not be read; this message is \
                     journaled with no mentions"
                );
                return Default::default();
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
        crate::runtime::mentions::resolve_reporting(text, supplied, sender, &record, &users)
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
        // **Stopping the company stops the work already running** (B-037).
        //
        // Refusing new runs is only half of "Pause stops this company": a graph
        // that is twenty nodes into thirty goes on calling models and spending
        // for as long as it takes to finish, and the operator who pressed Pause
        // — often *because* of that run — watches it keep going with no control
        // that bites. `is_busy` already treats a live run as work; pause has to
        // agree with it.
        //
        // Fired **after** the durable write, never before: the lifecycle is what
        // `ensure_running` reads, so once it says paused nothing new can be
        // admitted, and a run cancelled before it landed could be replaced by
        // one racing in behind it.
        //
        // This is the Stop button's own path (`run_supervisor().cancel`, issue
        // #383), not a second stop mechanism: the engine checks the token at the
        // next node boundary, a node already executing finishes and is
        // journaled, and a wedged one is hard-aborted after a bounded grace. So
        // a paused run settles `cancelled` with its partial trail intact, the
        // same outcome and the same shape as the operator pressing Stop on each
        // one by hand.
        //
        // Best-effort, exactly as cancelling is everywhere else: a run
        // registered on a superseded runtime (see `RuntimeHandover`) still
        // finishes and still journals. Nothing here can fail the transition.
        if to != "running" {
            self.stop_live_runs(&to);
        }
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

    /// Fires the stop signal on every workflow run this company still has in
    /// flight, for a lifecycle that means "not running" (B-037).
    ///
    /// Separate from [`set_lifecycle`](Self::set_lifecycle) so the sweep is
    /// readable and directly testable, and because the cancel is deliberately
    /// synchronous: `RunSupervisor` is a `Mutex`-guarded map and firing a token
    /// does not await, so there is no window between the lifecycle write and the
    /// stop for another run to slip through.
    ///
    /// Deliberately **only** the workflow supervisor. A dispatched card or a
    /// desk delegation lives in the steer registry and is stopped by its own
    /// controls; folding them in here would make Pause a silent bulk-cancel of
    /// work the operator never saw a Stop button for. Workflow runs are what the
    /// promise on the settings screen is about, and what the report named.
    fn stop_live_runs(&self, to: &str) {
        let live = self.run_supervisor.live();
        if live.is_empty() {
            return;
        }
        tracing::info!(
            company = %self.id,
            lifecycle = %to,
            runs = live.len(),
            "company stopped: cancelling the workflow runs still in flight"
        );
        for (run_id, _workflow_id) in live {
            self.run_supervisor.cancel(&run_id);
        }
    }

    /// Fires the stop signal on every run of **one** workflow still in flight,
    /// and answers how many it fired at (B-121).
    ///
    /// The sibling of [`stop_live_runs`](Self::stop_live_runs), narrowed to one
    /// graph, for the single event that makes a run uncontrollable: deleting the
    /// workflow it belongs to. Delete already tears the schedule and the
    /// revisions down; the run is the piece it used to leave executing, and it
    /// left it with nowhere to be stopped from — the console's only Stop button
    /// lives on the workflow detail page that has just ceased to exist.
    ///
    /// Same mechanism and same guarantees as Pause's sweep and as the Stop
    /// button itself (`run_supervisor().cancel`, issue #383): the engine checks
    /// the token at the next node boundary, an executing node finishes and is
    /// journaled, and a wedged one is hard-aborted after a bounded grace. So the
    /// run settles `cancelled` with its partial trail intact, and its history
    /// row outlives the workflow exactly as every other past run of it does.
    ///
    /// Best-effort in the one way everything that cancels here is: a run
    /// registered on a superseded runtime (see `RuntimeHandover`) is not
    /// reachable from this supervisor and still finishes on its own.
    pub fn stop_runs_of_workflow(&self, workflow_id: &str) -> usize {
        let stopping: Vec<String> = self
            .run_supervisor
            .live()
            .into_iter()
            .filter(|(_, wf)| wf == workflow_id)
            .map(|(run_id, _)| run_id)
            .collect();
        if stopping.is_empty() {
            return 0;
        }
        tracing::info!(
            company = %self.id,
            workflow = %workflow_id,
            runs = stopping.len(),
            "workflow deleted: cancelling the runs of it still in flight"
        );
        stopping
            .iter()
            .filter(|run_id| self.run_supervisor.cancel(run_id))
            .count()
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

    /// Engages the emergency stop: the company admits no further work, and
    /// every new effect outside
    /// [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other) is denied,
    /// until an operator releases it.
    ///
    /// The halt is [`ensure_not_emergency_stopped`](Self::ensure_not_emergency_stopped),
    /// which has the in-flight semantics and the list of doorways it guards.
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
    pub async fn emergency_resume(
        self: &Arc<Self>,
        by: Actor,
        reason: Option<String>,
    ) -> Result<bool> {
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
        let released = self.approval_gate.set_emergency(false);
        if released {
            // Codex review finding on PR #2140 (`3951723403`): a blocked node
            // stranded by `reconcile_stranded_blocked_nodes`'s own emergency-stop
            // guard while this company was stopped otherwise sat armed until the
            // next full restart — that function's own doc says "this runs again
            // on the boot after the release", which was true only because
            // nothing ran it any sooner. Running it here catches the same case
            // up on the still-live process, the moment an operator actually
            // releases the stop, instead of leaving a durable decision
            // undelivered until somebody restarts the host.
            self.reconcile_stranded_blocked_nodes().await;
            // A durable explicit-request continuation whose dispatch this same
            // stop refused (`spawn_follow_up`'s check, above) is not a blocked-
            // node stash, so the reconciler above never sees it — it is still
            // sitting in `self.journal.approval_continuations`, exactly as a
            // cold boot's replay would find it. Re-arming and re-running the
            // boot recovery pair catches it up on this live process instead of
            // leaving it for the next restart.
            self.arm_replayed_continuation_recovery();
            self.schedule_replayed_continuations();
            // Codex review finding on PR #2140 (`3955615146`): a durable
            // blocker answer whose settlement this same stop refused
            // (`settle_approval`'s emergency check) is neither an explicit
            // continuation nor a blocked-node stash, so neither call above
            // sees it — it is still sitting in `self.journal.blocker_resolutions`,
            // exactly as a cold boot's replay would find it (`recover`, above).
            // Re-arming and re-running that pair catches it up on this live
            // process instead of leaving it for the next restart.
            self.arm_replayed_blocker_recovery();
            self.schedule_replayed_blocker_resolutions();
            // Codex review finding on PR #2140 (`3955615141`): a workflow gate
            // batch this same stop refused to release
            // (`WorkflowGateQueue::release`'s emergency check) is left fully
            // decided in the queue rather than destroyed, exactly so this can
            // hand it back to `resume_run` the moment the stop lifts instead of
            // requiring an operator to notice and manually re-run the workflow.
            for turn in self.workflow_gates.ready_for_release() {
                let rt = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(error) =
                        crate::runtime::workflow_resume::resume_run(&rt, &turn).await
                    {
                        tracing::error!(
                            company = %rt.id,
                            %turn,
                            %error,
                            "[approval] a workflow gate batch released by an emergency-resume \
                             redrive failed"
                        );
                        // CodeRabbit review finding on PR #2140 (`3960328835`):
                        // matches `resume_workflow_run`'s own failure path — a
                        // silent log here is every sign-off already in, with
                        // nothing telling the operator the run still needs a
                        // manual re-run.
                        rt.announce_to_operator(&format!(
                            "Every sign-off on a workflow step blocked by the emergency stop is \
                             in, but the run could not be restarted after release: {error}. \
                             Re-run the workflow to pick it back up."
                        ))
                        .await;
                    }
                });
            }
        }
        Ok(released)
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

/// The root-cause group a parked blocker belongs to (issue #1862), or `None`
/// for an ordinary approval or an ungrouped blocker.
///
/// Reads the `group_key` off the blocker payload, gated on the effect kind so a
/// non-blocker effect that happens to carry a `group_key`-shaped field is never
/// mistaken for one.
fn blocker_group_key_of(effect: &crate::ports::types::Effect) -> Option<String> {
    if !effect.kind.starts_with(&format!(
        "{}.",
        crate::ports::blockers::BLOCKER_EFFECT_PREFIX
    )) {
        return None;
    }
    serde_json::from_value::<crate::ports::blockers::BlockerPayload>(effect.payload.clone())
        .ok()?
        .group_key
}

/// Which kind of stopped step a parked blocker names — `"task"` or `"node"`
/// — or `None` for a non-blocker effect or a blocker with no step behind it.
///
/// Reads [`BlockerPayload::step`](crate::ports::blockers::BlockerPayload::step)
/// the same guarded way [`blocker_group_key_of`] reads `group_key`: gated on
/// the effect kind so a non-blocker effect that happens to carry a
/// step-shaped field is never mistaken for one. The console needs this to
/// word `skip`/`cancel` honestly — those verdicts do not do the same thing to
/// a paused board card that they do to a stopped workflow node.
fn blocker_step_kind_of(effect: &crate::ports::types::Effect) -> Option<String> {
    if !effect.kind.starts_with(&format!(
        "{}.",
        crate::ports::blockers::BLOCKER_EFFECT_PREFIX
    )) {
        return None;
    }
    let step =
        serde_json::from_value::<crate::ports::blockers::BlockerPayload>(effect.payload.clone())
            .ok()?
            .step?;
    Some(blocker_step_kind_str(&step).to_string())
}

/// `"task"` or `"node"` for a [`BlockerStep`](crate::ports::blockers::BlockerStep),
/// the single spelling both [`blocker_step_kind_of`] and
/// [`CompanyRuntime::pending_blocker_groups`] read it into.
fn blocker_step_kind_str(step: &crate::ports::blockers::BlockerStep) -> &'static str {
    use crate::ports::blockers::BlockerStep;

    match step {
        BlockerStep::Task { .. } => "task",
        BlockerStep::Node { .. } => "node",
    }
}

/// One root-cause group of parked blockers pending in a single DM (issue
/// #1862): the approvals that share a cause, plus a one-line label for the
/// ask-which prompt.
#[cfg(feature = "openhuman")]
struct PendingBlockerGroup {
    /// The shared `group_key`, or `None` for a lone ungrouped blocker.
    key: Option<String>,
    /// The step kind every member stopped at — folded alongside `key` so a
    /// group never mixes a task-step blocker with a node-step one.
    step_kind: Option<&'static str>,
    /// Every approval in the group — the set a single verdict fans across.
    ids: Vec<ApprovalId>,
    /// The first line of the blocker's reason, for disambiguation copy.
    label: String,
}

#[cfg(feature = "openhuman")]
impl PendingBlockerGroup {
    /// The connection name a `connection:<name>` group is about, used to match
    /// a reply that names it. `None` for an ungrouped blocker, which therefore
    /// never auto-resolves out of an ambiguous set — it can only be answered
    /// from its own card.
    fn connection_hint(&self) -> Option<&str> {
        self.key
            .as_deref()
            .and_then(|k| k.strip_prefix("connection:"))
    }
}

/// What resolving a blocker reply does, decided before anything is written.
#[cfg(feature = "openhuman")]
#[derive(Debug)]
pub(crate) enum BlockerReplyPlan {
    /// Not a verdict, or no blocker pending here — run an ordinary turn.
    NotBlocker,
    /// Settle these approvals with the operator's verdict.
    Resolve {
        ids: Vec<ApprovalId>,
        intent: crate::company::task_intent::BlockerReplyIntent,
    },
    /// Several blockers pend and the reply named none — ask which, settle none.
    AskWhich { prompt: String },
}

/// The reason stamped on a card an operator cancelled from its blocker DM
/// (issue #1863). Its own wording — nothing failed and nothing timed out; the
/// operator chose to stop the work.
#[cfg(feature = "openhuman")]
const BLOCKER_CANCELLED: &str =
    "cancelled from the blocker chat — the work was stopped, not failed";

#[cfg(feature = "openhuman")]
const BLOCKER_WAIVED: &str = "blocker question waived by the operator — no work was run";

/// The one line posted back into a blocker's DM when its answer re-enters the
/// stopped step (issue #1863), phrased per verdict so the operator sees what
/// their answer did.
#[cfg(feature = "openhuman")]
fn blocker_resume_note(resolution: &crate::ports::blockers::BlockerResolution) -> String {
    use crate::ports::blockers::BlockerVerdict;
    match resolution.verdict {
        BlockerVerdict::Retry => "Got it — picking that back up now.".to_string(),
        BlockerVerdict::Amend => {
            "Thanks — using that and carrying on from where it stopped.".to_string()
        }
        BlockerVerdict::Skip => "Okay — skipping that and moving on.".to_string(),
        // Cancel never reaches here: it does not resume, and its callers post
        // their own settle notice.
        BlockerVerdict::Cancel => "Okay — cancelled.".to_string(),
    }
}

/// The line asked back when a DM holds several distinct blocked things and the
/// reply did not name one (issue #1862). Says what is blocked and asks which —
/// and deliberately does not promise that answering resumes anything (#1863).
#[cfg(feature = "openhuman")]
fn ask_which_prompt(groups: &[PendingBlockerGroup]) -> String {
    let items = groups
        .iter()
        .enumerate()
        .map(|(i, group)| format!("{}. {}", i + 1, group.label))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "A few different things are blocked in this chat. Which one do you mean?\n{items}\n\nReply naming it and what you'd like done."
    )
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
#[path = "runtime_ambiguous_mentions_tests.rs"]
mod tests_ambiguous_mentions;
#[cfg(test)]
#[path = "runtime_approval_tests.rs"]
mod tests_approval;
#[cfg(test)]
#[path = "runtime_blocked_node_tests.rs"]
mod tests_blocked_node;
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_blocker_claim_tests.rs"]
mod tests_blocker_claim;
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_blocker_dms_concurrency_tests.rs"]
mod tests_blocker_dms_concurrency;
/// A blocker surfaces in the responsible teammate's DM, groups by root
/// cause, and an operator's reply routes back as a verdict.
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_blocker_dms_reply_tests.rs"]
mod tests_blocker_dms_reply;
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_blocker_race_tests.rs"]
mod tests_blocker_race;
/// Resuming the stopped step — a task card is re-dispatched, a cancel
/// settles it — and a blocker's inert effect is never executed.
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_blocker_resume_card_tests.rs"]
mod tests_blocker_resume_card;
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_blocker_resume_console_tests.rs"]
mod tests_blocker_resume_console;
#[cfg(test)]
#[path = "runtime_conversation_tests.rs"]
mod tests_conversation;
#[cfg(test)]
#[path = "runtime_core_tests.rs"]
mod tests_core;
#[cfg(test)]
#[path = "runtime_dispatch_tests.rs"]
mod tests_dispatch;
#[cfg(test)]
#[path = "runtime_emergency_stop_tests.rs"]
mod tests_emergency_stop;
/// What is under test is whether a continuation run is started, with what
/// trigger input, and how many times.
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_node_blocker_resume_retry_tests.rs"]
mod tests_node_blocker_resume_retry;
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_node_blocker_resume_stash_tests.rs"]
mod tests_node_blocker_resume_stash;
#[cfg(test)]
#[path = "runtime_notify_mentions_tests.rs"]
mod tests_notify_mentions;
/// The thread-as-review-surface: a reply to a settled `in_review` dispatch
/// card's settle pill or relay bubble routes as review feedback and re-runs
/// the card; an Approve verdict finishes it.
#[cfg(feature = "openhuman")]
#[cfg(test)]
#[path = "runtime_review_tests.rs"]
mod tests_review;
