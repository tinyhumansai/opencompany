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
use crate::ports::runs::{RunOutcome, RunStatus};
use crate::ports::tasks::{COLUMN_TODO, TaskRecord};
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
/// `<what the operator wrote>` + this marker + `<a list of card titles>`.
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
        // briefing of the desk's open work to the message text, so a "hi" sent
        // to a desk that is mid-task stops looking like "hi" one statement
        // later. Reading the events first is what makes the fast path fire on
        // exactly the messages it is for.
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
            if let Some(record) = &record {
                self.inject_handed_task_awareness(record, &mut events).await;
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
            cycle_conversation(&request.events, |id| {
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
    /// What it appends begins with [`OPEN_WORK_ANNOTATION`] — read that constant
    /// before adding any code downstream that reasons about an operator message,
    /// because after this runs the text is no longer only what the operator
    /// typed.
    async fn inject_handed_task_awareness(
        &self,
        record: &CompanyRecord,
        events: &mut [CompanyEvent],
    ) {
        // Cheap exit before touching the task store: nothing is addressed.
        if !events
            .iter()
            .any(|e| matches!(e, CompanyEvent::OperatorMessage { chat: Some(_), .. }))
        {
            return;
        }
        let cards = self.rt.tasks().list(&self.rt.id).await.unwrap_or_default();
        let open: Vec<&TaskRecord> = cards
            .iter()
            .filter(|c| c.column != "done" && !c.assignee.trim().is_empty())
            .collect();
        if open.is_empty() {
            return;
        }
        for event in events.iter_mut() {
            let CompanyEvent::OperatorMessage {
                text,
                chat: Some(target),
                ..
            } = event
            else {
                continue;
            };
            let mut lines: Vec<String> = open
                .iter()
                .filter(|c| assignment_matches(record, target.as_str(), &c.assignee))
                .map(|c| match &c.note {
                    Some(note) if !note.trim().is_empty() => {
                        format!("- {} — {}", c.title, first_line(note, 120))
                    }
                    _ => format!("- {}", c.title),
                })
                .collect();
            if lines.is_empty() {
                continue;
            }
            lines.sort();
            text.push_str(&format!(
                "{OPEN_WORK_ANNOTATION} (answer truthfully if asked what you are \
working on):\n{}\n]",
                lines.join("\n")
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
        let scope = crate::policy::consequence::standing_scope_of(&tool, &args);
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
fn sanitize_work_segment(thread: &str) -> Option<String> {
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
fn cycle_conversation(
    events: &[CompanyEvent],
    approval_conversation: impl Fn(&ApprovalId) -> Option<ApprovalConversation>,
) -> ApprovalConversation {
    // `(channel, thread-root-within-it)`. The channel is what rivals; the root
    // rides along and is demoted to `None` on disagreement — see above.
    let mut found: Option<(String, Option<EventSeq>)> = None;
    for event in events {
        let candidate = match event {
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
                Some((chat.clone(), *parent))
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
            .rt
            .approvals
            .park(&self.company, effect.clone())
            .await?;
        self.rt
            .journal
            .record_parked(
                &approval_id,
                &effect,
                now_millis(),
                TaskLink::from_task_id(self.task_id.as_deref()),
                // Which channel, and — issue #435 — where inside it, so the
                // continuation can be threaded back under the same root rather
                // than landing flat in the channel. Built as one value so the
                // pair cannot be written down describing two different places.
                ApprovalConversation {
                    thread: self.thread_id.clone(),
                    parent: self.thread_parent,
                },
                // Issue #469: which turn is blocked on this. Recorded here
                // because this is the one write path into the approval queue, so
                // the count the continuation queue keeps below cannot describe a
                // different set of approvals from the one that is parked.
                Some(self.cycle_id.clone()),
            )
            .await?;
        // Issue #796: a parked approval mints no grant until it resolves, so
        // until then neither grant map names this work unit. Mark it pending on
        // the shared grant set so an unrelated turn's `sweep_orphans` treats the
        // checkout this parked step is holding as live rather than orphaned. The
        // key is derived exactly as `approval_work_key` derives the grant's
        // `origin_task` (the card, else the sanitised thread), so the pending
        // mark and the grant it becomes name one unit; cleared when the approval
        // is settled or expires.
        if let Some(work) = self
            .task_id
            .clone()
            .or_else(|| self.thread_id.as_deref().and_then(sanitize_work_segment))
        {
            self.rt.grants.mark_pending(&approval_id, work);
        }
        // …and armed on the live counter in the same breath. A turn that parks
        // four calls is blocked on four decisions; the runtime holds its
        // continuation until the last of them lands and then runs it once.
        // Strictly after the journal write, so a crash between the two replays
        // as "still parked" and is re-armed by recovery rather than leaving a
        // counter for an approval no record describes.
        self.rt.continuations.arm(&self.cycle_id);
        // Issue #379: tell every subscribed console a request just parked, so an
        // inline card can appear in the conversation *as it happens* rather than
        // on the next poll of the approvals feed.
        //
        // Strictly **after** the journal write, and best-effort — the same
        // division `sweep_expired_approvals` draws. The journal is the binding
        // record of what is parked; the event is an advisory nudge, and a failed
        // log write must not undo a park that already happened (the queue would
        // then hold an effect no record describes). A console that misses the
        // frame still sees the approval on its next feed refresh.
        //
        // Deliberately **thin**: an id, a kind and a thread. The payload is not
        // here because `pending_approvals()` is the single place #372's
        // host-side redaction runs, and a payload-bearing durable event would
        // open a second surface that has to redact — and eventually will not.
        // The console reacts by refreshing the feed and renders from the
        // redacted summary. One round trip, on purpose.
        if let Err(err) = self
            .rt
            .events
            .append(
                &self.company,
                CompanyEvent::ApprovalParked {
                    approval_id: approval_id.clone(),
                    effect_kind: effect.kind.clone(),
                    thread: self.thread_id.clone(),
                },
            )
            .await
        {
            tracing::warn!(
                approval_id = %approval_id,
                error = %err,
                "approval parked and journaled, but its event-log entry failed",
            );
        }
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
            title: parsed.title.clone(),
            note: parsed.note,
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: parsed.assignee.unwrap_or_default(),
            updated_at_millis: now_millis(),
            origin_chat_id: None,
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
            title: first_line(&parsed.instruction, 80),
            note: Some(note),
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: desk_id.clone(),
            updated_at_millis: now_millis(),
            origin_chat_id: None,
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
mod test {
    use super::*;

    /// `single_agent` picks an agent slot only when the batch is one addressed
    /// operator message, and falls back to the whole-company lock otherwise —
    /// the invariant the per-agent lock leans on (issue: parallel agent turns).
    #[test]
    fn single_agent_picks_one_addressee_and_falls_back_otherwise() {
        fn op(chat: Option<&str>) -> (Option<EventSeq>, CompanyEvent) {
            (
                None,
                CompanyEvent::OperatorMessage {
                    text: "hi".to_string(),
                    by: None,
                    chat: chat.map(str::to_string),
                    parent: None,
                    deliverable: None,
                    mentions: Vec::new(),
                    attachments: Vec::new(),
                },
            )
        }

        // One message addressed to one agent -> that agent's slot.
        assert_eq!(
            single_agent(&[op(Some("frits"))]),
            Some("frits".to_string())
        );
        // Several messages, all the same agent -> still that agent's slot.
        assert_eq!(
            single_agent(&[op(Some("frits")), op(Some("frits"))]),
            Some("frits".to_string())
        );
        // Two different agents in one batch -> whole company.
        assert_eq!(single_agent(&[op(Some("frits")), op(Some("sjaan"))]), None);
        // Unaddressed message (routed to the orchestrator) -> whole company.
        assert_eq!(single_agent(&[op(None)]), None);
        // A non-operator event in the batch -> whole company.
        assert_eq!(
            single_agent(&[(
                None,
                CompanyEvent::TurnStarted {
                    turn_id: "t1".to_string(),
                    chat_id: "frits".to_string(),
                    parent: None,
                    by: None,
                },
            )]),
            None
        );
        // Empty batch -> whole company.
        assert_eq!(single_agent(&[]), None);
    }

    /// Issue #845: a `workflow` message reaches the brain carrying the builder
    /// briefing, and nothing else does.
    ///
    /// This is the fix for the mode actually observed on staging: the builder
    /// pass had already produced a proposal for `weekly-aeo-audit` while the
    /// desk agent answering the same message was telling the operator that it
    /// "cannot make it exist". The turn was right about its own toolset and
    /// wrong about the company, because nothing told it.
    ///
    /// The `chat` case is issue #1152's, and "nothing else does" is why it is
    /// pinned here rather than assumed: a "Just chatting" message opens no card,
    /// so there is no builder pass owning anything, so briefing the turn that a
    /// build is under way would be telling it something untrue. The injection
    /// matches `Workflow` exactly and this is what keeps it exact.
    #[test]
    fn only_a_workflow_message_gets_the_builder_briefing() {
        let msg = |deliverable| CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: "set up a weekly AEO audit".to_string(),
            by: None,
            chat: None,
            parent: None,
            deliverable,
            attachments: Vec::new(),
        };
        let text_of = |event: &CompanyEvent| match event {
            CompanyEvent::OperatorMessage { text, .. } => text.clone(),
            _ => unreachable!("fixture is an operator message"),
        };

        let mut events = vec![
            msg(Some(MessageIntent::Workflow)),
            msg(Some(MessageIntent::Once)),
            msg(None),
            msg(Some(MessageIntent::Chat)),
            // A non-operator event must be left entirely alone.
            CompanyEvent::ScheduleFired {
                cron: "0 6 * * 5".to_string(),
                prompt: "run the audit".to_string(),
            },
        ];
        CycleRunner::inject_workflow_builder_awareness(&mut events);

        let briefed = text_of(&events[0]);
        assert!(briefed.contains(BUILDER_ANNOTATION), "{briefed}");
        assert!(
            briefed.starts_with("set up a weekly AEO audit"),
            "the operator's own words come first, untouched: {briefed}"
        );
        // The whole point: the turn is told not to deny the capability.
        assert!(
            briefed.contains("do not report that you cannot"),
            "{briefed}"
        );

        for (i, label) in [(1, "once"), (2, "no choice"), (3, "chat")] {
            let text = text_of(&events[i]);
            assert_eq!(
                text, "set up a weekly AEO audit",
                "a `{label}` message must reach the brain exactly as typed"
            );
            assert!(
                !text.contains(BUILDER_ANNOTATION),
                "a `{label}` message must carry no builder briefing: {text}"
            );
        }
        assert!(matches!(events[4], CompanyEvent::ScheduleFired { .. }));
    }

    // ── Issue #1725: a bare greeting must not run the agentic loop ──

    /// The reported bug, end to end and at the level that costs money: "hi"
    /// reaches the cycle, an answer comes back, and **the brain is never
    /// called**.
    ///
    /// A turn count rather than a reply assertion, deliberately. The observed
    /// failure was not "the wording is wrong" — it was a greeting spending a
    /// full agentic turn (memory retrieval, a tool step, a long answer carried
    /// over from a task nobody had asked about). `CountingBrain` bills 4,500
    /// tokens and writes a memory trace per call, so a regression shows up as a
    /// call count, a metered spend and a trace that should not exist.
    #[tokio::test]
    async fn a_bare_greeting_answers_without_calling_the_brain() {
        let home_dir = tmp_home();
        let brain = Arc::new(CountingBrain::default());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
                .with_brain(brain.clone())
                .build()
                .await
                .unwrap(),
        );

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "hi".into(),
                by: Some(operator()),
                chat: None,
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            }])
            .await
            .unwrap();

        assert_eq!(
            brain.calls(),
            0,
            "a greeting must not spend a turn: the brain was called"
        );
        assert_eq!(
            report.responses.len(),
            1,
            "the operator still gets an answer"
        );
        let reply = &report.responses[0];
        assert_eq!(
            reply.text,
            crate::company::task_intent::SmallTalk::Hello.reply()
        );
        assert!(
            reply.steps.is_empty(),
            "no tool ran, so the timeline is empty: {:?}",
            reply.steps
        );
        // Issue #885: the reply is the company's, not the operator's.
        assert_eq!(
            reply.agent.as_deref(),
            Some("ceo"),
            "the greeting comes back in the voice the turn would have used"
        );
        // Nothing was written back for a later turn to retrieve.
        assert!(
            rt.memory
                .recent_traces(rt.id(), 8)
                .await
                .unwrap()
                .is_empty(),
            "a pleasantry must leave no memory behind it"
        );
    }

    /// The other half, and the one that matters more: everything that is not a
    /// bare pleasantry still runs the full turn.
    ///
    /// A fast path that swallowed a real request would answer "Hey! What can I
    /// help you with?" to "build the landing page" and drop the work on the
    /// floor — worse than the bug it fixes. Each case here is one of the
    /// conditions in `small_talk_result`, driven through the real cycle.
    #[tokio::test]
    async fn everything_that_is_not_a_pleasantry_still_runs_the_turn() {
        let home_dir = tmp_home();
        let brain = Arc::new(CountingBrain::default());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
                .with_brain(brain.clone())
                .build()
                .await
                .unwrap(),
        );

        let message =
            |text: &str, deliverable, attachments: Vec<crate::ports::types::Attachment>| {
                CompanyEvent::OperatorMessage {
                    text: text.into(),
                    by: Some(operator()),
                    chat: None,
                    parent: None,
                    deliverable,
                    mentions: Vec::new(),
                    attachments,
                }
            };
        let attached = vec![crate::ports::types::Attachment {
            node_id: "node-1".into(),
            name: "brief.pdf".into(),
            mime: "application/pdf".into(),
            size: 12,
            extracted_text: None,
        }];

        let cases = vec![
            (
                "a real request",
                message("build the landing page", None, Vec::new()),
            ),
            // A greeting with an ask under it is an ask.
            (
                "a greeting with an ask",
                message("hi, build the landing page", None, Vec::new()),
            ),
            // "yes" answering a teammate's question is an instruction.
            ("an acknowledgement", message("yes", None, Vec::new())),
            // The operator said, positively, that this message asks for work.
            (
                "an explicit work choice",
                message("hi", Some(MessageIntent::Once), Vec::new()),
            ),
            (
                "an explicit workflow choice",
                message("hi", Some(MessageIntent::Workflow), Vec::new()),
            ),
            // A file with "hi" over it is a request to look at the file.
            ("an attachment", message("hi", None, attached)),
        ];

        for (i, (label, event)) in cases.into_iter().enumerate() {
            rt.run_cycle(vec![event]).await.unwrap();
            assert_eq!(brain.calls(), i + 1, "{label} must still run a full turn");
        }
    }

    /// The conditions `small_talk_result` decides on its own, driven directly
    /// so the ones a live cycle cannot easily reach are still pinned: a
    /// confined workflow-copilot thread, a batch of more than one event, and
    /// who the reply is attributed to on an addressed thread.
    #[test]
    fn the_fast_path_declines_a_copilot_thread_and_a_batch() {
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "writer"
role = "Writer"

[[group_chat]]
id = "content"
name = "Content desk"
members = ["writer"]
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        };
        let hi = |chat: Option<&str>| CompanyEvent::OperatorMessage {
            text: "hi".into(),
            by: None,
            chat: chat.map(str::to_string),
            parent: None,
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        };

        // An addressed desk answers in that desk's lead's voice, not the
        // orchestrator's — the same routing `responder_for` does.
        let desk = small_talk_result(&record, &[hi(Some("content"))]).expect("a pleasantry");
        assert_eq!(desk.channel_responses[0].agent.as_deref(), Some("writer"));
        assert!(desk.token_usage.is_zero(), "no model was called");

        // Unaddressed falls to the orchestrator.
        let main = small_talk_result(&record, &[hi(None)]).expect("a pleasantry");
        assert_eq!(main.channel_responses[0].agent.as_deref(), Some("ceo"));

        // A workflow copilot thread is confined and answered by an ephemeral
        // agent this cannot speak as, so it declines (issue #416).
        assert!(
            small_talk_result(&record, &[hi(Some("workflow-copilot:weekly-aeo-audit"))]).is_none(),
            "a copilot thread keeps its confined turn"
        );

        // A batch is a scheduler tick or several messages at once; neither is
        // small talk, even when one of its members looks like it.
        assert!(small_talk_result(&record, &[hi(None), hi(None)]).is_none());
        assert!(
            small_talk_result(
                &record,
                &[CompanyEvent::ScheduleFired {
                    cron: "0 6 * * 5".into(),
                    prompt: "run the audit".into(),
                }]
            )
            .is_none()
        );
        assert!(small_talk_result(&record, &[]).is_none());

        // A company with nobody on the roster has no voice to answer in, so it
        // declines rather than journaling an unattributed bubble (issue #885).
        let mut empty = record.clone();
        empty.manifest.agents.clear();
        empty.manifest.group_chats.clear();
        assert!(small_talk_result(&empty, &[hi(None)]).is_none());
    }

    /// Issue #845, the wiring: the briefing actually reaches the brain.
    ///
    /// [`only_a_workflow_message_gets_the_builder_briefing`] pins what the
    /// injection *does* by calling it; this pins that `run_cycle` calls it. The
    /// two are separate failures — a correct injection nothing invokes leaves
    /// the bug exactly where it was — and only this one covers the wiring, so
    /// deleting the call site has to fail a test.
    ///
    /// `EffectBrain` echoes the text it was handed, so the reply is a faithful
    /// window onto what the brain actually saw.
    #[tokio::test]
    async fn the_builder_briefing_reaches_the_brain_through_run_cycle() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let effect = Effect {
            kind: "noop".into(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("full"))
                .with_brain(Arc::new(EffectBrain { effect }))
                .build()
                .await
                .unwrap(),
        );

        let ask = |deliverable| CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: "set up a weekly AEO audit".into(),
            by: None,
            chat: None,
            parent: None,
            deliverable,
            attachments: Vec::new(),
        };

        let workflow = rt
            .run_cycle(vec![ask(Some(MessageIntent::Workflow))])
            .await
            .unwrap();
        let seen = workflow
            .responses
            .iter()
            .map(|r| r.text.clone())
            .collect::<String>();
        assert!(
            seen.contains(BUILDER_ANNOTATION),
            "the brain must be told the builder owns this: {seen}"
        );

        // …and a one-off is handed through byte-for-byte, so the annotation is
        // not simply always on.
        let once = rt
            .run_cycle(vec![ask(Some(MessageIntent::Once))])
            .await
            .unwrap();
        let seen_once = once
            .responses
            .iter()
            .map(|r| r.text.clone())
            .collect::<String>();
        assert!(!seen_once.contains(BUILDER_ANNOTATION), "{seen_once}");
        assert!(
            seen_once.contains("set up a weekly AEO audit"),
            "{seen_once}"
        );
    }

    /// Issue #796: a DM's thread becomes a safe, `dm-`-prefixed branch segment
    /// `RepoManager::validate_task_segment` accepts; an empty or all-garbage
    /// thread yields nothing, so `repo_publish` refuses rather than build a
    /// broken ref.
    #[test]
    fn sanitize_work_segment_makes_a_safe_branch_segment() {
        // An already-safe thread is unchanged and keeps a readable key.
        assert_eq!(sanitize_work_segment("coder"), Some("dm-coder".into()));
        // Dots and underscores are already valid and survive.
        assert_eq!(sanitize_work_segment("a_b.c"), Some("dm-a_b.c".into()));
        // Nothing usable.
        assert_eq!(sanitize_work_segment(""), None);
        assert_eq!(sanitize_work_segment("///"), None);

        // When folding/trimming loses information, the readable body is kept and
        // a digest of the raw thread is appended so distinct threads never share
        // a work key. Colons, slashes and spaces fold to '-'; leading/trailing
        // separators are trimmed before the prefix.
        let folded = sanitize_work_segment("dm:coder/main x").unwrap();
        assert!(folded.starts_with("dm-dm-coder-main-x-"), "{folded}");
        let trimmed = sanitize_work_segment("--weird--").unwrap();
        assert!(trimmed.starts_with("dm-weird-"), "{trimmed}");

        // The collision the digest closes: two threads that fold to the same body
        // get distinct keys — and the digest is deterministic across calls.
        assert_ne!(
            sanitize_work_segment("coder/main"),
            sanitize_work_segment("coder-main"),
            "distinct threads must not share a work key"
        );
        assert_eq!(
            sanitize_work_segment("coder/main"),
            sanitize_work_segment("coder/main")
        );
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::company::CompanyManifest;
    use crate::company::runtime::CompanyMail;
    use crate::policy::ManifestApprovalGate;
    use crate::ports::ChannelAdapter;
    use crate::ports::brain::Brain;
    use crate::ports::types::SecretValue;
    use crate::ports::types::{
        ActorKind, ChunkAddr, ChunkHit, ChunkMeta, CompressedTrace, ContextChunk, CycleResult,
        EffectGroup, EventSeq, EvictionPolicy, ReplyTo, TaskResult, TokenUsage,
    };
    use crate::ports::{ContextStore, MemoryStore};
    use crate::runtime::RuntimeBuilder;
    use crate::runtime::channel::OperatorChannel;
    use crate::server::ops::mailer::RecordingMailSender;
    use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};
    use crate::store::paths::Bundle;
    use crate::store::{FsContextStore, FsMemoryStore};

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-cycle-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest(policy_mode: &str) -> CompanyManifest {
        let toml_src = format!(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "{policy_mode}"
            "#
        );
        toml::from_str(&toml_src).expect("parse manifest")
    }

    fn operator() -> Actor {
        Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        }
    }

    /// A brain that emits one caller-supplied effect on each `OperatorMessage`.
    struct EffectBrain {
        effect: Effect,
    }

    #[async_trait]
    impl Brain for EffectBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            let mut responses = Vec::new();
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    host.emit_effect(self.effect.clone()).await?;
                    responses.push(OutboundMessage {
                        message_id: None,
                        task_id: None,
                        channel: "operator".into(),
                        agent: None,
                        text: format!("handled: {text}"),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    });
                }
            }
            Ok(CycleResult {
                channel_responses: responses,
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "effect cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    #[derive(Default)]
    struct ExplicitExpiryBrain {
        decisions: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Brain for ExplicitExpiryBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                match event {
                    CompanyEvent::OperatorMessage { .. } => {
                        host.park_effect(harness_effect(
                            "finance",
                            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                            serde_json::json!({
                                "title": "Submit the filing",
                                "question": "May I submit it?"
                            }),
                        ))
                        .await?;
                    }
                    CompanyEvent::ApprovalResolved {
                        verdict: Verdict::Deny,
                        ..
                    } => {
                        self.decisions
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "explicit expiry")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A brain that counts how many times it was asked to think, and bills for
    /// it — the instrument for issue #1725, where the cost of a turn is the
    /// thing under test.
    #[derive(Default)]
    struct CountingBrain {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingBrain {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Brain for CountingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(CycleResult {
                channel_responses: vec![OutboundMessage {
                    message_id: None,
                    task_id: None,
                    channel: "operator".into(),
                    agent: Some("ceo".into()),
                    text: "a full turn ran".into(),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                }],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "a turn's memory")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage {
                    input: 4_000,
                    output: 500,
                    cached_input: 0,
                    cost_usd: 0.12,
                },
            })
        }
    }

    /// A brain that parks one caller-supplied effect per `OperatorMessage`
    /// through [`CycleHost::park_effect`] — the shape the harness brain produces
    /// when its openhuman policy blocked a tool call inside the turn (#172).
    struct ParkingBrain {
        effect: Effect,
    }

    #[async_trait]
    impl Brain for ParkingBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            let mut responses = Vec::new();
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    host.park_effect(self.effect.clone()).await?;
                    responses.push(OutboundMessage {
                        message_id: None,
                        task_id: None,
                        channel: "operator".into(),
                        agent: None,
                        text: format!("that needs your approval: {text}"),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    });
                }
            }
            Ok(CycleResult {
                channel_responses: responses,
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "parking cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A brain that answers on the operator channel and *also* emits a
    /// delegated reply addressed by agent id — the shape `run_delegation` and a
    /// dispatched card's post-back both produce.
    struct DelegatingBrain;

    #[async_trait]
    impl Brain for DelegatingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            Ok(CycleResult {
                channel_responses: vec![
                    OutboundMessage {
                        message_id: None,
                        task_id: None,
                        channel: "operator".into(),
                        agent: None,
                        text: "orchestrator".into(),
                        steps: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    },
                    OutboundMessage {
                        message_id: None,
                        task_id: None,
                        // Addressed by *agent id*: no adapter answers to this.
                        channel: "maya".into(),
                        agent: None,
                        text: "delegated reply".into(),
                        steps: Vec::new(),
                        reply_to: Some(ReplyTo {
                            chat_id: "strategy".into(),
                        }),
                        mentions: Vec::new(),
                    },
                ],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "delegating cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// Issue #151: a delegated reply is addressed by agent id, so no adapter
    /// matches it. It used to be dropped silently — the operator REST route
    /// never noticed because it reads `CycleReport.responses` directly, but a
    /// company reached over a channel adapter lost every delegated reply while
    /// still receiving the orchestrator's.
    #[tokio::test]
    async fn a_reply_addressed_by_agent_id_reaches_the_operator_channel() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let operator_channel = OperatorChannel::new();
        let channels: Vec<Arc<dyn ChannelAdapter>> = vec![Arc::new(operator_channel.clone())];
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(Arc::new(DelegatingBrain))
            .with_channels(channels)
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hand it off".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        let sent = operator_channel.sent();
        assert_eq!(sent.len(), 2, "both replies must be delivered: {sent:?}");
        // Attribution survives the fallback — the bubble still names the agent.
        let delegated = sent
            .iter()
            .find(|m| m.text == "delegated reply")
            .expect("the delegated reply must be delivered");
        assert_eq!(delegated.channel, "maya");
        assert_eq!(
            delegated.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
    }

    /// A brain that fails every cycle — the shape the terminality backstop has
    /// to cover, because a `?` on `run_cycle` would otherwise skip every settle
    /// and strand the attempt row `Running` until the next boot.
    struct FailingBrain;

    #[async_trait]
    impl Brain for FailingBrain {
        async fn run_cycle(
            &self,
            _req: CycleRequest,
            _host: &dyn CycleHost,
        ) -> Result<CycleResult> {
            Err(OpenCompanyError::Store("the brain fell over".into()))
        }
    }

    /// A brain that settles the dispatched run itself, the way `run_task` does
    /// on the harness path — so the backstop can be shown to leave a rich settle
    /// alone rather than racing it.
    struct SettlingBrain {
        runs: Arc<dyn crate::ports::RunStore>,
        status: RunStatus,
    }

    #[async_trait]
    impl Brain for SettlingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                if let CompanyEvent::TaskDispatched {
                    run_id: Some(run_id),
                    ..
                } = event
                {
                    let mut outcome = RunOutcome::new(self.status);
                    if self.status == RunStatus::Failed {
                        outcome = outcome.with_error("the brain said so");
                    }
                    self.runs
                        .finish_run(&req.company_id, run_id, outcome)
                        .await?;
                }
            }
            Ok(CycleResult {
                channel_responses: vec![OutboundMessage {
                    message_id: None,
                    task_id: None,
                    channel: "operator".into(),
                    agent: None,
                    text: "settled".into(),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                }],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "settling cycle")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// The rich settle always wins: `run_task` finishes the row *inside*
    /// `brain.run_cycle`, which is awaited before the backstop, so there is no
    /// race for the backstop to lose. Pinned rather than argued, because a
    /// backstop that overwrote a real outcome with a generic failure would be
    /// worse than no backstop at all.
    ///
    /// Both cases matter. A **terminal** settle must survive; so must a
    /// **parked** one — `Paused` and `WaitingApproval` are waiting on something
    /// outside the cycle, and reclaiming them would delete real pending work
    /// every time a cycle ended.
    #[tokio::test]
    async fn the_backstop_never_overwrites_a_settle_the_brain_already_made() {
        for (status, error) in [
            (RunStatus::Succeeded, None),
            (RunStatus::Paused, None),
            (RunStatus::WaitingApproval, None),
            (RunStatus::Failed, Some("the brain said so")),
        ] {
            let home_dir = tmp_home();
            let runs: Arc<dyn crate::ports::RunStore> =
                Arc::new(crate::store::FsOps::new(home_dir.path().to_path_buf()));
            let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
                .with_runs(Arc::clone(&runs))
                .with_brain(Arc::new(SettlingBrain {
                    runs: Arc::clone(&runs),
                    status,
                }))
                .build()
                .await
                .unwrap();
            let run_id = pending_run(&rt, "t-1").await;

            rt.run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: Some(run_id.clone()),
            }])
            .await
            .expect("cycle");

            let settled = rt
                .runs()
                .get_run(rt.id(), &run_id)
                .await
                .expect("read")
                .expect("row");
            assert_eq!(
                settled.status, status,
                "the backstop must not overwrite a {status} settle"
            );
            assert_eq!(settled.error.as_deref(), error);
        }
    }

    /// Mints a `Pending` run for `task`, so a test can drive a dispatch cycle
    /// the way `CompanyRuntime::dispatch_task` does.
    async fn pending_run(rt: &crate::company::runtime::CompanyRuntime, task: &str) -> String {
        rt.runs()
            .create_run(
                rt.id(),
                crate::ports::runs::NewRun::for_task(crate::ports::generate_id(), task, "ceo"),
            )
            .await
            .expect("mint a run")
            .id
    }

    /// Issue #242, the `begin_run` half: the run moves `Pending` → `Running`
    /// stamped with the **seq of the very `TaskDispatched` event that drove
    /// it**, and by the end of the cycle it is terminal rather than stranded —
    /// even though the default build's brain ignores `TaskDispatched` entirely
    /// and settles nothing.
    #[tokio::test]
    async fn a_dispatch_cycle_starts_its_run_and_never_leaves_it_claiming_to_be_live() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        let report = rt
            .run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: Some(run_id.clone()),
            }])
            .await
            .expect("the cycle itself succeeds");

        let run = rt
            .runs()
            .get_run(rt.id(), &run_id)
            .await
            .expect("read")
            .expect("the run survives its cycle");
        assert_eq!(
            run.trigger_event_seq, report.persisted_seq,
            "the run must name the exact log line that drove it"
        );
        assert!(
            run.started_at_millis.is_some(),
            "begin_run stamps when the attempt actually began"
        );
        assert_eq!(
            run.status,
            RunStatus::Failed,
            "an echo-brain dispatch produces no rich settle, so the backstop closes it"
        );
        assert_eq!(run.error.as_deref(), Some(RUN_UNSETTLED_ERROR));
        assert!(run.finished_at_millis.is_some());
    }

    /// Issue #337: the backstop settles the **card** as well as the row.
    ///
    /// Driven offline through the default build's echo brain, which ignores
    /// `TaskDispatched` entirely — so nothing produces a rich settle and the
    /// backstop is the only thing that can move anything. Before this, it
    /// closed the row and left the card in In Progress: the board claimed work
    /// that provably was not happening, and nothing would re-drive it, because
    /// `task_enters_in_progress` fires on the transition and that already
    /// happened.
    #[tokio::test]
    async fn the_backstop_returns_a_card_its_run_abandoned() {
        use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_TODO, TaskRecord};

        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();
        rt.tasks()
            .upsert(
                rt.id(),
                &TaskRecord {
                    id: "t-1".to_string(),
                    title: "Draft the spec".to_string(),
                    note: None,
                    column: COLUMN_IN_PROGRESS.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin_chat_id: None,
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
                    bounced: None,
                },
            )
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some(run_id),
        }])
        .await
        .expect("the cycle itself succeeds");

        let card = rt
            .tasks()
            .list(rt.id())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .expect("card");
        assert_eq!(
            card.column, COLUMN_TODO,
            "an unsettled attempt must not leave its card claiming to be worked"
        );
        let note = card.note.expect("the board must say why");
        assert!(note.contains(RUN_UNSETTLED_ERROR), "{note}");

        // The caller-level backstop must announce the same bounce through the
        // durable notification feed, not merely update the board.
        let notifications = rt
            .notifications()
            .list(rt.id(), "owner")
            .await
            .expect("read notifications");
        let notification = notifications
            .iter()
            .find(|n| n.notification.kind == "dispatch_failed")
            .expect("a bounced To-do card emits a dispatch-failed notification");
        assert_eq!(
            notification.notification.subject.id, "t-1",
            "the notification must point at the affected task"
        );
        assert!(
            notification
                .notification
                .title
                .contains(RUN_UNSETTLED_ERROR),
            "the notification must carry the failure reason: {:?}",
            notification.notification.title
        );
    }

    /// The guard, at the backstop: a card an operator has already parked is
    /// **not** dragged back to To-do by a late settle. The row still closes —
    /// the two are independent, and only one of them is the operator's.
    #[tokio::test]
    async fn the_backstop_leaves_a_parked_card_exactly_where_the_operator_put_it() {
        use crate::ports::tasks::{COLUMN_PAUSED, TaskRecord};

        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();
        rt.tasks()
            .upsert(
                rt.id(),
                &TaskRecord {
                    id: "t-1".to_string(),
                    title: "Draft the spec".to_string(),
                    note: Some("[operator] parked this".to_string()),
                    column: COLUMN_PAUSED.to_string(),
                    priority: "medium".to_string(),
                    assignee: "ceo".to_string(),
                    updated_at_millis: 1,
                    origin_chat_id: None,
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
                    bounced: None,
                },
            )
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some(run_id.clone()),
        }])
        .await
        .expect("the cycle itself succeeds");

        let card = rt
            .tasks()
            .list(rt.id())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .expect("card");
        assert_eq!(card.column, COLUMN_PAUSED);
        assert_eq!(
            card.note.as_deref(),
            Some("[operator] parked this"),
            "a refused move must not annotate the card either"
        );
        // …and the row is still closed, because bookkeeping is not the
        // operator's business.
        assert_eq!(
            rt.runs()
                .get_run(rt.id(), &run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RunStatus::Failed
        );
    }

    /// The other backstop arm: the brain **errored**. The cycle error still
    /// propagates to the caller (nothing is swallowed), and the row settles
    /// carrying that same reason instead of sitting `Running` forever.
    #[tokio::test]
    async fn a_failed_cycle_settles_its_run_and_still_reports_the_failure() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .with_brain(Arc::new(FailingBrain))
            .build()
            .await
            .unwrap();
        let run_id = pending_run(&rt, "t-1").await;

        let err = rt
            .run_cycle(vec![CompanyEvent::TaskDispatched {
                task_id: "t-1".into(),
                run_id: Some(run_id.clone()),
            }])
            .await
            .expect_err("a failing brain still fails the cycle");
        assert!(err.to_string().contains("the brain fell over"), "{err}");

        let run = rt
            .runs()
            .get_run(rt.id(), &run_id)
            .await
            .expect("read")
            .expect("run");
        assert_eq!(run.status, RunStatus::Failed);
        let reason = run.error.unwrap_or_default();
        assert!(reason.starts_with(RUN_CYCLE_FAILED_ERROR), "{reason}");
        assert!(
            reason.contains("the brain fell over"),
            "the row must carry the reason the caller saw: {reason}"
        );

        // …and the company-wide badge must NOT (CodeRabbit review on #1905).
        // The attempt row above is scoped to whoever can see the card; a
        // notification title is broadcast to every member, and
        // `advance::notify_dispatch_failed` only flattens newlines — so a
        // provider body quoting a key, a URL or a customer's name would go out
        // to the whole company. The badge names the class; the words stay on
        // the row and the card note.
        for filed in rt
            .notifications()
            .list(rt.id(), "owner")
            .await
            .expect("read notifications")
            .iter()
            .filter(|n| n.notification.kind == "dispatch_failed")
        {
            assert!(
                !filed.notification.title.contains("the brain fell over"),
                "the cycle error must not reach a company-wide title: {:?}",
                filed.notification.title
            );
            assert!(
                filed.notification.title.contains(RUN_CYCLE_FAILED_ERROR),
                "it still has to say what happened: {:?}",
                filed.notification.title
            );
        }
    }

    /// A dispatch whose run row could not be minted (`run_id: None`) — the
    /// documented degraded path — must still run the cycle normally. The
    /// dispatch is the work; the row is only the record of it.
    #[tokio::test]
    async fn an_untracked_dispatch_still_runs_its_cycle() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
        }])
        .await
        .expect("an untracked dispatch is still a dispatch");

        assert!(
            rt.runs()
                .list_runs(rt.id(), &crate::ports::runs::RunFilter::default())
                .await
                .expect("list")
                .is_empty(),
            "no row was minted, so none may be invented"
        );
    }

    /// A `run_id` naming a row that does not exist (a replayed journal line, a
    /// row lost with its store) must not fail the cycle either — and must not
    /// be tracked, so the backstop has nothing to settle.
    #[tokio::test]
    async fn a_dispatch_naming_an_unknown_run_does_not_fail_the_cycle() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::fs_defaults(home_dir.path().to_path_buf(), manifest("full"))
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: Some("run-that-never-was".into()),
        }])
        .await
        .expect("an unknown run id is a bookkeeping miss, not a cycle failure");
    }

    /// A [`MemoryStore`] that counts the calls a cycle makes, delegating the
    /// work to a real fs store so the runtime behaves normally around it.
    struct CountingMemory {
        inner: FsMemoryStore,
        reads: AtomicUsize,
        writes: AtomicUsize,
    }

    impl CountingMemory {
        fn new(inner: FsMemoryStore) -> Self {
            Self {
                inner,
                reads: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl MemoryStore for CountingMemory {
        async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.inner.save_trace(id, trace).await
        }

        async fn recent_traces(
            &self,
            id: &CompanyId,
            limit: usize,
        ) -> Result<Vec<CompressedTrace>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.recent_traces(id, limit).await
        }

        async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
            self.inner.save_task_result(id, result).await
        }

        async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
            self.inner.evict(id, policy).await
        }
    }

    /// The [`ContextStore`] half of the same instrument.
    struct CountingContext {
        inner: FsContextStore,
        lists: AtomicUsize,
    }

    impl CountingContext {
        fn new(inner: FsContextStore) -> Self {
            Self {
                inner,
                lists: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ContextStore for CountingContext {
        async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
            self.inner.put(id, chunk).await
        }

        async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.list(id, prefix).await
        }

        async fn peek(
            &self,
            id: &CompanyId,
            addr: &ChunkAddr,
            range: Option<std::ops::Range<usize>>,
        ) -> Result<String> {
            self.inner.peek(id, addr, range).await
        }

        async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>> {
            self.inner.search(id, query, limit).await
        }

        async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
            self.inner.delete(id, addr).await
        }

        async fn delete_label(
            &self,
            id: &CompanyId,
            addr: &ChunkAddr,
            label: &str,
        ) -> Result<bool> {
            self.inner.delete_label(id, addr, label).await
        }
    }

    /// Issue #1175: a cycle used to load 32 recent traces *and the whole context
    /// index* (`list(company, "")` — no prefix, no limit) into `CycleRequest`,
    /// where no brain read either. Both reads are gone, and the context one was
    /// the expensive half: it grew with every turn the company had ever run.
    ///
    /// The trace *write* deliberately stayed — traces travel with the export
    /// bundle — so this asserts the save as well. Without that half, a later
    /// "nothing reads traces, delete the write" would pass silently.
    #[tokio::test]
    async fn a_cycle_reads_neither_recent_traces_nor_the_context_index() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let memory = Arc::new(CountingMemory::new(FsMemoryStore::new(home.clone())));
        let context = Arc::new(CountingContext::new(FsContextStore::new(home.clone())));
        let rt = RuntimeBuilder::new(home, manifest("full"))
            .with_memory(memory.clone())
            .with_context(context.clone())
            .build()
            .await
            .unwrap();

        // Boot is not what this test is about; only what one cycle costs.
        memory.reads.store(0, Ordering::SeqCst);
        memory.writes.store(0, Ordering::SeqCst);
        context.lists.store(0, Ordering::SeqCst);

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            // Issue #1725: not "hi". A bare pleasantry is now answered without
            // a turn at all, and this test is about what a turn costs.
            text: "ship the landing page".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        assert_eq!(
            memory.reads.load(Ordering::SeqCst),
            0,
            "a cycle must not read traces back: no brain consumes them"
        );
        assert_eq!(
            context.lists.load(Ordering::SeqCst),
            0,
            "a cycle must not scan the context index: no brain consumes it"
        );
        assert_eq!(
            memory.writes.load(Ordering::SeqCst),
            1,
            "the trace write is not dead code — it feeds the export bundle"
        );
    }

    #[tokio::test]
    async fn end_to_end_operator_message_echoes_and_persists() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                // Issue #1725: not "hi". A bare pleasantry never reaches the
                // brain now, and the echo this asserts is the brain's.
                text: "ship the landing page".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();

        // (a) an operator response came back.
        assert_eq!(report.responses.len(), 1);
        assert_eq!(report.responses[0].channel, "operator");
        assert_eq!(report.responses[0].text, "You said: ship the landing page");

        // (b) the event was appended to the log.
        //
        // Filtered rather than counted: since issue #327 boot's workspace
        // scaffold journals a `WorkspaceChanged` per reserved root, so the log
        // already holds entries this test is not about.
        let stored = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        let operator: Vec<_> = stored
            .iter()
            .filter(|e| matches!(e.event, CompanyEvent::OperatorMessage { .. }))
            .collect();
        assert_eq!(operator.len(), 1);
        assert_eq!(
            operator[0].event,
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "ship the landing page".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }
        );

        // (c) a compressed trace was persisted.
        let traces = rt.memory.recent_traces(rt.id(), 10).await.unwrap();
        assert!(!traces.is_empty());
    }

    #[tokio::test]
    async fn effect_executes_at_most_once_across_reload() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
            .await
            .unwrap();

        let effect = Effect {
            kind: "x402.spend".into(),
            group: EffectGroup::Spend,
            amount_usd: Some(3.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };

        execute_effect_once(&rt, "k1", &effect, None).await.unwrap();
        // Same key again: skipped, no second ledger entry.
        execute_effect_once(&rt, "k1", &effect, None).await.unwrap();

        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert_eq!(record.ledger.len(), 1);

        // Rebuild the runtime over the same home; journal replay must remember
        // the executed key so a replayed effect does not run twice.
        let rt2 = RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
            .await
            .unwrap();
        assert!(rt2.journal.is_executed("k1"));
        execute_effect_once(&rt2, "k1", &effect, None)
            .await
            .unwrap();
        let record = rt2.store.load(rt2.id()).await.unwrap().unwrap();
        assert_eq!(record.ledger.len(), 1);
    }

    #[tokio::test]
    async fn supervised_effect_runs_without_policy_hitl() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(EffectBrain {
                    effect: sign_effect,
                }))
                .build()
                .await
                .unwrap(),
        );

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "file it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        assert!(report.parked.is_empty());
        assert!(rt.pending_approvals().is_empty());
    }

    // --- Single-use grants on approve (issue #243) ---------------------------

    /// A harness-projected effect, i.e. one carrying `agent`. Its payload is a
    /// tool's argument object, not something the runtime can perform.
    fn harness_effect(agent: &str, tool: &str, args: serde_json::Value) -> Effect {
        Effect {
            kind: tool.into(),
            group: EffectGroup::Sign,
            // A real spend amount, deliberately: it is what proves the effect
            // was NOT executed. `perform_effect` ledgers any `amount_usd`, so an
            // empty ledger is positive evidence that the native path was skipped
            // rather than merely evidence that nothing observable happened.
            amount_usd: Some(42.0),
            established_thread: false,
            first_time_counterparty: false,
            payload: args,
            agent: Some(agent.to_string()),
            run_id: None,
        }
    }

    /// Parks `effect` through a real cycle and returns the runtime + approval id.
    /// Returns the runtime behind an `Arc`, as the server's registry holds it:
    /// resolving an approval spawns its follow-up cycle onto a clone of that
    /// handle, so the cycle outlives the request that asked for it (issue #383).
    async fn park_one(
        home: std::path::PathBuf,
        effect: Effect,
    ) -> (Arc<CompanyRuntime>, ApprovalId) {
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain { effect }))
                .build()
                .await
                .unwrap(),
        );
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "do it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        assert_eq!(report.parked.len(), 1);
        let id = report.parked[0].clone();
        (rt, id)
    }

    /// Parks one tool call, then fails every follow-up turn.
    struct FailingContinuationBrain {
        effect: Effect,
    }

    #[async_trait]
    impl Brain for FailingContinuationBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                match event {
                    CompanyEvent::OperatorMessage { .. } => {
                        host.park_effect(self.effect.clone()).await?;
                    }
                    CompanyEvent::ApprovalResolved { .. } => {
                        return Err(OpenCompanyError::Unimplemented("the follow-up turn failed"));
                    }
                    _ => {}
                }
            }
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "failing continuation")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// Issue #383: a follow-up cycle that fails leaves a *recoverable* state,
    /// not a stranded one.
    ///
    /// Detaching the cycle means its failure has nowhere to be returned to, so
    /// the safety net has to be the ordering rather than the caller: the verdict
    /// is journaled and the grant minted before the turn is ever attempted, and
    /// re-approving is a no-op that mints no second grant (issue #243). This
    /// pins all three, so "the runtime logs it and the operator can retry" is a
    /// property of the code rather than a claim in a PR body.
    #[tokio::test]
    async fn a_failed_follow_up_cycle_leaves_the_verdict_and_grant_intact() {
        let home_dir = tmp_home();
        let effect = harness_effect("finance", "composio_execute", serde_json::json!({}));
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(Arc::new(FailingContinuationBrain {
                    effect: effect.clone(),
                }))
                .build()
                .await
                .unwrap(),
        );
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "do it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        let id = report.parked[0].clone();

        let failed = rt.resolve_approval(&id, Verdict::Approve, operator()).await;
        assert!(failed.is_err(), "the caller still learns the turn failed");

        // The operator's decision survived it.
        assert!(
            rt.pending_approvals().is_empty(),
            "the verdict was journaled before the turn was attempted"
        );
        assert!(rt.grants.peek(&id).is_some(), "and the grant was minted");
        assert_eq!(rt.grants.live_count(), 1);

        // Retrying is safe: a no-op report, and still exactly one grant.
        let again = rt
            .resolve_approval(&id, Verdict::Approve, operator())
            .await
            .expect("re-approving is a no-op, not a second failure");
        assert_eq!(
            again.responses[0].text,
            "This approval was already resolved."
        );
        assert_eq!(
            rt.grants.live_count(),
            1,
            "a retry after a failed continuation mints no second grant"
        );
    }

    /// The core of #243: approving an agent's blocked tool call mints a
    /// single-use grant and does **not** execute the effect.
    ///
    /// Executing it would be worse than useless. The payload is the tool's
    /// arguments, so `perform_effect` would ledger a spend for money nothing
    /// actually moved and route no message — the operator would see an approval
    /// marked done, a charge on the books, and no email sent. The grant is what
    /// makes approval mean "the agent may now really do this, once".
    #[tokio::test]
    async fn approving_a_harness_tool_call_mints_a_grant_instead_of_executing() {
        let home_dir = tmp_home();
        // Issue #470: a catalogued send, keyed the way `composio_execute`'s
        // schema keys it, with the action's parameters under `arguments`.
        let args = crate::policy::test_support::composio_args_with(
            crate::policy::test_support::COMPOSIO_SEND_SLUG,
            serde_json::json!({ "to": "a@b.test" }),
        );
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect("finance", "composio_execute", args.clone()),
        )
        .await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();

        // A grant exists, scoped to the agent, tool and exact arguments.
        let grant = rt.grants.peek(&id).expect("a grant was minted");
        assert_eq!(grant.agent, "finance");
        assert_eq!(grant.tool, "composio_execute");
        assert_eq!(grant.args, args);

        // ...and the effect was NOT executed: no ledger row, no journal key.
        let record = rt.store.load(rt.id()).await.unwrap().unwrap();
        assert!(
            record.ledger.is_empty(),
            "a harness tool call must not be performed natively — its payload is \
             arguments, so executing it books a spend for work that never happened"
        );
        assert!(!rt.journal.is_executed(&format!("approval:{id}")));
    }

    // --- What the card says (issue #372) ------------------------------------

    /// **Issue #1024.** The parked effect's consequence group reaches the card.
    ///
    /// A `GMAIL_SEND_EMAIL` gate sat parked for days and mailed a five-day-old
    /// digest the moment an operator cleared a backlog. The age was already on
    /// the card — as a bare "5d ago" in the footer, where it reads as how long
    /// the QUEUE has held the item rather than how old the PAYLOAD is. The
    /// console can only tell those apart for an effect that leaves the company,
    /// and it cannot work out which those are on its own: for a harness tool
    /// call `kind` is the TOOL NAME (`composio_execute`), not `email.send`, so
    /// a console keying on `kind` would miss exactly this send.
    ///
    /// So the host's own classification has to ride on the summary. This pins
    /// that it is the PARKED EFFECT's group and not a constant: a summary that
    /// hard-coded `Other` would render every outbound send as internal and put
    /// the bug straight back.
    #[tokio::test]
    async fn a_parked_effect_carries_its_group_to_the_card() {
        let home_dir = tmp_home();
        let mut effect = harness_effect(
            "devrel",
            "composio_execute",
            serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
        );
        // The group a real `composio_execute` of a send resolves to.
        effect.group = EffectGroup::Send;
        let (rt, _id) = park_one(home_dir.path().to_path_buf(), effect).await;

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].group,
            EffectGroup::Send,
            "the card must carry the parked effect's own group; anything constant \
             renders an outbound send as internal"
        );
        // And the tool name is NOT the discriminator — the reason the group has
        // to be sent at all.
        assert_eq!(pending[0].kind, "composio_execute");

        // It survives the wire: the console reads this field, so a summary that
        // classified correctly and serialized nothing would be no fix.
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&pending[0]).unwrap()).unwrap();
        assert_eq!(wire["group"], "send");
    }

    /// A harness-projected park reaches the operator naming its asker and what
    /// it will actually do — the whole point of #372, where the card used to say
    /// only "Shell".
    #[tokio::test]
    async fn a_harness_park_projects_its_agent_and_payload() {
        const FAKE_SECRET: &str = "NOT-A-REAL-KEY-planted-for-tests";
        let home_dir = tmp_home();
        let (rt, _id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect(
                "engineer",
                "shell",
                serde_json::json!({
                    "command": "./deploy.sh --staging",
                    "env": { "API_KEY": FAKE_SECRET },
                }),
            ),
        )
        .await;

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        let summary = &pending[0];
        assert_eq!(summary.agent.as_deref(), Some("engineer"));

        let payload = summary.payload.as_ref().expect("the arguments are carried");
        // The command is verbatim: it IS the thing being consented to.
        assert_eq!(payload["command"], "./deploy.sh --staging");
        // ...and the planted credential never leaves the host.
        let wire = serde_json::to_string(summary).unwrap();
        assert!(
            !wire.contains(FAKE_SECRET),
            "secret reached the wire: {wire}"
        );
        assert!(wire.contains(crate::runtime::approval_display::REDACTED));
    }

    /// A **native** effect the runtime performs itself names no asker, and an
    /// argument-less one carries no payload — so the card renders exactly as it
    /// did before #372 rather than inventing an agent. This is also the shape a
    /// journal-replayed pre-#243 park takes.
    #[tokio::test]
    async fn a_native_park_projects_no_agent_and_no_payload() {
        let home_dir = tmp_home();
        let (rt, _id) = park_one(
            home_dir.path().to_path_buf(),
            Effect {
                kind: "filing.submit".into(),
                group: EffectGroup::Sign,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::Value::Null,
                agent: None,
                run_id: None,
            },
        )
        .await;

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].agent.is_none());
        assert!(pending[0].payload.is_none());
    }

    /// The wire stays **additive**: absent fields are omitted entirely, so the
    /// JSON an old console receives is byte-identical to the pre-#372 shape and
    /// its unknown-key tolerance is never exercised.
    #[tokio::test]
    async fn absent_display_fields_are_omitted_from_the_wire() {
        let home_dir = tmp_home();
        let (rt, _id) = park_one(
            home_dir.path().to_path_buf(),
            Effect {
                kind: "filing.submit".into(),
                group: EffectGroup::Sign,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::Value::Null,
                agent: None,
                run_id: None,
            },
        )
        .await;

        let wire: serde_json::Value =
            serde_json::to_value(&rt.pending_approvals()[0]).expect("serializes");
        let keys: Vec<&str> = wire
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert!(!keys.contains(&"agent"), "agent leaked as null: {keys:?}");
        assert!(
            !keys.contains(&"payload"),
            "payload leaked as null: {keys:?}"
        );
    }

    /// Approve-with-edit mints against the **amended** arguments.
    ///
    /// Granting the original would let the agent re-issue the very call the
    /// operator edited, silently discarding the edit — worse than not supporting
    /// amend at all, because the operator has every reason to think their change
    /// took effect.
    #[tokio::test]
    async fn amending_an_approval_grants_the_edited_arguments() {
        let home_dir = tmp_home();
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect(
                "finance",
                "composio_execute",
                serde_json::json!({ "to": "wrong@b.test", "body": "hi" }),
            ),
        )
        .await;

        rt.resolve_approval_amended(&id, serde_json::json!({ "to": "right@b.test" }), operator())
            .await
            .unwrap();

        let grant = rt.grants.peek(&id).expect("a grant was minted");
        assert_eq!(
            grant.args,
            serde_json::json!({ "to": "right@b.test", "body": "hi" }),
            "the grant admits the operator's edit, overlaid onto the original"
        );
        // The un-edited call must NOT be redeemable.
        assert!(
            rt.grants
                .consume(
                    "finance",
                    "composio_execute",
                    &serde_json::json!({ "to": "wrong@b.test", "body": "hi" })
                )
                .is_none()
        );
    }

    /// A denied approval grants nothing. "No" must not leave a live permission
    /// behind for the agent to find.
    #[tokio::test]
    async fn denying_a_harness_tool_call_mints_nothing() {
        let home_dir = tmp_home();
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect("finance", "composio_execute", serde_json::json!({})),
        )
        .await;

        rt.resolve_approval(&id, Verdict::Deny, operator())
            .await
            .unwrap();

        assert!(rt.grants.peek(&id).is_none());
        assert_eq!(rt.grants.live_count(), 0);
    }

    #[tokio::test]
    async fn denying_an_explicit_request_mints_a_durable_decision_continuation() {
        let home_dir = tmp_home();
        let args = serde_json::json!({
            "title": "Submit the filing",
            "question": "May I submit it?"
        });
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                args.clone(),
            ),
        )
        .await;

        let summary = &rt.pending_approvals()[0];
        assert!(!summary.broadly_grantable);
        assert!(!summary.broadly_deniable);

        rt.resolve_approval(&id, Verdict::Deny, operator())
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while rt.grants.peek_continuation(&id).is_some()
                || !rt.journal.replayed_approval_continuations().is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("denial reaches its asker and retires the durable continuation");
        assert!(
            rt.journal
                .replayed_grants()
                .into_iter()
                .all(|grant| grant.approval_id != id),
            "a denied request must never replay as executable authority"
        );
        assert!(
            rt.journal.replayed_approval_continuations().is_empty(),
            "the delivered follow-up is consumed durably, so restart must not repeat its model \
             turn"
        );
    }

    #[tokio::test]
    async fn an_explicit_question_refuses_a_standing_scope() {
        let home_dir = tmp_home();
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({
                    "title": "Submit the filing",
                    "question": "May I submit it?"
                }),
            ),
        )
        .await;

        let error = rt
            .resolve_approval_spawned(&id, Verdict::Deny, operator(), tool_scope())
            .await
            .expect_err("the question tool is not the proposed action");

        assert!(error.to_string().contains("can only be decided once"));
        assert_eq!(rt.pending_approvals().len(), 1);
        assert!(rt.standing_grants().is_empty());
        assert!(rt.grants.peek_continuation(&id).is_none());
    }

    #[tokio::test]
    async fn an_explicit_question_without_an_agent_stays_pending_with_an_error() {
        let home_dir = tmp_home();
        let mut effect = harness_effect(
            "finance",
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
            serde_json::json!({
                "title": "Submit the filing",
                "question": "May I submit it?"
            }),
        );
        effect.agent = None;
        let (rt, id) = park_one(home_dir.path().to_path_buf(), effect).await;

        let error = rt
            .resolve_approval(&id, Verdict::Deny, operator())
            .await
            .expect_err("an explicit request must name the agent to resume");

        assert!(error.to_string().contains("missing its requesting agent"));
        assert_eq!(rt.pending_approvals().len(), 1);
    }

    #[tokio::test]
    async fn an_explicit_question_refuses_approve_with_edit() {
        let home_dir = tmp_home();
        let (rt, id) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({
                    "title": "Submit the filing",
                    "question": "May I submit it?"
                }),
            ),
        )
        .await;

        let error = rt
            .resolve_approval_amended(
                &id,
                serde_json::json!({ "question": "May I submit it tomorrow?" }),
                operator(),
            )
            .await
            .expect_err("a question carries no executable payload to edit");

        assert!(error.to_string().contains("no executable payload to amend"));
        assert_eq!(rt.pending_approvals().len(), 1);
        assert!(rt.grants.peek_continuation(&id).is_none());
    }

    #[tokio::test]
    async fn recovery_schedules_a_durable_explicit_decision_continuation() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (rt, id) = park_one(
            home.clone(),
            harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({
                    "title": "Submit the filing",
                    "question": "May I submit it?"
                }),
            ),
        )
        .await;

        CycleRunner::new(&rt)
            .settle_approval(&id, Verdict::Approve, operator(), GrantScope::Once)
            .await
            .unwrap();
        drop(rt);

        let brain = Arc::new(CountingBrain::default());
        let recovered = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_brain(brain.clone())
                .build()
                .await
                .unwrap(),
        );
        let registry = crate::runtime::CompanyRegistry::new();
        registry.insert(recovered.id().clone(), recovered.clone());

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while brain.calls() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovery dispatches the owed follow-up");
        assert_eq!(brain.calls(), 1);
    }

    #[tokio::test]
    async fn shutdown_registration_defers_replayed_approval_work_to_next_boot() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (rt, id) = park_one(
            home.clone(),
            harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({
                    "title": "Submit the filing",
                    "question": "May I submit it?"
                }),
            ),
        )
        .await;
        CycleRunner::new(&rt)
            .settle_approval(&id, Verdict::Approve, operator(), GrantScope::Once)
            .await
            .unwrap();
        drop(rt);

        let brain = Arc::new(CountingBrain::default());
        let recovered = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_brain(brain.clone())
                .build()
                .await
                .unwrap(),
        );
        let registry = crate::runtime::CompanyRegistry::new();
        registry.begin_shutdown();
        registry.insert(recovered.id().clone(), recovered.clone());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(recovered.is_quiesced());
        assert_eq!(brain.calls(), 0);
        assert_eq!(recovered.journal.replayed_approval_continuations().len(), 1);
    }

    #[tokio::test]
    async fn explicit_dispatch_claim_waits_for_every_sibling_decision() {
        let home_dir = tmp_home();
        let brain = Arc::new(CountingBrain::default());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(brain.clone())
                .build()
                .await
                .unwrap(),
        );
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "explicit-batch".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );
        let first = host
            .park_effect(harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({ "title": "First", "question": "First?" }),
            ))
            .await
            .unwrap();
        let second = host
            .park_effect(harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({ "title": "Second", "question": "Second?" }),
            ))
            .await
            .unwrap();

        rt.resolve_approval(&first, Verdict::Deny, operator())
            .await
            .unwrap();
        assert_eq!(brain.calls(), 0, "one sibling is still pending");
        assert_eq!(rt.journal.replayed_approval_continuations().len(), 1);

        rt.resolve_approval(&second, Verdict::Deny, operator())
            .await
            .unwrap();
        assert_eq!(brain.calls(), 1, "the released batch runs one follow-up");
        assert!(rt.journal.replayed_approval_continuations().is_empty());
    }

    #[tokio::test]
    async fn a_denied_explicit_request_from_a_workflow_node_returns_to_its_agent() {
        let home_dir = tmp_home();
        let brain = Arc::new(CountingBrain::default());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(brain.clone())
                .build()
                .await
                .unwrap(),
        );
        let turn = crate::runtime::workflow_resume::workflow_node_turn_key("run-1", "work");
        let host = CycleHostImpl::new(
            rt.id().clone(),
            turn,
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );
        let id = host
            .park_effect(harness_effect(
                "finance",
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                serde_json::json!({
                    "title": "Submit the filing",
                    "question": "May I submit it?"
                }),
            ))
            .await
            .unwrap();

        rt.resolve_approval(&id, Verdict::Deny, operator())
            .await
            .unwrap();

        assert_eq!(
            brain.calls(),
            1,
            "the workflow-node fork must deliver the denial as an agent continuation"
        );
    }

    /// An approval that expired past its TTL grants nothing either, even though
    /// the operator clicked approve — default-deny-on-silence wins, and it must
    /// win here too or expiry would become a way to smuggle a live grant out of
    /// a stale approval.
    #[tokio::test]
    async fn an_expired_approval_mints_nothing_even_on_approve() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let gate = Arc::new(
            ManifestApprovalGate::new(manifest("supervised").policy.clone()).with_ttl_millis(0),
        );
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_approvals(gate)
                .with_brain(Arc::new(EffectBrain {
                    effect: harness_effect("finance", "composio_execute", serde_json::json!({})),
                }))
                .build()
                .await
                .unwrap(),
        );
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "do it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        let id = report.parked[0].clone();

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert_eq!(
            rt.grants.live_count(),
            0,
            "an expired approval is a deny, so it hands out no permission"
        );
    }

    /// Parks one harness tool call behind a **zero-TTL** gate, so it is past its
    /// deadline the instant it lands — the state an operator meets when they get
    /// to the queue late (issue #1449).
    async fn park_one_past_its_deadline(
        home: std::path::PathBuf,
    ) -> (Arc<CompanyRuntime>, ApprovalId) {
        let gate = Arc::new(
            ManifestApprovalGate::new(manifest("supervised").policy.clone()).with_ttl_millis(0),
        );
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_approvals(gate)
                .with_brain(Arc::new(EffectBrain {
                    effect: harness_effect(
                        "finance",
                        "composio_execute",
                        serde_json::json!({ "to": "a@b.test" }),
                    ),
                }))
                .build()
                .await
                .unwrap(),
        );
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "do it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        assert_eq!(report.parked.len(), 1);
        let id = report.parked[0].clone();
        (rt, id)
    }

    /// **The assertion this whole fix exists for** (issue #1449).
    ///
    /// The safety half was always right — an expired approval mints nothing, and
    /// [`an_expired_approval_mints_nothing_even_on_approve`] pins that. What was
    /// missing was the *reporting* half: the arm fell through to
    /// `record_resolved`, so the immutable journal said **a named operator
    /// approved this** about a call the host had already refused. That is a
    /// false statement about a person, written permanently, on the surface whose
    /// entire job is answering "who authorised this?".
    ///
    /// So: after a late approve, the journal must carry the expiry — the same
    /// line the sweeper writes when the identical outcome is reached by silence
    /// — and must carry **no** `ApprovalResolved` for that id at all.
    #[tokio::test]
    async fn a_late_approve_is_journaled_as_an_expiry_never_as_the_operators_verdict() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (rt, id) = park_one_past_its_deadline(home.clone()).await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();

        let raw = tokio::fs::read_to_string(Bundle::new(&home, rt.id()).journal_jsonl())
            .await
            .unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let about_this_approval = |record: &str| {
            lines.iter().any(|line| {
                line["record"] == record && line["id"] == serde_json::json!(id.as_ref() as &str)
            })
        };

        assert!(
            about_this_approval("ApprovalExpired"),
            "a late click leaves the SAME record as a deadline nobody noticed, got {raw}"
        );
        assert!(
            !about_this_approval("ApprovalResolved"),
            "the journal must never say this operator resolved an approval the \
             host had already default-denied, got {raw}"
        );
        assert!(
            !about_this_approval("ApprovalAmended"),
            "and it must not record an amendment either, got {raw}"
        );
        // The safety half, re-checked here rather than assumed: reporting the
        // truth is only half a fix if the grant came back.
        assert_eq!(rt.grants.live_count(), 0);
        assert!(rt.pending_approvals().is_empty());
    }

    /// The event log — what the brain and the operator's SSE feed read — must
    /// agree with the journal (issue #1449).
    ///
    /// Before this it received `ApprovalResolved { verdict: Approve, by: <the
    /// operator> }`, so the agent was re-dispatched to make a call it had never
    /// been granted, and the timeline named a person who approved nothing. An
    /// expiry is a default-**deny** by the **system**, exactly as the sweeper
    /// appends it.
    #[tokio::test]
    async fn a_late_approve_appends_a_system_deny_not_the_operators_approve() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_past_its_deadline(home_dir.path().to_path_buf()).await;

        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();

        let events = rt
            .events()
            .read_from(rt.id(), EventSeq::new(0), usize::MAX)
            .await
            .unwrap();
        let resolutions: Vec<_> = events
            .iter()
            .filter_map(|stored| match &stored.event {
                CompanyEvent::ApprovalResolved {
                    approval_id,
                    verdict,
                    by,
                } if approval_id == &id => Some((*verdict, by.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            resolutions.len(),
            1,
            "exactly one resolution event, got {resolutions:?}"
        );
        assert_eq!(resolutions[0].0, Verdict::Deny);
        assert_eq!(
            resolutions[0].1.kind,
            ActorKind::System,
            "the deadline decided this, not the person who clicked"
        );
    }

    /// The receipt says which end state was reached, so the HTTP layer can too.
    #[tokio::test]
    async fn a_late_approve_answers_with_an_expired_receipt() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_past_its_deadline(home_dir.path().to_path_buf()).await;

        let (receipt, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, operator(), GrantScope::Once)
            .await
            .unwrap();
        assert_eq!(receipt.outcome(), "expired");
        assert!(receipt.expired());
        assert!(
            !receipt.already_resolved(),
            "the approval WAS parked — it ran out, which is a different answer \
             from somebody else having decided it"
        );
        // And it owes no continuation of its own: `retire_approval` already
        // released the turn.
        let report = crate::company::runtime::join_follow_up(follow_up)
            .await
            .unwrap();
        assert!(report.responses[0].text.contains("deadline"));

        // A second click on the same card is now the ordinary already-gone case.
        let (again, _) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, operator(), GrantScope::Once)
            .await
            .unwrap();
        assert_eq!(again.outcome(), "already_resolved");
    }

    /// The amend half of the same defect: an edit applied after the deadline is
    /// still not a decision, and recorded an `ApprovalAmended` on top of the
    /// false approval before this (issue #1449).
    #[tokio::test]
    async fn a_late_amend_records_an_expiry_and_no_amendment() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (rt, id) = park_one_past_its_deadline(home.clone()).await;

        let (receipt, _) = rt
            .resolve_approval_amended_spawned(
                &id,
                serde_json::json!({ "to": "elsewhere@b.test" }),
                operator(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.outcome(), "expired");

        let raw = tokio::fs::read_to_string(Bundle::new(&home, rt.id()).journal_jsonl())
            .await
            .unwrap();
        assert!(raw.contains("ApprovalExpired"));
        assert!(
            !raw.contains("ApprovalAmended"),
            "an edit the host refused is not an amendment the operator made, got {raw}"
        );
        assert!(
            !raw.contains("elsewhere@b.test"),
            "and the edited arguments must not be recorded as approved, got {raw}"
        );
        assert_eq!(rt.grants.live_count(), 0);
    }

    /// A live grant survives a restart; a consumed one does not come back.
    ///
    /// The window between approve and re-issue spans a model turn, so a deploy
    /// inside it is ordinary — and a resurrected single-use grant is no longer
    /// single-use.
    #[tokio::test]
    async fn grants_replay_on_boot_but_a_spent_one_does_not() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let args = serde_json::json!({ "to": "a@b.test" });
        let (rt, id) = park_one(
            home.clone(),
            harness_effect("finance", "composio_execute", args.clone()),
        )
        .await;
        rt.resolve_approval(&id, Verdict::Approve, operator())
            .await
            .unwrap();
        drop(rt);

        // Restart: the grant comes back.
        let rt2 = RuntimeBuilder::fs_defaults(home.clone(), manifest("supervised"))
            .await
            .unwrap();
        assert_eq!(rt2.grants.live_count(), 1);
        // Redeem it and journal the consumption the way a cycle would.
        assert!(
            rt2.grants
                .consume("finance", "composio_execute", &args)
                .is_some()
        );
        for spent in rt2.grants.drain_consumed() {
            rt2.journal
                .record_grant_consumed(&spent, None)
                .await
                .unwrap();
        }
        drop(rt2);

        // Restart again: the spent grant stays spent.
        let rt3 = RuntimeBuilder::fs_defaults(home, manifest("supervised"))
            .await
            .unwrap();
        assert_eq!(
            rt3.grants.live_count(),
            0,
            "a redeemed grant must not be re-armed by a restart"
        );
    }

    /// Issue #243: a grant the agent never redeemed expires, is journaled, and
    /// the operator is TOLD.
    ///
    /// The silent version of this is the failure worth designing against: the
    /// operator approves, watches nothing happen, and has no way to tell whether
    /// the work is in flight, already done, or quietly dead. Announcing the lapse
    /// is what makes re-approving an informed choice rather than a guess.
    #[tokio::test]
    async fn an_unredeemed_grant_expires_journals_and_tells_the_operator() {
        let home_dir = tmp_home();
        let operator_channel = Arc::new(crate::runtime::channel::OperatorChannel::new());
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
            .with_channels(vec![operator_channel.clone()])
            .build()
            .await
            .unwrap();

        // `at_millis: 0` is unambiguously past the 15-minute TTL.
        rt.grants.grant(GrantedCall {
            approval_id: ApprovalId::new("appr-stale"),
            agent: "finance".into(),
            tool: "composio_execute".into(),
            args: serde_json::json!({ "to": "a@b.test" }),
            at_millis: 0,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
        });
        // A fresh one, to prove the sweep is selective rather than a flush.
        rt.grants.grant(GrantedCall {
            approval_id: ApprovalId::new("appr-fresh"),
            agent: "finance".into(),
            tool: "workspace_write".into(),
            args: serde_json::json!({}),
            at_millis: now_millis(),
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
        });

        let expired = rt.sweep_expired_grants().await.unwrap();
        assert_eq!(expired, vec![ApprovalId::new("appr-stale")]);
        assert_eq!(rt.grants.live_count(), 1, "the fresh grant is untouched");
        assert!(rt.grants.peek(&ApprovalId::new("appr-fresh")).is_some());

        // The operator was told, and told which tool and which agent — enough to
        // decide whether to re-approve without going digging.
        let sent = operator_channel.sent();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].text.contains("composio_execute"),
            "{}",
            sent[0].text
        );
        assert!(sent[0].text.contains("finance"), "{}", sent[0].text);
        assert!(sent[0].text.contains("re-approve"), "{}", sent[0].text);

        // The expiry is durable: a restart must not hand the permission back.
        assert!(
            rt.journal
                .replayed_grants()
                .iter()
                .all(|g| g.approval_id != ApprovalId::new("appr-stale"))
        );
    }

    /// Issue #243: resolving an approval that is already gone is a no-op, not a
    /// second resolution.
    ///
    /// A double-clicked approve, a retried request, or two operators on the same
    /// queue all hit this. Before the outcome enum, the second call could not be
    /// told apart from a deny: the gate returned `None` either way, so the
    /// runner appended a second `ApprovalResolved` journal record AND ran a
    /// second follow-up cycle — a whole model turn spent re-announcing a
    /// resolution the brain had already been given.
    #[tokio::test]
    async fn resolving_an_already_resolved_approval_is_a_deterministic_no_op() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain {
                    effect: sign_effect,
                }))
                .build()
                .await
                .unwrap(),
        );

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "file it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        let approval_id = report.parked[0].clone();

        rt.resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();
        let events_after_first = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 1000)
            .await
            .unwrap()
            .len();

        // The second submit.
        let again = rt
            .resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();

        assert_eq!(again.responses.len(), 1);
        assert_eq!(
            again.responses[0].text, "This approval was already resolved.",
            "the operator gets a deterministic line, not an error and not a re-run"
        );
        assert!(again.executed_effects.is_empty());
        assert!(again.parked.is_empty());
        assert!(
            again.persisted_seq.is_none(),
            "a no-op must not claim to have persisted anything"
        );
        assert_eq!(
            rt.events
                .read_from(rt.id(), EventSeq::new(0), 1000)
                .await
                .unwrap()
                .len(),
            events_after_first,
            "no second ApprovalResolved event, and no follow-up cycle behind it"
        );
    }

    /// Issue #172: an already-decided approval request parks and reaches the
    /// operator's queue **without** being re-evaluated.
    ///
    /// The company runs `full` autonomy and the effect classifies as `Other` —
    /// the two conditions under which `ApprovalGate::evaluate` returns `Allow`.
    /// Had the request gone through `emit_effect` it would have been "executed"
    /// as a no-op and vanished, which is exactly how a chat-gated tool call used
    /// to disappear before ever reaching the Approvals page.
    #[tokio::test]
    async fn a_decided_request_parks_without_being_re_evaluated() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let tool_effect = Effect {
            kind: "composio_execute".into(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: crate::policy::test_support::composio_send_args(),
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(ParkingBrain {
                effect: tool_effect.clone(),
            }))
            .build()
            .await
            .unwrap();

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "send that email".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();

        assert_eq!(report.parked.len(), 1, "the request parked");
        assert!(
            report.executed_effects.is_empty(),
            "a parked request must not execute"
        );

        // The Approvals page reads exactly this.
        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1, "the operator sees the request");
        assert_eq!(pending[0].kind, "composio_execute");
        assert_eq!(pending[0].id, report.parked[0]);

        // And it is durable: a fresh runtime over the same home replays it, so a
        // restart does not lose what the operator still owes an answer to.
        let rt2 = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(ParkingBrain {
                effect: tool_effect,
            }))
            .build()
            .await
            .unwrap();
        assert_eq!(rt2.pending_approvals().len(), 1);
    }

    #[tokio::test]
    async fn approval_survives_runtime_restart() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let approval_id = {
            let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain {
                    effect: sign_effect.clone(),
                }))
                .build()
                .await
                .unwrap();
            let report = rt
                .run_cycle(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "file it".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }])
                .await
                .unwrap();
            report.parked[0].clone()
        };

        // A fresh runtime over the same home rehydrates the parked approval and
        // can resolve it by its original id.
        let rt2 = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain {
                    effect: sign_effect,
                }))
                .build()
                .await
                .unwrap(),
        );
        assert_eq!(rt2.pending_approvals().len(), 1);
        rt2.resolve_approval(&approval_id, Verdict::Deny, operator())
            .await
            .unwrap();
        assert!(rt2.pending_approvals().is_empty());
    }

    #[tokio::test]
    async fn amend_then_approve_executes_edited_effect() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        // A parked Sign effect whose payload the operator will overwrite so the
        // executed effect routes an amended message to the operator channel.
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({ "channel": "operator", "text": "ORIGINAL" }),
            agent: None,
            run_id: None,
        };
        // A recording operator channel we keep a handle to (Arc-shared buffer).
        let operator_channel = OperatorChannel::new();
        let channels: Vec<Arc<dyn ChannelAdapter>> = vec![Arc::new(operator_channel.clone())];
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain {
                    effect: sign_effect,
                }))
                .with_channels(channels)
                .build()
                .await
                .unwrap(),
        );

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "file it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        let approval_id = report.parked[0].clone();

        // Approve with an edited payload: only `text` changes.
        let follow_up = rt
            .resolve_approval_amended(
                &approval_id,
                serde_json::json!({ "text": "AMENDED" }),
                operator(),
            )
            .await
            .unwrap();
        assert!(follow_up.parked.is_empty());
        assert!(rt.pending_approvals().is_empty());

        // The amended effect executed: the operator channel saw "AMENDED",
        // never the original "ORIGINAL" text.
        let sent = operator_channel.sent();
        assert!(
            sent.iter().any(|m| m.text == "AMENDED"),
            "amended text was routed, got {sent:?}"
        );
        assert!(sent.iter().all(|m| m.text != "ORIGINAL"));

        // The immutable journal records both the original park and the amend.
        let raw = tokio::fs::read_to_string(Bundle::new(&home, rt.id()).journal_jsonl())
            .await
            .unwrap();
        assert!(raw.contains("ApprovalParked"));
        assert!(raw.contains("ApprovalAmended"));
        assert!(raw.contains("AMENDED"));
    }

    #[tokio::test]
    async fn sweep_expires_parked_approval_to_deny() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sign_effect = Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        // A zero-TTL gate: anything parked is immediately past its deadline.
        let gate = Arc::new(
            ManifestApprovalGate::new(manifest("supervised").policy.clone()).with_ttl_millis(0),
        );
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .with_brain(Arc::new(EffectBrain {
                    effect: sign_effect,
                }))
                .with_approvals(gate)
                .build()
                .await
                .unwrap(),
        );

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "file it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        let approval_id = report.parked[0].clone();
        assert_eq!(rt.pending_approvals().len(), 1);

        // The maintenance sweep resolves the silent approval to a default-deny.
        let expired = rt.sweep_expired_approvals().await.unwrap();
        assert_eq!(expired, vec![approval_id]);
        assert!(rt.pending_approvals().is_empty());

        let raw = tokio::fs::read_to_string(Bundle::new(&home, rt.id()).journal_jsonl())
            .await
            .unwrap();
        assert!(raw.contains("ApprovalExpired"));
    }

    #[tokio::test]
    async fn an_expired_explicit_request_returns_a_system_denial_to_its_agent() {
        let home_dir = tmp_home();
        let gate = Arc::new(
            ManifestApprovalGate::new(manifest("supervised").policy.clone()).with_ttl_millis(0),
        );
        let brain = Arc::new(ExplicitExpiryBrain::default());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(brain.clone())
                .with_approvals(gate)
                .build()
                .await
                .unwrap(),
        );

        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "file it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        assert_eq!(report.parked.len(), 1);
        assert_eq!(rt.continuations.outstanding(&report.cycle_id), 1);

        rt.sweep_expired_approvals().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while brain.decisions.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the expiry denial reaches the asking agent; outstanding={}, continuation_live={}",
                rt.continuations.outstanding(&report.cycle_id),
                rt.grants.peek_continuation(&report.parked[0]).is_some()
            )
        });
        assert_eq!(brain.decisions.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── Issue #174: the generic cycle seam meters inference usage ────────────

    /// A brain that reports a fixed [`TokenUsage`] for every cycle — the shape
    /// hosted Medulla cognition produces once its `orch:usage` frames land.
    struct MeteredBrain {
        usage: TokenUsage,
        metering: UsageMetering,
    }

    impl MeteredBrain {
        fn per_cycle(usage: TokenUsage) -> Self {
            Self {
                usage,
                metering: UsageMetering::PerCycle,
            }
        }
    }

    #[async_trait]
    impl Brain for MeteredBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            Ok(CycleResult {
                channel_responses: vec![OutboundMessage {
                    message_id: None,
                    task_id: None,
                    channel: "operator".into(),
                    agent: None,
                    text: "thought about it".into(),
                    steps: Vec::new(),
                    reply_to: None,
                    mentions: Vec::new(),
                }],
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "metered cycle")],
                ledger_deltas: Vec::new(),
                token_usage: self.usage,
            })
        }

        fn cognition(&self) -> crate::ports::Cognition {
            crate::ports::Cognition {
                path: "test",
                provider: "medulla",
                model: None,
                metering: self.metering,
            }
        }
    }

    fn reported_usage(cost_usd: f64) -> TokenUsage {
        TokenUsage {
            input: 1_200,
            output: 340,
            cached_input: 200,
            cost_usd,
        }
    }

    /// The bug: a brain outside the openhuman harness reported real token usage
    /// and the cycle loop dropped it, so the Usage view read zero forever.
    #[tokio::test]
    async fn reported_cycle_usage_reaches_the_usage_meter() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(reported_usage(0.031))))
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "how are we doing".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        let samples = rt.usage().query(rt.id(), 0).await.unwrap();
        assert_eq!(samples.len(), 1, "one inference sample per metered cycle");
        let sample = &samples[0];
        assert_eq!(sample.kind, crate::ports::usage::SampleKind::Inference);
        assert_eq!(sample.input_tokens, 1_200);
        assert_eq!(sample.output_tokens, 340);
        assert_eq!(sample.cached_input_tokens, 200);
        assert_eq!(sample.cost_usd, 0.031);
        assert_eq!(sample.provider, "medulla");
        assert_eq!(sample.agent, crate::metering::UNATTRIBUTED_AGENT);

        // Cost also lands on Finances as an `inference.spend` ledger entry.
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        let spend: Vec<_> = record
            .ledger
            .iter()
            .filter(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
            .collect();
        assert_eq!(spend.len(), 1);
        // Negative: an outflow, per the ledger convention (issue #1047).
        assert_eq!(spend[0].amount_usd, -0.031);
    }

    /// Tokens without USD (the managed passthrough bills backend-side) still
    /// count on the Usage surface, but must not post a `$0.00` spend line.
    #[tokio::test]
    async fn token_only_usage_meters_without_a_ledger_entry() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(reported_usage(0.0))))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert_eq!(rt.usage().query(rt.id(), 0).await.unwrap().len(), 1);
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert!(
            !record
                .ledger
                .iter()
                .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
        );
    }

    /// A cycle that spent nothing writes nothing — an idle cycle or the offline
    /// echo brain must not mint an empty sample.
    #[tokio::test]
    async fn a_zero_usage_cycle_writes_no_sample() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(TokenUsage::default())))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
    }

    /// The openhuman harness meters every turn itself, so the cycle seam must
    /// stay out of its way: a self-metering path's cycle usage is ignored rather
    /// than charged a second time.
    #[tokio::test]
    async fn a_self_metering_brain_is_not_metered_twice() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain {
                usage: reported_usage(9.99),
                metering: UsageMetering::PerTurn,
            }))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert!(
            !record
                .ledger
                .iter()
                .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
        );
    }

    /// `UsageMetering::None` means "no model runs on this path", so the cycle
    /// seam must enforce it too. Without that arm a `None` brain reporting
    /// non-zero usage was still metered under its own slug — the echo brain
    /// would post a `provider: "none"` row into `byProvider`.
    #[tokio::test]
    async fn a_brain_that_runs_no_model_is_not_metered() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain {
                usage: reported_usage(4.2),
                metering: UsageMetering::None,
            }))
            .build()
            .await
            .unwrap();

        rt.run_cycle(Vec::new()).await.unwrap();

        assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
        let record = rt.store().load(rt.id()).await.unwrap().unwrap();
        assert!(
            !record
                .ledger
                .iter()
                .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND)
        );
    }

    /// Every cycle meters independently, so a multi-turn conversation
    /// accumulates rather than overwriting.
    #[tokio::test]
    async fn each_cycle_meters_its_own_usage() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(MeteredBrain::per_cycle(reported_usage(0.01))))
            .build()
            .await
            .unwrap();

        for _ in 0..3 {
            rt.run_cycle(Vec::new()).await.unwrap();
        }

        let samples = rt.usage().query(rt.id(), 0).await.unwrap();
        assert_eq!(samples.len(), 3);
        let total: u64 = samples.iter().map(|s| s.input_tokens).sum();
        assert_eq!(total, 3_600);
    }

    /// A brain that tracks the peak number of concurrently-active cycles.
    struct ConcurrencyBrain {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Brain for ConcurrencyBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "concurrency")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn cycles_are_serial_per_company() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let peak = Arc::new(AtomicUsize::new(0));
        let brain = Arc::new(ConcurrencyBrain {
            active: Arc::new(AtomicUsize::new(0)),
            peak: peak.clone(),
        });
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("full"))
                .with_brain(brain)
                .build()
                .await
                .unwrap(),
        );

        let a = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.run_cycle(Vec::new()).await })
        };
        let b = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.run_cycle(Vec::new()).await })
        };
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();

        // The serial lock kept the two cycles from overlapping.
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    // --- The cycle bracket (issue #390) --------------------------------------

    /// **The placement test.** The bracket opens *before* the serial lock, and
    /// this is the only thing that says so.
    ///
    /// Condition of the fix rather than a nice-to-have: `cycle_id` used to be
    /// minted inside `run_locked`, and moving it is the riskiest part of #390.
    /// The failure mode is a correctly-typed id written in the wrong place —
    /// invisible to the compiler and invisible to every other test, because a
    /// bracket that opens after the lock still opens, still closes, and still
    /// reads correctly once the cycle is over.
    ///
    /// So this holds the lock and asserts the cycle is *already* visible as open
    /// while it is still queued behind it. Move the mint or the `started` write
    /// back inside `run_locked` and this fails; nothing else does.
    #[tokio::test]
    async fn a_cycles_bracket_opens_before_the_serial_lock() {
        let home_dir = tmp_home();
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
                .build()
                .await
                .unwrap(),
        );

        assert!(rt.journal.open_cycles().is_empty(), "nothing has run yet");

        // Hold the lock, so any cycle we start is stuck on the near side of it —
        // the window a post-lock bracket cannot see.
        let guard = rt.serial.lock().await;

        let spawned = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.run_cycle(Vec::new()).await })
        };

        // Wait for the bracket, not for the cycle — the cycle cannot proceed.
        let mut open = Vec::new();
        for _ in 0..200 {
            open = rt.journal.open_cycles();
            if !open.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            open.len(),
            1,
            "a cycle blocked on the serial lock must already be bracketed; if this \
             is empty the `started` write moved back inside the lock and #390's \
             whole case — a continuation that dies waiting — is invisible again"
        );
        assert_eq!(open[0].trigger, "empty", "no events drove this one");

        // …and it closes once the lock is released.
        drop(guard);
        spawned.await.unwrap().unwrap();
        assert!(
            rt.journal.open_cycles().is_empty(),
            "the bracket closes when the cycle ends"
        );
    }

    /// The trigger label distinguishes the case #390 is about from an ordinary
    /// chat turn, which is the whole reason an operator can read the surface.
    #[test]
    fn the_trigger_label_names_what_drove_the_cycle() {
        assert_eq!(cycle_trigger(&[]), "empty");
        assert_eq!(
            cycle_trigger(&[CompanyEvent::ApprovalResolved {
                approval_id: ApprovalId::new("appr-1"),
                verdict: Verdict::Approve,
                by: operator(),
            }]),
            "approval-continuation"
        );
        assert_eq!(
            cycle_trigger(&[CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "hello".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            "operator-message"
        );
    }

    #[tokio::test]
    async fn distinct_companies_run_concurrently() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let one = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_id(CompanyId::new("one"))
            .build()
            .await
            .unwrap();
        let two = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_id(CompanyId::new("two"))
            .build()
            .await
            .unwrap();

        let (ra, rb) = tokio::join!(
            one.run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "a".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
            two.run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "b".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }]),
        );
        assert_eq!(ra.unwrap().responses.len(), 1);
        assert_eq!(rb.unwrap().responses.len(), 1);
    }

    fn test_smtp(from_email: &str) -> SmtpCredentials {
        SmtpCredentials {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "user".into(),
            password: SecretValue("hunter2".into()),
            from_name: "Acme".into(),
            from_email: from_email.into(),
        }
    }

    #[tokio::test]
    async fn email_send_effect_sends_and_records() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let email_effect = Effect {
            kind: "email.send".into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: true,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(EffectBrain {
                effect: email_effect,
            }))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "send it".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        assert_eq!(sender.sent().len(), 1);
        // The From address is the company's own address, never spoofable via
        // the effect payload (which carries no `from` field at all).
        assert_eq!(sender.sent()[0].0, "ceo@acme.test");
        let inbox = rt.inbox().messages(rt.id(), "ceo", 10, 0).await.unwrap();
        assert!(inbox.iter().any(|r| r.outbound && r.subject == "Hi"));
    }

    /// **The acceptance bar for issue #227.** Parking a cold recipient's report
    /// is only worth doing if approving it actually sends the mail — otherwise
    /// `pending` is a nicer-looking way to drop the report.
    ///
    /// This parks an `email.send` effect the way
    /// [`crate::workflows::delivery`] does — straight onto the gate + journal,
    /// with no cycle running and no brain involved — then resolves it the way
    /// the HTTP handler does. The mail must go out and leave the outbound audit
    /// record, through `resolve_approval` → `execute_effect_once` →
    /// `perform_effect` → `send_company_email`.
    ///
    /// Policy mode is `full` on purpose: nothing here relies on the gate
    /// deciding to park. It was parked directly, exactly as delivery parks it.
    #[tokio::test]
    async fn a_directly_parked_email_send_is_mailed_when_approved() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("full"))
                .with_mail(CompanyMail {
                    sender: sender.clone(),
                    smtp: test_smtp("ceo@acme.test"),
                })
                .build()
                .await
                .unwrap(),
        );

        // What `park_cold_recipient` builds, field for field.
        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: true,
            payload: serde_json::json!({
                "to": "stranger@ext.com",
                "subject": "[Acme] Report flow — Owner summary",
                "body": "Q3 is up 12%.",
            }),
            agent: None,
            run_id: None,
        };
        let approval_id = rt.approvals.park(rt.id(), effect.clone()).await.unwrap();
        rt.journal
            .record_parked(
                &approval_id,
                &effect,
                now_millis(),
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();

        // It reaches the operator's queue — the same list a workflow's park
        // shows up in, since it is the same journal.
        assert_eq!(rt.pending_approvals().len(), 1);
        assert_eq!(rt.pending_approvals()[0].kind, EMAIL_SEND_KIND);
        assert!(sender.sent().is_empty(), "parked means not yet sent");

        rt.resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();

        // Approving SENDS.
        assert_eq!(sender.sent().len(), 1, "approving must mail the report");
        assert_eq!(sender.sent()[0].1.to, "stranger@ext.com");
        assert!(sender.sent()[0].1.body.contains("Q3 is up 12%."));
        // From the company's own address, never anything the payload named.
        assert_eq!(sender.sent()[0].0, "ceo@acme.test");
        // …and leaves the outbound audit record, which also makes the recipient
        // an established thread for next time.
        let inbox = rt.inbox().messages(rt.id(), "ceo", 10, 0).await.unwrap();
        assert!(
            inbox
                .iter()
                .any(|r| r.outbound && r.subject.contains("Owner summary")),
            "{inbox:?}"
        );
        assert!(rt.pending_approvals().is_empty(), "the queue drains");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// The other half of the same bar: DENYING sends nothing and drains the
    /// queue. A parked report must not leak out on a refusal.
    #[tokio::test]
    async fn a_directly_parked_email_send_is_not_mailed_when_denied() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("full"))
                .with_mail(CompanyMail {
                    sender: sender.clone(),
                    smtp: test_smtp("ceo@acme.test"),
                })
                .build()
                .await
                .unwrap(),
        );

        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: true,
            payload: serde_json::json!({
                "to": "stranger@ext.com",
                "subject": "[Acme] Report flow — Owner summary",
                "body": "Q3 is up 12%.",
            }),
            agent: None,
            run_id: None,
        };
        let approval_id = rt.approvals.park(rt.id(), effect.clone()).await.unwrap();
        rt.journal
            .record_parked(
                &approval_id,
                &effect,
                now_millis(),
                TaskLink::Unlinked,
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();

        rt.resolve_approval(&approval_id, Verdict::Deny, operator())
            .await
            .unwrap();

        assert!(sender.sent().is_empty(), "a denied report must not go out");
        assert!(
            rt.inbox()
                .messages(rt.id(), "ceo", 10, 0)
                .await
                .unwrap()
                .iter()
                .all(|r| !r.outbound),
            "nothing was sent, so there is no outbound record"
        );
        assert!(rt.pending_approvals().is_empty());
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    /// **Restart durability.** A parked report survives a process restart with
    /// its original id and still sends on approval — the property that makes a
    /// `pending` row honest even though the run itself is not persisted.
    #[tokio::test]
    async fn a_parked_email_send_survives_a_restart_and_still_sends() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let effect = Effect {
            kind: EMAIL_SEND_KIND.into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: true,
            payload: serde_json::json!({
                "to": "stranger@ext.com",
                "subject": "[Acme] Report flow — Owner summary",
                "body": "Q3 is up 12%.",
            }),
            agent: None,
            run_id: None,
        };
        let approval_id = {
            let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
                .build()
                .await
                .unwrap();
            let id = rt.approvals.park(rt.id(), effect.clone()).await.unwrap();
            rt.journal
                .record_parked(
                    &id,
                    &effect,
                    now_millis(),
                    TaskLink::Unlinked,
                    ApprovalConversation::default(),
                    None,
                )
                .await
                .unwrap();
            id
        };

        // Fresh runtime over the same home: boot replay rehydrates the card.
        let sender = Arc::new(RecordingMailSender::new());
        let rt2 = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("full"))
                .with_mail(CompanyMail {
                    sender: sender.clone(),
                    smtp: test_smtp("ceo@acme.test"),
                })
                .build()
                .await
                .unwrap(),
        );
        let pending = rt2.pending_approvals();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].id, approval_id, "the ORIGINAL id, not a new one");

        rt2.resolve_approval(&approval_id, Verdict::Approve, operator())
            .await
            .unwrap();
        assert_eq!(
            sender.sent().len(),
            1,
            "a card approved after a restart must still mail"
        );
        assert_eq!(sender.sent()[0].1.to, "stranger@ext.com");
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn email_send_effect_without_mail_errors() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let email_effect = Effect {
            kind: "email.send".into(),
            group: EffectGroup::Send,
            amount_usd: None,
            established_thread: true,
            first_time_counterparty: false,
            payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_brain(Arc::new(EffectBrain {
                effect: email_effect,
            }))
            .build()
            .await
            .unwrap();

        let err = perform_effect(
            &rt,
            &Effect {
                kind: "email.send".into(),
                group: EffectGroup::Send,
                amount_usd: None,
                established_thread: true,
                first_time_counterparty: false,
                payload: serde_json::json!({ "to": "x@ext.com", "subject": "Hi", "body": "yo" }),
                agent: None,
                run_id: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("email is not configured"));
    }

    #[tokio::test]
    async fn established_true_only_after_inbound_from_recipient() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: Arc::new(RecordingMailSender::new()),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        assert!(!recipient_is_established(&rt, "x@ext.com").await);

        rt.inbox()
            .append(
                rt.id(),
                &crate::ports::inbox::EmailRecord {
                    id: "1".into(),
                    inbox: "ceo".into(),
                    from_name: "".into(),
                    from_email: "x@ext.com".into(),
                    subject: "hi".into(),
                    body: "".into(),
                    at_millis: 0,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();

        assert!(recipient_is_established(&rt, "X@EXT.COM").await);
    }

    /// Issue #1113: the trigger boundary, named event by event. Outside
    /// content — a webhook here, an A2A task in the sibling test — makes a
    /// cycle external; the company's own machinery — operator speech,
    /// payments, dispatches, schedule fires — stays Internal, per the
    /// operator-facts authorship precedent.
    #[test]
    fn outside_content_makes_a_cycle_external_and_own_machinery_does_not() {
        use crate::ports::types::Actor;
        let webhook = CompanyEvent::WebhookReceived {
            channel: "telegram".into(),
            body: serde_json::json!({"text": "raw payload"}),
        };
        let operator = CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: "please do the thing".into(),
            by: Option::<Actor>::None,
            chat: None,
            parent: None,
            deliverable: None,
            attachments: Vec::new(),
        };
        // The company's own machinery stays Internal, event by event: a
        // payment landing is the company's ledger speaking, not third-party
        // prose riding an open boundary.
        let payment = CompanyEvent::PaymentReceived {
            amount_usd: 10.0,
            memo: "invoice".into(),
        };
        assert!(cycle_is_external(&[webhook]));
        assert!(!cycle_is_external(&[operator]));
        assert!(!cycle_is_external(&[payment]));
        assert!(!cycle_is_external(&[]));
    }

    /// The #68 sibling review's M1: an A2A task is a remote agent's payload —
    /// third-party content, external. Mixed batches over-taint (any(), not
    /// all()): the safe direction, asserted in both orderings.
    #[test]
    fn a2a_tasks_are_external_and_mixed_batches_over_taint() {
        use crate::ports::types::Actor;
        let a2a = || CompanyEvent::A2aTaskReceived {
            from: "remote-agent".into(),
            task: serde_json::json!({"text": "do the thing"}),
        };
        let operator = || CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: "hi".into(),
            by: Option::<Actor>::None,
            chat: None,
            parent: None,
            deliverable: None,
            attachments: Vec::new(),
        };
        assert!(cycle_is_external(&[a2a()]));
        assert!(cycle_is_external(&[operator(), a2a()]));
        assert!(cycle_is_external(&[a2a(), operator()]));
    }

    /// The routing the flag drives: an externally-triggered cycle's
    /// `ContextOp::Put` lands on the inbound (taint-stamping) port, an
    /// ordinary cycle's on the plain context port — proven with two disjoint
    /// stores, so a write to the wrong one is a visible row, not a guess.
    #[tokio::test]
    async fn external_cycles_put_through_the_inbound_port() {
        use crate::ports::ContextStore;
        use crate::store::FsContextStore;

        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let plain_dir = tempfile::tempdir().unwrap();
        let inbound_dir = tempfile::tempdir().unwrap();
        let plain: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(plain_dir.path().to_path_buf()));
        let inbound: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(inbound_dir.path().to_path_buf()));
        let rt = RuntimeBuilder::new(home, manifest("full"))
            .with_context(plain.clone())
            .with_inbound_context(inbound.clone())
            .build()
            .await
            .unwrap();

        for (external, cycle) in [(false, "cyc-int"), (true, "cyc-ext")] {
            let host = CycleHostImpl::new(
                rt.id().clone(),
                cycle.into(),
                &rt,
                None,
                external,
                ApprovalConversation::default(),
            );
            host.context_op(ContextOp::Put(crate::ports::types::ContextChunk {
                label: format!("probe/{cycle}"),
                body: format!("body {cycle}"),
            }))
            .await
            .unwrap();
        }

        let company = rt.id().clone();
        let plain_rows = plain.list(&company, "probe/").await.unwrap();
        let inbound_rows = inbound.list(&company, "probe/").await.unwrap();
        assert_eq!(
            plain_rows
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            ["probe/cyc-int"],
            "the ordinary cycle writes the plain port, and only it"
        );
        assert_eq!(
            inbound_rows
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            ["probe/cyc-ext"],
            "the external cycle writes the inbound port, and only it"
        );
    }

    /// The same routing guarantee through `with_memory_overlay`: the overlay
    /// carries the inbound port, and dropping it in the builder was the break
    /// that once left the whole firewall dead (issue #1113). An external
    /// cycle on an overlay-built runtime must still write the taint-stamping
    /// store.
    #[tokio::test]
    async fn overlay_built_runtimes_route_external_puts_through_the_inbound_port() {
        use crate::ports::ContextStore;
        use crate::store::{FsContextStore, FsMemoryStore, MemoryOverlay};

        let home_dir = tmp_home();
        let plain_dir = tempfile::tempdir().unwrap();
        let inbound_dir = tempfile::tempdir().unwrap();
        let plain: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(plain_dir.path().to_path_buf()));
        let inbound: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(inbound_dir.path().to_path_buf()));
        let memory_dir = tempfile::tempdir().unwrap();
        let overlay = MemoryOverlay::test_with_ports(
            Arc::new(FsMemoryStore::new(memory_dir.path().to_path_buf())),
            plain.clone(),
            Some(inbound.clone()),
        );
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .with_memory_overlay(&overlay)
            .build()
            .await
            .unwrap();

        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-overlay-ext".into(),
            &rt,
            None,
            true,
            ApprovalConversation::default(),
        );
        host.context_op(ContextOp::Put(crate::ports::types::ContextChunk {
            label: "probe/overlay".into(),
            body: "external body".into(),
        }))
        .await
        .unwrap();

        let company = rt.id().clone();
        assert_eq!(
            inbound
                .list(&company, "probe/")
                .await
                .unwrap()
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            ["probe/overlay"],
            "the overlay's inbound port receives the external cycle's put"
        );
        assert!(
            plain.list(&company, "probe/").await.unwrap().is_empty(),
            "the plain port must not see the external put"
        );
    }

    /// And through `RuntimeHandover`: a successor runtime adopts the
    /// predecessor's inbound port, so an external cycle after a live swap
    /// still writes taint-stamped. A handover that dropped the port would
    /// silently revert every post-swap external put to internal trust.
    #[tokio::test]
    async fn handover_preserves_the_inbound_port_for_external_puts() {
        use crate::ports::ContextStore;
        use crate::store::FsContextStore;

        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let plain_dir = tempfile::tempdir().unwrap();
        let inbound_dir = tempfile::tempdir().unwrap();
        let plain: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(plain_dir.path().to_path_buf()));
        let inbound: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(inbound_dir.path().to_path_buf()));
        let first = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_context(plain.clone())
            .with_inbound_context(inbound.clone())
            .build()
            .await
            .unwrap();
        let successor = RuntimeBuilder::new(home, manifest("full"))
            .with_handover(first.handover())
            .build()
            .await
            .unwrap();

        let host = CycleHostImpl::new(
            successor.id().clone(),
            "cyc-swap-ext".into(),
            &successor,
            None,
            true,
            ApprovalConversation::default(),
        );
        host.context_op(ContextOp::Put(crate::ports::types::ContextChunk {
            label: "probe/swap".into(),
            body: "external body".into(),
        }))
        .await
        .unwrap();

        let company = successor.id().clone();
        assert_eq!(
            inbound
                .list(&company, "probe/")
                .await
                .unwrap()
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            ["probe/swap"],
            "the successor's external cycle writes the inherited inbound port"
        );
        assert!(
            plain.list(&company, "probe/").await.unwrap().is_empty(),
            "the successor's plain port must not see the external put"
        );
    }

    #[tokio::test]
    async fn send_email_without_mail_returns_clean_error() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        // No `.with_mail(..)`: the company has no mailbox wired at all.
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-nomail".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        let res = host
            .send_email(serde_json::json!({ "to": "x@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(
            res.output["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not configured")
        );
    }

    #[tokio::test]
    async fn send_email_bad_args_missing_to_yields_no_effect() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-bad".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        let res = host
            .send_email(serde_json::json!({ "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.output["error"].is_string());
    }

    #[tokio::test]
    async fn send_email_runs_without_policy_hitl_for_a_new_recipient() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-park".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        let res = host
            .send_email(serde_json::json!({ "to": "new@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert_eq!(res.output["status"], "sent");
        assert_eq!(sender.sent().len(), 1);
    }

    /// Issue #333: an effect parked by a card's dispatch cycle is journaled
    /// against that card, so the card's Approvals tab can find it.
    #[tokio::test]
    async fn a_dispatch_cycle_stamps_its_task_onto_every_approval_it_parks() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-task".into(),
            &rt,
            Some("t-42".to_string()),
            false,
            ApprovalConversation::default(),
        );

        host.park_effect(harness_effect(
            "ceo",
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
            serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
        ))
        .await
        .unwrap();

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].task,
            Some(TaskLink::Task {
                id: "t-42".to_string()
            }),
            "the parked approval must name the card that asked for it",
        );
        assert_eq!(
            rt.approval_origins()
                .get(&pending[0].id)
                .and_then(|o| o.task.clone()),
            Some(TaskLink::Task {
                id: "t-42".to_string()
            }),
            "and the link must outlive the queue entry",
        );
    }

    /// A cycle with no card behind it records the park as *explicitly* unlinked
    /// rather than leaving the link blank (#333 review follow-up).
    ///
    /// The blank is reserved for pre-#333 journal lines, and it is the only
    /// thing the read side still window-guesses on. If a chat turn's park were
    /// written that way too, every one of them would land on whatever card
    /// happened to be mid-run — the bug this issue exists to close.
    #[tokio::test]
    async fn a_cycle_with_no_card_parks_explicitly_unlinked() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-chat".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        host.park_effect(harness_effect(
            "ceo",
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
            serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
        ))
        .await
        .unwrap();

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].task,
            Some(TaskLink::Unlinked),
            "an unlinked park must say so, not leave the link absent",
        );
    }

    /// Issue #379: an effect parked by a desk channel's turn carries that
    /// channel onto the summary the console reads, **and** announces itself on
    /// the event log so an inline card can appear without waiting for a poll.
    #[tokio::test]
    async fn a_chat_cycle_stamps_its_thread_and_announces_the_park() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-thread".into(),
            &rt,
            None,
            false,
            ApprovalConversation {
                thread: Some("desk-finance".to_string()),
                parent: None,
            },
        );

        host.park_effect(harness_effect(
            "ceo",
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
            serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
        ))
        .await
        .unwrap();

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].thread,
            Some("desk-finance".to_string()),
            "the parked approval must name the conversation that asked for it",
        );

        let logged = rt
            .events()
            .read_from(rt.id(), EventSeq::new(0), 50)
            .await
            .unwrap();
        let parked: Vec<_> = logged
            .iter()
            .filter_map(|e| match &e.event {
                CompanyEvent::ApprovalParked {
                    approval_id,
                    effect_kind,
                    thread,
                } => Some((approval_id.clone(), effect_kind.clone(), thread.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(parked.len(), 1, "exactly one park announcement: {logged:?}");
        assert_eq!(parked[0].0, pending[0].id);
        assert_eq!(
            parked[0].1,
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
        );
        assert_eq!(parked[0].2, Some("desk-finance".to_string()));
    }

    /// The same park with no conversation behind it announces itself with **no
    /// thread**, which is what keeps it Approvals-page-only. Inline is additive,
    /// never a replacement (#379).
    #[tokio::test]
    async fn a_threadless_park_announces_itself_without_a_channel() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-none".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        host.park_effect(harness_effect(
            "ceo",
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
            serde_json::json!({ "title": "Send the message", "question": "May I send it?" }),
        ))
        .await
        .unwrap();

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].thread, None);
        // And it is omitted from the serialized summary entirely, so an older
        // console sees the wire shape it already knows.
        let wire = serde_json::to_value(&pending[0]).unwrap();
        assert!(
            wire.get("thread").is_none(),
            "an approval with no conversation must not carry an empty thread key: {wire}",
        );

        let logged = rt
            .events()
            .read_from(rt.id(), EventSeq::new(0), 50)
            .await
            .unwrap();
        assert!(
            logged
                .iter()
                .any(|e| matches!(&e.event, CompanyEvent::ApprovalParked { thread: None, .. })),
            "the park is still announced, it simply names no channel: {logged:?}",
        );
    }

    /// Issue #842: **every gated call one turn parks carries that turn's key**,
    /// and a different turn's parks carry a different one.
    ///
    /// This is the whole of the batching mechanism, and it is deliberately not
    /// a new one. #469 already records the parking cycle so a turn blocked on
    /// four decisions is continued once rather than four times; the operator
    /// was simply never shown that grouping, so a research turn that reached
    /// three sites interrupted the conversation three times to ask about one
    /// piece of work. Projecting the key it already had is what lets the
    /// conversation ask once.
    ///
    /// The second host is the half that matters. A key every park shares would
    /// consolidate correctly and also fold two unrelated turns into one card —
    /// an operator approving a batch they never saw raised. Grouping is only
    /// safe because the key separates turns, so both directions are asserted.
    ///
    /// What is *not* changed here, and is asserted to make the point: the parks
    /// stay two records with two ids. Chat groups them for display; each is
    /// still decided on its own and still mints its own host-scoped grant
    /// (#739).
    #[tokio::test]
    async fn every_approval_one_turn_parks_carries_that_turns_batch_key() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        // One turn, two gated calls — the shape the issue reports, where a
        // research turn reaches several outside hosts before it yields.
        let turn = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-research".into(),
            &rt,
            None,
            false,
            ApprovalConversation {
                thread: Some("desk-marketing".to_string()),
                parent: None,
            },
        );
        turn.park_effect(harness_effect(
            "seo",
            "web_fetch",
            serde_json::json!({ "url": "https://espn.com/nba" }),
        ))
        .await
        .unwrap();
        turn.park_effect(harness_effect(
            "seo",
            "web_fetch",
            serde_json::json!({ "url": "https://bbc.com/sport" }),
        ))
        .await
        .unwrap();

        // A later, unrelated turn in the same conversation.
        let other = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-later".into(),
            &rt,
            None,
            false,
            ApprovalConversation {
                thread: Some("desk-marketing".to_string()),
                parent: None,
            },
        );
        other
            .park_effect(harness_effect(
                "seo",
                "web_fetch",
                serde_json::json!({ "url": "https://theguardian.com/uk" }),
            ))
            .await
            .unwrap();

        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 3, "one record per gated call, still");
        let batches: Vec<Option<String>> = pending.iter().map(|p| p.batch.clone()).collect();
        assert!(
            batches.iter().all(Option::is_some),
            "a park raised by a turn must name it: {batches:?}"
        );

        let by_url = |url: &str| {
            pending
                .iter()
                .find(|p| p.payload.as_ref().is_some_and(|v| v["url"] == url))
                .unwrap_or_else(|| panic!("no parked approval for {url}"))
        };
        let espn = by_url("https://espn.com/nba");
        let bbc = by_url("https://bbc.com/sport");
        let guardian = by_url("https://theguardian.com/uk");

        assert_eq!(
            espn.batch, bbc.batch,
            "two calls one turn parked belong to one batch, so the operator is asked once"
        );
        assert_ne!(
            espn.batch, guardian.batch,
            "a different turn is a different question — consolidating across turns would ask \
             an operator to approve work they never saw raised"
        );
        // Still three decisions underneath. The batch is presentation; the park
        // is the unit of truth, and each keeps its own id to be resolved by.
        assert_eq!(
            std::collections::HashSet::from([&espn.id, &bbc.id, &guardian.id]).len(),
            3,
            "grouping must not merge the records it groups"
        );
    }

    /// The correlation key itself (#333): which card a cycle is working, read
    /// off its own trigger events.
    #[test]
    fn cycle_task_id_reads_a_dispatch_inherits_a_resolution_and_refuses_to_guess() {
        use crate::ports::types::{Actor, ActorKind, ApprovalId, Verdict};

        // The lookup a live cycle does per id, stubbed: `appr-1` belongs to a
        // card, `appr-none` is a recorded unlinked park, `appr-legacy` is a
        // pre-#333 line, and anything else has no origin at all.
        let approval_task = |id: &ApprovalId| match id.as_ref() {
            "appr-1" => Some(Some(TaskLink::Task { id: "t-1".into() })),
            "appr-none" => Some(Some(TaskLink::Unlinked)),
            "appr-legacy" => Some(None),
            _ => None,
        };
        let dispatched = |id: &str| CompanyEvent::TaskDispatched {
            task_id: id.to_string(),
            run_id: None,
        };
        let resolved = |id: &str| CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new(id),
            verdict: Verdict::Approve,
            by: Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            },
        };

        let chat = || CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        };

        // A dispatch names the card outright.
        assert_eq!(
            cycle_task_id(&[dispatched("t-1")], approval_task),
            Some("t-1".into())
        );
        // A follow-up cycle inherits it from the approval it is resolving, so a
        // run needing two sign-offs keeps the link through the first.
        assert_eq!(
            cycle_task_id(&[resolved("appr-1")], approval_task),
            Some("t-1".into())
        );
        // An approval with no origin at all claims nothing.
        assert_eq!(
            cycle_task_id(&[resolved("appr-unknown")], approval_task),
            None
        );
        // Nor does a pre-#333 one.
        assert_eq!(
            cycle_task_id(&[resolved("appr-legacy")], approval_task),
            None
        );
        // Nothing task-shaped at all.
        assert_eq!(cycle_task_id(&[chat()], approval_task), None);
        // Two different cards in one batch: refuse to guess rather than hand one
        // of them the other's approvals.
        assert_eq!(
            cycle_task_id(&[dispatched("t-1"), dispatched("t-2")], approval_task),
            None
        );
        // The same card twice is not ambiguous.
        assert_eq!(
            cycle_task_id(&[dispatched("t-1"), resolved("appr-1")], approval_task),
            Some("t-1".into())
        );

        // Review follow-up: a cycle is a batch, not a turn. A chat message
        // riding the same batch as a dispatch is its own work, and an effect it
        // parks is not the card's — so the batch is ambiguous, exactly as two
        // cards would be. Same for a webhook, a schedule tick, or an A2A task.
        assert_eq!(
            cycle_task_id(&[dispatched("t-1"), chat()], approval_task),
            None,
            "a chat turn batched with a dispatch must not be stamped with the card",
        );
        assert_eq!(
            cycle_task_id(
                &[
                    dispatched("t-1"),
                    CompanyEvent::ScheduleFired {
                        cron: "* * * * *".into(),
                        prompt: "tick".into(),
                    },
                ],
                approval_task,
            ),
            None,
        );
        // A payment and a filed feedback item are inbound triggers too — they
        // drive their own turn, so neither may inherit the card's stamp.
        assert_eq!(
            cycle_task_id(
                &[
                    dispatched("t-1"),
                    CompanyEvent::PaymentReceived {
                        amount_usd: 10.0,
                        memo: "invoice".into(),
                    },
                ],
                approval_task,
            ),
            None,
        );
        assert_eq!(
            cycle_task_id(
                &[
                    dispatched("t-1"),
                    CompanyEvent::FeedbackFiled {
                        note: "it mis-filed".into(),
                    },
                ],
                approval_task,
            ),
            None,
        );
        // A record of something that already happened is not a rival: it names
        // no card and competes for none, so the dispatch still stamps.
        assert_eq!(
            cycle_task_id(
                &[
                    dispatched("t-1"),
                    CompanyEvent::DeskTaskCompleted {
                        task_id: "t-9".into(),
                        desk: "ops".into(),
                        column: "done".into(),
                        artifact_ids: Vec::new(),
                        output: String::new(),
                        origin_chat_id: None,
                    },
                ],
                approval_task,
            ),
            Some("t-1".into()),
            "a completion record must not disqualify the batch",
        );
        // And a resolution known to belong to no card is a rival trigger too,
        // not a neutral event — it is somebody's work, just not a card's.
        assert_eq!(
            cycle_task_id(&[dispatched("t-1"), resolved("appr-none")], approval_task),
            None,
        );

        // Issue #327: a workspace write is a record of something that already
        // happened, so it is neutral on both counts. Alone it names no card…
        assert_eq!(cycle_task_id(&[workspace_changed()], approval_task), None);
        // …and — the arm that actually matters — it must not *disqualify* a
        // dispatch it happens to share a batch with. An agent writing a note
        // while its cycle runs is the ordinary case, and treating that write as
        // a rival trigger would strip the card off its own stamp.
        assert_eq!(
            cycle_task_id(&[dispatched("t-1"), workspace_changed()], approval_task),
            Some("t-1".into()),
            "a workspace write must not disqualify the dispatch beside it",
        );
        assert_eq!(
            cycle_task_id(
                &[workspace_changed(), dispatched("t-1"), workspace_changed()],
                approval_task
            ),
            Some("t-1".into()),
            "and not from either side of it",
        );

        // Issue #382: a per-node start bracket is the same kind of record — a
        // workflow walking its graph, not a stimulus. Alone it names no card…
        assert_eq!(
            cycle_task_id(&[workflow_node_started()], approval_task),
            None,
        );
        // …and it must not disqualify a dispatch it shares a batch with (a
        // workflow node beginning while a cycle's card runs is the ordinary
        // case).
        assert_eq!(
            cycle_task_id(&[dispatched("t-1"), workflow_node_started()], approval_task),
            Some("t-1".into()),
            "a node-start bracket must not disqualify the dispatch beside it",
        );
    }

    /// One workspace write (issue #327), for the neutrality assertions in both
    /// `cycle_*_id` tests. Shared because it is the same claim made twice: an
    /// event neutral for the card but not for the thread would mis-stamp every
    /// cycle that answered a message and touched the tree.
    fn workspace_changed() -> CompanyEvent {
        CompanyEvent::WorkspaceChanged {
            node_id: "n-1".into(),
            change: "updated".into(),
        }
    }

    /// One per-node start bracket (issue #382), for the neutrality assertions in
    /// both `cycle_*_id` tests. Like `workspace_changed`, it is a record of a
    /// workflow walking its graph — it names no card and no thread, so alone it
    /// stamps neither, and beside a trigger it must not disqualify the batch.
    fn workflow_node_started() -> CompanyEvent {
        CompanyEvent::WorkflowNodeStarted {
            workflow_id: "digest".into(),
            run_id: "run-1".into(),
            node_id: "n-1".into(),
        }
    }

    /// The conversation key (#379): which chat thread a cycle is answering, read
    /// off its own trigger events.
    ///
    /// The trap this exists to close is the one `Effect::agent` cannot: a desk
    /// channel and a direct message to that desk's lead are answered by the same
    /// teammate and are **different threads**. `OperatorMessage.chat` is the only
    /// field that tells them apart, which is why the stamp is read from there.
    #[test]
    fn cycle_thread_id_reads_an_addressed_message_inherits_a_resolution_and_refuses_to_guess() {
        use crate::ports::types::{Actor, ActorKind, ApprovalId, Verdict};

        // The lookup a live cycle does per id, stubbed: `appr-desk` was raised in
        // a desk channel, `appr-dm` in a direct message, `appr-none` had no
        // conversation behind it (or is a pre-#379 line — the same answer, on
        // purpose), and anything else has no origin at all.
        let conv = |thread: Option<&str>, parent: Option<u64>| ApprovalConversation {
            thread: thread.map(str::to_string),
            parent: parent.map(EventSeq::new),
        };
        let approval_conversation = move |id: &ApprovalId| match id.as_ref() {
            "appr-desk" => Some(conv(Some("desk-finance"), None)),
            "appr-dm" => Some(conv(Some("agent-cfo"), None)),
            "appr-none" => Some(conv(None, None)),
            // Issue #435: raised inside a thread of the desk channel.
            "appr-desk-threaded" => Some(conv(Some("desk-finance"), Some(7))),
            _ => None,
        };
        let addressed = |chat: &str| CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "pay the invoice".into(),
            by: None,
            chat: Some(chat.to_string()),
            deliverable: None,
            attachments: Vec::new(),
        };
        let unaddressed = || CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        };
        let resolved = |id: &str| CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new(id),
            verdict: Verdict::Approve,
            by: Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            },
        };
        let dispatched = || CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
        };

        // An addressed message names the thread outright.
        assert_eq!(
            cycle_conversation(&[addressed("desk-finance")], approval_conversation).thread,
            Some("desk-finance".into()),
        );
        // The whole point, stated as an assertion: the desk channel and a DM to
        // that desk's lead are different stamps, even though the same agent
        // answers both.
        assert_eq!(
            cycle_conversation(&[addressed("agent-cfo")], approval_conversation).thread,
            Some("agent-cfo".into()),
        );
        // A follow-up cycle inherits the thread from the approval it resolves, so
        // a turn needing a second sign-off re-parks in the same channel.
        assert_eq!(
            cycle_conversation(&[resolved("appr-desk")], approval_conversation).thread,
            Some("desk-finance".into()),
        );
        assert_eq!(
            cycle_conversation(&[resolved("appr-dm")], approval_conversation).thread,
            Some("agent-cfo".into()),
        );
        // An approval with no origin at all claims nothing — and does not block.
        assert_eq!(
            cycle_conversation(
                &[resolved("appr-unknown"), addressed("desk-finance")],
                approval_conversation
            )
            .thread,
            Some("desk-finance".into()),
        );
        // An unaddressed message went to the orchestrator with no conversation of
        // its own. It is a rival, not a pass-through.
        assert_eq!(
            cycle_conversation(&[unaddressed()], approval_conversation).thread,
            None
        );
        assert_eq!(
            cycle_conversation(
                &[addressed("desk-finance"), unaddressed()],
                approval_conversation
            )
            .thread,
            None,
            "an unaddressed turn batched with an addressed one must not borrow its channel",
        );
        // Two different threads in one batch: refuse rather than raise one
        // conversation's request inside the other.
        assert_eq!(
            cycle_conversation(
                &[addressed("desk-finance"), addressed("agent-cfo")],
                approval_conversation,
            )
            .thread,
            None,
        );
        // The same thread twice is not ambiguous.
        assert_eq!(
            cycle_conversation(
                &[addressed("desk-finance"), resolved("appr-desk")],
                approval_conversation,
            )
            .thread,
            Some("desk-finance".into()),
        );
        // A resolution known to have come from no conversation is a rival too.
        assert_eq!(
            cycle_conversation(
                &[addressed("desk-finance"), resolved("appr-none")],
                approval_conversation,
            )
            .thread,
            None,
        );
        // Inbound triggers that are their own work disqualify the batch, exactly
        // as a rival chat turn does for the card stamp.
        for rival in [
            dispatched(),
            CompanyEvent::ScheduleFired {
                cron: "* * * * *".into(),
                prompt: "tick".into(),
            },
            CompanyEvent::WebhookReceived {
                channel: "stripe".into(),
                body: serde_json::json!({}),
            },
            CompanyEvent::A2aTaskReceived {
                from: "peer".into(),
                task: serde_json::json!({}),
            },
            CompanyEvent::PaymentReceived {
                amount_usd: 10.0,
                memo: "invoice".into(),
            },
            CompanyEvent::FeedbackFiled {
                note: "it mis-filed".into(),
            },
        ] {
            assert_eq!(
                cycle_conversation(
                    &[addressed("desk-finance"), rival.clone()],
                    approval_conversation
                )
                .thread,
                None,
                "{rival:?} is its own work and must not inherit the channel",
            );
        }
        // A record of something that already happened is not a rival — including
        // this cycle's own park event, which is appended after the park it
        // describes and would otherwise disqualify a second one.
        for record in [
            CompanyEvent::ApprovalParked {
                approval_id: ApprovalId::new("appr-desk"),
                effect_kind: "payment.send".into(),
                thread: Some("desk-finance".into()),
            },
            CompanyEvent::DeskTaskCompleted {
                task_id: "t-9".into(),
                desk: "ops".into(),
                column: "done".into(),
                artifact_ids: Vec::new(),
                output: String::new(),
                origin_chat_id: None,
            },
            CompanyEvent::AgentReply {
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                chat_id: "desk-ops".into(),
                agent_id: "ops".into(),
                text: "done".into(),
                steps: Vec::new(),
                task_id: None,
            },
            // Issue #327: appended by the workspace store after the write it
            // describes. An agent that answers a message and touches the tree
            // in the same turn must keep its channel stamp.
            workspace_changed(),
            // Issue #382: a per-node start bracket is a workflow walking its
            // graph — a record, not a trigger, so it must not steal the channel
            // off an addressed message beside it either.
            workflow_node_started(),
        ] {
            assert_eq!(
                cycle_conversation(
                    &[addressed("desk-finance"), record.clone()],
                    approval_conversation
                )
                .thread,
                Some("desk-finance".into()),
                "{record:?} is a record, not a trigger, and must not disqualify the batch",
            );
        }
        // And alone neither claims a conversation of its own.
        assert_eq!(
            cycle_conversation(&[workspace_changed()], approval_conversation).thread,
            None,
        );
        assert_eq!(
            cycle_conversation(&[workflow_node_started()], approval_conversation).thread,
            None,
        );
    }

    /// Issue #435: the thread *within* the channel, and the asymmetry between
    /// the two keys.
    ///
    /// The channel rule is #379's and is asserted above. This pins the part
    /// that is easy to get wrong in the obvious way: resolving `(channel,
    /// thread)` as a single unit would make two messages in one channel but
    /// two different threads ambiguous, dropping an approval that lands
    /// correctly today off its conversation entirely. A finer key must never
    /// cost a coarser answer that was already right.
    #[test]
    fn cycle_conversation_carries_the_thread_root_and_degrades_it_before_the_channel() {
        use crate::ports::types::{Actor, ActorKind, ApprovalId, Verdict};

        let conv = |thread: Option<&str>, parent: Option<u64>| ApprovalConversation {
            thread: thread.map(str::to_string),
            parent: parent.map(EventSeq::new),
        };
        let approval_conversation = move |id: &ApprovalId| match id.as_ref() {
            // Raised inside thread 7 of the desk channel.
            "appr-threaded" => Some(conv(Some("desk-finance"), Some(7))),
            // Raised straight in the same channel, outside any thread.
            "appr-flat" => Some(conv(Some("desk-finance"), None)),
            _ => None,
        };
        let in_thread = |chat: &str, parent: Option<u64>| CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: parent.map(EventSeq::new),
            text: "pay the invoice".into(),
            by: None,
            chat: Some(chat.to_string()),
            deliverable: None,
            attachments: Vec::new(),
        };
        let resolved = |id: &str| CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new(id),
            verdict: Verdict::Approve,
            by: Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            },
        };

        // A message asked inside a thread names both keys.
        assert_eq!(
            cycle_conversation(&[in_thread("desk-finance", Some(7))], approval_conversation),
            conv(Some("desk-finance"), Some(7)),
        );
        // A message asked straight in the channel names only the channel —
        // the pre-#435 behaviour, which must not change.
        assert_eq!(
            cycle_conversation(&[in_thread("desk-finance", None)], approval_conversation),
            conv(Some("desk-finance"), None),
        );
        // A follow-up cycle inherits the thread as well as the channel, so a
        // second sign-off re-parks under the same root rather than flat.
        assert_eq!(
            cycle_conversation(&[resolved("appr-threaded")], approval_conversation),
            conv(Some("desk-finance"), Some(7)),
        );
        assert_eq!(
            cycle_conversation(&[resolved("appr-flat")], approval_conversation),
            conv(Some("desk-finance"), None),
        );
        // The same thread twice is not ambiguous.
        assert_eq!(
            cycle_conversation(
                &[
                    in_thread("desk-finance", Some(7)),
                    resolved("appr-threaded")
                ],
                approval_conversation,
            ),
            conv(Some("desk-finance"), Some(7)),
        );

        // THE ASYMMETRY. One channel, two threads: the channel survives and
        // only the thread is dropped. Answering in the channel is exactly what
        // this batch did before #435, so the fallback is the old behaviour
        // rather than a new failure.
        assert_eq!(
            cycle_conversation(
                &[
                    in_thread("desk-finance", Some(7)),
                    in_thread("desk-finance", Some(9)),
                ],
                approval_conversation,
            ),
            conv(Some("desk-finance"), None),
            "a thread disagreement must cost the thread, never the channel",
        );
        // Threaded batched with flat is the same disagreement, both orders.
        for batch in [
            [
                in_thread("desk-finance", Some(7)),
                in_thread("desk-finance", None),
            ],
            [
                in_thread("desk-finance", None),
                in_thread("desk-finance", Some(7)),
            ],
        ] {
            assert_eq!(
                cycle_conversation(&batch, approval_conversation),
                conv(Some("desk-finance"), None),
            );
        }
        // Inheriting a thread that disagrees with the batch's own is the same
        // rule, one hop further out.
        assert_eq!(
            cycle_conversation(
                &[
                    in_thread("desk-finance", Some(9)),
                    resolved("appr-threaded"),
                ],
                approval_conversation,
            ),
            conv(Some("desk-finance"), None),
        );

        // A channel disagreement still costs everything — #379's rule, intact.
        // The thread must not survive its own channel.
        assert_eq!(
            cycle_conversation(
                &[
                    in_thread("desk-finance", Some(7)),
                    in_thread("agent-cfo", Some(7)),
                ],
                approval_conversation,
            ),
            ApprovalConversation::default(),
            "a parent without a channel is a sequence number with nothing to \
             resolve it against",
        );
        // And a rival trigger clears both keys, not just the channel.
        assert_eq!(
            cycle_conversation(
                &[
                    in_thread("desk-finance", Some(7)),
                    CompanyEvent::TaskDispatched {
                        task_id: "t-1".into(),
                        run_id: None,
                    },
                ],
                approval_conversation,
            ),
            ApprovalConversation::default(),
        );
    }

    #[tokio::test]
    async fn send_email_sends_for_established_recipient() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();
        rt.inbox()
            .append(
                rt.id(),
                &crate::ports::inbox::EmailRecord {
                    id: "1".into(),
                    inbox: "ceo".into(),
                    from_name: "".into(),
                    from_email: "known@ext.com".into(),
                    subject: "hi".into(),
                    body: "".into(),
                    at_millis: 0,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-send".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        let res = host
            .send_email(serde_json::json!({ "to": "known@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert_eq!(res.output["status"], "sent");
        assert_eq!(sender.sent().len(), 1);
    }

    /// Issue #232: the established-correspondent gate must not weaken as the
    /// inbox grows.
    ///
    /// [`InboxStore::messages`] returns **oldest-first**, so the old
    /// `messages(.., 500, 0)` scan only ever saw the 500 *oldest* messages.
    /// Past that size every newer correspondent read as unknown, and every
    /// reply to a real thread parked for approval — an approval queue nobody
    /// can distinguish from noise is an approval queue everyone rubber-stamps.
    ///
    /// So the correspondent here is filed **last**, past the old cap. Policy is
    /// `full` (every effect executes) to isolate the flags on the effect from
    /// the gate decision they feed: this asserts what the send path *believes*
    /// about the recipient, not what the policy did with that belief.
    #[tokio::test]
    async fn established_recipient_past_the_old_page_cap_is_not_first_time() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .with_mail(CompanyMail {
                sender: sender.clone(),
                smtp: test_smtp("ceo@acme.test"),
            })
            .build()
            .await
            .unwrap();

        let file = async |id: usize, from: &str| {
            rt.inbox()
                .append(
                    rt.id(),
                    &crate::ports::inbox::EmailRecord {
                        id: format!("m{id}"),
                        inbox: "ceo".into(),
                        from_name: String::new(),
                        from_email: from.to_string(),
                        subject: "hi".into(),
                        body: String::new(),
                        at_millis: id as u64,
                        read: false,
                        outbound: false,
                    },
                )
                .await
                .unwrap();
        };

        // 501 older messages from other people, so the real correspondent lands
        // at index 501 — one past the end of the old 500-message page.
        for i in 0..501 {
            file(i, &format!("filler{i}@ext.com")).await;
        }
        file(501, "known@ext.com").await;

        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc-deep".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );
        let res = host
            .send_email(serde_json::json!({ "to": "known@ext.com", "subject": "s", "body": "b" }))
            .await
            .unwrap();
        assert_eq!(res.output["status"], "sent");

        let (executed, parked) = host.into_outcomes();
        assert!(parked.is_empty(), "an established thread must not park");
        let effect = executed
            .iter()
            .find(|e| e.kind == EMAIL_SEND_KIND)
            .expect("the send path emitted an email.send effect");
        assert!(
            effect.established_thread,
            "a correspondent who wrote in past message 500 is still established"
        );
        assert!(
            !effect.first_time_counterparty,
            "a correspondent who wrote in is never a first-time counterparty"
        );
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    // -----------------------------------------------------------------------
    // Issue #176: delegation host arms + handed-task awareness.
    // -----------------------------------------------------------------------

    /// A manifest with an Engineering desk (`eng`, lead `eng1`) — for the desk
    /// resolution paths of `delegate_to_desk` and the awareness matcher.
    fn desk_manifest() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "chief"
            role = "Chief"
            tier = "orchestrator"

            [[agent]]
            id = "eng1"
            role = "Engineer"

            [[group_chat]]
            id = "eng"
            name = "Engineering"
            members = ["eng1"]

            [policy]
            mode = "full"
            "#;
        toml::from_str(toml_src).expect("parse desk manifest")
    }

    /// A brain that records the text of every operator message it is handed, so
    /// a test can assert what awareness the kernel folded in before the brain.
    struct CapturingBrain {
        seen: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl Brain for CapturingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { text, .. } = event {
                    self.seen.lock().expect("seen").push(text.clone());
                }
            }
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "capture")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn spawn_task_arm_opens_a_board_card() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        let res = host
            .spawn_task(serde_json::json!({ "title": "  Ship it ", "assignee": " eng " }))
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(res.output["status"], "queued");

        let cards = rt.tasks().list(rt.id()).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Ship it");
        assert_eq!(cards[0].assignee, "eng");
        // Intake lands in To-do, never on the dispatch edge: a spawned card
        // must not spend an agent turn before an operator has seen it.
        assert_eq!(cards[0].column, COLUMN_TODO);

        // A blank title is a clean tool error, no card.
        let bad = host
            .spawn_task(serde_json::json!({ "title": "  " }))
            .await
            .unwrap();
        assert!(!bad.ok);
        assert_eq!(rt.tasks().list(rt.id()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delegate_to_desk_arm_records_handoff_and_rejects_unknown_desk() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        // Known desk (by name) → card assigned to the resolved desk id, lead noted.
        let ok = host
            .delegate_to_desk(
                serde_json::json!({ "desk": "Engineering", "instruction": "build invoicing" }),
            )
            .await
            .unwrap();
        assert!(ok.ok);
        assert_eq!(ok.output["desk"], "eng");
        assert_eq!(ok.output["lead"], "eng1");

        // Unknown desk → clean error, no card.
        let bad = host
            .delegate_to_desk(serde_json::json!({ "desk": "Legal", "instruction": "review" }))
            .await
            .unwrap();
        assert!(!bad.ok);
        assert_eq!(bad.output["status"], "unknown_desk");

        let cards = rt.tasks().list(rt.id()).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].assignee, "eng");
        // Same as the spawn path: a handoff opens a card, it does not dispatch.
        assert_eq!(cards[0].column, COLUMN_TODO);
    }

    /// Issue #1872 (codex): the hosted path refuses an `auto` channel, and
    /// says why.
    ///
    /// This path deliberately does **not** refuse an ordinary leadless desk —
    /// a hosted hand-off is a durable card, visible on the board whether or
    /// not anyone leads the desk yet. An auto channel is different in kind: it
    /// has no lead by design and never will, so accepting one wrote a card
    /// noting "no lead member on the roster yet", which is false about a
    /// staffed channel and permanently so — and it disagreed with the
    /// built-in tool, which refuses. Remove the guard and this opens a card.
    #[tokio::test]
    async fn delegate_to_desk_refuses_an_auto_channel_on_the_hosted_path() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .build()
            .await
            .unwrap();
        let mut record = rt.store().load(rt.id()).await.unwrap().unwrap();
        record.overlay_desks.push(crate::ports::types::OverlayDesk {
            id: "launch".to_string(),
            name: "Launch week".to_string(),
            description: None,
            members: vec!["eng1".to_string()],
            responder: crate::ports::types::ResponderMode::Auto,
        });
        rt.store().save(&record).await.unwrap();

        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );
        let refused = host
            .delegate_to_desk(
                serde_json::json!({ "desk": "launch", "instruction": "ship the launch" }),
            )
            .await
            .unwrap();
        assert!(!refused.ok, "{:?}", refused.output);
        assert_eq!(refused.output["status"], "no_lead");
        let error = refused.output["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("picked per message"),
            "the refusal says why, rather than reusing the leadless-desk wording: {error}"
        );
        assert!(
            rt.tasks().list(rt.id()).await.unwrap().is_empty(),
            "a refused hand-off opens no card"
        );
    }

    #[tokio::test]
    async fn call_tool_dispatches_delegation_tools() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "cyc".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        // Reached through the CycleHost trait exactly as the hosted brain does.
        let res = host
            .call_tool(ToolCall {
                tool: SPAWN_TASK_TOOL.to_string(),
                args: serde_json::json!({ "title": "via call_tool" }),
            })
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(rt.tasks().list(rt.id()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fallback_call_tool_parks_an_explicit_approval_request() {
        let home_dir = tmp_home();
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .build()
            .await
            .unwrap();
        let host = CycleHostImpl::new(
            rt.id().clone(),
            "fallback-approval".into(),
            &rt,
            None,
            false,
            ApprovalConversation::default(),
        );

        let result = host
            .call_tool(ToolCall {
                tool: crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND.to_string(),
                args: serde_json::json!({
                    "title": "Submit filing",
                    "question": "May I submit it?"
                }),
            })
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.output["status"], "pending");
        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].kind,
            crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND
        );
    }

    #[tokio::test]
    async fn handed_task_awareness_surfaces_open_cards_on_a_direct_query() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .with_brain(Arc::new(CapturingBrain { seen: seen.clone() }))
            .build()
            .await
            .unwrap();

        // Hand work to the Engineering desk (card assigned to the desk id).
        rt.tasks()
            .upsert(
                rt.id(),
                &TaskRecord {
                    id: "t1".into(),
                    title: "Ship invoicing".into(),
                    note: Some("build the importer".into()),
                    column: COLUMN_TODO.into(),
                    priority: "medium".into(),
                    assignee: "eng".into(),
                    updated_at_millis: 0,
                    origin_chat_id: None,
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
                    bounced: None,
                },
            )
            .await
            .unwrap();

        // Asking the desk directly (by name) surfaces the handed task...
        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "what are you working on?".into(),
            by: None,
            chat: Some("Engineering".into()),
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        // ...and asking with no address (the orchestrator) does NOT get the
        // desk's briefing folded into it.
        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "status?".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].contains("Open work already handed to you")
                && seen[0].contains("Ship invoicing"),
            "direct query carries the briefing: {:?}",
            seen[0]
        );
        assert!(
            !seen[1].contains("Open work already handed to you"),
            "unaddressed query has no desk briefing: {:?}",
            seen[1]
        );
    }

    #[tokio::test]
    async fn awareness_skips_done_cards() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
            .with_brain(Arc::new(CapturingBrain { seen: seen.clone() }))
            .build()
            .await
            .unwrap();
        rt.tasks()
            .upsert(
                rt.id(),
                &TaskRecord {
                    id: "t1".into(),
                    title: "Already finished".into(),
                    note: None,
                    column: "done".into(),
                    priority: "medium".into(),
                    assignee: "eng".into(),
                    updated_at_millis: 0,
                    origin_chat_id: None,
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
                    bounced: None,
                },
            )
            .await
            .unwrap();
        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "what's up?".into(),
            by: None,
            chat: Some("eng".into()),
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(
            !seen[0].contains("Open work already handed to you"),
            "done cards are not surfaced as open work: {:?}",
            seen[0]
        );
    }

    // -----------------------------------------------------------------------
    // Standing grants (issue #374)
    // -----------------------------------------------------------------------

    /// A harness tool call the operator IS allowed to grant broadly.
    ///
    /// `harness_effect` deliberately uses `Sign` and a real amount, because it
    /// exists to prove the effect was not executed. Both would refuse a broad
    /// scope, so the grantable case needs its own fixture.
    ///
    /// **The tool passed in is now load-bearing** (issue #444). These tests
    /// used to grant a standing scope on `workspace_write` — which was
    /// grantable only because its name contains no consequence word, while the
    /// parking side of the same gate refused to exempt it precisely because it
    /// overwrites guidance the operator wrote. That contradiction is what #444
    /// is about, and it is resolved in the direction the parking side already
    /// argued: `workspace_write` stays a per-call decision. `file_write` is the
    /// honest fixture — it mutates, so it still parks, but what it mutates is
    /// the agent's own sandboxed workspace, which is exactly the low-consequence
    /// shape a standing grant is for.
    fn grantable_effect(agent: &str, tool: &str, args: serde_json::Value) -> Effect {
        Effect {
            kind: tool.into(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: args,
            agent: Some(agent.to_string()),
            run_id: None,
        }
    }

    fn in_an_hour() -> u64 {
        now_millis() + 60 * 60 * 1000
    }

    fn tool_scope() -> GrantScope {
        GrantScope::Tool {
            expires_at_millis: in_an_hour(),
        }
    }

    /// Parks `effect` the way a blocked harness tool call actually parks —
    /// through `park_effect`, which bypasses the manifest gate's `evaluate`.
    ///
    /// `park_one` cannot serve here: it routes through `emit_effect`, and the
    /// manifest gate auto-allows `EffectGroup::Other` under supervised. That is
    /// correct for a native effect and irrelevant to a harness one, whose park
    /// decision was already made inside the agent's turn by `ApprovalPolicy`.
    async fn park_one_blocked_tool_call(
        home: std::path::PathBuf,
        effect: Effect,
    ) -> (Arc<CompanyRuntime>, ApprovalId) {
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain { effect }))
                .build()
                .await
                .unwrap(),
        );
        let report = rt
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "do it".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();
        assert_eq!(report.parked.len(), 1);
        let id = report.parked[0].clone();
        (rt, id)
    }

    /// Like [`park_one_blocked_tool_call`], but parks the same effect **twice**
    /// — two cycles, two identical cards on one runtime.
    ///
    /// The ordering matters and is why this exists: the deny/grant reconcile
    /// tests need both cards parked before either is resolved, because once a
    /// standing deny is live the identical call is denied inline and never
    /// parks again.
    async fn park_two_blocked_tool_calls(
        home: std::path::PathBuf,
        effect: Effect,
    ) -> (Arc<CompanyRuntime>, Vec<ApprovalId>) {
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain { effect }))
                .build()
                .await
                .unwrap(),
        );
        let mut ids = Vec::new();
        for text in ["do it", "again"] {
            let report = rt
                .run_cycle(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: text.into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }])
                .await
                .unwrap();
            assert_eq!(report.parked.len(), 1);
            ids.push(report.parked[0].clone());
        }
        (rt, ids)
    }

    /// The headline: approving with the broader scope arms a standing grant, and
    /// mints **no** single-use grant beside it.
    ///
    /// The second half is not tidiness. A redundant single-use grant would go
    /// unredeemed — the standing grant already admits the re-issued call — and
    /// fifteen minutes later the TTL sweep would tell the operator "the agent
    /// didn't act", about work that ran immediately.
    #[tokio::test]
    async fn approving_with_a_tool_scope_mints_a_standing_grant_and_no_single_use_one() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect("ops", "file_write", serde_json::json!({ "path": "a" })),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;

        assert_eq!(rt.grants.standing_count(), 1);
        assert_eq!(
            rt.grants.live_count(),
            0,
            "no single-use grant is left behind to expire noisily"
        );
        let listed = rt.standing_grants();
        assert_eq!(listed[0].tool, "file_write");
        assert_eq!(listed[0].agent, "ops");
        assert_eq!(listed[0].approval_id, id, "provenance back to the card");
        assert_eq!(
            listed[0].granted_by.id, "owner",
            "the resolving actor is recorded, not a placeholder"
        );
    }

    /// Issue #457: the grant records **which provider the card was about**.
    ///
    /// `composio_execute` carries every action of every connected toolkit under
    /// one name, so a grant that recorded only the name turned "read from
    /// GitHub" — the sentence on the card — into "make any Composio read,
    /// anywhere". The toolkit is read off the parked effect's own payload, so
    /// what is stored is what the operator was shown.
    ///
    /// Gated on the harness feature because the toolkit comes from the vendored
    /// catalogue; the default build cannot mint a Composio standing grant at all
    /// (every action reads as a send there), which
    /// `without_the_catalogue_every_composio_action_is_a_send` pins.
    #[tokio::test]
    #[cfg(feature = "openhuman")]
    async fn a_standing_grant_on_a_composio_read_records_the_toolkit_it_was_shown_for() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                crate::policy::consequence::COMPOSIO_EXECUTE,
                serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
            ),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;

        let listed = rt.standing_grants();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].scope.as_deref(),
            Some("github"),
            "the grant has to remember which account the operator was looking at"
        );
    }

    /// The counterpart: a tool whose name already is the whole of what it can do
    /// records no scope, so its grant matches exactly as it always did.
    #[tokio::test]
    async fn a_standing_grant_on_an_ordinary_tool_records_no_scope() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect("ops", "file_write", serde_json::json!({ "path": "a" })),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;

        assert_eq!(
            rt.standing_grants()[0].scope,
            None,
            "there is nothing to narrow `file_write` to"
        );
    }

    /// Issue #1458, from the deny side: a standing denial minted against a
    /// scoped tool remembers **which slice** the operator refused.
    ///
    /// The mint used to re-read the journal's payload-scrubbed copy of the
    /// effect (issue #351), whose `Null` payload made `standing_scope_of`
    /// answer `None` — and a stored `None` is a wildcard in `admits_scope`, so
    /// refusing one web origin blocked every origin for that teammate until
    /// expiry. The resolve now carries the parked effect whole, so the deny
    /// records the same scope the card showed.
    #[tokio::test]
    async fn a_standing_deny_on_a_scoped_tool_keeps_the_scope_it_was_shown_for() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                crate::policy::consequence::WEB_FETCH,
                serde_json::json!({ "url": "https://docs.rs/x" }),
            ),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Deny, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;

        let listed = rt.standing_grants();
        assert_eq!(listed.len(), 1, "one standing denial is minted");
        assert_eq!(listed[0].verdict, Verdict::Deny);
        assert_eq!(
            listed[0].scope.as_deref(),
            Some("https://docs.rs"),
            "the deny records the origin the operator refused, not a wildcard"
        );
    }

    /// Issue #1458: when two identical cards park and the operator resolves the
    /// first as a standing **denial** and the second as a standing **approval**,
    /// the newer decision wins. `ApprovalPolicy` checks a deny above a standing
    /// grant, so without reconciliation the approval would list as a live
    /// permission and never admit a call until the refusal expired — the
    /// operator's later "yes" silently inert.
    #[tokio::test]
    async fn a_new_standing_approval_revokes_an_older_standing_denial_for_the_same_scope() {
        let home_dir = tmp_home();
        let (rt, ids) = park_two_blocked_tool_calls(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                crate::policy::consequence::WEB_FETCH,
                serde_json::json!({ "url": "https://docs.rs/x" }),
            ),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&ids[0], Verdict::Deny, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;
        assert_eq!(
            rt.standing_grants()[0].verdict,
            Verdict::Deny,
            "the first resolution arms a standing denial"
        );

        let (_, follow_up) = rt
            .resolve_approval_spawned(&ids[1], Verdict::Approve, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;

        let listed = rt.standing_grants();
        assert_eq!(listed.len(), 1, "the deny is revoked, not left shadowing");
        assert_eq!(listed[0].verdict, Verdict::Approve);
        assert_eq!(
            listed[0].scope.as_deref(),
            Some("https://docs.rs"),
            "the surviving policy keeps the scope both were minted for"
        );
    }

    /// The mirror direction: a standing **denial** minted after a standing
    /// **approval** of the same scope revokes the grant. Enforcement would
    /// already have the deny win, but a listed-but-dead grant is a wrong
    /// contract for the operator who approved it.
    #[tokio::test]
    async fn a_new_standing_denial_revokes_an_older_standing_approval_for_the_same_scope() {
        let home_dir = tmp_home();
        let (rt, ids) = park_two_blocked_tool_calls(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                crate::policy::consequence::WEB_FETCH,
                serde_json::json!({ "url": "https://docs.rs/x" }),
            ),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&ids[0], Verdict::Approve, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;
        assert_eq!(
            rt.standing_grants()[0].verdict,
            Verdict::Approve,
            "the first resolution arms a standing grant"
        );

        let (_, follow_up) = rt
            .resolve_approval_spawned(&ids[1], Verdict::Deny, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;

        let listed = rt.standing_grants();
        assert_eq!(listed.len(), 1, "the grant is revoked by the newer refusal");
        assert_eq!(listed[0].verdict, Verdict::Deny);
    }

    /// Issue #1458 under concurrency: two opposite-polarity resolutions of the
    /// **same** scope settled while both are in flight — the approve and the
    /// deny each half-finished before either mints — must still leave a single
    /// policy, not the deny permanently shadowing the approve.
    ///
    /// Before the reconcile lock this could interleave: the journal appends
    /// between the [`opposite_polarity`] snapshot and the `grant_standing`
    /// insert are awaited, so a concurrent settle gets polled in that window,
    /// snapshots the same empty opposite set, and then both insert. Because
    /// `ApprovalPolicy` matches a standing denial above a standing grant, the
    /// approve then sits listed but never admits a call whatever the operator's
    /// true order. The lock serialises the two mints, so the second observes
    /// the first's policy and supersedes it — the same single-policy state the
    /// sequential tests above assert.
    #[tokio::test]
    async fn concurrent_opposite_polarity_resolutions_leave_one_policy() {
        let home_dir = tmp_home();
        let (rt, ids) = park_two_blocked_tool_calls(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                crate::policy::consequence::WEB_FETCH,
                serde_json::json!({ "url": "https://docs.rs/x" }),
            ),
        )
        .await;

        let (a, b) = tokio::join!(
            rt.resolve_approval_spawned(&ids[0], Verdict::Approve, operator(), tool_scope()),
            rt.resolve_approval_spawned(&ids[1], Verdict::Deny, operator(), tool_scope()),
        );
        let (_, follow_up_a) = a.unwrap();
        let (_, follow_up_b) = b.unwrap();
        let _ = tokio::join!(
            crate::company::runtime::join_follow_up(follow_up_a),
            crate::company::runtime::join_follow_up(follow_up_b),
        );

        let listed = rt.standing_grants();
        assert_eq!(
            listed.len(),
            1,
            "the concurrent resolutions must not leave both polarities live"
        );
    }

    /// A scope the runtime must not honour changes **nothing**: the approval is
    /// still parked and no verdict was journaled.
    ///
    /// This is why the check runs before `resolve_outcome`. Validating after it
    /// would have dropped the card from the queue and recorded a resolution,
    /// leaving the operator with nothing to re-decide and a verdict whose effect
    /// never happened.
    #[tokio::test]
    async fn a_refused_scope_leaves_the_approval_parked_and_unjournaled() {
        for effect in [
            // A named consequence group — stays a per-call decision.
            harness_effect("finance", "composio_execute", serde_json::json!({})),
            // A native effect — no teammate and no tool to grant.
            Effect {
                kind: EMAIL_SEND_KIND.into(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({ "channel": "operator", "text": "hi" }),
                agent: None,
                run_id: None,
            },
        ] {
            let home_dir = tmp_home();
            let (rt, id) =
                park_one_blocked_tool_call(home_dir.path().to_path_buf(), effect.clone()).await;

            let err = rt
                .resolve_approval_spawned(&id, Verdict::Approve, operator(), tool_scope())
                .await
                .expect_err("a scope the host cannot honour is refused");
            assert!(
                matches!(err, OpenCompanyError::InvalidRequest(_)),
                "refusal must be a bad-request, not a server fault: {err:?}"
            );

            assert_eq!(
                rt.pending_approvals().len(),
                1,
                "the card is still there to be decided: {}",
                effect.kind
            );
            assert_eq!(rt.grants.standing_count(), 0);
            assert_eq!(rt.grants.live_count(), 0);

            // And the card is still decidable — nothing about the refused
            // request consumed it. Declining rather than approving, so this
            // asserts the queue state without dragging in whether the host has
            // a mailer wired for the native case.
            rt.resolve_approval(&id, Verdict::Deny, operator())
                .await
                .unwrap();
            assert!(rt.pending_approvals().is_empty());
        }
    }

    /// Issue #1458: a standing **denial** for a workflow is refused at the
    /// edge — the workflow gate does not enforce a `Deny` verdict
    /// (`src/workflows/gate.rs`), so a time-bounded refusal would be a control
    /// that never took effect. The card stays parked so the operator can still
    /// deny it once.
    #[tokio::test]
    async fn a_standing_deny_on_a_workflow_gate_is_refused_and_mints_nothing() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            Effect {
                kind: crate::runtime::workflow_resume::WORKFLOW_APPROVE_KIND.to_string(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({
                    "workflow_id": "sports_digest",
                    "node_id": "fetch_bbc",
                    "tool": "web_fetch",
                    "args": { "url": "https://docs.rs/x" },
                }),
                agent: None,
                run_id: None,
            },
        )
        .await;

        let err = match rt
            .resolve_approval_spawned(&id, Verdict::Deny, operator(), tool_scope())
            .await
        {
            Ok(_) => panic!("a workflow standing denial must be refused at the edge"),
            Err(err) => err,
        };
        assert!(
            matches!(
                err,
                OpenCompanyError::InvalidRequest(ref msg)
                    if msg.contains("'web_fetch' is a workflow call")
                        && msg.contains("does not enforce a standing refusal")
            ),
            "{err:?}"
        );

        assert_eq!(
            rt.pending_approvals().len(),
            1,
            "the card is still there to be denied once"
        );
        assert_eq!(rt.grants.standing_count(), 0, "no refusal is minted");
        assert_eq!(rt.grants.live_count(), 0);

        // And the operator can still deny it once — the refused request did not
        // consume the card.
        rt.resolve_approval(&id, Verdict::Deny, operator())
            .await
            .unwrap();
        assert!(rt.pending_approvals().is_empty());
    }

    /// Issue #1458, the console half: a workflow-gate card must not offer a
    /// standing **denial**.
    ///
    /// `check_broadly_scoped` refuses a workflow standing denial with a 400 —
    /// the gate does not enforce a `Deny` verdict — so a card that advertised
    /// the control would let the operator click "don't ask again" and get an
    /// error that leaves the approval parked. The grant half is still offered:
    /// a workflow *can* hold a standing permission. Only the deny control is
    /// withheld.
    #[tokio::test]
    async fn a_workflow_gate_card_is_not_advertised_as_broadly_deniable() {
        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            Effect {
                kind: crate::runtime::workflow_resume::WORKFLOW_APPROVE_KIND.to_string(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({
                    "workflow_id": "sports_digest",
                    "node_id": "fetch_bbc",
                    "tool": "web_fetch",
                    "args": { "url": "https://docs.rs/x" },
                }),
                agent: None,
                run_id: None,
            },
        )
        .await;

        assert!(
            !rt.pending_approvals()[0].broadly_deniable,
            "a workflow card must not advertise a standing refusal nothing enforces"
        );
        assert!(
            rt.pending_approvals()[0].broadly_grantable,
            "a workflow card can still hold a standing permission"
        );

        // The same tool, parked from an agent turn, offers the deny control.
        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                crate::policy::consequence::WEB_FETCH,
                serde_json::json!({ "url": "https://docs.rs/x" }),
            ),
        )
        .await;
        assert!(rt.pending_approvals()[0].broadly_deniable);
    }

    /// The default scope is byte-identical to pre-#374 behaviour.
    ///
    /// The existing suite passing untouched is the real proof; this pins the
    /// negative the suite cannot state — that no number of ordinary approvals
    /// ever *infers* a standing grant. A "we noticed you approve this a lot"
    /// heuristic is the silent accumulation the issue forbids.
    #[tokio::test]
    async fn repeated_ordinary_approvals_never_infer_a_standing_grant() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let effect = grantable_effect("ops", "file_write", serde_json::json!({ "path": "a" }));

        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .with_brain(Arc::new(ParkingBrain {
                    effect: effect.clone(),
                }))
                .build()
                .await
                .unwrap(),
        );

        for _ in 0..5 {
            let report = rt
                .run_cycle(vec![CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "do it".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                }])
                .await
                .unwrap();
            let id = report.parked[0].clone();
            rt.resolve_approval(&id, Verdict::Approve, operator())
                .await
                .unwrap();
        }

        assert_eq!(
            rt.grants.standing_count(),
            0,
            "a standing grant is only ever asked for, never inferred"
        );
    }

    /// Standing grants survive a restart, and revoking one is durable too.
    #[tokio::test]
    async fn a_standing_grant_replays_on_boot_and_a_revoked_one_does_not() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let (rt, id) = park_one_blocked_tool_call(
            home.clone(),
            grantable_effect("ops", "file_write", serde_json::json!({ "path": "a" })),
        )
        .await;

        let (_, follow_up) = rt
            .resolve_approval_spawned(&id, Verdict::Approve, operator(), tool_scope())
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;
        let grant_id = rt.standing_grants()[0].id.clone();

        // A fresh runtime over the same home rehydrates it.
        let rt2 = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest("supervised"))
                .build()
                .await
                .unwrap(),
        );
        rt2.recover().await.unwrap();
        assert_eq!(rt2.grants.standing_count(), 1);
        assert_eq!(rt2.standing_grants()[0].id, grant_id);

        // Revoke, then boot again: it must stay gone.
        assert!(
            rt2.revoke_standing_grant(&grant_id, operator())
                .await
                .unwrap()
        );
        assert_eq!(rt2.grants.standing_count(), 0);
        assert!(
            !rt2.revoke_standing_grant(&grant_id, operator())
                .await
                .unwrap(),
            "revoking twice reports nothing to revoke"
        );

        let rt3 = Arc::new(
            RuntimeBuilder::new(home, manifest("supervised"))
                .build()
                .await
                .unwrap(),
        );
        rt3.recover().await.unwrap();
        assert_eq!(
            rt3.grants.standing_count(),
            0,
            "a restart must not hand back a permission the operator took away"
        );
    }

    /// The maintenance sweep retires a lapsed standing grant and journals it.
    #[tokio::test]
    async fn the_sweep_expires_a_lapsed_standing_grant() {
        let home_dir = tmp_home();
        let (rt, id) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect("ops", "file_write", serde_json::json!({})),
        )
        .await;

        // Already past its deadline the moment it is minted.
        let (_, follow_up) = rt
            .resolve_approval_spawned(
                &id,
                Verdict::Approve,
                operator(),
                GrantScope::Tool {
                    expires_at_millis: 1,
                },
            )
            .await
            .unwrap();
        let _ = crate::company::runtime::join_follow_up(follow_up).await;
        assert_eq!(rt.grants.standing_count(), 1);

        rt.sweep_expired_grants().await.unwrap();
        assert_eq!(rt.grants.standing_count(), 0);
    }

    /// The summary carries the flag only where the control is actually
    /// offerable — and what the tool can reach is what decides it.
    #[tokio::test]
    async fn the_summary_marks_only_broadly_grantable_cards() {
        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect("ops", "file_write", serde_json::json!({})),
        )
        .await;
        assert!(rt.pending_approvals()[0].broadly_grantable);

        // A Composio call with no action slug the classifier recognises reads
        // as a send, so no scope control is offered (issue #441's cautious
        // direction — before it, *every* Composio call landed here, including
        // the reads).
        let home_dir = tmp_home();
        let (rt, _) = park_one(
            home_dir.path().to_path_buf(),
            harness_effect("finance", "composio_execute", serde_json::json!({})),
        )
        .await;
        assert!(!rt.pending_approvals()[0].broadly_grantable);

        // Issue #444: `workspace_write` used to be marked grantable, because
        // its name carries no consequence word. It overwrites guidance the
        // operator wrote, so it stays a per-call decision — the same answer
        // the parking side of the gate has always given for it.
        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect("ops", "workspace_write", serde_json::json!({ "path": "a" })),
        )
        .await;
        assert!(
            !rt.pending_approvals()[0].broadly_grantable,
            "overwriting operator-owned guidance is not a week-long permission"
        );

        // And neither is running an arbitrary command, which is where an
        // operator on staging *could* get a standing grant before #444.
        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect("ops", "shell", serde_json::json!({ "command": "ls" })),
        )
        .await;
        assert!(!rt.pending_approvals()[0].broadly_grantable);
    }

    /// Issue #441, from the mint side: the same tool, two different answers,
    /// decided by the action in the arguments rather than the name they share.
    ///
    /// This is the whole shape of the bug — an operator could grant a standing
    /// scope on running arbitrary terminal commands, and could not grant one on
    /// reading a repository's pull requests.
    #[tokio::test]
    #[cfg(feature = "openhuman")]
    async fn a_composio_read_is_offerable_and_a_composio_send_is_not() {
        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                "composio_execute",
                serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
            ),
        )
        .await;
        assert!(
            rt.pending_approvals()[0].broadly_grantable,
            "a repository read scoped to a connected account is grantable"
        );

        let home_dir = tmp_home();
        let (rt, _) = park_one_blocked_tool_call(
            home_dir.path().to_path_buf(),
            grantable_effect(
                "ops",
                "composio_execute",
                serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
            ),
        )
        .await;
        assert!(
            !rt.pending_approvals()[0].broadly_grantable,
            "sending mail stays a per-call decision"
        );
    }

    // ── Issue #983: the pre-journaled entry point ────────────────────────────

    /// Every input of a cycle appears in the journal **exactly once**, whichever
    /// entry point drove it.
    ///
    /// This is the pin the plumbing change is worth having. `run_cycle` is what
    /// every other trigger in the tree uses — the scheduler, cron, webhooks, the
    /// telegram poller, delegation, approval follow-ups — so the append it does
    /// must stay exactly one per input; and `run_journaled_cycle`, which exists
    /// so the chat route can append at accept time instead, must do none. Either
    /// half getting it wrong is invisible at the call site and shows up as a
    /// duplicated (or missing) message in somebody's transcript.
    ///
    /// It also pins `CycleReport::input_seqs`, and therefore the chat response's
    /// `messageId`: the pre-journaled path reports back the seqs it was handed,
    /// not seqs of its own.
    #[tokio::test]
    async fn each_input_is_journaled_exactly_once_by_either_entry_point() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let rt = RuntimeBuilder::new(home, manifest("full"))
            .with_brain(Arc::new(CapturingBrain {
                seen: Arc::clone(&seen),
            }))
            .build()
            .await
            .unwrap();

        let ask = |text: &str| CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: text.to_string(),
            by: None,
            chat: None,
            parent: None,
            deliverable: None,
            attachments: Vec::new(),
        };
        let messages = |stored: &[crate::ports::types::StoredEvent]| -> Vec<String> {
            stored
                .iter()
                .filter_map(|s| match &s.event {
                    CompanyEvent::OperatorMessage { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };

        // The appending wrapper: one line per input, and the report names them.
        let appended = rt.run_cycle(vec![ask("first")]).await.unwrap();
        let stored = rt
            .events()
            .read_from(rt.id(), EventSeq::new(0), 1_000)
            .await
            .unwrap();
        assert_eq!(
            messages(&stored),
            ["first"],
            "the appending entry point wrote its input exactly once"
        );
        assert_eq!(appended.input_seqs.len(), 1);
        assert_eq!(
            stored
                .iter()
                .find(|s| matches!(&s.event, CompanyEvent::OperatorMessage { text, .. } if text == "first"))
                .map(|s| s.seq),
            appended.input_seqs.first().copied(),
            "the reported seq is the one the message was appended under"
        );

        // The pre-journaled entry point: the caller's append is the only one.
        let pre = rt.events().append(rt.id(), ask("second")).await.unwrap();
        let journaled = rt
            .run_journaled_cycle(vec![(pre, ask("second"))], None)
            .await
            .unwrap();
        let stored = rt
            .events()
            .read_from(rt.id(), EventSeq::new(0), 1_000)
            .await
            .unwrap();
        assert_eq!(
            messages(&stored),
            ["first", "second"],
            "the pre-journaled entry point appended its input a second time"
        );
        assert_eq!(
            journaled.input_seqs,
            vec![pre],
            "the report carries the seq the caller supplied, not one of its own"
        );

        // And the brain saw both, so skipping the append did not skip the input.
        assert_eq!(*seen.lock().expect("seen"), ["first", "second"]);
    }

    /// A pre-journaled cycle moves the caller's run row to `Running` **inside**
    /// the serial lock, and leaves settling it to the caller.
    ///
    /// Both halves matter. Starting the row outside the lock would make
    /// `Running` mean "accepted" rather than "owns the lock", which is exactly
    /// the queued-behind-another-turn wait an operator needs to see. And letting
    /// the cycle's terminality backstop settle it would close the row while the
    /// task that journals the turn's replies is still running.
    #[tokio::test]
    async fn a_journaled_cycle_starts_the_callers_run_and_leaves_it_running() {
        use crate::ports::runs::{NewRun, RunStatus};

        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = RuntimeBuilder::new(home, manifest("full"))
            .build()
            .await
            .unwrap();
        rt.runs()
            .create_run(rt.id(), NewRun::for_chat("turn-1", "general", "ceo"))
            .await
            .unwrap();

        let seq = rt
            .events()
            .append(
                rt.id(),
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "hello".into(),
                    by: None,
                    chat: None,
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .unwrap();
        rt.run_journaled_cycle(
            vec![(
                seq,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    text: "hello".into(),
                    by: None,
                    chat: None,
                    parent: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )],
            Some("turn-1".to_string()),
        )
        .await
        .unwrap();

        let row = rt
            .runs()
            .get_run(rt.id(), "turn-1")
            .await
            .unwrap()
            .expect("the row survives the cycle");
        assert_eq!(
            row.status,
            RunStatus::Running,
            "the cycle started the row and must not have settled it"
        );
        assert_eq!(
            row.trigger_event_seq,
            Some(seq),
            "the row is stamped with the seq the caller supplied"
        );
    }

    /// **Issue #1739.** A cycle reports one `turn_finished`, and the operator's
    /// own words are not in it.
    ///
    /// The message text here is the thing the payload must never carry, so it is
    /// deliberately distinctive: the assertion is a substring search over the
    /// whole rendered event, which fails if any field ever starts holding
    /// free-form text.
    #[tokio::test]
    async fn a_cycle_reports_its_shape_and_not_the_operators_words() {
        let home_dir = tmp_home();
        let recorder = Arc::new(crate::analytics::RecordingTracker::new());
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .with_analytics(recorder.clone())
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "acquire Northwind Traders for 4.2 million".into(),
            by: None,
            chat: None,
            parent: None,
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        let turns: Vec<_> = recorder
            .events()
            .into_iter()
            .filter(|event| matches!(event, crate::analytics::Event::TurnFinished { .. }))
            .collect();
        assert_eq!(turns.len(), 1, "one cycle, one event: {turns:?}");

        match turns[0] {
            crate::analytics::Event::TurnFinished {
                trigger,
                outcome,
                failure,
                ..
            } => {
                assert_eq!(trigger, crate::analytics::Trigger::OperatorMessage);
                assert_eq!(outcome, crate::analytics::Outcome::Ok);
                assert_eq!(failure, None);
            }
            ref other => panic!("{other:?}"),
        }

        let rendered = format!("{:?}", turns[0]);
        assert!(
            !rendered.contains("Northwind"),
            "the operator's message reached the payload: {rendered}"
        );
    }

    /// `turn_finished` counts the effects the cycle actually performed.
    ///
    /// These two numbers used to be read off `CycleReport`, which exists only
    /// on the success path — so every failed cycle reported zero effects and
    /// zero parked approvals, including one that executed an irreversible
    /// effect and *then* hit an adapter error on the way out. That is a
    /// systematic undercount of exactly the turns worth looking at.
    ///
    /// They now come from the host, read before the fallible tail of
    /// `run_locked` rather than after it. This covers the reading being
    /// faithful: a cycle whose brain emits one effect reports one. The failure
    /// case is covered by where the read happens — `*effects = host.counts()`
    /// sits above `let result = result?;` and above every `?` that follows, so
    /// no later error can reach the tracker with the counts unset.
    #[tokio::test]
    async fn a_cycle_reports_the_effects_it_actually_performed() {
        let home_dir = tmp_home();
        let recorder = Arc::new(crate::analytics::RecordingTracker::new());
        let effect = Effect {
            kind: "noop".into(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };
        let rt = RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("full"))
            .with_brain(Arc::new(EffectBrain { effect }))
            .with_analytics(recorder.clone())
            .build()
            .await
            .unwrap();

        rt.run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "do the thing".into(),
            by: None,
            chat: None,
            parent: None,
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

        let turns: Vec<_> = recorder
            .events()
            .into_iter()
            .filter(|event| matches!(event, crate::analytics::Event::TurnFinished { .. }))
            .collect();
        assert_eq!(turns.len(), 1, "one cycle, one event: {turns:?}");

        match turns[0] {
            crate::analytics::Event::TurnFinished {
                effects_executed,
                approvals_parked,
                ..
            } => {
                assert_eq!(
                    effects_executed + approvals_parked,
                    1,
                    "the cycle's one effect must be counted, executed or parked: {:?}",
                    turns[0]
                );
            }
            ref other => panic!("{other:?}"),
        }
    }
}
