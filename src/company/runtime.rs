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
use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, CompanyId, Verdict};
use crate::ports::{
    AgentEconomy, ApprovalGate, ArtifactStore, Brain, ChannelAdapter, CompanyStore, ContextStore,
    EventLog, FactStore, InboxStore, LoginCodeStore, MemoryStore, RunStore, SecretStore,
    SessionStore, SkillStateStore, TaskRecord, TaskStore, ToolProvider, UsageMeter, UserStore,
    WorkspaceStore,
};

/// The board column a task must enter to be dispatched to its assignee. Read
/// from the task port (#205) so this edge and the write boundary that validates
/// the column cannot drift onto two different literals.
use crate::ports::tasks::COLUMN_IN_PROGRESS as IN_PROGRESS;

/// Whether an upsert moves a card **into** `in_progress` (the dispatch edge).
/// A card already in `in_progress` re-saved is not a fresh dispatch.
fn task_enters_in_progress(prev_column: Option<&str>, next_column: &str) -> bool {
    next_column == IN_PROGRESS && prev_column != Some(IN_PROGRESS)
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
    /// The usage meter (written by the WS4 cost hook, read by WS5).
    pub usage: Arc<dyn UsageMeter>,
    /// Operator deltas over the company's skills.
    pub skills: Arc<dyn SkillStateStore>,
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
            workflow_runner: None,
            steer: crate::company::steer::InflightRegistry::new(),
            run_supervisor: crate::runtime::RunSupervisor::new(),
            grants,
            serial: Arc::new(TokioMutex::new(())),
            task_writes: Arc::new(TokioMutex::new(())),
            quiesced: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "openhuman")]
            harness: None,
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

    /// The company's on-disk source directory, when built on the serve path.
    /// `None` in platform-provisioned mode.
    pub fn source_dir(&self) -> Option<&Path> {
        self.source_dir.as_deref()
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

    /// Upserts a board task and edge-fires a dispatch when the write moves the
    /// card **into** `in_progress` — the drag into `in_progress` is the human
    /// approval gate. The single write site for REST task mutations, so the
    /// trigger cannot be bypassed by writing straight to the store.
    ///
    /// The dispatch is detached (see [`dispatch_task`](Self::dispatch_task)), so
    /// the HTTP write returns immediately; the agent turn's result lands back on
    /// the card asynchronously. Without an attached harness the board stays inert
    /// — the card simply rests in `in_progress`.
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
        self.ops.tasks.upsert(&self.id, task).await?;
        if dispatch {
            self.dispatch_task(task).await;
        }
        Ok(())
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
    /// A failed follow-up is logged here rather than swallowed, because the
    /// detached caller has nowhere to put it. It is genuinely recoverable: the
    /// verdict and the grant are already durable, and re-approving is a safe
    /// no-op (`ResolveOutcome::NotParked` → the already-resolved report, per
    /// issue #243), so the operator can retry without minting a second grant.
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
            let report = CycleRunner::new(&rt).run(vec![event]).await;
            if let Err(error) = &report {
                tracing::error!(
                    company = %rt.id,
                    %error,
                    "[approval] the follow-up cycle after a resolved approval failed; \
                     the verdict and the grant are already durable, so the agent was \
                     not told the outcome — re-approving is a safe no-op and will \
                     re-run it"
                );
            }
            report
        })
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
    pub async fn sweep_expired_approvals(&self) -> Result<Vec<ApprovalId>> {
        let now = now_millis();
        let expired = self.approval_gate.sweep_expired(now);
        for id in &expired {
            self.journal.record_expired(id, now).await?;
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
                // Issue #374. Both halves matter: a native effect has no
                // teammate and no tool to grant, and a named consequence group
                // stays a per-call decision. Read off the parked effect, so the
                // control is offered on exactly the classification the card
                // itself is showing.
                broadly_grantable: p.effect.agent.is_some()
                    && p.effect.group.is_broadly_grantable(),
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
    use super::task_enters_in_progress;

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
}
