//! The [`RunStore`] port: a task attempt as a first-class, queryable record.
//!
//! A dispatched task used to be an *ephemeral* cycle — a
//! [`CycleRequest`](crate::ports::types::CycleRequest) whose scrubbed
//! [`TurnStep`] list was folded once into the `AgentReply` event and then
//! discarded as a unit. There was no id for the attempt, no status while it
//! ran, and no per-attempt cost, so a crash between the dispatch edge and the
//! spawned cycle left nothing at all behind.
//!
//! This port records that attempt. A [`RunRecord`] is *mutable, queryable
//! state*; the event log stays the immutable trail. The two are deliberately
//! distinct: folding the whole journal per read is what makes the existing
//! workflow-run list expensive, and one event per [`TurnStep`] would flood
//! every chat-history reload.
//!
//! ## The transition API
//!
//! The port does **not** expose a free-form "upsert a run" verb as its API.
//! A task board that accepted any string as a column silently lost cards until
//! #205 put the vocabulary behind a check ([`crate::ports::tasks`]); the same
//! argument applies here with more force, because a run's status is a state
//! *machine*, not a label. So writers move a run with
//! [`RunStore::create_run`], [`RunStore::begin_run`] and
//! [`RunStore::finish_run`], and the legality of each move is enforced here, in
//! the port layer, once — backends only ever see dumb storage verbs.
//!
//! ## Paused is not WaitingApproval
//!
//! Epic #183 settled that **who unblocks the work** decides where it parks: if
//! a person has to act, the attempt is [`RunStatus::WaitingApproval`];
//! everything else — a dependency, a rate limit, a missing credential, a retry
//! — is [`RunStatus::Paused`]. Both are non-terminal, and
//! [`RunStatus::WaitingApproval`] is deliberately **re-enterable**: #243 grants
//! are single-use and argument-exact, so an attempt that needed three separate
//! approvals must be able to park three separate times. A state machine that
//! allowed only one approval stop would force those into one batched grant,
//! which is exactly what #243 rules out.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::ports::types::{CompanyId, EventSeq, TokenUsage, TurnStep};

/// The `error` a boot reaper stamps on a run it found still active.
///
/// See [`reap_orphaned_runs`] for why an active row at boot is provably dead
/// rather than merely suspicious.
pub const ORPHAN_ERROR: &str =
    "the host restarted while this attempt was running, so its outcome was lost";

/// The `error` stamped on an attempt whose dispatch was refused because the
/// company's runtime was being replaced (issue #290).
///
/// Deliberately distinct from [`ORPHAN_ERROR`]. That one means the host process
/// died and nothing is left to say what happened; this one means the process is
/// perfectly healthy and the company's runtime was swapped underneath a card an
/// operator had just dispatched, which is a retry-me rather than an incident.
pub const RUNTIME_REPLACED_ERROR: &str =
    "the company runtime was being replaced when this card was dispatched, so it never started";

// ---------------------------------------------------------------------------
// RunStatus
// ---------------------------------------------------------------------------

/// Where one task attempt currently stands.
///
/// Serialized in `snake_case` (`waiting_approval`), matching the board's own
/// column vocabulary, and [`Self::as_str`] returns the identical literal so a
/// backend's indexed `status` column and the JSON blob can never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Minted, not yet started. The row exists *before* the cycle spawns, which
    /// is the whole point: a crash in the gap is visible instead of silent.
    Pending,
    /// The cycle is executing.
    Running,
    /// Parked because **a person** must act — an approval, a review, a
    /// decision. Re-enterable: see the module docs.
    WaitingApproval,
    /// Parked because something *other than a person* must resolve — a
    /// dependency, a rate limit, a missing credential, a retry, an operator
    /// steer-to-pause. Neither cancelled nor failed.
    Paused,
    /// Parked on a **question for a person** the work cannot answer itself
    /// (issue #1861): a rejected model id, an expired credential, a missing
    /// prerequisite, or an agent's own `escalate_to_human`.
    ///
    /// Distinct from both neighbours on purpose. [`Paused`](Self::Paused) is
    /// documented as waiting on something *other than a person*, which is the
    /// opposite of a blocker. [`WaitingApproval`](Self::WaitingApproval) is a
    /// person deciding whether an effect may happen — there is an effect, and
    /// approving it performs it; a blocker has no effect to approve, only an
    /// answer to supply. Folding blockers into either would make "what is this
    /// waiting for" unanswerable from the status, which is the whole point of
    /// the epic.
    ///
    /// Parked, not terminal: the answer resumes the stopped step (#1863/#1864),
    /// and an unanswered blocker expires through the approval TTL back to
    /// `todo` carrying its question.
    Blocked,
    /// Terminal: the attempt completed.
    Succeeded,
    /// Terminal: the attempt failed. [`RunRecord::error`] carries why.
    Failed,
    /// Terminal: the attempt was cancelled before it could settle.
    Cancelled,
    /// Terminal: the workflow compiler **declined by design** to automate the
    /// work — "better done once than built into a workflow" (issue #1809).
    ///
    /// Neither an error nor an ordinary success: the builder was asked whether
    /// the card should become a workflow and its honest answer was "don't". It
    /// used to settle `Failed` — which made a correct refusal indistinguishable
    /// from a model timeout — and then `Succeeded` (#873), which hid it among
    /// completed work. Its own terminal state keeps the external "work that
    /// stopped" surface from bucketing the best thing the compiler does as the
    /// product breaking, without pretending nothing happened.
    Declined,
}

impl RunStatus {
    /// The wire/column literal for this status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Declined => "declined",
        }
    }

    /// Parses a wire/column literal back into a status, or `None`.
    ///
    /// The inverse of [`Self::as_str`], and deliberately in the same `impl`
    /// rather than left to each reader: a REST `?status=` filter that grew its
    /// own literal table would be free to drift from the column the backend
    /// indexes. [`from_wire_round_trips`](self::test::from_wire_round_trips)
    /// pins the two together.
    pub fn from_wire(word: &str) -> Option<Self> {
        Some(match word {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "waiting_approval" => Self::WaitingApproval,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "declined" => Self::Declined,
            _ => return None,
        })
    }

    /// Whether the attempt is over for good. Nothing moves out of a terminal
    /// status — a re-run is a *new* attempt with its own ordinal, never a
    /// resurrection of this one.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Declined
        )
    }

    /// Which of the three coarse phases this status sits in: `active`,
    /// `parked`, or `terminal`.
    ///
    /// A projection of [`Self::is_active`] / [`Self::is_parked`] /
    /// [`Self::is_terminal`], which partition the enum
    /// ([`phases_partition_every_status`](self::test::phases_partition_every_status)
    /// pins that).
    ///
    /// It exists for readers — chiefly the console — that need to know whether
    /// an attempt is live, waiting, or over, and must not answer that from a
    /// timestamp. `finished_at_millis` is `None` for a **parked** run as well as
    /// a live one, so "no finish time" does not mean "still running"; a reader
    /// that guessed from the timestamp would render a run waiting on a human as
    /// though it were burning tokens. Projecting the phase here means the
    /// terminal set is enumerated once, in the module that owns the state
    /// machine, instead of being re-derived by every surface and drifting the
    /// next time a status is added.
    pub fn phase(self) -> &'static str {
        if self.is_terminal() {
            "terminal"
        } else if self.is_parked() {
            "parked"
        } else {
            "active"
        }
    }

    /// Whether the attempt claims to be live in *this* process — the states a
    /// boot reaper reclaims. Deliberately excludes both parked states: parked
    /// is not orphaned.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    /// Whether the attempt is parked awaiting something outside the cycle.
    pub fn is_parked(self) -> bool {
        matches!(self, Self::WaitingApproval | Self::Paused | Self::Blocked)
    }

    /// Whether a run may move from `self` to `next`.
    ///
    /// The table, in one place:
    ///
    /// - `Pending` → `Running` (the cycle started), or straight to a terminal
    ///   (it never got to start — a reaper, or a dispatch that failed before
    ///   the first turn).
    /// - `Running` → any parked or terminal status.
    /// - parked → `Running` (resumed), the *other* parked status, or a
    ///   terminal. Re-entering `WaitingApproval` is a `Running` →
    ///   `WaitingApproval` move, so the resume edge is what makes it repeatable.
    /// - terminal → nothing.
    ///
    /// A no-op move (`s` → `s`) is rejected for the active states, because
    /// `Running` → `Running` would mean a second `begin_run` on a live run and
    /// that is a caller bug worth hearing about. Parked → same-parked *is*
    /// allowed: settling a paused run as paused again (a second steer) is
    /// ordinary.
    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Pending => next == Self::Running || next.is_terminal(),
            Self::Running => next.is_parked() || next.is_terminal(),
            Self::WaitingApproval | Self::Paused | Self::Blocked => {
                next == Self::Running || next.is_parked() || next.is_terminal()
            }
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Declined => false,
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One recorded attempt at a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    /// Stable id for the attempt within the company. Minted by the caller
    /// *before* the cycle spawns.
    pub id: String,
    /// The company that owns the attempt. Carried on the record as well as in
    /// the store key so an exported/streamed row is self-describing.
    pub company: CompanyId,
    /// The board card this attempt is an attempt *at*, when there is one.
    ///
    /// Optional since issue #983: an operator chat turn is a recorded attempt at
    /// *work*, and the work frequently opens no card. Serialized with
    /// `skip_serializing_if` rather than as an explicit `null`, matching how
    /// every other optional field on this record is written, so a dispatch row
    /// is byte-identical to the ones written before the field could be absent —
    /// and `#[serde(default)]` is what lets every row already on disk, which
    /// carries a plain string here, still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The desk/teammate the card was dispatched to.
    pub agent_id: String,
    /// The conversation this attempt belongs to, when one raised it
    /// (issue #983) — the only handle a card-less chat turn has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Which attempt at `task_id` this is, **1-based** — the first run of a card
    /// is `Attempt 1`. Assigned by the backend at create time; see
    /// [`RunStore::create_run`] for the concurrency contract.
    pub attempt: u32,
    /// Where the attempt stands.
    pub status: RunStatus,
    /// The [`EventLog`](crate::ports::EventLog) seq of the `TaskDispatched`
    /// event that drove this attempt, once it has been appended.
    ///
    /// `None` while the run is still `Pending` (the row is written before the
    /// event, on purpose) and for any run whose dispatch failed before the
    /// append.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_event_seq: Option<EventSeq>,
    /// Epoch-millis the row was minted.
    /// The workflow run whose node spawned this attempt, when one did.
    ///
    /// Absent for a card dispatch and a chat turn, which is *true* rather than
    /// merely tolerated — those attempts belong to no workflow. Additive in the
    /// same shape `task_id`/`chat_id` took, so a row written before this field
    /// existed loads with `None` and re-serializes byte-identically. There is no
    /// backfill to write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    /// The graph node within that run.
    ///
    /// Falls back to the agent ref on a graph compiled before nodes carried a
    /// first-class id — the same honest fallback the blocked-node rows use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub created_at_millis: u64,
    /// Epoch-millis the cycle actually began. `None` while `Pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_millis: Option<u64>,
    /// Epoch-millis the attempt reached a **terminal** status.
    ///
    /// Stays `None` for a parked run: `Paused` and `WaitingApproval` can still
    /// resume, so stamping a finish time there would make a resumable attempt
    /// read as over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_millis: Option<u64>,
    /// Why the attempt failed, in plain language. `None` unless the settle
    /// carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Tokens and cost attributed to this attempt, folded across its turns.
    #[serde(default)]
    pub usage: TokenUsage,
    /// How many [`RunStepRecord`]s the attempt produced.
    #[serde(default)]
    pub step_count: u32,
}

impl RunRecord {
    /// Whether this attempt claims to be live in the current process.
    pub fn is_active(&self) -> bool {
        self.status.is_active()
    }
}

/// One step of an attempt's trace, in the order it happened.
///
/// The payload is a [`TurnStep`] rather than a bespoke shape so this stream
/// inherits the harness's scrubbing guarantees for free: labels are
/// server-computed, details are whitelisted, and raw tool arguments/output can
/// never reach here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStepRecord {
    /// The attempt this step belongs to.
    pub run_id: String,
    /// Position within the attempt: 0-based and dense.
    ///
    /// Deliberately **not** an [`EventSeq`]. An event seq is company-wide and
    /// shared with chat and audit; a step ordinal is run-scoped, so keeping
    /// them the same type would invite reading one as the other. It is also
    /// what makes an append idempotent — a backend keys on
    /// `(run_id, step_seq)`, so replaying a step overwrites rather than
    /// duplicates.
    pub step_seq: u32,
    /// Epoch-millis the step was recorded.
    pub at_millis: u64,
    /// The scrubbed step itself.
    pub step: TurnStep,
}

/// What [`RunStore::create_run`] needs to mint a row. Everything else on a
/// [`RunRecord`] is either derived (`attempt`, `created_at_millis`) or arrives
/// through a later transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRun {
    /// The caller-minted run id.
    pub id: String,
    /// The card being attempted, when there is one.
    ///
    /// `None` for a run that attempts no card — an operator chat turn
    /// (issue #983) frequently opens none, and inventing a synthetic card id to
    /// keep this required would put a card on the board that nobody asked for
    /// and that every `RunStore` consumer would then have to know was a
    /// fiction. A card-less run still carries a status, a step trace, a cost and
    /// an orphan reaper, which is the whole of what this store is for.
    pub task_id: Option<String>,
    /// The desk it was dispatched to.
    pub agent_id: String,
    /// The conversation the run belongs to, when one raised it (issue #983).
    ///
    /// What makes a card-less chat turn *findable*: without a `task_id` there is
    /// no other handle on it, so a desk asking "what is running for me" would
    /// have nothing to filter on. `None` for a dispatch, which is already
    /// reachable through its card.
    pub chat_id: Option<String>,
    /// The workflow run whose node spawned this attempt, when one did.
    ///
    /// Absent for a card dispatch and a chat turn, which is *true* rather than
    /// merely tolerated — those attempts belong to no workflow. Additive in the
    /// same shape `task_id`/`chat_id` took, so a row written before this field
    /// existed loads with `None` and re-serializes byte-identically. There is no
    /// backfill to write.
    pub workflow_run_id: Option<String>,
    /// The graph node within that run.
    ///
    /// Falls back to the agent ref on a graph compiled before nodes carried a
    /// first-class id — the same honest fallback the blocked-node rows use.
    pub node_id: Option<String>,
}

impl NewRun {
    /// A run that attempts a board card — the dispatch shape (issue #242).
    pub fn for_task(
        id: impl Into<String>,
        task_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            task_id: Some(task_id.into()),
            agent_id: agent_id.into(),
            chat_id: None,
            workflow_run_id: None,
            node_id: None,
        }
    }

    /// A run that attempts no card — the chat-turn shape (issue #983).
    pub fn for_chat(
        id: impl Into<String>,
        chat_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            task_id: None,
            agent_id: agent_id.into(),
            chat_id: Some(chat_id.into()),
            workflow_run_id: None,
            node_id: None,
        }
    }

    /// A run spawned by an `agent` node of a workflow run.
    ///
    /// Attempts no card and belongs to no conversation: a workflow node's turn
    /// has neither, which is exactly why nothing could find these attempts
    /// before — `RunStore` was joinable only by `task_id`/`chat_id`, so the run
    /// a node spawned was addressable by nothing at all.
    pub fn for_workflow_node(
        id: impl Into<String>,
        workflow_run_id: impl Into<String>,
        node_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            task_id: None,
            agent_id: agent_id.into(),
            chat_id: None,
            workflow_run_id: Some(workflow_run_id.into()),
            node_id: Some(node_id.into()),
        }
    }
}

/// The settle of an attempt, as handed to [`RunStore::finish_run`].
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    /// The status to settle into. Must not be [`RunStatus::Pending`] or
    /// [`RunStatus::Running`] — `finish_run` is how a run *stops* advancing,
    /// and the legality check rejects both.
    pub status: RunStatus,
    /// Why, when the settle carried a reason.
    pub error: Option<String>,
    /// Tokens/cost folded across the attempt's turns.
    pub usage: TokenUsage,
    /// How many steps the attempt produced.
    pub step_count: u32,
}

impl RunOutcome {
    /// A settle carrying nothing but a status — the shape a reaper and the
    /// terminality backstop use.
    pub fn new(status: RunStatus) -> Self {
        Self {
            status,
            error: None,
            usage: TokenUsage::default(),
            step_count: 0,
        }
    }

    /// Attaches a failure reason.
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
}

/// Which runs [`RunStore::list_runs`] should return.
///
/// [`Default`] is "every run in the company, newest first".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// Only attempts at this card.
    pub task_id: Option<String>,
    /// Only attempts spawned by this workflow run's nodes.
    ///
    /// The join the console needs: given a workflow run, which agent attempts
    /// did it produce. Before this there was no handle on them at all — a
    /// workflow node's turn has neither a card nor a conversation.
    pub workflow_run_id: Option<String>,
    /// Only attempts dispatched to this desk/teammate.
    ///
    /// The selector behind the console's per-teammate run history (issue
    /// #1573). Deliberately a *store* predicate rather than something a reader
    /// applies after the fact: a desk's attempts are a thin slice of a
    /// long-lived company's history, and filtering a fetched page in the
    /// console would make `limit` mean "the newest N runs in the company, of
    /// which some happen to be this desk's" — which reads as "this teammate has
    /// done nothing" for any desk that has been quiet lately.
    pub agent_id: Option<String>,
    /// Only these statuses. Empty means "any status".
    pub statuses: Vec<RunStatus>,
    /// At most this many rows, applied *after* ordering. `None` is unbounded.
    pub limit: Option<usize>,
}

impl RunFilter {
    /// Narrows to one card's attempts.
    pub fn for_task(task_id: impl Into<String>) -> Self {
        Self {
            task_id: Some(task_id.into()),
            ..Self::default()
        }
    }

    /// Narrows to one desk's attempts, whichever card (or none) they were at.
    pub fn for_agent(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: Some(agent_id.into()),
            ..Self::default()
        }
    }

    /// Narrows to the statuses a boot reaper reclaims
    /// ([`RunStatus::is_active`]).
    pub fn active() -> Self {
        Self {
            statuses: vec![RunStatus::Pending, RunStatus::Running],
            ..Self::default()
        }
    }

    /// Adds a status to the filter.
    pub fn with_status(mut self, status: RunStatus) -> Self {
        self.statuses.push(status);
        self
    }

    /// Caps the result count.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Whether `run` passes the `task_id`/`agent_id`/`statuses` predicates. The `limit` is
    /// **not** applied here — it is an ordering-dependent cap, so backends that
    /// filter in memory apply it after [`sort_newest_first`].
    pub fn matches(&self, run: &RunRecord) -> bool {
        if let Some(task_id) = &self.task_id
            && run.task_id.as_deref() != Some(task_id.as_str())
        {
            return false;
        }
        if let Some(workflow_run_id) = &self.workflow_run_id
            && run.workflow_run_id.as_deref() != Some(workflow_run_id.as_str())
        {
            return false;
        }
        if let Some(agent_id) = &self.agent_id
            && run.agent_id != *agent_id
        {
            return false;
        }
        if !self.statuses.is_empty() && !self.statuses.contains(&run.status) {
            return false;
        }
        true
    }

    /// Narrows to the attempts one workflow run's nodes spawned.
    #[must_use]
    pub fn for_workflow_run(workflow_run_id: impl Into<String>) -> Self {
        Self {
            workflow_run_id: Some(workflow_run_id.into()),
            ..Self::default()
        }
    }
}

/// The canonical [`RunStore::list_runs`] ordering: newest first, by
/// `created_at_millis` descending, ties broken by `attempt` then `id`, both
/// descending.
///
/// Shared by every backend rather than left to each one's natural order,
/// because the conformance suite asserts an exact sequence and two runs minted
/// in the same millisecond are the common case, not the exotic one — a
/// timestamp-only sort would make the suite pass or fail on scheduler luck.
pub fn sort_newest_first(runs: &mut [RunRecord]) {
    runs.sort_by(|a, b| {
        b.created_at_millis
            .cmp(&a.created_at_millis)
            .then_with(|| b.attempt.cmp(&a.attempt))
            .then_with(|| b.id.cmp(&a.id))
    });
}

// ---------------------------------------------------------------------------
// The port
// ---------------------------------------------------------------------------

/// Durable per-company run records and their step traces. Company A's runs MUST
/// be invisible to company B.
///
/// ## What a backend implements, and what it does not
///
/// The required methods are storage verbs only. The three transition
/// methods — [`create_run`](Self::create_run) is the exception and is required,
/// see below — are **provided**: they read, check legality, and write through
/// [`put_run`](Self::put_run). A backend that overrode them would be re-deriving
/// the state machine, which is the one thing this port exists to prevent, so
/// don't.
///
/// [`put_run`](Self::put_run) is the unchecked write the transitions are built
/// from. It is public only because Rust has no way to hide a supertrait method
/// from a `dyn` caller; treat it as the backend seam, not as API. Callers move
/// runs with the transition methods.
#[async_trait]
pub trait RunStore: Send + Sync {
    // -- storage verbs ------------------------------------------------------

    /// Mints a [`RunStatus::Pending`] row and assigns its `attempt` ordinal:
    /// one more than the highest ordinal already recorded for `spec.task_id`,
    /// so the first attempt at a card is `1`.
    ///
    /// **Concurrency contract.** The ordinal is allocated atomically where the
    /// backend can do so cheaply (sqlite in a transaction, MongoDB from its
    /// counters collection) and best-effort where it cannot (the filesystem
    /// backend allocates under its per-path lock, which is process-local). This
    /// is benign in practice because dispatch serialises through an awaited
    /// board write before it ever reaches here, so two attempts at one card do
    /// not race. It is documented rather than papered over: a caller that
    /// invents its own concurrency must not assume ordinals are gap-free or
    /// globally unique under contention.
    ///
    /// Creating a run whose id already exists is an
    /// [`OpenCompanyError::Conflict`] — ids are minted, and a repeat means the
    /// caller lost track of one, not that it wanted to overwrite an attempt.
    async fn create_run(&self, company: &CompanyId, spec: NewRun) -> Result<RunRecord>;

    /// Reads one run by id, or `None`.
    async fn get_run(&self, company: &CompanyId, id: &str) -> Result<Option<RunRecord>>;

    /// Writes a run row verbatim, creating or replacing it.
    ///
    /// The backend seam behind the transition methods — **not** the API. It
    /// performs no legality check at all, so calling it directly is how a run
    /// ends up in a state the rest of the system does not believe in.
    async fn put_run(&self, company: &CompanyId, run: &RunRecord) -> Result<()>;

    /// Lists runs matching `filter`, newest first (see [`sort_newest_first`]).
    async fn list_runs(&self, company: &CompanyId, filter: &RunFilter) -> Result<Vec<RunRecord>>;

    /// Appends (or replaces, on a matching `step_seq`) one step of a run's
    /// trace.
    async fn append_run_step(&self, company: &CompanyId, step: &RunStepRecord) -> Result<()>;

    /// Reads a run's steps in `step_seq` order, oldest first.
    async fn list_run_steps(&self, company: &CompanyId, run_id: &str)
    -> Result<Vec<RunStepRecord>>;

    /// Reads every run's steps in one call, keyed by run id.
    ///
    /// The Observatory index reads the traces of up to `limit` runs at once.
    /// The provided implementation loops the per-run read, which is the right
    /// shape for backends where one read is one indexed query (sqlite, MongoDB).
    /// The filesystem backend overrides this: its per-run read scans and
    /// deserializes the whole company-wide JSONL before filtering one run, so a
    /// loop over N runs would rescan the company's unbounded step history N
    /// times — quadratic in company history on the view operators poll. One
    /// scan, indexed once, keeps the index cost linear.
    async fn list_run_steps_for_runs(
        &self,
        company: &CompanyId,
        run_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<RunStepRecord>>> {
        let mut out = std::collections::HashMap::with_capacity(run_ids.len());
        for id in run_ids {
            out.insert(id.clone(), self.list_run_steps(company, id).await?);
        }
        Ok(out)
    }

    // -- transitions (provided; legality enforced here) ----------------------

    /// `Pending` → `Running`, stamping the driving event's seq and the start
    /// time.
    async fn begin_run(
        &self,
        company: &CompanyId,
        id: &str,
        trigger_event_seq: EventSeq,
    ) -> Result<RunRecord> {
        let mut run = require_run(self, company, id).await?;
        check_transition(&run, RunStatus::Running)?;
        run.status = RunStatus::Running;
        run.trigger_event_seq = Some(trigger_event_seq);
        // A resumed run keeps the moment it first started: `started_at_millis`
        // is when the attempt began, not when its latest leg did.
        run.started_at_millis.get_or_insert_with(now_millis);
        self.put_run(company, &run).await?;
        Ok(run)
    }

    /// `Pending` → `Running` for an attempt that **no journal event drove**.
    ///
    /// A workflow `agent` node is the case this exists for: it is activated by
    /// the engine walking a graph, not by a `TaskDispatched` the journal
    /// recorded, so there is no seq to stamp. `trigger_event_seq` is already
    /// `Option`, so leaving it `None` is the record's own way of saying "nothing
    /// in the journal drove this" — passing a made-up seq (or `0`) would point
    /// every workflow attempt at an unrelated event and quietly corrupt any
    /// reader that follows it.
    async fn begin_run_untriggered(&self, company: &CompanyId, id: &str) -> Result<RunRecord> {
        let mut run = require_run(self, company, id).await?;
        check_transition(&run, RunStatus::Running)?;
        run.status = RunStatus::Running;
        run.started_at_millis.get_or_insert_with(now_millis);
        self.put_run(company, &run).await?;
        Ok(run)
    }

    /// Settles a run into a parked or terminal status, recording its cost,
    /// step count and (on failure) reason.
    ///
    /// Rejects [`RunStatus::Pending`] and [`RunStatus::Running`] outright: this
    /// is the verb that *stops* a run advancing, and `begin_run` is the only
    /// way into `Running`.
    async fn finish_run(
        &self,
        company: &CompanyId,
        id: &str,
        outcome: RunOutcome,
    ) -> Result<RunRecord> {
        if outcome.status.is_active() {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "finish_run cannot settle a run as '{}': use begin_run to start one",
                outcome.status
            )));
        }
        let mut run = require_run(self, company, id).await?;
        check_transition(&run, outcome.status)?;
        run.status = outcome.status;
        run.error = outcome.error;
        run.usage = outcome.usage;
        run.step_count = outcome.step_count;
        // Only a terminal settle is a finish. A parked run can still resume, so
        // stamping it would make a resumable attempt read as over.
        run.finished_at_millis = outcome.status.is_terminal().then(now_millis);
        self.put_run(company, &run).await?;
        Ok(run)
    }

    /// Every run still claiming to be `Pending` or `Running`.
    ///
    /// At process start these are, by construction, dead — see
    /// [`reap_orphaned_runs`].
    async fn list_stale_active(&self, company: &CompanyId) -> Result<Vec<RunRecord>> {
        self.list_runs(company, &RunFilter::active()).await
    }
}

/// Loads a run or reports it missing — the shared prelude of every transition.
async fn require_run<S: RunStore + ?Sized>(
    store: &S,
    company: &CompanyId,
    id: &str,
) -> Result<RunRecord> {
    store
        .get_run(company, id)
        .await?
        .ok_or_else(|| OpenCompanyError::NotFound(format!("no run '{id}'")))
}

/// Rejects an illegal move with a message that names both ends, so a caller
/// reading the log can see which half surprised it.
fn check_transition(run: &RunRecord, next: RunStatus) -> Result<()> {
    if run.status.can_transition_to(next) {
        return Ok(());
    }
    Err(OpenCompanyError::Conflict(format!(
        "run '{}' cannot move from '{}' to '{}'",
        run.id, run.status, next
    )))
}

// ---------------------------------------------------------------------------
// Boot reaper
// ---------------------------------------------------------------------------

/// Fails every run still recorded as `Pending` or `Running` for `company`,
/// returning the records it reclaimed.
///
/// **The records, not a count (issue #337).** Each reaped row names a card that
/// is still sitting in `in_progress` claimed by an attempt that provably no
/// longer exists — so the caller needs the `task_id`s to make the board
/// truthful, not just a number for the log line. Rows that could not be written
/// are omitted, so the returned set is exactly "what was actually settled".
///
/// ## Why an active row at boot is *provably* dead, not merely suspicious
///
/// Three invariants, all verified against this tree:
///
/// 1. A cycle is a process-local `tokio::spawn` (`CompanyRuntime::dispatch_task`
///    and the scheduler), so it cannot outlive the process that spawned it.
/// 2. Exactly one process owns a company at a time — the append-only
///    [`RuntimeJournal`](crate::runtime::journal::RuntimeJournal) is documented
///    single-writer, and a second writer corrupts it.
/// 3. Cycles serialise on the per-company mutex held by
///    [`CycleRunner::run`](crate::runtime::CycleRunner), so no cycle from this
///    process is in flight before boot completes.
///
/// Together those mean any active row observed at boot belongs to an attempt
/// that no longer exists. This is a proof, not a timeout heuristic — nothing
/// here guesses at staleness from a clock.
///
/// ## The proof is about *boot*, and only boot (issue #290)
///
/// Invariant 3 is the load-bearing one and it is the one a
/// [runtime rebuild](crate::runtime::rebuild_company) breaks: a rebuild calls
/// [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build) a second
/// time in a live process, where cycles *have* run and may hold rows that are
/// genuinely in flight. Calling this from there would settle live attempts as
/// dead, which is the precise corruption the sweep exists to clean up — so
/// `build()` suppresses it whenever a
/// [`RuntimeHandover`](crate::runtime::RuntimeHandover) is present. Any future
/// caller owes the same argument: this function reclaims rows it can *prove* are
/// abandoned, and outside boot that proof does not exist.
///
/// **Parked runs are never touched.** `WaitingApproval` and `Paused` are
/// waiting on a person or an external condition, not on a process; reaping them
/// would delete real pending work every time the host restarted.
pub async fn reap_orphaned_runs(
    runs: &dyn RunStore,
    company: &CompanyId,
) -> Result<Vec<RunRecord>> {
    let stale = runs.list_stale_active(company).await?;
    let mut reaped = Vec::new();
    for run in stale {
        let outcome = RunOutcome::new(RunStatus::Failed).with_error(ORPHAN_ERROR);
        match runs.finish_run(company, &run.id, outcome).await {
            Ok(settled) => reaped.push(settled),
            // One unreclaimable row must not stop the rest, and must not fail
            // boot — but it is logged at warn, not swallowed, because a store
            // that cannot write at startup is a real problem.
            Err(err) => tracing::warn!(
                company = %company,
                run = %run.id,
                error = %err,
                "could not reap an orphaned run record"
            ),
        }
    }
    if !reaped.is_empty() {
        tracing::info!(
            company = %company,
            reaped = reaped.len(),
            "failed run records stranded by a previous host process"
        );
    }
    Ok(reaped)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::{TurnStepKind, TurnStepStatus};

    /// Every status, so a table-driven test cannot silently miss a new variant
    /// (adding one without extending this list fails the exhaustiveness check
    /// in [`all_statuses_are_listed`]).
    const ALL: [RunStatus; 9] = [
        RunStatus::Pending,
        RunStatus::Running,
        RunStatus::WaitingApproval,
        RunStatus::Paused,
        RunStatus::Blocked,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Cancelled,
        RunStatus::Declined,
    ];

    #[test]
    fn all_statuses_are_listed() {
        // A `match` with no wildcard: adding a variant breaks compilation here
        // first, which is the reminder to extend `ALL`.
        for status in ALL {
            match status {
                RunStatus::Pending
                | RunStatus::Running
                | RunStatus::WaitingApproval
                | RunStatus::Paused
                | RunStatus::Blocked
                | RunStatus::Succeeded
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::Declined => (),
            };
        }
        let mut seen: Vec<&str> = ALL.iter().map(|s| s.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ALL.len(), "status literals must be unique");
    }

    /// Issue #1861: a blocker waits on a person, so it is **parked** — not
    /// terminal, and not active. A terminal blocker could never be answered;
    /// an active one would be reclaimed by the boot reaper as an orphan while
    /// somebody was still deciding what to say.
    #[test]
    fn a_blocker_is_parked_not_terminal() {
        assert!(RunStatus::Blocked.is_parked());
        assert!(!RunStatus::Blocked.is_terminal());
        assert!(!RunStatus::Blocked.is_active());
        assert_eq!(RunStatus::Blocked.phase(), "parked");
    }

    /// The answer resumes the work, and an unanswered blocker settles through
    /// the TTL — so both edges out of `Blocked` have to exist.
    #[test]
    fn a_blocker_can_resume_or_settle() {
        assert!(RunStatus::Blocked.can_transition_to(RunStatus::Running));
        assert!(RunStatus::Blocked.can_transition_to(RunStatus::Failed));
        assert!(RunStatus::Running.can_transition_to(RunStatus::Blocked));
        assert!(
            !RunStatus::Succeeded.can_transition_to(RunStatus::Blocked),
            "a finished attempt cannot start waiting on somebody"
        );
    }

    #[test]
    fn status_literals_match_their_serde_form() {
        for status in ALL {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", status.as_str()),
                "the indexed column literal and the wire form must not drift"
            );
        }
    }

    /// The `?status=` filter parses the exact literal the backend indexes —
    /// pinned in both directions so neither table can drift alone.
    #[test]
    fn from_wire_round_trips() {
        for status in ALL {
            assert_eq!(
                RunStatus::from_wire(status.as_str()),
                Some(status),
                "{status} does not parse back from its own literal"
            );
        }
        assert_eq!(RunStatus::from_wire("Succeeded"), None, "case is exact");
        assert_eq!(RunStatus::from_wire("waitingApproval"), None);
        assert_eq!(RunStatus::from_wire(""), None);
    }

    /// `phase()` is only sound if the three predicates partition the enum:
    /// exactly one must hold for every status, or a run would report a phase
    /// that contradicts one of them.
    #[test]
    fn phases_partition_every_status() {
        for status in ALL {
            let held = [status.is_active(), status.is_parked(), status.is_terminal()]
                .into_iter()
                .filter(|b| *b)
                .count();
            assert_eq!(held, 1, "{status} is in {held} phases, not exactly one");
        }
        assert_eq!(RunStatus::Pending.phase(), "active");
        assert_eq!(RunStatus::Running.phase(), "active");
        assert_eq!(RunStatus::WaitingApproval.phase(), "parked");
        assert_eq!(RunStatus::Paused.phase(), "parked");
        assert_eq!(RunStatus::Succeeded.phase(), "terminal");
        assert_eq!(RunStatus::Failed.phase(), "terminal");
        assert_eq!(RunStatus::Cancelled.phase(), "terminal");
        assert_eq!(RunStatus::Declined.phase(), "terminal");
    }

    /// The trap this whole projection exists for: a parked run has **no**
    /// finish time, exactly like a live one, so a reader that inferred
    /// liveness from the timestamp would call a run waiting on a person "still
    /// running" forever.
    #[test]
    fn a_parked_run_has_no_finish_time_and_is_not_active() {
        for parked in [RunStatus::WaitingApproval, RunStatus::Paused] {
            let mut record = run("r1", 10, 1);
            record.status = RunStatus::Running;
            record.started_at_millis = Some(11);
            // What `finish_run` would stamp: only a terminal settle is a finish.
            record.status = parked;
            record.finished_at_millis = parked.is_terminal().then_some(12);
            assert_eq!(record.finished_at_millis, None);
            assert!(!record.is_active(), "{parked} must not read as live");
            assert_eq!(record.status.phase(), "parked");
        }
    }

    #[test]
    fn terminal_statuses_are_final() {
        for from in ALL.into_iter().filter(|s| s.is_terminal()) {
            for to in ALL {
                assert!(
                    !from.can_transition_to(to),
                    "{from} is terminal but claims it can move to {to}"
                );
            }
        }
    }

    #[test]
    fn pending_only_starts_or_dies() {
        assert!(RunStatus::Pending.can_transition_to(RunStatus::Running));
        assert!(RunStatus::Pending.can_transition_to(RunStatus::Failed));
        assert!(RunStatus::Pending.can_transition_to(RunStatus::Cancelled));
        // A run that never began cannot have succeeded... but it also cannot
        // park: nothing has run to hit an approval or a rate limit.
        assert!(!RunStatus::Pending.can_transition_to(RunStatus::WaitingApproval));
        assert!(!RunStatus::Pending.can_transition_to(RunStatus::Paused));
        assert!(!RunStatus::Pending.can_transition_to(RunStatus::Pending));
    }

    #[test]
    fn running_cannot_restart() {
        assert!(!RunStatus::Running.can_transition_to(RunStatus::Running));
        assert!(!RunStatus::Running.can_transition_to(RunStatus::Pending));
    }

    /// Epic #183 decision 3: an attempt may enter review many times, because
    /// #243 grants are single-use and argument-exact.
    #[test]
    fn waiting_approval_is_re_enterable() {
        assert!(RunStatus::Running.can_transition_to(RunStatus::WaitingApproval));
        assert!(RunStatus::WaitingApproval.can_transition_to(RunStatus::Running));
        // …and round again.
        assert!(RunStatus::Running.can_transition_to(RunStatus::WaitingApproval));
    }

    /// Epic #183 decision 2: who unblocks it decides where it parks, so the two
    /// parked states are distinct and both are non-terminal and resumable.
    #[test]
    fn both_parked_states_resume() {
        for parked in [RunStatus::WaitingApproval, RunStatus::Paused] {
            assert!(parked.is_parked());
            assert!(!parked.is_terminal());
            assert!(!parked.is_active());
            assert!(parked.can_transition_to(RunStatus::Running));
            assert!(parked.can_transition_to(RunStatus::Succeeded));
        }
        // A run waiting on a person can turn into one waiting on a dependency
        // (and back) without a terminal hop in between.
        assert!(RunStatus::WaitingApproval.can_transition_to(RunStatus::Paused));
        assert!(RunStatus::Paused.can_transition_to(RunStatus::WaitingApproval));
    }

    #[test]
    fn only_pending_and_running_are_reapable() {
        for status in ALL {
            assert_eq!(
                status.is_active(),
                matches!(status, RunStatus::Pending | RunStatus::Running),
                "{status} is misclassified for the boot reaper"
            );
        }
    }

    fn run(id: &str, created: u64, attempt: u32) -> RunRecord {
        RunRecord {
            id: id.to_string(),
            company: CompanyId::new("alpha"),
            task_id: Some("card".to_string()),
            chat_id: None,
            agent_id: "ceo".to_string(),
            attempt,
            status: RunStatus::Pending,
            trigger_event_seq: None,
            created_at_millis: created,
            started_at_millis: None,
            finished_at_millis: None,
            error: None,
            usage: TokenUsage::default(),
            step_count: 0,
            workflow_run_id: None,
            node_id: None,
        }
    }

    #[test]
    fn ordering_breaks_ties_deterministically() {
        // Same millisecond — the common case, not the exotic one.
        let mut runs = vec![run("a", 10, 1), run("c", 10, 3), run("b", 10, 2)];
        sort_newest_first(&mut runs);
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["c", "b", "a"], "newest attempt first within a tick");

        // A later timestamp outranks a higher ordinal.
        let mut runs = vec![run("old", 10, 9), run("new", 20, 1)];
        sort_newest_first(&mut runs);
        assert_eq!(runs[0].id, "new");
    }

    #[test]
    fn filter_matches_task_and_status() {
        let mut pending = run("a", 10, 1);
        let mut done = run("b", 10, 2);
        done.status = RunStatus::Succeeded;
        done.task_id = Some("other".to_string());

        assert!(RunFilter::default().matches(&pending));
        assert!(RunFilter::default().matches(&done));

        assert!(RunFilter::for_task("card").matches(&pending));
        assert!(!RunFilter::for_task("card").matches(&done));

        assert!(RunFilter::active().matches(&pending));
        assert!(!RunFilter::active().matches(&done));

        pending.status = RunStatus::Running;
        assert!(RunFilter::active().matches(&pending));

        let only_succeeded = RunFilter::default().with_status(RunStatus::Succeeded);
        assert!(only_succeeded.matches(&done));
        assert!(!only_succeeded.matches(&pending));
    }

    #[test]
    fn step_records_round_trip() {
        let record = RunStepRecord {
            run_id: "r1".to_string(),
            step_seq: 0,
            at_millis: 42,
            step: TurnStep {
                kind: TurnStepKind::ToolCall,
                status: TurnStepStatus::Ok,
                label: "Reading messages".to_string(),
                detail: None,
                elapsed_ms: Some(12),
                ..TurnStep::default()
            },
        };
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(record, serde_json::from_str(&json).unwrap());
        // The wire form is camelCase, like every other console-facing shape.
        assert!(json.contains("\"runId\""), "{json}");
        assert!(json.contains("\"stepSeq\""), "{json}");
    }

    #[test]
    fn run_records_round_trip_and_omit_empty_optionals() {
        let mut record = run("r1", 100, 1);
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(record, serde_json::from_str(&json).unwrap());
        assert!(!json.contains("triggerEventSeq"), "{json}");
        assert!(!json.contains("finishedAtMillis"), "{json}");

        record.status = RunStatus::Failed;
        record.trigger_event_seq = Some(EventSeq::new(7));
        record.finished_at_millis = Some(200);
        record.error = Some(ORPHAN_ERROR.to_string());
        record.usage = TokenUsage {
            input: 10,
            output: 5,
            cached_input: 1,
            cost_usd: 0.25,
        };
        record.step_count = 3;
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(record, serde_json::from_str(&json).unwrap());
        assert!(json.contains("\"status\":\"failed\""), "{json}");
    }
}
