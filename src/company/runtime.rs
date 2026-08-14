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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::feedback::service::{FeedbackFiler, FeedbackResponse};
use crate::feedback::store::FeedbackStore;
use crate::feedback::types::{FeedbackInput, FeedbackItem, FeedbackSummary};
use crate::policy::ManifestApprovalGate;
use crate::ports::now_millis;
use crate::ports::types::{
    Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, EventSeq, Verdict,
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

/// The board column a task must enter to be dispatched to its assignee. Read
/// from the task port (#205) so this edge and the write boundary that validates
/// the column cannot drift onto two different literals.
use crate::ports::tasks::COLUMN_IN_PROGRESS as IN_PROGRESS;
/// The board column a task must enter to be planned (issue #337). Read from the
/// task port for the same reason the dispatch literal is.
use crate::ports::tasks::COLUMN_PLANNING as PLANNING;

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
use crate::runtime::continuation::ContinuationQueue;
use crate::runtime::cycle::ResolveReceipt;
use crate::runtime::grants::{GRANT_TTL_MILLIS, GrantId, GrantScope, GrantSet, StandingGrant};
use crate::runtime::journal::{ApprovalOrigin, ExecutedEffect, RuntimeJournal};
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};
use crate::server::ops::mailer::MailSender;
use crate::server::ops::smtp::SmtpCredentials;

/// The WS3 console ports, bundled so the runtime constructor stays legible.
/// Each is an `Arc<dyn …>` keyed by [`CompanyId`], defaulting to the fs backend
/// and overridden together when a non-fs backend is selected.
#[derive(Clone)]
pub struct OpsStores {
    /// The durable task board.
    pub tasks: Arc<dyn TaskStore>,
    /// The durable workspace file tree.
    pub workspace: Arc<dyn WorkspaceStore>,
    /// The durable memory-facts view.
    pub facts: Arc<dyn FactStore>,
    /// Versioned task artifacts and their human-edit history (#187).
    pub artifacts: Arc<dyn ArtifactStore>,
    /// First-class records of each task attempt: status, trace, cost (#242).
    pub runs: Arc<dyn RunStore>,
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
    pub(crate) id: CompanyId,
    pub(crate) brain: Arc<dyn Brain>,
    pub(crate) store: Arc<dyn CompanyStore>,
    pub(crate) events: Arc<dyn EventLog>,
    pub(crate) memory: Arc<dyn MemoryStore>,
    pub(crate) context: Arc<dyn ContextStore>,
    pub(crate) tools: Arc<dyn ToolProvider>,
    pub(crate) channels: Vec<Arc<dyn ChannelAdapter>>,
    pub(crate) economy: Option<Arc<dyn AgentEconomy>>,
    pub(crate) approvals: Arc<dyn ApprovalGate>,
    /// The concrete gate, kept alongside the `dyn` port so the runtime can reach
    /// the amend and expiry-sweep methods that live outside the trait without a
    /// downcast.
    pub(crate) approval_gate: Arc<ManifestApprovalGate>,
    pub(crate) journal: Arc<RuntimeJournal>,
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
    /// The platform-injected bootstrap admin address
    /// (`OPENCOMPANY_ADMIN_EMAIL`, pre-normalized), a standing admin-in-waiting on
    /// a provisioned tenant whose manifest names nobody (issue #661). Install-wide
    /// rather than a constructor argument, set by the builder like
    /// [`source_dir`](Self::set_source_dir); read when resolving `owner`
    /// recipients so the approval-notification path (#750) reaches a fresh tenant
    /// the same way workflow delivery does.
    pub(crate) bootstrap_admin: Option<String>,
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
    /// Issue #245: the company's bound repositories and their host-side mirror
    /// cache, when the runtime was built over a filesystem home.
    ///
    /// `None` is a real state, not an omission: the manager is rooted at
    /// `companies/<slug>/repos/`, so a runtime assembled from injected ports
    /// with no home (a test harness, an embedding) has no cache to manage and
    /// the ops routes answer "not wired" rather than inventing a location.
    ///
    /// Compiled in every build. Nothing here is agent-facing — there is no
    /// grant and no tool in this tier — so it needs no feature gate, and the
    /// forge HTTP client it can optionally hold is the only part that does.
    pub(crate) repos: Option<Arc<crate::runtime::RepoManager>>,
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
    /// Held for the duration of a cycle so cycles never interleave per company.
    ///
    /// `Arc`-shared rather than owned so a rebuilt runtime can inherit the *same*
    /// lock (issue #290). Two runtimes for one company each holding their own
    /// mutex would mean two cycles running at once against a store whose `save`
    /// writes the whole record, which is exactly the invariant this exists to
    /// hold. Handing the lock over is also what makes
    /// [`quiesce`](Self::quiesce)'s drain meaningful across the swap.
    pub(crate) serial: Arc<TokioMutex<()>>,
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
    /// MCP installs and live connections for this runtime. The wrapper owns a
    /// company-home-scoped OpenHuman config while the live registry remains
    /// shared in-process with harness agents.
    #[cfg(feature = "mcp")]
    pub(crate) mcp: Option<Arc<crate::harness::mcp::McpRuntime>>,
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
            tools,
            channels,
            economy,
            approvals,
            approval_gate,
            journal,
            secrets,
            inbox,
            mail,
            ops,
            feedback,
            filer,
            source_dir: None,
            bootstrap_admin: None,
            workflow_runner: None,
            steer: crate::company::steer::InflightRegistry::new(),
            run_supervisor: crate::runtime::RunSupervisor::new(),
            repos: None,
            grants,
            continuations: ContinuationQueue::default(),
            serial: Arc::new(TokioMutex::new(())),
            task_writes: Arc::new(TokioMutex::new(())),
            quiesced: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "openhuman")]
            harness: None,
            #[cfg(feature = "openhuman")]
            planner: None,
            #[cfg(feature = "openhuman")]
            builder: None,
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

    /// Sets the platform-injected bootstrap admin address, pre-normalized by
    /// `AppConfig::bootstrap_admin` (issue #661). Set by the builder from the same
    /// resolved config the workflow delivery path reads, so `owner` recipients
    /// resolve identically on both.
    pub fn set_bootstrap_admin(&mut self, bootstrap_admin: Option<String>) {
        self.bootstrap_admin = bootstrap_admin;
    }

    /// The company's on-disk source directory, when built on the serve path.
    /// `None` in platform-provisioned mode.
    pub fn source_dir(&self) -> Option<&Path> {
        self.source_dir.as_deref()
    }

    /// Issue #245: attach the repository manager after construction, wired by
    /// the [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) from the same
    /// filesystem home the company's bundle hangs off.
    pub fn set_repos(&mut self, repos: Arc<crate::runtime::RepoManager>) {
        self.repos = Some(repos);
    }

    /// The company's bound repositories, if a mirror cache is wired. `None` on
    /// a runtime built without a filesystem home, where the ops routes report
    /// the surface as not wired.
    pub fn repos(&self) -> Option<&Arc<crate::runtime::RepoManager>> {
        self.repos.as_ref()
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
    pub fn journal(&self) -> &Arc<RuntimeJournal> {
        &self.journal
    }

    /// This company's durable record store.
    pub fn store(&self) -> &Arc<dyn CompanyStore> {
        &self.store
    }

    /// The ids of the chat channels actually wired for this running company —
    /// exactly what an `output` node's `channel` destination may target
    /// (issue #813). `operator` is always present; the rest are the enabled
    /// OpenHuman-provider manifest channels. The console reads this to offer a
    /// picker of real targets instead of a free-text box that only fails at
    /// delivery time with `ChannelNotWired`.
    pub fn wired_channel_ids(&self) -> Vec<String> {
        self.channels
            .iter()
            .map(|channel| channel.channel_id().to_string())
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

    /// The platform-injected bootstrap admin address (`OPENCOMPANY_ADMIN_EMAIL`),
    /// pre-normalized — a standing admin-in-waiting on a provisioned tenant whose
    /// manifest names nobody (issue #661). Read when resolving `owner` recipients
    /// for an approval notification (#750).
    pub(crate) fn bootstrap_admin(&self) -> Option<&str> {
        self.bootstrap_admin.as_deref()
    }

    /// This company's task board.
    pub fn tasks(&self) -> &Arc<dyn TaskStore> {
        &self.ops.tasks
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
    pub async fn upsert_task(self: &Arc<Self>, task: &TaskRecord) -> Result<()> {
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
        self.ops.tasks.upsert(&self.id, task).await?;
        if dispatch {
            self.dispatch_task(task).await;
        }
        if plan {
            self.plan_task(task);
        }
        Ok(())
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
        let _ = task;
    }

    /// The body of a dispatch's detached cycle (issue #242), split out of the
    /// `tokio::spawn` so the quiesce path below is reachable from a test.
    ///
    /// Owns the settle for the one dispatch failure the cycle's own terminality
    /// backstop cannot see — see [`abandon_run`](Self::abandon_run).
    #[cfg(feature = "openhuman")]
    async fn run_dispatch_cycle(self: Arc<Self>, task_id: String, run_id: Option<String>) {
        let Err(err) = self
            .run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: task_id.clone(),
                run_id: run_id.clone(),
            }])
            .await
        else {
            return;
        };
        // Issue #290 meets issue #242. `ensure_accepting` refuses *before*
        // `CycleRunner` takes the serial lock, so a dispatch that lands in the
        // window while this runtime is being replaced never reaches `begin_run`
        // — and the backstop inside the cycle only settles rows that cycle
        // started. Every other dispatch failure is already covered in there.
        // Left alone, the row minted a moment ago would sit `Pending` for the
        // rest of the process's life: a card reading as under way by an attempt
        // that never began, which nothing re-drives, and which the rebuild
        // deliberately does *not* run the boot reaper to clean up.
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
        if let Err(err) = crate::runtime::advance::advance_settled_card(
            self.ops.tasks.as_ref(),
            &self.id,
            task_id,
            crate::ports::runs::RunStatus::Failed,
            crate::ports::runs::RUNTIME_REPLACED_ERROR,
        )
        .await
        {
            tracing::warn!(
                company = %self.id,
                run = %run_id,
                task = %task_id,
                error = %err,
                "[runs] settled an attempt refused by a quiescing runtime but could not return \
                 its card; it stays in progress until the next boot"
            );
        }
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
        let spec = crate::ports::runs::NewRun {
            id: crate::ports::generate_id(),
            task_id: task.id.clone(),
            agent_id: task.assignee.clone(),
        };
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
    pub fn adopt_locks(&mut self, serial: Arc<TokioMutex<()>>, task_writes: Arc<TokioMutex<()>>) {
        self.serial = serial;
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

    /// Rejects a cycle on a runtime that is being replaced.
    ///
    /// Separate from [`ensure_running`](Self::ensure_running): that one reads a
    /// durable lifecycle an operator chose (paused, archived) and renders `409`;
    /// this one is a process-local window that clears itself within a turn and
    /// renders `503`.
    fn ensure_accepting(&self) -> Result<()> {
        if self.is_quiesced() {
            return Err(OpenCompanyError::Quiescing(self.id.as_ref().to_string()));
        }
        Ok(())
    }

    /// Runs one cycle over a batch of events, returning what happened.
    pub async fn run_cycle(&self, events: Vec<CompanyEvent>) -> Result<CycleReport> {
        self.ensure_accepting()?;
        CycleRunner::new(self).run(events).await
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
        Ok((receipt.clone(), self.spawn_follow_up(receipt)))
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
                ResolveReceipt::Settled(event) => event,
            };
            rt.continue_turn(event).await
        })
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

        // `Some(None)` is a park recorded before the turn key existed; `None` is
        // an id this journal never parked. Neither is gated.
        let turn = self.journal.approval_cycle(&approval_id).flatten();
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
        if batch.is_empty() {
            // Every approval the turn raised expired rather than being decided.
            // The sweep already appended each `ApprovalResolved` itself, so
            // there is nothing left to tell the brain.
            return Ok(CycleRunner::new(self).already_resolved_report());
        }
        self.run_continuation(&approval_id, batch).await
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
        match CycleRunner::new(self).run(batch).await {
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

    async fn publish_continuation(&self, approval_id: &ApprovalId, report: &mut CycleReport) {
        let conversation = self
            .journal
            .approval_conversation(approval_id)
            .unwrap_or_default();
        let thread = conversation.thread;
        for response in &mut report.responses {
            let chat_id = thread.clone().unwrap_or_else(|| response.channel.clone());
            // Checked against the channel actually being answered into, not
            // against the recorded thread: when `thread` is absent the reply
            // goes to the responding agent's own channel, and a root belonging
            // to some other channel must not follow it there.
            let parent = self.resolvable_parent(conversation.parent, &chat_id).await;
            match self
                .events
                .append(
                    &self.id,
                    CompanyEvent::AgentReply {
                        parent,
                        chat_id,
                        agent_id: response.channel.clone(),
                        text: response.text.clone(),
                        steps: response.steps.clone(),
                        task_id: response.task_id.clone(),
                    },
                )
                .await
            {
                Ok(seq) => response.message_id = Some(seq.value().to_string()),
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
            .append(
                &self.id,
                CompanyEvent::AgentReply {
                    parent,
                    chat_id: thread,
                    agent_id: crate::runtime::channel::OPERATOR_CHANNEL.to_string(),
                    text: "Your approval was recorded, but the agent could not pick the work \
                           back up. Nothing was half-done — approving again is safe and will \
                           retry it."
                        .to_string(),
                    steps: Vec::new(),
                    task_id: None,
                },
            )
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
                text: format!(
                    "Recorded. The agent picks this back up once the remaining {outstanding} \
                     sign-off{} on this step {} decided.",
                    if outstanding == 1 { "" } else { "s" },
                    if outstanding == 1 { "is" } else { "are" },
                ),
                steps: Vec::new(),
                reply_to: None,
            }],
            executed_effects: Vec::new(),
            parked: Vec::new(),
            persisted_seq: None,
            input_seqs: Vec::new(),
        }
    }

    /// Sweeps every parked approval past its TTL, resolving each to a
    /// default-deny and writing an `ApprovalExpired` audit entry to the journal.
    /// Returns the ids that expired. Driven by the runtime's maintenance timer.
    ///
    /// Each expiry also appends a `ApprovalResolved { verdict: Deny }` event
    /// attributed to the system. Expiry *is* a resolution — a default-deny on
    /// silence — but before this it wrote only the journal record, so a wait
    /// that ended in a timeout produced no event at all and was invisible to
    /// every event-log reader, including the task timeline (issue #305). The
    /// append is best-effort for the same reason steer's audit is: a sweep that
    /// already denied the effect must not be undone by a log write, and the
    /// journal remains the binding audit trail either way.
    ///
    /// An expiry is also a **decision** as far as issue #469's continuation gate
    /// is concerned, and has to be, or a turn that raised four sign-offs and
    /// only ever got three would wait for a fourth that is never coming. The
    /// turn is released here; the `ApprovalResolved` this appends is the event
    /// the brain gets, so the release contributes no second one.
    pub async fn sweep_expired_approvals(self: &Arc<Self>) -> Result<Vec<ApprovalId>> {
        let now = now_millis();
        let expired = self.approval_gate.sweep_expired(now);
        for id in &expired {
            self.journal.record_expired(id, now).await?;
            // Issue #796: the parked approval is gone, so its work unit is no
            // longer awaiting a resume — drop the pending mark so the checkout it
            // was holding across the park becomes sweepable.
            self.grants.clear_pending(id);
            // Issue #469: releasing the turn this approval was blocking, and
            // running its continuation when this expiry was the last thing it
            // waited on. Spawned rather than awaited: the continuation is a full
            // agent turn behind the per-company cycle lock, and the maintenance
            // tick this runs on fires on a minute boundary for every company.
            if let Some(turn) = self.journal.approval_cycle(id).flatten()
                && let Some(batch) = self.continuations.decide(&turn, None)
                && !batch.is_empty()
            {
                let rt = Arc::clone(self);
                let released = id.clone();
                tokio::spawn(async move {
                    if let Err(error) = rt.run_continuation(&released, batch).await {
                        tracing::error!(
                            company = %rt.id,
                            %error,
                            "[approval] the continuation released by an expiry failed"
                        );
                    }
                });
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
        }
        Ok(expired)
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
                            text: text.clone(),
                            steps: Vec::new(),
                            reply_to: None,
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
                        text: text.to_string(),
                        steps: Vec::new(),
                        reply_to: None,
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
    pub async fn recover(&self) -> Result<()> {
        CycleRunner::new(self).recover().await
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

    /// The approvals currently awaiting the operator.
    ///
    /// The single projection point for [`ApprovalSummary`], and therefore the
    /// single place issue #372's `agent` + `payload` are filled in. The payload
    /// is redacted and bounded **here**, before it is a summary at all, so no
    /// caller can accidentally serialize the raw effect.
    pub fn pending_approvals(&self) -> Vec<ApprovalSummary> {
        self.journal
            .pending()
            .into_iter()
            .map(|p| ApprovalSummary {
                id: p.id,
                kind: p.effect.kind.clone(),
                amount_usd: p.effect.amount_usd,
                at_millis: p.at_millis,
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
                broadly_grantable: p.effect.agent.is_some() && p.effect.may_be_granted_standing(),
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
    pub async fn submit_feedback(
        &self,
        input: FeedbackInput,
        preview: bool,
    ) -> Result<FeedbackResponse> {
        let item = self.capture_feedback(input).await?;
        let manifest = self.store.load(&self.id).await?.map(|r| r.manifest);
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

    /// A status snapshot, loading the company record for name and lifecycle.
    pub async fn status(&self) -> Result<CompanyStatus> {
        let record = self.store.load(&self.id).await?;
        let (name, lifecycle, template_provenance) = match record {
            Some(record) => (
                record.manifest.company.name,
                record.lifecycle,
                record.template_provenance,
            ),
            None => (self.id.to_string(), "running".to_string(), None),
        };
        Ok(CompanyStatus {
            id: self.id.clone(),
            name,
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
        let mut record = self
            .store
            .load(&self.id)
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(self.id.to_string()))?;
        let from = record.lifecycle.clone();
        record.lifecycle = to.clone();
        self.store.save(&record).await?;
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
    use super::{emergency_from_load, task_enters_in_progress, task_enters_planning};

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
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
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
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
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
                    text: "pay the invoice".into(),
                    by: None,
                    chat: Some("desk-finance".into()),
                    parent: None,
                },
            )
            .await
            .expect("append");
        let elsewhere = rt
            .events
            .append(
                &rt.id,
                CompanyEvent::OperatorMessage {
                    text: "unrelated".into(),
                    by: None,
                    chat: Some("desk-ops".into()),
                    parent: None,
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
                    text: "and another thing".into(),
                    by: None,
                    chat: Some("desk-finance".into()),
                    parent: None,
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
                            text: "ship it".into(),
                            by: None,
                            chat: chat.map(str::to_string),
                            parent: None,
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
                    text: "unrelated".into(),
                    by: None,
                    chat: Some("desk-ops".into()),
                    parent: None,
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
}
