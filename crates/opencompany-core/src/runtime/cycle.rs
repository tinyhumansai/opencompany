//! [`CycleRunner`]: the serial drain → load → think → gate → persist loop.
//!
//! One cycle turns a batch of [`CompanyEvent`]s into a [`CycleReport`]:
//!
//! 1. **Drain** — accept the batched events.
//! 2. **Persist input** — append each event to the log (durable before work).
//! 3. **Load** — recent traces, the context index, and the roster.
//! 4. **Think** — call the brain, servicing its callbacks through a
//!    [`CycleHost`] that gates every emitted effect.
//! 5. **Gate** — inside the host: evaluate, then execute (at-most-once), park,
//!    or deny each effect.
//! 6. **Persist output** — save traces and ledger deltas, meter the cycle's
//!    inference usage, and route channel responses to their adapters.
//!
//! Step 6's metering is the *generic* cost seam: whatever the brain reports as
//! [`CycleResult::token_usage`](crate::ports::types::CycleResult::token_usage)
//! lands on the Usage/Finances surfaces, so hosted Medulla cognition is metered
//! like the openhuman harness instead of reading a blind zero (issue #174).
//!
//! The per-company serial lock is held for the whole cycle, so cycles never
//! interleave within a company while distinct companies stay concurrent.

use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::Result;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::feedback::tool::SEND_EMAIL_TOOL;
use crate::policy::gate::ResolveOutcome;
use crate::ports::brain::{CycleHost, UsageMetering};
use crate::ports::runs::{RunFilter, RunOutcome, RunStatus};
use crate::ports::tasks::{COLUMN_TODO, TaskOrigin, TaskRecord, column_label};
use crate::ports::types::MessageIntent;
use crate::ports::types::{
    Actor, ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult,
    CycleRequest, CycleResult, Effect, EffectDisposition, EffectGroup, EventSeq, LedgerEntry,
    OutboundMessage, PolicyDecision, TokenUsage, ToolCall, ToolResult, Verdict,
};
use crate::ports::{generate_id, now_millis};
use crate::runtime::channel::OPERATOR_CHANNEL;
use crate::runtime::delegation_tools::{
    DELEGATE_TO_DESK_TOOL, DelegateArgs, SPAWN_TASK_TOOL, SpawnTaskArgs, chat_responder, desk_lead,
    unknown_desk_message,
};
use crate::runtime::grants::{
    ApprovalContinuation, GrantId, GrantScope, GrantSubject, GrantedCall, StandingGrant,
};
use crate::runtime::journal::{ApprovalConversation, ExecutedEffect, TaskLink};
use crate::runtime::types::CycleReport;
use crate::server::chat_history;
use crate::server::ops::mailer::{MailCredentials, OutboundEmail};

/// The `Effect::kind` for an outbound email send. Shared between where the
/// effect is built (`CycleHostImpl::send_email`, and the workflow delivery path
/// in [`crate::workflows::delivery`]) and where it is executed
/// (`perform_effect`) so they can't drift apart.
///
/// `pub(crate)` because delivery parks an effect this same executor has to
/// recognise on approval: a duplicated `"email.send"` literal over there would
/// park cards that silently do nothing when approved.
pub(crate) const EMAIL_SEND_KIND: &str = "email.send";

/// The `error` the terminality backstop stamps on an attempt row whose cycle
/// ended without settling it (issue #242) — a brain that ignored the dispatch,
/// not a brain that failed at it.
pub(crate) const RUN_UNSETTLED_ERROR: &str =
    "the dispatch cycle ended without settling this attempt";

/// The `error` prefix the backstop stamps when the cycle itself errored, so the
/// row carries the same reason the caller saw rather than a generic one.
pub(crate) const RUN_CYCLE_FAILED_ERROR: &str = "the dispatch cycle failed";

/// Where the machine-appended part of a desk-addressed operator message begins
/// (issue #176's handed-task awareness, written by
/// [`inject_handed_task_awareness`](CycleRunner::inject_handed_task_awareness)).
///
/// **An operator message is not only what the operator typed.** For a message
/// addressed to a desk or teammate, the cycle appends a briefing of that
/// target's open cards before the brain ever sees it — so `text` arrives as
/// `<what the operator wrote>` + this marker + `<a list of card lines>`.
///
/// Since issue #1859 each line carries more than a title: the card's board
/// column ([`column_label`]) and, when at least one attempt has run, the
/// latest attempt's 1-based ordinal and [`RunStatus`] — so the briefing (and
/// the model reading it) can distinguish "todo, never attempted" from "paused
/// on its second attempt" instead of rendering every open card identically.
///
/// This exists as a shared constant because issue #442 needs to read the
/// operator's own words back out of that: it decides whether a message asks for
/// something substantial enough to open a card, and scoring the appended card
/// list instead made every desk message look substantial — including "thanks!".
/// Self-amplifying, too: each card it opened lengthened the briefing on the next
/// message, which opened another.
///
/// A `const` rather than a literal in each place so the writer and the reader
/// cannot drift apart. Anything that reasons about an operator message's
/// *content* must split on this first; see
/// [`operator_words`](crate::runtime::delegation::operator_words), whose test
/// builds its input from this constant so a wording change fails the test rather
/// than silently un-splitting the message.
pub(crate) const OPEN_WORK_ANNOTATION: &str = "\n\n[Open work already handed to you";

/// Cap on how many of a target's open cards get a `list_runs` attempt lookup
/// while building the handed-task briefing (issue #176).
///
/// The lookup runs once per matching card while the per-agent/serial cycle
/// guard is held, so an assignee with many open cards would otherwise pay one
/// store round trip per card, in sequence, before the brain even sees the
/// message. Bounding it keeps the worst case constant regardless of board
/// size; cards past the cap still render with their column, just without an
/// attempt clause — the same shape a never-attempted card already renders as.
const HANDED_TASK_ATTEMPT_LOOKUP_CAP: usize = 8;

/// Where the thread index begins on an operator message (issue #1890 E,
/// written by [`inject_thread_index`](CycleRunner::inject_thread_index)).
///
/// The fourth machine-appended part of an operator message, on exactly
/// [`OPEN_WORK_ANNOTATION`]'s terms: in-memory only, never journaled, and
/// stripped by [`operator_words`](crate::runtime::delegation::operator_words)
/// before anything reasons about what the operator asked for.
///
/// # What it is for
///
/// A thread scoped to itself (#1890 A) is **cold by construction**: the turn
/// answering in it sees that thread and nothing else, which is the whole point
/// and also means it does not know what else its channel is about. A reference
/// to "the other thread" resolves to nothing, and a channel-level turn asked
/// "where are we?" can speak only for the channel line.
///
/// This is the orientation, folded into the prompt rather than into history —
/// because history is what A scoped, and widening it again would undo A. The
/// same seam and the same terms as its three siblings.
///
/// # Sized for deciding, never for knowing
///
/// Each line is the root's own opening words, its state, and its recency, and
/// **that is the whole budget**. If lines grow long enough to answer *from*,
/// the flat channel window A removed has been rebuilt in the prompt and paid
/// for twice — the failure this constant's own shape has to prevent.
///
/// The opening words are the operator's, verbatim and truncated, never
/// summarised: summarising costs a model call per thread per turn and loses the
/// exact words a later reference will echo. They are the discriminator, so
/// "the launch email one" resolves.
///
/// # Default is not to read
///
/// Most turns reference nothing outside their own thread, so the instruction
/// gates on an *explicit* reference. Over-reading is the failure mode to guard
/// hardest: an agent that pulls three threads to be safe has silently undone A.
/// Where a reference is ambiguous across the index, asking beats guessing and
/// beats reading all three.
pub(crate) const THREAD_INDEX_ANNOTATION: &str = "\n\n[Other conversations in this channel";

/// Where the settled-work briefing begins on an operator message addressed to a
/// conversation that has raised work (issue #1890 C, written by
/// [`inject_handed_task_awareness`](CycleRunner::inject_handed_task_awareness)).
///
/// The third machine-appended part of an operator message, on exactly
/// [`OPEN_WORK_ANNOTATION`]'s terms: in-memory only, never journaled, and
/// stripped by [`operator_words`](crate::runtime::delegation::operator_words)
/// before anything reasons about what the operator asked for.
///
/// # What it is for
///
/// A card raised from a conversation settles, and `chat_history::owns` files a
/// `finished → In review` marker back into that conversation — so the operator
/// can see it. The **model** cannot: the chat seed drops the marker for want of
/// a conversational body, which is correct (a settle is not a turn) and leaves
/// the one durable fact answering *"did that ship?"* on screen and absent from
/// context. This is that fact, as briefing rather than as a turn.
///
/// # Why not a seed line
///
/// `seed_resume_from_messages` recognises `user`, `agent` and `assistant`, and
/// **falls back to the user role for anything else** — losing context being
/// worse than mislabelling it, on its own terms. So a marker emitted into the
/// seed under a `system` role would reach the model as though the operator had
/// typed "finished → In review". Worse than dropping it, and the reason C is a
/// briefing at all. The seed would also only be rebuilt on a *switch*, so a
/// settle landing mid-conversation would never arrive.
///
/// # It reports the board, not the marker
///
/// The console's marker is rendered from the journal and frozen at settle time;
/// a card dragged to Done afterwards still reads `finished → In review` on
/// screen. This briefing reads the card's **current** column instead, so it
/// answers "did that ship?" with where the work actually is. The two can
/// therefore disagree, deliberately: the screen is a record of what happened,
/// and this is a statement of what is true now.
pub(crate) const SETTLED_WORK_ANNOTATION: &str = "\n\n[Work raised in this conversation";

/// Where the builder-pass briefing begins on a `workflow`-deliverable operator
/// message (issue #845, written by
/// [`inject_workflow_builder_awareness`](CycleRunner::inject_workflow_builder_awareness)).
///
/// The second machine-appended part of an operator message, on exactly
/// [`OPEN_WORK_ANNOTATION`]'s terms: in-memory only, never journaled, and
/// stripped by [`operator_words`](crate::runtime::delegation::operator_words)
/// before anything reasons about what the operator asked for.
///
/// # What it is for
///
/// A message sent with "Build me the workflow" opens a card the **builder pass**
/// owns ([`crate::harness::workflow_build`]): the card does not dispatch to its
/// assignee, and authoring the graph *is* its In-Progress work. The chat cycle
/// still runs on the same message, in parallel — and the desk agent answering it
/// holds no workflow-authoring tool, correctly refuses to pretend otherwise, and
/// says so.
///
/// Both halves were behaving correctly and the operator was told the opposite of
/// what was happening: on staging, "I can't build the workflow … `weekly-aeo-audit`
/// does not exist and I cannot make it exist" was delivered while a proposal for
/// exactly that workflow was landing In Review. This annotation is what tells the
/// turn who owns the authoring, so it answers the substance instead of denying a
/// capability that is being exercised on its own message.
///
/// It grants nothing. `create_workflow` stays orchestrator-only, the builder
/// still only *proposes*, and a person still applies the proposal — this only
/// stops the turn from contradicting that.
pub(crate) const BUILDER_ANNOTATION: &str = "\n\n[This request is already being built";

/// What settling an approval's verdict produced — the outcome of the fast half
/// of a resolve, before any model is called (issue #383).
///
/// Every arm means the operator has no decision left to make. They differ in
/// what is still owed and, crucially, in **what may be claimed about the
/// operator**: `Settled` owes one follow-up cycle and is the only arm that may
/// be journaled as this person's verdict; `AlreadyResolved` and `Expired` owe
/// no cycle because nothing of the operator's was recorded at all.
#[derive(Debug, Clone)]
pub enum ResolveReceipt {
    /// Nothing was parked under this id — an unknown id, or one a concurrent
    /// request (or a double-click) already resolved. No journal record was
    /// written and no cycle is owed. Issue #243 made this a safe no-op rather
    /// than a second grant; surfacing it here lets the HTTP layer say so.
    AlreadyResolved,
    /// The approval was still parked but past its deadline, so the gate
    /// default-denied it whatever the operator asked for (issue #1449).
    ///
    /// **This is not the operator's verdict and must never be recorded as
    /// one.** The click arrived too late to be a decision: no grant is minted,
    /// nothing is executed, and the durable record is an
    /// [`ApprovalExpired`](crate::runtime::journal) — the same line the sweeper
    /// writes for the identical outcome reached by silence. Before this the arm
    /// fell through to `Settled`, so a late click journaled a named operator
    /// approving something the host had already refused, and told them in green
    /// that it was being carried out.
    ///
    /// The retirement transaction it owes — journal, pending mark, continuation
    /// release, event — is
    /// [`CompanyRuntime::retire_approval`](crate::runtime::CompanyRuntime), run
    /// by the caller that holds the `Arc`. See
    /// [`settle_approval`](CycleRunner::settle_approval).
    Expired,
    /// The verdict is journaled and any approved effect settled — the grant is
    /// minted, or the native effect executed. The carried `ApprovalResolved` is
    /// the event the follow-up cycle must run so the brain learns the verdict.
    ///
    /// Boxed: `CompanyEvent` is a wide enum (its largest variant is the
    /// workflow-run outcome), and holding it inline made every
    /// `AlreadyResolved` — the common answer — pay that width. Indirection here
    /// costs one allocation on the settled path only.
    Settled(Box<CompanyEvent>),
}

impl ResolveReceipt {
    /// Whether this resolve found nothing left to resolve.
    pub fn already_resolved(&self) -> bool {
        matches!(self, Self::AlreadyResolved)
    }

    /// Whether the deadline decided this rather than the operator (issue #1449).
    pub fn expired(&self) -> bool {
        matches!(self, Self::Expired)
    }

    /// The wire discriminator for what actually happened (issue #1449).
    ///
    /// A string rather than a third boolean because the arms are mutually
    /// exclusive and there is a fourth coming: two independent `bool`s can spell
    /// states that cannot exist, and every console reading them has to know
    /// which combinations are real. One field with one value per arm cannot.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::AlreadyResolved => "already_resolved",
            Self::Expired => "expired",
            Self::Settled(_) => "settled",
        }
    }
}

/// Drives cycles for one [`CompanyRuntime`].
/// A short, stable label for what a cycle is running (issue #390).
///
/// Read off the driving events rather than passed in, so every entry point gets
/// one without threading a string through. It exists so an operator looking at
/// an open bracket can tell a stuck approval continuation — the case #390 is
/// about — from a stuck chat turn, without joining the bracket to anything.
///
/// Deliberately coarse and deliberately not the event's payload: this lands in a
/// durable record, and a label is not the place for message text or tool
/// arguments.
fn cycle_trigger_of(events: &[(Option<EventSeq>, CompanyEvent)]) -> String {
    match events.first() {
        Some((_, first)) => cycle_trigger(std::slice::from_ref(first)),
        None => "empty".to_string(),
    }
}

fn cycle_trigger(events: &[CompanyEvent]) -> String {
    let Some(first) = events.first() else {
        return "empty".to_string();
    };
    match first {
        CompanyEvent::ApprovalResolved { .. } => "approval-continuation",
        CompanyEvent::OperatorMessage { .. } => "operator-message",
        CompanyEvent::TaskDispatched { .. } => "task-dispatch",
        CompanyEvent::AgentReply { .. } => "agent-reply",
        _ => "other",
    }
    .to_string()
}

pub struct CycleRunner<'a> {
    rt: &'a CompanyRuntime,
}

/// The single agent this cycle is addressed to, if there is exactly one.
///
/// `None` means "touches the whole company" and yields the company-wide
/// [`serial`](crate::company::runtime::CompanyRuntime::serial) lock. That is the
/// safe side: better to serialize a cycle that could have run beside another
/// than to let two turns that write each other's state overlap.
///
/// A batch naming two different agents is deliberately whole-company — that one
/// cycle runs both turns, so it must serialize against each of them.
fn single_agent(events: &[(Option<EventSeq>, CompanyEvent)]) -> Option<String> {
    let mut found: Option<&str> = None;
    for (_, event) in events {
        let chat = match event {
            CompanyEvent::OperatorMessage { chat, .. } => chat.as_deref(),
            // Any other event kind may touch the whole company and so takes the
            // wide lock. If a second per-agent event kind is ever added, it
            // belongs here explicitly.
            _ => return None,
        };
        // A message with no `chat` is routed to the orchestrator, which may
        // drive the whole company.
        let name = chat?;
        match found {
            None => found = Some(name),
            Some(prev) if prev == name => {}
            Some(_) => return None,
        }
    }
    found.map(str::to_owned)
}

/// The answer to a bare pleasantry, when this batch is one (issue #1725) — and
/// `None` whenever anything about it says a real turn is owed.
///
/// # Why the runtime answers this itself
///
/// On staging, "hi" ran the full agentic pipeline: memory retrieval, a tool
/// step, and a long analysis belonging to a task nobody had asked about in that
/// turn. Two things produced that, and only one of them is in this repository.
/// The vendored turn re-injects an uncompleted per-thread goal on **every**
/// turn (`threads::goals::runtime::load_for_current_thread`, which also
/// *resumes* a paused one), and the pooled agent's transcript is keyed by agent
/// id alone, so a prior task's fetched page is still in the context window. A
/// greeting therefore did not merely cost a model call — it inherited somebody
/// else's objective and continued it.
///
/// Both of those are properties of a turn that runs. The one fix available on
/// this side of the seam, and the one that holds regardless of what the vendored
/// runtime does with its goals, is not to run the turn: no model call, no tool,
/// no memory read, no goal to inherit, and nothing written back for a later turn
/// to retrieve.
///
/// # The conditions are narrow on purpose
///
/// A fast path that fires on a message that *was* work is a far worse bug than
/// the one it fixes — the operator's request is answered with a canned
/// pleasantry and silently dropped. So every arm here is a reason NOT to
/// short-circuit:
///
/// * **One event, and an operator message.** A batch is a scheduler tick, a
///   dispatch, or several messages at once; none of those are small talk.
/// * **Nothing attached.** A file with "hi" over it is a request to look at the
///   file.
/// * **No explicit work choice.** The composer's "Build me the workflow" /
///   "one-off" is a positive statement by the person who wrote the message
///   (issue #845's reasoning, in the other direction), and it outranks anything
///   read out of the words. Only an absent choice and "Just chatting" reach
///   here.
/// * **Not a workflow copilot thread**, whose turns are confined and answered
///   by an ephemeral agent this has no way to speak as (issue #416).
/// * **A pleasantry, by [`small_talk`](crate::company::task_intent::small_talk)** —
///   which is narrower than the triage's greeting list, and excludes every
///   acknowledgement, because "yes" answering *"shall I ship it?"* is an
///   instruction.
/// * **Somebody to say it.** A company whose roster resolves to nobody has no
///   voice to answer in, and an unattributed bubble is journaled under the
///   operator (issue #885).
fn small_talk_result(record: &CompanyRecord, events: &[CompanyEvent]) -> Option<CycleResult> {
    let [
        CompanyEvent::OperatorMessage {
            text,
            chat,
            deliverable,
            attachments,
            ..
        },
    ] = events
    else {
        return None;
    };
    if !attachments.is_empty() {
        return None;
    }
    if !matches!(deliverable, None | Some(MessageIntent::Chat)) {
        return None;
    }
    if crate::company::copilot::is_copilot_thread(chat.as_deref()) {
        return None;
    }
    let talk = crate::company::task_intent::small_talk(text)?;

    // Who speaks. The same resolution the harness brain's `responder_for` runs,
    // so the greeting comes back in the voice the turn it replaced would have
    // used.
    //
    // A company with nobody to speak as declines the fast path rather than
    // answering anonymously: `agent: None` is read by `journal_chat_replies` as
    // the destination channel, so an unattributed bubble on the operator
    // channel is filed as though the operator had written it — permanently, and
    // in the transcript rather than only on screen (issue #885). Better to run
    // the turn a roster-less company was always going to run.
    let responder = chat
        .as_deref()
        .and_then(|chat| chat_responder(record, chat))
        .or_else(|| {
            crate::company::orchestrator_id(&record.effective_agents()).map(str::to_string)
        })?;

    Some(CycleResult {
        channel_responses: vec![OutboundMessage {
            message_id: None,
            task_id: None,
            outputs: Vec::new(),
            channel: OPERATOR_CHANNEL.to_string(),
            agent: Some(responder),
            text: talk.reply().to_string(),
            // No tool ran, so the timeline is empty rather than the "1 step"
            // the console showed for a greeting.
            steps: Vec::new(),
            reply_to: None,
            mentions: Vec::new(),
        }],
        // Nothing is written back to memory. A pleasantry is not something a
        // later turn should retrieve, and the whole point of skipping the turn
        // is that this exchange leaves no context behind it.
        new_traces: Vec::new(),
        ledger_deltas: Vec::new(),
        // No model was called. A zero here is the truth, and `record_cycle_usage`
        // meters it as such.
        token_usage: TokenUsage::default(),
    })
}

impl<'a> CycleRunner<'a> {
    /// Binds a runner to a runtime.
    pub fn new(rt: &'a CompanyRuntime) -> Self {
        Self { rt }
    }

    /// Runs one cycle over `events`, holding the per-company serial lock.
    ///
    /// # The bracket opens before the lock (issue #390)
    ///
    /// `cycle_id` is minted **here**, not inside [`run_locked`](Self::run_locked)
    /// where it used to be, and the journal's `started` record is written before
    /// `serial.lock()` is awaited. The lock is held for a whole cycle, so a
    /// continuation queued behind a busy company waits on it for an unbounded
    /// time — and a host that dies in that wait is precisely the "I approved and
    /// nothing happened" failure this bracket exists to make visible. Opening it
    /// after the lock would report that case as though the cycle had never been
    /// asked for.
    ///
    /// Moving the mint is safe because nothing between `run_locked`'s first
    /// statement and the old mint site read it: the input-append loop, the
    /// `begin_run` call and the history/context/roster loads are all keyed on
    /// the company and the event, never on the cycle.
    /// `a_cycles_id_is_minted_before_the_serial_lock` pins the placement, since
    /// the failure mode here is a correctly-typed id written in the wrong place
    /// rather than anything the compiler can see.
    ///
    /// Paths that owe no cycle open no bracket: `already_resolved_report` and
    /// `still_waiting_report` return without reaching this function. That is
    /// deliberate — a banked decision is *correctly* not running, and giving it
    /// a bracket would leave an open cycle that never closes and never should.
    pub async fn run(&self, events: Vec<CompanyEvent>) -> Result<CycleReport> {
        // Every input still needs appending — this is the wrapper every trigger
        // but the chat route uses, and it is byte-unchanged from its pre-#983
        // self.
        self.run_bracketed(
            events.into_iter().map(|event| (None, event)).collect(),
            None,
            Vec::new(),
        )
        .await
    }

    /// [`run`](Self::run), for inputs whose journal append has **already
    /// happened** (issue #983).
    ///
    /// The chat route appends the operator's message the instant the request is
    /// accepted, before it takes this lock, so `chat/history` is correct from
    /// acceptance rather than from whenever the cycle wins the per-company
    /// mutex — which, behind a busy company, is an unbounded time later. That
    /// append must not then happen a second time in here, so the caller hands
    /// over each event together with the [`EventSeq`] it was appended under and
    /// this skips the write.
    ///
    /// **Everything downstream stays keyed on the supplied seqs.** The
    /// `TaskDispatched` → `begin_run` handling, `CycleReport::input_seqs` (and
    /// therefore the chat response's `messageId`), and the seq list the brain
    /// sees are all built from them, so a pre-journaled cycle is
    /// indistinguishable from an appending one everywhere except in who wrote
    /// the line.
    ///
    /// `run_id` is a run row to move `Pending` → `Running` once the serial lock
    /// is actually held. That placement is the point: a chat turn's row is
    /// minted at accept time, so `Pending` means "queued behind other turns" and
    /// `Running` means "owns the lock" — a distinction the caller cannot make
    /// from outside, because the wait on the lock happens in here.
    pub async fn run_journaled(
        &self,
        events: Vec<(EventSeq, CompanyEvent)>,
        run_id: Option<String>,
    ) -> Result<CycleReport> {
        self.run_bracketed(
            events
                .into_iter()
                .map(|(seq, event)| (Some(seq), event))
                .collect(),
            run_id,
            Vec::new(),
        )
        .await
    }

    /// Runs a released explicit-approval batch, claiming its continuations only
    /// after this cycle owns the company/agent lock.
    pub async fn run_continuation(
        &self,
        events: Vec<CompanyEvent>,
        continuations: Vec<ApprovalContinuation>,
    ) -> Result<CycleReport> {
        self.run_bracketed(
            events.into_iter().map(|event| (None, event)).collect(),
            None,
            continuations,
        )
        .await
    }

    /// The shared body of both entry points: open the journal bracket, take the
    /// serial lock, run, close the bracket.
    async fn run_bracketed(
        &self,
        events: Vec<(Option<EventSeq>, CompanyEvent)>,
        run_id: Option<String>,
        continuation_claims: Vec<ApprovalContinuation>,
    ) -> Result<CycleReport> {
        let cycle_id = crate::ports::generate_id();
        let trigger = cycle_trigger_of(&events);
        // Issue #1739. Nothing in this tree measures how long a cycle takes —
        // the journal records that one started and that one finished, never the
        // span between — so this is new instrumentation rather than a read of
        // something already kept. `Instant` because the only question is a
        // duration, and a wall clock that steps backwards mid-cycle would
        // report a negative one.
        let started_at = std::time::Instant::now();
        let analytics_trigger =
            crate::analytics::Trigger::of(events.first().map(|(_, event)| event));

        // Best-effort, and it must stay that way: record-keeping does not get to
        // refuse a cycle. A failed open simply means this cycle is unbracketed,
        // which is the pre-#390 behaviour rather than a new failure.
        if let Err(err) = self
            .rt
            .journal
            .record_cycle_started(&cycle_id, &trigger)
            .await
        {
            tracing::warn!(
                company = %self.rt.id,
                cycle = %cycle_id,
                %err,
                "could not journal a cycle start; this cycle runs unbracketed"
            );
        }

        // Which agent is this cycle addressed to?
        //
        // If it is exactly one, take only that agent's slot so two operators
        // talking to two different agents run side by side. If the cycle touches
        // the whole company (a scheduler tick, an unaddressed message routed to
        // the orchestrator, or a batch naming more than one agent), take
        // `serial` and serialize against everything — including every in-flight
        // agent turn.
        //
        // The per-agent slot is looked up (or created) under a short-lived lock
        // on the map, which is released immediately: holding the map for the
        // whole turn would reintroduce exactly the serialization this lifts. The
        // guard chosen below outlives the bracket close, so neither lock can be
        // released before the critical section it describes ends.
        // Both branches yield the same guard type — an owned guard over an
        // `Arc<tokio::sync::Mutex<()>>` — so the choice of which lock to hold does not
        // leak into the rest of this function.
        let guard = match single_agent(&events) {
            Some(agent) => {
                let slot = {
                    let mut slots = self.rt.per_agent.lock().await;
                    slots
                        .entry(agent)
                        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                        .clone()
                };
                slot.lock_owned().await
            }
            None => self.rt.serial.clone().lock_owned().await,
        };
        // Codex review finding on PR #2140 (`3951723394`): `ensure_accepting`
        // (or `ensure_not_emergency_stopped` for a continuation) is checked by
        // the caller before this lock is even requested, and that wait is
        // unbounded — "behind a busy company, an unbounded time later" per this
        // function's own doc above. A stop engaged while a cycle queues behind
        // this lock must still catch it once the lock is actually held, or a
        // queued request starts a turn after the switch was pulled. Checked
        // before the journal is touched, so a refusal here leaves nothing
        // claimed and nothing to unwind.
        if let Err(err) = self.rt.ensure_not_emergency_stopped() {
            if let Err(finish_err) = self
                .rt
                .journal
                .record_cycle_finished(&cycle_id, Some(err.to_string()))
                .await
            {
                tracing::warn!(
                    company = %self.rt.id,
                    cycle = %cycle_id,
                    %finish_err,
                    "could not journal a cycle finish for a stop-refused cycle"
                );
            }
            drop(guard);
            return Err(err);
        }
        let claimed_grants: Vec<GrantedCall> = events
            .iter()
            .filter_map(|(_, event)| match event {
                CompanyEvent::ApprovalResolved { approval_id, .. } => {
                    self.rt.grants.peek(approval_id)
                }
                _ => None,
            })
            .collect();
        for grant in &claimed_grants {
            self.rt
                .journal
                .record_grant_dispatched(&grant.approval_id, now_millis())
                .await?;
        }
        let mut claimed: Vec<ApprovalContinuation> = Vec::new();
        for continuation in continuation_claims {
            if let Err(error) = self
                .rt
                .journal
                .record_approval_continuation_dispatched(
                    &continuation.call.approval_id,
                    now_millis(),
                )
                .await
            {
                for previous in &claimed {
                    if let Err(requeue_error) =
                        self.rt.journal.record_approval_continuation(previous).await
                    {
                        tracing::error!(
                            approval_id = %previous.call.approval_id,
                            %requeue_error,
                            "[approval] a partial batch dispatch claim could not be requeued"
                        );
                    }
                }
                return Err(error);
            }
            claimed.push(continuation);
        }
        let mut effects = EffectCounts::default();
        // Issue #1846 review (Codex #3865812419/#3865812423/#3865812432):
        // the ambient `RedeemContext` for this cycle, read by every
        // `BudgetPauseSet::park` call underneath it (the top-level turn, a
        // CEO-relay call, a delegate's own turn) so a redeem replays the
        // operator's ORIGINAL thread parent, deliverable choice, and
        // resolved mentions instead of empty defaults. Derived from `events`
        // before the batch moves into `run_locked` — see
        // `RedeemContext::from_events` for why "first `OperatorMessage`" is
        // the right read.
        let redeem_context = crate::runtime::grants::RedeemContext::from_events(&events);
        let outcome = crate::runtime::grants::with_redeem_context(
            redeem_context,
            self.run_locked(events, cycle_id.clone(), run_id, &mut effects),
        )
        .await;
        for grant in &claimed_grants {
            if self.rt.grants.peek(&grant.approval_id).is_some()
                && let Err(err) = self.rt.journal.record_granted(grant).await
            {
                tracing::warn!(
                    approval_id = %grant.approval_id,
                    error = %err,
                    "[approval] an unused single-use grant could not be re-armed after its turn"
                );
            }
        }
        if outcome.is_ok() {
            // Harness cognition consumes while redispatching and run_locked
            // journals that buffered fact. Hosted and sidecar cognition instead
            // receive the verdict event directly, so successful return is their
            // delivery acknowledgement and the outer runner closes the claimed
            // continuation here.
            for continuation in &claimed {
                let id = &continuation.call.approval_id;
                if self.rt.grants.consume_continuation(id).is_some()
                    && let Err(err) = self
                        .rt
                        .journal
                        .record_approval_continuation_consumed(id)
                        .await
                {
                    tracing::warn!(
                        approval_id = %id,
                        error = %err,
                        "[approval] a fallback decision continuation completed but its journal \
                         record failed; a restart may repeat the follow-up cycle"
                    );
                }
            }
        }
        // Issue #1739: the product's unit of work, reported as shape and outcome.
        //
        // Emitted here rather than inside `run_locked` for the same reason the
        // bracket is opened here: this is where a cycle's whole span is
        // observable, including the wait on the serial lock, which is the part
        // an operator experiences as "nothing is happening". Nothing is awaited
        // — `Tracker::track` is synchronous and infallible — so a turn is never
        // delayed by, and can never fail because of, analytics.
        self.rt
            .tracker
            .track(crate::analytics::Event::TurnFinished {
                trigger: analytics_trigger,
                outcome: match &outcome {
                    Ok(_) => crate::analytics::Outcome::Ok,
                    Err(_) => crate::analytics::Outcome::Failed,
                },
                // The coarse class only. `err.to_string()` is the single richest
                // source of user content in this crate — absolute paths, company
                // ids, MCP server names, tool names, ledger slugs, agent text — and
                // is exactly what the journal line below carries and a payload must
                // not.
                failure: outcome
                    .as_ref()
                    .err()
                    .map(crate::analytics::FailureCode::of),
                duration_ms: started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                // From the host, not from the report: the report exists only on
                // the success path, so reading it here reported zero effects
                // and zero parked approvals for every failed cycle — including
                // one that executed an irreversible effect and *then* hit an
                // adapter error, which is the turn most worth counting.
                effects_executed: effects.executed,
                approvals_parked: effects.parked,
            });
        // Closed while the lock is still held, so the bracket cannot outlive the
        // critical section it describes.
        let error = outcome.as_ref().err().map(|err| err.to_string());
        if let Err(err) = self
            .rt
            .journal
            .record_cycle_finished(&cycle_id, error)
            .await
        {
            tracing::warn!(
                company = %self.rt.id,
                cycle = %cycle_id,
                %err,
                "could not journal a cycle finish; the boot sweep will settle it"
            );
        }
        drop(guard);
        outcome
    }

    async fn run_locked(
        &self,
        inputs: Vec<(Option<EventSeq>, CompanyEvent)>,
        cycle_id: String,
        run_id: Option<String>,
        effects: &mut EffectCounts,
    ) -> Result<CycleReport> {
        let company = self.rt.id.clone();

        // 2. Persist input — durable before any thinking.
        let mut persisted_seq = None;
        let mut event_seqs = Vec::with_capacity(inputs.len());
        // Issue #242: the attempt rows this cycle is about to run, moved
        // `Pending` → `Running` below and backstopped after the brain returns.
        let mut dispatched_runs: Vec<String> = Vec::new();
        let mut events: Vec<CompanyEvent> = Vec::with_capacity(inputs.len());
        for (journaled, event) in inputs {
            // Issue #983: an input the caller already appended keeps the seq it
            // was appended under. Everything below is keyed on `seq` and not on
            // who wrote it, so the two entry points diverge here and nowhere
            // else.
            let seq = match journaled {
                Some(seq) => seq,
                None => self.rt.events.append(&company, event.clone()).await?,
            };
            event_seqs.push(seq);
            persisted_seq = Some(seq);
            // Start the run here, not inside the brain: the serial lock is held,
            // the driving event's seq now exists, and every brain — harness,
            // hosted, echo — passes through this one place. A brain that ignores
            // `TaskDispatched` entirely still leaves a correctly-started row for
            // the backstop below to settle.
            if let CompanyEvent::TaskDispatched {
                run_id: Some(run_id),
                ..
            } = &event
            {
                match self.rt.runs().begin_run(&company, run_id, seq).await {
                    Ok(_) => dispatched_runs.push(run_id.clone()),
                    // Not fatal, and not silent. The row may be missing (its
                    // `create_run` failed at the choke point and the dispatch
                    // proceeded anyway) or already past `Pending` (a replayed
                    // event). Either way the work still runs — record-keeping
                    // does not fail the work it records — but the run is not
                    // tracked as this cycle's, so the backstop leaves it alone.
                    Err(err) => tracing::warn!(
                        company = %company,
                        run = %run_id,
                        error = %err,
                        "[runs] could not start an attempt row; the cycle runs untracked"
                    ),
                }
            }
            events.push(event);
        }

        // Issue #983: the caller's own run row, moved `Pending` → `Running`
        // here — inside the serial lock — because that is what makes the two
        // statuses mean anything. A row started at accept time would read
        // `Running` while it was in fact queued behind another turn, which is
        // precisely the wait an operator staring at a slow company needs to see.
        //
        // Deliberately **not** added to `dispatched_runs`: the terminality
        // backstop settles what it started as soon as this cycle ends, and the
        // chat turn's settle belongs to the task that also journals its replies,
        // which outlives this call. A cycle error still settles the row — the
        // caller sees the `Err` and settles it `Failed` — and a panic is the
        // boot reaper's job, exactly as it is for a dispatch.
        //
        // Best-effort and logged, on the same terms as the dispatch rows above:
        // record-keeping does not get to fail the work it records.
        if let Some(run_id) = run_id.as_deref()
            && let Some(seq) = event_seqs.first().copied()
            && let Err(err) = self.rt.runs().begin_run(&company, run_id, seq).await
        {
            tracing::warn!(
                company = %company,
                run = %run_id,
                error = %err,
                "[runs] could not start a turn row; the turn runs untracked"
            );
        }

        // 3. Load — the company record, and nothing else.
        //
        // Issue #1175: this step used to also read 32 recent traces and the
        // *entire* context index (`list(&company, "")` — no prefix, no limit, so
        // a full scan that grows with every turn the company has ever run) into
        // `CycleRequest`. No brain read either one. Both loads are gone; see the
        // note on [`CycleRequest`] for why the fields went with them. Traces are
        // still written below — only the read was dead.
        let record = self.rt.store.load(&company).await?;

        // Issue #1455: a console policy PUT/DELETE persisted the override but
        // must not reach the live native gate mid-turn — an in-flight turn
        // finishes under the snapshot it started with. The start of a cycle,
        // holding the serial lock with the freshly-loaded record in hand, is
        // that safe boundary: re-apply the effective policy (mode, always-ask
        // list, spend cap) so this turn's native effects evaluate against what
        // the console reports, even on a company that has not been rebuilt. The
        // deadline half is immediate already (the ops handler writes the TTL
        // right after save); re-applying it here costs nothing and keeps a
        // boot/rebuild-created runtime consistent. A test-injected gate carries
        // its own policy on purpose and is exempt.
        if !self.rt.gate_injected
            && let Some(record) = &record
        {
            self.rt
                .approval_gate
                .apply_effective_policy(record.effective_policy());
        }

        // Issue #1725: is this batch a bare pleasantry? Decided HERE — before
        // the injections below and before any brain — because both of those
        // are how "hi" turned into a full agentic turn on staging.
        //
        // Ordering, not tidiness: `inject_handed_task_awareness` appends a
        // briefing of the desk's open work — and, since #1890 C, of the work
        // this conversation raised that has finished — to the message text, so
        // a "hi" sent to a desk that is mid-task stops looking like "hi" one
        // statement later. Reading the events first is what makes the fast path
        // fire on exactly the messages it is for.
        let small_talk = record
            .as_ref()
            .and_then(|record| small_talk_result(record, &events));

        if small_talk.is_none() {
            // Issue #176 (handed-task awareness): when an operator message is
            // addressed to a desk/agent that already has open work handed to it,
            // fold a briefing of that work into the message the brain sees — so a
            // direct "what are you working on?" surfaces the handed task truthfully.
            // Brain-agnostic (both brains read `req.events`); mutates only the
            // in-memory copy handed to the brain, never the durable log persisted
            // above.
            if let Some(record) = &record
                // Cheap exit before touching either store: no operator message,
                // so no briefing has anywhere to land.
                //
                // Every operator message counts, addressed or not. `chat: None`
                // is not "unaddressed" — `chat_and_emit` routes it to the
                // General desk and every reader of the journal folds it there
                // (`is_general_chat`), so requiring `Some` silently withheld
                // both briefings from exactly the turns a bare REST or ACP
                // caller sends: "did that ship?" answered blind, in the one
                // conversation the console itself defaults to (codex on #1972).
                && events
                    .iter()
                    .any(|e| matches!(e, CompanyEvent::OperatorMessage { .. }))
            {
                // One read, three briefings (#1890 C, E). The board answers
                // "what are you working on?" and "did that ship?"; the journal
                // answers "what else is this channel about?".
                let cards = self.rt.tasks().list(&self.rt.id).await.unwrap_or_default();
                self.inject_handed_task_awareness(record, &mut events, &cards)
                    .await;
                // Issue #1890 E: and where else this channel is talking, so a
                // thread scoped to itself (#1890 A) is not also blind to its
                // own channel. Same in-memory-only terms as its siblings.
                self.inject_thread_index(record, &mut events, &cards).await;
            }
            // Issue #845: and when the operator asked for a workflow rather than a
            // one-off, tell the turn that the builder pass owns authoring it — so it
            // answers the substance instead of denying a capability that is being
            // exercised on this very message. Same terms as the injection above:
            // brain-agnostic, in-memory only, never journaled.
            Self::inject_workflow_builder_awareness(&mut events);
        }

        // Issue #390: `cycle_id` is now minted by `run` before the serial lock,
        // so the journal's bracket can cover the wait on that lock. Nothing
        // above this point ever read it — see the note on `run`.
        //
        // Issue #364: the report carries the input seqs too, so the chat route
        // can tell the console the durable id of the message it just sent. The
        // brain needs the same list, and it is the append loop above — the one
        // place that knows it — that produced it.
        let input_seqs = event_seqs.clone();
        let request = CycleRequest {
            cycle_id: cycle_id.clone(),
            company_id: company.clone(),
            events,
            event_seqs,
            // The same snapshot the native gate was re-applied from above: the
            // harness rebuilds its roster against it, so a console override
            // that lands mid-turn (between this load and the harness's own
            // refresh) reaches neither gate until the next cycle boundary.
            //
            // A test-injected gate carries its own policy on purpose — the
            // reason the re-apply above is exempted — so the roster must pin
            // THAT policy, not the persisted record's effective one, or the
            // harness gate and the native gate would disagree about which tier
            // is live (issue #1455).
            policy: if self.rt.gate_injected {
                Some(self.rt.approval_gate.policy())
            } else {
                record.as_ref().map(|record| record.effective_policy())
            },
        };

        // 4. Think + 5. Gate — the host services callbacks and gates effects.
        // The card this cycle is working (issue #351) is read off the trigger
        // events before `request` is handed to the brain, and is a different
        // granularity from #242's `run_id`: which *card* an effect belongs to,
        // not which attempt at it. Both ride the same cycle.
        let host = CycleHostImpl::new(
            company.clone(),
            cycle_id.clone(),
            self.rt,
            // Per-id lookups, not a snapshot: the origins map is unbounded and
            // never pruned, and a cycle needs the link for at most the couple of
            // `ApprovalResolved` ids in its own batch.
            cycle_task_id(&request.events, |id| self.rt.journal.approval_task(id)),
            cycle_is_external(&request.events),
            // Issue #379: and which conversation, on the same terms — read off
            // the same trigger events, from the same retained origins. Issue
            // #435 widened this to the channel *and* the thread within it, in
            // one pass, so the pair always describes a single message.
            cycle_conversation(&request.events, &request.event_seqs, |id| {
                self.rt.journal.approval_conversation(id)
            }),
        );
        // Issue #1725: a bare pleasantry answers from here and the brain is
        // never called. Everything below — metering, response routing, the
        // terminality backstop — runs exactly as it does for a real turn, so
        // the reply is journaled, delivered and settled by one code path; what
        // is skipped is the thinking, which is the whole of the bug.
        let result = match small_talk {
            Some(result) => {
                tracing::debug!(
                    company = %company,
                    "[small-talk] answered a bare pleasantry without a turn"
                );
                Ok(result)
            }
            None => self.rt.brain.run_cycle(request, &host).await,
        };
        // Issue #242: the terminality backstop. Whatever the brain did — settled
        // the run richly (the harness path), ignored `TaskDispatched` entirely
        // (the echo brain), or errored out — no attempt row may be left claiming
        // to be live once the cycle that owned it is over. Runs deliberately
        // BEFORE the `?` so a brain error settles its rows too; the only path
        // that escapes it is a panic, which is the boot reaper's job.
        self.backstop_dispatched_runs(&company, &dispatched_runs, result.as_ref().err())
            .await;
        // Before the `?`, and before the fallible persistence below, for the
        // same reason the backstop is: an effect that executed and an approval
        // that parked are facts, and a later adapter error does not un-happen
        // them. Read here, the turn event reports them whichever way the cycle
        // ends (issue #1739).
        *effects = host.counts();
        let result = result?;

        // 6. Persist output.
        for trace in &result.new_traces {
            self.rt.memory.save_trace(&company, trace.clone()).await?;
        }
        for delta in &result.ledger_deltas {
            self.rt.store.append_ledger(&company, delta.clone()).await?;
        }
        // 6b. Meter what the cycle's thinking cost. This is the *generic* seam, so
        // every brain that reports usage is metered — before issue #174 only the
        // openhuman harness metered (per turn, through its own hook) and the
        // hosted/sidecar paths dropped `CycleResult.token_usage` on the floor,
        // leaving the Usage view at a blind zero. A brain that meters itself
        // reports zero here (see `HarnessBrain`), so nothing is counted twice.
        self.record_cycle_usage(&company, &result.token_usage).await;
        for response in &result.channel_responses {
            self.route_response(response).await?;
        }

        // 6c. Issue #243: journal every grant this cycle's turns redeemed.
        //
        // Redemption happens inside `ToolPolicy::check`, which is sync and holds
        // no journal handle, so the id is buffered on the grant set and written
        // here — after the cycle it belongs to. Best-effort and logged rather
        // than propagated: the tool has already run by this point, so failing the
        // cycle over the bookkeeping write would discard real model output to
        // record something whose only consequence is that a restart might re-arm
        // a spent grant (which then re-asks the operator — the safe direction).
        //
        // Issue #351: this is also where an operator-approved *tool call* gets
        // described. It never passes through `execute_effect_once` — approving
        // it mints a grant, and the tool runs inside the agent's next turn — so
        // without a description here an approved `composio_execute` payment
        // would reach no retry dialog at all.
        for id in self.rt.grants.drain_consumed() {
            let executed = self.consumed_grant_effect(&id);
            if let Err(err) = self.rt.journal.record_grant_consumed(&id, executed).await {
                tracing::warn!(
                    approval_id = %id,
                    error = %err,
                    "[approval] a grant was redeemed but its journal record failed; \
                     a restart before it is re-written may re-arm it, and the call \
                     it admitted will not be named on a retry confirmation"
                );
            }
        }
        for id in self.rt.grants.drain_consumed_continuations() {
            if let Err(err) = self
                .rt
                .journal
                .record_approval_continuation_consumed(&id)
                .await
            {
                tracing::warn!(
                    approval_id = %id,
                    error = %err,
                    "[approval] an explicit decision continuation completed but its journal \
                     record failed; a restart may repeat the follow-up turn"
                );
            }
        }

        let (executed_effects, parked) = host.into_outcomes();
        Ok(CycleReport {
            cycle_id,
            responses: result.channel_responses,
            executed_effects,
            parked,
            persisted_seq,
            input_seqs,
        })
    }

    /// Settles any attempt row this cycle started that is *still* claiming to be
    /// live (issue #242) — the terminality backstop.
    ///
    /// On the ordinary harness path this is a no-op: `run_task` settles the run
    /// richly (status, cost, step count, failure reason) and returns before
    /// `run_locked` gets here, so every row is already terminal or parked and
    /// [`RunStatus::is_active`] is false. The backstop exists for the paths that
    /// produce no rich settle at all:
    ///
    /// * a brain that ignores `TaskDispatched` (the default build's `EchoBrain`,
    ///   an injected test brain) — the row would otherwise sit `Running` until
    ///   the next boot reaped it;
    /// * a brain that **errored**, which is why this runs before the `?`.
    ///
    /// Best-effort per row, never propagated: the cycle either produced output
    /// the operator can already see or failed for a reason worth surfacing, and
    /// neither should be replaced by a bookkeeping error.
    ///
    /// A panic still escapes it — that is deliberately the boot reaper's job
    /// ([`reap_orphaned_runs`](crate::ports::runs::reap_orphaned_runs)), since a
    /// panicking cycle cannot run its own cleanup by definition.
    async fn backstop_dispatched_runs(
        &self,
        company: &CompanyId,
        run_ids: &[String],
        cycle_error: Option<&OpenCompanyError>,
    ) {
        for id in run_ids {
            let run = match self.rt.runs().get_run(company, id).await {
                Ok(Some(run)) => run,
                // Vanished between `begin_run` and here — nothing to settle.
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(
                        company = %company,
                        run = %id,
                        error = %err,
                        "[runs] could not read an attempt row for the terminality backstop"
                    );
                    continue;
                }
            };
            if !run.is_active() {
                // The rich settle already happened (or the run parked). Leaving
                // a parked run alone is the point: `Paused` / `WaitingApproval`
                // are waiting on something outside the cycle, not stranded by it.
                continue;
            }
            // Two readings of the same failure, because they go to two places
            // with different audiences (CodeRabbit review on #1905).
            //
            // `reason` is the full one: it lands on the attempt row and the
            // card note, both of which are already scoped to whoever can see
            // the card, and an operator debugging a stranded dispatch needs the
            // provider's actual words.
            //
            // `notice_reason` is what a **company-wide** notification title may
            // carry, and a free-form `err` is not it — `notify_dispatch_failed`
            // only flattens newlines, so a provider body quoting a key, a URL
            // or a customer's name would be broadcast to every member. The
            // cap-free arm is a fixed constant, so it passes through whole and
            // the badge still says what happened.
            let (reason, notice_reason) = match cycle_error {
                Some(err) => (
                    format!("{RUN_CYCLE_FAILED_ERROR}: {err}"),
                    RUN_CYCLE_FAILED_ERROR,
                ),
                None => (RUN_UNSETTLED_ERROR.to_string(), RUN_UNSETTLED_ERROR),
            };
            let outcome = RunOutcome::new(RunStatus::Failed).with_error(reason.clone());
            if let Err(err) = self.rt.runs().finish_run(company, id, outcome).await {
                tracing::warn!(
                    company = %company,
                    run = %id,
                    error = %err,
                    "[runs] the terminality backstop could not settle an attempt row"
                );
                // The row is still active, so the card is still truthfully
                // "being worked". Moving it now would claim an outcome the run
                // history does not record.
                continue;
            }
            // Issue #337: the card, too. Settling the row without moving the
            // card is exactly the stranding this backstop exists to prevent,
            // one level up — a brain that ignores `TaskDispatched`, or one that
            // errored, leaves a card sitting in In Progress that nothing will
            // ever re-drive, because `task_enters_in_progress` fires on the
            // *transition* into that column and that already happened.
            //
            // The reason goes onto the note so the board says why, and the move
            // is guarded: a card an operator has since dragged, or that a later
            // attempt parked, is left exactly where it is.
            // Issue #983: a card-less run has no card to strand, so there is
            // nothing here to make truthful. Settling the row above was the
            // whole of this run's cleanup.
            let Some(task_id) = run.task_id.as_deref() else {
                continue;
            };
            match crate::runtime::advance::advance_settled_card(
                self.rt.tasks().as_ref(),
                company,
                task_id,
                RunStatus::Failed,
                &reason,
            )
            .await
            {
                Ok(Some(column)) => {
                    tracing::info!(
                        company = %company,
                        run = %id,
                        task = %task_id,
                        column,
                        "[runs] the terminality backstop returned a stranded card"
                    );
                    // Issue #1865: the common shape of "board dispatch failed"
                    // — a brain that never answered `TaskDispatched`, or one
                    // whose cycle errored, left silent until this backstop
                    // caught it. See `CompanyRuntime::notify_dispatch_failed`.
                    // `notice_reason`, not `reason`: the title is company-wide
                    // and must not carry a free-form provider error. When there
                    // is no cycle error the two are the same constant, which is
                    // what #1883's test asserts reaches the title.
                    if column == crate::ports::tasks::COLUMN_TODO {
                        self.rt.notify_dispatch_failed(task_id, notice_reason).await;
                    }
                }
                Ok(None) => {}
                // Best-effort, like every other write here: the attempt row is
                // already settled and the cycle's own outcome must not be
                // replaced by a board-write fault.
                Err(err) => tracing::warn!(
                    company = %company,
                    run = %id,
                    task = %task_id,
                    error = %err,
                    "[runs] the terminality backstop settled an attempt but could not move its card"
                ),
            }
        }
    }

    /// Meters a finished cycle's inference usage onto the Usage + Finances
    /// surfaces, attributed to the brain's own provider slug (issue #174).
    ///
    /// A zero-usage cycle writes nothing, which covers the idle cycle, the
    /// offline echo brain, and the openhuman harness — the harness meters each
    /// turn as it runs and deliberately reports zero here, so its spend is never
    /// double-counted. Both non-`PerCycle` declarations are also enforced
    /// directly, so a path that reports usage against its own contract is warned
    /// about and dropped rather than trusted.
    ///
    /// Accounting never fails the cycle it accounts for: the write is
    /// logged-and-swallowed inside
    /// [`record_inference_usage`](crate::metering::record_inference_usage), so a
    /// meter fault cannot undo model output the operator can already see.
    async fn record_cycle_usage(&self, company: &CompanyId, usage: &TokenUsage) {
        if usage.is_zero() {
            return;
        }
        let cognition = self.rt.brain.cognition();
        // Both non-`PerCycle` arms declare "do not meter me here", so both are
        // enforced. Leaving `None` to fall through would have metered a brain
        // that runs no model at all under its own slug — the echo brain would
        // post a `provider: "none"` row into `byProvider`.
        match cognition.metering {
            UsageMetering::PerTurn => {
                // Defensive: a self-metering path should report zero. If one ever
                // reports usage too, drop it here rather than charge it twice, and
                // say so loudly.
                tracing::warn!(
                    company = %company,
                    path = %cognition.path,
                    input = usage.input,
                    output = usage.output,
                    "[usage] a per-turn-metered brain also reported cycle usage; ignoring it to avoid double-counting"
                );
                return;
            }
            UsageMetering::None => {
                tracing::warn!(
                    company = %company,
                    path = %cognition.path,
                    input = usage.input,
                    output = usage.output,
                    "[usage] a brain that declares it runs no model reported cycle usage; ignoring it — \
                     the path's Cognition::metering is wrong, or it grew a real model call"
                );
                return;
            }
            UsageMetering::PerCycle => {}
        }
        crate::metering::record_inference_usage(
            usage,
            crate::metering::UNATTRIBUTED_AGENT,
            cognition.provider,
            cognition.model,
            company,
            self.rt.store.as_ref(),
            self.rt.usage().as_ref(),
        )
        .await;
    }

    /// Folds a briefing of open handed work into any operator message addressed
    /// to a desk/agent that owns it (issue #176, handed-task awareness). Reads
    /// open task cards once and appends, to each addressed `OperatorMessage`,
    /// the cards whose assignee resolves to the addressed target — so a direct
    /// "what are you working on?" is answered truthfully. A no-op when nothing
    /// is addressed or no open work matches. Mutates only the in-memory events
    /// handed to the brain, never the durable event log.
    ///
    /// # And a briefing of work this conversation raised that has finished
    ///
    /// Issue #1890 C. Two briefings, one card read, two different axes: the one
    /// above matches on **who the work was handed to** and answers "what are you
    /// working on?"; this one matches on **which conversation raised it** and
    /// answers "did that ship?". A card can appear in either, both, or neither.
    ///
    /// It exists because the settle marker `chat_history::owns` files back into
    /// the conversation reaches the *operator* and not the model: the chat seed
    /// drops it for want of a conversational body, correctly, since a settle is
    /// not a turn. See [`SETTLED_WORK_ANNOTATION`] for why a briefing rather
    /// than a seed line, and why it reports the board rather than the marker.
    ///
    /// What it appends begins with [`OPEN_WORK_ANNOTATION`] or
    /// [`SETTLED_WORK_ANNOTATION`] — read those constants before adding any code
    /// downstream that reasons about an operator message, because after this
    /// runs the text is no longer only what the operator typed.
    async fn inject_handed_task_awareness(
        &self,
        record: &CompanyRecord,
        events: &mut [CompanyEvent],
        // Read once by the caller and shared with the thread index (#1890 E),
        // which needs the same cards to say where a thread's work landed. Two
        // `list` calls to answer related questions about one company is the
        // cost the caller's cheap exit exists to avoid.
        cards: &[TaskRecord],
    ) {
        let open: Vec<&TaskRecord> = cards
            .iter()
            .filter(|c| c.column != "done" && !c.assignee.trim().is_empty())
            .collect();
        // Issue #1890 C: the settled half, off the SAME read. Both briefings
        // answer a question about this company's cards, and paying for two
        // `list` calls to answer them separately would be the cost the cheap
        // exit above exists to avoid.
        let settled: Vec<&TaskRecord> = cards.iter().filter(|c| has_settled(c)).collect();
        if open.is_empty() && settled.is_empty() {
            return;
        }
        for event in events.iter_mut() {
            let CompanyEvent::OperatorMessage {
                text, chat, parent, ..
            } = event
            else {
                continue;
            };
            // Both spellings of the addressed desk, resolved from the record
            // already in hand. `None` resolves to General, which is where the
            // route sent it.
            let (desk_id, desk_name) = chat_history::desk_aliases(record, chat.as_deref());
            let target = desk_id.clone();
            // Bound before the borrow of `text` below, since both briefings
            // append to it.
            let thread = *parent;
            let mut lines: Vec<String> = Vec::new();
            for (idx, c) in open
                .iter()
                .filter(|c| assignment_matches(record, target.as_str(), &c.assignee))
                .enumerate()
            {
                // The latest attempt's ordinal + status, when one has run.
                // `list_runs` orders newest-first, so the first row is the
                // latest attempt. A card nobody has attempted yet omits the
                // clause; a run-history read failure marks it unavailable
                // rather than looking indistinguishable from no attempt.
                //
                // Bounded to `HANDED_TASK_ATTEMPT_LOOKUP_CAP` lookups: past
                // the cap a card renders with no attempt clause, same as one
                // nobody has attempted, rather than paying another guarded
                // round trip.
                let attempt_clause = if idx < HANDED_TASK_ATTEMPT_LOOKUP_CAP {
                    match self
                        .rt
                        .runs()
                        .list_runs(
                            &self.rt.id,
                            &RunFilter::for_task(c.id.as_str()).with_limit(1),
                        )
                        .await
                    {
                        Ok(runs) => match runs.first() {
                            Some(run) => {
                                format!(" · attempt {} {}", run.attempt, run.status.as_str())
                            }
                            None => String::new(),
                        },
                        Err(_) => " · attempt status unavailable".to_string(),
                    }
                } else {
                    String::new()
                };
                let column = column_label(&c.column);
                lines.push(match &c.note {
                    Some(note) if !note.trim().is_empty() => format!(
                        "- {} [{column}{attempt_clause}] — {}",
                        c.title,
                        first_line(note, 120)
                    ),
                    _ => format!("- {} [{column}{attempt_clause}]", c.title),
                });
            }
            if !lines.is_empty() {
                lines.sort();
                text.push_str(&format!(
                    "{OPEN_WORK_ANNOTATION} (answer truthfully if asked what you are \
working on):\n{}\n]",
                    lines.join("\n")
                ));
            }
            // Issue #1890 C. Matched on the **conversation the card was raised
            // in**, not on who it was handed to — the question this answers is
            // "did the thing I asked for here ship?", and the answer is the
            // same whoever ran it. That is a different axis from the briefing
            // above, which is why this is a second pass rather than a wider
            // filter on the first.
            //
            // Both halves of the origin, since #1890 B: the channel through
            // `same_conversation` (which folds General's four spellings), and
            // the thread verbatim.
            //
            // **Both desk spellings**, like `chat_history::owns`. This filter
            // originally compared the addressed selector verbatim, on the
            // argument that both sides are the raw chat id stamped from this
            // same field — which holds only while every caller spells the desk
            // the same way. They do not: a card raised by a client addressing
            // the desk by id, and a later "did that ship?" addressing it by
            // name, are the same conversation and compared unequal, so the
            // briefing went missing exactly when the operator was asking for it
            // (codex on #1972).
            let mut done: Vec<&&TaskRecord> = settled
                .iter()
                .filter(|c| {
                    // A recorded desk is required before any of this compares.
                    // `same_conversation(None, "General")` is `true` — `None`
                    // is one of General's four spellings *for a message* — but
                    // a card with no origin was raised by no conversation at
                    // all, and reading its absence as "General" briefs
                    // board-only work into an unaddressed turn as work "raised
                    // in this conversation". `chat_history::owns` already draws
                    // that line for the terminal (`a_terminal_with_no_origin_
                    // belongs_to_nobody_not_to_general`); this now draws the
                    // same one (coderabbit on #1982).
                    let Some(origin) = c.origin_chat_id() else {
                        return false;
                    };
                    (chat_history::same_conversation(Some(origin), Some(desk_id.as_str()))
                        || chat_history::same_conversation(Some(origin), Some(desk_name.as_str())))
                        && c.origin_parent() == thread
                })
                .collect();
            if done.is_empty() {
                continue;
            }
            // Most recent first: "did that ship?" is nearly always about the
            // latest thing, and the cap below cuts the tail.
            done.sort_by_key(|c| std::cmp::Reverse(c.updated_at_millis));
            let omitted = done.len().saturating_sub(SETTLED_WORK_BRIEFING_MAX);
            let lines: Vec<String> = done
                .iter()
                .take(SETTLED_WORK_BRIEFING_MAX)
                .map(|c| settled_briefing_line(c))
                .collect();
            // The truncation is DECLARED, never silent. A model handed 5 of 28
            // with no marker answers "that is everything" confidently and
            // wrongly — the same rule the epic sets for its thread index.
            let tail = if omitted > 0 {
                format!("\n- (and {omitted} more, not listed)")
            } else {
                String::new()
            };
            text.push_str(&format!(
                "{SETTLED_WORK_ANNOTATION} has finished — this is where each card \
stands now, which may differ from the marker in the transcript):\n{}{tail}\n]",
                lines.join("\n")
            ));
        }
    }

    /// Folds an index of the channel's other live threads into each addressed
    /// operator message (issue #1890 E). See [`THREAD_INDEX_ANNOTATION`].
    ///
    /// Separate from [`inject_handed_task_awareness`](Self::inject_handed_task_awareness)
    /// because it reads a different store — the journal rather than the board —
    /// and must be skippable on a host with no event log wired, which the board
    /// briefings are not.
    ///
    /// `settled` is passed in rather than re-read: the caller has just listed
    /// the cards, and a second `list` to answer a related question about the
    /// same company is the cost that function's cheap exit exists to avoid.
    async fn inject_thread_index(
        &self,
        record: &CompanyRecord,
        events: &mut [CompanyEvent],
        cards: &[TaskRecord],
    ) {
        let settled: Vec<&TaskRecord> = cards.iter().filter(|c| has_settled(c)).collect();
        let log = self.rt.events();
        for event in events.iter_mut() {
            let CompanyEvent::OperatorMessage {
                text, chat, parent, ..
            } = event
            else {
                continue;
            };
            let current = *parent;
            // Both spellings, resolved the way the seed resolves them.
            //
            // Passing the addressed id as both terms looked harmless and was
            // not: a named desk's id and its display name are different
            // strings, messages are journaled under either, and `owns` takes
            // two terms precisely so neither is orphaned. With one, every
            // thread stored under the other alias vanished from the index — so
            // a desk whose name differs from its id got a short index or none
            // at all (codex + coderabbit on #1972).
            //
            // From the record the caller already holds rather than a `load` per
            // message: same answer, no store round-trip, and `None` resolves to
            // the General desk the route sent it to.
            let (desk_id, desk_name) = chat_history::desk_aliases(record, chat.as_deref());
            let page = match log.read_before(&self.rt.id, None, THREAD_INDEX_PAGE).await {
                Ok(page) => page,
                // A read failure costs the turn its orientation and nothing
                // else. The same posture `build_chat_seed` takes: a briefing is
                // an enhancement, and failing the turn over one would be worse
                // than answering without it.
                Err(error) => {
                    tracing::warn!(
                        company = %self.rt.id,
                        %error,
                        "[thread-index] journal read failed; the turn answers without orientation"
                    );
                    return;
                }
            };
            let (lines, omitted) =
                thread_index(&page, &desk_id, &desk_name, current, text, &settled);
            if lines.is_empty() {
                continue;
            }
            // The truncation is DECLARED. A selection presented as an
            // enumeration is answered from confidently and wrongly.
            let tail = if omitted > 0 {
                format!("\n- (and {omitted} older, not listed)")
            } else {
                String::new()
            };
            // **The instruction is half the mechanism.** Without the gate an
            // agent reads every thread it is shown "to be safe", which rebuilds
            // the flat channel window this epic removed — in the prompt, and
            // paid for twice. With it, the index is a pointer: enough to notice
            // a reference, never enough to answer from.
            text.push_str(&format!(
                "{THREAD_INDEX_ANNOTATION}, for reference only — do NOT read or \
answer from them unless this message explicitly refers to one, and if a \
reference could mean more than one, ask which):\n{}{tail}\n]",
                lines
                    .iter()
                    .map(ThreadLine::render)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    /// Folds the builder-pass briefing into any `workflow`-deliverable operator
    /// message (issue #845). See [`BUILDER_ANNOTATION`] for why.
    ///
    /// Deliberately not `async` and touching no store: unlike the handed-work
    /// briefing, everything this needs is already on the event. Mutates only the
    /// in-memory events handed to the brain, never the durable event log.
    ///
    /// Applies to every `workflow` message, addressed or not. The refusal this
    /// prevents came from a desk agent in a channel, but an unaddressed message
    /// reaches the orchestrator — which *does* hold `create_workflow` — and
    /// telling it that a builder pass already owns this card is what stops it
    /// authoring a second graph beside the proposal.
    fn inject_workflow_builder_awareness(events: &mut [CompanyEvent]) {
        for event in events.iter_mut() {
            let CompanyEvent::OperatorMessage {
                text,
                deliverable: Some(MessageIntent::Workflow),
                ..
            } = event
            else {
                continue;
            };
            text.push_str(&format!(
                "{BUILDER_ANNOTATION}: the operator asked for a reusable workflow, not a \
one-off, so a card for it has been opened and the workflow builder owns authoring the graph. \
Do NOT try to create, save or schedule a workflow yourself, and do not report that you cannot \
— the build is already under way and its proposal goes to the operator for review. Answer the \
substance of what they asked, and say that the workflow itself is being drafted for their \
approval.]"
            ));
        }
    }

    /// Settles a parked approval's verdict — the **fast half** of resolving one.
    ///
    /// Records the outcome on the gate, journals it durably, and settles the
    /// approved effect (minting the single-use grant, or executing a native
    /// effect). Everything here is local bookkeeping and a couple of appends; no
    /// model is called. What it deliberately does *not* do is run the follow-up
    /// cycle — that is [`ResolveReceipt::Settled`]'s event, handed back for the
    /// caller to run separately.
    ///
    /// The split exists because the two halves have wildly different durations
    /// and wildly different consequences if they are lost (issue #383). The
    /// settle is milliseconds and, once it returns, the operator's decision is
    /// permanent. The follow-up is a full agent turn and can outlast any proxy
    /// in front of the host. Fusing them meant the HTTP status reported the
    /// *turn's* fate as though it were the *verdict's*, so a slow turn behind
    /// nginx read as "couldn't record your decision" over a decision that was
    /// already journaled and already granted (issue #380, defect 1). Worse,
    /// because the whole thing lived in the request future, the dropped
    /// connection took the continuation with it — grant spent, agent never
    /// re-dispatched (defect 3).
    ///
    /// Resolving an approval that is **not parked** — an unknown id, or one a
    /// concurrent request already resolved — is a no-op that yields
    /// [`ResolveReceipt::AlreadyResolved`] (issue #243). It writes no journal
    /// record and owes no cycle.
    ///
    /// Resolving one that is **past its deadline** yields
    /// [`ResolveReceipt::Expired`] and likewise writes nothing here — the
    /// operator's click arrived too late to be a decision, so nothing about it
    /// may be journaled (issue #1449). **The caller owes that approval its
    /// retirement**: `Expired` means the gate has already dropped the entry, and
    /// [`CompanyRuntime::retire_approval`](crate::runtime::CompanyRuntime) is
    /// the one transaction that finishes the job. `resolve_approval_spawned` —
    /// the only production caller — does exactly that.
    ///
    /// Before this the double-submit path was indistinguishable from a deny (see
    /// [`ResolveOutcome`]), so a double-clicked approve appended a second
    /// `ApprovalResolved` to the journal and ran a second follow-up cycle over an
    /// approval that no longer existed — burning a model turn to tell the brain
    /// about a resolution it had already been told about.
    pub async fn settle_approval(
        &self,
        id: &ApprovalId,
        verdict: Verdict,
        by: Actor,
        scope: GrantScope,
    ) -> Result<ResolveReceipt> {
        // Every caller of this already asked `ensure_accepting` before it, but
        // that ask sits behind at least one `.await` (the blocker claim lock,
        // arming a console blocker resolution) before this runs. Rechecked here
        // — first, before anything below commits — so a stop that lands in that
        // window still catches the settlement rather than letting it execute a
        // native effect or mint a grant after the company reports itself
        // stopped. Nothing has touched the gate or the journal yet, so a
        // refusal here leaves the approval exactly as parked as it was.
        self.rt.ensure_not_emergency_stopped()?;
        // Issue #374: a broader scope is validated BEFORE the gate is touched.
        //
        // The order is the whole safety story of a bad scope request. Validating
        // after `resolve_outcome` would have already dropped the approval from
        // the parked queue and journaled a verdict, so a request naming an
        // ungrantable tool would leave the operator with no card to re-decide
        // and a resolution they never got the effect of. Checked first, a bad
        // request changes nothing at all: the approval stays parked, no verdict
        // is journaled, and the operator can simply approve it "once" instead.
        if let GrantScope::Tool { .. } = scope {
            self.check_broadly_scoped(id, verdict)?;
        }
        if self
            .rt
            .approval_gate
            .parked_effect(id)
            .is_some_and(|effect| {
                effect.kind == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
                    && effect.agent.is_none()
            })
        {
            return Err(OpenCompanyError::InvalidRequest(
                "the explicit approval request is missing its requesting agent and cannot be \
                 resumed; the card remains pending"
                    .to_string(),
            ));
        }
        let outcome = self
            .rt
            .approval_gate
            .resolve_outcome(id, verdict, by.clone(), now_millis());
        if outcome == ResolveOutcome::NotParked {
            return Ok(ResolveReceipt::AlreadyResolved);
        }
        // Issue #1449: past its deadline is NOT this operator's verdict.
        //
        // The gate has already default-denied it — that is what `Expired`
        // means, and the safety half has always held: no grant is minted and
        // nothing runs. What did not exist was the *reporting* half. Falling
        // through here appended `ApprovalResolved` — the record for "the
        // operator decided this" — and returned `Settled { verdict: Approve }`,
        // so the durable audit trail said a named person approved something the
        // host had refused, and the console said so in green.
        //
        // Returning early leaves the retirement to the caller rather than doing
        // it here, because the transaction an expiry owes is four steps, not
        // one — journal, pending mark, continuation release, event — and it
        // already exists whole as `CompanyRuntime::retire_approval`, which the
        // sweeper reaches the identical outcome through. Re-implementing three
        // of its four steps at this seam is exactly the failure that function's
        // doc comment exists to prevent, and it needs an `Arc<Self>` to release
        // the continuation, which a `CycleRunner` does not hold.
        if outcome == ResolveOutcome::Expired {
            return Ok(ResolveReceipt::Expired);
        }
        // Issue #796: the approval has left the parked set — drop its pending
        // mark. On approve the grant minted just below now names the task; on
        // deny nothing does, so its held checkout becomes sweepable.
        self.rt.grants.clear_pending(id);
        self.rt.journal.record_resolved(id).await?;
        match outcome {
            ResolveOutcome::Approved(effect)
                if effect.kind == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND =>
            {
                let agent = effect
                    .agent
                    .clone()
                    .expect("explicit request agent validated before resolution");
                self.mint_approval_continuation(id, agent, effect, Verdict::Approve, by.clone())
                    .await?;
            }
            ResolveOutcome::Approved(effect) => {
                self.settle_approved_effect(id, effect, by.clone(), scope)
                    .await?;
            }
            ResolveOutcome::Denied(effect)
                if effect.kind == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND =>
            {
                let agent = effect
                    .agent
                    .clone()
                    .expect("explicit request agent validated before resolution");
                self.mint_approval_continuation(id, agent, effect, Verdict::Deny, by.clone())
                    .await?;
            }
            // Issue #1458: a standing denial is minted from the effect the
            // resolve carried, not `journal.approval_effect` — the journal keeps
            // a payload-scrubbed copy (issue #351), and `standing_scope_of` read
            // against a scrubbed payload answers `None`, which `admits_scope`
            // treats as a wildcard. A refusal prompted by one web origin would
            // then block every origin for that teammate.
            ResolveOutcome::Denied(effect) if matches!(scope, GrantScope::Tool { .. }) => {
                self.mint_standing_deny(id, effect, by.clone(), scope)
                    .await?;
            }
            _ => {}
        }
        // Issue #1825: bank a blocked-node approval here, inline and durable,
        // rather than leaving it to `continue_turn` on the detached follow-up
        // task this function's caller (`resolve_approval_spawned`) is about to
        // spawn. A restart between that spawn and the task's first poll used to
        // leave the verdict durable above but this bank never run — see
        // `CompanyRuntime::bank_blocked_node_approval` for the full window this
        // closes.
        self.rt.bank_blocked_node_approval(id, verdict).await;
        // The follow-up event, so the brain learns the verdict. Returning it
        // (rather than appending it here) keeps the event logged exactly once:
        // the cycle that runs it is the thing that appends it.
        Ok(ResolveReceipt::Settled(Box::new(
            CompanyEvent::ApprovalResolved {
                approval_id: id.clone(),
                verdict,
                by,
            },
        )))
    }

    /// Applies an approved effect: **mint a grant** when it came from a harness
    /// tool call, **execute it** when it is native (issue #243).
    ///
    /// This is the fork the whole feature turns on, and it is decided by
    /// [`Effect::agent`], which only
    /// [`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for)
    /// ever stamps:
    ///
    /// * **`None` — native.** Unchanged, byte for byte: `execute_effect_once`
    ///   under the `approval:<id>` key. Emails, workflow deliveries and Medulla
    ///   effect frames keep their at-most-once path exactly as before.
    /// * **`Some(agent)` — a harness tool call.** Executing it would be
    ///   meaningless: the payload is a tool's *arguments*, and `perform_effect`
    ///   would ledger a phantom spend and route nothing. Worse, it would look
    ///   like success while the tool never ran. So the effect is deliberately
    ///   NOT executed; a single-use grant is minted instead, and the brain's
    ///   `ApprovalResolved` arm re-dispatches the agent to re-issue the call for
    ///   real.
    ///
    /// Both forks are described for the retry warning (issue #351), but at
    /// different moments, because "it ran" happens at different moments. A
    /// native effect is described by `execute_effect_once` as it commits. A tool
    /// call is described when its grant is **redeemed** — minting one only means
    /// the agent is now allowed to make the call, and describing it here would
    /// warn about a payment for a grant that then quietly expired unused. See
    /// [`consumed_grant_effect`](Self::consumed_grant_effect).
    ///
    /// The journal record is written **before** the grant enters the live set.
    /// A crash between the two therefore replays as "granted", re-arming it —
    /// the safe direction. The reverse order would lose the operator's approval
    /// entirely on a crash, and the agent would come back asking for a
    /// permission it had already been given.
    /// Refuses a broad-scope request the runtime must not honour (issue #374),
    /// **without touching the gate or the journal**.
    ///
    /// Two refusals, both read off the parked effect:
    ///
    /// * **native** (`agent: None`) — there is no tool and no agent to grant to.
    ///   The runtime performs these itself; "this tool, for this teammate" names
    ///   neither of the two things it needs.
    /// * **not broadly grantable** — the tool can reach further than a standing
    ///   grant can honestly describe (issue #444), so it is a decision the
    ///   operator has to take per call.
    ///
    /// The verdict is read off the **parked effect** rather than re-derived from
    /// a live tool call, which is both cheaper and more honest: the effect
    /// carries the tool name and the arguments the card showed the operator, so
    /// what they see is what is checked. It is also what lets this run in the
    /// default build, where the harness classifier does not compile.
    ///
    /// An unknown or already-resolved id falls through to the ordinary
    /// already-resolved path rather than erroring here — a double-click on the
    /// scoped button must stay the no-op it is on the plain one.
    fn check_broadly_scoped(&self, id: &ApprovalId, verdict: Verdict) -> Result<()> {
        let Some(effect) = self.rt.approval_gate.parked_effect(id) else {
            return Ok(());
        };
        if effect.kind == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND {
            return Err(OpenCompanyError::InvalidRequest(
                "an explicit approval question can only be decided once; a standing scope would \
                 govern the request_approval tool rather than the proposed action"
                    .to_string(),
            ));
        }
        // Issue #1098: a teammate, or the authored workflow a gate belongs to.
        // Neither means the runtime itself is performing this, and there is
        // genuinely nothing to hold a permission — the refusal below is the same
        // one it always was, now stated about a subject rather than an agent.
        if crate::runtime::grants::subject_of(&effect).is_none() {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "'{}' is performed by the runtime itself, so there is nobody's tool use to \
                 grant; approve it once instead",
                effect.kind
            )));
        }
        if verdict == Verdict::Approve && !effect.may_be_granted_standing() {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "'{}' cannot be granted for a period — it can reach further than a standing \
                 permission can describe, so it stays a per-call decision; approve it once instead",
                effect.kind
            )));
        }
        // Issue #1458: a standing DENY is only enforced on the agent turn path,
        // where openhuman treats a `Deny` verdict as fail-closed. The workflow
        // gate deliberately does not honour `Deny` (`src/workflows/gate.rs`), so
        // minting one for a workflow would advertise a refusal no run ever
        // enforces — the operator clicks "don't ask again" and the next
        // scheduled run sails through the gate. A workflow refusal stays a
        // per-call decision until the gate learns to enforce the verdict.
        if verdict == Verdict::Deny
            && matches!(
                crate::runtime::grants::subject_of(&effect),
                Some(GrantSubject::Workflow(_))
            )
        {
            // Name the real call a gate is stopping, not the `workflow.approve`
            // wrapper — the same inner call the card showed the operator.
            let call = crate::runtime::workflow_resume::gate_inner_call(&effect)
                .map(|(tool, _)| tool)
                .unwrap_or(&effect.kind);
            return Err(OpenCompanyError::InvalidRequest(format!(
                "'{call}' is a workflow call, and the workflow path does not enforce a \
                 standing refusal yet; deny it once instead",
            )));
        }
        Ok(())
    }

    async fn mint_standing_deny(
        &self,
        id: &ApprovalId,
        effect: Effect,
        by: Actor,
        scope: GrantScope,
    ) -> Result<()> {
        let GrantScope::Tool { expires_at_millis } = scope else {
            unreachable!()
        };
        let Some(subject) = crate::runtime::grants::subject_of(&effect) else {
            unreachable!()
        };
        self.mint_standing_policy(id, subject, effect, by, expires_at_millis, Verdict::Deny)
            .await
    }

    async fn settle_approved_effect(
        &self,
        id: &ApprovalId,
        effect: Effect,
        by: Actor,
        scope: GrantScope,
    ) -> Result<()> {
        // Issue #1863: a blocker's effect is INERT. It carries a question, not a
        // tool call — `park_blocker` stamps `agent: None` and mints no grant —
        // so approving one must re-enter the stopped step, never execute the
        // payload. Without this guard a resuming verdict (Retry/Amend/Skip, all
        // mapped to `Approve`) would fall through to the native `agent.is_none()`
        // arm below and hand the blocker payload to `execute_effect_once`, which
        // would ledger a phantom spend and route nothing while reading as
        // success. The answer is already armed on the grant set's blocker
        // side-channel by the resolve entrypoint, and `continue_turn`'s blocker
        // fork drives the actual resume; there is nothing to do here.
        if crate::ports::blockers::is_blocker_effect(&effect) {
            return Ok(());
        }
        // Issue #1098: a gate carries no teammate but can still hold a standing
        // permission for its workflow, so that case is taken before the native
        // fall-through below. Only for `GrantScope::Tool` — a `Once` approval of
        // a gate is still performed natively, because the single-use grant this
        // would otherwise mint has nobody to redeem it (see `crate::workflows::gate`).
        if effect.agent.is_none()
            && let GrantScope::Tool { expires_at_millis } = scope
            && let Some(subject @ GrantSubject::Workflow(_)) =
                crate::runtime::grants::subject_of(&effect)
        {
            return self
                .mint_standing_grant(id, subject, effect, by, expires_at_millis)
                .await;
        }
        let Some(agent) = effect.agent.clone() else {
            let key = format!("approval:{id}");
            // The card that asked for this sign-off (issue #351). It is not
            // this call's caller — an approval is resolved from the Approvals
            // page, which knows only an id — so it comes off the parked record,
            // which `record_resolved` deliberately does not erase.
            let task_id = self
                .rt
                .journal
                .approval_task(id)
                .flatten()
                .and_then(|task| task.task_id().map(str::to_string));
            return execute_effect_once(self.rt, &key, &effect, task_id.as_deref()).await;
        };
        match scope {
            GrantScope::Once => self.mint_grant(id, agent, effect).await,
            GrantScope::Tool { expires_at_millis } => {
                self.mint_standing_grant(
                    id,
                    GrantSubject::Agent(agent),
                    effect,
                    by,
                    expires_at_millis,
                )
                .await
            }
        }
    }

    /// Journals then arms a **standing** grant: this tool, for this teammate,
    /// until `expires_at_millis` (issue #374).
    ///
    /// Deliberately mints **only** the standing grant. Minting a single-use one
    /// alongside it would be redundant — the standing grant already admits the
    /// re-issued call — and worse than redundant: the single-use grant would go
    /// unredeemed, and fifteen minutes later the TTL sweep would tell the
    /// operator "the agent didn't act", about work that ran immediately.
    ///
    /// Same journal-before-live-set ordering, and the same crash direction, as
    /// [`mint_grant`](Self::mint_grant): a crash between the two replays as
    /// granted rather than losing the operator's decision.
    async fn mint_standing_grant(
        &self,
        id: &ApprovalId,
        subject: GrantSubject,
        effect: Effect,
        by: Actor,
        expires_at_millis: u64,
    ) -> Result<()> {
        self.mint_standing_policy(id, subject, effect, by, expires_at_millis, Verdict::Approve)
            .await
    }

    async fn mint_standing_policy(
        &self,
        id: &ApprovalId,
        subject: GrantSubject,
        effect: Effect,
        by: Actor,
        expires_at_millis: u64,
        verdict: Verdict,
    ) -> Result<()> {
        let conversation = self
            .rt
            .journal
            .approval_conversation(id)
            .unwrap_or_default();
        // Issue #1098: a gate's `kind` is the `workflow.approve` wrapper, so the
        // tool and arguments the permission is *about* are the inner call issue
        // #846 wrote onto the payload — the same call the card showed. Every
        // other effect is its own call and answers `None` here.
        let (tool, args) = crate::runtime::workflow_resume::gate_inner_call(&effect)
            .map(|(tool, args)| (tool.to_string(), args.clone()))
            .unwrap_or_else(|| (effect.kind.clone(), effect.payload.clone()));
        // Issue #457: which slice of the tool the card was actually about, read
        // off the **parked effect's own payload** — the arguments the operator
        // was shown — rather than re-derived from anything live, so the grant
        // records the sentence they consented to. `None` for every tool whose
        // name is the whole of what it can do.
        //
        // Computed here rather than inline in the literal below, which would
        // borrow `tool` after the field above has moved it.
        let scope = match crate::policy::consequence::standing_mint_scope(&tool, &args, verdict) {
            crate::policy::consequence::StandingMintScope::Scoped(scope) => Some(scope),
            crate::policy::consequence::StandingMintScope::Unscoped => None,
            crate::policy::consequence::StandingMintScope::Refused(why) => {
                return Err(OpenCompanyError::InvalidRequest(why));
            }
        };
        let (agent, workflow) = match &subject {
            GrantSubject::Agent(agent) => (agent.clone(), None),
            GrantSubject::Workflow(workflow) => (String::new(), Some(workflow.clone())),
        };
        let grant = StandingGrant {
            id: GrantId::generate(),
            agent,
            workflow,
            // The tool, and nothing about the arguments. A standing grant has no
            // `args` field to copy them into — that is the type's whole point.
            tool,
            verdict,
            granted_by: by.clone(),
            approval_id: id.clone(),
            at_millis: now_millis(),
            expires_at_millis,
            // Issue #379: where the operator asked, so the re-dispatched turn's
            // reply lands back in that conversation. Read off the retained
            // origin, exactly as `mint_grant` does. Issue #435 added the thread
            // within it; both come from one read so they cannot disagree.
            origin_thread: conversation.thread,
            origin_parent: conversation.parent,
            // Issue #796: the task this call was parked from, carried so a
            // standing grant can reclaim the task's checkout across parks.
            origin_task: self.approval_work_key(id),
            // Derived from the same `(tool, args)` the grant records, which for
            // a gate is the inner call rather than the wrapper — read with the
            // same function the live side uses, so the two cannot drift into a
            // permission that never matches its own call.
            scope,
        };
        // Issue #1458: newest standing decision wins. `ApprovalPolicy` checks
        // a standing denial above a standing grant, so an approval minted while
        // a denial of the same scope was still live would sit listed but never
        // admit a call — the operator's later "yes" silently inert until the
        // older refusal expired or was revoked. Revoke the shadowed
        // opposite-polarity policy before arming the new one, journaled as a
        // revocation by the same resolving actor, so replay reconstructs the
        // same single-policy state. Scoped to what would actually shadow (either
        // scope overlapping the other), so a denial of one host leaves a grant
        // for another alone while a wildcard policy supersedes scoped
        // opposite-polarity ones in both directions.
        //
        // The reconcile is not itself atomic: the snapshot below, the journal
        // appends, and the insert are separate steps, and the journal appends
        // are awaited. Two concurrent resolutions of the same scope with
        // opposite verdicts — an approve and a deny landing within a few
        // milliseconds from separate console surfaces — could both snapshot an
        // empty opposite set before either inserts, leaving the deny shadowing
        // the approve whatever the operator's true order. Holding the grant
        // set's reconcile lock for the whole sequence makes the second mint see
        // the first's policy and supersede it, which is the same single-policy
        // state the sequential path already reconstructs on replay.
        let _reconcile = self.rt.grants.standing_reconcile().await;
        for old in self.rt.grants.opposite_polarity(
            &subject,
            &grant.tool,
            grant.scope.as_deref(),
            verdict,
            now_millis(),
        ) {
            self.rt
                .journal
                .record_standing_revoked(&old.id, by.clone(), now_millis())
                .await?;
            self.rt.grants.revoke_standing(&old.id);
            tracing::debug!(
                grant_id = %old.id,
                tool = %old.tool,
                agent = %old.agent,
                "[approval] minting a {:?} supersedes the opposite-polarity \
                 standing policy for the same scope",
                verdict
            );
        }
        self.rt.journal.record_standing_granted(&grant).await?;
        tracing::debug!(
            approval_id = %id,
            grant_id = %grant.id,
            tool = %grant.tool,
            agent = %grant.agent,
            workflow = ?grant.workflow,
            expires_at_millis,
            "[approval] minted a standing grant; this tool will not ask again until it expires"
        );
        self.rt.grants.grant_standing(grant);
        Ok(())
    }

    /// Journals then arms a single-use grant for `(agent, effect.kind,
    /// effect.payload)`.
    async fn mint_grant(&self, id: &ApprovalId, agent: String, effect: Effect) -> Result<()> {
        let conversation = self
            .rt
            .journal
            .approval_conversation(id)
            .unwrap_or_default();
        let grant = GrantedCall {
            approval_id: id.clone(),
            agent,
            tool: effect.kind.clone(),
            // The parked effect's payload IS the tool's argument object — see
            // `effect_for`. Granting against it verbatim is what makes the
            // policy's match "the exact call the operator saw".
            args: effect.payload.clone(),
            at_millis: now_millis(),
            // Issue #379: where the operator asked, carried onto the grant so
            // the re-dispatched turn's reply lands back in that conversation.
            // Read off the retained origin, not this call's caller — an approval
            // is resolved from a surface that knows only an id. Issue #435 added
            // the thread within it; one read, so the pair cannot disagree.
            origin_thread: conversation.thread,
            origin_parent: conversation.parent,
            // Issue #796: the task this call was parked from, so the
            // re-dispatched turn can reclaim its held-across-park checkout.
            origin_task: self.approval_work_key(id),
        };
        self.rt.journal.record_granted(&grant).await?;
        self.rt.grants.grant(grant);
        tracing::debug!(
            approval_id = %id,
            tool = %effect.kind,
            "[approval] minted a single-use grant; the agent will re-issue the call"
        );
        Ok(())
    }

    /// Journals then arms a verdict-bearing continuation for an explicit
    /// `request_approval` call. It is intentionally disjoint from executable
    /// grants: both yes and no resume the conversation, and neither authorises
    /// a tool call by itself.
    pub(crate) async fn mint_approval_continuation(
        &self,
        id: &ApprovalId,
        agent: String,
        effect: Effect,
        verdict: Verdict,
        by: Actor,
    ) -> Result<()> {
        let conversation = self
            .rt
            .journal
            .approval_conversation(id)
            .unwrap_or_default();
        let continuation = ApprovalContinuation {
            call: GrantedCall {
                approval_id: id.clone(),
                agent,
                tool: effect.kind,
                args: effect.payload,
                at_millis: now_millis(),
                origin_thread: conversation.thread,
                origin_parent: conversation.parent,
                origin_task: self.approval_work_key(id),
            },
            verdict,
            by,
        };
        self.rt
            .journal
            .record_approval_continuation(&continuation)
            .await?;
        self.rt.grants.continue_approval(continuation);
        Ok(())
    }

    /// The work unit a parked approval belongs to, for stamping a grant's
    /// `origin_task` (issue #796).
    ///
    /// A task card names it directly. A DM/chat has no card, but its conversation
    /// is just as much a unit of work — the agent checks out, edits, commits and
    /// publishes across a batch of approvals raised in the same thread — so the
    /// thread stands in, sanitised to a single safe branch segment. The checkout
    /// retention and `repo_publish`'s `oc/<company>/<unit>` branch then key on one
    /// value for both, and the whole task-scoped machinery covers a DM unchanged.
    ///
    /// `None` only when there is neither a card nor a usable thread.
    fn approval_work_key(&self, id: &ApprovalId) -> Option<String> {
        if let Some(task) = self
            .rt
            .journal
            .approval_task(id)
            .flatten()
            .and_then(|task| task.task_id().map(str::to_string))
        {
            return Some(task);
        }
        self.rt
            .journal
            .approval_conversation(id)
            .and_then(|c| c.thread)
            .and_then(|thread| sanitize_work_segment(&thread))
    }

    /// Describes a grant the agent just redeemed, so an operator-approved tool
    /// call is named on the retry confirmation like a native effect is
    /// (issue #351).
    ///
    /// The three facts all come off records the journal already keeps, joined on
    /// the [`ApprovalId`] the redemption reports:
    ///
    /// * **what it was** — the effect the approval was parked with (or the
    ///   amended one, which is what the grant was minted against). Read back
    ///   rather than re-projected from the grant's tool name and arguments, so
    ///   there is one projection and the operator is told about the call they
    ///   actually saw;
    /// * **whose card it was** — the same `approval_task` join the native
    ///   approved path uses;
    /// * **whether it can be taken back** — the same
    ///   `ManifestApprovalGate::is_irreversible`, asked at the moment the tool
    ///   ran rather than re-derived when somebody later opens the dialog.
    ///
    /// `None` when the park record is not recoverable — a grant rehydrated from
    /// a journal whose park line predates this field, say. The redemption is
    /// still journaled; it simply contributes no warning, which is the same
    /// additive degradation a pre-#351 `EffectExecuted` line has.
    fn consumed_grant_effect(&self, id: &ApprovalId) -> Option<ExecutedEffect> {
        let effect = self.rt.journal.approval_effect(id)?;
        Some(ExecutedEffect {
            kind: effect.kind.clone(),
            amount_usd: effect.amount_usd,
            // The card this call was on, for the retry confirmation (#351) — a
            // real task only, never the #796 DM work key, which is not a card.
            task_id: self
                .rt
                .journal
                .approval_task(id)
                .flatten()
                .and_then(|task| task.task_id().map(str::to_string)),
            at_millis: now_millis(),
            irreversible: self.rt.approval_gate.is_irreversible(&effect),
        })
    }

    /// The deterministic answer to resolving an approval that is already gone.
    ///
    /// Synthetic on purpose: no events, no effects, nothing parked, and a
    /// `persisted_seq` of `None` — the caller gets a well-formed report saying
    /// "nothing happened" instead of an error, because from the operator's side
    /// a double-submit is not a failure, it is a request whose work was already
    /// done.
    pub(crate) fn already_resolved_report(&self) -> CycleReport {
        CycleReport {
            cycle_id: generate_id(),
            responses: vec![OutboundMessage {
                task_id: None,
                outputs: Vec::new(),
                channel: OPERATOR_CHANNEL.to_string(),
                agent: None,
                text: "This approval was already resolved.".to_string(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
                message_id: None,
            }],
            executed_effects: Vec::new(),
            parked: Vec::new(),
            persisted_seq: None,
            input_seqs: Vec::new(),
        }
    }

    /// The synthetic report for a decision that arrived after the deadline
    /// (issue #1449).
    ///
    /// Same shape and same purpose as
    /// [`already_resolved_report`](Self::already_resolved_report) — a receipt
    /// that owes no cycle still answers on a handle of the same shape — and a
    /// different sentence, because it is a different thing to have happened. The
    /// approval was not resolved by anybody; its deadline passed and the host
    /// declined it. Saying "already resolved" here would be the smaller version
    /// of the same false claim this issue is about.
    pub(crate) fn expired_report(&self) -> CycleReport {
        CycleReport {
            cycle_id: generate_id(),
            responses: vec![OutboundMessage {
                task_id: None,
                outputs: Vec::new(),
                channel: OPERATOR_CHANNEL.to_string(),
                agent: None,
                text: "This approval had passed its deadline, so it was declined automatically. \
                       Nothing was carried out."
                    .to_string(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
                message_id: None,
            }],
            executed_effects: Vec::new(),
            parked: Vec::new(),
            persisted_seq: None,
            input_seqs: Vec::new(),
        }
    }

    /// Settles a parked approval to an operator-amended effect
    /// (approve-with-edit): overlays `amended_payload` onto the parked effect and
    /// executes the amended version (at-most-once).
    ///
    /// The amend counterpart to [`settle_approval`](Self::settle_approval), and
    /// split from its follow-up cycle for the same reasons (issue #383).
    ///
    /// It **does** have an [`Expired`](ResolveReceipt::Expired) arm, on the same
    /// terms as the plain path (issue #1449), and the caller owes the retirement.
    ///
    /// It also has an [`AlreadyResolved`](ResolveReceipt::AlreadyResolved) arm
    /// (issue #1825, PR review), on the same terms as the plain path: an id
    /// with nothing parked — never parked, or already resolved by an earlier
    /// call, by any verdict — owes no cycle. Before this it fell through and
    /// "simply settled to a resolution the brain is still told about", which
    /// was harmless before per-turn continuation batching (issue #469) but not
    /// after: `spawn_follow_up` runs a `Settled` receipt straight into
    /// `continue_turn`, which durably banks the (hardcoded) verdict and
    /// decrements the turn's outstanding-decisions counter — for a call that
    /// decided nothing. A retried amend against an id another call already
    /// resolved (a double-submit, or an amend replayed after a plain deny)
    /// would then count as a *second* decision on a node blocked on only two,
    /// releasing its continuation one real decision early.
    ///
    /// Both the original and the amended effect are preserved in the immutable
    /// journal (`ApprovalParked` + `ApprovalAmended`), so the audit trail shows
    /// what the brain requested and what the operator approved.
    pub async fn settle_approval_amended(
        &self,
        id: &ApprovalId,
        amended_payload: serde_json::Value,
        by: Actor,
    ) -> Result<ResolveReceipt> {
        // See the identical guard at the top of `settle_approval`: closes the
        // same window, before anything below has touched the gate or the
        // journal.
        self.rt.ensure_not_emergency_stopped()?;
        let now = now_millis();

        if self
            .rt
            .approval_gate
            .parked_effect(id)
            .is_some_and(|effect| effect.kind == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND)
        {
            return Err(OpenCompanyError::InvalidRequest(
                "an explicit approval question has no executable payload to amend; decide the \
                 question as written"
                    .to_string(),
            ));
        }

        // Overlay the operator's edit onto the parked effect. A missing id (or
        // an expired one, caught by the gate below) yields no executable effect.
        let amended = self.rt.approval_gate.parked_effect(id).map(|mut original| {
            original.payload = overlay_payload(original.payload, amended_payload);
            original
        });
        let outcome = match amended {
            Some(effect) => {
                self.rt
                    .approval_gate
                    .resolve_amended_outcome(id, effect, by.clone(), now)
            }
            None => ResolveOutcome::NotParked,
        };
        // Issue #1825 (PR review): nothing was parked under `id` this call —
        // either it was never parked, or an earlier call (amend or plain,
        // approve or deny) already resolved it. Same answer as the plain
        // path's identical guard in `settle_approval`: no journal record, no
        // bank, no continuation decision. See this function's doc comment for
        // why this arm is now required rather than merely tidy.
        if outcome == ResolveOutcome::NotParked {
            return Ok(ResolveReceipt::AlreadyResolved);
        }
        // Issue #1449, the amend half of the same defect. An edit applied to a
        // card past its deadline is still not a decision — and it is the worse
        // half of the bug, because the fall-through recorded an `ApprovalAmended`
        // too: a named operator both editing and approving an effect the gate
        // had already refused. Same answer as the plain path; the caller retires
        // it.
        if outcome == ResolveOutcome::Expired {
            return Ok(ResolveReceipt::Expired);
        }
        // Only `Approved` reaches here now — `NotParked` and `Expired` both
        // returned above. `executed` stays an `Option` (rather than unwrapping
        // outright) so the arm this guards stays legible on its own terms if a
        // future `ResolveOutcome` variant is ever added here.
        let executed = match outcome {
            ResolveOutcome::Approved(effect) => Some(effect),
            _ => None,
        };

        // Audit the amendment (when one ran) and drain the queue durably.
        if let Some(effect) = &executed {
            self.rt.journal.record_amended(id, effect, now).await?;
        }
        self.rt.journal.record_resolved(id).await?;

        // Issue #243: same fork as the plain approve — a harness tool call mints
        // a grant instead of executing. Crucially the grant is minted against the
        // **amended** arguments, so what the policy will admit is what the
        // operator actually approved. Granting the original would let the agent
        // re-issue the very call the operator edited, silently discarding the
        // edit — which is worse than not supporting amend at all, because the
        // operator would have every reason to believe their change took effect.
        //
        // Always `GrantScope::Once`. An argument edit and a standing grant are
        // contradictory requests: the edit says "this exact call, with my
        // correction", the standing grant says "any arguments, for a week". The
        // route rejects the pairing as a 400, so this arm never sees a broader
        // scope, and hard-coding it here means it cannot acquire one by accident.
        if let Some(effect) = &executed {
            self.settle_approved_effect(id, effect.clone(), by.clone(), GrantScope::Once)
                .await?;
        }

        // Issue #1825: same inline, durable bank as the plain approve path —
        // see `settle_approval` and `CompanyRuntime::bank_blocked_node_approval`.
        // `executed.is_some()` now always holds by construction (`NotParked`
        // and `Expired` both returned above), so this bank runs exactly when
        // `outcome` was `ResolveOutcome::Approved` — kept as an explicit `if`,
        // rather than unconditional, so the invariant this call depends on
        // ("only bank an id this call actually approved") stays visible here
        // too, not only at the early return that currently guarantees it.
        // `Verdict::Approve` is still the right constant to bank: an amend is
        // *defined* as an approve, and this arm now only ever runs for one.
        if executed.is_some() {
            self.rt
                .bank_blocked_node_approval(id, Verdict::Approve)
                .await;
        }
        // The follow-up event, so the brain learns the approval resolved (with
        // an edit). `CompanyEvent` is closed, so the verdict rides as `Approve`;
        // the edit itself lives in the journal audit trail.
        Ok(ResolveReceipt::Settled(Box::new(
            CompanyEvent::ApprovalResolved {
                approval_id: id.clone(),
                verdict: Verdict::Approve,
                by,
            },
        )))
    }

    /// Replays the journal to rebuild the executed-key set, the approval queue,
    /// and the live grant set.
    ///
    /// The grant window spans a model turn, so a deploy or a crash inside it is
    /// ordinary rather than exotic. Without this seeding, an operator's approval
    /// would evaporate across a restart and the agent would come back asking for
    /// a permission it had just been given. Consumed and expired grants are
    /// folded out during replay, so this can only ever re-arm one that never
    /// fired.
    pub async fn recover(&self) -> Result<()> {
        self.rt.journal.load().await?;
        self.rt.grants.rehydrate(self.rt.journal.replayed_grants());
        self.rt
            .grants
            .rehydrate_continuations(self.rt.journal.replayed_approval_continuations());
        // Issue #1863: a blocker answered moments before a restart must re-enter
        // the stopped step on the other side rather than evaporate, exactly as a
        // grant or a continuation does above.
        self.rt
            .grants
            .rehydrate_blocker_resolutions(self.rt.journal.replayed_blocker_resolutions());
        // Issue #374: standing grants outlive a restart too — a week-long
        // permission that evaporated on every deploy would be worse than not
        // offering one. Anything already past its deadline is folded out by the
        // replay itself, so a host that was down across an expiry cannot hand
        // the permission back.
        self.rt
            .grants
            .rehydrate_standing(self.rt.journal.replayed_standing_grants(now_millis()));
        // Issue #469: and the turns still blocked on a decision. A restart in
        // the middle of a partly-decided turn must come back knowing it is
        // blocked, or its continuation fires on the next decision as though the
        // others had never been owed.
        self.rt.continuations.rearm(self.rt.journal.parked_turns());
        // Issue #978: the run-scoped half, from the same replayed queue. A gate
        // still parked keeps its whole effect in the journal, so a rehydrated
        // batch is re-dispatchable; one already resolved is gone from both, and
        // the run it belonged to is continued by whatever decision remains.
        let parked = self.rt.journal.pending();
        self.rt
            .workflow_gates
            .rearm(parked.iter().filter_map(|entry| {
                entry
                    .batch
                    .clone()
                    .map(|turn| (turn, entry.id.clone(), &entry.effect))
            }));
        Ok(())
    }

    async fn route_response(&self, msg: &OutboundMessage) -> Result<()> {
        for channel in &self.rt.channels {
            if channel.channel_id() == msg.channel {
                channel.send(msg.clone()).await?;
                return Ok(());
            }
        }
        // Issue #151: an agent reply is addressed by *agent id*, not by adapter
        // id — a delegated desk bubble and a dispatched card's post-back both
        // carry `channel: "<agent_id>"` so the console can attribute them. No
        // adapter answers to an agent id, so this used to drop them silently:
        // the operator REST route reads `CycleReport.responses` directly and
        // never noticed, but a company reached over a real channel adapter got
        // the orchestrator's reply and lost every delegated one.
        //
        // Fall back to the operator adapter, which is the console's own surface
        // and always the right destination for an agent→human reply. The
        // message is forwarded unchanged, so its `channel` still names the agent
        // and attribution survives.
        if let Some(operator) = self
            .rt
            .channels
            .iter()
            .find(|c| c.channel_id() == OPERATOR_CHANNEL)
        {
            tracing::debug!(
                channel = %msg.channel,
                "no adapter for this channel id; delivering via the operator channel"
            );
            operator.send(msg.clone()).await?;
            return Ok(());
        }
        // Nothing to deliver on at all (a runtime with no operator adapter).
        tracing::debug!(
            channel = %msg.channel,
            "no adapter for this channel id and no operator channel; response not delivered"
        );
        Ok(())
    }
}

/// Overlays an operator's payload edit onto the original effect payload.
///
/// When both are JSON objects the top-level keys are merged (the edit wins);
/// otherwise the edit replaces the original wholesale. An operator can thus
/// tweak individual fields (e.g. lower an amount) without restating the payload.
fn overlay_payload(original: serde_json::Value, edit: serde_json::Value) -> serde_json::Value {
    match (original, edit) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(over)) => {
            for (key, value) in over {
                base.insert(key, value);
            }
            serde_json::Value::Object(base)
        }
        (_, edit) => edit,
    }
}

/// Executes an effect at most once, keyed by `key`.
///
/// The key is committed to the journal *before* the side effect runs, so a
/// crash after the commit drops the effect rather than repeating it — the
/// at-most-once durability guarantee.
pub(crate) async fn execute_effect_once(
    rt: &CompanyRuntime,
    key: &str,
    effect: &Effect,
    task_id: Option<&str>,
) -> Result<()> {
    if rt.journal.is_executed(key) {
        return Ok(());
    }
    // The commit boundary, and so the last place the stop can still hold.
    //
    // Every caller checks the flag before reaching here, and every one of those
    // checks sits behind at least one `.await` — resolving an approval journals
    // the verdict before this runs, and a tool-call settlement yields on the
    // grant lookup. A stop landing in that window would otherwise send the
    // email or move the money after the company had reported itself stopped.
    //
    // Refused before `record_executed`, never after: the at-most-once mark is
    // what makes the runtime never re-attempt an effect, so recording it and
    // then refusing would lose the effect permanently rather than defer it.
    // Unmarked, the key is still executable once an operator releases the stop.
    rt.ensure_not_emergency_stopped()?;
    // The commit now describes what it is committing (issue #351). Classified
    // here, against the gate in force at execution time, because this is the one
    // place that has both the effect and the policy — and because "was this
    // irreversible?" is a question about the moment it ran, not about whatever
    // the cap happens to be when somebody later opens the retry dialog.
    //
    // The record describes what is *committed to run*, and stands even if
    // `perform_effect` below then fails — that ordering is the at-most-once
    // guarantee, and the runtime will never re-attempt the effect afterwards, so
    // an operator has to assume it happened. Every wording downstream is
    // qualified to match; see [`ExecutedEffect`].
    rt.journal
        .record_executed(
            key,
            ExecutedEffect {
                kind: effect.kind.clone(),
                amount_usd: effect.amount_usd,
                task_id: task_id.map(str::to_string),
                at_millis: now_millis(),
                irreversible: rt.approval_gate.is_irreversible(effect),
            },
        )
        .await?;
    perform_effect(rt, effect).await
}

/// The Phase-1 effect executor: record spend to the ledger and route any
/// message payload to its channel. Richer effect kinds land in later phases.
async fn perform_effect(rt: &CompanyRuntime, effect: &Effect) -> Result<()> {
    if let Some(amount) = effect.amount_usd {
        rt.store
            .append_ledger(
                &rt.id,
                LedgerEntry {
                    at_millis: now_millis(),
                    kind: effect.kind.clone(),
                    amount_usd: amount,
                    memo: format!("effect {}", effect.kind),
                },
            )
            .await?;
    }
    if let (Some(channel), Some(text)) = (
        effect.payload.get("channel").and_then(|v| v.as_str()),
        effect.payload.get("text").and_then(|v| v.as_str()),
    ) {
        for adapter in &rt.channels {
            if adapter.channel_id() == channel {
                adapter
                    .send(OutboundMessage {
                        message_id: None,
                        task_id: None,
                        outputs: Vec::new(),
                        channel: channel.to_string(),
                        agent: None,
                        text: text.to_string(),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    })
                    .await?;
                break;
            }
        }
    }
    if effect.kind == EMAIL_SEND_KIND {
        let to = effect
            .payload
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let subject = effect
            .payload
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let body = effect
            .payload
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        send_company_email(rt, to, subject, body).await?;
    }
    // Issue #395: an approved workflow gate. The paused run is long settled —
    // the engine returns rather than suspending — so "continue" means starting a
    // fresh supervised run with the gate id in the trigger input's `approvals`.
    // At-most-once comes free from the `approval:<id>` key above; deny and TTL
    // expiry never reach here, and since nothing was held open, nothing running
    // is the complete outcome. See `workflow_resume` for why this is a re-run
    // and what that costs.
    //
    // Issue #978: it no longer starts that run **here**, and the distinction is
    // the whole of the amplification fix. This arm fires once per approved
    // effect, so a run with three gated nodes ran it three times: three runs,
    // each replaying the graph with one usable approval and re-parking the other
    // two — 3 → 6 → 12 → 24. The spawn now belongs to the batch release in
    // `continue_turn`, which happens once per run, and this only banks the
    // decision. A card whose run is not armed (parked before #978, so its
    // journal line carries no turn key) still re-dispatches immediately: there
    // is no batch coming to release it.
    if effect.kind == crate::runtime::WORKFLOW_APPROVE_KIND {
        crate::runtime::workflow_resume::on_gate_approved(rt, effect).await?;
    }
    Ok(())
}

/// Sends an `email.send` effect via the company's own outbound-mail handle
/// and records the send to the sender's own inbox (so the console shows
/// outbound mail alongside inbound).
async fn send_company_email(
    rt: &CompanyRuntime,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<()> {
    let Some(mail) = rt.mail() else {
        return Err(OpenCompanyError::InvalidRequest(
            "email is not configured for this company".into(),
        ));
    };
    let email = OutboundEmail {
        to: to.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
    };
    mail.sender
        .send(&MailCredentials::Smtp(mail.smtp.clone()), &email)
        .await?;
    // Record to the sender's own inbox (from = the company's own address).
    crate::server::ops::smtp::record_outbound(rt, &mail.smtp, &email).await;
    Ok(())
}

/// The company's own outbound-mail address, or empty when no mail is
/// configured for this company.
fn company_address(rt: &CompanyRuntime) -> String {
    rt.mail()
        .map(|mail| mail.smtp.from_email.clone())
        .unwrap_or_default()
}

/// True iff the company's inbox already holds a prior **inbound** email from
/// `to` — an established thread, so replying is auto-allowed instead of
/// parking for approval. Fails closed (`false`) on a missing mail handle or a
/// store error, which routes the caller to the cold-recipient park path.
///
/// Delegates the lookup to [`has_inbound_from`](crate::ports::InboxStore::has_inbound_from)
/// rather than scanning a page of
/// [`messages`](crate::ports::InboxStore::messages): this answer decides
/// an approval gate, and a gate built on a capped oldest-first page silently
/// stops finding real correspondents once the inbox outgrows the cap — past
/// that point every reply parks, and an approval queue full of legitimate mail
/// is one operators learn to rubber-stamp (issue #232).
async fn recipient_is_established(rt: &CompanyRuntime, to: &str) -> bool {
    let address = company_address(rt);
    if address.is_empty() {
        return false;
    }
    let key = crate::server::ops::smtp::local_part(&address);
    rt.inbox()
        .has_inbound_from(rt.id(), &key, to)
        .await
        .unwrap_or(false) // fail closed → parks for approval
}

/// Maps a conversation thread into a single safe branch segment (issue #796).
///
/// The write tier's branch is `oc/<company>/<unit>`, and for a DM the unit is
/// its thread — which, unlike a card id, can hold anything. Keep the characters
/// `RepoManager::validate_task_segment` accepts, fold the rest to `-`, and
/// prefix `dm-` so the result cannot lead with `-`, cannot be empty, cannot
/// collide with a card id, and reads as "a conversation's branch" in `git log`.
/// `None` when nothing usable survives.
pub(crate) fn sanitize_work_segment(thread: &str) -> Option<String> {
    let cleaned: String = thread
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['-', '.']);
    if cleaned.is_empty() {
        return None;
    }
    // Bound the body well under `validate_task_segment`'s 128-char cap; the
    // characters are all ASCII, so a byte take is a char take.
    let body: String = cleaned.chars().take(100).collect();
    // Injective, not just safe. Folding every disallowed character to `-` — which
    // is itself a keep-character — and trimming/truncating are all lossy, so two
    // distinct threads can reduce to one body: `coder/main` and `coder-main` both
    // become `coder-main`. Since this value keys checkout retention and the
    // `oc/<company>/<unit>` publish branch, a collision would let one thread
    // reclaim another's tree or publish over its branch. When anything was lost,
    // append a short stable digest of the *raw* thread so distinct threads keep
    // distinct keys; a thread that was already a safe segment is unchanged, so
    // its key stays readable.
    if body == thread {
        Some(format!("dm-{body}"))
    } else {
        Some(format!("dm-{body}-{}", short_thread_digest(thread)))
    }
}

/// A short, build-stable digest of a raw thread id (64-bit FNV-1a), used to keep
/// two threads that sanitise to the same body from sharing a work key.
///
/// A `std` `DefaultHasher` is deliberately not used: its output is not
/// guaranteed stable across toolchain versions, and this digest names a durable
/// branch and checkout key that must hash the same on every build.
fn short_thread_digest(thread: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in thread.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The board task a cycle is working, read off its own trigger events
/// (issue #333) — the correlation key every approval this cycle parks carries.
///
/// Two ways a cycle belongs to a card, and both are a real id:
///
/// * a [`TaskDispatched`](CompanyEvent::TaskDispatched) event — the card was
///   dragged into `in_progress` and this cycle is its run;
/// * an [`ApprovalResolved`](CompanyEvent::ApprovalResolved) event whose
///   approval was itself parked for a card. Approving a gated tool call
///   re-dispatches the agent (issue #243), and that follow-up cycle is still
///   the same card's work — so a run that needs two sign-offs keeps both,
///   instead of losing the link the moment the first one is granted.
///
/// **An ambiguous batch yields `None`.** A cycle is the unit of batching, not
/// of work: several triggers can ride one, and only some of them belong to a
/// card. Two rival triggers therefore mean no stamp at all, because guessing
/// one would hand a task approvals that are not its own — the precise failure
/// this issue exists to end. Two kinds of rivalry, both disqualifying:
///
/// * **two cards** — two `TaskDispatched` events, or a dispatch plus a
///   resolution belonging to a different card;
/// * **a card and a non-card turn** — an operator chat message, a webhook, a
///   schedule tick, an inbound A2A task, a payment or a filed feedback item
///   batched alongside a dispatch. That turn's parked effect is not the card's
///   work, and stamping it with the card's id is the same misattribution one
///   level down. (Issue #357 guards this seam at a finer grain, per *attempt*,
///   with a queue-position boundary; this rule only has to stop the cross-turn
///   leak.)
///
/// The match over [`CompanyEvent`] is **exhaustive on purpose** — no wildcard.
/// Every variant is classified as one of: names a card, rivals a card, or is a
/// record of something that already happened. A new variant should not silently
/// default to "harmless"; a new *inbound trigger* defaulting that way is exactly
/// how the misattribution above comes back. Adding one now fails the build until
/// somebody decides which of the three it is.
///
/// An unstamped park is recorded as
/// [`TaskLink::Unlinked`](crate::runtime::journal::TaskLink::Unlinked): honest,
/// and deliberately *not* a fall-back to the run window, which would put the
/// approval right back on whichever card was running.
/// Whether a cycle's trigger batch contains content that arrived from
/// OUTSIDE — a `WebhookReceived` (a channel message, an email, a third-party
/// callback). Operator speech (`OperatorMessage`), dispatches, schedule fires
/// and payment notifications are the company's own machinery: Internal, per
/// the operator-facts authorship precedent. Named and pure so the boundary is
/// testable — see `CycleHostImpl::external_trigger` for what rides on it.
///
/// `A2aTaskReceived` sits WITH `WebhookReceived`: it is a remote agent's raw
/// payload (the operator surface calls both "raw third-party payloads", and
/// the A2A route promptguard-sanitizes it for exactly that reason) — the #68
/// sibling review's M1 caught it missing here. `FeedbackFiled` is a
/// deliberate Internal: feedback is filed through the company's own console
/// by its own people — operator authorship, not outside content.
fn cycle_is_external(events: &[CompanyEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            CompanyEvent::WebhookReceived { .. } | CompanyEvent::A2aTaskReceived { .. }
        )
    })
}

fn cycle_task_id(
    events: &[CompanyEvent],
    approval_task: impl Fn(&ApprovalId) -> Option<Option<TaskLink>>,
) -> Option<String> {
    let mut found: Option<String> = None;
    for event in events {
        let candidate = match event {
            // Never a trigger: the marker records that a child turn was created,
            // it does not ask for one.
            CompanyEvent::ReferralEnqueued { .. } => None,
            CompanyEvent::TaskDispatched { task_id, .. } => Some(task_id.clone()),
            CompanyEvent::ApprovalResolved { approval_id, .. } => {
                match approval_task(approval_id) {
                    // Resolved an approval that belongs to a card: this cycle
                    // continues that card's work.
                    Some(Some(TaskLink::Task { id })) => Some(id),
                    // Known to belong to no card — a rival turn, not a neutral
                    // event, so the batch is ambiguous.
                    Some(Some(TaskLink::Unlinked)) => return None,
                    // A pre-#333 park, or an id with no origin at all: nothing
                    // is claimed either way, so it neither stamps nor blocks.
                    Some(None) | None => continue,
                }
            }
            // An inbound trigger that is its own work, riding the same batch as
            // a dispatch. Its parked effect is not the card's.
            CompanyEvent::OperatorMessage { .. }
            | CompanyEvent::WebhookReceived { .. }
            | CompanyEvent::ScheduleFired { .. }
            | CompanyEvent::A2aTaskReceived { .. }
            | CompanyEvent::PaymentReceived { .. }
            | CompanyEvent::FeedbackFiled { .. } => return None,
            // Records of something that already happened, not triggers for new
            // work: they neither name a card nor compete with one, so they pass
            // through without affecting the stamp.
            //
            // `ApprovalParked` (issue #379) is emphatically a record: it is
            // *this* function's own output reaching the log, appended after the
            // park it describes. Treating it as a trigger would make a cycle
            // that parks twice disqualify its own second stamp.
            CompanyEvent::LifecycleChanged { .. }
            // Issue #86: a record of an operator's governance decision, on the
            // same terms as the lifecycle change above it. The stop is enforced
            // in the approval gate, not by starting or claiming a cycle, so it
            // names no card and competes with none.
            | CompanyEvent::EmergencyPauseChanged { .. }
            // Issue #327: joins `TaskCardChanged` below on the same terms — a
            // record of a write that already happened, appended by the
            // workspace store after it. A note that started a cycle merely by
            // being written would re-enter that store and announce again.
            | CompanyEvent::WorkspaceChanged { .. }
            | CompanyEvent::AgentReply { .. }
            | CompanyEvent::ApprovalParked { .. }
            // Issue #1805: an operator's deadline extension is a record of a
            // decision, not a work trigger — it names no card and competes with
            // none, exactly like the park it defers.
            | CompanyEvent::ApprovalExtended { .. }
            | CompanyEvent::MemoryFactDeleted { .. }
            // A reaction (issue #364) is a reader's response to a message that
            // already exists. It starts no work and rivals no conversation, so
            // it passes through exactly like every other record here.
            | CompanyEvent::ReactionToggled { .. }
            // A credential or connection change (issue #403) is a record of an
            // admin's decision, not a stimulus: it names no card and competes
            // with none.
            | CompanyEvent::ToolAccessChanged { .. }
            | CompanyEvent::McpCallFailed { .. }
            | CompanyEvent::WorkflowCreated { .. }
            | CompanyEvent::WorkflowUpdated { .. }
            | CompanyEvent::WorkflowDeleted { .. }
            // Structural audit rows: a teammate or desk was created, a seat
            // moved, a move grammar changed. Each records a decision somebody
            // already made — none names a card and none competes with a
            // conversation, so they pass through exactly like every other
            // record here.
            | CompanyEvent::TeammateAdded { .. }
            | CompanyEvent::DeskCreated { .. }
            | CompanyEvent::DeskDeleted { .. }
            | CompanyEvent::DeskMembersChanged { .. }
            | CompanyEvent::DeskRoutingConfigured { .. }
            | CompanyEvent::SkillChanged { .. }
            // Plan hive-desks, Phase 4: the episode record — brackets around
            // the `AgentReply` rows a room wrote, and the driver's checkpoint.
            // Records of a round that already ran, not stimuli for a cycle.
            | CompanyEvent::EpisodeOpened { .. }
            | CompanyEvent::RoundStarted { .. }
            | CompanyEvent::RoundCommitted { .. }
            | CompanyEvent::BroadcastRouted { .. }
            | CompanyEvent::DmDelivered { .. }
            | CompanyEvent::EpisodeCompleted { .. }
            | CompanyEvent::ConversationOpened { .. }
            | CompanyEvent::ConversationConcluded { .. }
            | CompanyEvent::EpisodeSeatParked { .. }
            | CompanyEvent::EpisodeSeatResumed { .. }
            | CompanyEvent::EpisodeStateSaved { .. }
            | CompanyEvent::WorkflowEnabledChanged { .. }
            | CompanyEvent::WorkflowRunFinished { .. }
            // Issue #371/#382: a run's start and its per-node start/finish
            // brackets are records of a workflow walking its graph, not stimuli
            // for a new cycle. They name no card and compete with none, so they
            // pass through exactly like the run outcome they bracket.
            | CompanyEvent::WorkflowRunStarted { .. }
            | CompanyEvent::WorkflowNodeStarted { .. }
            | CompanyEvent::WorkflowNodeFinished { .. }
            // Issue #529: a report that left the process is a record of a
            // dispatch a workflow already made, journaled write-behind so a
            // re-run can skip it. Like the run outcome it precedes, it starts no
            // cycle and rivals no card.
            | CompanyEvent::WorkflowReportDelivered { .. }
            // Issue #617: a disclosure that a child's call was never offered for
            // approval. It records something a run already did; it names no card
            // and asks nothing of anyone, so it starts no cycle. Neutral for the
            // same reason the run bracket above is.
            | CompanyEvent::WorkflowChildCallNotOffered { .. }
            | CompanyEvent::TaskSteered { .. }
            | CompanyEvent::TaskDiscussionPosted { .. }
            // A withdrawal (#358) is a record about a record: it starts no
            // work, names no card to compete for, and its whole content is a
            // pointer to an earlier post.
            | CompanyEvent::TaskDiscussionRedacted { .. }
            // Issue #464: a board write announcing itself. Emphatically a
            // record — it is appended by the store *after* the write it
            // describes, so treating it as a trigger would let a card start
            // work merely by existing, and that work's own card writes would
            // announce again.
            | CompanyEvent::TaskCardChanged { .. }
            // Issue #983: the accept/settle brackets of a chat turn. Records of
            // something that already happened, exactly like the workflow-run
            // brackets above — they name no card, and they are appended by the
            // route that already started the turn they describe, so treating
            // either as a stimulus would make a turn re-trigger itself.
            | CompanyEvent::TurnStarted { .. }
            | CompanyEvent::TurnFailed { .. }
            | CompanyEvent::TurnSettled { .. }
            // Issue #1015: an attempt row announcing its own move. The same
            // argument as `TaskCardChanged` directly above, and it matters more
            // here — the store appends it *after* the status write, and the
            // write is made by this very cycle, so treating it as a stimulus
            // would let a run re-trigger itself on every transition it makes.
            | CompanyEvent::RunStatusChanged { .. }
            | CompanyEvent::DeskTaskCompleted { .. }
            // Issue #1843: both are records of the activation funnel moving,
            // best-effort journaled after the fact by
            // `crate::company::activation`. Neither names a card nor competes
            // with one, exactly like the other audit-trail arms above.
            | CompanyEvent::OnboardingStepCompleted { .. }
            | CompanyEvent::OnboardingCompleted { .. } => continue,
        };
        let Some(candidate) = candidate else { continue };
        match &found {
            Some(existing) if existing != &candidate => return None,
            Some(_) => {}
            None => found = Some(candidate),
        }
    }
    found
}

/// The chat thread a cycle is answering, read off its own trigger events
/// (issue #379) — the correlation key every approval this cycle parks carries,
/// and the one thing that lets a request be raised in the conversation that
/// produced it.
///
/// The sibling of [`cycle_task_id`], and deliberately the same shape, because
/// it is the same problem one axis over: a cycle is the unit of batching, not
/// of conversation, and stamping an approval with a thread it did not come from
/// puts a private request into a channel (or a channel's into a private line).
///
/// Two ways a cycle belongs to a thread, and both are a real id:
///
/// * an [`OperatorMessage`](CompanyEvent::OperatorMessage) carrying `chat` —
///   the desk id for a channel, the roster agent id for a direct message. That
///   field is precisely the disambiguator [`Effect::agent`] cannot be: a desk
///   channel and a DM to that desk's lead are answered by the same agent and
///   are **different strings** here;
/// * an [`ApprovalResolved`](CompanyEvent::ApprovalResolved) event whose
///   approval was itself parked in a thread. Approving a gated tool call
///   re-dispatches the agent (issue #243), and if that follow-up turn needs a
///   *second* sign-off, the re-park belongs in the channel the first one was
///   asked in — not nowhere.
///
/// **An ambiguous batch yields `None`**, and an unaddressed operator message is
/// itself a rival: it names no thread, so a batch holding one plus an addressed
/// message cannot say which conversation a parked effect came from. As with
/// `cycle_task_id`, no stamp means "no channel owns this", which lands the
/// approval on the Approvals page alone — today's behaviour, and never a guess.
///
/// The match is **exhaustive on purpose** — no wildcard. Every variant is one
/// of: names a thread, rivals a thread, or is a record of something that
/// already happened. A new *inbound trigger* silently defaulting to "harmless"
/// is exactly how a request leaks into the wrong conversation.
///
/// # The thread within the channel (issue #435)
///
/// Both keys are resolved here, in one pass, so a cycle can never be stamped
/// with one approval's channel and another's thread.
///
/// They are **not** resolved on the same terms, and that asymmetry is the whole
/// point. The channel rule above is untouched: a batch whose messages name
/// different channels is ambiguous and yields nothing, exactly as before. The
/// thread key is strictly weaker — when the batch agrees on a channel but
/// disagrees on the thread inside it, the channel survives and only the thread
/// is dropped.
///
/// Resolving the pair as one unit would have been wrong: two messages in one
/// channel, in two different threads, would have gone from "the channel" to
/// "nothing" and moved an approval that lands correctly today off the
/// conversation altogether. A finer key must never cost a coarser answer that
/// was already right. Dropping to `None` here means "the channel is the
/// answer", which is precisely the pre-#435 behaviour.
/// # A channel-level message is its own thread root (issue #1890)
///
/// `OperatorMessage::parent` is `None` for a message sent straight into a
/// channel, and reading it verbatim recorded "no thread" for the approval —
/// so the continuation after a sign-off landed flat in the channel while the
/// *pre-approval* reply to the very same message landed under it. The reply
/// path has not read `parent` verbatim since #1890: `reply_thread` is
/// `asked_in.unwrap_or(message_seq)`, because an unparented message is the
/// root of its own thread. This applies that same rule, which is why the
/// sequence numbers are needed here at all — a `CompanyEvent` is a body with
/// no identity, and a root can only name itself if something tells it its own
/// seq.
///
/// `seqs` is positionally aligned with `events` ([`CycleRequest::event_seqs`])
/// and may be **empty**: a caller that builds a request without threading seqs
/// is documented and supported. An absent seq degrades to `None` — today's
/// answer — rather than to a guess. The runtime always populates them, so the
/// paths an operator actually drives get the root; a seq-less caller keeps the
/// behaviour it already had.
fn cycle_conversation(
    events: &[CompanyEvent],
    seqs: &[EventSeq],
    approval_conversation: impl Fn(&ApprovalId) -> Option<ApprovalConversation>,
) -> ApprovalConversation {
    // `(channel, thread-root-within-it)`. The channel is what rivals; the root
    // rides along and is demoted to `None` on disagreement — see above.
    let mut found: Option<(String, Option<EventSeq>)> = None;
    for (index, event) in events.iter().enumerate() {
        let candidate = match event {
            // Names no conversation to answer in: it records that a child
            // turn was created elsewhere, and that turn carries its own.
            CompanyEvent::ReferralEnqueued { .. } => None,
            // The one event that names a thread outright. An unaddressed message
            // (`chat: None`) went to the orchestrator with no conversation of its
            // own — a rival, not a neutral pass-through, for the same reason a
            // non-card turn rivals a card above.
            // An unaddressed message short-circuits the whole scan, which is the
            // rival behaviour described above. (A `let … else` rather than `?`
            // only because this function no longer returns an `Option`; the
            // control flow is unchanged.)
            //
            // Issue #435: `parent` is the thread root the message hangs off,
            // read from the same event that names the channel — the two can
            // therefore never come from different messages.
            CompanyEvent::OperatorMessage { chat, parent, .. } => {
                let Some(chat) = chat else {
                    return ApprovalConversation::default();
                };
                // `parent` when it names one, otherwise this message's own seq
                // — the `reply_thread` rule, see the header. `seqs` may be
                // shorter than `events` (or empty), and then there is nothing
                // honest to fall back to.
                Some((chat.clone(), parent.or_else(|| seqs.get(index).copied())))
            }
            CompanyEvent::ApprovalResolved { approval_id, .. } => {
                match approval_conversation(approval_id) {
                    // Resolved an approval raised in a conversation: this cycle
                    // continues that conversation's work — and inherits the
                    // thread inside it, so a *second* sign-off re-parks in the
                    // thread the first was asked in rather than only its
                    // channel (issue #435, extending #379's inheritance).
                    Some(ApprovalConversation {
                        thread: Some(thread),
                        parent,
                    }) => Some((thread, parent)),
                    // Known to have come from no conversation — a rival turn,
                    // so the batch is ambiguous.
                    Some(ApprovalConversation { thread: None, .. }) => {
                        return ApprovalConversation::default();
                    }
                    // No origin recorded at all: nothing is claimed either way,
                    // so it neither stamps nor blocks.
                    None => continue,
                }
            }
            // Inbound triggers that are their own work, riding the same batch as
            // an addressed chat turn. Their parked effects are not that
            // conversation's.
            CompanyEvent::TaskDispatched { .. }
            | CompanyEvent::WebhookReceived { .. }
            | CompanyEvent::ScheduleFired { .. }
            | CompanyEvent::A2aTaskReceived { .. }
            | CompanyEvent::PaymentReceived { .. }
            | CompanyEvent::FeedbackFiled { .. } => return ApprovalConversation::default(),
            // Records of something that already happened, not stimuli for new
            // work: they name no thread and compete with none.
            CompanyEvent::LifecycleChanged { .. }
            // Issue #86: a record of an operator's governance decision, on the
            // same terms as the lifecycle change above it. The stop is enforced
            // in the approval gate, not by starting or claiming a cycle, so it
            // names no card and competes with none.
            | CompanyEvent::EmergencyPauseChanged { .. }
            // Issue #327: joins `TaskCardChanged` below on the same terms — a
            // record of a write that already happened, appended by the
            // workspace store after it. A note that started a cycle merely by
            // being written would re-enter that store and announce again.
            | CompanyEvent::WorkspaceChanged { .. }
            | CompanyEvent::AgentReply { .. }
            | CompanyEvent::ApprovalParked { .. }
            // Issue #1805: an operator's deadline extension is a record of a
            // decision, not a work trigger — it names no card and competes with
            // none, exactly like the park it defers.
            | CompanyEvent::ApprovalExtended { .. }
            | CompanyEvent::MemoryFactDeleted { .. }
            // A reaction (issue #364) is a reader's response to a message that
            // already exists. It starts no work and rivals no conversation, so
            // it passes through exactly like every other record here.
            | CompanyEvent::ReactionToggled { .. }
            // A credential or connection change (issue #403) is a record of an
            // admin's decision, not a stimulus: it names no card and competes
            // with none.
            | CompanyEvent::ToolAccessChanged { .. }
            | CompanyEvent::McpCallFailed { .. }
            | CompanyEvent::WorkflowCreated { .. }
            | CompanyEvent::WorkflowUpdated { .. }
            | CompanyEvent::WorkflowDeleted { .. }
            // Structural audit rows: a teammate or desk was created, a seat
            // moved, a move grammar changed. Each records a decision somebody
            // already made — none names a card and none competes with a
            // conversation, so they pass through exactly like every other
            // record here.
            | CompanyEvent::TeammateAdded { .. }
            | CompanyEvent::DeskCreated { .. }
            | CompanyEvent::DeskDeleted { .. }
            | CompanyEvent::DeskMembersChanged { .. }
            | CompanyEvent::DeskRoutingConfigured { .. }
            | CompanyEvent::SkillChanged { .. }
            // Plan hive-desks, Phase 4: the episode record — brackets around
            // the `AgentReply` rows a room wrote, and the driver's checkpoint.
            // Records of a round that already ran, not stimuli for a cycle.
            | CompanyEvent::EpisodeOpened { .. }
            | CompanyEvent::RoundStarted { .. }
            | CompanyEvent::RoundCommitted { .. }
            | CompanyEvent::BroadcastRouted { .. }
            | CompanyEvent::DmDelivered { .. }
            | CompanyEvent::EpisodeCompleted { .. }
            | CompanyEvent::ConversationOpened { .. }
            | CompanyEvent::ConversationConcluded { .. }
            | CompanyEvent::EpisodeSeatParked { .. }
            | CompanyEvent::EpisodeSeatResumed { .. }
            | CompanyEvent::EpisodeStateSaved { .. }
            | CompanyEvent::WorkflowEnabledChanged { .. }
            | CompanyEvent::WorkflowRunFinished { .. }
            | CompanyEvent::WorkflowRunStarted { .. }
            | CompanyEvent::WorkflowNodeStarted { .. }
            | CompanyEvent::WorkflowNodeFinished { .. }
            // Issue #529: a record of a report already dispatched — names no
            // thread and rivals none, exactly like the run events it sits among.
            | CompanyEvent::WorkflowReportDelivered { .. }
            // Issue #617: likewise a record, not a message. It belongs to no
            // conversation and rivals none.
            | CompanyEvent::WorkflowChildCallNotOffered { .. }
            | CompanyEvent::TaskSteered { .. }
            | CompanyEvent::TaskDiscussionPosted { .. }
            // A withdrawal (#358) is a record about a record: it starts no
            // work, names no card to compete for, and its whole content is a
            // pointer to an earlier post.
            | CompanyEvent::TaskDiscussionRedacted { .. }
            // Issue #464: a board write announcing itself. Emphatically a
            // record — it is appended by the store *after* the write it
            // describes, so treating it as a trigger would let a card start
            // work merely by existing, and that work's own card writes would
            // announce again.
            | CompanyEvent::TaskCardChanged { .. }
            // Issue #983: the accept/settle brackets of a chat turn. Records of
            // something that already happened, exactly like the workflow-run
            // brackets above — they name no card, and they are appended by the
            // route that already started the turn they describe, so treating
            // either as a stimulus would make a turn re-trigger itself.
            | CompanyEvent::TurnStarted { .. }
            | CompanyEvent::TurnFailed { .. }
            | CompanyEvent::TurnSettled { .. }
            // Issue #1015: an attempt row announcing its own move. The same
            // argument as `TaskCardChanged` directly above, and it matters more
            // here — the store appends it *after* the status write, and the
            // write is made by this very cycle, so treating it as a stimulus
            // would let a run re-trigger itself on every transition it makes.
            | CompanyEvent::RunStatusChanged { .. }
            | CompanyEvent::DeskTaskCompleted { .. }
            // Issue #1843: both are records of the activation funnel moving,
            // best-effort journaled after the fact by
            // `crate::company::activation`. Neither names a conversation nor
            // competes with one, exactly like the other audit-trail arms
            // above.
            | CompanyEvent::OnboardingStepCompleted { .. }
            | CompanyEvent::OnboardingCompleted { .. } => continue,
        };
        let Some(candidate) = candidate else { continue };
        match &mut found {
            // Different channels: the batch is ambiguous and nothing is
            // stamped. Unchanged from #379 — the channel is still the key that
            // rivals.
            Some((thread, _)) if *thread != candidate.0 => {
                return ApprovalConversation::default();
            }
            // Issue #435: same channel, different thread inside it. The channel
            // is still unambiguous and still correct, so it survives; only the
            // finer key is dropped. Escalating this to a full rival would move
            // an approval that lands correctly today off its conversation
            // entirely — see the asymmetry note in the doc above.
            Some((thread, parent)) if *parent != candidate.1 => {
                // Say so, because from the outside this is indistinguishable
                // from the bug #435 fixed: the operator sees a threaded request
                // answered in the channel and has no way to tell "we dropped the
                // thread on purpose, the batch named two" from "the thread was
                // lost again". Debug rather than warn — the outcome is correct
                // and is the pre-#435 behaviour, so it is an explanation on
                // demand, not an incident.
                tracing::debug!(
                    channel = %thread,
                    dropped_parent = ?*parent,
                    rival_parent = ?candidate.1,
                    "[approval] two threads in this batch, resuming in the channel; \
                     the channel is unambiguous so only the thread root is dropped (#435)"
                );
                *parent = None;
            }
            Some(_) => {}
            None => found = Some(candidate),
        }
    }
    found
        .map(|(thread, parent)| ApprovalConversation {
            thread: Some(thread),
            parent,
        })
        .unwrap_or_default()
}

/// The host the brain calls back into mid-cycle. Bridges tool, context, and
/// effect callbacks to the runtime's ports and gates every effect.
/// Effects executed and approvals parked by a cycle — counted, not owned.
///
/// Separate from `CycleReport` because the report only exists on success, while
/// these two numbers describe work that has already happened and cannot be
/// undone by a later failure. Reported as zero on a failed cycle, they made
/// `turn_finished` systematically undercount exactly the turns worth looking at.
#[derive(Clone, Copy, Default)]
struct EffectCounts {
    executed: u64,
    parked: u64,
}

struct CycleHostImpl<'a> {
    company: CompanyId,
    cycle_id: String,
    rt: &'a CompanyRuntime,
    counter: AtomicU64,
    executed: StdMutex<Vec<Effect>>,
    parked: StdMutex<Vec<ApprovalId>>,
    /// The board task this cycle is working, when it is working one
    /// (issue #333) — stamped onto every approval the cycle parks.
    ///
    /// Computed once, from the cycle's own trigger events, by
    /// [`cycle_task_id`]. It is a real id rather than a time window: whatever
    /// turn parks the effect — the dispatched card's own turn, a desk it
    /// delegated to, an email it tried to send — the approval belongs to the
    /// task whose dispatch opened this cycle, and to no other.
    task_id: Option<String>,
    /// The chat thread this cycle is answering, when it is answering one
    /// (issue #379) — stamped onto every approval the cycle parks, and what
    /// lets the request be raised in that conversation instead of only on the
    /// Approvals page.
    ///
    /// Computed once, from the cycle's own trigger events, by
    /// [`cycle_conversation`]. `None` for a cycle with no conversation behind it
    /// (a dispatched card, a scheduler tick, a workflow delivery) and for an
    /// ambiguous batch — both of which leave the approval where it is today.
    thread_id: Option<String>,
    /// The thread *inside* [`thread_id`](Self::thread_id) this cycle is
    /// answering (issue #435) — stamped onto every approval the cycle parks so
    /// the continuation can be threaded back under the same root, instead of
    /// landing flat in the channel and losing the conversation its own
    /// conclusion belongs to.
    ///
    /// Computed in the same pass as `thread_id`, so the two always describe one
    /// message. `None` whenever `thread_id` is, and additionally when the batch
    /// agrees on a channel but not on a thread within it — which degrades to
    /// exactly the pre-#435 behaviour of answering in the channel.
    thread_parent: Option<EventSeq>,
    /// Whether this cycle was triggered by content that arrived from OUTSIDE —
    /// a `WebhookReceived` (a channel message, an email, a third-party
    /// callback) or an `A2aTaskReceived` (a remote agent's payload) in its
    /// trigger batch. Computed once, like `task_id`. A brain-chosen
    /// `ContextOp::Put` in such a cycle can be (and on the medulla path
    /// routinely is) the raw inbound payload echoed back, so the write goes
    /// through the taint-stamping inbound port instead of the internal one
    /// (issue #1113). Coarse by design: the host cannot see which put quoted
    /// the payload, so every put of an externally-triggered cycle carries the
    /// external stamp — over-tainting is safe, under-tainting is the leak.
    external_trigger: bool,
}

impl<'a> CycleHostImpl<'a> {
    fn new(
        company: CompanyId,
        cycle_id: String,
        rt: &'a CompanyRuntime,
        task_id: Option<String>,
        external_trigger: bool,
        conversation: ApprovalConversation,
    ) -> Self {
        Self {
            company,
            cycle_id,
            rt,
            counter: AtomicU64::new(0),
            executed: StdMutex::new(Vec::new()),
            parked: StdMutex::new(Vec::new()),
            task_id,
            external_trigger,
            thread_id: conversation.thread,
            thread_parent: conversation.parent,
        }
    }

    /// What this host has irreversibly done so far, readable without consuming it.
    ///
    /// `into_outcomes` can only be reached on the success path, but an effect
    /// that has executed and an approval that has parked are facts already —
    /// they survive whatever fails afterwards. Reading the counts through the
    /// same mutexes lets the turn report them even when the cycle goes on to
    /// fail (issue #1739).
    fn counts(&self) -> EffectCounts {
        EffectCounts {
            executed: self.executed.lock().expect("executed poisoned").len() as u64,
            parked: self.parked.lock().expect("parked poisoned").len() as u64,
        }
    }

    fn into_outcomes(self) -> (Vec<Effect>, Vec<ApprovalId>) {
        (
            self.executed.into_inner().expect("executed poisoned"),
            self.parked.into_inner().expect("parked poisoned"),
        )
    }

    /// Evaluates an effect against policy and either executes it (at-most-once),
    /// parks it for approval, or denies it. Shared by `emit_effect` and the
    /// `send_email` tool interception.
    async fn gate_effect(&self, effect: Effect) -> Result<EffectDisposition> {
        match self.rt.approvals.evaluate(&self.company, &effect).await? {
            PolicyDecision::Allow => {
                let idx = self.counter.fetch_add(1, Ordering::Relaxed);
                let key = format!("{}:{idx}", self.cycle_id);
                execute_effect_once(self.rt, &key, &effect, self.task_id.as_deref()).await?;
                self.executed
                    .lock()
                    .expect("executed poisoned")
                    .push(effect);
                Ok(EffectDisposition::Executed)
            }
            PolicyDecision::RequireApproval => {
                Ok(EffectDisposition::PendingApproval(self.park(effect).await?))
            }
            PolicyDecision::Deny => Ok(EffectDisposition::Denied {
                reason: format!("policy denied {}", effect.kind),
            }),
        }
    }

    /// The runtime's park transaction, over this company's live handles.
    fn parker(&self) -> crate::runtime::approval_park::ApprovalParker {
        crate::runtime::approval_park::ApprovalParker::new(
            self.rt.approvals.clone(),
            self.rt.journal.clone(),
            self.rt.grants.clone(),
            self.rt.continuations.clone(),
            self.rt.events.clone(),
        )
    }

    /// Parks `effect` on the approval gate, journals it durably, and records the
    /// id on this cycle's outcome.
    ///
    /// The single write path into the operator's approval queue: the
    /// `RequireApproval` arm of [`gate_effect`](Self::gate_effect) and the
    /// already-decided [`CycleHost::park_effect`] callback both land here, so a
    /// parked effect is journaled exactly one way and survives a restart with its
    /// original [`ApprovalId`] regardless of who decided it.
    async fn park(&self, effect: Effect) -> Result<ApprovalId> {
        let approval_id = self
            .parker()
            .park(
                &self.company,
                effect.clone(),
                crate::runtime::approval_park::ParkSite {
                    task: TaskLink::from_task_id(self.task_id.as_deref()),
                    conversation: ApprovalConversation {
                        thread: self.thread_id.clone(),
                        parent: self.thread_parent,
                    },
                    turn: Some(self.cycle_id.clone()),
                },
            )
            .await?;
        self.parked
            .lock()
            .expect("parked poisoned")
            .push(approval_id.clone());
        tracing::debug!(
            kind = %effect.kind,
            group = ?effect.group,
            approval_id = %approval_id,
            cycle = %self.cycle_id,
            task = self.task_id.as_deref().unwrap_or("-"),
            thread = self.thread_id.as_deref().unwrap_or("-"),
            "[cycle] parked effect for operator approval"
        );
        Ok(approval_id)
    }

    /// Intercepts the `send_email` tool: parses `to`/`subject`/`body`, checks
    /// whether the recipient is an established thread, and routes the result
    /// through the effect gate as an `email.send` effect rather than invoking
    /// the tool provider directly.
    async fn send_email(&self, args: serde_json::Value) -> Result<ToolResult> {
        if self.rt.mail().is_none() {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "email is not configured for this company" }),
            });
        }
        let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(to), Some(subject), Some(body)) = (get("to"), get("subject"), get("body")) else {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "send_email requires to, subject, body" }),
            });
        };
        if to.trim().is_empty() {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "recipient (to) is empty" }),
            });
        }
        let established = recipient_is_established(self.rt, &to).await;
        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: established,
            first_time_counterparty: !established,
            payload: serde_json::json!({ "to": to, "subject": subject, "body": body }),
            agent: None,
            run_id: None,
        };
        match self.gate_effect(effect).await? {
            EffectDisposition::Executed => Ok(ToolResult {
                ok: true,
                output: serde_json::json!({ "status": "sent" }),
            }),
            EffectDisposition::PendingApproval(id) => Ok(ToolResult {
                ok: true,
                output: serde_json::json!({ "status": "pending_approval", "approval_id": id.as_ref() }),
            }),
            EffectDisposition::Denied { reason } => Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "status": "denied", "reason": reason }),
            }),
        }
    }

    /// Services the `spawn_task` tool (issue #176): opens a tracked task card on
    /// the company's board through the same [`TaskStore`](crate::ports::TaskStore)
    /// path the console and the harness path use. A blank title is a clean tool
    /// error rather than a silent no-op. The card is durable, so a later direct
    /// query to its assignee surfaces it (handed-task awareness).
    async fn spawn_task(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(parsed) = SpawnTaskArgs::parse(&args) else {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({ "error": "spawn_task requires a non-empty title" }),
            });
        };
        let card = TaskRecord {
            id: generate_id(),
            title: crate::ports::tasks::TaskTitle::system(&parsed.title),
            note: parsed.note,
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: parsed.assignee.unwrap_or_default(),
            updated_at_millis: now_millis(),
            // No conversation at all (#1890 B, step 5): this tool surface never
            // recorded the channel, so there is no thread inside one to narrow
            // either. The desk and the thread are one value now, so "absent
            // together" is the only state this can be in rather than an
            // invariant a reader has to trust.
            origin: TaskOrigin::new(None, None),
            // No parent (#185), for the same reason as the harness path: this
            // is a chat-turn delegation, so no task is in scope to be the
            // parent. Lineage is set through the task API's `parentTaskId`.
            parent_task_id: None,
            // Nothing has run yet, so there is no deliverable to point at
            // (issue #339). The first successful settle stamps it.
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
        self.rt.tasks().upsert(&self.company, &card).await?;
        Ok(ToolResult {
            ok: true,
            output: serde_json::json!({
                "status": "queued",
                "task_id": card.id,
                "title": parsed.title,
            }),
        })
    }

    /// Services the `delegate_to_desk` tool (issue #176) on the hosted path: a
    /// *durable, asynchronous* hand-off. Resolves the target desk, writes a task
    /// card assigned to that desk (so a later direct query to the desk surfaces
    /// the handed work), and returns a summary the remote cognition relays to
    /// the operator.
    ///
    /// This deliberately does NOT run the desk lead's turn: a hosted build has
    /// no in-process cognition pool. The synchronous, one-voice relay the
    /// harness performs needs Medulla multi-agent support and is tracked in
    /// #176; the durable hand-off is the brain-agnostic capability that ships
    /// now. An unknown desk is a clean tool error, not a lost hand-off.
    async fn delegate_to_desk(&self, args: serde_json::Value) -> Result<ToolResult> {
        let Some(parsed) = DelegateArgs::parse(&args) else {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({
                    "error": "delegate_to_desk requires a desk and an instruction"
                }),
            });
        };
        let record = self.rt.store.load(&self.company).await?;
        let Some(desk_id) = record
            .as_ref()
            .and_then(|r| r.resolve_desk_id(&parsed.desk))
        else {
            // Issue #272: the refusal now carries the company's real desk ids
            // (and, when the invented target names a teammate, the desk that
            // teammate is on), so the remote cognition can correct itself in the
            // same turn rather than only learning that its pick was wrong. The
            // message is the one the harness tool's boundary check uses, so the
            // two paths cannot drift.
            //
            // Only the *unknown* desk is refused here. A real desk with no
            // roster lead is left alone on this path: the hosted hand-off is a
            // durable card assigned to the desk, which is visible on the board
            // whether or not anyone leads it yet — there is nothing silent
            // about it.
            let error = match record.as_ref() {
                Some(record) => unknown_desk_message(record, &parsed.desk),
                None => format!("no desk matches \"{}\"", parsed.desk),
            };
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({
                    "status": "unknown_desk",
                    "error": error,
                }),
            });
        };
        // An `auto` channel is refused here even though an ordinary leadless
        // desk is not (issue #1835, codex on #1872). The carve-out above is
        // about a desk that has no lead *yet* — a card on the board is visible
        // work either way. An auto channel has no lead by design and never
        // will, so accepting one wrote a card noting "no lead member on the
        // roster yet": false about a staffed channel, and permanently so. The
        // reason comes from `reject_auto_channel_target`, the same definition
        // the harness tool refuses through, so the two paths cannot drift.
        if let Some(reason) = record
            .as_ref()
            .and_then(|r| crate::runtime::delegation_tools::reject_auto_channel_target(r, &desk_id))
        {
            return Ok(ToolResult {
                ok: false,
                output: serde_json::json!({
                    "status": "no_lead",
                    "error": reason,
                }),
            });
        }
        // The desk's lead, when it has a roster-backed one, is recorded in the
        // note; the card is assigned to the DESK so an operator asking the desk
        // directly (chat targets the desk) sees the hand-off.
        let lead = record.as_ref().and_then(|r| desk_lead(r, &parsed.desk));
        let note = match &lead {
            Some(member) => format!(
                "Delegated to the {desk_id} desk (lead: {member}).\n\n{instruction}",
                instruction = parsed.instruction
            ),
            None => format!(
                "Delegated to the {desk_id} desk (no lead member on the roster yet).\n\n{instruction}",
                instruction = parsed.instruction
            ),
        };
        let card = TaskRecord {
            id: generate_id(),
            title: crate::ports::tasks::mint_task_title(
                &parsed.instruction,
                None,
                self.rt.titler(),
            )
            .await,
            note: Some(note),
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: desk_id.clone(),
            updated_at_millis: now_millis(),
            // No conversation at all (#1890 B, step 5): this tool surface never
            // recorded the channel, so there is no thread inside one to narrow
            // either. The desk and the thread are one value now, so "absent
            // together" is the only state this can be in rather than an
            // invariant a reader has to trust.
            origin: TaskOrigin::new(None, None),
            // No parent (#185), for the same reason as the harness path: this
            // is a chat-turn delegation, so no task is in scope to be the
            // parent. Lineage is set through the task API's `parentTaskId`.
            parent_task_id: None,
            // Nothing has run yet, so there is no deliverable to point at
            // (issue #339). The first successful settle stamps it.
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
        self.rt.tasks().upsert(&self.company, &card).await?;
        Ok(ToolResult {
            ok: true,
            output: serde_json::json!({
                "status": "handed_off",
                "desk": desk_id,
                "lead": lead,
                "task_id": card.id,
            }),
        })
    }
}

/// The first non-empty line of `text`, trimmed and capped to `max` chars — the
/// task-card title derived from a delegation instruction (which may be a whole
/// paragraph). Falls back to a short cap of the whole string when there is no
/// line break. UTF-8-safe: never slices mid-codepoint.
/// How many settled cards the briefing lists before it starts counting
/// (issue #1890 C).
///
/// Sized for **deciding, not for knowing**: enough that "did that ship?" is
/// answered from the briefing on any ordinary conversation, small enough that a
/// long-lived channel's whole board history is not re-sent on every turn. What
/// does not fit is declared as a count rather than dropped — see the write site.
const SETTLED_WORK_BRIEFING_MAX: usize = 5;

/// How many threads the index lists before it starts counting (issue #1890 E).
///
/// A handful, because this is a **selection and not an enumeration**: a channel
/// accumulates roots without limit, and what does not fit is declared as a
/// count. A model handed 5 of 28 with no marker answers "that is everything"
/// confidently and wrongly.
const THREAD_INDEX_MAX: usize = 5;

/// How much of the journal's tail the index is drawn from (issue #1890 E).
///
/// **Liveness, expressed as a bound.** A thread is "live" here if it has
/// activity inside the page the chat seed already walks — which is the same
/// window the turn's own history comes from, so the index cannot name a
/// conversation the turn could not otherwise have heard of.
///
/// Cheap since #1890 G: a tail page is read from the end of the journal rather
/// than by streaming it from the head, so this costs the page and not the
/// company's history.
const THREAD_INDEX_PAGE: usize = 256;

/// Characters of a root kept as an index row's opening words.
///
/// One constant because two places must agree on it: the opening is cut to
/// this, and the self-exclusion below re-cuts the current message to compare
/// against that cut. A literal in each is two values that must match with
/// nothing making them — which is the defect this whole change removes.
const THREAD_OPENING_CHARS: usize = 120;

/// One line of the index — a thread the turn may decide to ask about.
struct ThreadLine {
    /// The root's sequence, which is the **handle** `read_thread` takes
    /// (issue #1890 F).
    ///
    /// Carried even though a reader gains nothing from seeing it, because the
    /// alternative is a tool that matches on the opening words — and a model
    /// paraphrases rather than quotes. Strict matching then fails an obviously
    /// correct reference, and loose matching reads the *wrong* thread while
    /// looking like success, which is the cross-thread leak #1890 A exists to
    /// prevent arriving through the tool instead of the seed. An id is either
    /// in the index or it is not.
    root: EventSeq,
    /// The root's own opening words, truncated. The discriminator, and what a
    /// later reference will echo.
    opening: String,
    /// How many replies hang off it.
    replies: usize,
    /// Where its work landed, when a card raised in it has settled — the fact
    /// #1890 B made answerable by recording a card's thread.
    landed: Option<String>,
    /// The newest sequence in the thread, for ordering by recency.
    latest: EventSeq,
}

impl ThreadLine {
    /// `- [41] "draft the launch email" — 4 replies`
    ///
    /// State before count where there is one: "finished → In review" answers
    /// the question a reader is actually asking, and a reply count is only how
    /// busy it was.
    ///
    /// The id leads because it is the one part a tool call must reproduce
    /// exactly; the words are what a *reference* will echo, and they follow.
    fn render(&self) -> String {
        let id = self.root.value();
        match (&self.landed, self.replies) {
            (Some(landing), _) => format!("- [{id}] {:?} — {landing}", self.opening),
            (None, 0) => format!("- [{id}] {:?} — no reply yet", self.opening),
            (None, 1) => format!("- [{id}] {:?} — 1 reply", self.opening),
            (None, n) => format!("- [{id}] {:?} — {n} replies", self.opening),
        }
    }
}

/// The channel's other live threads, newest first (issue #1890 E).
///
/// `current` is the thread the turn is answering in, excluded from its own
/// index — `None` for a channel-level turn, which therefore sees every thread,
/// and that asymmetry is the "both directions" the epic asks for rather than
/// two separate mechanisms.
///
/// Reads one bounded tail page and derives the roots from it; a thread whose
/// last activity fell outside that page is not live and is not listed. The
/// landing comes from the settled cards already in hand, matched on the thread
/// each recorded at raise time (#1890 B).
fn thread_index(
    page: &[crate::ports::types::StoredEvent],
    desk_id: &str,
    desk_name: &str,
    current: Option<EventSeq>,
    // The message being answered, so it never appears in its own index.
    //
    // At channel level `current` is `None` — there is no thread to exclude —
    // but the message has already been journaled by the time the cycle runs,
    // so it is itself an unparented root on the page and the index would list
    // the very message it is attached to. Matched on text for the same reason
    // `chat_seed::strip_current_message` is: the in-memory event carries no
    // sequence to compare against. The same trap applies and is worth naming —
    // a *different* thread opened with identical wording is excluded too,
    // which costs one line of orientation and never shows a reader their own
    // message back as somebody else's conversation.
    current_message: &str,
    settled: &[&TaskRecord],
) -> (Vec<ThreadLine>, usize) {
    use std::collections::HashMap;

    let mut roots: HashMap<EventSeq, ThreadLine> = HashMap::new();
    let mut replies: HashMap<EventSeq, usize> = HashMap::new();
    // The newest sequence seen in each thread, tracked **independently of the
    // roots map** because the page arrives newest-first: a reply is met before
    // the root it hangs off is inserted, so updating the line in place found
    // nothing and every thread kept its root's own sequence as its recency.
    // A channel with more than `THREAD_INDEX_MAX` roots then cut the live old
    // thread in favour of quiet newer ones — the exact inversion the ordering
    // exists to prevent (codex + coderabbit on #1972).
    let mut latest: HashMap<EventSeq, EventSeq> = HashMap::new();

    for stored in page {
        if !crate::server::chat_history::owns(desk_id, desk_name, &stored.event) {
            continue;
        }
        match &stored.event {
            // A root: an operator message that hangs off nothing. Only an
            // operator message opens a thread — an agent reply is always
            // parented to the question it answers.
            CompanyEvent::OperatorMessage {
                text, parent: None, ..
            } => {
                let opening = first_line(text, THREAD_OPENING_CHARS);
                if opening.is_empty() {
                    continue;
                }
                roots.insert(
                    stored.seq,
                    ThreadLine {
                        root: stored.seq,
                        opening,
                        replies: 0,
                        landed: None,
                        latest: stored.seq,
                    },
                );
            }
            CompanyEvent::OperatorMessage {
                parent: Some(root), ..
            }
            | CompanyEvent::AgentReply {
                parent: Some(root), ..
            } => {
                *replies.entry(*root).or_default() += 1;
                let seen = latest.entry(*root).or_insert(stored.seq);
                *seen = (*seen).max(stored.seq);
            }
            _ => {}
        }
    }

    let mut lines: Vec<ThreadLine> = roots
        .into_iter()
        .filter(|(seq, line)| {
            // Compared through `first_line` on both sides, not raw. `opening`
            // is already truncated, and truncation appends `…`, so a message
            // whose first line runs past THREAD_OPENING_CHARS never
            // `starts_with` its own opening — and the turn was then listed in
            // its own index as somebody else's conversation
            // (coderabbit on #1982).
            let mine = first_line(current_message, THREAD_OPENING_CHARS);
            Some(*seq) != current && !(!line.opening.is_empty() && mine == line.opening)
        })
        .map(|(seq, mut line)| {
            line.replies = replies.get(&seq).copied().unwrap_or(0);
            // A thread with no activity keeps its root's own sequence, which is
            // when it was opened — the only recency it has.
            line.latest = latest.get(&seq).copied().unwrap_or(seq).max(seq);
            // Where the work raised in this thread landed, if any did. The
            // question "did that ship?" for a thread the turn is not in.
            line.landed = settled
                .iter()
                .find(|card| card.origin_parent() == Some(seq))
                .map(|card| {
                    format!(
                        "finished → {}",
                        crate::ports::tasks::column_label(&card.column)
                    )
                });
            line
        })
        .collect();

    // Most recent first, so "the other one" resolves to the thread most likely
    // meant, and the cap cuts the stale tail rather than the live head.
    lines.sort_by_key(|line| std::cmp::Reverse(line.latest));
    let omitted = lines.len().saturating_sub(THREAD_INDEX_MAX);
    lines.truncate(THREAD_INDEX_MAX);
    (lines, omitted)
}

/// Has this card **stopped**, in the sense the transcript's `finished → …`
/// marker means (issue #1890 C)?
///
/// "Stopped" is not "succeeded": a cancelled or failed dispatch settles too, and
/// saying so is the whole point — the misleading case this briefing exists for
/// is precisely the run that stopped without finishing the work. The same
/// reading [`CompanyEvent::DeskTaskCompleted`] itself takes.
///
/// # `todo` is the hard arm, and it is [`TaskRecord::bounced`]'s question
///
/// Every other column answers from its id alone. `todo` cannot: it is **both**
/// the failure landing and the fresh state, so a card that bounced there off a
/// failed run is indistinguishable from one nobody has touched — which is the
/// gap issue #1865 added `bounced` to close, on the board, for a human reader.
/// This is the same distinction for a model reader, so it asks the same field
/// rather than inventing a second rule. A card re-dispatched after a bounce
/// clears the marker (`todo` → `in_progress`), so it correctly stops reading as
/// settled the moment it is running again.
///
/// `planning` and `in_progress` are never settled — a pass or an attempt is
/// live — and a briefing that called them finished would be the exact
/// "concluded the work had finished when it had in fact parked" misreading
/// issue #377 set out to remove.
fn has_settled(card: &TaskRecord) -> bool {
    match card.column.as_str() {
        crate::ports::tasks::COLUMN_IN_REVIEW
        | crate::ports::tasks::COLUMN_DONE
        | crate::ports::tasks::COLUMN_PAUSED => true,
        COLUMN_TODO => card.bounced.is_some(),
        // `planning`, `in_progress`, and any column a newer host names that
        // this build has not heard of. Silence is the safe answer for an
        // unknown state: claiming a card finished is a lie, claiming nothing is
        // a gap the operator can still see on their own board.
        _ => false,
    }
}

/// One settled card, as the briefing states it (issue #1890 C).
///
/// The landing label comes from [`crate::ledger::board`] through
/// `column_label`, so this is not a fourth transcription of the column names —
/// the same discipline `chat_history::dispatch_marker_text` follows, and for
/// the same reason: a renamed column must not half-land.
///
/// A bounced card carries **why**. Without the reason "finished → To-do" reads
/// as though the work were merely queued, which is the misreading the whole
/// bounced/fresh distinction exists to prevent.
fn settled_briefing_line(card: &TaskRecord) -> String {
    let landing = crate::ports::tasks::column_label(&card.column);
    match &card.bounced {
        Some(reason) if !reason.trim().is_empty() => {
            format!(
                "- {} — finished → {landing} ({})",
                card.title,
                first_line(reason, 120)
            )
        }
        _ => format!("- {} — finished → {landing}", card.title),
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(text)
        .trim();
    match line.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &line[..idx]),
        None => line.to_string(),
    }
}

/// Whether a task card assigned to `assignee` counts as "handed to" the target
/// a direct operator message is addressed to (issue #176). Matches when the two
/// are the same string (case-insensitively), resolve to the same desk, or the
/// assignee is the addressed desk's lead — so a hand-off recorded against a desk
/// id surfaces when the operator addresses that desk by id or name, and a card
/// assigned to a person surfaces when that person is addressed.
///
/// `pub(crate)` since issue #982 for a second caller — `chat_handler_card`'s
/// adoption predicate, which has to ask the same question about the card the
/// REST handler just wrote. One comparator, so the two cannot drift.
pub(crate) fn assignment_matches(record: &CompanyRecord, target: &str, assignee: &str) -> bool {
    if assignee.eq_ignore_ascii_case(target) {
        return true;
    }
    if let (Some(a), Some(b)) = (
        record.resolve_desk_id(target),
        record.resolve_desk_id(assignee),
    ) && a == b
    {
        return true;
    }
    if let Some(lead) = desk_lead(record, target) {
        return lead.eq_ignore_ascii_case(assignee);
    }
    false
}

#[async_trait]
impl CycleHost for CycleHostImpl<'_> {
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult> {
        if call.tool == crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND {
            for field in ["title", "question"] {
                if !call
                    .args
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    return Err(OpenCompanyError::InvalidRequest(format!(
                        "`{field}` must be a non-empty string"
                    )));
                }
            }
            let approval_id = self
                .park_effect(Effect {
                    kind: crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND.to_string(),
                    group: EffectGroup::Other,
                    amount_usd: None,
                    established_thread: false,
                    first_time_counterparty: false,
                    payload: call.args,
                    // Fallback cognition has no local roster identity, but the
                    // continuation still needs a subject so approve/deny routes
                    // back into that brain rather than native effect execution.
                    agent: Some("fallback-brain".to_string()),
                    run_id: None,
                })
                .await?;
            return Ok(ToolResult {
                ok: true,
                output: serde_json::json!({
                    "status": "pending",
                    "approval_id": approval_id.as_ref()
                }),
            });
        }
        if call.tool == SEND_EMAIL_TOOL {
            return self.send_email(call.args).await;
        }
        // Issue #176: service the delegation tools device-side so the hosted
        // (Medulla) path can delegate. Unlike the harness path — which runs the
        // desk lead's turn in-process and relays it in one voice — a hosted
        // build has no local cognition pool, so the hand-off is *durable and
        // asynchronous*: a board card the desk sees when asked directly. (The
        // synchronous cross-agent cognition relay needs Medulla multi-agent
        // support; tracked in #176.)
        if call.tool == SPAWN_TASK_TOOL {
            return self.spawn_task(call.args).await;
        }
        if call.tool == DELEGATE_TO_DESK_TOOL {
            return self.delegate_to_desk(call.args).await;
        }
        // The provider enforces the manifest grant before any side effect.
        self.rt.tools.invoke(&self.company, call).await
    }

    async fn context_op(&self, op: ContextOp) -> Result<ContextOpResult> {
        match op {
            ContextOp::Put(chunk) => Ok(ContextOpResult::Addr({
                // External-triggered cycles write through the taint-stamping
                // port; see `external_trigger` on this struct.
                let port = if self.external_trigger {
                    &self.rt.inbound_context
                } else {
                    &self.rt.context
                };
                port.put(&self.company, chunk).await?
            })),
            ContextOp::List { prefix } => Ok(ContextOpResult::Metas(
                self.rt.context.list(&self.company, &prefix).await?,
            )),
            ContextOp::Peek { addr, range } => Ok(ContextOpResult::Text(
                self.rt.context.peek(&self.company, &addr, range).await?,
            )),
            ContextOp::Search { query, limit } => Ok(ContextOpResult::Hits(
                self.rt.context.search(&self.company, &query, limit).await?,
            )),
        }
    }

    async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition> {
        self.gate_effect(effect).await
    }

    async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
        self.park(effect).await
    }
}

#[cfg(test)]
#[path = "cycle_tests_core.rs"]
mod tests_core;
#[cfg(test)]
#[path = "cycle_tests_core2.rs"]
mod tests_core2;
#[cfg(test)]
#[path = "cycle_tests_core3.rs"]
mod tests_core3;
#[cfg(test)]
#[path = "cycle_tests_part1.rs"]
mod tests_part1;
#[cfg(test)]
#[path = "cycle_tests_part10.rs"]
mod tests_part10;
#[cfg(test)]
#[path = "cycle_tests_part11.rs"]
mod tests_part11;
#[cfg(test)]
#[path = "cycle_tests_part12.rs"]
mod tests_part12;
#[cfg(test)]
#[path = "cycle_tests_part13.rs"]
mod tests_part13;
#[cfg(test)]
#[path = "cycle_tests_part2.rs"]
mod tests_part2;
#[cfg(test)]
#[path = "cycle_tests_part4.rs"]
mod tests_part4;
#[cfg(test)]
#[path = "cycle_tests_part5.rs"]
mod tests_part5;
#[cfg(test)]
#[path = "cycle_tests_part7.rs"]
mod tests_part7;
#[cfg(test)]
#[path = "cycle_tests_part8.rs"]
mod tests_part8;
#[cfg(test)]
#[path = "cycle_tests_part9.rs"]
mod tests_part9;
#[cfg(test)]
#[path = "cycle_tests_the_cycle_bracket_issue_390.rs"]
mod tests_the_cycle_bracket_issue_390;
#[cfg(test)]
#[path = "cycle_tests_what_the_card_says_issue_372.rs"]
mod tests_what_the_card_says_issue_372;
