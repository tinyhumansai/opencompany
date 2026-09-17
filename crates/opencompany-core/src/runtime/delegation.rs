//! Brain-agnostic delegation seam (issue #176, slice 1).
//!
//! Delegation — opening board tasks, handing a turn to a desk's lead member, and
//! the CEO-relay hand-back — used to live wired directly into
//! [`HarnessBrain`](crate::harness::brain::HarnessBrain), hard-coupled to the
//! [`HarnessPool`](crate::harness::HarnessPool). This module lifts that
//! orchestration out behind the [`RunTurn`] trait so a later slice can give the
//! hosted Medulla brain the same delegation without duplicating it.
//!
//! Slice 1 is a behaviour-preserving refactor: the harness path stays
//! byte-for-byte equivalent — [`HarnessBrain`](crate::harness::brain::HarnessBrain)
//! now drives a [`DelegationRunner`] over a
//! [`HarnessRunTurn`](crate::harness::run_turn::HarnessRunTurn) (the trait impl
//! that re-attaches [`HarnessDeps`](crate::harness::HarnessDeps)).
//!
//! Compiled only under `feature = "openhuman"`: [`TurnOutcome`] and the
//! delegation queue the runner drains are harness types. Slice 2 will generalise
//! the trait's turn types so a non-harness brain can implement it.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::company::Policy;
use crate::company::steer::{
    InflightEntry, InflightKind, InflightRegistry, SteerAction, SteerControl,
};
use crate::harness::TurnOutcome;
use crate::harness::lifecycle::{self, TaskRunEnd};
use crate::harness::orchestrator::{self, Delegation, DelegationQueue};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::run_trace::RunTraceSink;
use crate::harness::workflow_refs::WorkflowRefQueue;
use crate::ports::tasks::{
    COLUMN_TODO, TaskOutput, TaskOutputAction, TaskOutputSource, TaskOutputWorkflow,
};
use crate::ports::types::{CompanyId, CompanyRecord, EventSeq, OutboundMessage, TurnStep};
use crate::ports::{TaskOrigin, TaskRecord, TaskStore, generate_id, now_millis};
use crate::runtime::assignee;
use crate::runtime::cycle::{
    BUILDER_ANNOTATION, OPEN_WORK_ANNOTATION, SETTLED_WORK_ANNOTATION, THREAD_INDEX_ANNOTATION,
};

/// One agent turn, abstracted so delegation orchestration never touches the
/// harness-specific [`HarnessDeps`](crate::harness::HarnessDeps).
///
/// The three methods mirror the [`HarnessPool`](crate::harness::HarnessPool)
/// turn-runners the harness brain used inline: a streamed operator/desk turn
/// ([`run`](RunTurn::run)), a steered streamed turn
/// ([`run_steered`](RunTurn::run_steered)), and a steered, un-streamed background
/// turn ([`run_steered_background`](RunTurn::run_steered_background)). The impl
/// re-attaches whatever dependencies the concrete runtime needs; the runner only
/// ever sees a [`TurnOutcome`].
/// Which conversation a chat turn belongs to: the channel, and the thread
/// within it (#1890).
///
/// One argument rather than two loose `Option`s beside each other. A bare
/// `Option<EventSeq>` next to a bare `Option<&str>` is exactly the shape
/// [`ChatTurn`](crate::server::operator) already documents as the hazard — a
/// mis-ordered pair that compiles and then answers into the wrong
/// conversation — and there is nothing in either type to catch it.
///
/// A `None` `thread_root` is not "no thread". It is the channel-level
/// conversation: the one every unparented line hangs in, which is every line
/// in a company that has never opened a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatTarget<'a> {
    /// The desk / channel the turn is addressed to; `None` is unaddressed and
    /// folds to the General desk downstream.
    pub chat_id: Option<&'a str>,
    /// The root message this turn's thread hangs off; `None` is the channel
    /// itself.
    pub thread_root: Option<EventSeq>,
    /// Whether this turn is seeded with the desk's recent chat history
    /// (issue #1840).
    ///
    /// True for every ordinary reply, and the reason it is a knob at all is the
    /// **hive-mind episode**: a deliberating turn is handed its own attributed,
    /// visibility-filtered transcript by the episode prompt, and the recent-chat
    /// seed would hand it the *same* desk lines a second time — unattributed,
    /// and in the assistant role, i.e. as though this member had said them.
    /// That is not a duplicate, it is two contradicted claims: a blind opening
    /// round stops being blind (a peer's position arrives through the seed
    /// however carefully the projection withheld it), and the prompt's
    /// attribution — the thing `^N` citations are read against — is undercut by
    /// a history that names nobody.
    pub history_seed: bool,
    /// The journal sequence of the operator message this turn is answering,
    /// when the turn is answering one at all.
    ///
    /// The identity of the turn's own line in the log, which lets a projection
    /// over that log tell this turn's message apart from every other by
    /// `stored.seq` rather than by comparing text. Two different messages can
    /// legitimately share a prefix, so text can only ever be a guess; a seq
    /// cannot be two events.
    ///
    /// `None` on every turn that is not answering a journaled operator message
    /// — a relay, a delegate's instruction, a dispatched card, a workflow node
    /// — because those run on prose the model wrote, which no log line
    /// corresponds to. A consumer falls back to whatever it did before this
    /// field existed.
    pub message_seq: Option<EventSeq>,
}

impl Default for ChatTarget<'_> {
    /// An unaddressed, unthreaded, **seeded** turn answering no journaled
    /// message — what every caller had before the seed was a knob.
    fn default() -> Self {
        Self {
            chat_id: None,
            thread_root: None,
            history_seed: true,
            message_seq: None,
        }
    }
}

impl<'a> ChatTarget<'a> {
    /// A turn posted straight into a channel — the shape every caller had
    /// before threads were part of the key.
    pub fn channel(chat_id: Option<&'a str>) -> Self {
        Self {
            chat_id,
            ..Self::default()
        }
    }

    /// A turn inside `chat_id`, in the thread rooted at `thread_root`.
    pub fn in_thread(chat_id: Option<&'a str>, thread_root: Option<EventSeq>) -> Self {
        Self {
            chat_id,
            thread_root,
            ..Self::default()
        }
    }

    /// A turn inside `chat_id` that brings **its own** transcript: the desk's
    /// recent history is not seeded onto it.
    ///
    /// The hive-mind episode's turn seam. See [`history_seed`](Self::history_seed).
    pub fn deliberating(chat_id: Option<&'a str>, thread_root: Option<EventSeq>) -> Self {
        Self {
            chat_id,
            thread_root,
            history_seed: false,
            ..Self::default()
        }
    }

    /// A dispatched card's turn, bound to the conversation that raised it —
    /// **for addressing only**.
    ///
    /// A dispatched turn used to name no conversation at all
    /// ([`default`](Self::default)), which was the honest answer while nothing
    /// downstream could use one: its steps go to the card's note, and streaming
    /// them anywhere risked misattributing to whatever thread most recently
    /// sent (#125). The card has always known its origin, so the answer was
    /// available — it simply had no consumer.
    ///
    /// It has one now. The console holds a working row for the originating
    /// thread while the attempt runs, and its live tool frames can route to
    /// that thread rather than by recency — the same fix
    /// [`LiveStream::Workflow`] made for a workflow node, which routes by run
    /// and node instead of by chat.
    ///
    /// **Not seeded**, which is the whole reason this is its own constructor
    /// rather than [`in_thread`](Self::in_thread): the dispatched turn brings
    /// its own instruction and the card's history, and pouring the originating
    /// thread's transcript on top would change what the agent reads, not merely
    /// where its frames go. One task can span several turns, and that is
    /// unchanged.
    pub fn dispatched_from(chat_id: Option<&'a str>, thread_root: Option<EventSeq>) -> Self {
        Self {
            chat_id,
            thread_root,
            history_seed: false,
            ..Self::default()
        }
    }

    /// Binds this target to the journaled operator message the turn answers.
    ///
    /// Separate from the constructors because it is true of exactly one turn
    /// in a delegation drain — the responder's own — while the conversation
    /// the target names is shared by every turn in it.
    pub fn answering(mut self, message_seq: Option<EventSeq>) -> Self {
        self.message_seq = message_seq;
        self
    }
}

#[async_trait]
pub trait RunTurn: Send + Sync {
    /// A streamed turn on `agent_id` answering `message` in `chat`.
    async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        chat: ChatTarget<'_>,
    ) -> Result<TurnOutcome>;

    /// A streamed, operator-steerable turn (pause / cancel / redirect).
    ///
    /// `run_sink` is the dispatched attempt this turn belongs to, when it
    /// belongs to one (issue #242): a desk turn a *dispatched card* handed its
    /// work to traces into that card's run, while the same delegation reached
    /// from operator chat passes `None` and behaves exactly as before.
    async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat: ChatTarget<'_>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome>;

    /// A steerable turn WITHOUT live streaming — for a dispatched card whose
    /// steps are discarded into its note and must not leak onto the console
    /// timeline.
    ///
    /// `run_sink` carries the same meaning as on
    /// [`run_steered`](Self::run_steered). It is *this* method the dispatched
    /// card's own turns pass a sink to, which is what makes the card's trace
    /// durable while the turn runs even though nothing is streamed.
    ///
    /// `chat` is the conversation the turn belongs to, which is **not** implied
    /// by the absence of a stream (issue #1890 I). A dispatched card passes
    /// [`ChatTarget::default`] and binds to nothing; an approval's re-issued
    /// call passes the conversation the approval was raised in, so it runs
    /// against that thread's history rather than whatever was loaded last.
    async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat: ChatTarget<'_>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome>;

    /// A steerable turn that **streams** its live frames to the conversation
    /// `chat` names — a dispatched card whose origin the card recorded.
    ///
    /// Beside [`run_steered_background`](Self::run_steered_background) rather
    /// than a flag on it, because "does this turn belong to a conversation" and
    /// "should its frames be published" are different questions that issue
    /// #1890 I deliberately separated. An approval's re-issued call is the
    /// proof: it is addressed to the thread the approval was raised in *and*
    /// must stay un-streamed, because its answer arrives as the bubble the
    /// caller returns. Inferring the stream from a present `chat_id` collapses
    /// that distinction and leaks those frames onto whichever thread the
    /// console is watching — the exact misattribution #125 fixed.
    ///
    /// Defaults to the un-streamed method, so the sentinel and every test
    /// double inherit today's behaviour; only the streaming harness engine
    /// overrides it.
    async fn run_steered_dispatch(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &SteerControl,
        chat: ChatTarget<'_>,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.run_steered_background(company, agent_id, message, control, chat, run_sink)
            .await
    }

    /// An un-streamed, un-steered turn — a workflow agent node, which shows no
    /// operator chat bubble. Its transient frames must not reach the console
    /// timeline, which is the same reason this method exists beside
    /// [`run_steered_background`](Self::run_steered_background) rather than
    /// reusing [`run`](Self::run).
    ///
    /// `run_sink` is what makes such a turn *recorded*. It used to be absent,
    /// so a workflow node minted no attempt row, persisted no step trace, and
    /// was addressable by nothing — the node was green or red and that was the
    /// whole of what could be known about it.
    ///
    /// Defaults to [`run`](Self::run) so the sentinel and test doubles need not
    /// re-declare the same nothing; the streaming harness engines override it
    /// to suppress the live stream.
    async fn run_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        // The default drops the sink: [`run`](Self::run) has no channel for one,
        // and an engine with no step stream has nothing to feed it anyway. The
        // streaming harness overrides this and does record.
        let _ = run_sink;
        self.run(company, agent_id, message, ChatTarget::default())
            .await
    }

    /// Like [`run_background`](Self::run_background) but streams the node's live
    /// tool-call frames onto the turn-stream bus tagged with the workflow
    /// `run_id`/`node_id` (issue #1702), so the console's run-trace sheet can
    /// render a workflow agent node's tool calls *live* — the one dimension the
    /// merged snapshot trace does not carry.
    ///
    /// Defaults to [`run_background`](Self::run_background) so the sentinel and
    /// every test double inherit the existing un-streamed behaviour unchanged;
    /// only the streaming harness engines override it to actually publish the
    /// live frames.
    async fn run_background_workflow(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        run_sink: Option<Arc<RunTraceSink>>,
        workflow_run_id: &str,
        node_id: &str,
    ) -> Result<TurnOutcome> {
        // The default cannot stream (it has no turn-stream seam), so the ids are
        // unused and the turn runs exactly as an un-streamed background node.
        let _ = (workflow_run_id, node_id);
        self.run_background(company, agent_id, message, run_sink)
            .await
    }

    /// Warms whatever roster this engine caches before the first turn. The
    /// default is a no-op; a harness that builds its roster lazily behind a
    /// pool overrides it so a caller can ensure every lane before dispatch.
    async fn ensure(&self, _company: &CompanyRecord) -> Result<()> {
        Ok(())
    }

    /// Warms the roster against an explicit cycle-start policy snapshot instead
    /// of the live store overlay.
    ///
    /// Defaults to [`ensure`](Self::ensure), so lanes that do not distinguish
    /// the two — and every test double — keep their existing behaviour. The
    /// built-in harness overrides it so its roster's approval policy cannot
    /// drift from the native gate's mid-turn (issue #1455): both are pinned to
    /// the same record loaded at the top of the cycle.
    async fn ensure_with_policy(&self, company: &CompanyRecord, _policy: &Policy) -> Result<()> {
        self.ensure(company).await
    }

    /// Releases any cycle-start policy pin this engine's roster is holding, so
    /// the next plain [`ensure`](Self::ensure) rebuilds against the live store
    /// overlay.
    ///
    /// Defaults to a no-op: only the harness pool tracks a pin, and an engine
    /// that never pins has nothing to release. The built-in harness overrides
    /// it so a pin stored by [`ensure_with_policy`](Self::ensure_with_policy)
    /// is gone by the time the cycle is over — otherwise a standalone workflow
    /// turn between cycles would keep rebuilding against the last cycle's tier
    /// until an unrelated cycle refreshed it (issue #1455).
    async fn end_cycle(&self, _company: &CompanyId) {
        // no-op
    }

    /// The synchronous half of [`end_cycle`](Self::end_cycle), for a cycle's
    /// drop guard.
    ///
    /// A cycle whose future is cancelled or unwinds through a panic after
    /// [`ensure_with_policy`](Self::ensure_with_policy) installed its pin never
    /// reaches the async `end_cycle` — the `await` that would have called it is
    /// exactly where the future is dropped, so the pin would otherwise outlive
    /// the cycle and keep a standalone workflow turn between cycles on a stale
    /// snapshot until an unrelated cycle replaced it (issue #1455). A guard
    /// releases from `Drop`, so it cannot await; this synchronous removal is
    /// what lets it. Defaults to a no-op exactly like `end_cycle`; the built-in
    /// harness and the router fan-out override it.
    fn release_policy_pin_sync(&self, _company: &CompanyId) {
        // no-op
    }
}

// `desk_lead` is the brain-agnostic desk-lead resolver — it moved to
// `runtime::delegation_tools` (issue #176) so the hosted path can resolve a
// desk lead without the `openhuman` feature. Re-exported here so this module's
// callers (and its tests) keep using `desk_lead(...)` unchanged.
pub(crate) use crate::runtime::delegation_tools::desk_lead;

use crate::runtime::delegation_tools;

/// One hand-off, resolved: everything
/// [`run_hand_off`](DelegationRunner::run_hand_off) needs, and nothing about
/// *which kind* it was (issue #884).
///
/// The two hand-off delegations differ only in how they get here — a desk key
/// through [`desk_lead`], a roster id straight through
/// [`CompanyRecord::resolve_teammate_key`] — and this is the type that makes
/// that the *only* difference. A hand-off's card, its steer registration, its
/// depth bound and its relayed reply are properties of handing work over, not
/// of the namespace the target was named in.
struct HandOff {
    /// The roster id whose turn will run. Canonical, already validated.
    member: String,
    /// What that member is asked to do.
    instruction: String,
    /// What the hand-off is *called* on the operator's in-flight list: the desk
    /// key for a desk hand-off, the teammate's id for a teammate one.
    label: String,
    /// What this hand-off pushes onto the delegation scope chain — a resolved
    /// desk id, or a [`teammate_scope_key`](delegation_tools::teammate_scope_key)
    /// for the teammate form. The chain's length is the delegation depth and its
    /// contents are what the cycle guards compare against.
    scope_key: String,
}

/// The prompt for the CEO-relay hand-back turn: the operator's original message
/// plus each teammate's reply, framed so the orchestrator relays the answer back
/// as its own single, coherent response and does not delegate again.
pub(crate) fn build_relay_prompt(original: &str, desk_replies: &[(String, String)]) -> String {
    let mut prompt = format!(
        "The operator asked:\n{original}\n\nYou delegated this to your team and their reply is \
below. Relay their answer back to the operator now as your own single, coherent response — \
summarize it or pass it along. Do not delegate again; just relay what came back."
    );
    for (member, reply) in desk_replies {
        prompt.push_str(&format!("\n\n{member} replied:\n{reply}"));
    }
    prompt
}

/// The outcome of draining one queued delegation.
///
/// A `spawn_task` yields no bubble of its own — it opens a board card and
/// reports that card's id (issue #246), which is what the caller stamps onto the
/// bubble it was already sending. A synchronous `delegate_to_desk` yields a
/// [`DeskReply`] — the teammate's
/// answer captured so the orchestrator can **relay** it in a follow-up turn (the
/// CEO-relay hand-back) instead of leaving it as a disconnected sibling bubble.
/// `bubble` stays for any future delegation that surfaces its own standalone
/// message directly.
#[derive(Default)]
pub(crate) struct DelegationOutcome {
    /// Legacy single standalone bubble slot. Existing delegation kinds leave
    /// it empty; retained for the stable test seam.
    pub(crate) bubble: Option<OutboundMessage>,
    /// Chat bubbles to surface as-is. Conversation dispatch uses this for the
    /// recipient's DM reply and any bounded child replies it caused.
    pub(crate) bubbles: Vec<OutboundMessage>,
    /// A synchronous desk reply to relay through a second orchestrator turn.
    pub(crate) desk_reply: Option<DeskReply>,
    /// Set when an operator CANCELLED this delegation's run mid-flight, so its
    /// reply was discarded.
    ///
    /// `desk_reply: None` on its own does not mean "cancelled" —
    /// `run_delegation` also returns an empty outcome for a desk with no
    /// resolvable lead, and for every delegation that is not a hand-off. This
    /// flag carries the cancellation as a **fact** so a caller can report the
    /// cause rather than inferring one from an absence (issue #213 review).
    pub(crate) cancelled: bool,
    /// The id of the board card a `spawn_task` opened (issue #246).
    ///
    /// A `spawn_task` used to be entirely silent: it returned
    /// `DelegationOutcome::default()`, documented as surfacing nothing, so the
    /// operator got a reply with no sign that work had been opened. Reporting
    /// the id here is what lets the caller stamp it onto the bubble — and, from
    /// there, onto the journaled reply — so "a card was opened" is a fact the
    /// console can render rather than something the operator has to spot on the
    /// board.
    ///
    /// `None` for every other delegation kind, and for a `spawn_task` that
    /// found no task store to write to.
    pub(crate) spawned_task: Option<String>,
    /// Whether an `assign_task` actually wrote an owner onto a card (issue #661
    /// / M5).
    ///
    /// `spawn_task` reports its result through
    /// [`spawned_task`](Self::spawned_task); `assign_task` had nothing to report
    /// through, because its arm returns an empty outcome on **three** distinct
    /// paths — the write landed, the card is no longer on the board, or the name
    /// did not resolve to anybody on the roster (issue #205 deliberately leaves
    /// the previous owner in place then). Those are not the same fact, and a
    /// board row that called all three `assigned` would be exactly the confident
    /// falsehood the drain commitment exists to prevent.
    ///
    /// `false` for every other delegation kind, so the chat and task paths — which
    /// never read it — are unaffected.
    pub(crate) assigned: bool,
    /// A refused board write, reported without aborting the remaining drain.
    pub(crate) refused_card: Option<RefusedCardWrite>,
}

/// A board-write refusal and its operator-facing reason.
#[derive(Clone, Debug)]
pub(crate) struct RefusedCardWrite {
    /// `"assign_task"` or `"review_task"`, for the operator-facing note.
    pub(crate) tool: &'static str,
    pub(crate) task_id: String,
    pub(crate) reason: String,
}

/// Which workflow run a board write belongs to (issue #661 / M5).
///
/// Stamped onto every card a run's node opens
/// ([`TaskRecord::origin_run_id`](crate::ports::TaskRecord::origin_run_id) /
/// [`origin_workflow_id`](crate::ports::TaskRecord::origin_workflow_id)) and used
/// as the voice a run's note is recorded under, so a card the board shows says
/// *which machine act* put it there instead of borrowing the CEO's name.
///
/// Both ids together rather than the run alone: see `origin_workflow_id` for why
/// a run id on its own is not resolvable to a workflow once the journal is
/// trimmed.
///
/// # A sub-workflow child carries its parent's ids
///
/// `StoreWorkflowResolver` runs a `sub_workflow` child inside the engine under the
/// parent's capability bundle — one runner, one run id, one collector — so a child
/// node's card is stamped with the parent's run. That is the only run identity that
/// exists on that path, and the only run row a console can navigate to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowRunRef {
    /// The run whose node performed the write.
    pub(crate) run_id: String,
    /// The workflow graph that run is of.
    pub(crate) workflow_id: String,
}

/// The [`RunTurn`] a workflow-run drain is wired with: one that cannot run a turn.
///
/// A board drain never needs one. The only delegation that runs a turn is
/// [`Delegation::DelegateToDesk`], and a run holds a
/// [`DrainClaim::Board`](crate::harness::orchestrator::DrainClaim) claim, under
/// which a hand-off is refused at the tool boundary and can never be staged — so
/// nothing this drain executes reaches these methods.
///
/// **It errors rather than returning an empty turn**, and that is the point of
/// having it at all. A silent empty `TurnOutcome` would make a future path that
/// somehow staged a hand-off look like a desk that answered with nothing; an
/// `Err` is loud, lands in the log, and cannot be mistaken for an answer. Defense
/// in depth behind a guarantee that already holds one layer up.
struct NoTurn;

/// The single promoted instance [`DelegationRunner::for_workflow_run`] borrows.
///
/// A `const` so the borrow is `'static` and coerces to the runner's `&'a dyn
/// RunTurn` for any caller lifetime — the runner holds a reference, and a
/// temporary built inside the constructor would not outlive it.
const NO_TURN: NoTurn = NoTurn;

#[async_trait]
impl RunTurn for NoTurn {
    async fn run(
        &self,
        _company: &CompanyId,
        _agent_id: &str,
        _message: &str,
        _chat: ChatTarget<'_>,
    ) -> Result<TurnOutcome> {
        Err(no_turn_error())
    }

    async fn run_steered(
        &self,
        _company: &CompanyId,
        _agent_id: &str,
        _message: &str,
        _control: &SteerControl,
        _chat: ChatTarget<'_>,
        _run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        Err(no_turn_error())
    }

    async fn run_steered_background(
        &self,
        _company: &CompanyId,
        _agent_id: &str,
        _message: &str,
        _control: &SteerControl,
        _chat: ChatTarget<'_>,
        _run_sink: Option<Arc<RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        Err(no_turn_error())
    }
}

/// The one error [`NoTurn`] returns, in one place so the three arms cannot drift.
fn no_turn_error() -> crate::error::OpenCompanyError {
    crate::error::OpenCompanyError::Harness(
        "a workflow run's board drain cannot run a turn: a hand-off is refused at the tool \
         boundary under a board claim, so reaching here means a delegation was staged that this \
         path may not execute"
            .to_string(),
    )
}

/// A synchronous desk-lead answer captured for the orchestrator to relay: which
/// member answered, their reply text, and their own turn steps (folded onto the
/// operator timeline so the teammate's activity stays visible on the single
/// relayed bubble).
pub(crate) struct DeskReply {
    pub(crate) member: String,
    pub(crate) reply: String,
    pub(crate) steps: Vec<TurnStep>,
    /// Whether this teammate's turn — or any turn nested beneath it — paused at
    /// its tool-iteration cap (issue #926).
    ///
    /// Folded the same way `reply` and `steps` are: a deeper delegate's work is
    /// folded INTO this member's answer rather than surfacing on its own, so a
    /// cap two levels down is a cap on what the operator reads here.
    pub(crate) hit_iteration_cap: bool,
    /// The in-turn spend halt behind this answer, if one stopped it — this
    /// teammate's own turn or any turn nested beneath it (issue #1032).
    ///
    /// Folded exactly as `hit_iteration_cap` is, and for the same reason: a
    /// deeper delegate's work is folded INTO this member's reply, so a halt two
    /// levels down is a halt on what the operator ends up reading.
    ///
    /// **First halt wins** rather than last, because this carries figures and a
    /// teammate name rather than a bare flag — there is one bubble and it can
    /// name one cap. The first is the one that cut work short earliest, and the
    /// claim it makes is incomplete but never wrong, the same trade the
    /// first-wins `spawned_task` beside it already takes.
    pub(crate) halted_for_spend: Option<crate::harness::SpendHalt>,
    /// The budget pause behind this answer, if this teammate's own turn — or
    /// any turn nested beneath it — paused for lack of inference budget/credits
    /// (issue #1846).
    ///
    /// Folded exactly as `halted_for_spend` is, first-wins, for the same
    /// reason: one bubble, one figure worth naming.
    pub(crate) budget_paused: Option<crate::harness::BudgetPause>,
}

/// What was already decided about the operator message a drain belongs to
/// (issues #463, #267, #984).
///
/// Facts carried together because they answer the same question and because a
/// run of bare `bool` parameters at a call site is a swap waiting to happen.
/// All default to `false`, which is the honest reading for every drain with no
/// operator message in scope — a dispatched card's turn, the approval
/// re-dispatch — neither of which has a message to have carded or triaged.
///
/// Two of the three are settled before the model says anything; `chatter` is
/// the exception (issue #984) and is the model's own verdict, which is why it
/// is a separate field rather than another reading of `answering`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MessageContext {
    /// The REST chat handler already opened a To-do card for this message
    /// (issue #463), so no path below may open a second one.
    ///
    /// The handler has **two** roads to that card and this flag has to cover
    /// both: the triage naming a title, and — since #580 — the operator's
    /// composer asking for a workflow, which the handler takes as an override
    /// and supplies a title for when the triage declined to. Re-deriving this
    /// from the triage alone was true for the first road and false for the
    /// second, so a workflow request the triage did not recognise as work
    /// arrived here looking uncarded and got a second card (issue #1035).
    pub(crate) carded_by_handler: bool,
    /// The message triaged as
    /// [`MessageTriage::Answer`](crate::company::task_intent::MessageTriage)
    /// (issue #267). A hand-off still **runs** — consulting a desk is how a
    /// question the orchestrator cannot answer alone gets answered — but it
    /// opens no card, because nobody commissioned work.
    pub(crate) answering: bool,
    /// The lexical layer abstained and the **model** read this message as
    /// conversation (issue #984).
    ///
    /// Distinct from [`answering`](Self::answering) on purpose. That flag also
    /// narrows the model's own board tools; this one must not, because `Chatter`
    /// is the ambiguous bucket and withdrawing tools on a maybe is the expensive
    /// direction. All this does is stand the two deterministic card paths down —
    /// the paths that would otherwise open a card because
    /// [`is_trackable_work`]'s default is "everything is work".
    ///
    /// Only ever `true` where the lexical layer already abstained, so it can
    /// **subtract** a card and never mint one. Every degraded path — no harness,
    /// no escalation wired, an unparseable or slow verdict — leaves it `false`
    /// and behaves exactly as before.
    pub(crate) chatter: bool,
    /// The **operator** said this message is not a request for work (issue
    /// #1152) — they sent it under the composer's "Just chatting".
    ///
    /// A peer to [`chatter`](Self::chatter), never a reuse of it. That field is
    /// documented as *the model's* verdict, set only where the lexical layer
    /// abstained; this is a person's own statement about their own message,
    /// settled before any model runs, and it holds whatever the triage read —
    /// including a confident `Track`. Folding this into `chatter` would falsify
    /// that doc, make the debug line attribute an operator's choice to a model
    /// that was never asked, and put this change inside the field #984 owns.
    ///
    /// Subtractive only, like `chatter`: it stands the deterministic card paths
    /// down and touches nothing else. The model's own board tools are NOT
    /// narrowed — see [`open_work_card`](DelegationRunner::open_work_card) for
    /// why, and what that means the label does and does not promise.
    pub(crate) not_work: bool,
}

/// Whether a drain may run the hand-offs it finds, or must drop them.
///
/// The CEO-relay turn is the one caller that must drop: a second hand-off from
/// there is the re-delegation loop the drain exists to stop. Board writes are
/// still executed — a card the relay turn opened is not a re-delegation (issue
/// #442).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandOffs {
    /// Run them, and report what came back.
    Run,
    /// Drop them, with a log line; execute everything else.
    Drop,
}

/// What one drain of the delegation queue produced (issue #453).
///
/// Extracted so every path that runs a turn can reuse the *exact* execution
/// semantics rather than re-deriving them. Before this, only the two callers
/// that happened to contain the loop had them, and the approval re-dispatch —
/// which runs a full toolbelt turn — silently had none at all.
#[derive(Default)]
pub(crate) struct Drained {
    /// Standalone chat bubbles to surface as-is.
    pub(crate) bubbles: Vec<OutboundMessage>,
    /// Synchronous desk answers, in the order they came back. Empty when
    /// [`HandOffs::Drop`] was asked for.
    pub(crate) desk_replies: Vec<DeskReply>,
    /// The desks whose hand-off an operator CANCELLED mid-flight (issue #176).
    ///
    /// Carried as a fact rather than inferred from a missing reply, for the
    /// same reason [`DelegationOutcome::cancelled`] is: a hand-off yields no
    /// reply for several reasons and only one of them is a cancellation. The
    /// nested drain reads this so a delegate's own cancelled hand-off is folded
    /// into the reply as a cancellation note instead of vanishing — the
    /// alternative being an answer that quietly omits a branch the model said it
    /// had started.
    pub(crate) cancelled_desks: Vec<String>,
    /// The **first** board card this drain opened, matching
    /// [`OperatorTurn::spawned_task`]'s first-wins rule.
    pub(crate) spawned_task: Option<String>,
    /// Board-write refusals from this drain.
    pub(crate) refused_cards: Vec<RefusedCardWrite>,
}

impl Drained {
    fn absorb(&mut self, out: DelegationOutcome, target: Option<String>) {
        if out.cancelled
            && let Some(target) = target
        {
            self.cancelled_desks.push(target);
        }
        if let Some(id) = out.spawned_task {
            self.spawned_task.get_or_insert(id);
        }
        if let Some(bubble) = out.bubble {
            self.bubbles.push(bubble);
        }
        self.bubbles.extend(out.bubbles);
        if let Some(reply) = out.desk_reply {
            self.desk_replies.push(reply);
        }
        if let Some(refused) = out.refused_card {
            self.refused_cards.push(refused);
        }
    }

    fn merge(&mut self, nested: Drained) {
        self.bubbles.extend(nested.bubbles);
        self.desk_replies.extend(nested.desk_replies);
        self.cancelled_desks.extend(nested.cancelled_desks);
        self.refused_cards.extend(nested.refused_cards);
        if let Some(id) = nested.spawned_task {
            self.spawned_task.get_or_insert(id);
        }
    }
}

/// The operator-facing result of one operator message after delegation: the
/// bubble's reply and folded step timeline, plus any standalone delegation
/// bubbles to append as sibling channel responses. None of the current
/// delegations surface a standalone bubble; the field keeps the seam open for
/// one that does.
pub(crate) struct OperatorTurn {
    pub(crate) reply: String,
    pub(crate) steps: Vec<TurnStep>,
    pub(crate) bubbles: Vec<OutboundMessage>,
    /// The board card this turn opened, when it opened one (issue #246) — the
    /// **first**, if it opened several.
    ///
    /// Carried to the caller so the operator bubble can say a card was opened,
    /// which a `spawn_task` never did before: the card appeared on the board
    /// and the reply gave no sign of it.
    ///
    /// First-only because the field this ultimately lands in — the journaled
    /// `AgentReply.task_id` — is a single optional id, and widening it would
    /// break the byte-identical round-trip every already-stored reply relies
    /// on. The resulting claim is incomplete but never wrong, and the bubble's
    /// step timeline still shows every `spawn_task` the turn made.
    pub(crate) spawned_task: Option<String>,
    /// Whether **any** turn behind this bubble paused at its tool-iteration cap
    /// (issue #926) — the responder's, a desk lead's, or the CEO relay's.
    ///
    /// A sticky OR rather than "the last turn's value", because one operator
    /// message can run several turns and the operator gets exactly ONE bubble
    /// for the whole chain. The relay turn in particular *replaces* the reply
    /// text, so tracking the last value would erase a cap the responder or a
    /// delegate hit — the operator would read a relayed answer that quietly
    /// omits that a branch of it stopped half-done.
    pub(crate) hit_iteration_cap: bool,
    /// The in-turn spend halt behind **any** turn on this bubble (issue #1032)
    /// — the responder's, a desk lead's, or the CEO relay's.
    ///
    /// First-wins for the same reason the sticky OR beside it exists: one
    /// operator message can run several turns and the operator gets exactly ONE
    /// bubble for the whole chain, and the relay turn *replaces* the reply text,
    /// so tracking the last value would erase a halt the responder or a delegate
    /// hit. Where the flag beside it ORs, this keeps the first `Some` — it
    /// carries figures, and one notice can quote one cap.
    pub(crate) halted_for_spend: Option<crate::harness::SpendHalt>,
    /// The budget pause behind **any** turn on this bubble (issue #1846) — the
    /// responder's, a desk lead's, or the CEO relay's.
    ///
    /// First-wins, for the same reason `halted_for_spend` is: one bubble, one
    /// figure worth naming, and the relay turn replaces the reply text so
    /// tracking only the last value would erase an earlier pause.
    pub(crate) budget_paused: Option<crate::harness::BudgetPause>,
}

/// What a **dispatched card's** turn handed off (issue #204).
///
/// Returned by [`DelegationRunner::handle_task_delegations`] when the turn
/// called `delegate_to_desk` and the desk resolved to a real teammate: that
/// teammate is now the card's assignee and has already run. `reply` is what
/// they produced.
///
/// A `TaskHandoff` with `reply: None` is only ever built from a run an operator
/// actually CANCELLED — [`DelegationOutcome::cancelled`] is the input, not the
/// absence of a reply. A hand-off that yields nothing for any *other* reason
/// reports no hand-off at all, so the delegator's own turn settles the card
/// (the same path a desk with no resolvable lead already takes) rather than the
/// card being settled as a cancellation that never happened (issue #213
/// review).
pub(crate) struct TaskHandoff {
    /// The delegate that took the card over — now its `assignee`.
    pub(crate) delegate: String,
    /// What the delegate produced. `None` means their run was cancelled
    /// mid-flight, and nothing else.
    pub(crate) reply: Option<String>,
    /// The budget pause behind the delegate's run, if any (issue #1846
    /// review, Codex #3865395868).
    ///
    /// [`DeskReply`] has carried this since the top-level fix this issue
    /// added, but this struct dropped it on the way through — `reply` here
    /// is `desk.reply`'s text with `desk.budget_paused` thrown away, so a
    /// dispatched card whose delegate ran out of credits reached
    /// `HarnessBrain::run_task` with no way to tell a real completion from a
    /// pause notice standing in for one, and settled `Completed` either way.
    /// Carried through so the caller can gate the card's terminal state on
    /// it, the same way `direct_card` and this hand-off's own card already
    /// do.
    pub(crate) budget_paused: Option<crate::harness::BudgetPause>,
    /// SPIKE (async hand-off): the delegate owns the card but has NOT run yet.
    ///
    /// The synchronous model awaits the delegate inside the delegator's
    /// attempt, which is why `TaskRunEnd::Delegated` was documented as
    /// "unreachable as a run settle today". Handing over without running makes
    /// it reachable: the delegator settles `Delegated`, and the delegate is
    /// dispatched as its own attempt with its own cost and its own lock
    /// acquisition.
    pub(crate) pending: bool,
}

/// Drives the brain-agnostic delegation orchestration over a [`RunTurn`]: run the
/// responder's turn, drain and execute whatever it queued, and — when a desk
/// answered synchronously — relay through exactly one more responder turn.
///
/// Holds only brain-agnostic handles: the company record (for desk-lead
/// resolution), the task store, the steer registry, the company id, the shared
/// delegation queue the turn pushes onto, and the per-turn delegation cap. The
/// harness-specific [`HarnessDeps`](crate::harness::HarnessDeps) is deliberately
/// absent — that is the whole point of the seam; it lives behind the [`RunTurn`]
/// impl.
pub(crate) struct DelegationRunner<'a> {
    run_turn: &'a dyn RunTurn,
    record: &'a CompanyRecord,
    tasks: Option<&'a Arc<dyn TaskStore>>,
    steer: &'a InflightRegistry,
    company: &'a CompanyId,
    queue: &'a DelegationQueue,
    max_delegations: usize,
    /// The dispatched card this drain is running inside, when it is one (issue
    /// #204). Owned rather than borrowed so a caller can hold the card mutably
    /// while the runner runs. `None` for an operator chat turn.
    task: Option<String>,
    /// The attempt row that card is running under (issue #242), so a delegate's
    /// turn traces and meters into the *same* run rather than disappearing from
    /// the record the moment the work changes hands. `None` for an operator chat
    /// turn, and for a dispatch whose run row could not be minted.
    run_sink: Option<Arc<RunTraceSink>>,
    /// What the operator's composer said this message is for, when they chose
    /// (issues #1035, #1152). `None` for every path that is not an operator chat
    /// turn, and for a message whose sender expressed no preference.
    requested_intent: Option<crate::ports::types::MessageIntent>,
    /// The other teammates this message named, when it named any.
    ///
    /// Context for the turn, **never a second dispatch**: one operator message
    /// spawns exactly one turn, and this is how the teammate answering it
    /// learns who else was addressed. It decides whether the work should
    /// actually spread, and spreads it through the delegation tools it already
    /// has — so a mention cannot become a way to start N turns with no approval
    /// in sight.
    ///
    /// Empty on every path that is not a person typing into the composer, and
    /// on every message that names nobody.
    also_mentioned: Vec<String>,
    /// The original request this drain is answering, in the requester's own
    /// words — the operator's chat message, or the (possibly redirect-
    /// augmented) task instruction (issue #1846 review, Codex #3864988176).
    ///
    /// `run_hand_off` re-parks the delegate's budget-pause marker with this
    /// text when it is set, overwriting what `run_inner` already parked (the
    /// model-generated hand-off instruction) — see the re-park there for why
    /// that default is wrong for a delegated turn. `None` on every path that
    /// does not set it, which leaves `run_inner`'s park as the answer: no
    /// worse than before this fix, for the paths that have not been taught to
    /// carry a re-issue text.
    reissue_message: Option<String>,
    /// The thread this turn belongs to, when it belongs to one (#1890).
    ///
    /// A builder for the same reason [`requested`](Self::requested) and
    /// [`also_mentioned`](Self::also_mentioned) are, and the argument their docs
    /// already make applies here with more force: optional context about the
    /// turn, absent on every path that is not an operator message in a thread,
    /// and threading it as an argument made well over a hundred call sites
    /// restate "no thread" to say nothing — with `main` adding more of them
    /// while the change was in review, so every rebase re-broke the branch.
    ///
    /// The channel stays an argument, because every caller has one and it
    /// selects *who answers*. The thread only narrows *what they remember*.
    thread_root: Option<EventSeq>,
    /// The journal sequence of the operator message this drain is answering.
    ///
    /// Reaches only the responder's own turn (see
    /// [`answering_target`](Self::answering_target)), never the relay or a
    /// delegate's, because only that one turn is running the very text this seq
    /// names. `None` on every non-operator path, and on any caller that has not
    /// been taught to carry it.
    message_seq: Option<EventSeq>,
    /// The cycle's approval queue, read (never written) to tell whether a turn
    /// this runner drove parked an approval (issue #465).
    ///
    /// Only the count matters, taken either side of the turn: a card whose turn
    /// stopped at an unauthorised call has produced nothing to review, and
    /// [`settle_work_card`](Self::settle_work_card) needs to know that before it
    /// picks a landing. Optional so the ~dozen `DelegationRunner::new` sites in
    /// tests stay untouched; `None` reads as "nothing parked", which is the
    /// pre-#465 behaviour and correct for any runner that cannot park.
    approvals: Option<&'a ApprovalRequestQueue>,
    /// The workflow run this drain belongs to, when it is one (issue #661 / M5).
    ///
    /// `None` for every pre-existing constructor and therefore for every chat and
    /// task path — which is what makes this field a pure addition: the two arms
    /// that read it fall back to exactly the behaviour they had (no origin stamp,
    /// the orchestrator's voice on a note).
    workflow_run: Option<WorkflowRunRef>,
    /// A second opinion for the messages the lexical classifier abstained on
    /// (issue #678). `None` — every pre-#678 constructor, and any company whose
    /// build wires no evaluator — keeps the deterministic answer, which is the
    /// behaviour this had before.
    triage: Option<&'a dyn crate::harness::triage::TriageEscalation>,
    /// Names the work a card is opened for. `None` — every pre-existing
    /// constructor, and any company whose build wires no titler — falls back to
    /// shortening the request, which is what every card was named before.
    titler: Option<&'a dyn crate::ports::tasks::TitleSummariser>,
    /// Workflows the turn authored in-flight with the inline `create_workflow`
    /// tool (issues #112, #339), read so an operator turn can settle the card it
    /// adopted instead of leaving it in To-do (issue #678).
    ///
    /// Optional for the same reason [`approvals`](Self::approvals) is: the
    /// ~dozen `DelegationRunner::new` sites in tests stay untouched, and `None`
    /// reads as "this runner cannot see staged workflows", which is exactly the
    /// pre-#678 behaviour.
    workflow_refs: Option<&'a WorkflowRefQueue>,
}

impl<'a> DelegationRunner<'a> {
    /// Wires a runner over `run_turn` with the brain-agnostic handles it needs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run_turn: &'a dyn RunTurn,
        record: &'a CompanyRecord,
        tasks: Option<&'a Arc<dyn TaskStore>>,
        steer: &'a InflightRegistry,
        company: &'a CompanyId,
        queue: &'a DelegationQueue,
        max_delegations: usize,
    ) -> Self {
        Self {
            run_turn,
            record,
            tasks,
            steer,
            company,
            queue,
            max_delegations,
            task: None,
            run_sink: None,
            requested_intent: None,
            also_mentioned: Vec::new(),
            reissue_message: None,
            thread_root: None,
            message_seq: None,
            approvals: None,
            workflow_run: None,
            workflow_refs: None,
            triage: None,
            titler: None,
        }
    }

    /// Wires a runner for one **workflow run's** board drain (issue #661 / M5).
    ///
    /// A separate constructor rather than a builder method on
    /// [`new`](Self::new), because the two differ in what they can do rather than
    /// only in what they know: this one has no way to run a turn (see
    /// [`NoTurn`]), and it stamps run provenance onto everything it opens. The
    /// only thing it is ever asked to execute is
    /// [`execute_board_writes`](Self::execute_board_writes).
    ///
    /// `steer` is threaded because the shared runner needs one and there is no
    /// honest way to pass nothing — **it is never touched on this path**: the only
    /// registration site is the [`Delegation::DelegateToDesk`] arm, which a board
    /// claim makes unstageable. Passing the company's own registry rather than a
    /// fresh one keeps it that way by accident-proofing: were the arm ever
    /// reachable, the run would appear in the operator's in-flight list rather
    /// than in a registry nobody can see.
    ///
    /// No approval queue is wired: `with_approvals` exists so a *settle* can tell
    /// whether the turn it is recording parked (issue #465), and this runner
    /// settles nothing — a run's gated calls are parked by
    /// `HarnessAgentRunner::park_gated_calls` on the run's own approval scope.
    pub(crate) fn for_workflow_run(
        record: &'a CompanyRecord,
        tasks: Option<&'a Arc<dyn TaskStore>>,
        steer: &'a InflightRegistry,
        company: &'a CompanyId,
        queue: &'a DelegationQueue,
        run: WorkflowRunRef,
    ) -> Self {
        Self {
            run_turn: &NO_TURN,
            record,
            tasks,
            steer,
            company,
            queue,
            max_delegations: orchestrator::MAX_DELEGATIONS_PER_TURN,
            task: None,
            run_sink: None,
            // A workflow run has no operator message and therefore no composer
            // choice; `None` is the only honest value here.
            requested_intent: None,
            also_mentioned: Vec::new(),
            reissue_message: None,
            thread_root: None,
            message_seq: None,
            approvals: None,
            workflow_run: Some(run),
            workflow_refs: None,
            triage: None,
            titler: None,
        }
    }

    /// Wires the cycle's approval queue so a settle can tell whether the turn it
    /// is recording parked an approval (issue #465).
    ///
    /// Without it a turn whose first tool call parked settled as a plain
    /// success and its card landed in In Review — announcing a result to check
    /// on work that had never started.
    pub(crate) fn with_approvals(mut self, approvals: &'a ApprovalRequestQueue) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// How many approval requests are parked right now, or `0` when no queue is
    /// wired. Differenced across a turn to attribute parks to *that* turn.
    fn approvals_queued(&self) -> usize {
        self.approvals.map_or(0, ApprovalRequestQueue::queued)
    }

    /// Wires the cycle's workflow-reference queue so an operator turn can settle
    /// the card for a workflow it authored in-turn (issue #678).
    ///
    /// Without it that card is adopted and then abandoned: the bubble links to
    /// it, the operator gets the workflow, and the card sits in To-do because
    /// the only drain lives on the dispatched-card path.
    pub(crate) fn with_workflow_refs(mut self, workflow_refs: &'a WorkflowRefQueue) -> Self {
        self.workflow_refs = Some(workflow_refs);
        self
    }

    /// Wires the LLM escalation used when the lexical triage abstains
    /// (issue #678).
    ///
    /// Without it an abstention keeps `Chatter`'s no-gate behaviour, which is
    /// what every turn did before this existed.
    pub(crate) fn with_triage(
        mut self,
        triage: &'a dyn crate::harness::triage::TriageEscalation,
    ) -> Self {
        self.triage = Some(triage);
        self
    }

    /// Wires the pass that names the work a card is opened for.
    ///
    /// Without it a card is named by shortening the request, which is what
    /// every card was named before.
    pub(crate) fn with_titler(
        mut self,
        titler: &'a dyn crate::ports::tasks::TitleSummariser,
    ) -> Self {
        self.titler = Some(titler);
        self
    }

    /// Executes a workflow run's drained board writes, reporting one row each
    /// (issue #661 / M5).
    ///
    /// # Infallible on purpose — the signature *is* the guarantee
    ///
    /// A board write must never fail the node that made it. The turn already
    /// happened, the graph is mid-walk, and discarding a completed node's work
    /// over a `TaskStore` hiccup would be the worst available trade. So this
    /// returns `Vec<WorkflowRunBoardRow>` and not a `Result`: there is no `?` for
    /// a future edit to add, and the requirement holds by construction rather than
    /// by every caller remembering it. Same stance
    /// [`park_gated_calls`](crate::workflows::caps::HarnessAgentRunner) takes one
    /// queue over, for the same reason.
    ///
    /// A failed write is loud in two places instead: a `*Failed` row an operator
    /// reads on the run, and a `tracing::error` for whoever is watching the host.
    ///
    /// # What it may be handed
    ///
    /// Only [`Delegation::SpawnTask`] and [`Delegation::AssignTask`] — everything
    /// else is refused at the tool boundary under
    /// [`DrainClaim::Board`](crate::harness::orchestrator::DrainClaim). A third
    /// kind arriving here is a wiring defect, so it is logged at `error` and
    /// contributes no row: fabricating one would put a write on the run's record
    /// that this path did not perform.
    pub(crate) async fn execute_board_writes(
        &self,
        delegations: Vec<Delegation>,
    ) -> Vec<crate::ports::WorkflowRunBoardRow> {
        use crate::ports::{WorkflowBoardAction, WorkflowRunBoardRow};

        let mut rows = Vec::with_capacity(delegations.len());
        for delegation in delegations {
            // Read the row's structural fields off the delegation BEFORE it is
            // consumed by the drain. Nothing here is the model's prose beyond the
            // card's own title and the owner it named — see `WorkflowRunBoardRow`.
            let (spawn, task_id, title, assignee) = match &delegation {
                Delegation::SpawnTask {
                    title, assignee, ..
                } => (true, None, Some(title.clone()), assignee.clone()),
                Delegation::AssignTask {
                    task_id, assignee, ..
                } => (false, Some(task_id.clone()), None, Some(assignee.clone())),
                other => {
                    // `kind_label`, never `{other:?}`: a `Delegation`'s Debug
                    // carries the model's own instruction and note text, and this
                    // line goes to host stdout — which on a hosted deployment is
                    // the platform rather than the operator. Same split
                    // `DeliveryReason` draws against `DeliveryReport::detail`.
                    tracing::error!(
                        company = %self.company,
                        run_id = %self.run_id_label(),
                        kind = kind_label(other),
                        "[delegation] a workflow run's board drain was handed a delegation it may \
                         not perform; the tool boundary should have refused it before it was \
                         staged"
                    );
                    continue;
                }
            };
            // The SAME arm the chat path runs — which is what makes the
            // no-column-move invariant inherited rather than re-promised here.
            let outcome = self
                .run_delegation(delegation, None, MessageContext::default())
                .await;
            let row = match (spawn, outcome) {
                // A card id comes back only after the store took the write
                // (issue #246), so `Some` here means the card is genuinely on
                // the board.
                (true, Ok(outcome)) => match outcome.spawned_task {
                    Some(id) => WorkflowRunBoardRow {
                        action: WorkflowBoardAction::Spawned,
                        task_id: Some(id),
                        title,
                        assignee,
                    },
                    // `Ok` with no id: this runtime wired no task board. Not an
                    // error the node should fail on, and not a card either.
                    None => WorkflowRunBoardRow {
                        action: WorkflowBoardAction::SpawnFailed,
                        task_id: None,
                        title,
                        assignee,
                    },
                },
                (false, Ok(outcome)) => WorkflowRunBoardRow {
                    action: if outcome.assigned {
                        WorkflowBoardAction::Assigned
                    } else {
                        WorkflowBoardAction::AssignFailed
                    },
                    task_id,
                    title,
                    assignee,
                },
                (spawn, Err(err)) => {
                    tracing::error!(
                        company = %self.company,
                        run_id = %self.run_id_label(),
                        spawn,
                        %err,
                        "[delegation] a workflow run's board write failed; the run is unaffected \
                         and the failure is reported on its board rows"
                    );
                    WorkflowRunBoardRow {
                        action: if spawn {
                            WorkflowBoardAction::SpawnFailed
                        } else {
                            WorkflowBoardAction::AssignFailed
                        },
                        task_id,
                        title,
                        assignee,
                    }
                }
            };
            if row.action.failed() {
                tracing::error!(
                    company = %self.company,
                    run_id = %self.run_id_label(),
                    action = ?row.action,
                    "[delegation] a workflow node was told its board write would happen and it \
                     did not"
                );
            }
            rows.push(row);
        }
        rows
    }

    /// Scopes this runner to a dispatched card, so anything the turn spawns
    /// records that card as its parent (issue #185's `parent_task_id`).
    pub(crate) fn for_task(mut self, task_id: &str) -> Self {
        self.task = Some(task_id.to_string());
        self
    }

    /// [`for_task`](Self::for_task) for a caller that may or may not have one —
    /// a chat turn scopes itself to the card its THREAD already opened, and to
    /// nothing when the thread has none.
    pub(crate) fn maybe_for_task(self, task_id: Option<&str>) -> Self {
        match task_id {
            Some(id) => self.for_task(id),
            None => self,
        }
    }

    /// Scopes this runner to the card's **attempt** (issue #242), so a delegated
    /// turn's steps and spend land on the same run the dispatch opened.
    pub(crate) fn for_run(mut self, run_sink: Option<Arc<RunTraceSink>>) -> Self {
        self.run_sink = run_sink;
        self
    }

    /// Carries the operator's own statement of what this message is for
    /// (issues #1035, #1152).
    ///
    /// A builder rather than a parameter on
    /// [`handle_operator_message`](Self::handle_operator_message) for the same
    /// reason [`for_task`](Self::for_task) and [`for_run`](Self::for_run) are:
    /// it is optional context about the turn, absent on every path that is not a
    /// person typing into the composer, and threading it as an argument would
    /// make a dozen test call sites restate `None` to say nothing.
    pub(crate) fn requested(mut self, intent: Option<crate::ports::types::MessageIntent>) -> Self {
        self.requested_intent = intent;
        self
    }

    /// Carries the other teammates this message named.
    ///
    /// A builder for the same reason [`requested`](Self::requested) is: optional
    /// context about the turn, absent on every path that is not an operator
    /// message, and threading it as an argument would make a dozen test call
    /// sites restate an empty vector to say nothing.
    /// The conversation a turn in `chat_id` belongs to: that channel, plus
    /// whatever thread [`in_thread`](Self::in_thread) bound this runner to.
    ///
    /// The single place the two halves are rejoined, so a caller cannot pair a
    /// channel with the wrong thread by getting the argument order wrong — the
    /// hazard `ChatTarget`'s own docs name.
    fn target(&self, chat_id: Option<&'a str>) -> ChatTarget<'a> {
        ChatTarget::in_thread(chat_id, self.thread_root)
    }

    /// [`target`](Self::target), plus the identity of the journaled message the
    /// turn is answering.
    ///
    /// Only the responder's own turn gets this. The relay turn runs a prompt
    /// the orchestrator composed and a delegate runs an instruction the model
    /// wrote — neither is the operator's line, so claiming that seq for them
    /// would name someone else's message as their own.
    fn answering_target(&self, chat_id: Option<&'a str>) -> ChatTarget<'a> {
        self.target(chat_id).answering(self.message_seq)
    }

    /// Binds this turn to the thread rooted at `root` (#1890) — `None` is the
    /// channel-level conversation, which is what every non-threaded path wants
    /// and therefore never has to say.
    pub(crate) fn in_thread(mut self, root: Option<EventSeq>) -> Self {
        self.thread_root = root;
        self
    }

    /// Carries the journal sequence of the operator message this drain answers.
    ///
    /// A builder for the same reason [`in_thread`](Self::in_thread) is: optional
    /// context about the turn, absent on every path that is not a journaled
    /// operator message, and threading it as an argument would make every
    /// existing call site restate `None` to say nothing.
    pub(crate) fn answering(mut self, message_seq: Option<EventSeq>) -> Self {
        self.message_seq = message_seq;
        self
    }

    pub(crate) fn also_mentioned(mut self, agents: Vec<String>) -> Self {
        self.also_mentioned = agents;
        self
    }

    /// Carries the original request this drain is answering, in the
    /// requester's own words (issue #1846 review, Codex #3864988176) — the
    /// operator's chat message, or the task instruction a dispatched card is
    /// running.
    ///
    /// A builder for the same reason [`also_mentioned`](Self::also_mentioned)
    /// is: optional context, absent on every path that has not been taught to
    /// carry it, and threading it as a required argument would make every
    /// existing call site (and the ~dozen test constructors) restate `None`.
    /// See the field doc for what reads it.
    pub(crate) fn reissue_message(mut self, text: impl Into<String>) -> Self {
        self.reissue_message = Some(text.into());
        self
    }

    /// Handles one operator message end-to-end: claim the delegation queue for
    /// this turn (issue #453 — the acquire also clears, so nothing stale leaks
    /// in), run the responder's turn, drain whatever it queued (capped,
    /// discarded past the cap), and — when a synchronous desk delegation
    /// answered — run exactly one CEO-relay hand-back turn whose reply replaces
    /// the operator-facing text.
    ///
    /// The relay turn must not re-delegate: its prompt is relay-only, and as a
    /// safety net the delegation queue is cleared before it and drained-and-
    /// discarded after, so anything it queues is dropped (cost bounded to one
    /// extra turn, no re-delegation loop). With no delegation the single first
    /// turn is surfaced unchanged.
    pub(crate) async fn handle_operator_message(
        &self,
        responder: &str,
        message: &str,
        chat_id: Option<&str>,
    ) -> Result<OperatorTurn> {
        // What this message IS, decided once (issue #267).
        //
        // Evaluated here, and only here, for one reason: this is the only place
        // the operator's OWN words are still in scope. `run_delegation` receives
        // the instruction the model wrote, which is a different sentence — a
        // guard placed there would be asking the triage about the model's prose
        // rather than about the message the handler classified. #442 put the
        // stand-down in `open_direct_work_card` only, so a recognised imperative
        // the orchestrator handed off produced the REST card AND the delegation
        // card. One message, one card — so every card-opening path below reads
        // this same answer.
        let triaged = crate::company::task_intent::triage_message_detailed(operator_words(message));
        let triage = triaged.triage.clone();
        // Issue #267, Layer B: on a question, the model may not WRITE to the
        // board — but it keeps every means of answering, including the one that
        // runs somebody else's turn.
        //
        // Layer A (the REST handler) already declines to card a question, but
        // that closes one of two doors. The other is the model calling
        // `spawn_task` / `delegate_to_desk` / `assign_task` / `review_task`
        // itself — which is exactly where the "Tell what is there in the tasks
        // list" card came from, a pure read that only wanted one
        // `query_company` call. A brief cannot close that door; it is guidance,
        // and behaviour follows structure rather than caveats.
        //
        // The claim is what closes it, and it reuses semantics that already
        // exist rather than adding a second mechanism. It is **narrowed**, not
        // withheld (issue #267 review): under `claim_answering` the queue still
        // drains, `push_within_cap` refuses the three pure board writes with
        // `Staged::NoDrain` — in the model's own turn, telling it not to retry
        // and not to report the action as done — and lets a hand-off through.
        //
        // Withholding the claim outright was over-broad. `delegate_to_desk` is
        // not only a board write: it is how a question the orchestrator cannot
        // answer alone gets routed to a desk that can, and taking it away left
        // "what did the design desk ship this week?" unanswerable. Its *card*
        // is the part that must not happen on a question turn, and that is
        // suppressed one layer down in `run_delegation` — exactly the shape
        // `open_direct_work_card` already uses. So the claim on the queue
        // removes the ability to write, and nothing removes the ability to
        // reply.
        //
        // The read tools are untouched throughout — `query_company`,
        // `run_workflow` and `read_run_output` execute inline and never reach
        // the queue.
        //
        // `Chatter` deliberately does NOT gate at all. It is the ambiguous
        // bucket, and taking board tools away on a maybe would turn a triage
        // miss into work the company silently refuses to do — the expensive
        // direction of the issue's own tie-breaker (a missed card costs one
        // follow-up message).
        //
        // Cross-brain: this is the harness path only. `HostedMedullaBrain` has
        // no delegation stack at all (issue #176), so there is no model
        // board-write path to gate there; when #176 copies this drain site
        // through the canonical `delegation_tools` seam, the conditional claim
        // comes with it. Layer A above fronts both brains in the meantime.
        // Issue #678: the lexical layer answers most messages and abstains on
        // the rest. Only the residue is worth a model call — escalating every
        // message would tax each reply with a serial round-trip to improve a
        // minority of classifications, which is the trade this deliberately
        // does not make.
        //
        // An escalation can only ever *narrow* the claim, never widen what the
        // turn may do, and it never mints a card: a missed card costs one
        // follow-up message, a spurious card pollutes the board permanently.
        // `Work` and `Chatter` therefore both leave the gate where the
        // abstention left it, and only `Answer` moves it.
        let mut answering = triage.is_answer();
        // Issue #984: the same escalation, read for BOTH of its useful answers.
        //
        // `Answer` narrows the claim, as it always has. `Chatter` was computed,
        // logged and thrown away — and it is the verdict that matters here: the
        // lexical card detector's default is "everything is work", so a message
        // the model has just read as conversation still opened a card. We had
        // already paid for the call.
        //
        // Carried as its OWN fact rather than by folding it into `answering`.
        // They are different claims about the turn and only one of them touches
        // the model's board tools: `Chatter` deliberately does not gate those
        // (the comment above says why — taking them away on a maybe turns a
        // triage miss into work the company silently refuses), while it *does*
        // stand the deterministic card paths down.
        //
        // Seeded from the lexical layer's OWN matched verdict (issue #1725
        // review), not only the escalation below: a bare greeting or
        // acknowledgement is `Chatter` by a rule firing (`is_matched_chatter`),
        // not by abstention, so it never reaches the escalation branch at all —
        // that branch only ever runs on an abstained triage. Without this seed
        // the greeting fast path below could never fire for the exact messages
        // it exists to optimise.
        //
        // Kept as its own fact (`matched_chatter`), not folded into `chatter`
        // below, because the two need different amounts of trust at the
        // fast-path gate (see the `chat_only` computation further down, issue
        // #1725 review round 2): `matched_chatter` is a WHOLE-MESSAGE match
        // against `task_intent::GREETINGS` — by construction already exactly a
        // bare greeting/ack, nothing else — so it is safe to fast-path on
        // directly. `chatter` also absorbs the escalation verdict below, which
        // judges an ARBITRARY abstained message as "conversational"; that is
        // broader than a greeting shape, so it still needs the separate
        // `is_pure_small_talk` gate. Gating the lexical match through
        // `is_pure_small_talk` too was the review-round-2 bug: that predicate's
        // `SMALLTALK_OPENERS` is a first-WORD opener list, independently
        // maintained from `GREETINGS`'s whole-MESSAGE vocabulary, so they drift
        // ("hii", "sup", "good morning", "kk", "gotcha", "done", "lgtm" are all
        // in `GREETINGS` but not recognised as an opener) — a lexically matched
        // greeting was silently falling back to the full agentic turn anyway.
        let matched_chatter = triaged.is_matched_chatter();
        let mut chatter = matched_chatter;
        if !answering
            && triaged.abstained()
            && let Some(escalation) = self.triage
        {
            let verdict = escalation.classify(operator_words(message)).await;
            if verdict.is_answer() {
                tracing::debug!(
                    company = %self.company,
                    "[triage] the lexical layer abstained and the model read this as a question; \
                     narrowing the claim to answering-only"
                );
                answering = true;
            } else if verdict.is_chatter() {
                tracing::debug!(
                    company = %self.company,
                    "[triage] the lexical layer abstained and the model read this as \
                     conversation; opening no card for it"
                );
                chatter = true;
            }
        }
        // Claim the delegation queue for this turn and its drain (issue #453).
        //
        // The acquire-clear subsumes the bare `clear()` this used to open with —
        // same guarantee, that nothing a prior turn staged leaks into this one —
        // and adds the half that was missing: for the span of this claim, and
        // only for it, the delegation tools are allowed to queue at all. On every
        // exit path, including the `?`s below, `Drop` un-commits and empties, so
        // a turn that dies mid-drain cannot leave work staged for whoever runs
        // next.
        //
        // Additive beside the `approvals` handle #474 wired: they do not
        // interact — one reads a count either side of a turn, this one owns the
        // queue's write window.
        let _claim = match answering {
            true => self.queue.claim_answering(),
            false => self.queue.claim(),
        };
        // Issue #1035: the operator asked for a workflow, and the REST chat
        // handler cards on that signal whatever its triage said.
        //
        // `is_copilot_thread` is not an extra precaution — it is half of the
        // handler's own condition, and reproducing only the other half would
        // invert this fix on exactly one surface. A copilot thread is a
        // conversation ABOUT one graph, so the handler suppresses the card
        // there; a runtime that read the deliverable alone would conclude the
        // handler had carded, and stand down the paths that were the only ones
        // left to open one. The two conditions travel together or the signal
        // lies.
        let workflow_requested = self.requested_intent
            == Some(crate::ports::types::MessageIntent::Workflow)
            && !crate::company::copilot::is_copilot_thread(chat_id);
        // Issue #463: did the REST chat handler already card this message?
        //
        // A card actually persisted by the handler is the authority here.
        // Intent alone must not suppress a later tool-driven delegation.
        let handler_card = self.chat_handler_card().await?;
        let carded_by_handler = workflow_requested || handler_card.is_some();
        // Issue #1152: the mirror image of `workflow_requested` — the operator
        // said this message is not a request for work at all.
        //
        // The REST handler honours it by opening no card. This is the other half
        // of the same promise: the handler is not the only path that cards a
        // chat message, so a handler-only fix would leave "Just chatting" true on
        // an unaddressed message and false on a DM to a desk — which is worse
        // than not shipping the control, because the label would be a promise
        // the company keeps only sometimes.
        //
        // No `is_copilot_thread` term, unlike `workflow_requested` above. That
        // one reproduces half of the handler's condition because it concludes
        // "the handler already carded this", and on a copilot thread the handler
        // deliberately did not. This concludes nothing about the handler — it
        // reads the operator's own statement, which means the same thing on
        // every thread.
        let not_work = self
            .requested_intent
            .is_some_and(crate::ports::types::MessageIntent::is_chat);
        // Everything below that could open a card reads these facts about the
        // operator's message rather than re-deriving them from text that is no
        // longer the operator's (issues #463, #267, #1152).
        let ctx = MessageContext {
            carded_by_handler,
            answering,
            chatter,
            not_work,
        };
        // …and *which* card that is, when it is still on the board. Adopting it
        // is what carries "one message, one card" through the publish drain too:
        // the caller files a published deliverable onto `spawned_task` rather
        // than minting a second card beside this one (issue #463).
        //
        // Adoption is not on its own a settle. When the orchestrator answers
        // "create a workflow named X" by authoring the graph in-turn with the
        // inline `create_workflow` tool, this card used to be adopted — the
        // bubble linked to it — and then abandoned in To-do: `CreateWorkflowTool`
        // stages onto the `WorkflowRefQueue`, which was drained only by the
        // dispatched-card path in `HarnessBrain::run_task`, and the publish drain
        // settles from `pending_publishes`, which `create_workflow` never
        // populates. The operator got the workflow; the card lagged (issue #678).
        //
        // The drain below closes that, and since #806 the settled card also
        // carries a real output link: a `TaskOutput` whose source is the
        // conversation rather than a run row, naming the workflows the turn
        // authored. Run records stay reserved for actual work attempts (#183
        // §4), so this turn mints none — see `TaskOutputSource`.
        // A desk lead or teammate asked DIRECTLY opens no card by construction.
        // Issue #442 used to card anything "substantial" said to one here,
        // before their turn, because a non-orchestrator carried no tool that
        // could — and the result was that every message typed into a desk
        // became a board card nobody had asked for. Every roster agent now
        // carries `spawn_task` (and the hand-off tools) itself, so whether an
        // ask is tracked is the answering agent's decision, made with a tool
        // call, exactly as it is for the orchestrator.
        // Same discipline `run_task` keeps on the dispatched-card path: only
        // what *this* turn stages can be attributed to this turn's card, so
        // anything a previous turn left staged is dropped before the model runs
        // (issue #678). Guarded on `task.is_none()` throughout, so a dispatched
        // card's drain is never touched from here — the two paths cannot race
        // over one queue because only one of them ever reads it.
        let operator_turn = self.task.is_none();
        if operator_turn && let Some(refs) = self.workflow_refs {
            refs.clear();
        }
        // Issue #465: sampled either side of the turn so the settle below reads
        // what *this* turn parked, not what the cycle was already holding from
        // an earlier one.
        let approvals_before = self.approvals_queued();
        // Who else the message named, told to the teammate answering it.
        //
        // Appended to the turn input only — **not** to the journaled message,
        // which is already stored verbatim with its own mention rows. A reader
        // sees exactly what the author typed; the model additionally sees who
        // that resolved to, which it otherwise could not know, because a mention
        // is a structured fact about the message rather than a word in it.
        //
        // Deliberately phrased as context rather than an instruction: the turn
        // decides whether the work needs to spread, and spreads it through the
        // delegation tools it already has. Nothing here dispatches.
        let with_mentions;
        let message = if operator_turn && !self.also_mentioned.is_empty() {
            // A responder whose `delegates_to` narrows its reach past a
            // mentioned teammate cannot act on "hand work to them" — see
            // `reachable_mentioned`. Telling it to anyway is not a harmless
            // nudge: it is an instruction the tool would refuse, for a name it
            // now believes should be receiving work it never will.
            with_mentions = {
                let reachable = self.reachable_mentioned(responder);
                let unreachable: Vec<&str> = self
                    .also_mentioned
                    .iter()
                    .filter(|mentioned| !reachable.contains(mentioned))
                    .map(String::as_str)
                    .collect();
                if unreachable.is_empty() {
                    format!(
                        "{message}

[Also mentioned in this message: {}. They have not been asked to answer — you have. Hand work to them only if it genuinely needs them.]",
                        self.also_mentioned.join(", ")
                    )
                } else if reachable.is_empty() {
                    format!(
                        "{message}

[Also mentioned in this message: {}. They have not been asked to answer — you have. You have no way to hand this off to them, so answer it yourself, or say so if it genuinely needs them.]",
                        self.also_mentioned.join(", ")
                    )
                } else {
                    // Mixed: the responder can reach some of the named
                    // teammates but not others, so "hand work to them"
                    // would overstate the reach and "no way to hand off"
                    // would understate it. Name which ones are out of
                    // reach instead of asking the model to guess.
                    format!(
                        "{message}

[Also mentioned in this message: {}. They have not been asked to answer — you have. You can hand work to {}, but not to {} — answer it yourself, or say so if it genuinely needs them.]",
                        self.also_mentioned.join(", "),
                        reachable.join(", "),
                        unreachable.join(", ")
                    )
                }
            };
            with_mentions.as_str()
        } else {
            message
        };
        // Issue #1725: mark this turn chat-only when the operator's own message
        // is conversation, not work — either the explicit "Just chatting"
        // (`not_work`, i.e. `deliverable: "chat"`), a lexically MATCHED greeting
        // (`matched_chatter` — trusted directly, see its own comment above), or
        // a high-confidence greeting the model read as chatter on an abstained
        // message (gated through `is_pure_small_talk`, since the model's
        // "conversational" verdict is broader than a bare-greeting shape). The
        // harness pool reads the hint (same task, propagates through `RunTurn`)
        // and runs a cheap tool-less/memory-less/goal-less turn instead of the
        // full agentic loop. Only ever set on an operator turn — a dispatched
        // task card is always real work — and a greeting that carries a request
        // abstains via `is_pure_small_talk` (or is never lexically MATCHED
        // chatter in the first place — `GREETINGS` is a whole-message match),
        // so the card-tracking paths above are untouched.
        let chat_only = operator_turn
            && (not_work
                || matched_chatter
                || (chatter && is_pure_small_talk(operator_words(message))));
        let outcome = with_chat_only_hint(
            chat_only,
            self.run_turn.run(
                self.company,
                responder,
                message,
                self.answering_target(chat_id),
            ),
        )
        .await?;
        let parked = self.approvals_queued().saturating_sub(approvals_before);
        // The responder's own steps ride on the operator bubble; its reply is the
        // operator-facing text UNLESS a synchronous desk delegation runs, in which
        // case the relay turn's reply replaces it (below).
        let mut operator_steps = outcome.steps;
        let mut operator_reply = outcome.reply;
        // Issue #926: sticky from here to the `OperatorTurn` below — never
        // reassigned, only OR'd — so a cap the responder hit survives the relay
        // turn replacing the reply text.
        let mut hit_iteration_cap = outcome.hit_iteration_cap;
        // Issue #1032: sticky the same way, kept as first-wins — never
        // overwritten, only filled when still empty — so a spend halt the
        // responder hit survives the relay turn replacing the reply text.
        let mut halted_for_spend = outcome.halted_for_spend;
        // Issue #1846: sticky the same way, first-wins — the top-level fix
        // this issue adds. A responder whose own turn paused for lack of
        // inference budget/credits must survive the relay turn replacing the
        // reply text, exactly like a spend halt.
        let mut budget_paused = outcome.budget_paused;
        // A `spawn_task` opens a card silently; a `delegate_to_desk` runs the desk
        // lead and hands its answer back to RELAY rather than surfacing as a
        // disconnected sibling bubble. Any future delegation that surfaces its own
        // bubble lands in `bubbles`.
        let mut bubbles = Vec::new();
        let mut desk_replies: Vec<(String, String)> = Vec::new();
        let mut refused_cards: Vec<RefusedCardWrite> = Vec::new();
        // Issue #1846 review (Codex #3870516681): whether a DESK paused, kept
        // apart from the sticky `budget_paused` above. That one is already
        // carrying the responder's OWN pause, and a responder that paused on
        // the turn that queued the hand-off still got a real answer back from
        // the desk — relaying it is correct there, and is what
        // `a_responders_own_budget_pause_survives_the_relay_replacing_the_reply`
        // pins. Only a desk that paused has failed to answer.
        let mut desk_paused = false;
        // Issue #246: the first card this turn opened, which is what the
        // operator bubble reports. `get_or_insert` rather than assignment keeps
        // it the FIRST — a later spawn must not overwrite the id an earlier one
        // already claimed, or the reported card would be whichever the model
        // happened to queue last.
        //
        // The chat handler's card comes first when it opened one: it predates
        // every card this turn could open, so it IS the first card for this
        // message (issue #463). Before this it was invisible from here, which is
        // why the operator bubble linked to nothing on a recognised imperative
        // even though the board carried a card for it.
        // Cloned rather than moved: the workflow drain below settles *this*
        // card, and it has to still be readable after `spawned_task` takes it
        // (issue #678).
        let mut spawned_task: Option<String> = handler_card.clone();
        let drained = self.drain_and_execute(chat_id, ctx, HandOffs::Run).await?;
        if let Some(id) = drained.spawned_task {
            spawned_task.get_or_insert(id);
        }
        bubbles.extend(drained.bubbles);
        refused_cards.extend(drained.refused_cards);
        for desk in drained.desk_replies {
            // Fold the teammate's activity onto the operator timeline, then
            // remember the answer to relay.
            operator_steps.extend(desk.steps);
            hit_iteration_cap |= desk.hit_iteration_cap;
            halted_for_spend = halted_for_spend.or(desk.halted_for_spend);
            desk_paused |= desk.budget_paused.is_some();
            budget_paused = budget_paused.or(desk.budget_paused);
            desk_replies.push((desk.member, desk.reply));
        }
        // CEO-relay hand-back: when a synchronous desk delegation answered, run
        // exactly ONE more responder turn whose prompt is the original message
        // plus the teammate reply, and surface THAT as the operator bubble — so
        // the orchestrator comes back with the answer in one coherent
        // conversation.
        //
        // Issue #1846 review (Codex #3870516681): "answered" is the load-bearing
        // word, and a desk that paused for lack of credits has NOT answered —
        // `desk.reply` is the pause placeholder. Relaying it anyway fired a
        // second inference call at the same exhausted provider, which paused
        // too and parked a SECOND marker, this one for the responder. That
        // marker has no notice pointing at it (the operator only ever sees the
        // first delegate's pause), so it is unreachable, and being newer it can
        // supersede a live CTA on an older notice — disabling the one button
        // that would have worked. Skipped entirely instead.
        //
        // Issue #1906: what makes the skip lossless is NOT that the delegate's
        // text gets folded onto the bubble in the relay's place. #1886 wrote
        // such a fold and it never reached the operator — both of
        // `handle_operator_message`'s non-test callers (`HarnessBrain`, in the
        // interactive and the `ScheduleFired` paths) replace the whole reply
        // with `BUDGET_PAUSED_PLACEHOLDER_REPLY` whenever `budget_paused` is
        // `Some`, and `desk_paused` implies exactly that. The real reason is
        // simpler and does not depend on the caller at all: that same override
        // already discarded the RELAY's reply on every pause, so the inference
        // call it costs buys the operator nothing at a provider that has just
        // run dry. The fold is gone; see the skip branch below.
        //
        // Gated on `desk_paused`, NOT on the sticky `budget_paused`: the latter
        // also carries the RESPONDER's own pause, and a responder that paused
        // on the turn that queued the hand-off still has a real desk answer to
        // relay. Widening the gate to it would silently drop that answer.
        if !desk_replies.is_empty() && !desk_paused {
            let relay_prompt = build_relay_prompt(message, &desk_replies);
            self.queue.clear();
            let relay = self
                .run_turn
                .run(self.company, responder, &relay_prompt, self.target(chat_id))
                .await?;
            // The relay turn may only relay — a second hand-off from here is the
            // re-delegation loop this drain exists to stop, and it is dropped.
            //
            // But dropping *everything* was over-broad (issue #442): a card the
            // relay turn opened is not a re-delegation, it is the orchestrator
            // deciding — having now seen what came back — that this should be
            // tracked. Discarding that meant a card could be lost purely because
            // the turn that wanted it happened to be a relay, which is invisible
            // from the operator's side. Board writes are executed; hand-offs are
            // still dropped, so the bound stays exactly one extra turn.
            //
            // On a question turn the relay's board writes are held back too, and
            // that is deliberate rather than an oversight of the paragraph above
            // (issue #267 review). `_claim` is still live here — the relay runs
            // inside the same `claim_answering` scope as the turn that asked —
            // so `push_within_cap` refuses a relay `spawn_task` with
            // `NoDrainReason::Triage`, in the relay's own turn, and the model is
            // told this message was a question rather than that its context
            // cannot do board work. #442's reasoning does not survive the
            // narrowing: it says the relay is better placed to decide something
            // should be *tracked*, and #267 says a message the operator posed as
            // a question mints no card by any door. The relay is a door, and
            // seeing the answer first does not change who asked — replying to
            // "is the build ok?" with a card nobody asked for is the exact
            // behaviour #267 exists to stop. A relay that genuinely must
            // commission work has the same recourse the first turn has: say so,
            // and let the operator ask for it. Pinned by
            // `the_relay_turns_card_is_held_back_on_a_question_turn` and its
            // non-question sibling.
            let drained = self.drain_and_execute(chat_id, ctx, HandOffs::Drop).await?;
            if let Some(id) = drained.spawned_task {
                spawned_task.get_or_insert(id);
            }
            bubbles.extend(drained.bubbles);
            refused_cards.extend(drained.refused_cards);
            // A hand-off the relay turn's tool refused is dropped with the
            // hand-offs themselves — there is no card in scope to record it on,
            // and the drain would otherwise leak it into the next turn.
            //
            // Dropped, but not in silence. `push_refusal` exists precisely so
            // the board carries the fact independently of what the turn says
            // about it (issue #272); on this one path that independent record
            // has nowhere to go, so the log is the record.
            let refused = self.queue.drain_refusals(self.max_delegations);
            if !refused.is_empty() {
                tracing::warn!(
                    company = %self.company,
                    refused = refused.len(),
                    "[delegation] the relay turn attempted hand-offs to desks this company does not \
                     have; they are dropped with the relay's other delegations and recorded nowhere \
                     but here"
                );
            }
            operator_reply = relay.reply;
            operator_steps.extend(relay.steps);
            hit_iteration_cap |= relay.hit_iteration_cap;
            halted_for_spend = halted_for_spend.or(relay.halted_for_spend);
            // Issue #1846 review (Codex #3865395857): when the CEO-relay call
            // ITSELF runs out of credits, `run_inner`'s own default park (see
            // `mod.rs`) has already parked `relay_prompt` above — the
            // internally-generated prompt this turn was actually called
            // with — as the redeem marker's message, not the operator's own
            // words. Redeeming that submits the internal relay prompt as a
            // fresh human-authored `OperatorMessage`, potentially executing a
            // different request than the one the operator asked for.
            //
            // `run_hand_off` already re-parks a delegate's pause with the
            // right text via `self.reissue_message`; this is that fix's
            // sibling for the relay call, which carries no `reissue_message`
            // of its own — the "original" text here is simply `message`, the
            // parameter this whole turn started from.
            //
            // Read BEFORE the fold below moves `relay.budget_paused` into
            // `budget_paused`.
            if let Some(pause) = &relay.budget_paused {
                // Issue #1846 review (Codex #3865812419/#3865812423/
                // #3865812432): the ambient parent/deliverable/mentions the
                // cycle was started with, so a redeem replays the operator's
                // ORIGINAL thread/intent/audience.
                //
                // Issue #1846 review (Codex #3866418891): `message` is the
                // COMPOSED text (`with_attachment_refs` markers already
                // baked in) — the ambient context's own raw text +
                // structured attachments are preferred whenever this cycle
                // carries an `OperatorMessage`, so a redeem recomposes fresh
                // instead of doubling the attachment markers on top of the
                // ones already baked into `message`.
                let redeem_context = crate::runtime::grants::current_redeem_context();
                let park_message = redeem_context
                    .text
                    .clone()
                    .unwrap_or_else(|| message.to_string());
                let marker = crate::runtime::grants::budget_pauses_for(self.company)
                    .park_preserving_background(
                        pause.agent.clone(),
                        chat_id.map(str::to_string),
                        park_message,
                        pause.summary.clone(),
                        now_millis(),
                        redeem_context,
                    );
                tracing::info!(
                    company = %self.company,
                    agent = %pause.agent,
                    marker_id = %marker.id,
                    background = marker.background,
                    "[budget-pause] re-parked the CEO-relay's pause with the original operator \
                     message, replacing the relay prompt `run_inner` parked by default"
                );
            }
            budget_paused = budget_paused.or(relay.budget_paused);
        } else if !desk_replies.is_empty() {
            // The relay was skipped because a desk paused (see above), so the
            // operator bubble stays the responder's own reply — which the
            // caller overwrites with the pause placeholder before it is ever
            // rendered. Nothing is appended here on purpose (issue #1906): a
            // fold cannot outlive that override, and the pause itself travels
            // to the operator on `budget_paused`, not on this string.
            //
            // The one thing that IS worth doing on this path is the refusal
            // drain the relay branch does at its tail. Issue #1906: the relay
            // branch's copy runs only when a relay ran, so on the skip path a
            // hand-off the responder aimed at a desk this company does not have
            // left no trace anywhere. `DelegationClaim`'s drop clears the scope
            // either way, so nothing leaks — what is lost without this is the
            // log line, and that log line is the record (issue #272's reasoning,
            // same as the relay branch's). This branch is only reached when at
            // least one desk answered; a turn whose hand-offs were all refused,
            // and the relay branch's own `queue.clear()`, both predate this
            // drain — unchanged from before #1906.
            let refused = self.queue.drain_refusals(self.max_delegations);
            if !refused.is_empty() {
                tracing::warn!(
                    company = %self.company,
                    refused = refused.len(),
                    "[delegation] the responder attempted hand-offs to desks this company does \
                     not have; the relay turn that would otherwise have logged them was skipped \
                     for a budget pause, so they are recorded nowhere but here"
                );
            }
        }
        for unknown in refused_cards {
            operator_reply.push_str(&format!(
                "\n\n(tried to {} card {:?}, but {})",
                unknown.tool, unknown.task_id, unknown.reason
            ));
        }
        // Drained after the relay, not before it: a relay turn carries the same
        // inline `create_workflow` tool, so draining at the responder's turn
        // would settle the card without the workflow the relay went on to author
        // (issue #678).
        if operator_turn && let Some(refs) = self.workflow_refs {
            let authored = refs.drain();
            if !authored.is_empty() {
                self.settle_authored_workflow_card(
                    handler_card.as_deref(),
                    responder,
                    parked,
                    &authored,
                    chat_id,
                )
                .await;
            }
        }
        Ok(OperatorTurn {
            reply: operator_reply,
            steps: operator_steps,
            bubbles,
            spawned_task,
            hit_iteration_cap,
            halted_for_spend,
            budget_paused,
        })
    }

    /// Settles the adopted handler card for workflows this turn authored inline
    /// (issue #678).
    ///
    /// # Best-effort on purpose
    ///
    /// Returns `()`, never `Result`. The operator's reply is already written and
    /// the workflow is already saved; a task-store hiccup here must not sink
    /// either, the same call the chat handler makes when its own card write
    /// fails. A card left in To-do is the pre-#678 behaviour, so the failure
    /// mode of this function is exactly the bug it fixes — never something
    /// worse.
    ///
    /// # The output link (issue #806)
    ///
    /// The card is stamped with a [`TaskOutput`] whose source is
    /// [`TaskOutputSource::ChatTurn`] — the conversation this turn happened in,
    /// which is `chat_id`, not the card's `origin_chat_id` (which since issue
    /// #982 may carry the same thread, and never a different one) — carrying the
    /// workflows this turn authored.
    /// The note stays — it is prose a person reads — but the *link* is what the
    /// board's contract is written in terms of (#339), and until #806 this card
    /// could not have one: `TaskOutput` required a `run_id` and an operator chat
    /// turn has no run row. Minting one was rejected deliberately; see
    /// [`TaskOutputSource`].
    async fn settle_authored_workflow_card(
        &self,
        handler_card: Option<&str>,
        responder: &str,
        parked: usize,
        authored: &[TaskOutputWorkflow],
        chat_id: Option<&str>,
    ) {
        let Some(task_id) = handler_card else {
            // The turn authored a workflow with no card in scope to record it
            // on — the operator asked conversationally rather than in the shape
            // #463 cards. Nothing to settle, and the workflow is saved either
            // way; logged because a *missing* card here is the only signal that
            // the two heuristics disagreed about the same message.
            tracing::debug!(
                company = %self.company,
                authored = authored.len(),
                "[delegation] a chat turn authored workflows with no handler card to settle"
            );
            return;
        };
        let body = authored
            .iter()
            .map(|w| match (&w.action, w.run_id.as_deref()) {
                (TaskOutputAction::Ran, Some(run)) => {
                    format!("Ran workflow `{}` (run {run}).", w.workflow_id)
                }
                (TaskOutputAction::Ran, None) => format!("Ran workflow `{}`.", w.workflow_id),
                (TaskOutputAction::Created, _) => {
                    format!("Created workflow `{}`.", w.workflow_id)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let loaded = match self.load_card(task_id).await {
            Ok(Some(loaded)) => Some(loaded),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    company = %self.company,
                    task_id = %task_id,
                    error = %err,
                    "[delegation] could not read the card for a workflow this turn authored; it \
                     stays where it is"
                );
                return;
            }
        };
        let Some((_, mut card)) = loaded else {
            tracing::warn!(
                company = %self.company,
                task_id = %task_id,
                "[delegation] the card adopted for this turn is no longer on the board; the \
                 workflows it authored are recorded nowhere but here"
            );
            return;
        };
        // Issue #806: the link, stamped before the settle writes the card so one
        // store round-trip carries both.
        //
        // The source is the conversation, not a run: this turn made no work
        // attempt, and #183 §4 keeps run records meaning exactly that. Written
        // wholesale like every other stamp, so a card the orchestrator later
        // works for real overwrites this with the run that did it — a chat turn
        // is the weakest producer, never one that outranks an attempt.
        //
        // The TURN's own `chat_id` is what addresses it — deliberately not the
        // card's `origin_chat_id`. Since issue #982 an adopted handler card may
        // carry one, and when it does it is this same thread by construction
        // (adoption requires it), so the two agree; reading the turn's is still
        // the honest source, because it is the conversation this stamp is about
        // and it is defined even for a card that carries no origin. A turn with
        // no thread at all (a dispatched path) keeps the note-only behaviour
        // rather than getting a stamp pointing nowhere.
        match chat_id {
            Some(chat_id) => {
                card.output = Some(TaskOutput {
                    source: TaskOutputSource::ChatTurn {
                        chat_id: chat_id.to_string(),
                    },
                    at_millis: now_millis(),
                    artifacts: Vec::new(),
                    workflows: authored.to_vec(),
                });
            }
            None => {
                tracing::debug!(
                    company = %self.company,
                    task_id = %task_id,
                    "[delegation] this turn has no chat thread to address, so its card is \
                     recorded in the note without an output link"
                );
            }
        }
        if let Err(err) = self
            .settle_work_card(&mut card, responder, TaskRunEnd::Completed, parked, &body)
            .await
        {
            tracing::warn!(
                company = %self.company,
                task_id = %task_id,
                error = %err,
                "[delegation] could not settle the card for a workflow this turn authored; it \
                 stays in its current column"
            );
        }
    }

    /// Drains the queue (capped, discarded past the cap) and executes every
    /// delegation on it, reporting what came back (issue #453).
    ///
    /// # Why this is a function
    ///
    /// It used to be a loop written out twice inside
    /// [`handle_operator_message`](Self::handle_operator_message), which meant a
    /// path that ran a turn without copying that loop drained nothing — and two
    /// production paths did exactly that. The approval re-dispatch runs a full
    /// toolbelt turn and claimed only publishes, so a `review_task` made from a
    /// re-issued call was staged, never executed, and destroyed by the next
    /// turn's clear. Extracting the loop is what lets that path reuse the *exact*
    /// execution semantics — the cap, the first-wins card id, the refusal
    /// handling — instead of a near-copy that drifts.
    ///
    /// **This is not the guarantee.** The guarantee is
    /// [`DelegationQueue::claim`]: a caller that forgets to drain gets an
    /// in-turn refusal at the tool rather than a receipt for work that will not
    /// happen. This is the convenience that makes doing it right the short path.
    ///
    /// Refused desk keys are deliberately NOT drained here — they are recorded
    /// on the card by [`handle_task_delegations`](Self::handle_task_delegations)
    /// and logged by the relay path, and those are two different treatments of
    /// one fact rather than something a shared drain can decide.
    pub(crate) async fn drain_and_execute(
        &self,
        chat_id: Option<&str>,
        ctx: MessageContext,
        hand_offs: HandOffs,
    ) -> Result<Drained> {
        let mut drained = Drained::default();
        let mut conversation_dispatches = Vec::new();
        for delegation in self.queue.drain(self.max_delegations) {
            if hand_offs == HandOffs::Drop
                && let Some(target) = hand_off_target_of(&delegation)
            {
                tracing::debug!(
                    company = %self.company,
                    target = %target,
                    "[delegation] dropped a hand-off queued by the relay turn: a relay may only \
                     relay"
                );
                continue;
            }
            if matches!(delegation, Delegation::ConversationDispatch { .. }) {
                conversation_dispatches.push(delegation);
                continue;
            }
            // Captured before the delegation is consumed, so a cancellation can
            // be reported against whoever it was aimed at (issues #176, #884).
            let target = hand_off_target_of(&delegation).map(str::to_string);
            let out = self.run_delegation(delegation, chat_id, ctx).await?;
            drained.absorb(out, target);
        }
        // Separate committed DM messages are independent one-target decisions.
        // Run them together: distinct agents proceed concurrently, while two
        // messages to the same agent serialize on that agent's own session lock.
        let dispatched = futures::future::join_all(conversation_dispatches.into_iter().map(
            |delegation| async move {
                let target = hand_off_target_of(&delegation).map(str::to_string);
                self.run_delegation(delegation, chat_id, ctx)
                    .await
                    .map(|out| (out, target))
            },
        ))
        .await;
        for outcome in dispatched {
            let (out, target) = outcome?;
            drained.absorb(out, target);
        }
        if self.queue.has_queued() {
            let nested = Box::pin(self.drain_and_execute(chat_id, ctx, hand_offs)).await?;
            drained.merge(nested);
        }
        Ok(drained)
    }

    /// Drains and executes whatever a **dispatched card's** turn queued (issue
    /// #204), and reports the hand-off when one happened.
    ///
    /// Before this, `run_task` ran exactly one background turn and never
    /// touched the queue — so when the dispatched responder (the orchestrator,
    /// which carries the delegation tools) called `delegate_to_desk`, the
    /// delegation was silently dropped, the turn still returned `Ok`, and the
    /// card landed in `in_review` under the delegator with a blank assignee and
    /// no delegate ever having run.
    ///
    /// The first hand-off to a desk with a resolvable lead that **produces
    /// something** owns the card: the card is reassigned to that lead and
    /// persisted in [`COLUMN_IN_PROGRESS`](lifecycle::COLUMN_IN_PROGRESS)
    /// *before* their turn starts, so the board shows who is working it while
    /// they work it, and the caller settles the card from their output
    /// afterwards.
    ///
    /// An earlier hand-off that produced nothing (its run was cancelled
    /// mid-flight) holds the card only *provisionally* — a later hand-off that
    /// answers takes it over. Otherwise the card would settle from the
    /// cancellation while work that actually ran was merely appended to the
    /// note, filing a real deliverable under a card marked cancelled.
    ///
    /// Every other delegation — `spawn_task`, `assign_task`, `review_task`, a
    /// hand-off to a desk nobody leads, and any further hand-off once one has
    /// answered — executes for its side effect; a later hand-off's answer is
    /// appended to the note so it is recorded rather than silently discarded.
    ///
    /// `chat_id` is `None` throughout: a dispatched card has no chat thread, and
    /// stamping one would make a spawned card post back into an unrelated
    /// conversation.
    pub(crate) async fn handle_task_delegations(
        &self,
        card: &mut TaskRecord,
        delegator: &str,
    ) -> Result<Option<TaskHandoff>> {
        // A hand-off the tool refused (issue #272) never becomes a
        // `Delegation`, so it is read separately — and recorded on the card
        // before anything else, because the turn's own account of it is exactly
        // what cannot be trusted: a refused hand-off is precisely the case where
        // the reply claimed work had changed hands and the board showed
        // otherwise.
        for target in self.queue.drain_refusals(self.max_delegations) {
            tracing::warn!(
                task_id = %card.id,
                delegator = %delegator,
                "[task] a hand-off was refused before it could be queued"
            );
            card.note = Some(append_note(
                card.note.as_deref(),
                delegator,
                &undeliverable_handoff(
                    &target,
                    delegator,
                    // Kind-agnostic since #884: this list holds both refused desk
                    // keys and refused teammate ids, and the refusal that
                    // recorded them is not carried through the queue.
                    "it is not somewhere this company can hand work to",
                ),
            ));
        }
        let queued = self.queue.drain(self.max_delegations);
        if queued.is_empty() {
            return Ok(None);
        }
        tracing::debug!(
            task_id = %card.id,
            delegator = %delegator,
            queued = queued.len(),
            "[task] draining delegations queued by a dispatched turn"
        );
        let mut handoff: Option<TaskHandoff> = None;
        for delegation in queued {
            // Resolve the hand-off target BEFORE running it, so the card can be
            // reassigned while the delegate works. `desk_lead` is pure, so the
            // second resolution inside `run_delegation` yields the same member.
            let lead = match &delegation {
                Delegation::DelegateToDesk { desk, .. } => desk_lead(self.record, desk),
                // Issue #884: resolved directly, with no desk in between — which
                // is the point. `resolve_teammate_key` is pure over the same
                // record, so the second resolution inside `run_delegation`
                // yields the same member, exactly as `desk_lead` does above.
                //
                // It grounds the display-name half of the roster too (#1162).
                // The tool now queues the canonical id, so on the ordinary path
                // this is the identity — but `ground` fails open for the
                // orchestrator when the record cannot be read, and that path
                // queues the key exactly as the model wrote it. Resolving the
                // same way here is what stops a name that reached the queue
                // from being dropped at the drain.
                Delegation::DelegateToTeammate { teammate, .. } => {
                    self.record.resolve_teammate_key(teammate).agent()
                }
                _ => None,
            };
            let Some(member) = lead else {
                // A hand-off whose desk resolves to no lead cannot be
                // delivered. #213 settles the card under the delegator rather
                // than stranding it, which is right — but until #272 it did so
                // silently, leaving a card whose note claimed a hand-off that
                // never happened and whose assignee was the delegator, with
                // nothing on the board connecting the two. Record the
                // undeliverable hand-off on the card so the operator reads the
                // fact instead of inferring it from an absence. Every other
                // delegation kind carries no target and is unaffected.
                //
                // The cause is written per kind (issue #884): "that desk has no
                // lead" and "that teammate is not on the roster" are different
                // facts, and an operator reading the card is the one who has to
                // act on whichever it was.
                let undeliverable = hand_off_target_of(&delegation).map(|target| {
                    let cause = match delegation {
                        Delegation::DelegateToTeammate { .. } => {
                            "no teammate with that id is on the roster"
                        }
                        _ => "no desk with that id has a lead on the roster",
                    };
                    (target.to_string(), cause)
                });
                let outcome = self
                    .run_delegation(delegation, None, MessageContext::default())
                    .await?;
                if let Some(refused) = outcome.refused_card {
                    card.note = Some(append_note(
                        card.note.as_deref(),
                        delegator,
                        &format!(
                            "{} refused for card {:?}: {}",
                            refused.tool, refused.task_id, refused.reason
                        ),
                    ));
                }
                if let Some((target, cause)) = undeliverable {
                    card.note = Some(append_note(
                        card.note.as_deref(),
                        delegator,
                        &undeliverable_handoff(&target, delegator, cause),
                    ));
                }
                continue;
            };
            // The card belongs to the first hand-off that actually PRODUCES
            // something. A hand-off whose run was cancelled produced nothing, so
            // it does not get to keep the card: it would settle `Cancelled` ->
            // `todo` while a later hand-off that really ran had its answer
            // merely appended to the note — filing work that happened under a
            // card marked cancelled. So an empty hand-off is *provisional* and a
            // later one that answers takes the card over from it (issue #213
            // review finding 3).
            // Once a hand-off owns the card, no LATER one in this turn runs.
            //
            // The synchronous model could let a second hand-off run and take the
            // card from an empty first (issue #213 finding 3). Asynchronously it
            // cannot: the first hand-off's delegate is already dispatched, so a
            // second that ran here would produce work under a card whose owner
            // is somebody else — and if that owner is then cancelled, the card
            // settles `todo` carrying output that really did run. That is the
            // exact "work filed under a card marked cancelled" #213 fixed,
            // reached the other way round.
            //
            // So it is recorded and not started. One card, one owner, one
            // dispatch — and the operator can see what else was asked for.
            if handoff.as_ref().is_some_and(|prior| prior.pending) {
                if let Some(target) = hand_off_target_of(&delegation) {
                    card.note = Some(append_note(
                        card.note.as_deref(),
                        delegator,
                        &format!(
                            "also asked {target}: {} — not started, this card is already with \
                             its new owner",
                            instruction_of(&delegation)
                        ),
                    ));
                }
                continue;
            }
            // A hand-off that produced nothing is PROVISIONAL and a later one
            // that answers takes the card from it (issue #213 finding 3) — but
            // a PENDING hand-off is not "produced nothing", it is "has not run
            // yet". It already owns the card and the delegate is about to be
            // dispatched for it, so a second hand-off in the same turn must not
            // move the card again; its instruction is recorded on the note
            // instead, exactly as a non-owning hand-off's answer is.
            let owns_card = handoff
                .as_ref()
                .is_none_or(|prior| prior.reply.is_none() && !prior.pending);
            if owns_card {
                self.hand_card_over(card, delegator, &member, instruction_of(&delegation))
                    .await?;
                // SPIKE: hand over and STOP. The delegate is not run inside this
                // attempt — the card now names them, the delegator settles
                // `Delegated`, and the dispatch edge re-fires for the new owner.
                //
                // What that buys, and why the synchronous version could not:
                // one attempt row per agent (so cost is attributable), the
                // per-company cycle lock released between hops, and a card
                // whose `assignee` is a real reassignment rather than a
                // mid-turn display concession.
                handoff = Some(TaskHandoff {
                    delegate: member,
                    reply: None,
                    budget_paused: None,
                    pending: true,
                });
                continue;
            }
            let outcome = self
                .run_delegation(delegation, None, MessageContext::default())
                .await?;
            match (owns_card, outcome.desk_reply, outcome.cancelled) {
                // The delegate answered: they own the card and it settles from
                // their output.
                (true, Some(desk), _) => {
                    handoff = Some(TaskHandoff {
                        delegate: member,
                        reply: Some(desk.reply),
                        budget_paused: desk.budget_paused,
                        pending: false,
                    });
                }
                // An operator cancelled their run mid-flight, so it produced
                // nothing. Reported as a cancellation because `run_delegation`
                // said it was one — not because the reply is missing.
                (true, None, true) => {
                    handoff = Some(TaskHandoff {
                        delegate: member,
                        reply: None,
                        budget_paused: None,
                        pending: false,
                    });
                }
                // Nothing produced and NOT a cancellation. `run_delegation`'s
                // only other empty exit for a hand-off is a desk with no
                // resolvable lead, which cannot be reached here — the lead
                // resolved above and `desk_lead` is pure over the same record.
                // If it ever becomes reachable, this reports *no hand-off*, so
                // the delegator's own reply settles the card exactly as an
                // unresolvable desk already does, rather than telling the
                // operator their run was cancelled when it was not.
                (true, None, false) => {}
                // A later hand-off does not take the card over, but its answer
                // is real work — record it rather than dropping it.
                (false, Some(desk), _) => {
                    card.note = Some(append_note(card.note.as_deref(), &member, &desk.reply));
                }
                (false, None, _) => {}
            }
        }
        Ok(handoff)
    }

    /// Runs one hand-off: the delegate's turn, the card that tracks it, the
    /// steer registration that lets an operator cancel it, the nested drain of
    /// whatever it hands on in turn, and the [`DeskReply`] the relay folds into
    /// the operator's answer.
    ///
    /// Shared verbatim by both hand-off kinds (issue #884). The two arms above
    /// differ only in how they resolve a target to a roster member and what they
    /// push onto the scope chain; everything a hand-off *is* — tracked, steerable,
    /// depth-bounded, relayed — must be identical for both, and the only way to
    /// keep it identical is for there to be one copy of it.
    async fn run_hand_off(
        &self,
        hand_off: HandOff,
        chat_id: Option<&str>,
        ctx: MessageContext,
    ) -> Result<DelegationOutcome> {
        let HandOff {
            member,
            instruction,
            label,
            scope_key,
        } = hand_off;
        // Issue #442, path two: the card is opened HERE, before the desk
        // lead runs, as a consequence of work being handed off — not
        // because the model reached for `spawn_task` instead of this
        // tool. `spawn_task` and `delegate_to_desk` were both described
        // to the model as delegation and only one of them touched the
        // board, so a hand-off produced a real deliverable and left
        // nothing behind. Both now do.
        //
        // Nothing is opened when the drain is already running inside a
        // dispatched card (that card *is* the tracking, and #204 hands it
        // over to this delegate below), when the REST chat handler
        // already carded the operator message this hand-off came out of
        // (issue #463), or when the instruction is not a piece of work —
        // see `is_trackable_work`.
        let mut card = self
            .open_hand_off_work_card(&member, &instruction, chat_id, ctx)
            .await?;
        // Register the delegated turn so an operator can CANCEL it
        // mid-flight (cancel-only in v1 — pause/redirect are rejected at
        // the route). RAII guard deregisters on every exit path.
        let guard = self.steer.register(
            self.company,
            InflightEntry {
                key: generate_id(),
                task_id: None,
                kind: InflightKind::Delegation,
                title: label,
                agent_id: member.clone(),
                started_at_millis: now_millis(),
                pending_action: None,
            },
        );
        let control = guard.control().clone();
        // Issue #176: enter this hand-off's scope BEFORE the member's
        // turn runs, so a hand-off the member itself calls is validated
        // at the depth it is actually running at — and against a chain
        // that already contains this one, which is what makes A→B→A a
        // detectable cycle. The guard pops on every exit path below,
        // including the `?`s and the cancellation return.
        //
        // A RESOLVED identity, never the key the model typed: the chain
        // is compared by identity, and "Content desk" and "content" are
        // the same desk. Issue #884 namespaces the teammate form
        // (`agent:<id>`, see `teammate_scope_key`) so the desk guard and
        // the teammate guard cannot read each other's entries.
        let _scope = self.queue.enter_scope(scope_key);
        // Issue #465, same sampling as the direct-answer path: a desk
        // delegation whose first call parks has produced nothing to
        // review either.
        let approvals_before = self.approvals_queued();
        // Issue #176, the same before/after shape: a hand-off the
        // MEMBER's tool refused must be attributed to the member, not
        // swept up with whatever its delegator left unread.
        let refusals_before = self.queue.refusals_queued();
        let outcome = match self
            .run_turn
            .run_steered(
                self.company,
                &member,
                &instruction,
                &control,
                self.target(chat_id),
                // Issue #242: when this drain is running inside a
                // dispatched card, the delegate's turn is part of that
                // card's attempt — its steps and its spend belong to the
                // same run. `None` for a chat-path delegation.
                self.run_sink.clone(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(crate::error::OpenCompanyError::InvalidRequest(msg))
                if control.pending().is_some() && msg.contains("cancelled") =>
            {
                // A queued ACP turn cancelled before it started returns
                // InvalidRequest from AcpRunTurn (the "cancelled before it
                // started" path). The `?` on run_steered would propagate
                // that as a harness error, bypassing the control.take()
                // branch below that returns cancelled: true — so the
                // cancellation disposition is lost. Catch it here and
                // produce the cancellation outcome directly.
                self.queue.clear();
                if let Some(card) = card.as_mut() {
                    self.settle_work_card(
                        card,
                        &member,
                        TaskRunEnd::Cancelled,
                        // No approvals could have been queued: the turn
                        // never started, so nothing asked for approval.
                        0,
                        "the turn was cancelled before it started",
                    )
                    .await?;
                }
                return Ok(DelegationOutcome {
                    cancelled: true,
                    spawned_task: card.map(|c| c.id),
                    ..DelegationOutcome::default()
                });
            }
            Err(err) => return Err(err),
        };
        // Issue #1846 review (Codex #3864988176): `run_inner`'s own park (mod.rs)
        // parks whatever it was CALLED with as the delegate's turn message —
        // here, `&instruction`, the model-generated hand-off brief, not the
        // operator's own words. Redeeming that marker would re-dispatch the
        // hand-off instruction as a brand-new human-authored `OperatorMessage`,
        // which can name a materially different task than what the operator
        // actually asked for.
        //
        // `BudgetPauseSet::park` overwrites by agent id (at most one marker per
        // agent), so re-parking here with the correct text — when the caller
        // gave us one via `reissue_message` — simply replaces the wrong entry
        // rather than requiring `run_inner` to know which text is "the
        // original" across every caller it serves.
        if let Some(pause) = &outcome.budget_paused
            && let Some(original) = &self.reissue_message
        {
            // Issue #1846 review (Codex #3865812419/#3865812423/#3865812432):
            // the ambient parent/deliverable/mentions the cycle was started
            // with, so a redeem replays the operator's ORIGINAL
            // thread/intent/audience.
            //
            // Issue #1846 review (Codex #3866418891): `original`
            // (`self.reissue_message`) is the SAME composed text brain.rs
            // built with `with_attachment_refs` — markers already baked in.
            // The ambient context's own raw text + structured attachments
            // are preferred whenever this cycle carries an
            // `OperatorMessage`, so a redeem recomposes fresh instead of
            // doubling the attachment markers on top of the ones already
            // baked into `original`. Falls back to `original` only for the
            // (untested-in-practice) case where a caller set
            // `reissue_message` outside any ambient `OperatorMessage` scope.
            let redeem_context = crate::runtime::grants::current_redeem_context();
            let park_message = redeem_context
                .text
                .clone()
                .unwrap_or_else(|| original.clone());
            let marker = crate::runtime::grants::budget_pauses_for(self.company)
                .park_preserving_background(
                    pause.agent.clone(),
                    chat_id.map(str::to_string),
                    park_message,
                    pause.summary.clone(),
                    now_millis(),
                    redeem_context,
                );
            tracing::info!(
                company = %self.company,
                agent = %pause.agent,
                marker_id = %marker.id,
                background = marker.background,
                "[budget-pause] re-parked the delegated pause with the original request, \
                 replacing the hand-off instruction `run_inner` parked by default"
            );
        }
        let parked = self.approvals_queued().saturating_sub(approvals_before);
        // A cancel issued mid-flight discards the delegated reply —
        // nothing is relayed. Flagged as a cancellation so a caller that
        // has to explain the empty result can name the cause instead of
        // guessing at it (issue #213 review).
        if matches!(control.take(), Some(SteerAction::Cancel)) {
            // Anything the cancelled member queued before it was stopped
            // is dropped with it (issue #176). The outer drain already
            // moved its own items into a local vector, so the queue holds
            // nothing but this member's pushes; leaving them would run
            // them under the NEXT sibling hand-off's scope, attributing
            // one member's work to another.
            self.queue.clear();
            // The card this hand-off opened outlives the cancellation:
            // settling it returns it to To-do with the cancellation on
            // its note, so an operator sees the work was asked for and
            // stopped rather than the card vanishing with the reply.
            if let Some(card) = card.as_mut() {
                self.settle_work_card(
                    card,
                    &member,
                    TaskRunEnd::Cancelled,
                    parked,
                    "the run was cancelled mid-flight",
                )
                .await?;
            }
            return Ok(DelegationOutcome {
                cancelled: true,
                spawned_task: card.map(|c| c.id),
                ..DelegationOutcome::default()
            });
        }
        // A hand-off the member's own tool REFUSED never becomes a
        // `Delegation`, so it is read separately — and read here, before
        // the nested drain, because that drain runs turns of its own
        // which can push refusals a further level down.
        let refused = self
            .queue
            .drain_refusals_after(refusals_before, self.max_delegations);
        // Issue #176: run whatever the MEMBER queued during its own turn,
        // one level deeper, before the card is settled.
        //
        // This is the half without which the whole feature is a receipt
        // for work that never happens (#453's failure, one level down):
        // the member's tool told it the hand-off "will be answered this
        // turn", and if nobody drains here the delegation is destroyed by
        // the next `clear()` with the member none the wiser.
        //
        // `Box::pin` is mandatory, not stylistic — this is async
        // recursion (`run_delegation` → `drain_and_execute` →
        // `run_delegation`) and an unboxed cycle is an
        // infinitely-sized future the compiler rejects. It also keeps the
        // stack flat, which this repo has been bitten by before.
        //
        // Bounded by construction: each level pushes onto the scope chain
        // and `push_within_cap` refuses past `max_delegation_depth`,
        // which validation caps at 4.
        //
        // `ctx` is threaded through unchanged. `carded_by_handler` and
        // `answering` are properties of the OPERATOR's message, and they
        // stay true at every depth — a question the operator asked is
        // still a question three desks down, and must still mint no card.
        let nested = Box::pin(self.drain_and_execute(chat_id, ctx, HandOffs::Run)).await?;
        // Fold the nested answers INTO this member's reply rather than
        // giving each level its own relay turn. The top-level CEO-relay
        // already synthesises every desk reply into one coherent answer
        // for the operator, so a per-level relay would only multiply
        // turns to reach the same text. Steps ride along the same way, so
        // the operator's timeline shows the deeper member working.
        let mut reply = outcome.reply;
        let mut steps = outcome.steps;
        // Issue #926: the cap folds in exactly as the reply and steps do. A
        // deeper delegate that stopped half-done is folded into THIS member's
        // answer, so its pause is a pause on what the operator ends up reading.
        let mut hit_iteration_cap = outcome.hit_iteration_cap;
        // Issue #1032: and so does the spend halt. This is the fold that makes
        // a halt two levels down reach the operator at all — the deeper reply is
        // folded into THIS member's text, so without carrying its halt with it
        // the operator reads an answer whose missing half was cut for money and
        // is told nothing. First-wins, so the shallower halt (the one nearest
        // the answer the operator reads) is the one named.
        let mut halted_for_spend = outcome.halted_for_spend;
        // Issue #1846: folded exactly as `halted_for_spend` is, first-wins, for
        // the same reason — a deeper delegate's pause is folded INTO this
        // member's answer, and there is one figure worth naming per bubble.
        let mut budget_paused = outcome.budget_paused;
        for deeper in nested.desk_replies {
            reply.push_str(&format!(
                "\n\n{} (delegated by {member}) replied:\n{}",
                deeper.member, deeper.reply
            ));
            steps.extend(deeper.steps);
            hit_iteration_cap |= deeper.hit_iteration_cap;
            halted_for_spend = halted_for_spend.or(deeper.halted_for_spend);
            budget_paused = budget_paused.or(deeper.budget_paused);
        }
        // A cancelled nested run folds in as a cancellation, NEVER as a
        // reply: the member said it was handing that slice on, and an
        // answer that silently omits the branch is the confident
        // falsehood the delegation stack exists to prevent.
        for target in nested.cancelled_desks {
            reply.push_str(&format!(
                "\n\n({target} was handed a slice of this by {member}, but that run \
                 was cancelled before it replied)"
            ));
        }
        // A hand-off the member's OWN tool refused — an unknown desk, one
        // outside its allowlist, or one that would loop — never becomes a
        // `Delegation`, so without this the only record is the tool
        // result the member is free to describe however it likes. Folding
        // it into the reply puts it on the card note and in front of the
        // operator, the same independence #272 gave the delegator's
        // refusals.
        for target in refused {
            reply.push_str(&format!(
                "\n\n({member} tried to hand a slice of this to {target}, but that \
                 hand-off was refused and did not happen)"
            ));
        }
        for unknown in nested.refused_cards {
            reply.push_str(&format!(
                "\n\n({member} tried to {} card {:?}, but {})",
                unknown.tool, unknown.task_id, unknown.reason
            ));
        }
        // Issue #1846 review (Codex #3865395868): this hand-off's own card
        // (opened above by `open_hand_off_work_card`, distinct from any
        // dispatched-card the delegation is nested inside) must settle
        // `Paused` too when the member's turn — or a deeper delegate's,
        // folded in above — ran out of credits, same as `direct_card` does.
        // Otherwise a chat-created hand-off card lands in In Review with a
        // budget-pause notice standing in for a real completed answer.
        if let Some(card) = card.as_mut() {
            let end = if budget_paused.is_some() {
                TaskRunEnd::Paused
            } else {
                TaskRunEnd::Completed
            };
            self.settle_work_card(card, &member, end, parked, &reply)
                .await?;
        }
        // Hand the teammate's answer back to RELAY through a second
        // orchestrator turn (the CEO-relay hand-back). Their steps ride
        // along and get folded onto the relayed operator bubble.
        Ok(DelegationOutcome {
            bubble: None,
            bubbles: Vec::new(),
            // Not a board write; see `DelegationOutcome::assigned`.
            assigned: false,
            desk_reply: Some(DeskReply {
                member,
                reply,
                steps,
                hit_iteration_cap,
                halted_for_spend,
                budget_paused,
            }),
            cancelled: false,
            // Issue #442: the hand-off's own card, reported the same way
            // a `spawn_task` reports its card — so the operator bubble
            // says a card was opened whichever hand-off the orchestrator
            // chose. This is the field the console's "Card opened" chip
            // renders from.
            //
            // First-wins across the nested drain too (issue #176): this
            // hand-off's own card predates anything the member opened one
            // level down, so it stays the reported one; a card the member
            // opened is reported only when this hand-off opened none.
            spawned_task: card.map(|c| c.id).or(nested.spawned_task),
            refused_card: None,
        })
    }

    /// Opens the board card that tracks a piece of work, **before** the turn
    /// that does it runs (issue #442).
    ///
    /// This is the whole fix in one method: the card is opened by the runner as
    /// a structural consequence of work being handed to an agent, rather than
    /// by the model happening to reach for the card-shaped tool. Every caller
    /// that is about to run somebody's turn goes through here first, so there is
    /// no path on which work starts and the board stays empty.
    ///
    /// Returns `None` — no card, nothing to settle — in exactly six cases:
    ///
    /// * **no task store wired**, the silent no-op every task path on this seam
    ///   takes;
    /// * **already inside a dispatched card** (`for_task`), which is the card;
    ///   opening a second one would double-count one piece of work;
    /// * **the operator said this is not work** (`not_work`, issue #1152) — they
    ///   sent the message under "Just chatting";
    /// * **the model read this as conversation** (`chatter`, issue #984);
    /// * **the chat handler already carded this message** (`carded_by_handler`,
    ///   issue #463) — see [`handle_operator_message`](Self::handle_operator_message);
    /// * **nothing substantial was asked** — see [`is_trackable_work`]; this is
    ///   the carve-out that keeps a trivial question from minting a card;
    /// * the write failed, which propagates rather than returning `None`.
    ///
    /// # What `not_work` does NOT do (issue #1152)
    ///
    /// It stands down the paths that open a card **by construction**. The
    /// orchestrator's own `spawn_task` tool is untouched: narrowing the board
    /// tools would change which delegation-queue claim the turn runs under, and
    /// "this is not a work request" is not a reason to take the company's tools
    /// away mid-conversation. So it means the company will not *automatically*
    /// card the message, not that a card can never appear.
    ///
    /// The write goes through the [`TaskStore`] port rather than
    /// `CompanyRuntime::upsert_task`, so landing the card straight in
    /// [`COLUMN_IN_PROGRESS`](lifecycle::COLUMN_IN_PROGRESS) cannot re-fire the
    /// `column → in_progress` dispatch edge — the agent is already running it.
    async fn open_work_card(
        &self,
        assignee: &str,
        request: &str,
        chat_id: Option<&str>,
        ctx: MessageContext,
    ) -> Result<Option<TaskRecord>> {
        let Some(tasks) = self.tasks else {
            return Ok(None);
        };
        if self.task.is_some() {
            return Ok(None);
        }
        // Issue #1152: the operator said, on this message, that it is not a
        // request for work. Nothing below gets a vote.
        //
        // **Above the `chatter` check on purpose.** When both are true they
        // agree, so the order changes no outcome — but it changes what the log
        // says happened, and the operator is the one who can be asked why. A
        // line crediting the model for a stand-down a person asked for sends the
        // next person debugging this to the escalation prompt instead of to the
        // composer.
        //
        // One guard here rather than one per caller: all three card-opening
        // paths funnel through this method, and #442's lesson is exactly that a
        // stand-down placed in one caller leaves the others opening cards.
        if ctx.not_work {
            tracing::debug!(
                company = %self.company,
                assignee = %assignee,
                "[delegation] not opening a card: the operator sent this message as chat, not work"
            );
            return Ok(None);
        }
        // Issue #984: the model already read this as conversation, so no card.
        //
        // Placed HERE, in the shared helper, rather than beside the `answering`
        // check in each caller: `answering` differs between the two paths (a
        // hand-off and a direct ask log different things about a question), but
        // "the model called this chatter" is one fact about the message and both
        // paths owe it the same answer. #442 put its stand-down in one caller
        // only and the other path kept opening cards; this is that lesson.
        //
        // Below the `self.task.is_some()` guard deliberately: a dispatched
        // card's turn has no operator message to have triaged, and `ctx`
        // defaults to all-false there anyway.
        if ctx.chatter {
            tracing::debug!(
                company = %self.company,
                assignee = %assignee,
                "[delegation] not opening a card: the model read this message as conversation"
            );
            return Ok(None);
        }
        // Issue #463: the REST chat handler read the operator's original words
        // and already opened a To-do card for them. One message must not become
        // two cards, whichever of the two card-opening paths below is running —
        // #442 guarded only the direct path, and a recognised imperative that
        // was handed off doubled through this one.
        if ctx.carded_by_handler {
            tracing::debug!(
                company = %self.company,
                assignee = %assignee,
                "[delegation] not opening a card: the chat handler already opened one for this \
                 message"
            );
            return Ok(None);
        }
        // What the operator actually asked for, without the open-work briefing
        // the cycle appends to a desk-addressed message. Everything below reads
        // this rather than `request`: the decision, the title, and the note —
        // a card whose title was half a listing of other cards is the same bug
        // wearing a different hat.
        let request = operator_words(request).trim();
        if !is_trackable_work(request) {
            tracing::debug!(
                company = %self.company,
                assignee = %assignee,
                "[delegation] not opening a card: nothing substantial was asked"
            );
            return Ok(None);
        }
        let card = TaskRecord {
            id: generate_id(),
            title: crate::ports::tasks::mint_task_title(request, None, self.titler).await,
            note: Some(append_note(None, "operator", request)),
            // The agent runs it in this turn, so the board shows it in progress
            // while that happens — the same window `hand_card_over` opens for a
            // dispatched card's delegate.
            column: lifecycle::COLUMN_IN_PROGRESS.to_string(),
            priority: "medium".to_string(),
            assignee: assignee.to_string(),
            updated_at_millis: now_millis(),
            // Issue #1890 B: the conversation this card was raised in — the
            // desk, and *which thread* inside it. The runner was bound to the
            // raising turn's root by `in_thread`, so this is the same
            // conversation the turn itself answers in, not a second reading of
            // it. `None` for the thread is the channel-level conversation,
            // which is what an unthreaded hand-off has always been.
            origin: TaskOrigin::new(chat_id.map(str::to_string), self.thread_root),
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            origin_message_seq: None,
            bounced: None,
        };
        tasks.upsert(self.company, &card).await?;
        tracing::debug!(
            company = %self.company,
            task_id = %card.id,
            assignee = %assignee,
            "[delegation] opened a card for work handed to an agent"
        );
        Ok(Some(card))
    }

    /// The card a **hand-off** opens (issue #442, path two), or `None` when
    /// this hand-off must not open one.
    ///
    /// # It stands down on a question, and only the card does (issue #267)
    ///
    /// `delegate_to_desk` is the one delegation that ANSWERS. It runs the
    /// desk's lead and hands their reply back for the orchestrator to relay, so
    /// it is how a question the orchestrator cannot answer alone — "what did the
    /// design desk ship this week?" — reaches somebody who can. Refusing the
    /// tool on a question turn therefore cost the operator the answer, not just
    /// a card.
    ///
    /// So the tool runs and this suppresses the card, which is the same shape
    /// [`open_direct_work_card`](Self::open_direct_work_card) uses one path
    /// over and for the same reason: nobody commissioned work, so nothing
    /// should be tracked — but somebody did ask a question, so somebody should
    /// answer it. Everything else about the hand-off is untouched: the delegate
    /// runs, their steps fold onto the operator timeline, and the CEO-relay
    /// hand-back surfaces their answer.
    ///
    /// With no card there is nothing to settle and nothing to report on
    /// `spawned_task`, which is the honest reading — the console's "Card
    /// opened" chip must not claim a card that does not exist.
    async fn open_hand_off_work_card(
        &self,
        member: &str,
        instruction: &str,
        chat_id: Option<&str>,
        ctx: MessageContext,
    ) -> Result<Option<TaskRecord>> {
        if ctx.answering {
            tracing::debug!(
                company = %self.company,
                delegate = %member,
                "[delegation] not opening a hand-off card: the operator asked a question, so the \
                 desk lead answers it without the board carrying work nobody commissioned"
            );
            return Ok(None);
        }
        self.open_work_card(member, instruction, chat_id, ctx).await
    }

    /// The card the REST chat handler opened for this message, when it opened
    /// one and it is still on the board (issue #463).
    ///
    /// Found by the **sequence position of the message itself**
    /// ([`TaskRecord::origin_message_seq`]), which the handler stamps as it
    /// writes. One message has one journal position and one card, so this is an
    /// identity lookup rather than a search.
    ///
    /// # It used to match on the title, and that is why boards read as chat logs
    ///
    /// This re-ran the same lexical detector over the same words and compared
    /// the two titles for byte equality, narrowed by the card's column, its
    /// assignee and its origin thread. It worked, and it made the headline an
    /// identity key: any better name for a card broke adoption, so a
    /// model-authored title was refused outright rather than risk it. That is
    /// the constraint that kept every card named after the message that opened
    /// it.
    ///
    /// The failure mode is also why the coupling had to go rather than be worked
    /// around. Nothing errors when adoption stops matching — `spawned_task`
    /// falls back, the "Card opened" chip silently disappears from every carded
    /// chat message, and `settle_authored_workflow_card` stops running so a
    /// workflow the turn authored strands its card in To-do. Issues #982 and
    /// #576 are both that same silence, found twice, after the handler changed
    /// an assignee and then a column out from under clauses that were reading
    /// them.
    ///
    /// A sequence position has none of that surface: it is stamped by the write
    /// this looks for, it does not move when the handler changes what column or
    /// assignee it opens a card under, and it cannot be re-derived wrongly
    /// because it is not derived at all. It also settles a case title equality
    /// got wrong on its own terms — two alike-reading messages in one thread are
    /// two cards, and a newest-first scan adopted whichever came back first.
    ///
    /// `None` when no store is wired, when the turn is not answering a journaled
    /// message, or when nothing matches — the honest answer for a handler write
    /// that failed (it is best-effort there), for a card written before this
    /// field existed, and for every non-REST caller of this seam, none of which
    /// have a chat handler in front of them. `None` therefore reads as "no card
    /// to adopt", which — now that the handler cards on the operator's explicit
    /// workflow request alone — is also how `carded_by_handler` is decided,
    /// together with that request itself (a copilot thread suppresses the card
    /// but not the signal).
    async fn chat_handler_card(&self) -> Result<Option<String>> {
        let Some(tasks) = self.tasks else {
            return Ok(None);
        };
        let Some(seq) = self.message_seq else {
            return Ok(None);
        };
        Ok(tasks
            .list(self.company)
            .await?
            .into_iter()
            .find(|card| card.origin_message_seq == Some(seq))
            .map(|card| card.id))
    }

    /// Settles a card [`open_work_card`](Self::open_work_card) opened, once the
    /// turn it was tracking has ended.
    ///
    /// The landing column comes from [`lifecycle::settled_landing_column`] like
    /// every other settle on this seam, so a card opened by construction is
    /// finished by the same rule as one that came off the board: a produced
    /// answer stops in In Review for a person, a cancelled run goes back to
    /// To-do, and a run that stopped at an unauthorised call parks.
    ///
    /// `parked_approvals` is how many approvals **the turn this card is
    /// recording** left outstanding, differenced across that turn by the caller.
    /// Issue #465: this used to be [`lifecycle::landing_column`] with a
    /// hardcoded [`TaskRunEnd::Completed`], so a desk whose first tool call
    /// parked settled as a plain success — the card announced a result to review
    /// while the work had not started. The ending alone cannot see that; only
    /// the count can.
    async fn settle_work_card(
        &self,
        card: &mut TaskRecord,
        responder: &str,
        end: TaskRunEnd,
        parked_approvals: usize,
        body: &str,
    ) -> Result<()> {
        card.note = Some(append_note(
            card.note.as_deref(),
            &lifecycle::note_attribution(end, responder),
            body,
        ));
        card.column = lifecycle::settled_landing_column(end, parked_approvals).to_string();
        // Set bounced for failed/cancelled runs landing on todo (issue #1865).
        let settled_status = lifecycle::settled_run_status(end, parked_approvals);
        card.bounced = crate::runtime::advance::bounced_reason(&card.column, settled_status, body);
        card.updated_at_millis = now_millis();
        if let Some(tasks) = self.tasks {
            tasks.upsert(self.company, card).await?;
        }
        tracing::debug!(
            company = %self.company,
            task_id = %card.id,
            column = %card.column,
            "[delegation] settled the card opened for this turn"
        );
        Ok(())
    }

    /// Reassigns a dispatched card to the delegate taking it over and persists
    /// it, so the board shows them working it *while* they work rather than
    /// only once they are done (issue #204).
    ///
    /// The write goes through the [`TaskStore`] port, **not**
    /// `CompanyRuntime::upsert_task`, so it cannot re-fire the
    /// `column → in_progress` dispatch edge — the card is already in
    /// `in_progress` and this only re-states it. No task store wired is a silent
    /// no-op, matching every other task path on this seam.
    async fn hand_card_over(
        &self,
        card: &mut TaskRecord,
        delegator: &str,
        member: &str,
        instruction: &str,
    ) -> Result<()> {
        // **An owner is an agent. A desk is a channel, not an owner.**
        //
        // `member` is the agent that will run the card, and that is exactly who
        // now owns it — for a teammate hand-off the teammate, for a desk
        // hand-off that desk's lead.
        //
        // This briefly wrote the hand-off target reduced by
        // `AssigneeResolution::canonical` instead, on the reading that a desk
        // hand-off should leave the DESK on the card. That put a channel id in
        // an ownership field: `assignee` is also what the thread's overseer is
        // read from, so a card handed to a desk named nobody who could answer
        // for it. `canonical` maps a desk to its own id because it is the
        // stored-key helper for whatever a card happens to say — not a claim
        // that a desk is a thing which owns work.
        card.assignee = member.to_string();
        card.note = Some(append_note(
            card.note.as_deref(),
            delegator,
            &match instruction.trim() {
                "" => format!("delegated to {member}"),
                instruction => format!("delegated to {member}: {instruction}"),
            },
        ));
        card.column = lifecycle::landing_column(TaskRunEnd::Delegated).to_string();
        card.updated_at_millis = now_millis();
        tracing::debug!(
            task_id = %card.id,
            delegate = %member,
            column = %card.column,
            "[task] card handed over to the delegate"
        );
        if let Some(tasks) = self.tasks {
            tasks.upsert(self.company, card).await?;
        }
        Ok(())
    }

    /// Executes one drained delegation.
    ///
    /// `spawn_task` opens a To-do card through the
    /// [`TaskStore::upsert`](crate::ports::TaskStore) path the console uses and
    /// **reports the card's id** so the caller can say one was opened (issue
    /// #246) — it surfaces no bubble of its own, which is a different thing
    /// from the nothing it used to surface. A missing task store is a silent
    /// no-op.
    /// `delegate_to_desk` runs a single turn on the desk's lead member and
    /// **returns its reply for the orchestrator to relay** (a [`DeskReply`]). An
    /// unknown desk (no roster-backed lead) or a cancelled run yields nothing to
    /// relay.
    ///
    /// Since issue #176 a desk member the manifest opted in with `delegates_to`
    /// carries the hand-off tools itself, so its turn may queue too. That queue
    /// is drained **here**, recursively, inside this hand-off's scope — and the
    /// deeper answers are folded into this member's reply rather than relayed
    /// separately. Depth is bounded at the tool boundary by the scope chain, not
    /// by this function.
    ///
    /// `ctx` carries what
    /// [`handle_operator_message`](Self::handle_operator_message) already
    /// decided about the operator message this drain belongs to — whether the
    /// REST chat handler carded it (issue #463) and whether it triaged as a
    /// question (issue #267). Both are threaded in rather than recomputed
    /// because the only text in scope here is the instruction the *model*
    /// wrote, which is a different sentence from the one those decisions were
    /// made about. A dispatched card's drain has no operator message and passes
    /// [`MessageContext::default`].
    pub(crate) async fn run_delegation(
        &self,
        delegation: Delegation,
        chat_id: Option<&str>,
        ctx: MessageContext,
    ) -> Result<DelegationOutcome> {
        match delegation {
            Delegation::SpawnTask {
                title,
                note,
                assignee,
            } => {
                let Some(tasks) = self.tasks else {
                    return Ok(DelegationOutcome::default());
                };
                // Grounded against the roster on the same terms `AssignTask`
                // grounds its own: a name that resolves to nobody opens the card
                // unowned rather than stamping a phantom owner the board would
                // then render and no dispatch could ever reach.
                let owner = assignee
                    .as_deref()
                    .map(|name| assignee::resolve(self.record, name))
                    .and_then(|resolved| resolved.canonical().map(str::to_string))
                    .unwrap_or_default();
                let card = TaskRecord {
                    id: generate_id(),
                    title: crate::ports::tasks::TaskTitle::system(&title),
                    note,
                    origin_message_seq: None,
                    column: COLUMN_TODO.to_string(),
                    priority: "medium".to_string(),
                    assignee: owner,
                    updated_at_millis: now_millis(),
                    // Issue #151 §3.2: remember which conversation asked for this,
                    // so the completion can answer there instead of only landing in
                    // the note.
                    // Issue #661 (M5): `None` on the workflow path, and that is
                    // the lineage-root decision rather than a gap. A run has no
                    // conversation behind it, so there is nowhere for a
                    // completion to post back to — and stamping the chat that
                    // *scheduled* the workflow hours earlier would make the card
                    // answer into a conversation the operator has left. The run
                    // reference below is the provenance instead.
                    // Issue #1890 B: which thread inside it, too — the root the
                    // runner was bound to by `in_thread`, so a card a threaded
                    // turn spawns settles back into that thread rather than flat
                    // in the channel. On the workflow path the whole origin is
                    // `None` for the reason above: no conversation is behind a
                    // run, so there is no thread inside one either.
                    origin: TaskOrigin::new(chat_id.map(str::to_string), self.thread_root),
                    // Lineage (#185): the dispatched card whose turn queued this
                    // one, when the drain is running inside a task
                    // (`for_task`) — since #204 a dispatched turn drains the
                    // queue too, so a task IS in scope here and this is the site
                    // that stamps it. An orchestrator *chat* turn has no task in
                    // scope and still writes `None`; lineage for those is written
                    // through the task API's `parentTaskId` instead.
                    parent_task_id: self.task.clone(),
                    // Nothing has run yet, so there is no deliverable to point
                    // at (issue #339). The first successful settle stamps it.
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    // Issue #661 (M5): machine provenance for a card a workflow
                    // node opened — a reference to the run, never a parent. Both
                    // ids or neither; `None` on every chat and task path, which is
                    // every caller that did not go through `for_workflow_run`.
                    //
                    // A `sub_workflow` child's node stamps the PARENT run's ids:
                    // the resolver runs the child inside the engine under the
                    // parent's bundle, so there is exactly one run id in
                    // existence and it is the only one a console can navigate to.
                    origin_run_id: self.workflow_run.as_ref().map(|run| run.run_id.clone()),
                    origin_workflow_id: self
                        .workflow_run
                        .as_ref()
                        .map(|run| run.workflow_id.clone()),
                    // Issue #1865: a card just being minted has never bounced.
                    bounced: None,
                };
                tasks.upsert(self.company, &card).await?;
                // Issue #246: report the card so the caller can surface it. The
                // id is reported only after the write succeeded, so a bubble can
                // never claim a card that is not on the board.
                Ok(DelegationOutcome {
                    spawned_task: Some(card.id),
                    ..DelegationOutcome::default()
                })
            }
            Delegation::DelegateToDesk { desk, instruction } => {
                let Some(member) = desk_lead(self.record, &desk) else {
                    // Since #272 the harness tool refuses an ungrounded target
                    // before it is ever queued, so reaching here means the desk
                    // lost its lead between the tool call and this drain (or the
                    // delegation came from a path with no tool boundary). Either
                    // way it is a hand-off that will not happen: say so in the
                    // log, and — on the task path — on the card itself.
                    tracing::warn!(
                        company = %self.company,
                        desk = %desk,
                        "[delegation] hand-off could not be delivered: no desk with that id has a \
                         lead on the roster"
                    );
                    return Ok(DelegationOutcome::default());
                };
                let scope_key = self
                    .record
                    .resolve_desk_id(&desk)
                    .unwrap_or_else(|| desk.clone());
                // `Box::pin` on the same reasoning as the nested drain inside:
                // `run_delegation` → `run_hand_off` → `drain_and_execute` →
                // `run_delegation` is an async cycle, and the hand-off body is
                // by far the largest state in it. The cycle is already broken by
                // the box on the nested drain, so this one is not what makes it
                // compile — it is what keeps a recursive hand-off's frame off
                // the stack, which this repo has been bitten by before.
                Box::pin(self.run_hand_off(
                    HandOff {
                        member,
                        instruction,
                        label: desk,
                        scope_key,
                    },
                    chat_id,
                    ctx,
                ))
                .await
            }
            // Issue #884, D1: the same hand-off, resolved straight to a named
            // teammate instead of through a desk to whoever leads it. Everything
            // downstream — the card, the steer guard, the depth chain, the
            // `DeskReply` the relay folds in — is the desk path's, verbatim.
            Delegation::DelegateToTeammate {
                teammate,
                instruction,
            } => {
                let Some(member) = self.record.resolve_teammate_key(&teammate).agent() else {
                    // The mirror of the desk arm's warning, and reachable for the
                    // same narrow reason: the tool grounds the target before
                    // queuing, so this is a teammate removed from the roster
                    // between the call and the drain — or one named on the
                    // fail-open path, which queues the key ungrounded. Resolved
                    // with the same id-then-name resolve the tool boundary used
                    // (#1162), so a display name cannot be accepted there and
                    // silently dropped here.
                    tracing::warn!(
                        company = %self.company,
                        teammate = %teammate,
                        "[delegation] hand-off could not be delivered: no teammate with that id is \
                         on the roster"
                    );
                    return Ok(DelegationOutcome::default());
                };
                let scope_key = delegation_tools::teammate_scope_key(&member);
                Box::pin(self.run_hand_off(
                    HandOff {
                        label: member.clone(),
                        member,
                        instruction,
                        scope_key,
                    },
                    chat_id,
                    ctx,
                ))
                .await
            }
            Delegation::ConversationDispatch {
                source,
                target,
                message,
                chat_id,
                trigger_sequence,
                child_hop,
            } => {
                let prompt = format!("@{source} sent you this direct message:\n\n{message}");
                let outcome = with_turn_message_hop(
                    child_hop,
                    self.run_turn.run(
                        self.company,
                        &target,
                        &prompt,
                        ChatTarget::channel(Some(&chat_id))
                            .answering(Some(EventSeq::new(trigger_sequence))),
                    ),
                )
                .await?;
                let bubbles = vec![OutboundMessage {
                    message_id: None,
                    task_id: None,
                    outputs: Vec::new(),
                    channel: chat_id,
                    agent: Some(target),
                    text: outcome.reply,
                    steps: outcome.steps,
                    reply_to: None,
                    mentions: Vec::new(),
                }];
                tracing::debug!(
                    company = %self.company,
                    hop = child_hop,
                    "[tinyhivemind] completed a bounded agent-to-agent DM turn"
                );
                Ok(DelegationOutcome {
                    bubbles,
                    ..DelegationOutcome::default()
                })
            }
            // ── Issue #186 part b: orchestrator lifecycle authority ─────────
            //
            // Both write through the same `TaskStore` path the console uses, so
            // an orchestrator-driven change is persisted identically to an
            // operator-driven one. Neither yields anything for the cycle to
            // surface — no bubble and nothing to relay: the orchestrator is
            // mid-turn and will describe what it did in its own reply, and a
            // second voice would be it talking to itself.
            //
            // A card that has since vanished is a silent no-op, matching every
            // other task path on this seam.
            Delegation::AssignTask {
                task_id,
                assignee,
                note,
            } => {
                let Some((tasks, mut card)) = self.load_card(&task_id).await? else {
                    if self.tasks.is_none() {
                        tracing::warn!(
                            company = %self.company,
                            task_id = %task_id,
                            "[delegation] assign_task could not run: no task store is wired"
                        );
                        return Ok(DelegationOutcome::default());
                    }
                    tracing::warn!(
                        company = %self.company,
                        task_id = %task_id,
                        "[delegation] assign_task named a card that is not on the board; \
                         nothing was assigned, and the failure is carried out with the drain \
                         rather than aborting it"
                    );
                    return Ok(DelegationOutcome {
                        refused_card: Some(RefusedCardWrite {
                            tool: "assign_task",
                            task_id,
                            reason: "no such card is on the board".to_string(),
                        }),
                        ..DelegationOutcome::default()
                    });
                };
                let observed = card.clone();
                // Issue #205: the orchestrator writes this `assignee` out of an
                // LLM tool call, so it is exactly as capable of naming somebody
                // who does not exist as the operator's free-text field is. Held
                // to the same contract: an unresolvable name is not written to
                // the card at all — leaving the previous owner in place — and
                // the refusal is recorded in the orchestrator's own voice, so
                // the board neither shows a phantom owner nor loses the fact
                // that an assignment was attempted.
                let resolved = assignee::resolve(self.record, &assignee);
                // Issue #661 (M5): whether an owner was actually written, which
                // is the one thing the three `Ok` paths out of this arm disagree
                // about — see `DelegationOutcome::assigned`.
                let mut assigned = false;
                let entry = match resolved.canonical() {
                    // A blank or whitespace-only `assignee` resolves to
                    // `Unassigned`, whose canonical form is `""`. Clearing the
                    // owner is the right write — unassigning a card is a real
                    // thing to ask for — but there is no name to put in the
                    // note, and `assigned to {assignee}` would record a
                    // sentence that trails off with nothing after it. Name the
                    // effect instead, so the timeline says what happened.
                    Some("") => {
                        card.assignee = String::new();
                        // Clearing an owner IS an ownership write, so it counts as
                        // `assigned` for the run's board row — the row records
                        // that the run set who owns the card, and "nobody" is an
                        // answer to that.
                        assigned = true;
                        match note {
                            Some(note) => format!("cleared the assignee — {note}"),
                            None => "cleared the assignee".to_string(),
                        }
                    }
                    Some(canonical) => {
                        card.assignee = canonical.to_string();
                        assigned = true;
                        match note {
                            Some(note) => format!("assigned to {assignee} — {note}"),
                            None => format!("assigned to {assignee}"),
                        }
                    }
                    None => format!(
                        "could not assign to {assignee}: {}",
                        resolved
                            .rejection()
                            .unwrap_or_else(|| "not on the roster".to_string())
                    ),
                };
                card.note = Some(append_note(
                    card.note.as_deref(),
                    // Issue #661 (M5): `workflow:<id>` when a run drove this,
                    // the orchestrator otherwise. See `note_author`.
                    &self.note_author(),
                    &entry,
                ));
                // The column is untouched on purpose: dispatch fires from
                // `CompanyRuntime::upsert_task`, which this port cannot reach.
                // Assignment records ownership; the board's
                // `column → in_progress` PATCH still starts the work.
                //
                // Issue #661 (M5) inherits that invariant rather than restating
                // it: a workflow run's board drain executes THIS arm, so a run
                // cannot move a card between columns even though it may set the
                // card's owner. That is what bounds run → card → dispatch → run
                // cycles — every dispatch still needs an operator drag. The bound
                // holds one level deeper too: the write goes through the
                // `TaskStore` port, which cannot trigger dispatch at all.
                card.updated_at_millis = now_millis();
                if !tasks
                    .update_if_column(self.company, &card, &observed, &observed.column)
                    .await?
                {
                    return Ok(DelegationOutcome {
                        refused_card: Some(RefusedCardWrite {
                            tool: "assign_task",
                            task_id,
                            reason: "the card changed before the assignment could be recorded"
                                .to_string(),
                        }),
                        ..DelegationOutcome::default()
                    });
                }
                Ok(DelegationOutcome {
                    assigned,
                    ..DelegationOutcome::default()
                })
            }
            Delegation::ReviewTask {
                task_id,
                decision,
                note,
            } => {
                let Some((tasks, mut card)) = self.load_card(&task_id).await? else {
                    if self.tasks.is_none() {
                        tracing::warn!(
                            company = %self.company,
                            task_id = %task_id,
                            ?decision,
                            "[delegation] review_task could not run: no task store is wired"
                        );
                        return Ok(DelegationOutcome::default());
                    }
                    tracing::warn!(
                        company = %self.company,
                        task_id = %task_id,
                        ?decision,
                        "[delegation] review_task named a card that is not on the board; the \
                         verdict was recorded nowhere, and the failure is carried out with the \
                         drain rather than aborting it"
                    );
                    return Ok(DelegationOutcome {
                        refused_card: Some(RefusedCardWrite {
                            tool: "review_task",
                            task_id,
                            reason: "no such card is on the board".to_string(),
                        }),
                        ..DelegationOutcome::default()
                    });
                };
                let observed = card.clone();
                if card.column != lifecycle::COLUMN_IN_REVIEW {
                    return Ok(DelegationOutcome {
                        refused_card: Some(RefusedCardWrite {
                            tool: "review_task",
                            task_id,
                            reason: format!("the card is {:?}, not in_review", card.column),
                        }),
                        ..DelegationOutcome::default()
                    });
                }
                card.note = Some(append_note(
                    card.note.as_deref(),
                    &self.orchestrator_id(),
                    &lifecycle::review_note(decision, note.as_deref()),
                ));
                card.column = lifecycle::review_landing_column(decision).to_string();
                card.updated_at_millis = now_millis();
                if !tasks
                    .update_if_column(self.company, &card, &observed, lifecycle::COLUMN_IN_REVIEW)
                    .await?
                {
                    return Ok(DelegationOutcome {
                        refused_card: Some(RefusedCardWrite {
                            tool: "review_task",
                            task_id,
                            reason: "the card changed before the review could be recorded"
                                .to_string(),
                        }),
                        ..DelegationOutcome::default()
                    });
                }
                Ok(DelegationOutcome::default())
            }
        }
    }

    /// Loads a board card and its store handle, if both exist.
    async fn load_card(
        &self,
        task_id: &str,
    ) -> Result<Option<(&'a Arc<dyn TaskStore>, TaskRecord)>> {
        let Some(tasks) = self.tasks else {
            return Ok(None);
        };
        let card = tasks
            .list(self.company)
            .await?
            .into_iter()
            .find(|t| t.id == task_id);
        Ok(card.map(|card| (tasks, card)))
    }

    /// The company orchestrator's agent id — the single voice a lifecycle
    /// delegation's note is recorded under (issue #186). Mirrors
    /// `HarnessBrain::orchestrator`; on an empty roster it is the empty string,
    /// which `orchestrator_id` already tolerates.
    fn orchestrator_id(&self) -> String {
        orchestrator::orchestrator_id(&self.record.effective_agents()).unwrap_or_default()
    }

    /// Which of the specifically mentioned teammates `responder` can actually
    /// reach. The orchestrator can reach every roster teammate; ordinary
    /// responders are constrained by desk peers and their `delegates_to` list.
    ///
    /// The partition matters, not just whether it is empty: a responder that
    /// can reach one named teammate but not another must not be told to "hand
    /// work to them" as though everyone named were in play, nor told it has
    /// "no way to hand off" when it can reach some — the wording names who is
    /// out of reach.
    fn reachable_mentioned(&self, responder: &str) -> Vec<String> {
        if responder == self.orchestrator_id() {
            return self.also_mentioned.clone();
        }
        let Some(agent) = self.record.effective_agent(responder) else {
            return Vec::new();
        };
        let reachable =
            delegation_tools::teammate_targets(self.record, responder, &agent.delegates_to);
        self.also_mentioned
            .iter()
            .filter(|target| reachable.contains(target))
            .cloned()
            .collect()
    }

    /// The voice a note this drain appends is recorded under.
    ///
    /// The orchestrator on every chat and task path, unchanged. On a **workflow
    /// run** it is `workflow:<workflow_id>` instead (issue #661 / M5), because
    /// attributing the note to the CEO would say a person's agent decided
    /// something an authored graph did — and an operator reading the card's
    /// timeline has no other way to tell the two apart. The `workflow:` prefix is
    /// the same label `SearchMetering` already attributes a run's search spend
    /// under, so one convention names a run across the surfaces.
    fn note_author(&self) -> String {
        match &self.workflow_run {
            Some(run) => format!("workflow:{}", run.workflow_id),
            None => self.orchestrator_id(),
        }
    }

    /// This drain's run id for a log field, or `""` off the workflow path.
    fn run_id_label(&self) -> &str {
        self.workflow_run
            .as_ref()
            .map_or("", |run| run.run_id.as_str())
    }
}

/// A [`Delegation`]'s kind as a fixed label, safe to log.
///
/// Every arm is a literal and the type carries no `String` payload out through
/// here, so — unlike `{delegation:?}` — nothing a model wrote can ride this into
/// a host log. The [`DeliveryReason`](crate::ports::DeliveryReason) split, one
/// seam over.
fn kind_label(delegation: &Delegation) -> &'static str {
    match delegation {
        Delegation::SpawnTask { .. } => "spawn_task",
        Delegation::DelegateToDesk { .. } => "delegate_to_desk",
        Delegation::DelegateToTeammate { .. } => "delegate_to_teammate",
        Delegation::ConversationDispatch { .. } => "conversation_dispatch",
        Delegation::AssignTask { .. } => "assign_task",
        Delegation::ReviewTask { .. } => "review_task",
    }
}

/// What a hand-off was aimed at — a desk key or a teammate id — or `None` for
/// every delegation that is not a hand-off, which is what distinguishes "this
/// delegation had a target that did not resolve" from "this delegation never had
/// a target" (issues #272, #884).
///
/// Replaces the desk-only `desk_of` this seam used before #884. Its callers all
/// wanted "the thing this was handed to"; that they could only ever be handed a
/// desk was an accident of there being one hand-off kind. The one place the
/// distinction still matters — the card note that says *why* delivery failed —
/// picks its wording from the delegation's own variant at the call site rather
/// than from a second accessor.
fn hand_off_target_of(delegation: &Delegation) -> Option<&str> {
    match delegation {
        Delegation::DelegateToDesk { desk, .. } => Some(desk),
        Delegation::DelegateToTeammate { teammate, .. } => Some(teammate),
        Delegation::ConversationDispatch { target, .. } => Some(target),
        _ => None,
    }
}

/// The instruction a hand-off carries, for the note that records it (issue
/// #204). Empty for every other delegation kind — callers only ask this of a
/// [`Delegation::DelegateToDesk`].
fn instruction_of(delegation: &Delegation) -> &str {
    match delegation {
        Delegation::DelegateToDesk { instruction, .. }
        | Delegation::DelegateToTeammate { instruction, .. } => instruction,
        _ => "",
    }
}

/// The note recorded on a card when a hand-off could not be delivered (issue
/// #272).
///
/// Written in the delegator's voice, like every other note this seam appends,
/// and deliberately explicit about the two facts an operator otherwise has to
/// infer: nothing was handed off, and the card is still theirs. Names only the
/// target key, the cause, and the delegator — no instruction text, no delegate
/// output.
///
/// `target` names a desk **or** a teammate since #884, so the sentence no longer
/// calls it a desk; the `cause` its callers pass is what says which it was.
fn undeliverable_handoff(target: &str, delegator: &str, cause: &str) -> String {
    format!(
        "hand-off to \"{target}\" was not delivered — {cause}. Nothing was delegated; this \
card is still with {delegator}."
    )
}

/// Appends a responder-attributed result block to a card's note, preserving any
/// prior note above it (issue #186). Mirrors `harness::brain::append_result`,
/// kept local to the seam so the lifecycle arms never reach back into the brain.
pub(crate) fn append_note(prev: Option<&str>, responder: &str, body: &str) -> String {
    let block = format!("[{responder}] {body}");
    match prev.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n\n{block}"),
        None => block,
    }
}

// ---------------------------------------------------------------------------
// Is this substantial enough to be tracked? (issue #442)
// ---------------------------------------------------------------------------
//
// A card is the DEFAULT for anything substantial. That is the whole product
// promise — ask for something, watch it become work with an output you can open
// — and #442 is what happens when it is not: an agent reads a repository,
// writes the file you asked for, and the board stays empty because the work only
// ever existed as a conversation.
//
// So this is not a "should I open a card?" judgement handed to the model. It is
// a **carve-out**: everything is tracked unless there is positive evidence that
// nothing was asked for. The bias is deliberate and one-directional — a
// spurious card is visible on the board and can be dismissed in one click; a
// missing card is invisible, and every downstream station (planning, the
// prerequisite check, the gate, the settled-run mover, the deliverable link)
// hangs off it.

/// Past this many words, a request is substantial no matter what it says. A
/// genuinely trivial question is short; nothing this long is "just asking".
const TRACK_ALWAYS_WORDS: usize = 25;

/// The longest an utterance opening with small talk may run before it stops
/// being small talk. "thanks!" is chatter; "thanks — now pull together the Q3
/// numbers, the deck and the board memo" is not.
///
/// Raised from 6 to 8 by issue #984, which is a real trade and not a free one:
/// every word added here is a short instruction that opens with an
/// acknowledgement and now goes untracked. 8 is chosen to cover the common
/// two-clause ack ("noted, thanks — will pick that up tomorrow") without
/// reaching the length at which a sentence is usually carrying an instruction.
/// The model layer, not this number, is what handles the long conversational
/// message; pushing this much higher would buy those at the cost of real work.
const SMALLTALK_MAX_WORDS: usize = 8;

/// Verbs that name something being **produced or changed**. Their presence is
/// decisive: whatever else the sentence is doing, it is asking for work.
///
/// Deliberately excludes words that are far more often nouns in this domain —
/// `build`, `report`, `plan`, `review`, `design`, `check`, `update`, `test` —
/// because "what's the status of the build?" is a question, not a request, and
/// a classifier that mints a card for it is the fix becoming its own bug.
const WORK_VERBS: &[&str] = &[
    "analyse",
    "analyze",
    "assemble",
    "audit",
    "author",
    "collate",
    "compile",
    "compose",
    "draft",
    "implement",
    "investigate",
    "migrate",
    "prepare",
    "produce",
    "refactor",
    "rewrite",
    "summarise",
    "summarize",
    "write",
];

/// Wh-words that open a request **to know** rather than a request to do. Only
/// the unambiguous ones: `do` / `can` / `is` open questions *and* imperatives
/// ("do the quarterly close"), so they are read as interrogative only when the
/// text actually ends in a question mark.
const INTERROGATIVE_OPENERS: &[&str] = &[
    "what", "who", "whom", "whose", "when", "where", "which", "why", "how",
];

/// Openers that mark an utterance as conversation rather than a request.
const SMALLTALK_OPENERS: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yo",
    "morning",
    "afternoon",
    "evening",
    "gm",
    "thanks",
    "thank",
    "thx",
    "ty",
    "cheers",
    "ok",
    "okay",
    "k",
    "cool",
    "great",
    "nice",
    "perfect",
    "awesome",
    "lovely",
    "yes",
    "yeah",
    "yep",
    "yup",
    "no",
    "nope",
    "sure",
    "noted",
    "understood",
    "bye",
    "sounds",
    "got",
    "haha",
    "lol",
    // Issue #984: acknowledgement and meta vocabulary. A message opening with
    // one of these, and staying short, is somebody closing a loop rather than
    // opening one.
    //
    // This list is deliberately NARROWER than the issue proposed. `qa`, `test`
    // and `ignore` were suggested and are left out on purpose: each of them
    // opens a legitimate short instruction to a desk — "test the checkout flow
    // on staging", "ignore the stale rows and rebuild the index" — and this
    // rung has no way to tell those from chatter. Words that essentially never
    // open an instruction are safe here; words that often do are exactly the
    // ones the model layer above exists to judge.
    "ack",
    "acked",
    "fyi",
    "nvm",
    "nevermind",
    "disregard",
    "oops",
    "np",
    "agreed",
    "indeed",
    "ditto",
];

/// The lowercase alphanumeric word tokens of `text`.
///
/// Splitting on every non-alphanumeric character is what makes `what's` open
/// with `what` and `modules.md` two tokens — the classifier only ever asks
/// *which words are present*, so over-splitting costs nothing and under-
/// splitting would hide an opener behind an apostrophe.
fn work_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The operator's own words, with anything the cycle appended stripped off.
///
/// A desk-addressed operator message does not reach the brain as typed: the
/// cycle folds a briefing of the target's open cards onto the end of it
/// ([`OPEN_WORK_ANNOTATION`]) so a direct "what are you working on?" is answered
/// truthfully. Everything downstream that reasons about *what the operator
/// asked for* has to cut that off first.
///
/// Found live, not by a unit test: without this, "thanks!" in a desk thread
/// scored as a substantial request — the appended card list is long, and length
/// is evidence of substance — and opened a card. Which then lengthened the
/// briefing on the next message. Each card made the next one likelier.
///
/// Splits on the shared constants rather than transcribed copies of them, so the
/// two sides cannot drift.
///
/// Issue #845 added a second appended block, [`BUILDER_ANNOTATION`], on exactly
/// the same terms — so this cuts at whichever marker comes first. Missing it
/// would be the "thanks!" bug again in a new costume: the builder briefing is
/// several lines of imperative prose, and `looks_like_work` scores length and
/// work verbs, so every `workflow` message would read as substantial no matter
/// what the operator actually typed.
///
/// Issue #1890 C added a fourth: [`SETTLED_WORK_ANNOTATION`], briefing the turn
/// on work raised in this conversation that has since finished. Missing it would
/// be the "thanks!" bug a third time, and with a nastier loop than #176's: the
/// settled briefing grows as cards *finish*, so every completed card would make
/// the next message likelier to open one, which would in time finish and
/// lengthen the briefing again.
///
/// Issue #1682 added a third: the attachment markers
/// [`with_attachment_refs`](crate::brain::medulla::effects::with_attachment_refs)
/// appends when a message carries files. The harness brain feeds the agent
/// that composed text, and this triage must see only what the operator typed —
/// an attachment's extracted text is a large block of model-directed prose, and
/// scoring it would open a card on every "what does this say?" beside a file.
pub(crate) fn operator_words(message: &str) -> &str {
    let cut = [
        message.find(OPEN_WORK_ANNOTATION),
        message.find(BUILDER_ANNOTATION),
        message.find(SETTLED_WORK_ANNOTATION),
        message.find(THREAD_INDEX_ANNOTATION),
        message.find(crate::brain::medulla::effects::ATTACHMENT_MARKER_PREFIX),
    ]
    .into_iter()
    .flatten()
    .min();
    match cut {
        Some(at) => &message[..at],
        None => message,
    }
}

/// Whether `text` asks for something substantial enough that the board should
/// carry it — the single decision behind every card this seam opens by
/// construction (issue #442).
///
/// Reads as a ladder of carve-outs over a `true` default:
///
/// 1. **Nothing was said** — empty, or punctuation/emoji only. No work.
/// 2. **Long** — past [`TRACK_ALWAYS_WORDS`]. Work.
/// 3. **Names a deliverable** — any [`WORK_VERBS`] entry appears. Work.
/// 4. **A plain question** — ends in `?`, or opens with a wh-word. No work.
/// 5. **Small talk** — opens with a greeting/acknowledgement and stays short.
///    No work.
/// 6. **Anything else** — work.
///
/// Rung 3 runs before rung 4 on purpose: "can you write up the Q3 numbers?" is
/// a question in shape and a request for work in substance, and the substance
/// wins. The known cost is that a genuine question *about* a deliverable
/// ("what should I write here?") is tracked. That is the bias pointing the way
/// it was chosen to point.
pub(crate) fn is_trackable_work(text: &str) -> bool {
    let trimmed = text.trim();
    let words = work_words(trimmed);
    if words.is_empty() {
        return false;
    }
    if words.len() > TRACK_ALWAYS_WORDS {
        return true;
    }
    if words.iter().any(|w| WORK_VERBS.contains(&w.as_str())) {
        return true;
    }
    if trimmed.ends_with('?') || INTERROGATIVE_OPENERS.contains(&words[0].as_str()) {
        return false;
    }
    if words.len() <= SMALLTALK_MAX_WORDS && SMALLTALK_OPENERS.contains(&words[0].as_str()) {
        return false;
    }
    true
}

/// Whether `text` is HIGH-CONFIDENCE small talk — a greeting or acknowledgement
/// and nothing more — that the harness may answer with a cheap tool-less,
/// memory-less, goal-less turn instead of running the full agentic task loop
/// (issue #1725).
///
/// Deliberately far stricter than the [`is_trackable_work`] small-talk rung:
/// that one returns `false` (not-work) for plain *questions* too, but a question
/// deserves a real answer and possibly tools, so it must NOT take the fast path.
/// This predicate abstains (returns `false`) on anything but a short
/// greeting/acknowledgement:
///
/// 1. empty / punctuation-only → abstain (nothing to answer);
/// 2. longer than [`SMALLTALK_MAX_WORDS`] → abstain;
/// 3. contains any [`WORK_VERBS`] entry → abstain (a greeting in front of a
///    request is a request — keep the regression at
///    `a_greeting_in_front_of_a_request_does_not_hide_it` green);
/// 4. ends in `?` or opens with an interrogative → abstain (a question);
/// 5. opens with a [`SMALLTALK_OPENERS`] greeting/ack → **fast path**.
///
/// Abstention always falls through to the normal turn, never to a silent
/// non-answer.
pub(crate) fn is_pure_small_talk(text: &str) -> bool {
    let trimmed = text.trim();
    let words = work_words(trimmed);
    if words.is_empty() {
        return false;
    }
    if words.len() > SMALLTALK_MAX_WORDS {
        return false;
    }
    if words.iter().any(|w| WORK_VERBS.contains(&w.as_str())) {
        return false;
    }
    if trimmed.ends_with('?') || INTERROGATIVE_OPENERS.contains(&words[0].as_str()) {
        return false;
    }
    SMALLTALK_OPENERS.contains(&words[0].as_str())
}

tokio::task_local! {
    /// Set by the delegation runner around an operator turn it has classified as
    /// conversation rather than work — either the operator's explicit
    /// "Just chatting" (`deliverable: "chat"` → `not_work`) or a high-confidence
    /// [`is_pure_small_talk`] greeting. The harness pool reads it (same task, so
    /// it propagates through the `RunTurn` seam) and runs that turn with reduced
    /// scope: no tools to loop on, no pre-turn memory retrieval, and no prior
    /// task's thread goal re-injected (issue #1725). Absent = a normal turn.
    pub(crate) static CHAT_ONLY_TURN: bool;
}

tokio::task_local! {
    /// What the current turn is trying to do, in the requester's own words
    /// (issue #6014).
    ///
    /// Read by [`PayloadExtractor`](crate::harness::payload_extract) when a tool
    /// returns more than the per-result budget: knowing the task is what lets it
    /// keep the records that answer the question and shorten the ones that do
    /// not. Without it the extractor declines outright rather than guessing,
    /// because a task-blind extraction is a byte cut with a model call attached
    /// — it would drop the one issue that mattered exactly as readily as the
    /// twenty-nine that did not.
    ///
    /// Set to [`operator_words`], not the composed turn text: by the time a turn
    /// runs, `message` carries the cycle's machine briefings (open work, the
    /// settled digest, the thread index, attachment markers), and an extractor
    /// told the task is "here is a list of finished cards" would keep the wrong
    /// half of the payload. The same cut the triage and the budget-pause re-park
    /// already take, for the same reason.
    ///
    /// Absent on any path that has not been taught to set it, which the
    /// extractor treats as "no hint" and declines — no worse than before it
    /// existed.
    pub(crate) static TURN_TASK_HINT: String;
}

/// The current turn's task, when one is in scope.
pub(crate) fn current_task_hint() -> Option<String> {
    TURN_TASK_HINT.try_with(|hint| hint.clone()).ok()
}

/// Runs `fut` with `task` readable as the turn's task hint.
pub(crate) async fn with_task_hint<F: std::future::Future>(task: String, fut: F) -> F::Output {
    TURN_TASK_HINT.scope(task, fut).await
}

tokio::task_local! {
    /// The conversation the current turn is answering in (issue #1890 F).
    ///
    /// A task-local for the reason [`CHAT_ONLY_TURN`] is one: a tool's belt is
    /// built once per agent and a turn's conversation changes every message, so
    /// the tool cannot be handed it at construction. This is the ambient fact a
    /// tool reads at call time.
    ///
    /// It carries the **channel**, which is what scopes `read_thread`: a tool
    /// able to read any thread in any channel would reintroduce through the
    /// back door the leak #1890 A closed at the seed.
    static TURN_CONVERSATION: Option<String>;

    /// TinyHiveMind dispatch depth of the current conversational turn.
    static TURN_MESSAGE_HOP: u32;

    /// Whether the current turn has already SAID something through a speech
    /// tool (`desk_post` / `desk_dm` / `desk_close`).
    ///
    /// A task-local for the same reason `TURN_CONVERSATION` is one: the tool
    /// that sets it and the code that reads it are on opposite sides of the
    /// model loop, and neither is constructed per turn.
    ///
    /// This is what keeps the return-text fallback from double-posting. With
    /// `[speech] enabled`, an agent's line reaches the journal through the
    /// tool; its return text is then private thinking and must not be
    /// journaled a second time. But an agent that calls NO speech tool must
    /// still be heard — going silent because a model forgot a tool call is not
    /// an acceptable failure mode — so the fallback is gated on this flag
    /// rather than on the manifest knob alone.
    static TURN_SPEECH: std::sync::Arc<TurnSpeech>;
}

/// What one turn said through the speech tools.
///
/// One shared record rather than two task-locals: nesting a fourth
/// `task_local!` scope around the turn future pushed type inference past its
/// recursion limit, and two flags that are always set and read together were
/// never two facts anyway.
#[derive(Debug, Default)]
pub struct TurnSpeech {
    /// Whether the turn said anything at all through a speech tool — including
    /// a `desk_dm`, which journals itself and so leaves no utterance here.
    spoke: std::sync::atomic::AtomicBool,
    /// What it asked to say to its whole channel, in call order.
    ///
    /// `desk_post` and `desk_close` do **not** append. The crate's own rule is
    /// that *"a tool call is a request to speak — the host appends, the host
    /// decides"*, and the host that appends is the reply path that has always
    /// appended: it carries the folded steps, the live SSE frame, the resolved
    /// mentions and the board-card correlation, none of which a tool holds.
    ///
    /// `desk_dm` is the exception and journals directly, because a narrowed
    /// audience is not something a turn's single reply can express.
    utterances: std::sync::Mutex<Vec<String>>,
}

impl TurnSpeech {
    /// Whether this turn was heard.
    pub fn spoke(&self) -> bool {
        self.spoke.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The channel-visible lines, in call order.
    pub fn utterances(&self) -> Vec<String> {
        // tinysweeper: a poisoned lock (another task panicked while holding
        // it) means the vector may be incomplete or inconsistent, but that is
        // not a reason to answer "said nothing" — this turn may well have
        // spoken before the panic, and silently discarding that is worse than
        // surfacing a possibly-incomplete list with the corruption logged so
        // it is detectable.
        match self.utterances.lock() {
            Ok(lines) => lines.clone(),
            Err(poisoned) => {
                tracing::error!(
                    "[delegation] turn-speech mutex poisoned; returning a possibly incomplete utterance list"
                );
                poisoned.into_inner().clone()
            }
        }
    }
}

/// A fresh, silent record for one turn.
///
/// Shared rather than task-local-owned because the two readers are on opposite
/// sides of the scope: the tools write it *inside* the turn, and the reply path
/// reads it *after* the turn has returned and the task-local is gone.
pub fn new_turn_speech() -> std::sync::Arc<TurnSpeech> {
    std::sync::Arc::new(TurnSpeech::default())
}

/// Runs `fut` with `speech` as this turn's speech record.
pub(crate) async fn with_turn_speech<F: std::future::Future>(
    speech: std::sync::Arc<TurnSpeech>,
    fut: F,
) -> F::Output {
    TURN_SPEECH.scope(speech, fut).await
}

/// Records that this turn has said something through a speech tool.
pub fn mark_turn_spoke() {
    let _ = TURN_SPEECH.try_with(|speech| {
        speech
            .spoke
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Records one channel-visible utterance for this turn.
///
/// Returns whether it was collected: `false` outside a tracked turn, or when
/// the turn's speech mutex is poisoned, which tells the caller to fall back to
/// appending it itself. tinysweeper: the poisoned branch used to fall through
/// silently with no signal that anything was wrong; it now logs, so the
/// corruption is detectable rather than reading as an ordinary "no active
/// scope" — the caller's existing direct-append fallback still runs either
/// way, so the text itself is not lost.
pub fn collect_utterance(text: String) -> bool {
    TURN_SPEECH
        .try_with(|speech| match speech.utterances.lock() {
            Ok(mut lines) => {
                lines.push(text);
                speech
                    .spoke
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Err(_) => {
                tracing::error!(
                    "[delegation] turn-speech mutex poisoned; falling back to a direct append"
                );
                false
            }
        })
        .unwrap_or(false)
}

/// Run `fut` with the current turn's channel set (issue #1890 F).
pub(crate) async fn with_turn_conversation<F: std::future::Future>(
    chat_id: Option<String>,
    fut: F,
) -> F::Output {
    TURN_CONVERSATION.scope(chat_id, fut).await
}

pub(crate) async fn with_turn_message_hop<F: std::future::Future>(hop: u32, fut: F) -> F::Output {
    TURN_MESSAGE_HOP.scope(hop, fut).await
}

pub(crate) fn turn_message_hop() -> u32 {
    TURN_MESSAGE_HOP.try_with(|hop| *hop).unwrap_or(0)
}

/// The channel the current turn is answering in, or `None` outside one — a
/// dispatched card, a workflow node, or any path that never set it.
///
/// `None` is a refusal for `read_thread` rather than a wildcard: a turn with no
/// conversation has no threads it is entitled to read.
pub(crate) fn turn_conversation() -> Option<String> {
    TURN_CONVERSATION.try_with(Clone::clone).ok().flatten()
}

tokio::task_local! {
    /// The engine a tool may run ONE bounded peer turn on, for the length of
    /// this turn (#2368).
    ///
    /// `desk_dm` starts an exchange it has no way to finish. It queues the
    /// recipient's turn and returns `"Left for @peer."` — a success for work
    /// that has not happened — so the asking turn ends on a receipt, the answer
    /// lands in the pair thread after that turn closed, and nothing wakes the
    /// asker to read it. In a live run the reply sat unread at seq 49 until an
    /// operator message a turn later happened to sweep it up. The asker is not
    /// at fault: it was told its request succeeded.
    ///
    /// A task-local for the reason [`TURN_CONVERSATION`] is one — the belt is
    /// built once per agent, and the engine is not known at that point — and
    /// set brain-side, where [`RunTurn`] already exists, exactly as
    /// [`CHAT_ONLY_TURN`] is. Rides an existing scope rather than nesting a new
    /// one around the turn future: a fourth `task_local!` there already pushed
    /// type inference past its recursion limit (see [`TurnSpeech`]).
    ///
    /// Absent — a test, a non-harness caller, a turn with no engine — means the
    /// tool keeps the queue path, which is the behaviour this replaces rather
    /// than a degraded one: the message is still durable, still journaled, and
    /// still read on the recipient's next turn.
    static TURN_PEER_RUNNER: Option<Arc<dyn RunTurn>>;
}

/// Run `fut` with the bounded peer-turn engine available to this turn's tools.
pub async fn with_peer_runner<F: Future>(runner: Option<Arc<dyn RunTurn>>, fut: F) -> F::Output {
    TURN_PEER_RUNNER.scope(runner, fut).await
}

/// The engine a tool may run one bounded peer turn on, if this turn has one.
#[must_use]
pub fn peer_runner() -> Option<Arc<dyn RunTurn>> {
    TURN_PEER_RUNNER.try_with(Clone::clone).ok().flatten()
}

/// Run `fut` with the [`CHAT_ONLY_TURN`] hint set to `chat_only`.
pub(crate) async fn with_chat_only_hint<F: std::future::Future>(
    chat_only: bool,
    fut: F,
) -> F::Output {
    CHAT_ONLY_TURN.scope(chat_only, fut).await
}

/// Whether the current turn was marked chat-only by the delegation runner.
/// Reads the ambient [`CHAT_ONLY_TURN`] hint; `false` when unset (every path
/// that does not opt in, e.g. dispatched task cards and background turns).
pub(crate) fn is_chat_only_turn() -> bool {
    CHAT_ONLY_TURN.try_with(|v| *v).unwrap_or(false)
}

#[cfg(test)]
#[path = "delegation_tests_core.rs"]
mod tests_core;
#[cfg(test)]
#[path = "delegation_tests_core2.rs"]
mod tests_core2;
#[cfg(test)]
#[path = "delegation_tests_part1.rs"]
mod tests_part1;
#[cfg(test)]
#[path = "delegation_tests_part2.rs"]
mod tests_part2;
#[cfg(test)]
#[path = "delegation_tests_part3.rs"]
mod tests_part3;
#[cfg(test)]
#[path = "delegation_tests_part4.rs"]
mod tests_part4;
#[cfg(test)]
#[path = "delegation_tests_part5.rs"]
mod tests_part5;
#[cfg(test)]
#[path = "delegation_tests_part6.rs"]
mod tests_part6;
#[cfg(test)]
#[path = "delegation_tests_part7.rs"]
mod tests_part7;
#[cfg(test)]
#[path = "delegation_tests_part8.rs"]
mod tests_part8;
#[cfg(test)]
#[path = "delegation_tests_part9.rs"]
mod tests_part9;

#[cfg(test)]
#[path = "delegation_task_hint_tests.rs"]
mod task_hint_tests;
