//! The runtime journal: durable at-most-once effect execution and the
//! persistent approval queue.
//!
//! The journal is distinct from the [`EventLog`](crate::ports::EventLog).
//! [`CompanyEvent`](crate::ports::CompanyEvent) is a closed, binding enum with
//! no marker variants, so effect-execution and approval-parking markers cannot
//! ride the event log. They live here instead, in a per-company `journal.jsonl`
//! that boot replay reads back to rebuild in-flight state.
//!
//! Two guarantees:
//!
//! * **At-most-once effects.** Before a side effect runs, its idempotency key is
//!   committed to the journal. On recovery the committed key is skipped, so a
//!   crash after the commit but before the side effect drops the effect (at
//!   most once) rather than repeating it.
//! * **Durable approvals.** Parked effects are journaled and rehydrated on boot,
//!   so an approval survives a restart with its original [`ApprovalId`].
//!
//! Both guarantees are only as durable as what the records are written to, which
//! is why the sink is a port ([`JournalStore`], issue #726) rather than a file
//! path. On a hosted mongodb tenant the container's `/data` is ephemeral scratch,
//! so a journal pinned to the filesystem there lost every committed key and every
//! parked approval on container replacement. Everything semantic — the record
//! enum, replay, corrupt-line recovery — lives here and is backend-agnostic; the
//! store below it only keeps opaque lines in order.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::ports::journal::{Durability, JournalStore};
use crate::ports::types::{Actor, ApprovalId, CompanyId, Effect, EventSeq, StartedBy};
use crate::runtime::grants::{ApprovalContinuation, GrantId, GrantedCall, StandingGrant};
pub use crate::runtime::types::TaskLink;
use crate::store::fs::FsJournalStore;

/// Why a parked approval was retired without an operator deciding it
/// (issue #971).
///
/// Retirement has one implementation — [`CompanyRuntime::retire_approval`] —
/// and this says which rule invoked it. Recorded rather than inferred: the
/// journal is the audit trail for a default-deny, and "the deadline passed" and
/// any future automatic retirement are different things to have happened to
/// someone's request, however identical the resulting queue looks.
///
/// The enum exists rather than a bool because the reasons keep arriving — the
/// next one already known is an approval retired because a newer identical
/// request superseded it — and a `superseded: bool` beside a `reason` would be
/// two fields describing one fact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryReason {
    /// It sat unresolved past its `[policy].approval_ttl_hours` deadline.
    #[default]
    Ttl,
    /// The card it was parked for could not be written, so the blocker was
    /// withdrawn rather than left pointing at a card nobody paused
    /// (issue #1861).
    ///
    /// A blocker and its card are two writes to two stores, and the planning
    /// pass parks first so the queue can never promise a release for a column
    /// that never changed. That leaves the mirror-image gap when the second
    /// write fails: a live, journaled blocker against a card still sitting in
    /// Planning — which the TTL sweep cannot repair either, because
    /// `return_expired_blocker_card` only moves cards already in `paused`. This
    /// is the compensating retirement, recorded under its own name because "we
    /// could not write the card" and "the deadline passed" are different things
    /// to have happened to an operator's queue.
    CardUnwritable,
}

/// The pre-#1862 fallback for a [`JournalRecord::BlockedNodeStashed`] line
/// written before that record carried `started_by` at all — never a live
/// choice, only what a legacy row's `#[serde(default)]` decodes to.
fn default_started_by_operator() -> StartedBy {
    StartedBy::Operator
}

/// One durable journal record.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "record")]
enum JournalRecord {
    /// A side effect committed to run under this idempotency key.
    EffectExecuted {
        /// The effect's idempotency key.
        key: String,
        /// What the key committed (issue #351).
        ///
        /// The key alone answers "has this run?" and nothing else, which is all
        /// the at-most-once guarantee needs and not nearly enough to tell an
        /// operator what a previous attempt already did. Absent on records
        /// written before #351 — those replay as an executed key with no
        /// description, exactly as they behaved before, and set
        /// [`State::undescribed_executed`] so the console can say so instead of
        /// implying the gap is an all-clear.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<ExecutedEffect>,
    },
    /// An effect parked for operator approval.
    ApprovalParked {
        /// The parked approval's id.
        id: ApprovalId,
        /// The parked effect.
        effect: Effect,
        /// Epoch-millis the effect was parked.
        at_millis: u64,
        /// Which board task this effect was parked for (issue #333).
        ///
        /// This is the correlation key that makes a task's Approvals tab
        /// possible. Before it, an approval carried nothing tying it to a card,
        /// so the only join available was "did this resolve while that task was
        /// running" — a time window, which a second task worked in the same
        /// window silently absorbs.
        ///
        /// **Always written from #333 onward**, as either
        /// [`TaskLink::Task`] or [`TaskLink::Unlinked`] — never omitted. That is
        /// the whole point of the enum over a bare `Option<String>`: "parked for
        /// no card" and "parked by a host that did not record cards" are
        /// different facts, and only the second may fall back to the run window.
        /// Collapsing them sent every workflow delivery, chat turn and scheduler
        /// tick to whatever card happened to be running.
        ///
        /// `None` therefore means exactly one thing: a journal line written
        /// before this field existed. `#[serde(default)]` is what lets those
        /// replay instead of failing to parse.
        #[serde(default)]
        task: Option<TaskLink>,
        /// Which **chat thread** produced the parking cycle (issue #379) — the
        /// desk id for a channel, the roster agent id for a direct message.
        ///
        /// The correlation key that lets an approval be raised in the
        /// conversation that asked for it. [`Effect::agent`] cannot do that job:
        /// a desk channel and a direct message to that desk's lead resolve to
        /// the same agent id, so placing the card by asker would raise a
        /// channel's request inside the lead's private DM.
        ///
        /// A plain `Option<String>` rather than a [`TaskLink`]-style enum,
        /// because nothing downstream falls back to a heuristic when it is
        /// absent: an approval with no thread matches no channel filter and
        /// stays Approvals-page-only, which is exactly today's behaviour. So
        /// "parked by a host that did not record threads" and "parked by a turn
        /// with no conversation behind it" need not be told apart — both mean
        /// "no channel owns this", and both are correct.
        ///
        /// `#[serde(default)]` is what lets a pre-#379 line replay.
        #[serde(default)]
        thread: Option<String>,
        /// Which **thread within that channel** produced the parking cycle
        /// (issue #435) — the root the raising message hangs off, as that
        /// root's own [`EventSeq`].
        ///
        /// A separate field rather than a widening of `thread`, because the two
        /// answer different questions and both are needed: `thread` says which
        /// channel, this says where inside it. Overloading `thread` would have
        /// silently changed the meaning of every existing reader of it — see
        /// [`ApprovalOrigin::parent`] for the whole argument.
        ///
        /// `None` for a park raised straight into a channel rather than inside
        /// a thread, which is also every line written before this field
        /// existed. Both mean the same thing downstream and correctly so: the
        /// channel is the answer, which is exactly the pre-#435 behaviour.
        ///
        /// `#[serde(default)]` is what lets a pre-#435 line replay.
        #[serde(default)]
        parent: Option<EventSeq>,
        /// Which **cycle** parked it (issue #469) — the turn key.
        ///
        /// The three keys above all answer "what is this approval about". This
        /// one answers "what is waiting on it", and only it can: a single turn
        /// can park several calls, and each of the others is either shared by
        /// turns that are not blocked on each other (a thread hosts many turns)
        /// or absent for the case that matters most (a chat turn has no card and
        /// no run).
        ///
        /// Without it, resolving four sign-offs from one turn re-ran that turn
        /// four times, because nothing could say the four belonged together.
        /// With it, the runtime holds the continuation until the last of a
        /// turn's approvals is decided and then runs it once — see
        /// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue).
        ///
        /// `None` means a line written before this field existed, and falls back
        /// to the pre-#469 behaviour of continuing that approval on its own.
        /// `#[serde(default)]` is what lets those lines replay.
        #[serde(default)]
        cycle: Option<String>,
    },
    /// A parked approval that has since been resolved (approved or denied).
    ApprovalResolved {
        /// The resolved approval's id.
        id: ApprovalId,
    },
    /// A parked approval that expired to a default-deny with no operator action.
    ApprovalExpired {
        /// The expired approval's id.
        id: ApprovalId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
        /// Why it was retired (issue #971).
        ///
        /// `#[serde(default)]` is what lets every line written before this
        /// field existed replay: they were all TTL expiries, which is exactly
        /// what [`ExpiryReason::Ttl`] means, so the default is the truth about
        /// them rather than a placeholder.
        #[serde(default)]
        reason: ExpiryReason,
    },
    /// An operator pushed a parked approval's deadline out to a fresh full TTL
    /// window (issue #1805).
    ///
    /// The durable half of the extend lever. It carries the new anchor rather
    /// than an offset, because the anchor is exactly what the sweeper and the
    /// projected deadline both read (`ParkedApproval::deadline_anchor_millis`),
    /// so replay re-applies the move by rehydrating the gate from it — an
    /// extension survives a redeploy instead of reverting to the original park
    /// instant. Deliberately separate from `ApprovalParked::at_millis`, which
    /// dates the PAYLOAD (issue #1024) and must not shift when a deadline does.
    ApprovalExtended {
        /// The extended approval's id.
        id: ApprovalId,
        /// Epoch-millis the TTL window was re-anchored to (the extension time).
        at_millis: u64,
        /// Who extended it.
        by: Actor,
    },
    /// A parked approval the operator approved with an amended effect payload.
    ///
    /// Audit-only: the queue removal is recorded by the paired
    /// [`ApprovalResolved`](JournalRecord::ApprovalResolved). The original
    /// effect stays recoverable from the earlier
    /// [`ApprovalParked`](JournalRecord::ApprovalParked), so the immutable log
    /// shows both what was requested and what the operator approved.
    ApprovalAmended {
        /// The amended approval's id.
        id: ApprovalId,
        /// The operator-amended effect that was executed.
        amended_effect: Effect,
        /// Epoch-millis the amendment was recorded.
        at_millis: u64,
    },
    /// A single-use grant minted because the operator approved a tool call an
    /// agent had been blocked from making (issue #243).
    ///
    /// This is the durable audit line for "the operator said yes to *this*
    /// call": it carries the agent, the tool, and the exact arguments admitted,
    /// which is more than the event log's
    /// [`ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved) can
    /// hold. Written *before* the grant reaches the live set, so a crash between
    /// the two re-arms it on replay rather than losing the operator's decision.
    ApprovalGranted {
        /// The grant, whole.
        grant: GrantedCall,
    },
    /// A follow-up turn owed after an agent explicitly asked the operator a
    /// question. Unlike `ApprovalGranted`, this carries either verdict and
    /// conveys no authority to execute a tool call.
    ApprovalContinuationQueued {
        /// The verdict and routing context, whole.
        continuation: ApprovalContinuation,
    },
    /// An explicit decision follow-up is committed to one dispatch attempt.
    /// Written before the agent turn starts so recovery never repeats external
    /// actions from a continuation that may already have partially run.
    ApprovalContinuationDispatched {
        /// The approval whose follow-up was claimed.
        id: ApprovalId,
        /// Epoch-millis the dispatch was committed.
        at_millis: u64,
    },
    /// An explicit approval continuation was delivered to its requesting agent.
    ApprovalContinuationConsumed {
        /// The approval whose follow-up completed.
        id: ApprovalId,
    },
    /// An explicit approval continuation expired before it could be delivered.
    ApprovalContinuationExpired {
        /// The approval whose follow-up expired.
        id: ApprovalId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
    },
    /// A grant redeemed by its agent — the tool ran.
    GrantConsumed {
        /// The consumed grant's approval id.
        id: ApprovalId,
        /// What the redeemed grant actually did (issue #351).
        ///
        /// An approved *agent tool call* never reaches
        /// [`EffectExecuted`](Self::EffectExecuted): it is settled by minting a
        /// grant, and the tool then runs inside the agent's next turn. This
        /// record is therefore the only line in the journal that means "an
        /// operator-approved `composio_execute` payment fired", and without a
        /// description on it the retry dialog would open naming the native
        /// email beside it and nothing else — a confirmation understating what
        /// already happened.
        ///
        /// Absent on records written before this field existed; those replay as
        /// a consumed grant with no description, the same additive contract
        /// [`EffectExecuted`](Self::EffectExecuted) has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<ExecutedEffect>,
    },
    /// A grant that expired unredeemed past [`GRANT_TTL_MILLIS`](crate::runtime::grants::GRANT_TTL_MILLIS).
    GrantExpired {
        /// The expired grant's approval id.
        id: ApprovalId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
    },
    /// A **standing** grant minted because the operator chose the broader scope
    /// on an approval: this tool, for this teammate, until a deadline (#374).
    ///
    /// Carries the grant whole, like
    /// [`ApprovalGranted`](Self::ApprovalGranted), because this line is the only
    /// durable answer to "who opened this tool up, when, off which card, and
    /// until when". `StandingGrant::granted_by` is the operator's real identity,
    /// not the placeholder the resolve route used to hardcode.
    ///
    /// Written *before* the grant reaches the live set, the same crash direction
    /// `ApprovalGranted` takes.
    StandingGrantMinted {
        /// The standing grant, whole.
        grant: StandingGrant,
    },
    /// A standing grant the operator took back (#374).
    ///
    /// Takes effect on the **next** policy check — an already-admitted call is
    /// not aborted, because there is no abort lever inside an agent's turn and
    /// killing one mid-call is the lifecycle anti-pattern this codebase avoids
    /// elsewhere. The next check finds nothing and re-parks.
    StandingGrantRevoked {
        /// The revoked grant's id.
        id: GrantId,
        /// Who revoked it.
        by: Actor,
        /// Epoch-millis the revocation was recorded.
        at_millis: u64,
    },
    /// A standing grant that reached its deadline (#374).
    StandingGrantExpired {
        /// The expired grant's id.
        id: GrantId,
        /// Epoch-millis the expiry was recorded.
        at_millis: u64,
    },
    /// A cycle began (issue #390).
    ///
    /// Written **before the per-company serial lock is taken**, which is the
    /// whole point of the record — see
    /// [`open_cycles`](RuntimeJournal::open_cycles) for why after the lock would
    /// miss the case this exists for.
    CycleStarted {
        /// The cycle's id — the same `cycle_id`
        /// [`ApprovalParked::cycle`](JournalRecord::ApprovalParked) already
        /// correlates approvals on. No second identifier is introduced, for the
        /// reason `run_supervisor` gives for reusing `run_id`.
        cycle_id: String,
        /// Epoch-millis the cycle started.
        at_millis: u64,
        /// A short, stable label for what kicked the cycle off, so an operator
        /// reading an open bracket can tell a stuck approval continuation from
        /// a stuck chat turn without joining anything.
        trigger: String,
    },
    /// A cycle ended, for any reason (issue #390).
    CycleFinished {
        /// The cycle this closes.
        cycle_id: String,
        /// Epoch-millis the cycle ended.
        at_millis: u64,
        /// `None` when the cycle completed; the failure otherwise.
        ///
        /// A cycle that returned `Err` and one the host never finished are both
        /// failures an operator may need to retry, but they are different facts
        /// and the read side must not merge them — a boot sweep writes
        /// [`INTERRUPTED_BY_HOST_RESTART`] here, and a real failure writes what
        /// actually went wrong.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The two facts a blocked agent node's continuation needs — its workflow id
    /// and the paused run's trigger input — stashed durably at park time so an
    /// approval can re-dispatch the run **after a restart** (issue #1816,
    /// Stage 2).
    ///
    /// The runtime's in-memory
    /// [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue) is
    /// the fast path; this record is what the builder re-arms it from at boot
    /// (see [`blocked_stashes`](RuntimeJournal::blocked_stashes)). Written from
    /// the one place — the runner's block-settle — that holds the workflow id,
    /// the trigger input and the blocked-node list together, exactly where the
    /// in-memory stash is armed. The parked tool-call effect itself carries no
    /// workflow lineage, which is why this is a dedicated record rather than a
    /// widening of [`ApprovalParked`](Self::ApprovalParked).
    BlockedNodeStashed {
        /// The per-(run, node) turn key its parked calls also armed the
        /// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue)
        /// under, so a released batch and this stash name the same block.
        turn: String,
        /// The workflow whose run blocked, to load the graph for the re-run.
        workflow_id: String,
        /// The paused run's own trigger input, replayed unchanged — the grant the
        /// approve minted is what lets the identical gated call pass on the re-run.
        input: Value,
        /// The blocked run's own attribution (issue #1862 prerequisite), carried
        /// so a restart between park and approve rehydrates the real trigger
        /// instead of degrading every stash to [`StartedBy::Operator`] — see
        /// [`BlockedNodeQueue::rearm`](crate::runtime::blocked_nodes::BlockedNodeQueue::rearm).
        /// `#[serde(default)]` so a record written before this field existed
        /// still replays: it degrades to `Operator`, the same fallback the
        /// pre-#1862 code path always used.
        #[serde(default = "default_started_by_operator")]
        started_by: StartedBy,
        /// Epoch-millis the block was stashed.
        at_millis: u64,
    },
    /// A blocked-node stash whose run has been re-dispatched (or whose block was
    /// wholly refused): the paired terminator for
    /// [`BlockedNodeStashed`](Self::BlockedNodeStashed), so a resolved block does
    /// not rehydrate a duplicate continuation on the next boot (issue #1816).
    BlockedNodeReleased {
        /// The turn key whose stash this drops.
        turn: String,
    },
    /// At least one of a blocked agent node's parked calls has been approved,
    /// banked durably the moment that decision lands (issue #1816).
    ///
    /// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue)'s
    /// batch only carries the verdicts a single process happened to hold in
    /// memory when the turn's last decision released it — a restart between
    /// two decisions on the same node drops the earlier ones from that batch
    /// exactly as [`WorkflowGateQueue`](crate::runtime::workflow_gates::WorkflowGateQueue)'s
    /// own docs describe. That queue can afford the loss: the workflow graph
    /// replay re-parks whatever the batch forgot, so the operator is asked
    /// again rather than never. A blocked agent node has no such re-park —
    /// [`resume_blocked_agent_node`](crate::company::runtime::CompanyRuntime::resume_blocked_agent_node)
    /// either spawns the continuation or does not, once — so losing an earlier
    /// approval from the batch would silently strand a grant the operator
    /// already minted: an approved tool call that runs to redeem it never
    /// executes and nothing re-asks. This record is the fact the batch cannot
    /// carry, kept durable on its own so a restart mid-decision cannot erase
    /// it. Written from the same place [`ContinuationQueue::decide`] is told
    /// about the verdict, not deferred to release time, so it survives a
    /// restart that lands on any decision but the last.
    BlockedNodeApproved {
        /// The turn key whose node had at least one call approved.
        turn: String,
    },
    /// A blocked agent node's continuation is about to be launched (issue
    /// #1825), written by
    /// [`spawn_blocked_node_continuation`](crate::runtime::workflow_resume::spawn_blocked_node_continuation)
    /// itself, immediately after the run is admitted
    /// ([`RunSupervisor::begin`](crate::runtime::RunSupervisor::begin)) and
    /// immediately before its detached task is actually launched — and,
    /// either way, well before
    /// [`resume_blocked_agent_node`](crate::company::runtime::CompanyRuntime::resume_blocked_agent_node)
    /// retires the stash via [`BlockedNodeReleased`](Self::BlockedNodeReleased).
    ///
    /// That retirement is the pair of facts (`blocked_stashes` and
    /// `blocked_node_approvals`) that would otherwise tell a restart "this
    /// stash is ready to dispatch" — exactly as true after a real dispatch
    /// whose release-write failed as it is before any dispatch at all. Without
    /// a marker recorded on the success side of admission,
    /// `reconcile_stranded_blocked_nodes` cannot distinguish the two and would
    /// re-spawn a continuation that already ran, potentially repeating token
    /// spend or unprotected upstream work a second time. This record is that
    /// marker.
    ///
    /// **Ordering, precisely.** The write sits between admission and launch
    /// rather than after the whole spawn call returns (as the first cut of
    /// this fix had it) because the launched task is detached — its caller
    /// never awaits it — so a marker written only after the *call* returns
    /// races the entire run, however long it takes, not a moment's gap. Between
    /// admission and launch there is no further `.await`, so the crash window
    /// this leaves is the width of this write's own append landing, nothing
    /// more: a crash there can still leave the two out of sync (a marker with
    /// nothing yet launched to justify it, which strands the turn rather than
    /// duplicating it — the opposite, and cheaper, failure), but a crash
    /// after the write cannot land inside a run this record does not already
    /// know about.
    BlockedNodeDispatched {
        /// The turn key whose node's continuation has been spawned.
        turn: String,
    },
}

impl JournalRecord {
    /// Which failure this record must survive.
    ///
    /// Host durability is bought for exactly the records whose loss would make
    /// the runtime **repeat an external action**, and for nothing else. The
    /// frequency asymmetry is the whole argument, and it runs the helpful way:
    /// the dangerous records are rare and the frequent records are harmless.
    /// [`EffectExecuted`](Self::EffectExecuted) is written at human-approval
    /// scale, immediately in front of a network call that costs 100ms-2s, so a
    /// flush ahead of it is invisible; [`CycleStarted`](Self::CycleStarted) is
    /// written on the front edge of *every* cycle, before the per-company serial
    /// lock, and losing it costs an observability bracket. A blanket flush would
    /// tax the hottest cosmetic record in the journal to protect the rarest
    /// dangerous one.
    ///
    /// The match is **wildcard-free on purpose, and must stay that way**: a new
    /// record kind is a compile error until its author has decided which failure
    /// it must survive. That decision — not the two flushes — is what #392
    /// delivers. Same reasoning as [`TaskLink`] being an enum rather than a bare
    /// `Option`: the type refuses to let a decision be skipped by default.
    fn durability(&self) -> Durability {
        match self {
            // Written *before* the side effect runs (`execute_effect_once`), so
            // losing it makes the next boot re-fire the effect mechanically —
            // the single duplication this journal exists to prevent.
            Self::EffectExecuted { .. } => Durability::Host,
            // Losing it silently un-revokes: the grant replays live on the next
            // boot and keeps admitting calls until its own deadline, undoing an
            // operator's withdrawal of authority. An operator action, so rare
            // enough that the flush costs nothing measurable.
            Self::StandingGrantRevoked { .. } => Durability::Host,
            // Losing it re-arms a grant whose tool already ran. Replay keeps the
            // `ApprovalGranted` that minted it and drops the redemption, so the
            // grant returns to the live set and `GrantSet::consume` will admit
            // the identical call again — no card, no operator, until the grant's
            // own TTL. That is a repeated external action under an authority the
            // operator spent once, which is the criterion above.
            //
            // The flush does not close the window on its own, and is not claimed
            // to: redemption happens inside a sync `ToolPolicy::check` with no
            // journal handle, so the id is buffered and written at cycle end
            // (`CompanyCycle::run`), and a crash inside *that* gap loses the
            // record before any append is reached. Flushing removes the part
            // this file controls — the record that was written but only
            // page-cached. Narrowing a duplication window is worth one flush on
            // a record written at operator-decision scale; the batching is the
            // remaining half and is not this issue's to close.
            Self::GrantConsumed { .. } => Durability::Host,
            // Losing a park loses the *question*: the approval vanishes and the
            // agent parks it again on its next attempt. Nothing external fired.
            //
            // **Except for a workflow gate, which has no next attempt (issue
            // #1145).** The re-park reasoning above is a property of the
            // *caller*, not of the record, and it was generalised to a caller
            // that has none. A chat turn re-enters its gate and mints a new
            // approval, so the cost of the loss is one extra question — the
            // tolerance is exactly right there, and that is where the volume is.
            // A workflow run does not: `workflow_resume` turns on the fact that
            // "resume is a re-run, because a paused run is settled" — the engine
            // returned, the future completed, and nothing is holding a
            // continuation. So the parked effect is not a record *of* the
            // continuation, it **is** the continuation, carrying the whole
            // trigger input, and `WorkflowGateQueue::rearm` rebuilds the live
            // gate set at recovery from exactly these still-parked lines. Lose
            // the line and the run keeps a durable `pending_approvals` naming a
            // question that exists nowhere: no card to decide, no re-park
            // coming, and the whole downstream of that pipeline held behind it.
            //
            // Not a new guarantee so much as the one this crate already claims
            // in two other places and did not deliver — `workflow_resume`'s
            // "restart durability … a host that dies between the park and the
            // approval loses nothing", and `CompanyCycle::park`'s "survives a
            // restart with its original `ApprovalId`". Both hold for a *process*
            // restart and fail for the host death the first one names, because
            // `Process` is page-cache-resident by definition. The flush is the
            // same trade `GrantConsumed` accepted four arms up: one flush on a
            // record written at operator-decision scale.
            //
            // The card for an agent node's gated tool call used to stay
            // `Process` here on the theory that the continuation it strands is
            // durable a different way (issue #1816): the two facts the
            // continuation needs are written at park time as a dedicated
            // host-durable `BlockedNodeStashed` record and re-armed into
            // `BlockedNodeQueue` at boot, so a restart between park and approve
            // re-dispatches the run from that record — once the operator has
            // decided.
            //
            // "Once the operator has decided" is exactly what a lost card takes
            // away, and nothing gives it back. `BlockedNodeQueue::rearm`
            // restores the stash, but a stash with no matching card is not a
            // pending decision: it is invisible to the operator (`pending()`
            // and `parked_turns()` both replay from this same record, so a
            // lost line is a lost row on both) and invisible to
            // `reconcile_stranded_blocked_nodes`, which only resumes a turn
            // already durably marked in `blocked_node_approvals` — a set this
            // park's own loss keeps empty, because nobody ever got the chance
            // to approve it. A restart between the park and the decision does
            // not strand the *continuation* (#1816 covers that); it strands the
            // *question*, permanently, with no re-park coming — the identical
            // failure the workflow-gate arm below exists to close, one caller
            // down. So this park needs that arm's durability for that arm's
            // reason, bought the same way: human-approval scale, one flush per
            // blocked node.
            //
            // Keyed on `run_id.is_some()` rather than the effect kind, because
            // a blocked-node park's kind is the tool name itself and varies per
            // call — there is no fixed tag to match the way
            // `WORKFLOW_APPROVE_KIND` is one. `run_id` already carries the
            // distinction that matters: `ApprovalRequestQueue::stamp_run` stamps
            // it with the task-attempt id at the dispatch boundary for exactly a
            // workflow node's own gated call, and deliberately leaves a chat
            // turn's own park — which DOES re-park on its next attempt — at
            // `None`. `gate_effect` stamps the workflow gate's own park with a
            // run id too, so `is_some()` alone already covers it; the explicit
            // kind check is kept so that arm's own reasoning stays legible
            // without depending on this one.
            Self::ApprovalParked { effect, .. }
                if effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND
                    || effect.run_id.is_some() =>
            {
                Durability::Host
            }
            Self::ApprovalParked { .. } => Durability::Process,
            // Bookkeeping after the decision. A ghost approval that is approved
            // a second time cannot duplicate the effect, because the effect's
            // own commit is host-durable and `is_executed` skips it.
            Self::ApprovalResolved { .. } => Durability::Process,
            // Recomputed: the parked record carries the deadline, so replay
            // re-expires it.
            Self::ApprovalExpired { .. } => Durability::Process,
            // An operator's extension (issue #1805). `Process`, like the park it
            // moves: losing it on host death reverts the deadline to the original
            // window — the approval simply expires on its first schedule, the same
            // "one default-deny sooner" tolerance a lost park has. A redeploy
            // (process restart) keeps the page-cached record and replays the move,
            // which is the case the lever has to survive.
            Self::ApprovalExtended { .. } => Durability::Process,
            // Audit-only. The queue removal rides on the paired
            // `ApprovalResolved`, and the original effect stays recoverable from
            // the earlier `ApprovalParked`.
            Self::ApprovalAmended { .. } => Durability::Process,
            // Written *before* the grant goes live, so losing it forgets a YES:
            // the agent is blocked again and the operator is re-asked. The safe
            // direction — the cost of the loss is an extra question, never an
            // extra call.
            Self::ApprovalGranted { .. } => Durability::Process,
            // Conversation continuations carry no execution authority. Losing
            // a queued one means the agent misses a verdict; losing a terminal
            // line can repeat a model follow-up, but cannot repeat an effect.
            // Losing the dispatch claim can replay an entire model turn whose
            // earlier tool call already left the company. Host durability buys
            // at-most-once dispatch; a crash after the claim but before the turn
            // takes the safe at-most-once direction and may drop the follow-up.
            // Losing the queue record after `ApprovalResolved` survived leaves
            // a decided request with no card and no follow-up to recover.
            Self::ApprovalContinuationQueued { .. }
            | Self::ApprovalContinuationDispatched { .. } => Durability::Host,
            Self::ApprovalContinuationConsumed { .. }
            | Self::ApprovalContinuationExpired { .. } => Durability::Process,
            // The same direction as `ApprovalGranted`, one scope wider.
            Self::StandingGrantMinted { .. } => Durability::Process,
            // Deadline arithmetic rather than state: `replayed_standing_grants`
            // takes `now_millis` and re-expires anything past its deadline on
            // the next boot regardless of whether the record survived.
            Self::GrantExpired { .. } => Durability::Process,
            Self::StandingGrantExpired { .. } => Durability::Process,
            // Observability brackets, and the highest-volume records here.
            // Losing either half reads as an interrupted cycle — which, after a
            // host crash, it was.
            Self::CycleStarted { .. } => Durability::Process,
            Self::CycleFinished { .. } => Durability::Process,
            // Issue #1816: the whole point of the record is to outlive the
            // process — and, on a hosted tenant whose journal store is the
            // shared database, the container. `Process` would leave it
            // page-cache-resident and lost with the pod, which is precisely the
            // failure that stranded parked tasks on the ~90-min staging cron.
            // Written at human-approval scale (one per blocked node), so the
            // flush is invisible — the same trade the workflow-gate park makes
            // for its own continuation facts one arm up.
            Self::BlockedNodeStashed { .. } => Durability::Host,
            // The terminator must be at least as durable as the record it
            // retires: if the stash survived a crash but its release did not,
            // the next boot would rehydrate a stash whose run already
            // re-dispatched and could double-spawn under a boot sweep. Host, to
            // match `BlockedNodeStashed`.
            Self::BlockedNodeReleased { .. } => Durability::Host,
            // Protects against exactly the failure `BlockedNodeStashed` does —
            // the fact this exists to carry is only needed across the same
            // restart window, and a `Process`-tier write could be lost to the
            // same pod-roll that motivated the stash's own Host tier, silently
            // reopening the gap this record closes.
            Self::BlockedNodeApproved { .. } => Durability::Host,
            // Issue #1825: the same tier as `BlockedNodeStashed` for the same
            // reason — this is the fact that makes a `BlockedNodeReleased`
            // write failure safe rather than a double-dispatch, so a
            // `Process`-tier write that a pod-roll could still drop would
            // reopen exactly the gap it exists to close.
            Self::BlockedNodeDispatched { .. } => Durability::Host,
        }
    }
}

/// A parked approval awaiting resolution.
#[derive(Clone, Debug)]
pub struct PendingApproval {
    /// The approval's id.
    pub id: ApprovalId,
    /// The parked effect.
    pub effect: Effect,
    /// Epoch-millis the effect was parked.
    pub at_millis: u64,
    /// Epoch-millis this approval's deadline is measured from (issue #1805) —
    /// `at_millis` for a fresh park, the extension time once an operator has
    /// extended it. The projected `expires_at_millis` is this plus the gate's
    /// TTL, so a card's deadline reflects an extension. Distinct from
    /// `at_millis` (payload age, issue #1024) on purpose.
    pub deadline_anchor_millis: u64,
    /// Which board task this approval was parked for (issue #333). `None` only
    /// for a journal line written before the link existed — see [`TaskLink`].
    pub task: Option<TaskLink>,
    /// The chat thread that produced the parking cycle (issue #379) — a desk id
    /// for a channel, a roster agent id for a direct message.
    ///
    /// `None` for a pre-#379 journal line *and* for every park with no
    /// conversation behind it (a workflow delivery, a scheduler tick, a cycle
    /// whose triggers were ambiguous). Both are the same fact downstream: no
    /// channel owns this approval, so it is shown on the Approvals page only.
    pub thread: Option<String>,
    /// The turn that parked it (issue #469), carried out to the read side by
    /// issue #842 so the console can ask about a turn's gated calls **once**.
    ///
    /// Not a new fact and deliberately not a new record: `ApprovalParked`
    /// already journals the parking cycle, because #469 needed to know which
    /// approvals one turn is blocked on in order to continue it exactly once.
    /// #842 is the same grouping seen from the operator's side — a turn that
    /// reached three sites parked three calls, and being asked three times is
    /// the same fact told badly. Projecting the key it already had is the whole
    /// of the mechanism; each park stays its own record, its own decision and
    /// its own host-scoped grant.
    ///
    /// `None` for a pre-#469 journal line and for every park raised outside a
    /// cycle (a workflow node, a scheduler tick): `park_and_journal` in
    /// `workflows::delivery` passes no turn key, because a run holds no
    /// continuation for one to belong to. Both read downstream as "belongs to
    /// no batch", which renders exactly as it did before this field existed:
    /// one card, decided on its own.
    pub batch: Option<String>,
}

/// What an approval *was*, retained for the whole life of the journal — after
/// it resolves, expires, or is amended away (issue #333, over #305's index).
///
/// The parked effect itself is dropped from the queue on resolution, and
/// [`CompanyEvent::ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved)
/// carries only an id, a verdict and an actor. So without this index a resolved
/// approval is unreadable: the read side cannot say what was approved, when it
/// parked, or which task it belonged to.
///
/// **Entries are never removed, and the map is unbounded.** It has the same
/// append-only lifetime as the journal file it is replayed from: one resident
/// entry per approval ever parked, for the life of the process, growing
/// without a ceiling. #333 widens each entry from a `u64` to a `u64` plus two
/// `String`s (the effect kind and, when linked, the task id). No rotation
/// exists today, so `load` rebuilding this from every `ApprovalParked` line is
/// the only path — and it is the correct one. If journal rotation ever lands,
/// this index is the first thing that has to survive it, because a rotated-away
/// park line silently turns its approval unreadable.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalOrigin {
    /// Epoch-millis the effect was parked.
    pub at_millis: u64,
    /// The parked effect's dotted kind, e.g. `payment.send`.
    pub kind: String,
    /// Which board task the parking cycle was dispatched for. `None` only for a
    /// pre-#333 journal line — see [`TaskLink`].
    ///
    /// The **card-level** key, and the fallback one: it cannot say which of a
    /// card's attempts parked the approval. See [`run_id`](Self::run_id).
    pub task: Option<TaskLink>,
    /// The attempt this approval was parked under
    /// ([`Effect::run_id`](crate::ports::types::Effect::run_id), issue #242),
    /// copied off the effect at park time so the read side need not re-open it.
    ///
    /// The **attempt-level** key, and the authoritative one where present: a
    /// [`RunRecord`](crate::ports::runs::RunRecord) names its card, so a run id
    /// resolves to a task, while a task id can never resolve to a run. #183
    /// settled that repeat trips through review are normal, so two attempts on
    /// one card is the expected case — and only this key tells them apart.
    ///
    /// `None` by design for every park with no attempt behind it: a chat turn,
    /// a workflow delivery, a scheduler tick, and the hosted brain's own gate.
    /// That is why it cannot be the only key — see [`task`](Self::task).
    pub run_id: Option<String>,
    /// The chat thread the parking cycle answered (issue #379).
    ///
    /// The **conversation-level** key, and orthogonal to the two above: a chat
    /// turn has a thread and no card, a dispatched card has a card and no
    /// thread, and a desk turn triggered from a channel has both. Retained here
    /// (not only on the live queue) so a *resolved* approval's origin thread is
    /// still recoverable — which is what lets a follow-up cycle's own re-park
    /// stay in the channel the first sign-off was asked in.
    pub thread: Option<String>,
    /// The **thread within** that conversation the parking cycle answered
    /// (issue #435): the root message the raising message hangs off, as that
    /// root's own [`EventSeq`].
    ///
    /// Strictly finer-grained than [`thread`](Self::thread), never a substitute
    /// for it. `thread` names the channel a continuation is delivered to; this
    /// names where inside that channel it is threaded. A continuation needs
    /// both, and a `parent` without a `thread` is meaningless — a sequence
    /// number with no channel to resolve it against.
    ///
    /// **Why not widen `thread`.** `thread` is misleadingly named: it has
    /// always held a *channel* id (a desk id, or a roster agent id for a DM),
    /// and every reader of it — the approvals feed's channel filter, the
    /// continuation's `chat_id`, the grant's routing — depends on that. Making
    /// it mean "thread" would have changed all of them at once, silently, and
    /// the compiler could not have caught a single one because the type is
    /// unchanged. A new field makes the addition additive by construction: the
    /// no-thread path is not merely preserved, it is untouched.
    ///
    /// **The root, not the raising message.** The console folds a transcript
    /// one level deep — a reply whose parent is itself a reply renders nowhere
    /// (`buildTimeline` in `frontend/src/views/chat/model.ts`, pinned by the
    /// timeline unit test "renders a grandchild nowhere: the fold is exactly
    /// one level deep" in `frontend/test/unit/chat-timeline.test.ts`). That
    /// test exists for this decision: without it, growing a second fold level
    /// in the console would make the choice below unnecessary and nothing would
    /// say so — the routing would survive as an unexplained convention.
    ///
    /// So a continuation parented to the raising *message* would vanish
    /// precisely when that message is itself a thread reply, which is the case
    /// this issue exists to fix. Parenting to the root is also what the chat
    /// route already does for an ordinary answer — "the answer joins the thread
    /// its question was asked in, rather than opening one under the question"
    /// (issue #364, `crate::server::operator`) — so this is that established
    /// rule applied to the continuation, not a second convention. It is stable
    /// under an edit of the raising message for the same reason.
    ///
    /// `None` for a park with no thread behind it — a message posted straight
    /// into a channel, a workflow delivery, a scheduler tick — and for every
    /// line written before this field existed. All of them mean "the channel is
    /// the answer", which is the pre-#435 behaviour, unchanged.
    pub parent: Option<EventSeq>,
    /// The **turn** that parked it: the id of the parking cycle (issue #469).
    ///
    /// The key that groups the several approvals one turn can raise, so the
    /// turn is continued once — after the last of them is decided — instead of
    /// once per decision. `None` for a pre-#469 journal line, which continues
    /// on its own exactly as it used to.
    pub cycle: Option<String>,
}

/// Where an approval was raised: the channel, and the thread inside it
/// (issue #435).
///
/// The pair a continuation needs in order to land back where it was asked for.
/// Returned as one value by
/// [`approval_conversation`](Journal::approval_conversation) so the two can
/// never be read from different approvals; see that method for why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApprovalConversation {
    /// The channel — see [`ApprovalOrigin::thread`], whose name this mirrors
    /// and whose channel-not-thread meaning it keeps.
    pub thread: Option<String>,
    /// The thread root within that channel — see [`ApprovalOrigin::parent`].
    ///
    /// Only meaningful alongside `thread`. A `parent` with no `thread` cannot
    /// arise from a park — both are stamped from one cycle, and a cycle with no
    /// channel has no thread either — and would not be resolvable if it did.
    pub parent: Option<EventSeq>,
}

/// One approval currently waiting in the in-memory queue.
#[derive(Clone, Debug)]
struct ParkedApproval {
    effect: Effect,
    at_millis: u64,
    /// Epoch-millis this approval's TTL window is measured from (issue #1805).
    ///
    /// Starts equal to `at_millis` — a fresh park's deadline is `at_millis +
    /// ttl` — and moves to the extension time when an operator extends it. Held
    /// separately from `at_millis` because that one dates the PAYLOAD (issue
    /// #1024) and a deadline extension must not make the content look fresher.
    /// This is the anchor the gate is rehydrated from at boot, so both the live
    /// sweeper and the projected deadline stay in step across a redeploy.
    deadline_anchor_millis: u64,
    /// `None` only for a journal line written before #333.
    task: Option<TaskLink>,
    /// The chat thread that parked it (issue #379); `None` when no conversation
    /// produced it, or on a pre-#379 line.
    thread: Option<String>,
    /// The turn that parked it (issue #469); `None` on a pre-#469 line. Held on
    /// the live entry, not only in `origins`, because recovery has to re-arm the
    /// continuation queue from exactly the approvals that are *still* waiting.
    cycle: Option<String>,
}

/// A side effect that was **committed to run** (issue #351): what it was, which
/// board task it was run for, and whether it is one that cannot be taken back.
///
/// "Committed", not "completed", and the distinction is deliberate. The record
/// is written *before* the side effect is performed — that ordering is what
/// makes effects at-most-once — and a failed or interrupted perform leaves it
/// standing. So an entry means "this was committed, and the runtime will never
/// run it again", which is exactly the fact a retry warning needs: the operator
/// has to assume it happened, because nothing else will ever finish it and
/// nothing will re-attempt it. It does **not** mean the effect is known to have
/// completed. Operator-facing wording is qualified to match
/// (`RetryButton`, `frontend/src/views/TaskDetailView.tsx`).
///
/// Recorded alongside the idempotency key so a retry can say what the previous
/// attempt already did. Deliberately **not** the whole [`Effect`]: `payload`
/// carries recipients, message bodies and arguments, and this record is read
/// back out onto an operator's screen through the task-detail route, which
/// scrubs by construction. The classification facts are kept; the contents are
/// not.
///
/// `irreversible` is decided **at execution time**, by the gate that was in
/// force then (`ManifestApprovalGate::is_irreversible`), rather than re-derived
/// on read. A company that later raises its auto-approve cap does not get to
/// retroactively decide that the payment it made last week was routine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutedEffect {
    /// The dotted effect kind, e.g. `payment.send`. The console maps it to
    /// plain language; it is never shown raw.
    pub kind: String,
    /// The USD amount involved, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_usd: Option<f64>,
    /// The board task this effect was executed for, when a card was behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Epoch-millis the effect was committed.
    pub at_millis: u64,
    /// Whether the supervised taxonomy calls this one irreversible.
    pub irreversible: bool,
}

/// A journal line [`load`](RuntimeJournal::load) could not replay (issue #386).
///
/// Deliberately carries **no line content**. The journal holds effect payloads —
/// recipients, message bodies, arguments — and a corruption report exists to be
/// logged and read by an operator, which is the one place [`ExecutedEffect`]
/// goes to some trouble to keep those out of. The line number locates it in the
/// file, the byte length separates a merged pair (long) from a truncated tail
/// (short), and the parse error names the column without quoting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorruptLine {
    /// The record's 1-based position in what the [`JournalStore`] read back.
    ///
    /// For the filesystem backend that is the line's number in `journal.jsonl`,
    /// unchanged — the fs store returns every `\n`-separated segment, blanks
    /// included, so a blank line does not shift the count. For a database
    /// backend there is no file to open, and the number locates the record in
    /// append order.
    pub line: usize,
    /// The line's length in bytes.
    pub bytes: usize,
    /// What the parse rejected.
    pub message: String,
}

/// A cycle that journaled a start and no finish (issue #390).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCycle {
    /// The cycle's id.
    pub cycle_id: String,
    /// Epoch-millis it started.
    pub at_millis: u64,
    /// What kicked it off — see [`JournalRecord::CycleStarted::trigger`].
    pub trigger: String,
}

/// The error stamped on a cycle the host never got to finish (issue #390).
///
/// Phrased as a host fact rather than an agent fault, exactly as
/// [`INTERRUPTED_BY_RESTART`](crate::runtime::workflow_outcome::INTERRUPTED_BY_RESTART)
/// is for a workflow run: nothing about the turn went wrong, the process holding
/// it went away. An operator reading this should retry the approval, not go
/// looking at their agent.
pub const INTERRUPTED_BY_HOST_RESTART: &str = concat!(
    "this cycle was interrupted by a host restart and never finished; ",
    "if it was an approval's follow-up, re-approving is a safe no-op that ",
    "mints no second grant"
);

/// In-memory state rebuilt from (and kept in sync with) `journal.jsonl`.
#[derive(Default)]
struct State {
    executed: HashSet<String>,
    /// Cycles that started and have not finished (issue #390).
    ///
    /// A start inserts, a finish removes; whatever is left when replay ends
    /// either is running right now or died with a previous host. Telling those
    /// two apart is not this map's job — it is the boot sweep's, and the sweep
    /// is half the requirement rather than a follow-up. Without it every crashed
    /// cycle reads as in-flight forever, which is worse than the log line this
    /// replaces because it looks like live work.
    open_cycles: HashMap<String, OpenCycle>,
    /// Lines the last replay could not read — see [`CorruptLine`].
    corrupt: Vec<CorruptLine>,
    /// Every irreversible effect that ran for a board task, indexed by that
    /// task and oldest first within it (issue #351).
    ///
    /// Append-only for the same reason [`executed`](Self::executed) is: an
    /// effect that fired stays fired, and a retry warning that forgot half the
    /// history would be worse than none. One small record per effect, with no
    /// payload — see [`ExecutedEffect`].
    ///
    /// Indexed rather than a flat list because the read side is a per-task
    /// lookup on every Task Detail GET, and a linear scan of every effect a
    /// company ever executed is not flat for a long-lived one. Reversible
    /// effects and effects with no card behind them are dropped on the way in:
    /// nothing reads them, and the only thing keeping them would grow is
    /// memory.
    irreversible_by_task: HashMap<String, Vec<ExecutedEffect>>,
    /// Whether replay saw an executed key it cannot describe (issue #351).
    ///
    /// True when a pre-#351 `EffectExecuted` line is read back: the key proves
    /// something ran, and the record carries no way to say what. The retry
    /// dialog's "nothing irreversible here" is only honest when this is false,
    /// so the console is told and confirms regardless — see
    /// [`has_undescribed_history`](RuntimeJournal::has_undescribed_history).
    undescribed_executed: bool,
    parked: HashMap<ApprovalId, ParkedApproval>,
    /// The effect each approval was parked with, **payload scrubbed**, retained
    /// after the approval leaves [`parked`](Self::parked) (issue #351).
    ///
    /// Approving a harness tool call mints a grant rather than executing, so
    /// the only description of what the operator said yes to lives on the park
    /// record. This is what the grant-consumption path reads back to classify
    /// and name it once the tool has actually run. Overwritten by an
    /// approve-with-edit, because the grant is minted against the amended
    /// arguments and the amount the operator approved is the one to report.
    ///
    /// The payload is replaced with `Null` on the way in. Classification reads
    /// only the kind, group, amount and counterparty flags, and this map
    /// outlives the queue entry — retaining recipients and message bodies for
    /// the life of the process to answer a question that never asks for them
    /// would be the one leak [`ExecutedEffect`] exists to avoid.
    approval_effects: HashMap<ApprovalId, Effect>,
    /// What each approval was when it parked, retained after it leaves `parked`.
    ///
    /// This is what makes waiting time readable (issue #305) and what links a
    /// resolved approval back to its board task (issue #333). Both facts are
    /// journal-only — [`CompanyEvent::ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved)
    /// carries the resolution but neither the park time nor the task — so they
    /// are recoverable only by joining the two on [`ApprovalId`]. See
    /// [`ApprovalOrigin`] for why entries are never removed.
    origins: HashMap<ApprovalId, ApprovalOrigin>,
    /// Grants minted and not yet consumed or expired (issue #243).
    ///
    /// Unlike [`origins`](Self::origins) this one IS removed from on
    /// the terminal records: a replayed grant is handed straight back to the
    /// live [`GrantSet`](crate::runtime::grants::GrantSet), so keeping a
    /// consumed or expired entry here would re-arm a tool call that already ran
    /// (or that the operator was already told had lapsed) on every restart.
    grants: HashMap<ApprovalId, GrantedCall>,
    /// Explicit approval follow-ups still owed after replay. Kept separate from
    /// grants because a denial is a continuation, never executable authority.
    approval_continuations: HashMap<ApprovalId, ApprovalContinuation>,
    /// Standing grants minted and not yet revoked or expired (issue #374).
    ///
    /// Removed from on both terminal records for the same reason as
    /// [`grants`](Self::grants): a replayed entry is handed straight back to the
    /// live set, so retaining a revoked one would hand back a permission the
    /// operator explicitly took away — on every restart, silently.
    standing_grants: HashMap<GrantId, StandingGrant>,
    /// Blocked agent-node continuation facts still awaiting re-dispatch, keyed by
    /// the per-(run, node) turn key (issue #1816, Stage 2).
    ///
    /// A [`BlockedNodeStashed`](JournalRecord::BlockedNodeStashed) inserts, its
    /// paired [`BlockedNodeReleased`](JournalRecord::BlockedNodeReleased) removes
    /// — the same start-inserts / terminator-removes shape
    /// [`grants`](Self::grants) uses, and for the same reason: a replayed entry is
    /// handed straight back to the live
    /// [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue) at
    /// boot, so retaining a released one would rehydrate a run that already
    /// re-dispatched.
    blocked_stashes: HashMap<String, BlockedStash>,
    /// Blocked agent-node turns with at least one approved decision banked so
    /// far, keyed the same as [`blocked_stashes`](Self::blocked_stashes)
    /// (issue #1816).
    ///
    /// Inserted by [`BlockedNodeApproved`](JournalRecord::BlockedNodeApproved)
    /// and removed by its stash's paired
    /// [`BlockedNodeReleased`](JournalRecord::BlockedNodeReleased) — the release
    /// that retires the stash also retires whatever this set knows about it,
    /// so a turn never lingers here past the continuation it describes.
    blocked_node_approvals: HashSet<String>,
    /// Blocked agent-node turns whose continuation has already been spawned
    /// once, keyed the same as [`blocked_stashes`](Self::blocked_stashes)
    /// (issue #1825).
    ///
    /// Inserted by
    /// [`BlockedNodeDispatched`](JournalRecord::BlockedNodeDispatched), the
    /// moment [`resume_blocked_agent_node`](crate::company::runtime::CompanyRuntime::resume_blocked_agent_node)'s
    /// spawn attempt actually succeeds — before that call goes on to retire the
    /// stash via [`BlockedNodeReleased`](JournalRecord::BlockedNodeReleased),
    /// which is the write `retire_blocked_stash` treats as best-effort. If that
    /// later write fails, `blocked_stashes` and `blocked_node_approvals` both
    /// survive a restart exactly as if nothing had been dispatched, and without
    /// this set `reconcile_stranded_blocked_nodes` cannot tell that apart from
    /// the genuine stranded case — it would re-spawn a continuation that
    /// already ran.
    ///
    /// # Deliberately *not* retired by `BlockedNodeReleased` (finding `3877914597`)
    ///
    /// Unlike [`blocked_node_approvals`](Self::blocked_node_approvals), a turn
    /// entered here is permanent for the life of the process and every future
    /// replay — the same shape [`executed`](Self::executed) already uses, for
    /// the same reason. A workflow-gate blocked-node card's own
    /// [`ApprovalParked`](JournalRecord::ApprovalParked) is `Durability::Host`,
    /// but [`ApprovalResolved`](JournalRecord::ApprovalResolved) is always
    /// `Durability::Process`: a host crash can lose only the resolution and
    /// leave that card durably reopened as a "ghost" *after* its continuation
    /// already ran to completion and its own `BlockedNodeReleased` already
    /// landed. `resume_blocked_agent_node`'s guard against a ghost decision
    /// (issue #1825, finding `3877718169`) reads only this set, so a version
    /// that cleared the turn out of it on release (as this one used to) made
    /// that guard read `false` for exactly the case it exists to catch — the
    /// ghost then fell through to the "no stash on this host" branch, which
    /// tells the operator to re-run the workflow by hand, manually repeating
    /// the very side effect the guard exists to prevent automatically. One
    /// leaked turn key per completed blocked node is the accepted cost of
    /// closing that, the same trade `executed` already makes.
    blocked_node_dispatched: HashSet<String>,
}

/// One blocked agent node's durable continuation facts (issue #1816).
#[derive(Clone, Debug)]
struct BlockedStash {
    workflow_id: String,
    input: Value,
    /// The blocked run's own attribution (issue #1862 prerequisite), carried
    /// so [`blocked_stashes`](RuntimeJournal::blocked_stashes) can hand
    /// [`BlockedNodeQueue::rearm`](crate::runtime::blocked_nodes::BlockedNodeQueue::rearm)
    /// the real trigger instead of a hardcoded `Operator` default.
    started_by: StartedBy,
    /// Whether this stash's `BlockedNodeStashed` append has actually landed
    /// (issue #1825, P1 — found by chatgpt-codex-connector).
    ///
    /// Set on insert by [`replay`](RuntimeJournal::replay), since a record it
    /// folds is durable by construction. Starts `false` for a live insert made
    /// by [`record_blocked_node_stashed`](RuntimeJournal::record_blocked_node_stashed)
    /// ahead of its own append, and flips to `true` only once that append
    /// actually returns `Ok`. The settle-time fallback call reads this — not
    /// mere presence in the map — to decide whether there is still an append
    /// worth retrying.
    durable: bool,
}

impl State {
    /// Files an executed effect under the card it ran for, keeping only what
    /// the retry warning reads (issue #351).
    ///
    /// Two drops, both deliberate: a reversible effect is never named, and an
    /// effect with no card behind it belongs to no dialog. Retaining either
    /// would grow one map per company for a lookup that filters them straight
    /// back out.
    fn index_executed(&mut self, effect: ExecutedEffect) {
        if !effect.irreversible {
            return;
        }
        let Some(task_id) = effect.task_id.clone() else {
            return;
        };
        self.irreversible_by_task
            .entry(task_id)
            .or_default()
            .push(effect);
    }

    /// Retains an approval's effect for later description, without its payload.
    fn retain_approval_effect(&mut self, id: &ApprovalId, effect: &Effect) {
        self.approval_effects.insert(
            id.clone(),
            Effect {
                payload: serde_json::Value::Null,
                ..effect.clone()
            },
        );
    }
}

/// A per-company append-only journal backing at-most-once effects and the
/// durable approval queue.
///
/// One process should own a given company's journal, but [`append`](Self::append)
/// no longer depends on that for integrity (issue #386). The filesystem store
/// writes every record whole — terminator included — in a single `O_APPEND`
/// write that has reached the kernel before the call returns, so a concurrent
/// writer can land a record before or after but never inside one, and it
/// serialises writers within the process on a per-path lock. A database backend
/// gets the same property more cheaply: a row or document insert is atomic, and
/// its sequence comes from the server, so two live hosts interleave without
/// collision.
///
/// Writers through *one* `RuntimeJournal` additionally serialise on
/// [`write_lock`](Self::write_lock), which keeps records in call order — so a
/// park cannot be replayed after the resolution that drains it. That lock is
/// held across the store call, which is what keeps a backend's sequence
/// allocation in call order too.
/// The company id a [`file-pinned`](RuntimeJournal::new) journal reports.
///
/// The store behind that constructor addresses one named file and never looks at
/// the id, so this is what shows up in a log line rather than a key anything
/// resolves. Named instead of empty so a stray appearance in a trace is
/// self-explaining.
const FILE_PINNED_COMPANY: &str = "<file-pinned journal>";

pub struct RuntimeJournal {
    store: Arc<dyn JournalStore>,
    company: CompanyId,
    state: StdMutex<State>,
    write_lock: TokioMutex<()>,
}

impl RuntimeJournal {
    /// Opens the journal for `company` over `store`, without loading it.
    ///
    /// Call [`load`](Self::load) to replay an existing journal into memory.
    pub fn with_store(store: Arc<dyn JournalStore>, company: CompanyId) -> Self {
        Self {
            store,
            company,
            state: StdMutex::new(State::default()),
            write_lock: TokioMutex::new(()),
        }
    }

    /// Opens (or prepares) a filesystem journal at `path` without loading it.
    ///
    /// The convenience constructor over [`with_store`](Self::with_store) for the
    /// case where a caller has a file rather than a backend — every test in the
    /// crate, and nothing in production, which resolves its store from the
    /// selected backend in `RuntimeBuilder`.
    ///
    /// The store is pinned to the named file and ignores the company id, so the
    /// id here is a label rather than a key. Two journals over one path still
    /// share an append lock: the key is the absolutised path, so a relative and
    /// an absolute spelling of one file match; a symlinked or `..`-laden
    /// spelling still does not, and falls back on the atomic write for its
    /// safety.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_store(
            Arc::new(FsJournalStore::at_file(path)),
            CompanyId::new(FILE_PINNED_COMPANY),
        )
    }

    /// Replays the on-disk journal into memory, reconstructing the executed-key
    /// set and the parked-approval queue. Idempotent.
    ///
    /// **A damaged line does not fail the load** (issue #386). It is skipped,
    /// logged against the file and line number, and reported through
    /// [`corruption`](Self::corruption) for the caller to act on. Before this,
    /// one bad line returned `Err` from here and took the whole company's boot
    /// with it — turning the loss of a single record into the loss of every
    /// record after it, plus the tenant. An operator cannot repair a journal
    /// through a console that will not start.
    ///
    /// The skip is genuinely lossy and the safety argument is not symmetric: a
    /// dropped `ApprovalResolved` leaves an approval parked, which a person can
    /// still deny, while a dropped `EffectExecuted` un-commits a key and lets an
    /// effect run twice. That is why [`replay_line`] recovers a merged line in
    /// full rather than skipping it — the historical corruption this issue is
    /// about is exactly the recoverable kind, and skipping it is the outcome
    /// worth working to avoid.
    pub async fn load(&self) -> Result<()> {
        // The store hands back lines, never bytes, and decodes lossily on the
        // way: a torn write can split a multi-byte codepoint, and a whole-file
        // UTF-8 decode would fail the entire load on that one bad byte — failing
        // the boot for exactly the damage this function exists to survive.
        // Per-line decoding keeps a single mangled line on the `CorruptLine`
        // path with the rest of the journal intact.
        let lines = self.store.read_journal(&self.company).await?;

        let mut state = State::default();
        for (index, line) in lines.iter().enumerate() {
            let line = line.as_str();
            if line.trim().is_empty() {
                continue;
            }
            let records = match replay_line(line) {
                Ok(records) => records,
                Err(message) => {
                    let corrupt = CorruptLine {
                        line: index + 1,
                        bytes: line.len(),
                        message,
                    };
                    tracing::error!(
                        company = %self.company,
                        line = corrupt.line,
                        bytes = corrupt.bytes,
                        error = %corrupt.message,
                        "journal line could not be replayed; skipping it and continuing",
                    );
                    state.corrupt.push(corrupt);
                    continue;
                }
            };
            if records.len() > 1 {
                // Recovered, not lost — so not a `CorruptLine`. Still worth
                // saying out loud: the journal carries damage from a host that
                // predates the write fix, and a reader looking at it by hand
                // should know why one line holds several records.
                tracing::warn!(
                    company = %self.company,
                    line = index + 1,
                    records = records.len(),
                    "journal line holds several records with no separator; \
                     replaying all of them",
                );
            }
            for record in records {
                Self::replay(&mut state, record);
            }
        }
        *self.state.lock().expect("journal state poisoned") = state;
        Ok(())
    }

    /// Lines the last [`load`](Self::load) could not replay, in file order.
    ///
    /// Empty is the only healthy answer. A non-empty one means the company is
    /// running on an incomplete history and something above has to say so.
    pub fn corruption(&self) -> Vec<CorruptLine> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .corrupt
            .clone()
    }

    /// Folds one replayed record into the rebuilt state.
    fn replay(state: &mut State, record: JournalRecord) {
        match record {
            JournalRecord::EffectExecuted { key, effect } => {
                state.executed.insert(key);
                // Absent on a pre-#351 line: the key still replays, the
                // description simply does not exist to replay. Flag it, so
                // the console says "there is earlier activity I cannot
                // describe" rather than showing an all-clear.
                match effect {
                    Some(effect) => state.index_executed(effect),
                    None => state.undescribed_executed = true,
                }
            }
            JournalRecord::ApprovalParked {
                id,
                effect,
                at_millis,
                task,
                thread,
                parent,
                cycle,
            } => {
                state.retain_approval_effect(&id, &effect);
                state.origins.insert(
                    id.clone(),
                    ApprovalOrigin {
                        at_millis,
                        kind: effect.kind.clone(),
                        task: task.clone(),
                        run_id: effect.run_id.clone(),
                        thread: thread.clone(),
                        parent,
                        cycle: cycle.clone(),
                    },
                );
                state.parked.insert(
                    id,
                    ParkedApproval {
                        effect,
                        at_millis,
                        // Reset each replay to the park instant, then moved by
                        // any later `ApprovalExtended` line below — the log order
                        // is what makes the last extension win (issue #1805).
                        deadline_anchor_millis: at_millis,
                        task,
                        thread,
                        cycle,
                    },
                );
            }
            JournalRecord::ApprovalResolved { id } => {
                state.parked.remove(&id);
            }
            JournalRecord::ApprovalExpired { id, .. } => {
                state.parked.remove(&id);
            }
            // Issue #1805: re-anchor the deadline window. A no-op for an id no
            // longer parked (already resolved/expired earlier in the log), which
            // is correct — an extension of something since decided moves nothing.
            JournalRecord::ApprovalExtended { id, at_millis, .. } => {
                if let Some(parked) = state.parked.get_mut(&id) {
                    parked.deadline_anchor_millis = at_millis;
                }
            }
            // Audit-only for the queue: the paired `ApprovalResolved`
            // handles removal. The amended effect does supersede the parked
            // one for description, because it is the amended arguments the
            // grant was minted against.
            JournalRecord::ApprovalAmended {
                id, amended_effect, ..
            } => {
                state.retain_approval_effect(&id, &amended_effect);
            }
            JournalRecord::ApprovalGranted { grant } => {
                state.grants.insert(grant.approval_id.clone(), grant);
            }
            JournalRecord::ApprovalContinuationQueued { continuation } => {
                state
                    .approval_continuations
                    .insert(continuation.call.approval_id.clone(), continuation);
            }
            JournalRecord::ApprovalContinuationDispatched { id, .. }
            | JournalRecord::ApprovalContinuationConsumed { id }
            | JournalRecord::ApprovalContinuationExpired { id, .. } => {
                state.approval_continuations.remove(&id);
            }
            JournalRecord::GrantConsumed { id, effect } => {
                state.grants.remove(&id);
                // Absent only on a line written before the grant path was
                // described; same additive contract as `EffectExecuted`.
                if let Some(effect) = effect {
                    state.index_executed(effect);
                }
            }
            JournalRecord::GrantExpired { id, .. } => {
                state.grants.remove(&id);
            }
            JournalRecord::StandingGrantMinted { grant } => {
                state.standing_grants.insert(grant.id.clone(), grant);
            }
            JournalRecord::StandingGrantRevoked { id, .. } => {
                state.standing_grants.remove(&id);
            }
            JournalRecord::StandingGrantExpired { id, .. } => {
                state.standing_grants.remove(&id);
            }
            // Issue #390: start inserts, finish removes. A finish for a cycle
            // this journal never started removes nothing, which is right rather
            // than a gap — a pre-#390 line has no start to be matched against,
            // so no such cycle can be sitting in the map.
            JournalRecord::CycleStarted {
                cycle_id,
                at_millis,
                trigger,
            } => {
                state.open_cycles.insert(
                    cycle_id.clone(),
                    OpenCycle {
                        cycle_id,
                        at_millis,
                        trigger,
                    },
                );
            }
            JournalRecord::CycleFinished { cycle_id, .. } => {
                state.open_cycles.remove(&cycle_id);
            }
            // Issue #1816: start inserts, terminator removes — the same shape as
            // grants. A `BlockedNodeReleased` for a turn this journal never
            // stashed removes nothing, which is correct: a pre-#1816 line has no
            // stash to retire, so none can be sitting in the map.
            JournalRecord::BlockedNodeStashed {
                turn,
                workflow_id,
                input,
                started_by,
                ..
            } => {
                state.blocked_stashes.insert(
                    turn,
                    BlockedStash {
                        workflow_id,
                        input,
                        started_by,
                        // A record `replay` folds is durable by construction —
                        // it was read back from the journal it describes.
                        durable: true,
                    },
                );
            }
            JournalRecord::BlockedNodeReleased { turn } => {
                state.blocked_stashes.remove(&turn);
                state.blocked_node_approvals.remove(&turn);
                // `blocked_node_dispatched` is deliberately NOT retired here —
                // see that field's own doc comment (finding `3877914597`). A
                // ghost decision can still reach this turn after this very
                // release replays, and the guard it feeds needs the tombstone
                // to still be standing when it does.
            }
            JournalRecord::BlockedNodeApproved { turn } => {
                state.blocked_node_approvals.insert(turn);
            }
            JournalRecord::BlockedNodeDispatched { turn } => {
                state.blocked_node_dispatched.insert(turn);
            }
        }
    }

    /// Opens a cycle's bracket (issue #390).
    ///
    /// # Called before the serial lock, deliberately
    ///
    /// The issue's body asked for this "as the follow-up cycle takes the serial
    /// lock". That placement cannot see the failure the issue exists for. The
    /// per-company serial lock is held for a **whole** cycle, so a continuation
    /// spawned behind a busy company waits on it for an unbounded time — and
    /// every way an operator ends up with "I approved, it said `recorded: true`,
    /// nothing happened" is on the near side of that lock:
    ///
    /// * the host dies after `tokio::spawn` but before the task is first polled;
    /// * the host dies while the task is queued on the lock;
    /// * the spawned task panics before the cycle body runs.
    ///
    /// Bracketing after the lock would report every one of those as though the
    /// cycle had never been asked for, which is the state of the world today.
    ///
    /// # The window this still does not cover
    ///
    /// A host that dies between the **durable verdict** and `tokio::spawn`
    /// writes no start at all, so nothing — not this bracket, not the sweep —
    /// can see it, and the operator is exactly as blind as before. Closing that
    /// needs a record written when the verdict is settled (an "owed
    /// continuation"), which is a different feature from a cycle bracket and is
    /// deliberately not built here. Named rather than left to be discovered, in
    /// the register of `run_supervisor`'s two known gaps.
    ///
    /// # Ordering
    ///
    /// Appends serialise on [`JOURNAL_WRITE_LOCKS`], **not** on the cycle's
    /// serial lock, so brackets from concurrent cycles interleave in the file.
    /// That is harmless for [`open_cycles`](Self::open_cycles), which folds by
    /// id rather than by position — but the journal stops reading as one
    /// sequential story by hand, and anyone doing that should know why.
    pub async fn record_cycle_started(&self, cycle_id: &str, trigger: &str) -> Result<()> {
        let at_millis = crate::ports::now_millis();
        self.state
            .lock()
            .expect("journal state poisoned")
            .open_cycles
            .insert(
                cycle_id.to_string(),
                OpenCycle {
                    cycle_id: cycle_id.to_string(),
                    at_millis,
                    trigger: trigger.to_string(),
                },
            );
        self.append(&JournalRecord::CycleStarted {
            cycle_id: cycle_id.to_string(),
            at_millis,
            trigger: trigger.to_string(),
        })
        .await
    }

    /// Closes a cycle's bracket (issue #390). `error` is `None` on success.
    ///
    /// A **panicking** cycle task journals nothing here — it unwinds past this
    /// call — so it reads as open until the next boot sweep settles it. That is
    /// the same exposure `run_supervisor` documents for a panicking workflow
    /// run, and the same remedy covers both.
    pub async fn record_cycle_finished(&self, cycle_id: &str, error: Option<String>) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .open_cycles
            .remove(cycle_id);
        self.append(&JournalRecord::CycleFinished {
            cycle_id: cycle_id.to_string(),
            at_millis: crate::ports::now_millis(),
            error,
        })
        .await
    }

    /// Cycles that started and never finished, oldest first (issue #390).
    ///
    /// Only honest because [`sweep_interrupted_cycles`](Self::sweep_interrupted_cycles)
    /// settles the strays at boot. Without it this would report every cycle any
    /// dead host ever started as in-flight forever.
    pub fn open_cycles(&self) -> Vec<OpenCycle> {
        let mut open: Vec<OpenCycle> = self
            .state
            .lock()
            .expect("journal state poisoned")
            .open_cycles
            .values()
            .cloned()
            .collect();
        // Sorted so the surface is deterministic; a `HashMap` order would make
        // two reads of an unchanged journal disagree for no reason.
        open.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then(a.cycle_id.cmp(&b.cycle_id))
        });
        open
    }

    /// Settles every cycle left open by a previous host process, returning how
    /// many were closed (issue #390).
    ///
    /// # Why an unterminated start is provably dead at boot
    ///
    /// The same argument
    /// [`sweep_interrupted_runs`](crate::runtime::sweep_interrupted_runs) rests
    /// on: a cycle journals its start before it does anything, every cycle is
    /// driven inside this process, and one process owns this journal. So at
    /// boot, before any entry point can have started a cycle, an unmatched start
    /// cannot belong to a live one — there are no live ones. No timeout
    /// heuristic is needed.
    ///
    /// # It must NOT run on a rebuild
    ///
    /// That argument holds at boot and is false the moment a company has been
    /// serving. A cycle survives a live runtime swap
    /// ([`rebuild_company`](crate::runtime::rebuild_company)), so sweeping
    /// mid-life would stamp "interrupted by a host restart" on a cycle still
    /// running — and its real finish would then land after the synthetic one,
    /// leaving two contradictory outcomes for one cycle id. The caller gates on
    /// the handover being absent; see the call site in the runtime builder.
    ///
    /// Best-effort: an append failure is logged and swallowed, because
    /// record-keeping must never stop a company from booting.
    pub async fn sweep_interrupted_cycles(&self) -> usize {
        let open = self.open_cycles();
        let mut settled = 0;
        for cycle in open {
            tracing::info!(
                company = %self.company,
                cycle = %cycle.cycle_id,
                trigger = %cycle.trigger,
                started_at = cycle.at_millis,
                "settling a cycle left open by a previous host process"
            );
            match self
                .record_cycle_finished(
                    &cycle.cycle_id,
                    Some(INTERRUPTED_BY_HOST_RESTART.to_string()),
                )
                .await
            {
                Ok(()) => settled += 1,
                Err(err) => tracing::warn!(
                    company = %self.company,
                    cycle = %cycle.cycle_id,
                    %err,
                    "could not settle an interrupted cycle; it stays open in the journal"
                ),
            }
        }
        settled
    }

    /// Whether an effect under `key` was already committed.
    pub fn is_executed(&self, key: &str) -> bool {
        self.state
            .lock()
            .expect("journal state poisoned")
            .executed
            .contains(key)
    }

    /// Commits an effect key to the journal before its side effect runs,
    /// alongside a description of what the key is about to do (issue #351).
    ///
    /// A no-op (returns `Ok`) if the key is already committed — which is also
    /// what keeps the executed-effect list free of duplicates: the second
    /// commit under a key never reaches the append.
    ///
    /// **A failed append releases the key again.** The in-memory set is a mirror
    /// of what the file holds, and holding a key the append refused makes it lie
    /// in the one direction that is silent: `execute_effect_once` aborts before
    /// `perform_effect` on the error, so nothing external fired — but a later
    /// attempt under the same key would then find the key present, take the
    /// `Ok(())` early return, and skip the effect *reporting success*. The
    /// effect would never run and no caller would ever hear that. Releasing the
    /// key makes the retry a real retry.
    ///
    /// This does not weaken at-most-once, because the two are on opposite sides
    /// of the side effect. The guarantee is about a crash *after* a commit that
    /// succeeded; this is a commit that failed, before which nothing ran. The
    /// worst case is the uncertain one — the write reached the file and only the
    /// flush failed — and it still cannot duplicate: the retry appends a second
    /// line for the key (replay dedupes, `executed` is a set) and runs the
    /// effect exactly once, and a crash before the retry replays the first line
    /// and skips the effect entirely. Every path is one execution or none.
    ///
    /// Contrast [`record_grant_consumed`](Self::record_grant_consumed), which
    /// deliberately does *not* roll back: its tool has already run by the time
    /// the record is written, so keeping the grant spent in memory is the safe
    /// direction and restoring it would re-arm a grant that was redeemed.
    pub async fn record_executed(&self, key: &str, effect: ExecutedEffect) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            if !state.executed.insert(key.to_string()) {
                return Ok(());
            }
        }
        let appended = self
            .append(&JournalRecord::EffectExecuted {
                key: key.to_string(),
                effect: Some(effect.clone()),
            })
            .await;
        let mut state = self.state.lock().expect("journal state poisoned");
        match appended {
            // Indexed only once the commit is on the file, so the retry warnings
            // built from it describe effects the journal actually committed.
            Ok(()) => state.index_executed(effect),
            Err(_) => {
                state.executed.remove(key);
            }
        }
        appended
    }

    /// Records a newly parked approval and which board task it belongs to
    /// (issue #333).
    ///
    /// `task` is deliberately **not** an `Option`: every caller must say which
    /// it is, [`TaskLink::Task`] or [`TaskLink::Unlinked`], so that a missing
    /// link can only ever mean "written before #333". A caller with an
    /// `Option<&str>` in hand converts with [`TaskLink::from_task_id`].
    ///
    /// `thread` **is** an `Option`, and deliberately so (issue #379): unlike the
    /// task link, nothing downstream distinguishes "no conversation produced
    /// this" from "this host does not record conversations". Both mean no
    /// channel owns the approval, and both correctly leave it on the Approvals
    /// page alone.
    ///
    /// `cycle` is the parking turn (issue #469), and is what lets the runtime
    /// continue a turn once rather than once per approval it raised. `Option`
    /// on the same terms as `thread`: absent means "this host did not record a
    /// turn", which falls back to continuing the approval on its own.
    ///
    /// `conversation` carries the channel **and** the thread root inside it as
    /// one value (issue #435), rather than as two adjacent parameters. Both of
    /// its fields are `Option` on the terms above, and its `parent` is only ever
    /// meaningful alongside its `thread` — see [`ApprovalOrigin::parent`]. They
    /// travel together for the same reason
    /// [`approval_conversation`](Self::approval_conversation) returns them
    /// together: two same-shaped `Option`s side by side in a call are trivially
    /// transposable by a caller and the compiler would not notice, and a park is
    /// the one place a wrong pairing would be written down durably. The
    /// `ApprovalConversation` this hands back on the read side is the same type,
    /// so a continuation round-trips one value instead of re-assembling two.
    pub async fn record_parked(
        &self,
        id: &ApprovalId,
        effect: &Effect,
        at_millis: u64,
        task: TaskLink,
        conversation: ApprovalConversation,
        cycle: Option<String>,
    ) -> Result<()> {
        let ApprovalConversation { thread, parent } = conversation;
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            state.origins.insert(
                id.clone(),
                ApprovalOrigin {
                    at_millis,
                    kind: effect.kind.clone(),
                    task: Some(task.clone()),
                    run_id: effect.run_id.clone(),
                    thread: thread.clone(),
                    parent,
                    cycle: cycle.clone(),
                },
            );
            state.parked.insert(
                id.clone(),
                ParkedApproval {
                    effect: effect.clone(),
                    at_millis,
                    // A fresh park's deadline runs from when it was parked.
                    deadline_anchor_millis: at_millis,
                    task: Some(task.clone()),
                    thread: thread.clone(),
                    cycle: cycle.clone(),
                },
            );
            state.retain_approval_effect(id, effect);
        }
        self.append(&JournalRecord::ApprovalParked {
            id: id.clone(),
            effect: effect.clone(),
            at_millis,
            task: Some(task),
            thread,
            parent,
            cycle,
        })
        .await
    }

    /// The turn key of every approval **still parked**, one entry per approval
    /// (issue #469).
    ///
    /// Read once, at recovery, to re-arm the
    /// [`ContinuationQueue`](crate::runtime::continuation::ContinuationQueue):
    /// a restart in the middle of a partly-decided turn must come back still
    /// knowing that turn is blocked, or its continuation would either fire early
    /// or never fire at all. Approvals with no turn key (pre-#469 lines) are
    /// omitted — they continue on their own and are never gated.
    pub fn parked_turns(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .parked
            .values()
            .filter_map(|p| p.cycle.clone())
            .collect()
    }

    /// Every blocked agent-node stash still awaiting re-dispatch, as
    /// `(turn, workflow_id, input, started_by)` (issue #1816, Stage 2; the
    /// `started_by` field added for issue #1862's prerequisite).
    ///
    /// The builder folds this at boot into the live
    /// [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue)
    /// (via [`rearm`](crate::runtime::blocked_nodes::BlockedNodeQueue::rearm)) so
    /// an approval landing after a restart finds the run to continue, the way
    /// [`pending`](Self::pending) feeds the gate queue's re-arm. Only stashes
    /// whose paired [`BlockedNodeReleased`](JournalRecord::BlockedNodeReleased)
    /// has not replayed are returned — a re-dispatched run does not come back.
    pub fn blocked_stashes(&self) -> Vec<(String, String, Value, StartedBy)> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_stashes
            .iter()
            .map(|(turn, stash)| {
                (
                    turn.clone(),
                    stash.workflow_id.clone(),
                    stash.input.clone(),
                    stash.started_by.clone(),
                )
            })
            .collect()
    }

    /// Stashes a blocked agent node's continuation facts durably (issue #1816).
    ///
    /// Called from `HarnessAgentRunner::park_gated_calls` at park time, in
    /// lockstep with the in-memory
    /// [`BlockedNodeQueue::arm`](crate::runtime::blocked_nodes::BlockedNodeQueue::arm)
    /// (issue #1825, P1 second follow-up) — and again, redundantly, from the
    /// runner's block-settle pass, which called this first and alone until
    /// that follow-up. Both calls for one node carry the same `(turn,
    /// workflow_id, input)`, so once the append has actually landed the
    /// second call is a first-write-wins no-op rather than a second source of
    /// truth — the same tier `arm` itself skips at for the identical reason,
    /// now applied one durability level up so the settle pass's fallback call
    /// does not double every blocked node's `BlockedNodeStashed` write
    /// forever. Best-effort at the call site either way: a failed durable
    /// write leaves the in-memory stash serving the common (no-restart) case,
    /// exactly as a failed gate journal leaves its live queue in place.
    ///
    /// # Retrying a failed first append (issue #1825, P1 — found by
    /// chatgpt-codex-connector)
    ///
    /// The in-memory insert below lands before the append that durably backs
    /// it, so a transient failure on the park-time call still leaves `turn` in
    /// `blocked_stashes` — otherwise a resolve landing before the settle-time
    /// fallback would find no stash to release even though the in-memory arm
    /// (this call's sibling) says the node is blocked. But that same
    /// in-memory presence used to be read as "already durable": the
    /// settle-time fallback's call would see the entry, assume its own append
    /// was the redundant second write, and return without ever appending —
    /// so the durable record was never retried, and a restart landing before
    /// the run re-dispatches rehydrates nothing for an approval card that is
    /// still sitting there, clickable. [`BlockedStash::durable`] is what closes
    /// that: it is only set once an append for this stash has actually
    /// returned `Ok`, so a call that finds the turn present but not yet
    /// durable retries the append instead of skipping it.
    pub async fn record_blocked_node_stashed(
        &self,
        turn: &str,
        workflow_id: &str,
        input: &Value,
        started_by: &StartedBy,
    ) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            match state.blocked_stashes.get(turn) {
                Some(existing) if existing.durable => {
                    // Already durably recorded — either this run's own
                    // park-time write already landed and the settle pass is
                    // the redundant call, or a retry of this same call raced
                    // itself. Either way the facts are identical (one node
                    // parks under one turn), so a second durable append would
                    // only double the flush for no new information.
                    return Ok(());
                }
                Some(_) => {
                    // In memory, but its first durable append never landed —
                    // fall through and retry below instead of returning early
                    // and leaving the in-memory state misrepresent durability
                    // forever.
                }
                None => {
                    state.blocked_stashes.insert(
                        turn.to_string(),
                        BlockedStash {
                            workflow_id: workflow_id.to_string(),
                            input: input.clone(),
                            started_by: started_by.clone(),
                            durable: false,
                        },
                    );
                }
            }
        }
        self.append(&JournalRecord::BlockedNodeStashed {
            turn: turn.to_string(),
            workflow_id: workflow_id.to_string(),
            input: input.clone(),
            started_by: started_by.clone(),
            at_millis: crate::ports::now_millis(),
        })
        .await?;
        // Reached only once the append actually landed. A concurrent release
        // (the turn resolved and retired between the block above and here)
        // leaves nothing for `and_modify` to touch — correctly: there is no
        // stash left to mark durable, and none should be resurrected here.
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_stashes
            .entry(turn.to_string())
            .and_modify(|stash| stash.durable = true);
        Ok(())
    }

    /// Retires a blocked-node stash once its run has re-dispatched (or its block
    /// was wholly refused), so a later boot does not rehydrate it (issue #1816).
    ///
    /// Also retires the turn from `blocked_node_approvals`, mirroring what
    /// replaying this same record does in [`replay`](Self::replay) — the
    /// doc-stated invariant on that field is that a turn never lingers there
    /// past the continuation it describes. Without this, a live release left
    /// the turn banked in that set for the rest of the process's life: a
    /// long-running tenant would accumulate one stale key per completed
    /// block, invisible until the next full reload replayed the same record
    /// correctly.
    ///
    /// `blocked_node_dispatched` is the one exception — deliberately left
    /// standing here, matching [`replay`](Self::replay)'s own fold. See that
    /// field's doc comment (finding `3877914597`) for why a live release
    /// clearing its own dispatch tombstone reopens the exact ghost-redispatch
    /// gap issue #1825 exists to close.
    pub async fn record_blocked_node_released(&self, turn: &str) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            state.blocked_stashes.remove(turn);
            state.blocked_node_approvals.remove(turn);
        }
        self.append(&JournalRecord::BlockedNodeReleased {
            turn: turn.to_string(),
        })
        .await
    }

    /// Every blocked-node turn durably known to have at least one approved
    /// decision banked (issue #1816).
    ///
    /// The builder folds this at boot into the live
    /// [`BlockedNodeQueue`](crate::runtime::blocked_nodes::BlockedNodeQueue)
    /// (via [`mark_approved`](crate::runtime::blocked_nodes::BlockedNodeQueue::mark_approved),
    /// once per turn) alongside [`blocked_stashes`](Self::blocked_stashes), so a
    /// restart that landed between an approval and the node's last decision
    /// still knows that approval happened when the last one lands.
    pub fn blocked_node_approvals(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_node_approvals
            .iter()
            .cloned()
            .collect()
    }

    /// Records durably that a blocked agent node's turn has at least one
    /// approved decision, the moment that decision lands (issue #1816).
    ///
    /// Called beside [`ContinuationQueue::decide`](crate::runtime::continuation::ContinuationQueue::decide),
    /// not deferred to the turn's release — the whole point is to survive a
    /// restart that lands on a decision that is not the turn's last, which is
    /// exactly the window release-time bookkeeping cannot cover. Idempotent by
    /// construction (a set insert), so a node whose second call is also
    /// approved writes this again harmlessly.
    pub async fn record_blocked_node_approved(&self, turn: &str) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_node_approvals
            .insert(turn.to_string());
        self.append(&JournalRecord::BlockedNodeApproved {
            turn: turn.to_string(),
        })
        .await
    }

    /// Every blocked-node turn durably known to have already had its
    /// continuation spawned once (issue #1825).
    ///
    /// [`CompanyRuntime::reconcile_stranded_blocked_nodes`](crate::company::runtime::CompanyRuntime::reconcile_stranded_blocked_nodes)
    /// checks this before re-spawning an approved-but-still-rehydrated stash:
    /// without it, a boot cannot tell "never dispatched" apart from "dispatched,
    /// but its `BlockedNodeReleased` write failed" — both leave the same
    /// `blocked_stashes` + `blocked_node_approvals` pair behind. See
    /// [`BlockedNodeDispatched`](JournalRecord::BlockedNodeDispatched) for the
    /// full reasoning.
    pub fn blocked_node_dispatched(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_node_dispatched
            .iter()
            .cloned()
            .collect()
    }

    /// Whether `turn`'s continuation has already been durably marked
    /// dispatched (issue #1825, finding `3877718169`).
    ///
    /// A single-lookup twin of [`blocked_node_dispatched`](Self::blocked_node_dispatched),
    /// for callers that only need one turn's membership rather than the whole
    /// set — [`CompanyRuntime::resume_blocked_agent_node`](crate::company::runtime::CompanyRuntime::resume_blocked_agent_node)'s
    /// own guard checks this on every live decision reaching a blocked node,
    /// not once per boot the way [`CompanyRuntime::reconcile_stranded_blocked_nodes`](crate::company::runtime::CompanyRuntime::reconcile_stranded_blocked_nodes)
    /// does, so cloning the full set on every call would be waste for no
    /// reason a `HashSet::contains` doesn't already avoid.
    pub fn is_blocked_node_dispatched(&self, turn: &str) -> bool {
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_node_dispatched
            .contains(turn)
    }

    /// Records durably that a blocked agent node's continuation has been
    /// spawned, the moment the spawn attempt actually succeeds (issue #1825).
    ///
    /// Called from [`CompanyRuntime::resume_blocked_agent_node`](crate::company::runtime::CompanyRuntime::resume_blocked_agent_node)'s
    /// `Ok(())` arm, **before** it calls `retire_blocked_stash` — so this
    /// record lands even when that call's own durable write later fails.
    /// Idempotent by construction (a set insert), matching
    /// [`record_blocked_node_approved`](Self::record_blocked_node_approved).
    pub async fn record_blocked_node_dispatched(&self, turn: &str) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .blocked_node_dispatched
            .insert(turn.to_string());
        self.append(&JournalRecord::BlockedNodeDispatched {
            turn: turn.to_string(),
        })
        .await
    }

    /// The turn that parked `id`, if it is one this journal recorded
    /// (issue #469).
    ///
    /// Two levels of absence, and they mean different things — the same shape
    /// [`approval_thread`](Self::approval_thread) uses. `None`: nothing was ever
    /// parked under this id. `Some(None)`: parked, by a line written before the
    /// turn key existed.
    pub fn approval_cycle(&self, id: &ApprovalId) -> Option<Option<String>> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .map(|o| o.cycle.clone())
    }

    /// The effect an approval was parked with, payload scrubbed (issue #351).
    ///
    /// Answers the grant-consumption path's question: the agent just redeemed
    /// this approval's grant and the tool ran — what was it, and was it one that
    /// cannot be taken back? Superseded by an approve-with-edit, since that is
    /// what the grant was minted against.
    pub fn approval_effect(&self, id: &ApprovalId) -> Option<Effect> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_effects
            .get(id)
            .cloned()
    }

    /// Whether replay read back an executed key it cannot describe (issue #351).
    ///
    /// Company-wide rather than per-task, and necessarily so: an undescribed
    /// record carries no card either, so there is nothing to attribute it to.
    /// The console's contract is that an empty
    /// [`irreversible_effects`](Self::irreversible_effects) means the journal
    /// holds nothing irreversible for a card — true only when this is `false`.
    /// When it is `true` the console confirms regardless and says the earlier
    /// activity cannot be described, instead of showing an all-clear it cannot
    /// stand behind.
    ///
    /// The related pre-#351 gap it does **not** detect on its own: an approval
    /// parked before the upgrade carries no `task_id`, so approving it
    /// afterwards executes an effect attributed to no card. That record is
    /// byte-identical to a legitimately card-less park written today, so
    /// flagging it would misreport every company that has ever parked an
    /// approval from operator chat. In practice a company old enough to hold a
    /// pre-#351 park also holds pre-#351 executed lines, so this flag is set and
    /// the same warning shows.
    pub fn has_undescribed_history(&self) -> bool {
        self.state
            .lock()
            .expect("journal state poisoned")
            .undescribed_executed
    }

    /// The irreversible effects this task has already executed, oldest first
    /// (issue #351).
    ///
    /// Drawn from the journal's own executed record — the same append-only set
    /// that makes effects at-most-once — rather than re-derived from timeline
    /// labels, which describe what an agent *said* and not what was committed.
    /// A direct index lookup, so a company's history length does not price a
    /// Task Detail read.
    pub fn irreversible_effects(&self, task_id: &str) -> Vec<ExecutedEffect> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .irreversible_by_task
            .get(task_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Records that a parked approval was resolved (removing it from the queue).
    pub async fn record_resolved(&self, id: &ApprovalId) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .parked
            .remove(id);
        self.append(&JournalRecord::ApprovalResolved { id: id.clone() })
            .await
    }

    /// Records that a parked approval expired to a default-deny, removing it
    /// from the queue. This is the durable audit entry for
    /// default-deny-on-silence.
    /// Drops the in-memory traces of a park whose durable line never landed
    /// (issue #1861).
    ///
    /// [`record_parked`](Self::record_parked) populates `origins`, `parked` and
    /// the retained effect *before* it appends, so a failing append leaves a
    /// live approval in the projection that no journal line will ever replay:
    /// present until this process exits, gone on the next boot. This removes
    /// the three entries and writes nothing — deliberately, since the caller is
    /// here precisely because the durable write is the thing that failed, and a
    /// compensating record would be a second write down the same broken path.
    ///
    /// **Not a retirement.** Nothing was durably parked, so there is nothing to
    /// retire and no default-deny to record; the caller reports the park as
    /// failed and its own path returns the card. Contrast
    /// [`CompanyRuntime::unpark_blocker`](crate::company::CompanyRuntime), which
    /// undoes a park that *did* land and therefore owes the full audit trail.
    pub fn discard_unrecorded_park(&self, id: &ApprovalId) {
        let mut state = self.state.lock().expect("journal state poisoned");
        state.parked.remove(id);
        state.origins.remove(id);
        state.approval_effects.remove(id);
    }

    pub async fn record_expired(
        &self,
        id: &ApprovalId,
        at_millis: u64,
        reason: ExpiryReason,
    ) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .parked
            .remove(id);
        self.append(&JournalRecord::ApprovalExpired {
            id: id.clone(),
            at_millis,
            reason,
        })
        .await
    }

    /// Records that an operator extended a parked approval's deadline, moving
    /// the live entry's TTL anchor to `at_millis` (issue #1805).
    ///
    /// Updates the in-memory queue so the very next `pending()` projects the new
    /// deadline, and appends the durable line so a redeploy replays the move. A
    /// no-op against the in-memory state for an id that is not parked — the
    /// caller (`CompanyRuntime::extend_approval`) has already asked the gate and
    /// refused with a 404 in that case, so this is only reached for a live entry.
    pub async fn record_extended(&self, id: &ApprovalId, at_millis: u64, by: Actor) -> Result<()> {
        if let Some(parked) = self
            .state
            .lock()
            .expect("journal state poisoned")
            .parked
            .get_mut(id)
        {
            parked.deadline_anchor_millis = at_millis;
        }
        self.append(&JournalRecord::ApprovalExtended {
            id: id.clone(),
            at_millis,
            by,
        })
        .await
    }

    /// Records an operator-amended approval (an approve-with-edit) for the audit
    /// trail. Removal from the queue is recorded separately by
    /// [`record_resolved`](Self::record_resolved).
    pub async fn record_amended(
        &self,
        id: &ApprovalId,
        amended_effect: &Effect,
        at_millis: u64,
    ) -> Result<()> {
        // The amendment supersedes the park as the description of what the
        // operator approved (issue #351) — a grant is minted against the
        // amended arguments, so an edited amount is the one to report.
        self.state
            .lock()
            .expect("journal state poisoned")
            .retain_approval_effect(id, amended_effect);
        self.append(&JournalRecord::ApprovalAmended {
            id: id.clone(),
            amended_effect: amended_effect.clone(),
            at_millis,
        })
        .await
    }

    /// A snapshot of what every approval ever parked *was*, keyed by
    /// [`ApprovalId`] — including approvals since resolved or expired.
    ///
    /// The read side joins this against the event log's
    /// [`ApprovalResolved`](crate::ports::CompanyEvent::ApprovalResolved) to
    /// recover how long an approval was waiting (issue #305) and which board
    /// task it belonged to (issue #333). Taken as one snapshot per request
    /// rather than per lookup, so a fold never holds the state lock while it
    /// works.
    pub fn approval_origins(&self) -> HashMap<ApprovalId, ApprovalOrigin> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .clone()
    }

    /// What one approval was when it parked, without cloning the whole
    /// [`origins`](State::origins) map.
    ///
    /// The read path resolves a bounded number of ids per request — the
    /// approval events on one page of the fold, plus the parked queue — so it
    /// takes this per id rather than a snapshot. [`approval_origins`] copies an
    /// index that grows with every approval ever parked and is never pruned, and
    /// the task-detail route is polled, so a snapshot there costs the whole
    /// history on every poll.
    ///
    /// [`approval_origins`]: Self::approval_origins
    pub fn approval_origin(&self, id: &ApprovalId) -> Option<ApprovalOrigin> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .cloned()
    }

    /// The task link recorded for one approval, without cloning the whole
    /// [`origins`](State::origins) map.
    ///
    /// The map is unbounded and never pruned (see [`ApprovalOrigin`]), so a
    /// caller that needs the link for a couple of known ids — every cycle does,
    /// via [`cycle_task_id`](crate::runtime::cycle) — must not pay a full clone
    /// per cycle to read them. `approval_origins` stays the right call for a
    /// fold that will look up an unknown number of ids.
    ///
    /// The outer `Option` is "no such approval"; the inner is a pre-#333 line.
    pub fn approval_task(&self, id: &ApprovalId) -> Option<Option<TaskLink>> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .map(|o| o.task.clone())
    }

    /// The chat thread recorded for one approval (issue #379), read the same
    /// per-id way as [`approval_task`](Self::approval_task) and for the same
    /// reason — the origins map is unbounded, and a cycle needs at most the
    /// couple of ids in its own batch.
    ///
    /// The outer `Option` is "no such approval"; the inner is "no conversation
    /// behind it" (which a pre-#379 line is indistinguishable from, by design).
    /// Reading it off the retained origin rather than the live queue is what
    /// makes it answerable *after* the approval resolved — the case
    /// [`cycle_thread_id`](crate::runtime::cycle) needs so a second sign-off
    /// re-parks in the channel the first one was asked in.
    pub fn approval_thread(&self, id: &ApprovalId) -> Option<Option<String>> {
        self.approval_conversation(id).map(|c| c.thread)
    }

    /// Where one approval was raised, channel **and** thread, in a single read
    /// (issue #435).
    ///
    /// One accessor rather than an `approval_thread` plus an `approval_parent`,
    /// deliberately. The two values are only meaningful together — a parent is
    /// a sequence number with no channel to resolve it against — and reading
    /// them separately would take the state lock twice, admitting a torn pair
    /// that names one approval's channel and another's thread. Nothing today
    /// mutates an origin after it is inserted, so that tear is currently
    /// unreachable; this keeps it unreachable by construction rather than by
    /// coincidence.
    ///
    /// `None` is "no such approval". A present [`ApprovalConversation`] may
    /// still hold `None` in either field, on the terms each documents.
    pub fn approval_conversation(&self, id: &ApprovalId) -> Option<ApprovalConversation> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .origins
            .get(id)
            .map(|o| ApprovalConversation {
                thread: o.thread.clone(),
                parent: o.parent,
            })
    }

    /// Records a minted single-use grant (issue #243).
    ///
    /// Called *before* the grant enters the live set, so the ordering failure
    /// mode is "recorded but not live" — which replay fixes — rather than "live
    /// but not recorded", which a crash would lose silently.
    pub async fn record_granted(&self, grant: &GrantedCall) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .insert(grant.approval_id.clone(), grant.clone());
        self.append(&JournalRecord::ApprovalGranted {
            grant: grant.clone(),
        })
        .await
    }

    /// Records a verdict-bearing explicit approval continuation before it is
    /// armed in memory.
    pub async fn record_approval_continuation(
        &self,
        continuation: &ApprovalContinuation,
    ) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_continuations
            .insert(continuation.call.approval_id.clone(), continuation.clone());
        self.append(&JournalRecord::ApprovalContinuationQueued {
            continuation: continuation.clone(),
        })
        .await
    }

    /// Records that an explicit approval continuation reached its agent.
    pub async fn record_approval_continuation_consumed(&self, id: &ApprovalId) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_continuations
            .remove(id);
        self.append(&JournalRecord::ApprovalContinuationConsumed { id: id.clone() })
            .await
    }

    /// Durably claims one explicit continuation before its agent turn starts.
    /// Replay removes a claimed continuation from the recovery queue, choosing
    /// a possibly missed follow-up over repeating an external action.
    pub async fn record_approval_continuation_dispatched(
        &self,
        id: &ApprovalId,
        at_millis: u64,
    ) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_continuations
            .remove(id);
        self.append(&JournalRecord::ApprovalContinuationDispatched {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Records that an explicit approval continuation expired undelivered.
    pub async fn record_approval_continuation_expired(
        &self,
        id: &ApprovalId,
        at_millis: u64,
    ) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_continuations
            .remove(id);
        self.append(&JournalRecord::ApprovalContinuationExpired {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Records that a grant was redeemed — the agent re-issued the call and the
    /// tool ran. Removes it from the replay set so a restart cannot re-arm it.
    ///
    /// `effect` describes what the redeemed call was (issue #351), so an
    /// operator-approved tool call reaches the retry warning at all. This is the
    /// grant path's only chance to be described: it is settled by minting a
    /// grant, not by `execute_effect_once`, so it writes no `EffectExecuted`
    /// line. `None` when the approval's parked effect is no longer recoverable
    /// — the redemption is still recorded, it simply contributes no warning.
    pub async fn record_grant_consumed(
        &self,
        id: &ApprovalId,
        effect: Option<ExecutedEffect>,
    ) -> Result<()> {
        {
            let mut state = self.state.lock().expect("journal state poisoned");
            state.grants.remove(id);
            if let Some(effect) = effect.clone() {
                state.index_executed(effect);
            }
        }
        self.append(&JournalRecord::GrantConsumed {
            id: id.clone(),
            effect,
        })
        .await
    }

    /// Records that a grant expired unredeemed. Same replay removal as
    /// consumption: the operator has been told it lapsed, so a restart must not
    /// quietly hand the agent the permission back.
    pub async fn record_grant_expired(&self, id: &ApprovalId, at_millis: u64) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .remove(id);
        self.append(&JournalRecord::GrantExpired {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Every grant still live according to the journal — what boot recovery
    /// seeds the in-memory [`GrantSet`](crate::runtime::grants::GrantSet) with.
    pub fn replayed_grants(&self) -> Vec<GrantedCall> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .grants
            .values()
            .cloned()
            .collect()
    }

    /// Explicit approval continuations still owed according to journal replay.
    pub fn replayed_approval_continuations(&self) -> Vec<ApprovalContinuation> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .approval_continuations
            .values()
            .cloned()
            .collect()
    }

    /// Records a minted standing grant (issue #374).
    ///
    /// Called *before* the grant enters the live set, so the ordering failure
    /// mode is "recorded but not live" — which replay fixes — rather than "live
    /// but not recorded", which would leave a permission nobody can see or
    /// revoke.
    pub async fn record_standing_granted(&self, grant: &StandingGrant) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .insert(grant.id.clone(), grant.clone());
        self.append(&JournalRecord::StandingGrantMinted {
            grant: grant.clone(),
        })
        .await
    }

    /// Records that the operator revoked a standing grant (issue #374).
    pub async fn record_standing_revoked(
        &self,
        id: &GrantId,
        by: Actor,
        at_millis: u64,
    ) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .remove(id);
        self.append(&JournalRecord::StandingGrantRevoked {
            id: id.clone(),
            by,
            at_millis,
        })
        .await
    }

    /// Records that a standing grant reached its deadline (issue #374).
    pub async fn record_standing_expired(&self, id: &GrantId, at_millis: u64) -> Result<()> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .remove(id);
        self.append(&JournalRecord::StandingGrantExpired {
            id: id.clone(),
            at_millis,
        })
        .await
    }

    /// Every standing grant still live according to the journal, with anything
    /// already past its deadline folded out (issue #374).
    ///
    /// The expiry filter matters beyond tidiness: the sweep only runs while the
    /// process is up, so a host that was down across a grant's deadline has no
    /// `StandingGrantExpired` line for it. Replaying on `at_millis` alone would
    /// hand a lapsed permission back to the live set, and a restart would be a
    /// way to resurrect one — the exact silent accumulation this issue forbids.
    pub fn replayed_standing_grants(&self, now_millis: u64) -> Vec<StandingGrant> {
        self.state
            .lock()
            .expect("journal state poisoned")
            .standing_grants
            .values()
            .filter(|g| g.is_live_at(now_millis))
            .cloned()
            .collect()
    }

    /// A snapshot of the currently parked approvals, oldest first.
    pub fn pending(&self) -> Vec<PendingApproval> {
        let state = self.state.lock().expect("journal state poisoned");
        let mut out: Vec<PendingApproval> = state
            .parked
            .iter()
            .map(|(id, parked)| PendingApproval {
                id: id.clone(),
                effect: parked.effect.clone(),
                at_millis: parked.at_millis,
                deadline_anchor_millis: parked.deadline_anchor_millis,
                task: parked.task.clone(),
                thread: parked.thread.clone(),
                // Issue #842: the turn key the entry already carries for #469's
                // continuation counter, read out rather than recomputed. The
                // two must name the same set — the batch the operator is asked
                // about in one card is precisely the batch the runtime holds a
                // single continuation for — and reading one field is how that
                // stays true without a rule anyone has to remember.
                batch: parked.cycle.clone(),
            })
            .collect();
        out.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.id.as_ref().cmp(b.id.as_ref()))
        });
        out
    }

    /// Appends one record, whole, and does not return until the sink has made it
    /// durable to the level the record asked for.
    ///
    /// **Durability is per record kind, by decision (issue #392).** Every record
    /// declares which failure it must outlast through
    /// [`JournalRecord::durability`], and this is the single choke point that
    /// passes that decision to the sink:
    ///
    /// * The three unconditional [`Durability::Host`] kinds — `EffectExecuted`,
    ///   `GrantConsumed` and `StandingGrantRevoked` — are on stable storage
    ///   before this returns. So the at-most-once contract holds against
    ///   **losing the machine** for precisely the records whose loss would
    ///   repeat an external action, and a failed flush fails the append — which
    ///   aborts `execute_effect_once` before `perform_effect` and so cannot
    ///   produce the duplicate it is guarding against.
    /// * `ApprovalParked` is [`Durability::Host`] **for a workflow gate only**
    ///   (issue #1145), and `Process` for every other park. It is the one kind
    ///   whose level is decided by its contents rather than by its tag, because
    ///   the reasoning behind `Process` is a property of the caller: a chat turn
    ///   re-enters its gate and re-parks, a paused workflow run has already
    ///   settled and never will. For that run the parked effect *is* the
    ///   continuation, so its loss strands the run permanently rather than
    ///   costing a second question.
    /// * The other nine are [`Durability::Process`]: killing the process cannot
    ///   lose them, a host crash can. That is the decision, not a gap left open.
    ///   Losing any of them makes the runtime **re-ask** — an approval is parked
    ///   again, an operator is prompted again, a cycle bracket reads as
    ///   interrupted — and never re-fire. Flushing them would tax the journal's
    ///   highest-volume records to protect against a re-asked question.
    ///
    /// What each backend does to honour the two levels is its own business and
    /// is documented on [`append_journal`](JournalStore::append_journal): an
    /// `O_APPEND` write with (or without) a `sync_data`, a sqlite commit under
    /// `synchronous=FULL` (or `NORMAL`), a mongodb insert with (or without)
    /// `j:true`.
    ///
    /// Issue #726 removed the bound this used to carry. The journal was
    /// constructed unconditionally on the filesystem, so a hosted tenant whose
    /// `/data` is ephemeral scratch did not keep its journal across a container
    /// replacement — let alone a host crash — and gained nothing from the flush.
    /// The sink now comes from the selected storage backend, so the flush is
    /// bought on a volume that outlives the container.
    ///
    /// The write lock is taken **around the store call**, not merely around the
    /// serialisation, and that is load-bearing: a backend that allocates a
    /// sequence number inside the append would otherwise be free to allocate two
    /// concurrent appends out of call order, and a park replayed after the
    /// resolution that drains it resurrects a resolved approval.
    async fn append(&self, record: &JournalRecord) -> Result<()> {
        let line = serde_json::to_string(record)?;
        let durability = record.durability();
        let _guard = self.write_lock.lock().await;
        self.store
            .append_journal(&self.company, &line, durability)
            .await
    }
}

/// Parses one journal line into the record or records it holds.
///
/// The healthy answer is one record. A line written by a pre-#386 host may hold
/// **two or more** with nothing between them, because `append` used to emit a
/// record and its newline as separate unflushed writes and the newline could
/// lose the race. `serde_json`'s stream deserializer reads concatenated values
/// natively, so such a line replays *in full* instead of being dropped — which
/// matters because dropping one would silently un-commit an `EffectExecuted`
/// key and let an at-most-once effect run a second time. Recovering the merge
/// is not a nicety; it is the difference between a cosmetic repair and a
/// duplicated payment.
///
/// A line that is truncated rather than merged — a crash partway through a
/// write, a filesystem that lost a tail — has no valid parse and is reported.
/// All-or-nothing per line: half a line applied is worse than none, because the
/// caller would have no way to know which half it got.
fn replay_line(line: &str) -> std::result::Result<Vec<JournalRecord>, String> {
    let single = match serde_json::from_str::<JournalRecord>(line) {
        Ok(record) => return Ok(vec![record]),
        Err(e) => e,
    };
    match serde_json::Deserializer::from_str(line)
        .into_iter::<JournalRecord>()
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(records) if !records.is_empty() => Ok(records),
        // Report the single-value error, not the stream one: it is the error
        // that describes the line as it was meant to be written.
        _ => Err(single.to_string()),
    }
}

impl std::fmt::Debug for RuntimeJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeJournal")
            .field("company", &self.company)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod test {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::ports::now_millis;
    use crate::ports::types::EffectGroup;

    fn effect() -> Effect {
        Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        }
    }

    /// A private directory for one test's journal file.
    ///
    /// The name comes from the OS, not from [`crate::ports::generate_id`] —
    /// minted ids are unique only within a process, so two test processes
    /// sharing `/tmp` could otherwise land on the same journal path and mix
    /// their records into one file. Since #386 that no longer produces an
    /// unparseable line, but it still produces a journal holding another
    /// test's history, which fails these assertions just as thoroughly.
    /// Dropping the returned handle removes the directory, including after a
    /// failed assert.
    fn tmp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-journal-")
            .tempdir()
            .expect("tempdir")
    }

    /// An executed effect as journaled (issue #351): irreversible, against
    /// `t-1`, unless a test says otherwise.
    fn executed(at_millis: u64) -> ExecutedEffect {
        ExecutedEffect {
            kind: "filing.submit".into(),
            amount_usd: None,
            task_id: Some("t-1".into()),
            at_millis,
            irreversible: true,
        }
    }

    #[tokio::test]
    async fn effect_key_commits_once_and_survives_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        assert!(!journal.is_executed("cyc:0"));
        journal.record_executed("cyc:0", executed(0)).await.unwrap();
        assert!(journal.is_executed("cyc:0"));
        // Re-committing the same key does not append a second record.
        journal.record_executed("cyc:0", executed(0)).await.unwrap();

        // A fresh journal over the same file (a restart) replays the commit.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.is_executed("cyc:0"));

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(raw.lines().filter(|l| !l.trim().is_empty()).count(), 1);

        // The re-commit is also what keeps the description list free of
        // duplicates: one key, one entry, however many times it is committed.
        assert_eq!(reloaded.irreversible_effects("t-1").len(), 1);
    }

    /// **Issue #726**: a journal over a non-filesystem store replays exactly
    /// what the same records replay over a file — and survives the loss of the
    /// bundle directory, which is the whole point.
    ///
    /// Every semantic decision (the record enum, replay, the parked queue, the
    /// grant sets) lives above the store, so this is what proves the split is
    /// real rather than merely stated: the same call sequence through two
    /// different sinks must produce identical state, and the sink that is not a
    /// file must still hold it after `/data` is gone.
    #[tokio::test]
    async fn a_journal_over_a_non_filesystem_store_replays_identically() {
        use crate::ports::journal::MemoryJournalStore;

        let company = CompanyId::new("acme");
        let store = Arc::new(MemoryJournalStore::default());

        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        // The same call sequence through both sinks: a committed key, a parked
        // approval, a minted grant, a resolution.
        let approval = ApprovalId::new("ap-1");
        for journal in [
            RuntimeJournal::with_store(store.clone(), company.clone()),
            RuntimeJournal::new(&path),
        ] {
            journal
                .record_executed("cyc:0", executed(1_000))
                .await
                .unwrap();
            journal
                .record_parked(
                    &approval,
                    &effect(),
                    2_000,
                    TaskLink::Task { id: "t-1".into() },
                    ApprovalConversation::default(),
                    None,
                )
                .await
                .unwrap();
            journal.record_granted(&grant("g-1", 3_000)).await.unwrap();
        }

        // The bundle directory is gone — a container replacement on a tenant
        // whose `/data` is ephemeral scratch. The file-backed journal loses
        // everything; the ported one loses nothing.
        drop(dir);

        let over_store = RuntimeJournal::with_store(store.clone(), company.clone());
        over_store.load().await.unwrap();
        let over_file = RuntimeJournal::new(&path);
        over_file.load().await.unwrap();

        assert!(
            over_store.is_executed("cyc:0"),
            "the at-most-once key must survive the loss of the data dir"
        );
        assert!(
            !over_file.is_executed("cyc:0"),
            "the file-backed journal is exactly what this issue is about: \
             losing /data un-commits every key"
        );
        assert_eq!(
            over_store.pending().len(),
            1,
            "the parked approval must survive too"
        );
        assert_eq!(over_store.pending()[0].id, approval, "with its original id");
        assert_eq!(
            over_store.replayed_grants().len(),
            1,
            "and so must a live grant"
        );
        assert!(over_store.corruption().is_empty());

        // Isolation: another company on the same store sees none of it.
        let other = RuntimeJournal::with_store(store, CompanyId::new("globex"));
        other.load().await.unwrap();
        assert!(!other.is_executed("cyc:0"));
        assert!(other.pending().is_empty());
    }

    /// **Issue #351**: the executed record says what ran, for which card, and
    /// whether it can be taken back — and survives a restart.
    #[tokio::test]
    async fn executed_effects_are_filtered_by_task_and_by_irreversibility() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        // This card's irreversible effect — the one a retry must name.
        journal
            .record_executed("cyc:0", executed(1_000))
            .await
            .unwrap();
        // The same card, but a read: it changed nothing, so it warns about
        // nothing.
        journal
            .record_executed(
                "cyc:1",
                ExecutedEffect {
                    kind: "web.search".into(),
                    irreversible: false,
                    ..executed(1_100)
                },
            )
            .await
            .unwrap();
        // Another card's payment. Irreversible, and none of this card's
        // business.
        journal
            .record_executed(
                "cyc:2",
                ExecutedEffect {
                    kind: "payment.send".into(),
                    amount_usd: Some(2_400.0),
                    task_id: Some("t-2".into()),
                    ..executed(1_200)
                },
            )
            .await
            .unwrap();
        // A workflow delivery: no card behind it at all.
        journal
            .record_executed(
                "cyc:3",
                ExecutedEffect {
                    task_id: None,
                    ..executed(1_300)
                },
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();

        let mine = reloaded.irreversible_effects("t-1");
        assert_eq!(mine.len(), 1, "{mine:?}");
        assert_eq!(mine[0].kind, "filing.submit");
        assert_eq!(mine[0].at_millis, 1_000);

        let theirs = reloaded.irreversible_effects("t-2");
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].amount_usd, Some(2_400.0));

        assert!(reloaded.irreversible_effects("t-never-ran").is_empty());
    }

    /// A journal line written before #351 carries a key and nothing else. It
    /// must still replay as an executed key — the at-most-once guarantee is not
    /// negotiable — and simply contribute no description.
    #[tokio::test]
    async fn a_pre_351_executed_line_still_replays_as_a_committed_key() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        tokio::fs::write(
            &path,
            "{\"record\":\"EffectExecuted\",\"key\":\"cyc-old:0\"}\n",
        )
        .await
        .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#351 line still replays");
        assert!(
            journal.is_executed("cyc-old:0"),
            "dropping the key would re-run an effect that already fired",
        );
        assert!(journal.irreversible_effects("t-1").is_empty());
        assert!(
            journal.has_undescribed_history(),
            "an empty list here is 'cannot say', not 'nothing happened'",
        );
    }

    /// The companion assertion: a journal whose every executed line carries a
    /// description reports no gap, so an empty list stays a genuine all-clear
    /// and Retry stays one click.
    #[tokio::test]
    async fn a_fully_described_journal_reports_no_undescribed_history() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        journal.record_executed("cyc:0", executed(0)).await.unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(!reloaded.has_undescribed_history());
    }

    /// **Issue #351**: an operator-approved *tool call* is settled by minting a
    /// grant, never by `execute_effect_once`, so redeeming that grant is the
    /// only line in the journal that can say the call fired. It must reach the
    /// same per-task read the native path does, and survive a restart.
    #[tokio::test]
    async fn a_redeemed_grant_names_what_it_did_against_its_card() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-tool");

        journal
            .record_parked(
                &id,
                &effect(),
                1_000,
                TaskLink::Task { id: "t-1".into() },
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal.record_resolved(&id).await.unwrap();
        journal
            .record_grant_consumed(
                &id,
                Some(ExecutedEffect {
                    kind: "composio_execute".into(),
                    amount_usd: Some(2_400.0),
                    ..executed(1_200)
                }),
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let named = reloaded.irreversible_effects("t-1");
        assert_eq!(named.len(), 1, "{named:?}");
        assert_eq!(named[0].kind, "composio_execute");
        assert_eq!(named[0].amount_usd, Some(2_400.0));
        assert!(
            reloaded.replayed_grants().is_empty(),
            "describing the redemption must not re-arm it",
        );
    }

    /// A redemption the runtime could not describe still journals, and simply
    /// contributes no warning — the same additive degradation a pre-#351
    /// `EffectExecuted` line has.
    #[tokio::test]
    async fn an_undescribed_redemption_still_journals_and_warns_about_nothing() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        journal
            .record_grant_consumed(&ApprovalId::new("appr-old"), None)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.irreversible_effects("t-1").is_empty());
    }

    /// Issue #351: the description a redeemed grant is built from comes off the
    /// park record, retained past resolution and **scrubbed of its payload** —
    /// this map outlives the queue entry, and the retry read never wants a
    /// recipient or a body.
    #[tokio::test]
    async fn an_approvals_effect_outlives_it_without_its_payload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-1");
        let parked = Effect {
            payload: serde_json::json!({ "to": "someone@example.com", "body": "secret" }),
            ..effect()
        };

        journal
            .record_parked(
                &id,
                &parked,
                1_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal.record_resolved(&id).await.unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();

        // Live and replayed must agree: a grant can be redeemed either side of
        // a restart.
        for from in [&journal, &reloaded] {
            let kept = from.approval_effect(&id).expect("retained past resolve");
            assert_eq!(kept.kind, "filing.submit");
            assert_eq!(kept.group, EffectGroup::Sign);
            assert_eq!(
                kept.payload,
                serde_json::Value::Null,
                "the payload must not be retained past the queue entry",
            );
        }
        assert_eq!(journal.approval_effect(&ApprovalId::new("never")), None);
    }

    /// An approve-with-edit supersedes the park: the grant is minted against the
    /// amended arguments, so the amended amount is the one to report.
    #[tokio::test]
    async fn an_amendment_supersedes_the_parked_effect_as_the_description() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-1");

        journal
            .record_parked(
                &id,
                &Effect {
                    amount_usd: Some(2_400.0),
                    ..effect()
                },
                1_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal
            .record_amended(
                &id,
                &Effect {
                    amount_usd: Some(400.0),
                    ..effect()
                },
                1_100,
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.approval_effect(&id).and_then(|e| e.amount_usd),
            Some(400.0),
            "reporting the pre-edit amount would name a payment nobody approved",
        );
    }

    #[tokio::test]
    async fn parked_approvals_rehydrate_and_resolve() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-1");
        journal
            .record_parked(
                &id,
                &effect(),
                now_millis(),
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(journal.pending().len(), 1);

        // Reload from disk: the parked approval comes back.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.pending().len(), 1);
        assert_eq!(reloaded.pending()[0].id, id);

        // Resolving removes it, and the removal is durable.
        reloaded.record_resolved(&id).await.unwrap();
        assert!(reloaded.pending().is_empty());

        let after = RuntimeJournal::new(&path);
        after.load().await.unwrap();
        assert!(after.pending().is_empty());
    }

    /// **Issue #333**: the board task an approval was parked for is carried on
    /// the record, survives a restart, and outlives the resolution.
    ///
    /// The whole point of the field is the *resolved* case — a task's Approvals
    /// tab has to say which sign-offs were its own long after they left the
    /// queue — so the origin assertion after `record_resolved` is the one that
    /// matters, not the pending one before it.
    #[tokio::test]
    async fn a_parked_approval_carries_its_task_across_a_restart_and_a_resolution() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let mine = ApprovalId::new("appr-mine");
        let theirs = ApprovalId::new("appr-theirs");
        let orphan = ApprovalId::new("appr-orphan");
        journal
            .record_parked(
                &mine,
                &effect(),
                1_000,
                TaskLink::Task { id: "t-1".into() },
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal
            .record_parked(
                &theirs,
                &effect(),
                1_100,
                TaskLink::Task { id: "t-2".into() },
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        // No card behind it (a workflow delivery, an operator-chat turn).
        journal
            .record_parked(
                &orphan,
                &effect(),
                1_200,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let pending = reloaded.pending();
        assert_eq!(
            pending
                .iter()
                .filter(|p| p.task.as_ref().and_then(TaskLink::task_id) == Some("t-1"))
                .count(),
            1,
            "the parked queue must name the task, not just the effect",
        );

        // The resolution drains the queue but must not drain the link.
        reloaded.record_resolved(&mine).await.unwrap();
        assert!(reloaded.pending().iter().all(|p| p.id != mine));
        let origins = reloaded.approval_origins();
        assert_eq!(
            origins.get(&mine),
            Some(&ApprovalOrigin {
                at_millis: 1_000,
                kind: "filing.submit".into(),
                task: Some(TaskLink::Task { id: "t-1".into() }),
                run_id: None,
                thread: None,
                parent: None,
                cycle: None,
            }),
        );
        assert_eq!(
            origins.get(&theirs).and_then(|o| o.task.clone()),
            Some(TaskLink::Task { id: "t-2".into() }),
            "a second task's approval keeps its own id, so neither absorbs the other",
        );
        // Recorded as deliberately unlinked — *not* as a missing link, which is
        // what tells the read side never to fall back to the run window for it.
        assert_eq!(
            origins.get(&orphan).and_then(|o| o.task.clone()),
            Some(TaskLink::Unlinked),
        );
    }

    /// A journal line written before #333 has no `task` key at all. It must
    /// replay with **no link** rather than failing to parse — and that absence
    /// is what the read side falls back to the old run-window correlation for.
    ///
    /// The distinction this pins is the one the whole feature rests on: a
    /// missing key replays as `None`, while a park this host recorded as having
    /// no card behind it replays as `Some(Unlinked)`. Both are "no task id",
    /// and they must not be confused.
    #[tokio::test]
    async fn a_pre_333_parked_line_replays_with_no_task() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let legacy = serde_json::json!({
            "record": "ApprovalParked",
            "id": "appr-legacy",
            "effect": effect(),
            "at_millis": 4_000,
        });
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#333 line still replays");
        let id = ApprovalId::new("appr-legacy");
        assert_eq!(journal.pending().len(), 1);
        assert_eq!(journal.pending()[0].task, None, "no key means no link");
        assert_eq!(
            journal.approval_origins().get(&id).map(|o| o.at_millis),
            Some(4_000),
        );
        assert_eq!(journal.approval_task(&id), Some(None));

        // A park this host records with no card behind it is a *different*
        // fact, written explicitly, and must not read back as the legacy shape.
        let fresh = ApprovalId::new("appr-new");
        journal
            .record_parked(
                &fresh,
                &effect(),
                5_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            journal.approval_task(&fresh),
            Some(Some(TaskLink::Unlinked))
        );

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let fresh_line = raw
            .lines()
            .find(|l| l.contains("appr-new"))
            .expect("the new park was appended");
        assert!(
            fresh_line.contains(r#""link":"unlinked""#),
            "an unlinked park must say so on disk: {fresh_line}",
        );
    }

    /// A park line written before #379 has no `thread` key. It must replay as
    /// "no thread" rather than failing to parse — which is what leaves every
    /// already-parked approval on the Approvals page and in no channel, exactly
    /// as it was before this shipped.
    ///
    /// The second half is the one that has to keep working after the resolution:
    /// the thread is read off the retained origin, so it survives the queue
    /// removal. That is what lets a follow-up cycle's own re-park stay in the
    /// channel the first sign-off was asked in.
    #[tokio::test]
    async fn a_pre_379_parked_line_replays_with_no_thread_and_a_stamped_one_survives_resolution() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let legacy = serde_json::json!({
            "record": "ApprovalParked",
            "id": "appr-legacy",
            "effect": effect(),
            "at_millis": 4_000,
            "task": { "link": "unlinked" },
        });
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#379 line still replays");
        let legacy_id = ApprovalId::new("appr-legacy");
        assert_eq!(journal.pending().len(), 1);
        assert_eq!(
            journal.pending()[0].thread,
            None,
            "no key means no conversation owns it",
        );
        assert_eq!(journal.approval_thread(&legacy_id), Some(None));
        assert_eq!(
            journal.approval_task(&legacy_id),
            Some(Some(TaskLink::Unlinked)),
            "the #333 link is untouched by the new field",
        );

        // A park stamped with the desk channel that produced it.
        let stamped = ApprovalId::new("appr-desk");
        journal
            .record_parked(
                &stamped,
                &effect(),
                5_000,
                TaskLink::Unlinked,
                ApprovalConversation {
                    thread: Some("desk-finance".to_string()),
                    parent: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            journal
                .pending()
                .iter()
                .find(|p| p.id == stamped)
                .unwrap()
                .thread,
            Some("desk-finance".to_string()),
        );

        // Resolving drains the queue but must not drain the origin thread —
        // the follow-up cycle reads it back from here.
        journal.record_resolved(&stamped).await.unwrap();
        assert!(journal.pending().iter().all(|p| p.id != stamped));
        assert_eq!(
            journal.approval_thread(&stamped),
            Some(Some("desk-finance".to_string())),
        );

        // And it round-trips through a reload, from the raw line.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let stamped_line = raw
            .lines()
            .find(|l| l.contains("appr-desk"))
            .expect("the stamped park was appended");
        assert!(
            stamped_line.contains(r#""thread":"desk-finance""#),
            "a thread-stamped park must say so on disk: {stamped_line}",
        );
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.approval_thread(&stamped),
            Some(Some("desk-finance".to_string())),
        );
        assert_eq!(reloaded.approval_thread(&legacy_id), Some(None));
    }

    /// Issue #435: the thread root survives a resolution and a reload on
    /// exactly the terms the channel does, and a line written before the field
    /// existed replays as "no thread" rather than failing to parse.
    ///
    /// The reload half is the one that matters. The continuation is journaled
    /// *after* the operator decides, so a restart between the two is an
    /// ordinary case, not an exotic one — and a root that did not survive it
    /// would silently drop the answer back into the channel.
    #[tokio::test]
    async fn a_thread_root_survives_resolution_and_reload_and_a_pre_435_line_replays() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        // A pre-#435 line: stamped with a channel, no `parent` key at all.
        let legacy = serde_json::json!({
            "record": "ApprovalParked",
            "id": "appr-pre435",
            "effect": effect(),
            "at_millis": 4_000,
            "task": { "link": "unlinked" },
            "thread": "desk-finance",
        });
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();
        let journal = RuntimeJournal::new(&path);
        journal.load().await.expect("a pre-#435 line still replays");
        let legacy_id = ApprovalId::new("appr-pre435");
        assert_eq!(
            journal.approval_conversation(&legacy_id),
            Some(ApprovalConversation {
                thread: Some("desk-finance".to_string()),
                parent: None,
            }),
            "the channel is untouched and the missing root reads as no thread",
        );

        // A park raised inside thread 7 of that channel.
        let threaded = ApprovalId::new("appr-threaded");
        journal
            .record_parked(
                &threaded,
                &effect(),
                5_000,
                TaskLink::Unlinked,
                ApprovalConversation {
                    thread: Some("desk-finance".to_string()),
                    parent: Some(EventSeq::new(7)),
                },
                None,
            )
            .await
            .unwrap();

        // Resolving drains the queue but must not drain the origin: the
        // continuation reads the root back from here, after the fact.
        journal.record_resolved(&threaded).await.unwrap();
        assert!(journal.pending().iter().all(|p| p.id != threaded));
        assert_eq!(
            journal.approval_conversation(&threaded),
            Some(ApprovalConversation {
                thread: Some("desk-finance".to_string()),
                parent: Some(EventSeq::new(7)),
            }),
        );

        // It is on disk, and it comes back from the raw line.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let line = raw
            .lines()
            .find(|l| l.contains("appr-threaded"))
            .expect("the threaded park was appended");
        assert!(
            line.contains(r#""parent":7"#),
            "a thread-rooted park must say so on disk: {line}",
        );
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.approval_conversation(&threaded),
            Some(ApprovalConversation {
                thread: Some("desk-finance".to_string()),
                parent: Some(EventSeq::new(7)),
            }),
        );
        assert_eq!(
            reloaded.approval_conversation(&legacy_id),
            Some(ApprovalConversation {
                thread: Some("desk-finance".to_string()),
                parent: None,
            }),
        );
        // And the #379 accessor still answers the question it always did.
        assert_eq!(
            reloaded.approval_thread(&threaded),
            Some(Some("desk-finance".to_string())),
        );
    }

    #[tokio::test]
    async fn expired_record_removes_parked_and_survives_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-exp");
        journal
            .record_parked(
                &id,
                &effect(),
                now_millis(),
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(journal.pending().len(), 1);

        journal
            .record_expired(&id, now_millis(), ExpiryReason::Ttl)
            .await
            .unwrap();
        assert!(journal.pending().is_empty());

        // A restart replays the expiry: the approval stays gone.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.pending().is_empty());

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalExpired"));
    }

    #[tokio::test]
    async fn amended_record_is_audit_only_and_round_trips() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let id = ApprovalId::new("appr-amend");
        journal
            .record_parked(
                &id,
                &effect(),
                now_millis(),
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();

        let mut amended = effect();
        amended.payload = serde_json::json!({ "edited": true });
        journal
            .record_amended(&id, &amended, now_millis())
            .await
            .unwrap();
        // The audit record alone does not drain the queue.
        assert_eq!(journal.pending().len(), 1);
        // The paired resolution removes it.
        journal.record_resolved(&id).await.unwrap();
        assert!(journal.pending().is_empty());

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.pending().is_empty());

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalAmended"));
        assert!(raw.contains("\"edited\":true"));
    }

    /// Issue #305: the park instant outlives the parked entry.
    ///
    /// Waiting time is only recoverable by joining a resolved approval back to
    /// when it parked, and the event log carries no park time. If the index were
    /// cleared alongside `parked` on resolve — the obvious symmetry — every
    /// *finished* wait would be unreadable, which is exactly the case the header
    /// needs. Expiry (the default-deny path) must retain it for the same reason.
    #[tokio::test]
    async fn approval_origins_outlive_resolution_and_expiry_and_survive_reload() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let resolved = ApprovalId::new("appr-resolved");
        let expired = ApprovalId::new("appr-expired");
        journal
            .record_parked(
                &resolved,
                &effect(),
                1_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal
            .record_parked(
                &expired,
                &effect(),
                2_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();

        journal.record_resolved(&resolved).await.unwrap();
        journal
            .record_expired(&expired, 9_000, ExpiryReason::Ttl)
            .await
            .unwrap();

        // Both left the queue...
        assert!(journal.pending().is_empty());
        // ...but their park instants are still joinable.
        let origins = journal.approval_origins();
        assert_eq!(origins.get(&resolved).map(|o| o.at_millis), Some(1_000));
        assert_eq!(origins.get(&expired).map(|o| o.at_millis), Some(2_000));

        // And a restart replays them out of the file, so history predating this
        // process is readable too.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let origins = reloaded.approval_origins();
        assert!(reloaded.pending().is_empty());
        assert_eq!(origins.get(&resolved).map(|o| o.at_millis), Some(1_000));
        assert_eq!(origins.get(&expired).map(|o| o.at_millis), Some(2_000));
    }

    // --- The cycle bracket (issue #390) ----------------------------------

    /// A cycle's bracket survives a restart as an *open* one, which is what the
    /// boot sweep then settles. Both halves in one test because neither is
    /// meaningful alone: an open bracket nobody settles is worse than no
    /// bracket — it reports a dead cycle as live work, forever.
    #[tokio::test]
    async fn an_interrupted_cycle_replays_as_open_and_is_swept_at_boot() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let journal = RuntimeJournal::new(&path);
        journal
            .record_cycle_started("cycle-1", "approval-continuation")
            .await
            .unwrap();
        journal
            .record_cycle_started("cycle-2", "operator-message")
            .await
            .unwrap();
        journal
            .record_cycle_finished("cycle-2", None)
            .await
            .unwrap();

        // A fresh journal over the same file: the host died with `cycle-1` open.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let open = reloaded.open_cycles();
        assert_eq!(open.len(), 1, "only the unfinished one replays as open");
        assert_eq!(open[0].cycle_id, "cycle-1");
        assert_eq!(open[0].trigger, "approval-continuation");

        // The sweep settles it, and a second boot finds nothing left to settle —
        // so a swept cycle cannot be settled twice into two contradictory
        // outcomes for one id.
        assert_eq!(reloaded.sweep_interrupted_cycles().await, 1);
        assert!(reloaded.open_cycles().is_empty());

        let after = RuntimeJournal::new(&path);
        after.load().await.unwrap();
        assert!(after.open_cycles().is_empty());
        assert_eq!(
            after.sweep_interrupted_cycles().await,
            0,
            "a second boot has nothing to settle"
        );
    }

    /// A cycle that fails still closes its bracket, carrying the reason — a
    /// failed cycle is not an open one, and an operator reading the bracket
    /// needs to tell "it broke" from "it never came back".
    #[tokio::test]
    async fn a_failed_cycle_closes_its_bracket_with_the_error() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_cycle_started("cycle-1", "approval-continuation")
            .await
            .unwrap();
        assert_eq!(journal.open_cycles().len(), 1);
        journal
            .record_cycle_finished("cycle-1", Some("the brain fell over".into()))
            .await
            .unwrap();
        assert!(
            journal.open_cycles().is_empty(),
            "a failure closes the bracket; only a host that never came back leaves it open"
        );

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(
            reloaded.open_cycles().is_empty(),
            "and it stays closed on replay"
        );
    }

    fn grant(id: &str, at_millis: u64) -> GrantedCall {
        GrantedCall {
            approval_id: ApprovalId::new(id),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: crate::policy::test_support::composio_send_args(),
            at_millis,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
        }
    }

    /// Issue #243: a grant minted before a restart is still redeemable after it.
    ///
    /// The window between "operator approved" and "agent re-issued the call"
    /// spans a model turn, so a deploy or crash inside it is ordinary. Without
    /// replay the operator's approval would evaporate and the agent would come
    /// back asking for the same permission it had just been given.
    #[tokio::test]
    async fn a_live_grant_replays_across_a_restart() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_granted(&grant("appr-1", 1_000))
            .await
            .unwrap();
        assert_eq!(journal.replayed_grants().len(), 1);

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let replayed = reloaded.replayed_grants();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].approval_id, ApprovalId::new("appr-1"));
        assert_eq!(replayed[0].agent, "finance");
        assert_eq!(replayed[0].tool, "composio_execute");
        assert_eq!(
            replayed[0].args,
            crate::policy::test_support::composio_send_args(),
            "the exact arguments the operator approved survive the restart"
        );
    }

    #[tokio::test]
    async fn a_denied_explicit_request_replays_only_as_a_verdict_continuation() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);
        let continuation = ApprovalContinuation {
            call: GrantedCall {
                tool: crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND.into(),
                ..grant("appr-denied", 1_000)
            },
            verdict: crate::ports::types::Verdict::Deny,
            by: Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "operator".into(),
            },
        };

        journal
            .record_approval_continuation(&continuation)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(
            reloaded.replayed_grants().is_empty(),
            "a denial must never replay as executable authority"
        );
        assert_eq!(
            reloaded.replayed_approval_continuations(),
            vec![continuation.clone()]
        );

        reloaded
            .record_approval_continuation_dispatched(&continuation.call.approval_id, 2_000)
            .await
            .unwrap();
        let after_dispatch = RuntimeJournal::new(&path);
        after_dispatch.load().await.unwrap();
        assert!(
            after_dispatch.replayed_approval_continuations().is_empty(),
            "a host-durable dispatch claim prevents restart from repeating the turn"
        );

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalContinuationQueued"));
        assert!(raw.contains("ApprovalContinuationDispatched"));
        assert!(!raw.contains("ApprovalGranted"));
    }

    /// The other half, and the one that actually matters for safety: a grant
    /// that already fired — or that lapsed and was announced as lapsed — must
    /// NOT come back on replay.
    ///
    /// A single-use grant resurrected by a restart is no longer single-use. The
    /// fold therefore *removes* on both terminal records, unlike `origins`
    /// (#305) which deliberately retains.
    #[tokio::test]
    async fn consumed_and_expired_grants_are_not_rehydrated() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_granted(&grant("consumed", 1_000))
            .await
            .unwrap();
        journal
            .record_granted(&grant("expired", 2_000))
            .await
            .unwrap();
        journal.record_granted(&grant("live", 3_000)).await.unwrap();
        assert_eq!(journal.replayed_grants().len(), 3);

        journal
            .record_grant_consumed(&ApprovalId::new("consumed"), None)
            .await
            .unwrap();
        journal
            .record_grant_expired(&ApprovalId::new("expired"), 9_000)
            .await
            .unwrap();

        let still_live: Vec<_> = journal.replayed_grants();
        assert_eq!(still_live.len(), 1);
        assert_eq!(still_live[0].approval_id, ApprovalId::new("live"));

        // And the removal is durable, not just in-memory.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let replayed = reloaded.replayed_grants();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].approval_id, ApprovalId::new("live"));

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("ApprovalGranted"));
        assert!(raw.contains("GrantConsumed"));
        assert!(raw.contains("GrantExpired"));
    }

    fn standing(id: &str, tool: &str, expires_at_millis: u64) -> StandingGrant {
        StandingGrant {
            id: GrantId::new(id),
            agent: "ops".into(),
            workflow: None,
            tool: tool.into(),
            verdict: crate::ports::types::Verdict::Approve,
            granted_by: Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "user-42".into(),
            },
            approval_id: ApprovalId::new(format!("appr-{id}")),
            at_millis: 1_000,
            expires_at_millis,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
            scope: None,
        }
    }

    /// Issue #374: a standing grant survives a restart, with its expiry and the
    /// operator who granted it intact.
    #[tokio::test]
    async fn a_standing_grant_replays_across_a_restart() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_standing_granted(&standing("g1", "shell", 100_000))
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let replayed = reloaded.replayed_standing_grants(2_000);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id, GrantId::new("g1"));
        assert_eq!(replayed[0].tool, "shell");
        assert_eq!(replayed[0].expires_at_millis, 100_000);
        assert_eq!(
            replayed[0].granted_by.id, "user-42",
            "who opened this tool up is the point of the record"
        );
    }

    /// Revoked, expired, and *silently lapsed* standing grants all stay gone.
    ///
    /// The third case is the one only replay can catch: the sweep runs while the
    /// process is up, so a host that was down across a deadline never wrote a
    /// `StandingGrantExpired` line. Replaying on the record alone would hand the
    /// permission back, making a restart a way to resurrect one.
    #[tokio::test]
    async fn revoked_expired_and_lapsed_standing_grants_are_not_rehydrated() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        for g in [
            standing("revoked", "shell", 100_000),
            standing("expired", "workspace_write", 100_000),
            standing("lapsed", "web_fetch", 3_000),
            standing("live", "shell", 100_000),
        ] {
            journal.record_standing_granted(&g).await.unwrap();
        }

        journal
            .record_standing_revoked(
                &GrantId::new("revoked"),
                Actor {
                    kind: crate::ports::types::ActorKind::User,
                    id: "user-42".into(),
                },
                5_000,
            )
            .await
            .unwrap();
        journal
            .record_standing_expired(&GrantId::new("expired"), 5_000)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        // `lapsed` has no terminal record at all — only its deadline stops it.
        let replayed = reloaded.replayed_standing_grants(10_000);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id, GrantId::new("live"));

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw.contains("StandingGrantMinted"));
        assert!(raw.contains("StandingGrantRevoked"));
        assert!(raw.contains("StandingGrantExpired"));
    }

    /// A journal written before #374 decodes unchanged, and replays no standing
    /// grants. The forward-only half — an old binary cannot read a new journal —
    /// is the same contract every prior variant addition made.
    #[tokio::test]
    async fn a_pre_374_journal_decodes_and_yields_no_standing_grants() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_parked(
                &ApprovalId::new("appr-old"),
                &effect(),
                500,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal
            .record_granted(&grant("appr-old", 1_000))
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.pending().len(), 1);
        assert_eq!(
            reloaded.replayed_grants().len(),
            1,
            "the single-use path replays byte-identically"
        );
        assert!(reloaded.replayed_standing_grants(2_000).is_empty());
    }

    /// Issue #1805: an `ApprovalExtended` line moves the deadline anchor, and the
    /// move replays on reload — so an operator's extension survives a redeploy
    /// rather than reverting to the original park window. The payload timestamp
    /// (`at_millis`, issue #1024) is deliberately left where it was: extending a
    /// deadline does not make the content fresher.
    #[tokio::test]
    async fn an_extension_replays_and_moves_only_the_deadline_anchor() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let id = ApprovalId::new("appr-extend");
        journal
            .record_parked(
                &id,
                &effect(),
                1_000,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        // A fresh park's anchor is the park instant.
        assert_eq!(journal.pending()[0].deadline_anchor_millis, 1_000);

        journal
            .record_extended(
                &id,
                9_000,
                crate::ports::types::Actor {
                    kind: crate::ports::types::ActorKind::User,
                    id: "operator".into(),
                },
            )
            .await
            .unwrap();
        // The live queue moved immediately.
        assert_eq!(journal.pending()[0].deadline_anchor_millis, 9_000);

        // And a reload replays the move rather than the bare park.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        let pending = reloaded.pending();
        assert_eq!(
            pending.len(),
            1,
            "the approval is still parked after reload"
        );
        assert_eq!(
            pending[0].deadline_anchor_millis, 9_000,
            "the extension survived the reload"
        );
        assert_eq!(
            pending[0].at_millis, 1_000,
            "the payload timestamp is untouched by an extension"
        );
    }

    /// The grant records must not disturb the approval-queue fold they share a
    /// file with — including #309's origin index, which the Task Detail
    /// waiting-time read joins against.
    #[tokio::test]
    async fn grant_records_leave_the_parked_queue_and_origins_intact() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let parked_id = ApprovalId::new("appr-parked");
        journal
            .record_parked(
                &parked_id,
                &effect(),
                500,
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        journal
            .record_granted(&grant("appr-granted", 1_000))
            .await
            .unwrap();
        journal
            .record_grant_consumed(&ApprovalId::new("appr-granted"), None)
            .await
            .unwrap();

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.pending().len(),
            1,
            "the parked approval is untouched"
        );
        assert_eq!(reloaded.pending()[0].id, parked_id);
        assert_eq!(
            reloaded
                .approval_origins()
                .get(&parked_id)
                .map(|o| o.at_millis),
            Some(500)
        );
        assert!(reloaded.replayed_grants().is_empty());
    }

    /// Every non-empty line of the journal at `path`, parsed. Panics with the
    /// offending line's number and text when one does not parse, because a
    /// torn line is exactly what these tests exist to catch and
    /// `unwrap`-on-`Err` hides which line it was.
    async fn parse_every_line(path: &Path) -> Vec<JournalRecord> {
        let raw = tokio::fs::read_to_string(path).await.expect("journal file");
        raw.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, line)| {
                serde_json::from_str::<JournalRecord>(line)
                    .unwrap_or_else(|e| panic!("line {} did not parse: {e}\n  {line}", i + 1))
            })
            .collect()
    }

    /// **Issue #386**: rapid appends through a *single* journal must not tear a
    /// line.
    ///
    /// This is the shape CI actually hit. `append` used to leave its trailing
    /// newline in a `tokio::fs::File` whose background write nobody awaited,
    /// then drop the handle and release the lock — so the next append's opening
    /// bytes could reach the file before the previous record's terminator, and
    /// two records landed on one line. One writer was enough; concurrency
    /// across instances was never required.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rapid_appends_through_one_journal_never_tear_a_line() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        const N: usize = 256;
        for i in 0..N {
            journal
                .record_executed(&format!("cyc:{i}"), executed(i as u64))
                .await
                .unwrap();
        }

        let records = parse_every_line(&path).await;
        assert_eq!(records.len(), N, "every append is its own line");

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        for i in 0..N {
            assert!(
                reloaded.is_executed(&format!("cyc:{i}")),
                "cyc:{i} must survive the reload",
            );
        }
    }

    /// **Issue #386**: a line an old host merged replays in full.
    ///
    /// This is the shape already sitting in journals written before the write
    /// fix, and the shape CI tripped over. It must not be *skipped*: dropping a
    /// merged line would un-commit an `EffectExecuted` key and let an
    /// at-most-once effect fire again, which is a worse outcome than the parse
    /// error it replaces.
    #[tokio::test]
    async fn a_merged_line_replays_every_record_it_holds() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let merged = format!(
            "{}{}",
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: "cyc:0".into(),
                effect: Some(executed(0)),
            })
            .unwrap(),
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: "cyc:1".into(),
                effect: Some(executed(1)),
            })
            .unwrap(),
        );
        let intact = serde_json::to_string(&JournalRecord::EffectExecuted {
            key: "cyc:2".into(),
            effect: Some(executed(2)),
        })
        .unwrap();
        tokio::fs::write(&path, format!("{merged}\n{intact}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("a merged line must not fail the load");
        for key in ["cyc:0", "cyc:1", "cyc:2"] {
            assert!(journal.is_executed(key), "{key} must replay");
        }
        assert!(
            journal.corruption().is_empty(),
            "a merged line is recovered, not lost, so it is not corruption",
        );
    }

    /// **Issue #386**: a truncated line is reported, and the records around it
    /// still replay.
    ///
    /// The old `load` returned `Err` here, which failed the company's boot: one
    /// unreadable line cost every readable one after it, plus the console an
    /// operator would need to repair the file.
    #[tokio::test]
    async fn a_truncated_line_is_reported_and_the_rest_still_replays() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let record = |key: &str, at| {
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: key.into(),
                effect: Some(executed(at)),
            })
            .unwrap()
        };
        let whole = record("cyc:1", 1);
        let truncated = &whole[..whole.len() / 2];
        tokio::fs::write(
            &path,
            format!(
                "{}\n{truncated}\n{}\n",
                record("cyc:0", 0),
                record("cyc:2", 2)
            ),
        )
        .await
        .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("one bad line must not fail the boot");

        assert!(journal.is_executed("cyc:0"), "the line before must replay");
        assert!(
            journal.is_executed("cyc:2"),
            "the lines after the damage are the ones the old load lost",
        );
        assert!(
            !journal.is_executed("cyc:1"),
            "the truncated record is gone"
        );

        let corruption = journal.corruption();
        assert_eq!(corruption.len(), 1, "exactly one line was unreadable");
        assert_eq!(corruption[0].line, 2, "the report must locate the line");
        assert_eq!(corruption[0].bytes, truncated.len());
        assert!(
            !corruption[0].message.contains("filing.submit"),
            "a corruption report must not quote the line's contents",
        );
    }

    /// **Issue #386**: a torn write can split a multi-byte codepoint, so the
    /// damaged line is not merely bad JSON — it is not valid UTF-8 at all.
    ///
    /// `load` used to `read_to_string`, which fails on the first invalid byte
    /// anywhere in the file. That turned exactly the damage this recovery path
    /// exists for into the whole-boot failure it exists to prevent, and no
    /// amount of per-line JSON handling downstream could have saved it. Raised
    /// in review of PR #389.
    #[tokio::test]
    async fn a_line_that_is_not_valid_utf8_is_skipped_like_any_other_damage() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        let record = |key: &str, at| {
            serde_json::to_string(&JournalRecord::EffectExecuted {
                key: key.into(),
                effect: Some(executed(at)),
            })
            .unwrap()
        };

        // A lone continuation byte: never valid on its own, which is what the
        // tail of a split codepoint looks like.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(record("cyc:0", 0).as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0x7b, 0x9f, 0x8d]);
        bytes.push(b'\n');
        bytes.extend_from_slice(record("cyc:2", 2).as_bytes());
        bytes.push(b'\n');
        tokio::fs::write(&path, &bytes).await.unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("invalid UTF-8 on one line must not fail the boot");

        assert!(journal.is_executed("cyc:0"), "the line before must replay");
        assert!(
            journal.is_executed("cyc:2"),
            "the lines after the damage must still replay",
        );

        let corruption = journal.corruption();
        assert_eq!(corruption.len(), 1, "exactly one line was unreadable");
        assert_eq!(corruption[0].line, 2, "the report must locate the line");
    }

    /// **Issue #386**: when `append` returns, the record is on the file.
    ///
    /// The deterministic half of the bug, and the more serious one. The
    /// at-most-once guarantee is that an effect's key is durable *before* the
    /// side effect runs; the old write path returned once the write was queued
    /// on tokio's blocking pool, so `record_executed` reported a commit that a
    /// crash could still lose and an `ENOSPC` on the real write reached nobody.
    /// Measured against that path, 199 of 200 appends failed this assertion —
    /// the torn line was the rare, visible symptom of a window that was open
    /// almost always.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_append_has_reached_the_file_before_it_returns() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        let mut expected = 0usize;
        for i in 0..64u64 {
            let key = format!("cyc:{i}");
            expected += serde_json::to_string(&JournalRecord::EffectExecuted {
                key: key.clone(),
                effect: Some(executed(i)),
            })
            .unwrap()
            .len()
                + 1;
            journal.record_executed(&key, executed(i)).await.unwrap();
            // A synchronous stat, so the assertion cannot be satisfied by the
            // very blocking pool that would still be running a queued write.
            let on_disk = std::fs::metadata(&path).expect("journal file").len() as usize;
            assert_eq!(
                on_disk,
                expected,
                "append #{} returned with {} of {expected} bytes on the file",
                i + 1,
                on_disk,
            );
        }
    }

    /// **Issue #386**: two journals over one path must not interleave.
    ///
    /// `write_lock` is per-instance, so it serialises nothing between two
    /// `RuntimeJournal` values sharing a file. Nothing in the type stops a
    /// caller building two, and the test suite builds them routinely. The
    /// defence is the process-wide per-path lock plus the single whole-line
    /// write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_from_two_journals_over_one_path_lose_nothing() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");

        const N: usize = 128;
        let one = Arc::new(RuntimeJournal::new(&path));
        let two = Arc::new(RuntimeJournal::new(&path));

        let a = tokio::spawn({
            let one = Arc::clone(&one);
            async move {
                for i in 0..N {
                    one.record_executed(&format!("a:{i}"), executed(i as u64))
                        .await
                        .unwrap();
                }
            }
        });
        let b = tokio::spawn({
            let two = Arc::clone(&two);
            async move {
                for i in 0..N {
                    two.record_executed(&format!("b:{i}"), executed(i as u64))
                        .await
                        .unwrap();
                }
            }
        });
        a.await.unwrap();
        b.await.unwrap();

        let records = parse_every_line(&path).await;
        assert_eq!(records.len(), N * 2, "no record may be lost or merged");

        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        for i in 0..N {
            assert!(reloaded.is_executed(&format!("a:{i}")), "a:{i} lost");
            assert!(reloaded.is_executed(&format!("b:{i}")), "b:{i} lost");
        }
    }

    /// **Old journal lines replay unchanged (issue #971).**
    ///
    /// Every `ApprovalExpired` written before the field existed was a TTL
    /// expiry, so the serde default is the truth about them. A missing default
    /// here would not be a cosmetic regression: replay is how the parked queue
    /// is rebuilt at boot, and a line that fails to parse leaves an approval
    /// resurrected that the host had already retired.
    #[test]
    fn a_pre_reason_expiry_line_replays_as_a_ttl_expiry() {
        let old_line = r#"{"record":"ApprovalExpired","id":"ap-old","at_millis":42}"#;
        let parsed: JournalRecord = serde_json::from_str(old_line).expect("old line must replay");
        match parsed {
            JournalRecord::ApprovalExpired {
                id,
                at_millis,
                reason,
            } => {
                assert_eq!(id.as_ref(), "ap-old");
                assert_eq!(at_millis, 42);
                assert_eq!(reason, ExpiryReason::Ttl);
            }
            other => panic!("expected ApprovalExpired, got {other:?}"),
        }

        // And a line written today carries the reason explicitly, so the two
        // are told apart by what is on the wire rather than by inference.
        let written = serde_json::to_string(&JournalRecord::ApprovalExpired {
            id: ApprovalId::new("ap-new"),
            at_millis: 43,
            reason: ExpiryReason::Ttl,
        })
        .expect("serialize");
        assert!(written.contains(r#""reason":"ttl""#), "{written}");
    }

    #[test]
    fn expired_and_amended_records_round_trip_under_record_tag() {
        for record in [
            JournalRecord::ApprovalExpired {
                id: ApprovalId::new("x"),
                at_millis: 42,
                reason: ExpiryReason::Ttl,
            },
            JournalRecord::ApprovalAmended {
                id: ApprovalId::new("y"),
                amended_effect: effect(),
                at_millis: 7,
            },
            JournalRecord::ApprovalGranted {
                grant: grant("z", 11),
            },
            JournalRecord::GrantConsumed {
                id: ApprovalId::new("z"),
                effect: None,
            },
            JournalRecord::GrantConsumed {
                id: ApprovalId::new("z2"),
                effect: Some(executed(21)),
            },
            JournalRecord::GrantExpired {
                id: ApprovalId::new("z"),
                at_millis: 13,
            },
        ] {
            let json = serde_json::to_value(&record).unwrap();
            assert!(json.get("record").is_some());
            let back: JournalRecord = serde_json::from_value(json).unwrap();
            // Re-serialize to compare (JournalRecord has no PartialEq).
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                serde_json::to_string(&record).unwrap()
            );
        }
    }

    /// One value of every [`JournalRecord`] variant (issue #392).
    ///
    /// Hand-built, so it carries its own completeness check below — the tag
    /// count. The *classification* needs no such guard: `durability`'s match is
    /// wildcard-free, so a new variant cannot compile until somebody decides
    /// which failure it must survive.
    fn every_record_kind() -> Vec<JournalRecord> {
        vec![
            JournalRecord::EffectExecuted {
                key: "k".into(),
                effect: Some(executed(1)),
            },
            JournalRecord::ApprovalParked {
                id: ApprovalId::new("a"),
                effect: effect(),
                at_millis: 1,
                task: Some(TaskLink::Unlinked),
                thread: None,
                parent: None,
                cycle: None,
            },
            JournalRecord::ApprovalResolved {
                id: ApprovalId::new("a"),
            },
            JournalRecord::ApprovalExpired {
                id: ApprovalId::new("a"),
                at_millis: 2,
                reason: ExpiryReason::Ttl,
            },
            JournalRecord::ApprovalAmended {
                id: ApprovalId::new("a"),
                amended_effect: effect(),
                at_millis: 3,
            },
            JournalRecord::ApprovalGranted {
                grant: grant("a", 4),
            },
            JournalRecord::ApprovalContinuationQueued {
                continuation: ApprovalContinuation {
                    call: grant("continuation", 4),
                    verdict: crate::ports::types::Verdict::Approve,
                    by: revoker(),
                },
            },
            JournalRecord::ApprovalContinuationDispatched {
                id: ApprovalId::new("continuation"),
                at_millis: 4,
            },
            JournalRecord::ApprovalContinuationConsumed {
                id: ApprovalId::new("continuation"),
            },
            JournalRecord::ApprovalContinuationExpired {
                id: ApprovalId::new("continuation"),
                at_millis: 5,
            },
            JournalRecord::GrantConsumed {
                id: ApprovalId::new("a"),
                effect: None,
            },
            JournalRecord::GrantExpired {
                id: ApprovalId::new("a"),
                at_millis: 5,
            },
            JournalRecord::StandingGrantMinted {
                grant: standing("s", "composio_execute", 9),
            },
            JournalRecord::StandingGrantRevoked {
                id: GrantId::new("s"),
                by: revoker(),
                at_millis: 6,
            },
            JournalRecord::StandingGrantExpired {
                id: GrantId::new("s"),
                at_millis: 7,
            },
            JournalRecord::CycleStarted {
                cycle_id: "c".into(),
                at_millis: 8,
                trigger: "test".into(),
            },
            JournalRecord::CycleFinished {
                cycle_id: "c".into(),
                at_millis: 9,
                error: None,
            },
            JournalRecord::BlockedNodeStashed {
                turn: "t".into(),
                workflow_id: "w".into(),
                input: serde_json::json!({}),
                started_by: StartedBy::Operator,
                at_millis: 10,
            },
            JournalRecord::BlockedNodeReleased { turn: "t".into() },
            JournalRecord::BlockedNodeApproved { turn: "t".into() },
            JournalRecord::BlockedNodeDispatched { turn: "t".into() },
        ]
    }

    /// The operator who takes a standing grant back.
    fn revoker() -> Actor {
        Actor {
            kind: crate::ports::types::ActorKind::User,
            id: "user-42".into(),
        }
    }

    /// A record's `record` tag — the same name the serialized line carries, so a
    /// failure names the variant rather than an index.
    fn record_tag(record: &JournalRecord) -> String {
        serde_json::to_value(record).unwrap()["record"]
            .as_str()
            .expect("every record is tagged")
            .to_string()
    }

    /// **Issue #392**: the host-durable set is a policy, and this pins it.
    ///
    /// The wildcard-free match in [`JournalRecord::durability`] already makes
    /// *completeness* a compile error — a new variant will not build until it is
    /// classified. What no compiler can catch is an existing kind being moved
    /// across the line: flipping `EffectExecuted` to `Process` compiles, passes
    /// every other test in this file, and silently gives up the one guarantee
    /// the journal exists for. This is the test that notices.
    ///
    /// `every_record_kind`'s `ApprovalParked` sample carries an ordinary effect,
    /// so it belongs on the `Process` side here. Its workflow-gate arm is the
    /// one kind whose level depends on contents rather than tag, and it is
    /// pinned separately below (issue #1145) — deliberately not by loosening
    /// this list, which is the assertion that would have stopped noticing.
    #[test]
    fn host_durable_kinds_are_exactly_the_nine_that_protect_approval_work() {
        let all = every_record_kind();
        let tags: HashSet<String> = all.iter().map(record_tag).collect();
        assert_eq!(
            tags.len(),
            21,
            "every JournalRecord variant must appear once in every_record_kind"
        );

        let mut host: Vec<String> = all
            .iter()
            .filter(|record| record.durability() == Durability::Host)
            .map(record_tag)
            .collect();
        host.sort();
        assert_eq!(
            host,
            vec![
                "ApprovalContinuationDispatched".to_string(),
                "ApprovalContinuationQueued".to_string(),
                "BlockedNodeApproved".to_string(),
                "BlockedNodeDispatched".to_string(),
                "BlockedNodeReleased".to_string(),
                "BlockedNodeStashed".to_string(),
                "EffectExecuted".to_string(),
                "GrantConsumed".to_string(),
                "StandingGrantRevoked".to_string()
            ],
            "the host-durable set is these nine kinds and nothing else; \
             widening it taxes the hot path, narrowing it lets an effect duplicate, \
             a spent grant re-arm, an explicit follow-up repeat, or a blocked node's \
             stash/approval/dispatch survive a process restart but not the host crash it also \
             promises to survive"
        );
    }

    /// One `ApprovalParked` line, built from the given effect kind.
    fn parked_with_kind(kind: &str) -> JournalRecord {
        let mut effect = effect();
        effect.kind = kind.to_string();
        JournalRecord::ApprovalParked {
            id: ApprovalId::new("a"),
            effect,
            at_millis: 1,
            task: Some(TaskLink::Unlinked),
            thread: None,
            parent: None,
            cycle: None,
        }
    }

    /// **Issue #1145.** A workflow gate's park is host-durable; every other park
    /// is not.
    ///
    /// Both arms in one test because the assertion *is* the distinction. The
    /// `Host` half alone would pass if every park were flushed — taxing the
    /// journal's approval path to fix one caller — and the `Process` half alone
    /// would pass on the code this replaces.
    ///
    /// Why the gate is different: a paused workflow run is *settled*, not
    /// suspended, so nothing re-enters the gate and the parked effect is the
    /// run's only continuation. A chat turn re-parks on its next attempt, which
    /// is why its park stays `Process` — and why the volume this record is
    /// written at is untouched.
    #[test]
    fn only_a_workflow_gate_park_is_host_durable() {
        assert_eq!(
            parked_with_kind(crate::runtime::WORKFLOW_APPROVE_KIND).durability(),
            Durability::Host,
            "a workflow gate's park is the run's only continuation — losing it \
             strands the run behind a question that exists nowhere, and nothing \
             re-parks it"
        );

        // The callers that do re-park, and the volume this record is written at.
        for kind in ["shell", "http.request", "message.send", "composio_execute"] {
            assert_eq!(
                parked_with_kind(kind).durability(),
                Durability::Process,
                "{kind} parks are re-asked on the next attempt; flushing them \
                 taxes the approval path for a question that comes back on its own"
            );
        }
    }

    /// **Issue #1825** (Codex finding `3866158654`). A blocked agent-node's
    /// gated tool call park is host-durable too — the same reasoning as
    /// `only_a_workflow_gate_park_is_host_durable` above, one caller down:
    /// `BlockedNodeStashed` durably carries the continuation, but nothing
    /// regenerates a lost `ApprovalParked` card, so a card lost to a host
    /// crash between park and approve strands the question forever with no
    /// re-park coming.
    ///
    /// Discriminated by `run_id` rather than `kind`, because a blocked node's
    /// park carries the tool's own name — it varies per call, unlike
    /// `WORKFLOW_APPROVE_KIND`. See `ApprovalRequestQueue::stamp_run`'s doc for
    /// why `run_id` is exactly the field a workflow-dispatched call has and a
    /// chat turn's own call does not.
    #[test]
    fn a_blocked_node_tool_call_park_is_host_durable_too() {
        let mut blocked_node_park = parked_with_kind("shell");
        let JournalRecord::ApprovalParked { effect, .. } = &mut blocked_node_park else {
            panic!("parked_with_kind always returns ApprovalParked");
        };
        effect.run_id = Some("run-1".to_string());
        assert_eq!(
            blocked_node_park.durability(),
            Durability::Host,
            "a blocked node's card is the only durable record that can put its \
             decision in front of the operator again — losing it strands the \
             stash `BlockedNodeStashed` keeps alive behind a question nobody can \
             answer"
        );

        // A chat turn's own gated call carries no `run_id` (`stamp_run` leaves
        // it `None`) and must not be swept up by the widened arm above — it
        // still re-parks on its own next attempt.
        assert_eq!(
            parked_with_kind("shell").durability(),
            Durability::Process,
            "a chat-turn park has no run_id and re-asks on its own"
        );
    }

    /// **Issue #392**: a host-durable record's append really does take the
    /// flushing path, and a process-durable one really does not.
    ///
    /// What this proves and what it does not. It proves the **policy** (which
    /// kinds route where) and the **plumbing** (the flush was requested and its
    /// syscall returned `Ok` — the append would have failed otherwise), plus
    /// that the record is readable back afterwards. It does **not** prove the
    /// bytes reached stable storage, and no portable unit test can: a synced and
    /// an unsynced line are byte-identical on disk, and `SIGKILL` does not drop
    /// the page cache, so no process-level test can simulate host loss. Proving
    /// that needs a crash-consistency harness — dm-log-writes, or a VM whose
    /// unsynced page cache is dropped — which this repo does not have. The OS
    /// contract carries the rest.
    #[tokio::test]
    async fn host_records_route_through_the_durable_append() {
        use crate::store::fs::append_probe;

        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal.record_cycle_started("c-1", "test").await.unwrap();
        assert_eq!(
            append_probe::counts(&path),
            (1, 0),
            "CycleStarted is process-durable and must not flush"
        );

        journal.record_executed("k-1", executed(1)).await.unwrap();
        assert_eq!(
            append_probe::counts(&path),
            (1, 1),
            "EffectExecuted must flush before its side effect runs"
        );

        journal
            .record_standing_revoked(&GrantId::new("s-1"), revoker(), 6)
            .await
            .unwrap();
        assert_eq!(
            append_probe::counts(&path),
            (1, 2),
            "StandingGrantRevoked must flush"
        );

        journal
            .record_grant_consumed(&ApprovalId::new("appr-1"), None)
            .await
            .unwrap();
        assert_eq!(
            append_probe::counts(&path),
            (1, 3),
            "GrantConsumed must flush: losing it re-arms a grant whose tool ran"
        );

        journal.record_cycle_finished("c-1", None).await.unwrap();
        assert_eq!(
            append_probe::counts(&path),
            (2, 3),
            "CycleFinished is process-durable and must not flush"
        );

        // The durable path must leave a journal that still replays.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(reloaded.is_executed("k-1"), "the flushed commit replays");
    }

    /// **Issue #392**: a host-durable record creates its journal's parent chain
    /// durably too.
    ///
    /// The wiring half of `create_dir_all_durable`. A journal's *first* append
    /// is the one that creates the directories, and it is also the one most
    /// likely to be an `EffectExecuted` commit. Reaching for the plain
    /// `create_dir_all` there would flush the record under ancestors that were
    /// not flushed, and a host crash takes the subtree — and the record with it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_host_record_flushes_the_directories_its_journal_creates() {
        use crate::store::fs::append_probe;

        let dir = tmp_dir();
        let companies = dir.path().join("companies");
        let home = companies.join("acme");
        let journal = RuntimeJournal::new(home.join("journal.jsonl"));

        journal.record_executed("k-1", executed(1)).await.unwrap();

        for (created, holder) in [("companies", dir.path()), ("acme", &companies)] {
            assert!(
                append_probe::dir_syncs(holder) > 0,
                "the directory holding the entry naming `{created}` must be flushed \
                 before a host-durable record is reported durable"
            );
        }
        assert!(
            append_probe::dir_syncs(&home) > 0,
            "the journal file's own directory entry must be flushed"
        );
    }

    /// **Issue #392**: a commit whose append fails must release its key, so the
    /// next attempt under that key is a real retry.
    ///
    /// Holding the key would make the failure silent in the worst way. The
    /// append error aborts `execute_effect_once` before `perform_effect`, so
    /// nothing external ran; a later attempt would then find the key present,
    /// take the already-committed early return, and report `Ok` for an effect
    /// that never happened and never will.
    ///
    /// The fault is injected by putting a **directory** where the journal file
    /// belongs: `append`'s `create_dir_all` on the parent still succeeds, and
    /// the append's own `open` then fails with `EISDIR`. A real failure of the
    /// real `append_line_durable`, with no production seam added to stage it.
    #[tokio::test]
    async fn a_failed_commit_releases_the_key_for_a_retry() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        tokio::fs::create_dir_all(&path).await.unwrap();
        let journal = RuntimeJournal::new(&path);

        let first = journal.record_executed("k-1", executed(1)).await;
        assert!(
            first.is_err(),
            "the append cannot land on a path occupied by a directory"
        );
        assert!(
            !journal.is_executed("k-1"),
            "a key whose commit never reached the file must not be held: \
             the effect did not run, so the key must not claim it did"
        );

        let retry = journal.record_executed("k-1", executed(1)).await;
        assert!(
            retry.is_err(),
            "the retry must re-attempt the commit and surface the failure again, \
             never take the already-committed early return and report success"
        );
    }

    /// A live release must retire a turn from `blocked_node_approvals` exactly
    /// as replaying its `BlockedNodeReleased` record does — the doc-stated
    /// invariant on the field itself: "a turn never lingers here past the
    /// continuation it describes."
    ///
    /// `record_blocked_node_released` used to remove only from
    /// `blocked_stashes`, leaving the turn's entry in
    /// `blocked_node_approvals` for the rest of the process's life — a
    /// long-running tenant that parks and approves many blocked nodes
    /// accumulates one stale key per release, unlike a fresh reload, which
    /// (via `replay`) always retires both together. Calling the live method
    /// directly, with no restart in between, isolates exactly that gap.
    #[tokio::test]
    async fn release_retires_the_turn_from_blocked_node_approvals_live() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_blocked_node_stashed(
                "turn-1",
                "wf-1",
                &serde_json::json!({}),
                &StartedBy::Operator,
            )
            .await
            .unwrap();
        journal
            .record_blocked_node_approved("turn-1")
            .await
            .unwrap();
        assert_eq!(journal.blocked_node_approvals(), vec!["turn-1".to_string()]);

        journal
            .record_blocked_node_released("turn-1")
            .await
            .unwrap();
        assert!(
            journal.blocked_node_approvals().is_empty(),
            "a released turn must not linger in the live approval set — it \
             must be retired the moment release lands, not only on the next \
             reload's replay"
        );
    }

    /// The mirror image of the test above: unlike `blocked_node_approvals`,
    /// a live release must **not** retire the turn from
    /// `blocked_node_dispatched` (finding `3877914597`).
    ///
    /// `resume_blocked_agent_node`'s guard against a ghost decision (issue
    /// #1825, finding `3877718169`) reads only this set to tell "already
    /// dispatched" apart from "genuinely nothing left on this host". A ghost
    /// `ApprovalResolved` can reach that guard *after* the turn's own
    /// continuation already ran to completion and its `BlockedNodeReleased`
    /// already landed — a host crash loses only the process-durable
    /// resolution while the host-durable park, approve, and dispatch facts
    /// all survive. If release cleared this set too (as it used to, mirroring
    /// `blocked_node_approvals` above), the guard would read `false` for
    /// exactly that case and the ghost would fall through into the "no stash
    /// on this host" branch, which tells the operator to re-run the workflow
    /// by hand — manually repeating the tool call the continuation already
    /// ran once. Proven live and across a reload, since the guard's only
    /// caller reads a freshly-replayed journal at boot.
    #[tokio::test]
    async fn release_does_not_retire_the_turn_from_blocked_node_dispatched_live() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let journal = RuntimeJournal::new(&path);

        journal
            .record_blocked_node_stashed(
                "turn-1",
                "wf-1",
                &serde_json::json!({}),
                &StartedBy::Operator,
            )
            .await
            .unwrap();
        journal
            .record_blocked_node_approved("turn-1")
            .await
            .unwrap();
        journal
            .record_blocked_node_dispatched("turn-1")
            .await
            .unwrap();
        assert!(journal.is_blocked_node_dispatched("turn-1"));

        journal
            .record_blocked_node_released("turn-1")
            .await
            .unwrap();
        assert!(
            journal.is_blocked_node_dispatched("turn-1"),
            "the dispatch tombstone must survive its own turn's release — a \
             ghost decision reaching this turn after release is exactly the \
             case issue #1825's guard exists to catch, and the guard reads \
             this set"
        );

        // A fresh reload's replay must fold the same record the same way.
        let reloaded = RuntimeJournal::new(&path);
        reloaded.load().await.unwrap();
        assert!(
            reloaded.is_blocked_node_dispatched("turn-1"),
            "replay must agree with the live path — the boot-time \
             reconciler and the live resume guard cannot disagree about \
             whether a turn was already dispatched"
        );
    }

    /// A [`JournalStore`] whose `append_journal` fails exactly once — the
    /// park-time write's transient failure — then passes every later append
    /// straight through to an in-memory backend, including a retry of the
    /// very same record. Mirrors `FailNJournalStore` in
    /// `blocked_node_continuation_test`, scoped down to this module's own
    /// unit tests via [`RuntimeJournal::with_store`].
    struct FailOnceJournalStore {
        inner: crate::ports::journal::MemoryJournalStore,
        failed: std::sync::atomic::AtomicBool,
    }

    impl FailOnceJournalStore {
        fn new() -> Self {
            Self {
                inner: crate::ports::journal::MemoryJournalStore::default(),
                failed: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl JournalStore for FailOnceJournalStore {
        async fn append_journal(
            &self,
            id: &CompanyId,
            line: &str,
            durability: Durability,
        ) -> crate::Result<()> {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::error::OpenCompanyError::Store(
                    "FailOnceJournalStore: forced failure on the first append".to_string(),
                ));
            }
            self.inner.append_journal(id, line, durability).await
        }

        async fn read_journal(&self, id: &CompanyId) -> crate::Result<Vec<String>> {
            self.inner.read_journal(id).await
        }

        async fn journal_imported(&self, id: &CompanyId) -> crate::Result<bool> {
            self.inner.journal_imported(id).await
        }

        async fn complete_import(&self, id: &CompanyId, lines: Vec<String>) -> crate::Result<()> {
            self.inner.complete_import(id, lines).await
        }
    }

    /// Issue #1825 (P1 — found by chatgpt-codex-connector): a park-time
    /// `record_blocked_node_stashed` whose first durable append fails
    /// transiently must have that append retried by the settle-time fallback
    /// call, not silently skipped.
    ///
    /// # The bug this reproduces
    ///
    /// The in-memory insert lands before the append that backs it durably, so
    /// the first (failing) call still leaves `turn` in `blocked_stashes` —
    /// that half is correct and load-bearing (a resolve landing before settle
    /// must still find an in-memory stash to release). But the early-return
    /// guard used to read that in-memory presence as proof the append had
    /// already landed: the settle-time fallback's call (the *same*
    /// `(turn, workflow_id, input)`, by construction) would see the entry,
    /// assume it was the redundant second write, and return `Ok(())` without
    /// ever appending. A restart between that skipped retry and the run
    /// re-dispatching then replays no `BlockedNodeStashed` line at all — the
    /// approval card is still there, clickable, but `BlockedNodeQueue::rearm`
    /// has nothing to rebuild the stash from.
    ///
    /// # Why this proves the fix
    ///
    /// `FailOnceJournalStore` fails only the very first append, so the second
    /// `record_blocked_node_stashed` call — standing in for the settle-time
    /// fallback — hits a store that is willing to succeed. Pre-fix, the
    /// early-return means that willingness is never tested: the assertion
    /// that a fresh reload actually replays the stash fails, because nothing
    /// durable was ever written. Post-fix the second call retries the append,
    /// it lands, and a reload rehydrates `turn-1`.
    #[tokio::test]
    async fn a_stash_whose_first_durable_append_failed_is_retried_and_lands() {
        let company = CompanyId::new("acme");
        let store = Arc::new(FailOnceJournalStore::new());
        let journal = RuntimeJournal::with_store(store.clone(), company.clone());
        let input = serde_json::json!({ "request": "quarterly numbers" });

        // Park time: the first append fails transiently. The in-memory stash
        // still has to be there for a fast resolve to release, so this must
        // not be lost even though the call itself reports an error.
        let first = journal
            .record_blocked_node_stashed("turn-1", "wf-1", &input, &StartedBy::Operator)
            .await;
        assert!(
            first.is_err(),
            "the forced first-append failure must surface, not be swallowed"
        );
        assert_eq!(
            journal.blocked_stashes(),
            vec![(
                "turn-1".to_string(),
                "wf-1".to_string(),
                input.clone(),
                StartedBy::Operator
            )],
            "the in-memory stash must still be there after a failed append — a resolve \
             landing before the next retry has to find it"
        );

        // Settle time: the fallback calls the same method with the same
        // facts. The store is willing to succeed now — the fix must actually
        // retry the append instead of taking the early return.
        let second = journal
            .record_blocked_node_stashed("turn-1", "wf-1", &input, &StartedBy::Operator)
            .await;
        assert!(
            second.is_ok(),
            "the retry must re-attempt the durable append rather than silently reporting \
             success off the in-memory presence alone: {second:?}"
        );

        // The proof that it actually landed, not merely that `Ok` came back:
        // a fresh journal over the same store replays the stash from the
        // durable line.
        let reloaded = RuntimeJournal::with_store(store, company);
        reloaded.load().await.unwrap();
        assert_eq!(
            reloaded.blocked_stashes(),
            vec![(
                "turn-1".to_string(),
                "wf-1".to_string(),
                input,
                StartedBy::Operator
            )],
            "a restart must rehydrate this stash — the retried append is the only durable \
             record of it, and pre-fix it was never written at all"
        );
    }

    /// Issue #1862 prerequisite (`CodeRabbit`, comment `3879554180`; also
    /// raised by `chatgpt-codex-connector`, comment `3879402310`): a
    /// `BlockedNodeStashed` line written before this issue added `started_by`
    /// to the record must still replay — `#[serde(default)]` is what makes
    /// that true, degrading to `Operator` rather than failing the whole
    /// journal load, the same fallback the pre-#1862 code path always used.
    #[tokio::test]
    async fn a_pre_1862_blocked_node_stashed_line_replays_as_operator() {
        let dir = tmp_dir();
        let path = dir.path().join("journal.jsonl");
        let legacy = serde_json::json!({
            "record": "BlockedNodeStashed",
            "turn": "turn-legacy",
            "workflow_id": "wf-legacy",
            "input": { "request": "before #1862" },
            "at_millis": 1_000,
        });
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();

        let journal = RuntimeJournal::new(&path);
        journal
            .load()
            .await
            .expect("a pre-#1862 line with no started_by field still replays");

        let stashes = journal.blocked_stashes();
        assert_eq!(stashes.len(), 1);
        let (turn, workflow_id, input, started_by) = &stashes[0];
        assert_eq!(turn, "turn-legacy");
        assert_eq!(workflow_id, "wf-legacy");
        assert_eq!(input, &serde_json::json!({ "request": "before #1862" }));
        assert_eq!(
            started_by,
            &StartedBy::Operator,
            "a legacy line with no started_by field degrades to Operator, the coarse \
             pre-#1862 fallback — not a load failure"
        );
    }
}
