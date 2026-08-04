//! [`HarnessBrain`]: the cognition [`Brain`] backed by the embedded OpenHuman
//! runtime.
//!
//! Where [`EchoBrain`](crate::brain::EchoBrain) turns every operator message
//! into `"You said: …"`, `HarnessBrain` routes it to a live openhuman
//! [`Agent`](openhuman_core::openhuman::agent::Agent) through a
//! [`HarnessPool`], so the reply comes from the hosted brain and the turn's
//! token/cost usage is metered into the company ledger.
//!
//! The default chat responder is the company **orchestrator** (issue #53): the
//! roster agent tagged `tier = "orchestrator"`, or the first agent when none is
//! (so a company without an orchestrator behaves exactly as before). An operator
//! message addressed to a desk (its `chat` field) is answered by that desk's
//! lead member; an unaddressed message goes to the orchestrator, which may
//! delegate — the queue its tools fill is drained here after its turn (v1:
//! synchronous, in-cycle, capped, no sub-agent re-delegation).
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::company::steer::{InflightEntry, InflightKind, SteerAction, cap_redirect};
use crate::harness::lifecycle::{self, TaskRunEnd};
use crate::harness::orchestrator;
// `Delegation` is only named by the test-only `run_delegation` wrapper and the
// delegation tests (via `use super::*`); the cycle path drives the runner's
// `handle_operator_message` and never spells the type out.
#[cfg(test)]
use crate::harness::orchestrator::Delegation;
use crate::harness::run_turn::HarnessRunTurn;
use crate::harness::{HarnessDeps, HarnessPool};
use crate::runtime::assignee;
use crate::runtime::delegation::{self, DelegationRunner, RunTurn};

/// The most operator redirects honored within a single task dispatch (issue
/// #111). A redirect re-runs the turn in-loop with the fresh instruction
/// appended; past this cap the run is finalized to its terminal column (see
/// [`lifecycle::success_terminal_column`]) so a redirect storm can't loop
/// forever.
const MAX_REDIRECTS_PER_DISPATCH: u32 = 3;

/// The `error` a dispatched attempt settles with when the company has no task
/// board wired at all — the card cannot even be read, so nothing was tried.
const NO_TASK_STORE: &str = "this company has no task board wired, so the card could not be run";

/// The `error` a dispatched attempt settles with when its card is gone by the
/// time the cycle reaches it (deleted, or never persisted).
const CARD_VANISHED: &str = "the card was gone by the time its dispatch ran";

use crate::harness::run_trace::RunTraceSink;
use crate::ports::artifacts::{ArtifactAuthor, ArtifactKind, ArtifactRecord};
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::runs::{RunOutcome, RunStatus};
use crate::ports::types::{
    CompanyEvent, CompanyRecord, CompressedTrace, CycleRequest, CycleResult, OutboundMessage,
    TokenUsage, TurnStep, TurnStepKind, TurnStepStatus, Verdict,
};
use crate::ports::{Cognition, TaskRecord, UsageMetering, generate_id, now_millis};

/// A [`Brain`] that answers with a live openhuman agent turn.
pub struct HarnessBrain {
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: CompanyRecord,
    responder: String,
    /// The attempt records a dispatched card writes into (issue #242).
    ///
    /// Held here rather than on [`HarnessDeps`] on purpose: every one of the
    /// ~28 `HarnessDeps` literals in this crate would otherwise have to be
    /// widened for a handle only the dispatch path reads — the same argument
    /// that put the grant set on `ApprovalRequestQueue` instead.
    ///
    /// `None` **fails silent, not closed**: the card still runs, its outcome
    /// still lands on the board and in the journal, and only the run record is
    /// missing. That is the right direction for a purely observational store —
    /// and it is why every test construction can leave it unset.
    runs: Option<Arc<dyn crate::ports::RunStore>>,
}

impl HarnessBrain {
    /// Builds a harness brain for `record`, answering unaddressed operator
    /// messages with the company orchestrator (the `tier = "orchestrator"` agent,
    /// else the first roster agent). The pool is shared so the roster is built
    /// once and reused across cycles.
    pub fn new(pool: Arc<HarnessPool>, deps: HarnessDeps, record: CompanyRecord) -> Self {
        let responder = orchestrator::orchestrator_id(&record.manifest.agents).unwrap_or_default();
        Self {
            pool,
            deps,
            record,
            responder,
            runs: None,
        }
    }

    /// Overrides which roster agent answers operator messages.
    pub fn with_responder(mut self, agent_id: impl Into<String>) -> Self {
        self.responder = agent_id.into();
        self
    }

    /// Wires the run store a dispatched card records its attempt into (#242).
    pub fn with_runs(mut self, runs: Arc<dyn crate::ports::RunStore>) -> Self {
        self.runs = Some(runs);
        self
    }

    /// Runs a dispatched card to completion and, when the card remembers the
    /// conversation it was spawned from, returns the reply to post back there
    /// (issue #151 §3.2).
    ///
    /// Loads the card, routes it to its assignee (or the default responder) for
    /// a single turn, and writes the outcome back onto the board — moved to its
    /// success terminal column on success (see
    /// [`lifecycle::success_terminal_column`]),
    /// back to `todo` with the error noted on failure. A missing task store
    /// or a card that has since vanished is a silent no-op.
    ///
    /// Before this the answer only ever reached `card.note`: the card runs
    /// asynchronously, long after the turn that spawned it has answered, so the
    /// operator had to know to go and look. The note is still written — it stays
    /// the durable record — and the post-back is additive.
    /// Re-dispatches the agent that holds a single-use grant for `approval_id`,
    /// instructing it to re-issue the exact call the operator approved
    /// (issue #243).
    ///
    /// Returns the agent's reply as a bubble on its own channel, or `None` when
    /// there is no grant to redeem — which is the common case and must stay a
    /// silent no-op:
    ///
    /// * a **denied** approval (this arm only runs on `Approve`, but a deny
    ///   reaching here must not turn into a turn either);
    /// * a **native** effect the runtime already executed, which mints nothing;
    /// * a **legacy** parked effect from before `Effect::agent` existed, which
    ///   replays as `None` and so mints nothing;
    /// * the **system expiry** `ApprovalResolved { verdict: Deny }` that #309's
    ///   sweep appends;
    /// * a grant already consumed or swept.
    ///
    /// The turn is registered on the steer registry with the same RAII guard
    /// `run_task` uses, so an operator can cancel a re-issue mid-flight and a
    /// crashed turn never strands a ghost row in the in-flight strip.
    async fn redispatch_granted_call(
        &self,
        approval_id: &crate::ports::types::ApprovalId,
    ) -> Result<Option<OutboundMessage>> {
        let Some(grant) = self.deps.approval_requests.grants().peek(approval_id) else {
            return Ok(None);
        };

        // Re-issue verbatim. The grant admits ONE call matching these arguments
        // exactly, so any drift the model introduces will simply re-park — the
        // instruction is emphatic because a re-worded argument silently costs the
        // operator a second approval round-trip.
        let args = serde_json::to_string(&grant.args).unwrap_or_else(|_| "{}".to_string());
        let instruction = format!(
            "Operator approved your `{tool}` call. Re-issue it now with EXACTLY these \
             arguments: {args}. Do not modify them.",
            tool = grant.tool,
        );

        let guard = self.deps.steer.register(
            &self.record.id,
            InflightEntry {
                key: format!("approval:{approval_id}"),
                task_id: None,
                kind: InflightKind::Delegation,
                title: format!("re-issue {}", grant.tool),
                agent_id: grant.agent.clone(),
                started_at_millis: now_millis(),
                pending_action: None,
            },
        );
        let control = guard.control().clone();

        let run_turn = HarnessRunTurn::new(&self.pool, &self.deps);
        // Un-streamed, like a dispatched card: this turn is answered by the
        // bubble returned below, and its transient frames would otherwise
        // misattribute onto whichever chat thread the console is watching.
        let outcome = run_turn
            .run_steered_background(&self.record.id, &grant.agent, &instruction, &control, None)
            .await;
        drop(guard);

        let text = match outcome {
            Ok(outcome) => outcome.reply,
            Err(err) => {
                // The grant stays live: the call did not go through, so the
                // operator's approval has not been spent and the TTL sweep will
                // tell them if it never does. Reporting the failure is what stops
                // this looking like a silent success.
                tracing::warn!(
                    approval_id = %approval_id,
                    tool = %grant.tool,
                    error = %err,
                    "[approval] re-issuing an approved tool call failed"
                );
                format!(
                    "re-issuing the approved `{}` call failed: {err}",
                    grant.tool
                )
            }
        };

        // Journal the reply so the transcript echo survives a console refresh.
        // The bubble below is live-only — a reader that reloads the page rebuilds
        // the thread from the event log, and without this the operator would
        // approve, watch the agent answer, refresh, and find the answer gone.
        //
        // `chat_id` is the AGENT id (not the approval id) so `chat_history::owns`
        // routes it into that desk's thread — the same thread the original
        // request came from, which is where the operator is looking.
        if let Some(events) = self.deps.events.as_ref()
            && let Err(err) = events
                .append(
                    &self.record.id,
                    CompanyEvent::AgentReply {
                        chat_id: grant.agent.clone(),
                        agent_id: grant.agent.clone(),
                        text: text.clone(),
                        steps: Vec::new(),
                        task_id: None,
                    },
                )
                .await
        {
            tracing::warn!(
                approval_id = %approval_id,
                error = %err,
                "[approval] failed to journal the re-issued call's reply; continuing"
            );
        }

        Ok(Some(OutboundMessage {
            task_id: None,
            channel: grant.agent.clone(),
            text,
            steps: Vec::new(),
            reply_to: None,
        }))
    }

    async fn run_task(
        &self,
        task_id: &str,
        run_id: Option<&str>,
    ) -> Result<Option<OutboundMessage>> {
        // Issue #242: the attempt this dispatch is recorded under. `None`
        // whenever the run store is unwired or the choke point could not mint a
        // row — the card runs either way, untracked.
        let sink = self.open_trace(run_id);

        let Some(tasks) = self.deps.tasks.as_ref() else {
            self.settle_run(sink.as_deref(), RunStatus::Failed, Some(NO_TASK_STORE))
                .await;
            return Ok(None);
        };
        let Some(mut card) = tasks
            .list(&self.record.id)
            .await?
            .into_iter()
            .find(|t| t.id == task_id)
        else {
            self.settle_run(sink.as_deref(), RunStatus::Failed, Some(CARD_VANISHED))
                .await;
            return Ok(None);
        };

        // Issue #205: who works this card, resolved against the FULL roster —
        // teammates, operator-overlay teammates and desks alike.
        let resolution = assignee::resolve(&self.record, &card.assignee);
        if let Some(reason) = resolution.rejection() {
            // The card names somebody this company does not have. Before this
            // it dispatched to the orchestrator anyway and the board kept the
            // invalid name, so the only trace was a timeline that read "reply
            // from ceo" on a card assigned to somebody else. Refuse instead: no
            // turn is run (nothing was asked of the orchestrator), the card
            // returns to `todo` carrying the reason, and the operator is
            // told — on the board, on the timeline, and in the thread the card
            // came from.
            tracing::warn!(
                task_id = %card.id,
                assignee = %card.assignee,
                "[task] refusing dispatch: {reason}"
            );
            return self
                .refuse_dispatch(tasks, card, &reason, sink.as_deref())
                .await;
        }
        // A blank assignee is the one legitimate miss: nobody was named, so the
        // orchestrator picks it up.
        //
        // Not final: a turn that hands the work off (issue #204) reassigns the
        // card to the delegate, and from that point the delegate is the
        // responder every downstream write credits — the note, the artifact,
        // the journal, and the relay.
        let mut responder = resolution
            .working_agent()
            .unwrap_or(&self.responder)
            .to_string();

        // Link the working agent to the card, and persist it BEFORE the turn
        // runs (#205). A card the CEO picked up used to keep `assignee = ""`
        // for the whole run and forever after, so the board never named who was
        // doing the work. Writing it up front is what makes the board show the
        // card "working" under a real agent while the turn is in flight — the
        // store `upsert` here is the plain persistence path, not
        // `CompanyRuntime::upsert_task`, so it cannot re-fire the dispatch edge.
        //
        // Only for an assignee that names a teammate or nobody, though. A desk
        // assignment is ownership and stays one — `AssigneeResolution::canonical`
        // deliberately stores the **desk** id and the REST boundary honours it;
        // dispatch only picks which member runs *this* turn. Writing the lead
        // back would silently turn a card assigned to `eng` into one assigned to
        // `engineer` the first time it ran, erasing the desk from the board and
        // breaking the very invariant `canonical()` documents (#214 review).
        if resolution.links_working_agent() && card.assignee != responder {
            card.assignee = responder.clone();
            card.updated_at_millis = now_millis();
            tasks.upsert(&self.record.id, &card).await?;
        }

        // Register the run so an operator can steer it mid-flight. The guard's
        // RAII `Drop` deregisters on every exit path (success, error, redirect
        // exhaustion), so a crashed turn never leaves a ghost row in the strip.
        let guard = self.deps.steer.register(
            &self.record.id,
            InflightEntry {
                key: card.id.clone(),
                task_id: Some(card.id.clone()),
                kind: InflightKind::Task,
                title: card.title.clone(),
                agent_id: responder.clone(),
                started_at_millis: now_millis(),
                pending_action: None,
            },
        );
        let control = guard.control().clone();

        // The base turn instruction is frozen at dispatch (the card's note keeps
        // accumulating operator/agent blocks, but a redirect always re-runs from
        // the original brief plus the fresh instruction — last redirect wins).
        let base_instruction = task_instruction(&card);
        let mut instruction = base_instruction.clone();
        let mut redirects: u32 = 0;
        // Route the background turn through the brain-agnostic `RunTurn` seam
        // (issue #176), re-attaching `HarnessDeps` behind `HarnessRunTurn`.
        let run_turn = HarnessRunTurn::new(&self.pool, &self.deps);
        // Issue #242: where this attempt's own approval requests begin. The
        // queue is shared with any chat turn earlier in the same cycle and is
        // append-only until the cycle-end drain, so a position taken here stays
        // the boundary between "somebody else parked that" and "this run did".
        let approvals_before = self.deps.approval_requests.queued();

        // The loop yields how the run ended plus its operator-facing result on
        // whichever path ends it, so the artifact (#187), the completion event
        // (#185) and the attempt row (#242) all record exactly what the note
        // does rather than three divergent renderings of one run. The ending
        // rides along because the run's status cannot be re-derived from the
        // card's landing column — `Failed` and `Cancelled` share one column and
        // are not the same outcome (see [`lifecycle::run_status_for`]).
        let (run_end, result_text) = loop {
            // Start each turn from an empty queue so nothing a prior turn (this
            // cycle's operator message, or an earlier redirect rerun) left
            // behind can hijack this card — the same guard
            // `handle_operator_message` opens with.
            self.deps.delegations.clear();
            let outcome = run_turn
                // A dispatched task card carries no chat bubble (its steps are
                // discarded into the note), so its live turn frames must not leak
                // onto the console timeline — run it un-streamed (#125 review).
                .run_steered_background(
                    &self.record.id,
                    &responder,
                    &instruction,
                    &control,
                    // Issue #242: un-streamed does not mean unrecorded. The
                    // trace this turn produces is written to the attempt row as
                    // it happens, which is what a redirect re-run appends to
                    // rather than restarting.
                    sink.clone(),
                )
                .await;
            // One-shot read of what (if anything) the operator asked for. `None`
            // is the ordinary, unsteered path.
            match control.take() {
                None => {
                    // A dispatched task discards its steps — the note is text-only.
                    match outcome {
                        Ok(outcome) => {
                            // Issue #204: the turn may have DELEGATED rather
                            // than done the work. The dispatched responder is
                            // the orchestrator, which carries `delegate_to_desk`
                            // / `spawn_task`, and nothing here used to drain
                            // what those queued — so the hand-off was dropped,
                            // the turn still read as a clean completion, and the
                            // card landed in `in_review` under the delegator
                            // with the delegate never having run. Draining here
                            // runs the delegate, reassigns the card to them, and
                            // settles it from THEIR output.
                            //
                            // An errored hand-off lands exactly like an errored
                            // turn (the `Err` arm below), and must NOT propagate
                            // with `?`. By the time `run_delegation` can fail,
                            // `hand_card_over` has already persisted the card as
                            // `in_progress` reassigned to the delegate — so
                            // unwinding here would skip both the settle and the
                            // final `upsert` and leave the card sitting in
                            // `in_progress` under a delegate that produced
                            // nothing, with no result and nothing to re-dispatch
                            // it: `task_enters_in_progress` only edge-fires on
                            // the *transition* into that column, which already
                            // happened. That is precisely the stranded state
                            // this fix exists to eliminate.
                            //
                            // The card keeps the delegate as its assignee on the
                            // way to `todo` — the hand-off did happen, and a
                            // re-dispatch should start from who it was given to.
                            let handoff = match self
                                .delegation_runner(&run_turn)
                                .for_task(&card.id)
                                // The delegate's turn is part of THIS attempt —
                                // its steps and its spend belong to the card's
                                // run, not to nothing (#242).
                                .for_run(sink.clone())
                                .handle_task_delegations(&mut card, &responder)
                                .await
                            {
                                Ok(handoff) => handoff,
                                Err(err) => {
                                    let result = format!("hand-off failed: {err}");
                                    settle(&mut card, TaskRunEnd::Failed, &responder, &result);
                                    break (TaskRunEnd::Failed, result);
                                }
                            };
                            // `settle` writes the note (attributed to whoever
                            // actually produced the text) and the landing column
                            // via the #186 lifecycle seam; the loop still yields
                            // the reply so the #185/#190 completion events
                            // report the same text that landed in the note.
                            let (end, result) = match handoff {
                                // The delegate answered: they own the card, and
                                // every downstream write credits them.
                                Some(handoff) => {
                                    responder = handoff.delegate;
                                    match handoff.reply {
                                        Some(reply) => {
                                            settle(
                                                &mut card,
                                                TaskRunEnd::Completed,
                                                &responder,
                                                &reply,
                                            );
                                            (TaskRunEnd::Completed, reply)
                                        }
                                        // The hand-off ran and an operator
                                        // CANCELLED it in flight, so it produced
                                        // nothing. Naming the cancellation here
                                        // is safe because `TaskHandoff` only
                                        // carries `reply: None` for a run
                                        // `run_delegation` reported as cancelled
                                        // — a hand-off that ends empty for any
                                        // other reason reports no hand-off at
                                        // all and never reaches this arm (issue
                                        // #213 review).
                                        //
                                        // Partial work is discarded and the card
                                        // returns to To-do, exactly as a
                                        // cancelled dispatch does — it must not
                                        // read as finished, and it must not
                                        // strand in `in_progress` either.
                                        None => {
                                            let reply =
                                                "the delegated run was cancelled before it \
                                                 produced anything"
                                                    .to_string();
                                            settle(
                                                &mut card,
                                                TaskRunEnd::Cancelled,
                                                &responder,
                                                &reply,
                                            );
                                            (TaskRunEnd::Cancelled, reply)
                                        }
                                    }
                                }
                                // Nothing was handed off — the responder did the
                                // work itself, as before.
                                None => {
                                    let result = outcome.reply;
                                    settle(&mut card, TaskRunEnd::Completed, &responder, &result);
                                    (TaskRunEnd::Completed, result)
                                }
                            };
                            break (end, result);
                        }
                        Err(err) => {
                            let result = format!("dispatch failed: {err}");
                            settle(&mut card, TaskRunEnd::Failed, &responder, &result);
                            break (TaskRunEnd::Failed, result);
                        }
                    }
                }
                Some(SteerAction::Cancel) => {
                    // Partial work is DISCARDED — only a cancellation note lands,
                    // and the card returns to `todo`. The note is attributed to
                    // the operator, not the assignee (the lifecycle seam decides
                    // that). The loop still yields the text for #185/#190.
                    let result = "cancelled while in flight".to_string();
                    settle(&mut card, TaskRunEnd::Cancelled, &responder, &result);
                    break (TaskRunEnd::Cancelled, result);
                }
                Some(SteerAction::Pause) => {
                    // Partial work is PRESERVED in the note; the card parks in the
                    // `paused` column. The cycle ends normally, so the per-tenant
                    // serial lock releases while parked — resume is a plain
                    // `column → in_progress` PATCH that re-triggers dispatch.
                    let partial = match &outcome {
                        Ok(outcome) => format!("[paused] {}", outcome.reply),
                        Err(err) => format!("[paused] dispatch failed: {err}"),
                    };
                    settle(&mut card, TaskRunEnd::Paused, &responder, &partial);
                    break (TaskRunEnd::Paused, partial);
                }
                Some(SteerAction::Redirect { instruction: fresh }) => {
                    redirects += 1;
                    card.note = Some(append_result(
                        card.note.as_deref(),
                        "operator redirect",
                        &fresh,
                    ));
                    if redirects > MAX_REDIRECTS_PER_DISPATCH {
                        // Exhausted the redirect budget — finalize the last run's
                        // reply to the card's terminal column rather than looping
                        // forever.
                        let last = match &outcome {
                            Ok(outcome) => outcome.reply.clone(),
                            Err(err) => format!("dispatch failed: {err}"),
                        };
                        settle(&mut card, TaskRunEnd::RedirectsExhausted, &responder, &last);
                        break (TaskRunEnd::RedirectsExhausted, last);
                    }
                    // Re-run from the original brief plus the (codepoint-capped)
                    // operator instruction.
                    instruction = format!(
                        "{base_instruction}\n\nOperator redirect: {}",
                        cap_redirect(&fresh)
                    );
                    continue;
                }
            }
        };

        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record.id, &card).await?;
        // `guard` drops here → the run leaves the in-flight strip.
        drop(guard);

        // Issue #242: settle the attempt row from how the run actually ended.
        //
        // Here, right after the card is persisted and before any of the
        // best-effort journal writes below, so the run record and the board
        // agree the moment either is readable. It also means the cycle's
        // terminality backstop finds this row already settled and no-ops — the
        // rich settle always wins the race, because `run_task` returns before
        // `run_locked` reaches the backstop.
        //
        // First (#333's unblocking move): tag every approval this attempt's own
        // turns parked with the run that produced it, and count them. A run that
        // finished its work but left a person something to act on has not
        // succeeded — it is in review.
        let parked = match sink.as_ref() {
            Some(sink) => self
                .deps
                .approval_requests
                .stamp_run(approvals_before, sink.run_id()),
            None => 0,
        };
        self.settle_run_end(sink.as_deref(), run_end, &result_text, parked)
            .await;

        // Issue #187: record the run's output as a versioned artifact so the
        // Task Detail Artifacts tab has something behind it, and so a later
        // operator edit can be diffed against what the agent actually wrote.
        //
        // Only a card that landed in its **success terminal** produces one:
        // that is the state meaning "the agent produced something reviewable".
        // A failure, a cancellation, or a pause writes its line to the note as
        // before but is NOT an artifact — versioning `dispatch failed: …`
        // strings would bury the real drafts and make the churn metric
        // meaningless.
        //
        // The success terminal is two columns, not one (#179): a board-created
        // card parks in `in_review` for its operator reviewer, while a card
        // carrying an `origin_chat_id` — a delegated handoff nobody is watching
        // the board for — completes straight to `done`. Both ran a turn and
        // both produced a deliverable, so testing the column against
        // `success_terminal_column` keeps the artifact tied to "the run
        // succeeded" rather than to one particular landing column.
        //
        // Recorded before the #185 journal writes below because those move
        // `result_text` into the completion event; the artifact only borrows it.
        if card.column == lifecycle::success_terminal_column(&card) {
            self.record_task_artifact(
                &card,
                &responder,
                &result_text,
                sink.as_ref().map(|s| s.run_id()),
            )
            .await?;
        }

        // Issue #185: correlate this dispatch's journal trail to its card.
        //
        // Ordering matters. Any MCP failures the turn queued are drained FIRST,
        // tagged with this task, so they land on the task's own timeline. Before
        // this they were left in the queue for whichever operator turn drained
        // next — which both mis-attributed them to an unrelated chat bubble and
        // left the dispatch's timeline silent about the very calls that broke.
        //
        // The steps the drain produces are discarded, matching the rest of
        // `run_task`: a dispatched card has no chat bubble to render them on
        // (they are journaled as `McpCallFailed` events instead).
        //
        // Every write below is **best-effort**: the card was already persisted
        // above, so propagating a journal failure with `?` would abandon the
        // terminal anchor *and* the #151 post-back for a dispatch that has in
        // fact landed — leaving a timeline stuck "still running" for a card the
        // board already shows in its terminal column, and failing the whole cycle over
        // a bookkeeping write. Matches the existing journal-after-persist sites
        // (`chat_and_emit`, `WorkflowCreated`, `TaskSteered`).
        let mut discarded_steps = Vec::new();
        if let Err(err) = self
            .surface_mcp_failures(&mut discarded_steps, Some(&card.id))
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal dispatch MCP failures; continuing"
            );
        }

        self.journal_task_outcome(&card, &responder, result_text)
            .await;

        // Issue #151 §3.2: answer in the conversation the card was spawned
        // from. Only a card that remembers an origin posts back — one created
        // straight on the board, or written before `origin_chat_id` existed,
        // has no thread to answer in and behaves exactly as before.
        //
        // Issue #186: the **orchestrator** relays the result, not the assignee.
        //
        // The bubble used to be attributed to the responder, so a desk member
        // spoke straight to the operator — which bypasses the orchestrator's
        // role as the single point of contact that `run_delegation` already
        // honours. It is now the orchestrator's bubble, and the assignee is
        // credited inside the text, so the operator still knows who did the
        // work without a second voice in the thread.
        //
        // It still carries the card's landing column, so the operator reads one
        // line and knows both what came back and where the card went. Steps are
        // deliberately empty: a dispatched card discards them into the note.
        let Some(origin) = card.origin_chat_id.clone() else {
            return Ok(None);
        };
        Ok(Some(lifecycle::relay_reply(
            &card,
            &responder,
            &self.orchestrator(),
            origin,
        )))
    }

    /// Ends a dispatch that never ran because its `assignee` names nobody this
    /// company has (issue #205).
    ///
    /// Takes the same three exits a finished run does — the card is settled and
    /// persisted, the outcome is journaled onto the task's timeline, and a card
    /// that remembers an origin thread is answered there — so the refusal is
    /// visible everywhere a result would have been, instead of being the silence
    /// this issue is about. It deliberately skips the three things that belong
    /// to a run that happened: no in-flight registration (nothing is in flight),
    /// no MCP drain (no turn queued anything), and no artifact (`Failed` lands
    /// in `todo`, never a success terminal).
    ///
    /// Attributed to the **orchestrator**: it is the company answering for its
    /// own roster, and the named assignee does not exist to speak.
    async fn refuse_dispatch(
        &self,
        tasks: &Arc<dyn crate::ports::TaskStore>,
        mut card: TaskRecord,
        reason: &str,
        sink: Option<&RunTraceSink>,
    ) -> Result<Option<OutboundMessage>> {
        let orchestrator = self.orchestrator();
        let text = format!("dispatch refused: {reason}");
        settle(&mut card, TaskRunEnd::Failed, &orchestrator, &text);
        card.updated_at_millis = now_millis();
        tasks.upsert(&self.record.id, &card).await?;
        // A refusal is a real, terminal attempt — one that spent nothing. It
        // settles like any other ending (#242), so the card's run history shows
        // "this was tried and refused, and why" rather than a gap.
        self.settle_run_end(sink, TaskRunEnd::Failed, &text, 0)
            .await;

        self.journal_task_outcome(&card, &orchestrator, text).await;

        let Some(origin) = card.origin_chat_id.clone() else {
            return Ok(None);
        };
        Ok(Some(lifecycle::relay_reply(
            &card,
            &orchestrator,
            &orchestrator,
            origin,
        )))
    }

    /// Opens the trace sink for this dispatch's attempt row (issue #242), or
    /// `None` when there is nothing to record into.
    ///
    /// Two independent reasons for `None`, and both are ordinary: the run store
    /// is unwired (every test construction, and any embedder that never called
    /// [`with_runs`](Self::with_runs)), or the dispatch choke point could not
    /// mint a row and sent `run_id: None`. In both cases the card runs exactly
    /// as it did before this issue.
    fn open_trace(&self, run_id: Option<&str>) -> Option<Arc<RunTraceSink>> {
        let run_id = run_id?;
        let runs = self.runs.as_ref()?;
        Some(Arc::new(RunTraceSink::new(
            self.record.id.clone(),
            run_id,
            Arc::clone(runs),
        )))
    }

    /// Settles the attempt row from how its run ended, folding in the trace's
    /// step count and cost (issue #242).
    ///
    /// `parked_approvals` is how many approval requests **this attempt's own
    /// turns** left for a person to act on. A run that otherwise succeeded while
    /// parking at least one finishes [`RunStatus::WaitingApproval`] rather than
    /// [`RunStatus::Succeeded`] — epic #183 decision 2: a person must act, so
    /// the attempt is in review, not done. A run that failed, was cancelled or
    /// was paused keeps its own status; the operator has a bigger problem than a
    /// pending approval, and overwriting the reason it stopped would hide it.
    ///
    /// `WaitingApproval` is terminal-in-v1 (resuming an approved attempt is its
    /// own issue) and deliberately **re-enterable across attempts**: the
    /// re-dispatch after an approval is a new run that can wait again, which is
    /// what keeps #243's single-use, argument-exact grants coherent instead of
    /// forcing an operator to batch several approvals into one.
    async fn settle_run_end(
        &self,
        sink: Option<&RunTraceSink>,
        end: TaskRunEnd,
        result: &str,
        parked_approvals: usize,
    ) {
        let status = lifecycle::settled_run_status(end, parked_approvals);
        // Only a failure carries a reason: `error` is "why this went wrong", not
        // "what the agent said". Stamping a success's reply here would put the
        // deliverable in a field every reader renders as a fault.
        let error = matches!(status, RunStatus::Failed).then_some(result);
        self.settle_run(sink, status, error).await;
    }

    /// Writes one settle through to the run store, best-effort.
    ///
    /// Never propagates: the card is already persisted and the operator can
    /// already see the outcome by the time this runs, so a store fault must not
    /// fail the cycle — the same journal-after-persist rule
    /// [`journal_task_outcome`](Self::journal_task_outcome) follows. A run left
    /// unsettled by a failure here is still caught by the cycle's terminality
    /// backstop, and failing that by the boot reaper.
    async fn settle_run(
        &self,
        sink: Option<&RunTraceSink>,
        status: RunStatus,
        error: Option<&str>,
    ) {
        let (Some(sink), Some(runs)) = (sink, self.runs.as_ref()) else {
            return;
        };
        let outcome = RunOutcome {
            status,
            error: error.map(str::to_string),
            usage: sink.usage(),
            step_count: sink.step_count(),
        };
        if let Err(err) = runs
            .finish_run(&self.record.id, sink.run_id(), outcome)
            .await
        {
            tracing::warn!(
                company = %self.record.id,
                run = %sink.run_id(),
                error = %err,
                "[runs] could not settle an attempt row; the dispatch itself landed"
            );
        }
    }

    /// Journals a finished dispatch onto its card's timeline (issue #185): the
    /// run's reply, then the terminal anchor that closes the timeline.
    ///
    /// Every write is **best-effort** and the errors are logged, never
    /// propagated. The card is already persisted by the time this runs, so
    /// failing the cycle over a bookkeeping write would abandon the terminal
    /// anchor *and* the #151 post-back for a dispatch that has in fact landed —
    /// leaving a timeline stuck "still running" for a card the board already
    /// shows in its terminal column. Matches the existing journal-after-persist
    /// sites (`chat_and_emit`, `WorkflowCreated`, `TaskSteered`).
    async fn journal_task_outcome(&self, card: &TaskRecord, responder: &str, result_text: String) {
        let Some(events) = self.deps.events.as_ref() else {
            return;
        };
        // The run's reply, tagged so the per-task timeline can filter it out of
        // the company-scoped journal.
        //
        // `chat_id` is the **card id**, deliberately, not the card's origin
        // thread. `chat_history::owns` routes a reply into a desk's history by
        // matching `chat_id` against the desk id/name, so using the origin here
        // would inject this record into that desk's chat — a behaviour change
        // well outside a read foundation, and a duplicate of the live post-back
        // bubble the caller returns. A card id matches no desk, so the record
        // stays exactly what it is: timeline material, reachable only through
        // `task_id`. An empty string would be worse still — it folds into the
        // General desk.
        if let Err(err) = events
            .append(
                &self.record.id,
                CompanyEvent::AgentReply {
                    chat_id: card.id.clone(),
                    agent_id: responder.to_string(),
                    text: result_text.clone(),
                    steps: Vec::new(),
                    task_id: Some(card.id.clone()),
                },
            )
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal dispatch reply; continuing"
            );
        }
        // The terminal anchor, journaled after the card's landing column is
        // persisted so it always records a completed run. Attempted even if the
        // reply above failed — the anchor is what closes a timeline, so dropping
        // it is strictly worse than dropping the reply.
        if let Err(err) = events
            .append(
                &self.record.id,
                CompanyEvent::DeskTaskCompleted {
                    task_id: card.id.clone(),
                    desk: responder.to_string(),
                    output: result_text,
                    column: card.column.clone(),
                },
            )
            .await
        {
            tracing::warn!(
                task_id = %card.id,
                error = %err,
                "[task] failed to journal task completion; continuing"
            );
        }
    }

    /// The company orchestrator's agent id — the single voice that answers the
    /// operator (issue #186).
    ///
    /// Resolved from the roster rather than read off [`Self::responder`],
    /// because `with_responder` can point that at any agent for a test or a
    /// single-desk company; the relay must still be attributed to the real
    /// orchestrator. Falls back to `responder` only when the roster has no
    /// orchestrator to name at all, which is the same empty-roster case
    /// [`orchestrator::orchestrator_id`] already tolerates.
    fn orchestrator(&self) -> String {
        orchestrator::orchestrator_id(&self.record.manifest.agents)
            .unwrap_or_else(|| self.responder.clone())
    }

    /// Records a completed dispatch's output as a versioned artifact (#187).
    ///
    /// A task that is dispatched, reviewed, and dispatched again is the same
    /// deliverable evolving — so the second run appends a **version** to the
    /// existing artifact rather than opening a second one. The artifact to
    /// extend is the most recently updated one already attached to this card;
    /// only the first run creates.
    ///
    /// A missing artifact store is a silent no-op, exactly like a missing task
    /// store: the note is still written, so the board behaves as it did before
    /// this issue.
    /// `run_id` is the attempt that produced `body`, stamped onto the revision
    /// this call writes (issue #242) so a run row can point at what it actually
    /// wrote. `None` for an untracked dispatch, which behaves exactly as before.
    async fn record_task_artifact(
        &self,
        card: &TaskRecord,
        responder: &str,
        body: &str,
        run_id: Option<&str>,
    ) -> Result<()> {
        let Some(artifacts) = self.deps.artifacts.as_ref() else {
            return Ok(());
        };
        let existing = artifacts
            .list(&self.record.id, Some(&card.id))
            .await?
            .into_iter()
            .max_by_key(|a| a.updated_at_millis);
        let at = now_millis();
        let mut record = match existing {
            Some(mut found) => {
                found.push_version(body, ArtifactAuthor::Agent, responder, at, None);
                found
            }
            None => ArtifactRecord::new(
                generate_id(),
                &card.id,
                &card.title,
                ArtifactKind::Text,
                body,
                responder,
                at,
            ),
        };
        // Only the revision this run wrote. An earlier attempt's version keeps
        // the attempt that wrote *it*, which is the point of stamping per
        // version instead of per record.
        if let Some(run_id) = run_id {
            record.stamp_run(run_id);
        }
        artifacts.upsert(&self.record.id, &record).await?;
        Ok(())
    }

    /// Resolves which agent answers an operator message.
    ///
    /// Resolution order, and the order matters:
    ///
    /// 1. a **desk** (the `chat` field naming a group chat with a lead member) is
    ///    answered by that desk's lead — unchanged;
    /// 2. a **roster teammate id** is answered by that teammate directly, which
    ///    is what makes a per-agent DM thread possible (issue #151 §3.3);
    /// 3. everything else — the "General" desk, an unknown id, an unaddressed
    ///    message — goes to the orchestrator, as before.
    ///
    /// Desks are tried first so a desk whose id happens to match an agent id
    /// keeps routing as a desk; the DM case only ever claims ids that resolve to
    /// no desk at all. Without step 2 a DM thread would silently reach the
    /// orchestrator instead of the teammate the operator opened — the console
    /// would look like it were addressing an agent while talking to someone
    /// else.
    fn responder_for(&self, chat: Option<&str>) -> String {
        let Some(chat) = chat else {
            return self.responder.clone();
        };
        if let Some(lead) = self.desk_lead(chat) {
            return lead;
        }
        if self.record.is_roster_agent(chat) {
            return chat.to_string();
        }
        self.responder.clone()
    }

    /// The lead member of a desk: the first member of the matching group chat
    /// (by id, or by case-insensitive name) that is a real roster teammate.
    /// `None` when no desk matches or none of its members are on the roster.
    ///
    /// Membership is the desk's **effective** roster — the manifest members
    /// unioned with operator-added overlay members (issue #72) — resolved through
    /// the same [`CompanyRecord::effective_desk_members`] the REST `list_desks`
    /// handler uses, so the two cannot drift. A roster teammate is a manifest
    /// agent or a team-overlay teammate, so an overlay-added lead is reachable on
    /// a desk the manifest left empty.
    fn desk_lead(&self, desk: &str) -> Option<String> {
        // Desk-lead resolution is brain-agnostic — it reads only `CompanyRecord`
        // — so it lives on the delegation seam (issue #176); this stays a thin
        // wrapper for the routing callers on the brain.
        delegation::desk_lead(&self.record, desk)
    }

    /// Drains the MCP failure queue **onto the operator bubble's step timeline**
    /// as error steps (the Activity-trace re-skin of the error-hardening cell's
    /// original fallback bubble), and journals a scrubbed
    /// [`CompanyEvent::McpCallFailed`] audit event per failure when the event log
    /// is wired.
    ///
    /// One surface, one renderer, one scrub discipline: a silently-failed MCP
    /// call shows up as a red step in the same timeline as every other tool call
    /// instead of a separate warning bubble. Every string was already scrubbed at
    /// the source (`OcMcpCallTool`), so `scrubbed_message` is safe to show and to
    /// persist.
    ///
    /// `task_id` is the dispatched card the failing turn belonged to, when the
    /// drain runs inside a [`CompanyEvent::TaskDispatched`] cycle (issue #185).
    /// It is stamped onto each journaled failure so a task's broken tool calls
    /// can be filtered out of the company-scoped journal onto its own timeline;
    /// a chat turn passes `None` and journals exactly as before.
    async fn surface_mcp_failures(
        &self,
        steps: &mut Vec<TurnStep>,
        task_id: Option<&str>,
    ) -> Result<()> {
        for failure in self.deps.mcp_failures.drain() {
            steps.push(TurnStep {
                kind: TurnStepKind::Note,
                status: TurnStepStatus::Error,
                label: format!("MCP: {} unavailable", failure.server),
                detail: Some(failure.scrubbed_message.clone()),
                elapsed_ms: None,
            });
            if let Some(events) = self.deps.events.as_ref() {
                // Best-effort **per failure**. `drain` is a `mem::take`, so the
                // queue is already empty by the time this loop runs and the
                // batch exists only in this iterator. Propagating with `?` here
                // would discard every failure after the first journal error —
                // permanently, since nothing remains to retry from. A failed
                // audit write must not cost us the rest of the audit.
                let server = failure.server.clone();
                if let Err(err) = events
                    .append(
                        &self.record.id,
                        CompanyEvent::McpCallFailed {
                            task_id: task_id.map(str::to_string),
                            server: failure.server,
                            tool: failure.tool,
                            status: failure.status,
                            message: failure.scrubbed_message,
                        },
                    )
                    .await
                {
                    tracing::warn!(
                        server = %server,
                        task_id = task_id.unwrap_or("-"),
                        error = %err,
                        "[task] failed to journal an MCP failure; draining the rest"
                    );
                }
            }
        }
        Ok(())
    }

    /// Drains the approval-request queue and parks each request on the host's
    /// approval gate, so an approval-gated tool call the agent hit during this
    /// cycle reaches the operator's Approvals page (issue #172).
    ///
    /// The missing half of the approval path. openhuman resolves a
    /// `RequireApproval` **inline** — it blocks the tool and narrates the
    /// refusal to the model — so nothing downstream of the turn ever learned a
    /// request existed and `journal.pending()` stayed empty. The
    /// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) now records
    /// each blocked call on the shared queue; this drains it once per cycle and
    /// parks it through
    /// [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect).
    ///
    /// Parked, not re-evaluated:
    /// [`emit_effect`](crate::ports::brain::CycleHost::emit_effect) would
    /// re-decide the request against the runtime
    /// [`ApprovalGate`](crate::ports::ApprovalGate), which allows (and therefore
    /// "executes") anything it classifies as
    /// [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other) — most
    /// gated tool calls — and the request would disappear again. The verdict was
    /// already reached inside the turn; the runtime's job here is only to hold
    /// it for the operator.
    ///
    /// Bounded by
    /// [`MAX_APPROVAL_REQUESTS_PER_TURN`](crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN);
    /// anything past the cap is discarded rather than flooding the queue.
    ///
    /// **A failed park never takes the batch or the turn down with it.**
    /// [`ApprovalRequestQueue::drain`](crate::harness::policy::ApprovalRequestQueue::drain)
    /// empties the shared queue up front, so propagating the first
    /// [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect)
    /// error with `?` would lose every *later* request in the batch — already out
    /// of the queue and never retried — and would discard the turn's
    /// already-computed operator reply along with it. That is precisely the
    /// silent-disappearance failure this issue exists to fix, so each failure is
    /// logged at `error` and the drain continues.
    async fn park_approval_requests(&self, host: &dyn CycleHost) -> Result<()> {
        for request in self
            .deps
            .approval_requests
            .drain(crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN)
        {
            match host.park_effect(request.effect).await {
                Ok(approval_id) => log::info!(
                    "[harness::brain] parked '{}' for operator approval (id={approval_id}): {}",
                    request.tool,
                    request.reason
                ),
                // Loud, and the only trace of a request the operator will never
                // see — the queue entry is already gone.
                Err(err) => log::error!(
                    "[harness::brain] failed to park '{}' for operator approval ({}): {err}",
                    request.tool,
                    request.reason
                ),
            }
        }
        Ok(())
    }

    /// Executes one drained delegation from the orchestrator's turn.
    ///
    /// `spawn_task` opens a To-do card through the same
    /// [`TaskStore::upsert`](crate::ports::TaskStore) path the console uses and
    /// reports the card's id (issue #246); it surfaces no bubble of its own. A
    /// missing task store is a silent no-op.
    /// `delegate_to_desk` runs a single turn on the desk's lead member and
    /// **returns its reply for the orchestrator to relay** (a [`DeskReply`]) —
    /// the CEO-relay hand-back: instead of a disconnected sibling bubble the
    /// teammate's answer feeds a second orchestrator turn so the CEO comes back
    /// with it in one coherent conversation. An unknown desk (no roster-backed
    /// lead) or a cancelled run yields nothing to relay. No sub-agent
    /// re-delegation in v1: desk members carry no delegation tools, so their
    /// turns queue nothing.
    ///
    /// The orchestration lives on the brain-agnostic seam (issue #176); this is
    /// a thin wrapper that re-attaches `HarnessDeps` behind a
    /// [`HarnessRunTurn`] and drives a [`DelegationRunner`]. It exists only to
    /// keep the delegation tests exercising the same code path the cycle drives
    /// through [`DelegationRunner::handle_operator_message`], so it is
    /// test-only — the cycle never calls it directly.
    #[cfg(test)]
    async fn run_delegation(
        &self,
        delegation: Delegation,
        chat_id: Option<&str>,
    ) -> Result<delegation::DelegationOutcome> {
        let run_turn = HarnessRunTurn::new(&self.pool, &self.deps);
        self.delegation_runner(&run_turn)
            .run_delegation(delegation, chat_id)
            .await
    }

    /// Builds a [`DelegationRunner`] over `run_turn`, threading the brain-agnostic
    /// handles it needs — the record (desk-lead resolution), the task store, the
    /// steer registry, the company id, and the shared delegation queue the turn
    /// pushes onto. `HarnessDeps` never crosses the seam; it stays behind
    /// `run_turn`.
    fn delegation_runner<'a>(&'a self, run_turn: &'a HarnessRunTurn<'a>) -> DelegationRunner<'a> {
        DelegationRunner::new(
            run_turn,
            &self.record,
            self.deps.tasks.as_ref(),
            &self.deps.steer,
            &self.record.id,
            &self.deps.delegations,
            orchestrator::MAX_DELEGATIONS_PER_TURN,
        )
    }
}

/// The turn instruction for a dispatched card: its title, plus its note when it
/// carries one, framed as a work item to act on.
fn task_instruction(card: &TaskRecord) -> String {
    match card.note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => format!("Task: {}\n\n{}", card.title, note),
        None => format!("Task: {}", card.title),
    }
}

/// Records one run ending on the card: the result block on its note, and the
/// board column it lands in.
///
/// Both decisions are the orchestrator's (issue #186), so both are read from
/// [`crate::harness::lifecycle`] rather than written as literals here. Every
/// break point in `run_task`'s steer loop goes through this one function, which
/// is what stops a sixth exit inventing a sixth column string — and gives #171
/// (the `in_review → done` write, PR #179, now folded into
/// [`lifecycle::landing_column`]) and #190's `DeskTaskCompleted { column, .. }`
/// a single decision to consume. #187's artifact guard reads the same seam via
/// [`lifecycle::success_terminal_column`], so "the run succeeded" stays one
/// decision rather than a literal column compared in two places.
fn settle(card: &mut TaskRecord, end: TaskRunEnd, responder: &str, body: &str) {
    card.note = Some(append_result(
        card.note.as_deref(),
        &lifecycle::note_attribution(end, responder),
        body,
    ));
    card.column = lifecycle::landing_column(end, card).to_string();
}

/// Appends a responder-attributed result block to a card's note, preserving any
/// prior note above it. Slice 1 has no first-class `TaskRecord.result` field, so
/// the outcome lives in the note.
fn append_result(prev: Option<&str>, responder: &str, body: &str) -> String {
    let block = format!("[{responder}] {body}");
    match prev.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n\n{block}"),
        None => block,
    }
}

#[async_trait]
impl Brain for HarnessBrain {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
        // Idempotent — builds the roster on the first cycle, a no-op after.
        self.pool.ensure(&self.record, &self.deps).await?;

        // Issue #172: start from an empty approval queue so nothing a prior
        // cycle — or a workflow run sharing these deps — left behind is parked
        // under this cycle. Every turn this cycle runs (the operator turn, its
        // delegated desk turns, a dispatched card) pushes onto the same queue and
        // is drained once at the end.
        self.deps.approval_requests.clear();

        let mut channel_responses = Vec::new();
        for event in &req.events {
            match event {
                CompanyEvent::OperatorMessage { text, chat, .. } => {
                    // Route to the addressed desk's lead, else the orchestrator.
                    let responder = self.responder_for(chat.as_deref());
                    // The chat/desk thread this turn answers — the same id the
                    // reply is journaled under (`AgentReply.chat_id`). Passed into
                    // the pool so the live turn-stream frames carry it and the
                    // console routes them to this thread; a delegated desk reply
                    // in this cycle rides the same operator thread, so it gets the
                    // same id.
                    let chat_id = chat.as_deref();
                    // Clear stale MCP failures so nothing leaks from a prior turn
                    // (the delegation queue is cleared inside the runner, right
                    // before the orchestrator turn).
                    self.deps.mcp_failures.clear();
                    // Drive the brain-agnostic delegation seam (issue #176): the
                    // orchestrator turn, its queued delegations, and the CEO-relay
                    // hand-back all run behind the `RunTurn` impl. `HarnessDeps` is
                    // re-attached behind `HarnessRunTurn`.
                    let run_turn = HarnessRunTurn::new(&self.pool, &self.deps);
                    let turn = self
                        .delegation_runner(&run_turn)
                        .handle_operator_message(&responder, text, chat_id)
                        .await?;
                    let mut operator_steps = turn.steps;
                    let operator_reply = turn.reply;
                    // Re-skin any MCP tool-call failures (from the orchestrator
                    // turn, a delegated desk turn, or the relay turn) as error
                    // steps on the operator bubble — one surface, one renderer.
                    self.surface_mcp_failures(&mut operator_steps, None).await?;
                    channel_responses.push(OutboundMessage {
                        // Issue #246: when the turn opened a board card, say so
                        // on the bubble it opened it from. Before this a
                        // `spawn_task` was invisible in chat — the card landed
                        // on the board and the reply carried nothing tying the
                        // two together.
                        task_id: turn.spawned_task,
                        channel: "operator".to_string(),
                        text: operator_reply,
                        reply_to: None,
                        steps: operator_steps,
                    });
                    channel_responses.extend(turn.bubbles);
                }
                CompanyEvent::TaskDispatched { task_id, run_id } => {
                    if let Some(message) = self.run_task(task_id, run_id.as_deref()).await? {
                        channel_responses.push(message);
                    }
                }
                // Issue #243: an approval the operator APPROVED that minted a
                // single-use grant — re-dispatch the granting agent so it
                // actually makes the call.
                //
                // This arm is the reason the feature was invisible before. The
                // match had exactly two arms and everything else fell into
                // `_ => {}`, so an `ApprovalResolved` produced no turn, no
                // response, and the cycle ended on the "Acknowledged." fallback
                // below. The operator approved, saw "Acknowledged.", and nothing
                // happened — indistinguishable from the tool having run.
                CompanyEvent::ApprovalResolved {
                    approval_id,
                    verdict: Verdict::Approve,
                    ..
                } => {
                    if let Some(message) = self.redispatch_granted_call(approval_id).await? {
                        channel_responses.push(message);
                    }
                }
                _ => {}
            }
        }

        // Issue #172: every approval-gated tool call this cycle's turns hit is
        // parked on the host's gate now, so it shows up on the operator's
        // Approvals page instead of only being narrated away in chat.
        self.park_approval_requests(host).await?;

        // The runtime requires at least one channel response per cycle.
        if channel_responses.is_empty() {
            channel_responses.push(OutboundMessage {
                task_id: None,
                channel: "operator".to_string(),
                text: "Acknowledged.".to_string(),
                steps: Vec::new(),
                reply_to: None,
            });
        }

        let trace = CompressedTrace::now(
            req.cycle_id.clone(),
            format!("harness cycle handled {} event(s)", req.events.len()),
        );

        // No `ledger_deltas` / `token_usage` here on purpose: `HarnessPool::run`
        // is the single cost-accounting site (it writes the ledger entry and the
        // usage sample through `deps`), so surfacing the same spend again would
        // double-count it — the runtime meters a non-zero `token_usage` for every
        // brain (issue #174), and `cognition()` below declares that this path has
        // already done it.
        Ok(CycleResult {
            channel_responses,
            new_traces: vec![trace],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }

    /// The harness meters itself per turn in [`HarnessPool::run`], against the
    /// live provider slug the turn resolved to — which is why `run_cycle` reports
    /// zero `token_usage` and the runtime's cycle-level metering is a no-op here.
    fn cognition(&self) -> Cognition {
        Cognition {
            path: crate::ports::brain::HARNESS_PATH,
            provider: "per-turn",
            metering: UsageMetering::PerTurn,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tinyagents::harness::message::Message;
    use tinyagents::harness::model::{ChatModel, ModelRequest, ModelResponse};

    use crate::company::CompanyManifest;
    use crate::harness::provider::{HarnessModel, MockProvider};
    use crate::ports::brain::CycleHost;
    // Issue #301: every lifecycle return now lands in To-do (the `backlog` pool
    // is gone), so these assertions read the const rather than a literal.
    use crate::ports::tasks::COLUMN_TODO;
    use crate::ports::types::{
        ApprovalId, CompanyId, ContextOp, ContextOpResult, Effect, EffectDisposition, OverlayAgent,
        ToolCall, ToolResult,
    };
    use crate::store::{FsCompanyStore, FsContextStore, FsOps};

    /// A `CycleHost` that auto-executes anything the brain asks for and swallows
    /// anything it parks; used by every test that isn't about approvals.
    #[derive(Default)]
    struct NoopHost;

    #[async_trait]
    impl CycleHost for NoopHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            Ok(EffectDisposition::Executed)
        }
        async fn park_effect(&self, _effect: Effect) -> Result<ApprovalId> {
            Ok(ApprovalId::new("appr-parked"))
        }
    }

    /// A `CycleHost` that records every effect parked for approval, so the
    /// approval drain can be asserted on (issue #172). Anything else it does is
    /// inert.
    #[derive(Default)]
    struct ParkingHost {
        parked: std::sync::Mutex<Vec<Effect>>,
    }

    impl ParkingHost {
        /// The effects parked through `park_effect`, in order.
        fn parked(&self) -> Vec<Effect> {
            self.parked.lock().expect("parked").clone()
        }
    }

    #[async_trait]
    impl CycleHost for ParkingHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            panic!("an approval request must be parked, never re-evaluated as an effect");
        }
        async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
            let mut parked = self.parked.lock().expect("parked");
            parked.push(effect);
            Ok(ApprovalId::new(format!("appr-{}", parked.len())))
        }
    }

    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
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
            template_provenance: None,
        }
    }

    fn brain_over_mock(dir: &std::path::Path) -> HarnessBrain {
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    fn request(events: Vec<CompanyEvent>) -> CycleRequest {
        CycleRequest {
            cycle_id: "cycle-1".to_string(),
            company_id: CompanyId::new("acme"),
            events,
            event_seqs: Vec::new(),
            compressed_history: Vec::new(),
            roster: Vec::new(),
            context_index: Vec::new(),
        }
    }

    #[tokio::test]
    async fn operator_message_gets_an_agent_reply() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "status?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        // The mock provider prefixes the routed message, proving the turn ran
        // through the openhuman agent rather than an echo.
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
        // The offline mock runs no tools and emits no progress, so the operator
        // bubble carries zero steps — the tell that distinguishes a tool-less
        // (here, memory/echo-style) answer from a tool-backed one.
        assert!(
            result.channel_responses[0].steps.is_empty(),
            "a tool-less turn carries no steps: {:?}",
            result.channel_responses[0].steps
        );
        assert_eq!(result.new_traces.len(), 1);
        // Single cost-accounting site: the cycle result carries no ledger delta.
        assert!(result.ledger_deltas.is_empty());
    }

    #[tokio::test]
    async fn no_events_still_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(request(Vec::new()), &NoopHost)
            .await
            .expect("cycle runs");
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
    }

    #[test]
    fn responder_defaults_to_first_roster_agent() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        assert_eq!(brain.responder, "ceo");
        let brain = brain.with_responder("cfo");
        assert_eq!(brain.responder, "cfo");
    }

    // --- Task dispatch ------------------------------------------------------

    use crate::ports::TaskStore;

    /// A two-agent record so assignee routing has somewhere to route.
    fn record_two() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds it."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
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
            template_provenance: None,
        }
    }

    /// A brain wired to a real task store (shared handle returned for seeding /
    /// asserting), over the offline mock provider.
    fn brain_with_tasks(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_two()),
            tasks,
        )
    }

    /// As [`brain_with_tasks`], but the roster also carries an `eng` desk led by
    /// the engineer — the shape `delegate_to_desk` writes into a card's
    /// `assignee` (issue #205).
    fn brain_with_desk_tasks(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        let (mut brain, tasks) = brain_with_tasks(dir);
        brain.record.manifest.group_chats = toml::from_str::<CompanyManifest>(
            r#"
[company]
name = "Acme"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "eng"
name = "Engineering desk"
members = ["engineer"]
"#,
        )
        .expect("valid manifest")
        .group_chats;
        (brain, tasks)
    }

    /// As [`brain_with_tasks`], but with the artifact store wired to the same
    /// [`FsOps`] handle (it implements both), so a dispatch's versioned output
    /// is observable.
    fn brain_with_artifacts(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        let ops = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(ops.clone()),
            artifacts: Some(ops.clone()),
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_two()),
            ops,
        )
    }

    fn card(id: &str, assignee: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: "Ship the thing".to_string(),
            note: None,
            column: "in_progress".to_string(),
            priority: "high".to_string(),
            assignee: assignee.to_string(),
            updated_at_millis: 0,
            origin_chat_id: None,
            parent_task_id: None,
        }
    }

    // ── Issue #151 §3.2: a finished card answers where it was asked ──────

    // The post-back's *text* rules — title, landing status, note folding,
    // whitespace-only notes — moved with the renderer to
    // `crate::harness::lifecycle` (issue #186), which owns them now and covers
    // each case plus the new assignee-credit rule. What stays here is the
    // wiring: that `run_task` reaches the relay at all, and attributes it to
    // the orchestrator.

    /// The compatibility guarantee: a card with no remembered origin — one made
    /// straight on the board, or written before `origin_chat_id` existed —
    /// posts back nowhere and behaves exactly as it did before.
    #[tokio::test]
    async fn a_card_with_no_origin_posts_back_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        // A roster assignee: since #205 an off-roster one is refused outright,
        // which would satisfy the no-post-back assertion below without ever
        // running the dispatch this test is about.
        let mut c = card("t-no-origin", "engineer");
        c.origin_chat_id = None;
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain.run_task("t-no-origin", None).await.expect("run");
        assert!(
            posted.is_none(),
            "a card with no originating thread must not post back"
        );
        // The note is still the durable record.
        assert!(only_card(&tasks).await.note.is_some());
    }

    /// …and one that does remember its origin answers there, threaded with
    /// `reply_to` and — since issue #186 — attributed to the **orchestrator**
    /// rather than to the assignee that did the work.
    #[tokio::test]
    async fn a_card_with_an_origin_posts_back_to_that_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        // A roster assignee, deliberately: an off-roster one falls back to the
        // default responder (`task_responder`), which in this fixture *is* the
        // orchestrator — so the credit would be correctly suppressed and this
        // test would prove nothing about the one-voice relay.
        let mut c = card("t-origin", "engineer");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain
            .run_task("t-origin", None)
            .await
            .expect("run")
            .expect("a card with an origin must post back");
        assert_eq!(
            posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        // Issue #186: one voice. The bubble belongs to the orchestrator, and
        // the assignee that ran the card is credited in the text instead of
        // speaking to the operator directly.
        assert_eq!(
            posted.channel,
            brain.orchestrator(),
            "the orchestrator relays a finished card, not the assignee"
        );
        assert_ne!(
            posted.channel, "engineer",
            "the assignee must not address the operator directly"
        );
        assert!(posted.text.contains("Ship the thing"), "{}", posted.text);
        assert!(
            posted.text.contains("engineer"),
            "the relay must still credit who did the work: {}",
            posted.text
        );
        // A dispatched card discards its steps into the note.
        assert!(posted.steps.is_empty());
    }

    async fn only_card(tasks: &Arc<FsOps>) -> TaskRecord {
        tasks
            .list(&CompanyId::new("acme"))
            .await
            .expect("list")
            .into_iter()
            .next()
            .expect("one card")
    }

    /// A dispatched **board-created** card (no `origin_chat_id`) runs a turn and
    /// moves to `in_review` — the operator who made it is the reviewer — with
    /// its result folded into the note under the responder that ran it.
    #[tokio::test]
    async fn task_dispatch_runs_and_moves_to_in_review() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "in_review");
        let note = moved.note.expect("result written to note");
        // Default responder (first roster agent) ran it, and the mock provider
        // echoes the instruction (the card title) back into the reply.
        assert!(note.contains("[ceo]"), "{note:?}");
        assert!(note.contains("Ship the thing"), "{note:?}");
    }

    // ── Issue #171: a delegated handoff reaches `done` on its own ─────────

    /// The regression: a card spawned by a delegating turn (so it carries an
    /// `origin_chat_id`) has no operator watching the board, so leaving it in
    /// `in_review` stranded it forever. It must complete to `done`.
    #[tokio::test]
    async fn dispatched_card_with_an_origin_completes_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        // A roster assignee: since #205 an off-roster one never runs a turn, so
        // it would settle to `todo` and prove nothing about the terminal.
        let mut c = card("t-origin", "engineer");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");
        // `run_task` is driven directly here rather than through `run_cycle`,
        // so the roster the turn runs on has to be built explicitly. Without it
        // every dispatch fails with "company not found" and settles to
        // `todo` — which still satisfies this test's post-back assertions
        // while proving nothing about the terminal column.
        brain
            .pool
            .ensure(&brain.record, &brain.deps)
            .await
            .expect("roster");

        let posted = brain
            .run_task("t-origin", None)
            .await
            .expect("run")
            .expect("a card with an origin posts back");

        let moved = only_card(&tasks).await;
        assert_eq!(
            moved.column, "done",
            "a delegated handoff must reach the terminal column, not park in in_review"
        );
        // The note stays the durable record of what came back.
        assert!(moved.note.expect("note").contains("Ship the thing"));
        // …and the bubble says so rather than asking for a review nobody will do.
        assert!(posted.text.contains("is done"), "{}", posted.text);
        assert!(!posted.text.contains("ready for review"), "{}", posted.text);
    }

    /// Issue #179 split the success terminal in two, and artifact capture (#187)
    /// keys off it: a delegated card completes to `done` rather than parking in
    /// `in_review`, and its deliverable must still be versioned.
    ///
    /// Gating capture on the literal `in_review` would silently stop versioning
    /// exactly the cards nobody is watching the board for — the run succeeded
    /// and produced output either way, so the guard has to track "landed on the
    /// success terminal", not one particular column name.
    #[tokio::test]
    async fn a_delegated_card_completing_to_done_still_records_an_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, ops) = brain_with_artifacts(dir.path());
        // Empty assignee → the default responder, so the turn actually runs.
        let mut c = card("t-origin", "");
        c.origin_chat_id = Some("strategy".to_string());
        ops.upsert(&CompanyId::new("acme"), &c).await.expect("seed");

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t-origin".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&ops).await;
        assert_eq!(
            moved.column, "done",
            "a delegated card lands on the `done` success terminal (#179)"
        );
        let artifacts = crate::ports::artifacts::ArtifactStore::list(
            &*ops,
            &CompanyId::new("acme"),
            Some("t-origin"),
        )
        .await
        .expect("list");
        assert_eq!(
            artifacts.len(),
            1,
            "a delegated card's deliverable must still be versioned"
        );
        assert_eq!(artifacts[0].versions.len(), 1);
    }

    /// The other half of the same guard: a run that did NOT succeed still
    /// writes its note but must not open an artifact, whichever terminal the
    /// card would otherwise have used.
    #[tokio::test]
    async fn a_cancelled_delegated_card_records_no_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, ops, _provider) =
            brain_that_steers_itself(dir.path(), "t-cancel", vec![SteerAction::Cancel]);
        let mut c = card("t-cancel", "");
        c.origin_chat_id = Some("strategy".to_string());
        ops.upsert(&CompanyId::new("acme"), &c).await.expect("seed");

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t-cancel".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(only_card(&ops).await.column, COLUMN_TODO);
        let artifacts = crate::ports::artifacts::ArtifactStore::list(
            &*ops,
            &CompanyId::new("acme"),
            Some("t-cancel"),
        )
        .await
        .expect("list");
        assert!(
            artifacts.is_empty(),
            "a cancelled run has no deliverable to version"
        );
    }

    /// The success terminal is chosen by origin, not by outcome: a board-created
    /// card keeps its `in_review` review gate. The decision itself now lives in
    /// [`lifecycle`] (and is unit-tested there); this pins that `settle` — every
    /// run-ending path in this file — actually consults it.
    #[test]
    fn success_terminal_column_is_done_only_for_a_card_with_an_origin() {
        let mut board_card = card("t1", "maya");
        settle(&mut board_card, TaskRunEnd::Completed, "maya", "shipped");
        assert_eq!(board_card.column, "in_review");

        let mut delegated = card("t2", "maya");
        delegated.origin_chat_id = Some("strategy".to_string());
        settle(&mut delegated, TaskRunEnd::Completed, "maya", "shipped");
        assert_eq!(delegated.column, "done");
    }

    /// The redirect-cap finalize branch is the other success terminal, so it has
    /// to make the same choice — otherwise a steered handoff still strands.
    #[tokio::test]
    async fn redirect_cap_finalizes_a_card_with_an_origin_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = || SteerAction::Redirect {
            instruction: "focus on the API".to_string(),
        };
        let (brain, tasks, _provider) = brain_that_steers_itself(
            dir.path(),
            "t1",
            vec![redirect(), redirect(), redirect(), redirect()],
        );
        let mut c = card("t1", "");
        c.origin_chat_id = Some("strategy".to_string());
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(only_card(&tasks).await.column, "done");
    }

    /// The relay has to have wording for the `done` landing column — without it
    /// the fallback arm renders the raw column id into the sentence.
    #[test]
    fn postback_reads_naturally_for_a_done_card() {
        let mut finished = card("t1", "maya");
        finished.column = "done".to_string();
        finished.note = None;
        assert_eq!(
            lifecycle::relay_text(&finished, "maya", "ceo"),
            "\"Ship the thing\" is done (maya ran it)."
        );
    }

    /// An `assignee` that names a roster member routes the turn to that member.
    #[tokio::test]
    async fn task_dispatch_routes_to_assignee() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "engineer"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let note = only_card(&tasks).await.note.expect("note");
        assert!(note.contains("[engineer]"), "{note:?}");
    }

    // ── Issue #242: the attempt row records what the dispatch actually did ──

    /// Wires a run store onto a task-capable brain, mints the `Pending` row the
    /// dispatch choke point would have minted, and returns both.
    async fn brain_with_a_pending_run(
        dir: &std::path::Path,
        assignee: &str,
    ) -> (HarnessBrain, Arc<FsOps>, Arc<dyn crate::ports::RunStore>) {
        use crate::ports::runs::NewRun;

        let (brain, tasks) = brain_with_tasks(dir);
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir));
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", assignee))
            .await
            .expect("seed");
        runs.create_run(
            &company,
            NewRun {
                id: "run-1".to_string(),
                task_id: "t-1".to_string(),
                agent_id: assignee.to_string(),
            },
        )
        .await
        .expect("mint");
        (brain.with_runs(Arc::clone(&runs)), tasks, runs)
    }

    /// The settle. This fixture's pool holds no roster, so the turn errors —
    /// which is exactly the `TaskRunEnd::Failed` path — and the row must end
    /// **terminal**, carrying the reason the card's note carries, rather than
    /// sitting `Pending` for the boot reaper to find.
    #[tokio::test]
    async fn a_dispatch_settles_its_attempt_row_from_how_the_run_ended() {
        use crate::ports::runs::RunStatus;

        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, runs) = brain_with_a_pending_run(dir.path(), "engineer").await;
        let company = CompanyId::new("acme");

        brain.run_task("t-1", Some("run-1")).await.expect("run");

        let settled = runs
            .get_run(&company, "run-1")
            .await
            .expect("read")
            .expect("the row survives");
        assert_eq!(settled.status, RunStatus::Failed);
        assert!(
            settled.finished_at_millis.is_some(),
            "a terminal settle stamps when the attempt ended"
        );
        let reason = settled.error.expect("a failure carries its reason");
        assert!(reason.contains("dispatch failed"), "{reason}");
        // …and the row agrees with the card, rather than telling a second story.
        let note = only_card(&tasks).await.note.expect("note");
        assert!(note.contains("dispatch failed"), "{note}");
        // No turn ran on this offline fixture, so there is nothing to charge.
        assert_eq!(settled.step_count, 0);
        assert_eq!(settled.usage, TokenUsage::default());
    }

    /// A refusal is an attempt too. It spends nothing and runs no turn, but it
    /// is a real, terminal outcome — the card's history must show "this was
    /// tried and refused, and why" rather than a gap where an attempt was.
    #[tokio::test]
    async fn a_refused_dispatch_settles_its_attempt_rather_than_leaving_a_gap() {
        use crate::ports::runs::RunStatus;

        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks, runs) = brain_with_a_pending_run(dir.path(), "Shane").await;
        let company = CompanyId::new("acme");

        brain.run_task("t-1", Some("run-1")).await.expect("run");

        let settled = runs
            .get_run(&company, "run-1")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(settled.status, RunStatus::Failed);
        let reason = settled.error.expect("reason");
        assert!(reason.contains("dispatch refused"), "{reason}");
        assert!(
            reason.contains("Shane"),
            "the row must name what was wrong, like the note does: {reason}"
        );
    }

    /// The degraded path stays degraded, not broken: a dispatch carrying no run
    /// id runs the card exactly as before and invents no row for it.
    #[tokio::test]
    async fn an_untracked_dispatch_runs_the_card_and_records_no_attempt() {
        use crate::ports::runs::RunFilter;

        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
        let brain = brain.with_runs(Arc::clone(&runs));
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", "engineer"))
            .await
            .expect("seed");

        brain.run_task("t-1", None).await.expect("run");

        assert!(
            only_card(&tasks).await.note.is_some(),
            "the card still ran and still recorded its outcome"
        );
        assert!(
            runs.list_runs(&company, &RunFilter::default())
                .await
                .expect("list")
                .is_empty(),
            "no row was minted for this dispatch, so none may be invented"
        );
    }

    /// A card that vanished between the dispatch write and the cycle still
    /// closes its attempt. Otherwise the row would sit `Pending` until a
    /// restart reaped it with a misleading "the host restarted" reason.
    #[tokio::test]
    async fn a_dispatch_whose_card_is_gone_still_closes_its_attempt() {
        use crate::ports::runs::{NewRun, RunStatus};

        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_tasks(dir.path());
        let runs: Arc<dyn crate::ports::RunStore> = Arc::new(FsOps::new(dir.path()));
        let brain = brain.with_runs(Arc::clone(&runs));
        let company = CompanyId::new("acme");
        runs.create_run(
            &company,
            NewRun {
                id: "run-1".to_string(),
                task_id: "t-gone".to_string(),
                agent_id: "engineer".to_string(),
            },
        )
        .await
        .expect("mint");

        assert!(
            brain
                .run_task("t-gone", Some("run-1"))
                .await
                .expect("run")
                .is_none(),
            "a missing card posts nothing back"
        );

        let settled = runs
            .get_run(&company, "run-1")
            .await
            .expect("read")
            .expect("row");
        assert_eq!(settled.status, RunStatus::Failed);
        assert_eq!(settled.error.as_deref(), Some(CARD_VANISHED));
    }

    // ── Issue #205: the working agent is linked, and a bad assignee is refused ──

    /// The reported bug. A card assigned to "Shane" — nobody this company has —
    /// used to dispatch to the orchestrator anyway, keeping `assignee = "Shane"`
    /// while the timeline read "reply from ceo" and nothing said the name was
    /// invalid. It must now be **refused**: the card goes back to `todo`
    /// carrying the reason, and the orchestrator runs no turn on its behalf.
    #[tokio::test]
    async fn task_dispatch_off_roster_assignee_is_refused_not_silently_reassigned() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "Shane"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let refused = only_card(&tasks).await;
        assert_eq!(
            refused.column, COLUMN_TODO,
            "a card nobody can work must not sit in in_progress"
        );
        assert_eq!(
            refused.assignee, "Shane",
            "the invalid name is left as typed for the operator to correct"
        );
        let note = refused.note.expect("the refusal is written to the note");
        assert!(
            note.contains("Shane"),
            "the operator must be told which name is not a teammate: {note:?}"
        );
        assert!(
            note.contains("dispatch refused"),
            "the note must read as a refusal, not as work the CEO did: {note:?}"
        );
        assert!(
            !note.contains("mock: "),
            "no turn may run for an assignee nobody answers to: {note:?}"
        );
    }

    /// The other half of #205: a card the orchestrator picks up because nobody
    /// was named gets that orchestrator written onto it, so the board names who
    /// is actually doing the work instead of showing a blank assignee.
    #[tokio::test]
    async fn task_dispatch_links_the_working_agent_to_an_unassigned_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(
            only_card(&tasks).await.assignee,
            "ceo",
            "the orchestrator that worked the card must be linked to it"
        );
    }

    /// A card assigned to a **desk** is worked by that desk's lead, but the card
    /// stays the desk's. `delegate_to_desk` writes a desk id into `assignee`, so
    /// this is the shape a hand-off actually produces. Dispatch picks who runs
    /// this turn, not who owns the card, so only the note names the member that
    /// ran it — relinking the lead onto `assignee` would erase the desk from the
    /// board the first time the card ran (#214 review).
    #[tokio::test]
    async fn task_dispatch_routes_a_desk_assignee_to_its_lead_but_keeps_the_desk() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_desk_tasks(dir.path());
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "eng"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let worked = only_card(&tasks).await;
        assert_eq!(
            worked.assignee, "eng",
            "a desk assignment is ownership: the card stays the desk's"
        );
        let note = worked.note.expect("note");
        assert!(
            note.contains("[engineer]"),
            "the desk's lead member still did the work, and the note names them: {note:?}"
        );
    }

    /// An **operator-overlay** teammate is a roster teammate. The narrow
    /// `manifest.agents`-only lookup used to drop these onto the orchestrator.
    #[tokio::test]
    async fn task_dispatch_routes_to_an_overlay_teammate() {
        let dir = tempfile::tempdir().unwrap();
        let (mut brain, tasks) = brain_with_tasks(dir.path());
        brain.record.overlay_agents.push(OverlayAgent {
            id: "nova".into(),
            name: "Nova".into(),
            role: "Growth".into(),
            description: None,
        });
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", "nova"))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let note = only_card(&tasks).await.note.expect("note");
        assert!(
            note.contains("[nova]"),
            "an overlay teammate must work their own card: {note:?}"
        );
    }

    /// A refused dispatch still answers in the thread the card came from —
    /// otherwise a delegated hand-off to a bad assignee is silent twice over.
    #[tokio::test]
    async fn a_refused_dispatch_posts_the_reason_back_to_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-origin", "Shane");
        c.origin_chat_id = Some("strategy".to_string());
        tasks
            .upsert(&CompanyId::new("acme"), &c)
            .await
            .expect("seed");

        let posted = brain
            .run_task("t-origin", None)
            .await
            .expect("run")
            .expect("a refused card with an origin must still post back");
        assert_eq!(
            posted.reply_to.as_ref().map(|r| r.chat_id.as_str()),
            Some("strategy")
        );
        assert_eq!(
            posted.channel,
            brain.orchestrator(),
            "the orchestrator answers for its own roster"
        );
        assert!(posted.text.contains("Shane"), "{}", posted.text);
    }

    /// A dispatch for a card that no longer exists is a silent no-op, not an
    /// error.
    #[tokio::test]
    async fn task_dispatch_missing_card_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "nope".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs without a card");
        assert!(
            tasks
                .list(&CompanyId::new("acme"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    // --- Orchestrator routing + delegation ----------------------------------

    /// A roster with an `orchestrator`-tier agent (not first) and a desk.
    fn record_with_desk() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"
description = "Coordinates the company."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds it."

[[group_chat]]
id = "eng_desk"
name = "Engineering"
members = ["engineer"]
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
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
            template_provenance: None,
        }
    }

    /// A brain over `record`, wired to a real task store.
    fn brain_over(dir: &std::path::Path, record: CompanyRecord) -> (HarnessBrain, Arc<FsOps>) {
        let tasks = Arc::new(FsOps::new(dir));
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record),
            tasks,
        )
    }

    /// A brain over the desk-bearing record, wired to a real task store.
    fn brain_with_desk(dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
        brain_over(dir, record_with_desk())
    }

    /// The default responder is the `orchestrator`-tier agent, even when it is
    /// not first on the roster.
    #[test]
    fn default_responder_is_the_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder, "chief");
    }

    /// An addressed desk routes to its lead member (by id or name); anything else
    /// — the "General" desk, an unknown id, or no address — falls to the
    /// orchestrator.
    #[test]
    fn responder_for_routes_desk_to_lead_else_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        assert_eq!(brain.responder_for(Some("Engineering")), "engineer");
        assert_eq!(brain.responder_for(Some("General")), "chief");
        assert_eq!(brain.responder_for(Some("nope")), "chief");
        assert_eq!(brain.responder_for(None), "chief");
    }

    // ── Issue #151 §3.3: a DM thread reaches the teammate it names ──

    /// A chat id naming a roster teammate answers as that teammate, which is
    /// what a per-agent DM thread is. Before this it fell through to the
    /// orchestrator, so the console would show an agent's thread while someone
    /// else answered in it.
    #[test]
    fn responder_for_routes_a_roster_agent_id_to_that_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        assert_eq!(brain.responder_for(Some("engineer")), "engineer");
        assert_eq!(brain.responder_for(Some("chief")), "chief");
    }

    /// Desks still win. A desk id is resolved as a desk even if an agent shares
    /// the name, so no existing thread changes where it lands.
    #[test]
    fn a_desk_still_outranks_an_agent_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // `eng_desk` is a desk led by `engineer`; it must resolve through the
        // desk path, not the DM path.
        assert_eq!(brain.responder_for(Some("eng_desk")), "engineer");
        // And an id that is neither still reaches the orchestrator.
        assert_eq!(brain.responder_for(Some("not-a-teammate")), "chief");
    }

    /// An operator-added overlay member is resolved as a desk's lead (issue #72):
    /// on a desk the manifest left empty, the overlay addition becomes the lead,
    /// and an addressed message routes to it. Proves `desk_lead`/`responder_for`
    /// read the effective (manifest ∪ overlay) membership.
    #[test]
    fn overlay_member_resolves_as_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        // `design` is a manifest desk with no declared members; the operator adds
        // `engineer` to it through the overlay.
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "chief"
role = "Chief of Staff"
tier = "orchestrator"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "design"
name = "Design"
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
                desk_id: "design".to_string(),
                agent_id: "engineer".to_string(),
            }],
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(brain.desk_lead("design"), Some("engineer".to_string()));
        assert_eq!(brain.responder_for(Some("design")), "engineer");
    }

    /// The operator's desk hierarchy drives the desk lead: a desk with manifest
    /// members `[eng1, eng2]` plus an overlay `cto`, ordered `[cto, eng1, eng2]`,
    /// resolves its lead to `cto` — `desk_lead` reads `effective_desk_members`,
    /// so the reorder flows through with no change to the resolver (issue #131).
    #[test]
    fn desk_order_drives_the_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
        )
        .expect("valid manifest");
        let record = CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: vec![crate::ports::types::OverlayAgent {
                id: "cto".to_string(),
                name: "Cto".to_string(),
                role: "CTO".to_string(),
                description: None,
            }],
            overlay_desk_members: vec![crate::ports::types::OverlayDeskMember {
                desk_id: "eng".to_string(),
                agent_id: "cto".to_string(),
            }],
            overlay_desk_order: vec![crate::ports::types::OverlayDeskOrder {
                desk_id: "eng".to_string(),
                ordered: vec!["cto".to_string(), "eng1".to_string(), "eng2".to_string()],
            }],
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        };
        let (brain, _tasks) = brain_over(dir.path(), record);
        assert_eq!(brain.desk_lead("eng"), Some("cto".to_string()));
    }

    /// Regression for the builder seeding path (#133): a desk-order change written
    /// to the store must take effect on routing once the brain is rebuilt from the
    /// persisted record. The builder used to construct the brain with an empty
    /// `overlay_desk_order`, so desk chats kept routing to the pre-reorder lead.
    /// Here we persist a record, build a brain from the loaded record (blueprint
    /// lead), then write a new order and rebuild the brain from the reloaded record
    /// — the lead must update, not stay stale.
    #[tokio::test]
    async fn desk_order_change_updates_routing_after_rebuild() {
        use crate::ports::store::CompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let store = FsCompanyStore::new(dir.path());
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "eng1"
role = "Engineer One"

[[agent]]
id = "eng2"
role = "Engineer Two"

[[group_chat]]
id = "eng"
name = "Engineering"
members = ["eng1", "eng2"]
"#,
        )
        .expect("valid manifest");
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest,
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        // Brain built from the persisted record before any reorder: blueprint lead.
        let loaded = store.load(&id).await.unwrap().unwrap();
        let (brain, _tasks) = brain_over(dir.path(), loaded);
        assert_eq!(
            brain.desk_lead("eng"),
            Some("eng1".to_string()),
            "blueprint lead before reorder"
        );

        // Operator reorders the desk (as `set_desk_order` does), promoting eng2.
        let mut record = store.load(&id).await.unwrap().unwrap();
        record
            .overlay_desk_order
            .push(crate::ports::types::OverlayDeskOrder {
                desk_id: "eng".to_string(),
                ordered: vec!["eng2".to_string(), "eng1".to_string()],
            });
        store.save(&record).await.unwrap();

        // Rebuild the brain from the reloaded record: routing follows the reorder,
        // no stale lead.
        let reloaded = store.load(&id).await.unwrap().unwrap();
        let (rebuilt, _tasks2) = brain_over(dir.path(), reloaded);
        assert_eq!(
            rebuilt.desk_lead("eng"),
            Some("eng2".to_string()),
            "reorder did not take effect on routing after rebuild"
        );
    }

    /// A `spawn_task` delegation opens a To-do card and surfaces no bubble.
    #[tokio::test]
    async fn spawn_task_delegation_opens_a_todo_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_desk(dir.path());
        let out = brain
            .run_delegation(
                Delegation::SpawnTask {
                    title: "Draft the plan".to_string(),
                    note: Some("by friday".to_string()),
                    assignee: Some("engineer".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            out.bubble.is_none() && out.desk_reply.is_none(),
            "spawn_task surfaces nothing to relay or bubble"
        );

        let cards = tasks.list(&CompanyId::new("acme")).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Draft the plan");
        assert_eq!(cards[0].column, COLUMN_TODO);
        assert_eq!(cards[0].assignee, "engineer");
        // Issue #246: it surfaces no *bubble*, but it no longer surfaces
        // *nothing* — the card it opened is reported, which is what lets the
        // caller tell the operator a card exists instead of leaving them to
        // notice it on the board.
        assert_eq!(
            out.spawned_task.as_deref(),
            Some(cards[0].id.as_str()),
            "the opened card must be reported, and be the one actually written"
        );
    }

    /// Issue #246: a chat turn that opened a card says so on the bubble it
    /// answered from. Before this the card appeared on the board and the reply
    /// carried nothing tying the two together, so an operator had no way to
    /// tell a turn that opened work from one that only talked about it.
    #[tokio::test]
    async fn a_turn_that_opens_a_card_reports_it_on_the_operator_bubble() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::SpawnTask {
                title: "Draft the announcement".to_string(),
                note: None,
                assignee: None,
            })],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "we should announce this".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        let reported = bubble.task_id.as_deref().expect("the bubble names a card");
        let cards = brain
            .deps
            .tasks
            .as_ref()
            .unwrap()
            .list(&brain.record.id)
            .await
            .unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(
            reported, cards[0].id,
            "the reported card must be the one on the board"
        );
    }

    /// Issue #246, the documented limitation stated as a test rather than only
    /// as prose: a turn that opens several cards reports the **first**. The
    /// journal field this feeds is a single optional id, so widening it would
    /// break the byte-identical round-trip every already-stored reply relies
    /// on. Pinned to *first* — not "whichever won" — because a later spawn
    /// silently overwriting an earlier one would make the reported card depend
    /// on queue order, which is the model's choice, not a contract.
    #[tokio::test]
    async fn a_turn_that_opens_several_cards_reports_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![
                Delegation::SpawnTask {
                    title: "First".to_string(),
                    note: None,
                    assignee: None,
                },
                Delegation::SpawnTask {
                    title: "Second".to_string(),
                    note: None,
                    assignee: None,
                },
            ]],
            TurnFaults::default(),
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "two things".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let reported = result.channel_responses[0]
            .task_id
            .as_deref()
            .expect("the bubble names a card");
        let cards = brain
            .deps
            .tasks
            .as_ref()
            .unwrap()
            .list(&brain.record.id)
            .await
            .unwrap();
        assert_eq!(cards.len(), 2, "both cards are opened either way");
        let first = cards
            .iter()
            .find(|c| c.title == "First")
            .expect("the first card exists");
        assert_eq!(
            reported, first.id,
            "the bubble reports the first card opened, not the last"
        );
    }

    /// The other side of the same contract: a turn that opened no card must
    /// leave the field empty, so no bubble grows a "card opened" chip it has
    /// not earned.
    #[tokio::test]
    async fn a_turn_that_opens_no_card_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _provider) = brain_that_delegates(dir.path(), Vec::new());

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "status?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert!(
            result.channel_responses[0].task_id.is_none(),
            "an ordinary chat turn must not claim a card"
        );
    }

    // ── Issue #186 part b: orchestrator lifecycle authority ────────────────

    /// `assign_task` changes who owns an existing card, records the change in
    /// the orchestrator's voice, and — deliberately — does **not** touch the
    /// column: dispatch fires from `CompanyRuntime::upsert_task`, which the
    /// `TaskStore` port this drain writes through cannot reach.
    #[tokio::test]
    async fn assign_task_reassigns_the_card_without_dispatching_it() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-assign", "engineer");
        c.column = COLUMN_TODO.to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        let out = brain
            .run_delegation(
                Delegation::AssignTask {
                    task_id: "t-assign".to_string(),
                    assignee: "ceo".to_string(),
                    note: Some("closer to the customer".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            out.bubble.is_none() && out.desk_reply.is_none(),
            "the orchestrator is mid-turn; a second voice here would be it talking to itself"
        );

        let after = only_card(&tasks).await;
        assert_eq!(after.assignee, "ceo");
        assert_eq!(
            after.column, COLUMN_TODO,
            "assignment records ownership; it must not start the work"
        );
        let note = after.note.expect("note");
        assert!(note.contains("assigned to ceo"), "{note}");
        assert!(note.contains("closer to the customer"), "{note}");
        assert!(
            note.contains(&format!("[{}]", brain.orchestrator())),
            "the assignment is recorded in the orchestrator's voice: {note}"
        );
    }

    /// #205: `assign_task` takes its `assignee` from an LLM tool call, so it can
    /// name somebody the company does not have just as easily as the operator's
    /// free-text field can. The bad name must not reach the card — the previous
    /// owner stays, and the refusal is recorded on the note.
    #[tokio::test]
    async fn assign_task_refuses_an_off_roster_assignee_and_keeps_the_current_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-assign", "engineer");
        c.column = COLUMN_TODO.to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::AssignTask {
                    task_id: "t-assign".to_string(),
                    assignee: "Shane".to_string(),
                    note: None,
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(
            after.assignee, "engineer",
            "a name nobody answers to must not displace the real owner"
        );
        let note = after.note.expect("note");
        assert!(note.contains("could not assign to Shane"), "{note}");
    }

    /// #214 review: a blank `assignee` resolves to `Unassigned`, whose canonical
    /// form is `""`. Clearing the owner is correct — unassigning is a real
    /// request — but the note must say so. It used to fall through the named
    /// arm and record `assigned to ` with nothing after it.
    #[tokio::test]
    async fn assign_task_with_a_blank_assignee_clears_the_owner_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-assign", "engineer");
        c.column = COLUMN_TODO.to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::AssignTask {
                    task_id: "t-assign".to_string(),
                    assignee: "   ".to_string(),
                    note: None,
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(
            after.assignee, "",
            "a blank assignee unassigns the card, which is a legitimate write"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("cleared the assignee"),
            "the note names the effect rather than trailing off: {note}"
        );
        assert!(
            !note.contains("assigned to "),
            "the truncated 'assigned to <nothing>' note must not come back: {note}"
        );
    }

    /// Approving finishes a board-created card: this is #171's `in_review →
    /// done` write (PR #179) for the card shape #179's own origin rule cannot
    /// reach, with the verdict recorded on the note.
    #[tokio::test]
    async fn review_approve_records_the_verdict_and_completes_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-review", "engineer");
        c.column = "in_review".to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::ReviewTask {
                    task_id: "t-review".to_string(),
                    decision: lifecycle::ReviewDecision::Approve,
                    note: Some("ships as-is".to_string()),
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(
            after.column, "done",
            "an approving verdict is the in_review -> done transition (#171)"
        );
        let note = after.note.expect("note");
        assert!(note.contains("reviewed: approved"), "{note}");
        assert!(note.contains("ships as-is"), "{note}");
    }

    /// `revise` is a transition #186 does own: the card goes back to the
    /// To-do so it can be picked up and re-dispatched.
    #[tokio::test]
    async fn review_revise_sends_the_card_back_to_todo() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());
        let mut c = card("t-revise", "engineer");
        c.column = "in_review".to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();

        brain
            .run_delegation(
                Delegation::ReviewTask {
                    task_id: "t-revise".to_string(),
                    decision: lifecycle::ReviewDecision::Revise,
                    note: None,
                },
                None,
            )
            .await
            .expect("delegation runs");

        let after = only_card(&tasks).await;
        assert_eq!(after.column, COLUMN_TODO);
        assert!(
            after.note.expect("note").contains("needs another pass"),
            "the verdict must be recorded even without a reviewer comment"
        );
    }

    /// A card that has since been deleted is a silent no-op, matching every
    /// other task path in this file — never an error that kills the turn.
    #[tokio::test]
    async fn a_lifecycle_delegation_for_a_missing_card_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks) = brain_with_tasks(dir.path());

        for delegation in [
            Delegation::AssignTask {
                task_id: "ghost".to_string(),
                assignee: "ceo".to_string(),
                note: None,
            },
            Delegation::ReviewTask {
                task_id: "ghost".to_string(),
                decision: lifecycle::ReviewDecision::Approve,
                note: None,
            },
        ] {
            let out = brain
                .run_delegation(delegation, None)
                .await
                .expect("a missing card must not error");
            assert!(out.bubble.is_none() && out.desk_reply.is_none());
        }
        assert!(
            tasks
                .list(&CompanyId::new("acme"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A `delegate_to_desk` delegation runs the desk lead and hands its reply
    /// back to relay (a `DeskReply` attributed to the lead, no standalone
    /// bubble); an unknown desk yields nothing.
    #[tokio::test]
    async fn delegate_to_desk_delegation_answers_as_the_desk_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _tasks) = brain_with_desk(dir.path());
        // The pool must have the roster before a member turn can run.
        brain
            .pool
            .ensure(&brain.record, &brain.deps)
            .await
            .expect("roster");

        let out = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "ship-marker".to_string(),
                },
                None,
            )
            .await
            .expect("delegation runs");
        // The answer comes back as a DeskReply to relay — not a standalone
        // bubble — attributed to the desk lead, and the mock provider echoes the
        // instruction, proving the member's turn ran.
        assert!(
            out.bubble.is_none(),
            "the desk reply is relayed, not bubbled"
        );
        let desk = out.desk_reply.expect("desk lead replies");
        assert_eq!(desk.member, "engineer");
        assert!(desk.reply.contains("ship-marker"), "{:?}", desk.reply);

        // An unknown desk delegates to nobody.
        let none = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "ghost".to_string(),
                    instruction: "hello".to_string(),
                },
                None,
            )
            .await
            .expect("delegation runs");
        assert!(
            none.bubble.is_none() && none.desk_reply.is_none(),
            "an unknown desk yields nothing"
        );
    }

    // --- MCP failure drain --------------------------------------------------

    /// A recorded MCP failure re-skins into an **error step** on the operator
    /// bubble's timeline AND a scrubbed `McpCallFailed` audit event when the
    /// event log is wired (the Activity-trace re-skin of the old warning bubble).
    #[tokio::test]
    async fn mcp_failures_surface_as_error_steps_and_event() {
        use crate::harness::mcp_probe::McpFailure;
        use crate::ports::EventLog;
        use crate::ports::types::EventSeq;
        use crate::store::FsEventLog;

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        let failures = crate::harness::mcp_probe::McpFailureQueue::default();
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir.path())),
            store: Arc::new(FsCompanyStore::new(dir.path())),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(events.clone()),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: failures.clone(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record());

        // A failure recorded during the turn (its message already scrubbed).
        failures.push(McpFailure {
            server: "browserbase".into(),
            tool: "browse".into(),
            status: "tool_call_rejected".into(),
            hint: None,
            scrubbed_message: "server rejected the call".into(),
        });

        let mut steps: Vec<TurnStep> = Vec::new();
        // `None` — this is the chat-turn drain, which journals no `task_id`
        // (#185). The dispatch drain passes the card id; see `run_task`.
        brain
            .surface_mcp_failures(&mut steps, None)
            .await
            .expect("drain surfaces failures");

        assert_eq!(steps.len(), 1, "one error step");
        assert_eq!(steps[0].kind, TurnStepKind::Note);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert!(
            steps[0].label.contains("browserbase"),
            "{:?}",
            steps[0].label
        );
        assert_eq!(steps[0].detail.as_deref(), Some("server rejected the call"));

        let logged = events
            .read_from(&CompanyId::new("acme"), EventSeq::new(0), usize::MAX)
            .await
            .expect("read events");
        assert!(
            logged.iter().any(|e| matches!(
                &e.event,
                CompanyEvent::McpCallFailed { server, status, .. }
                    if server == "browserbase" && status == "tool_call_rejected"
            )),
            "an McpCallFailed audit event was journaled"
        );
    }

    /// #185 review follow-up: one bad journal write must not swallow the rest of
    /// the batch.
    ///
    /// `McpFailureQueue::drain` is a `mem::take` — by the time the loop runs the
    /// queue is empty and the batch exists only in that iterator. Propagating
    /// the first append error with `?` therefore did not merely skip one audit
    /// event, it discarded every failure behind it with nothing left to retry
    /// from. Journaling is per-item best-effort so the drain always completes.
    #[tokio::test]
    async fn a_failed_journal_write_does_not_swallow_the_rest_of_the_drain() {
        use crate::harness::mcp_probe::McpFailure;
        use crate::ports::EventLog;
        use crate::ports::types::{EventSeq, StoredEvent};
        use futures::stream::{self, BoxStream};

        /// An event log whose FIRST append fails and whose later appends
        /// succeed, recording what got through.
        #[derive(Default)]
        struct FailFirstLog {
            seen: StdMutex<Vec<CompanyEvent>>,
            appends: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl EventLog for FailFirstLog {
            async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
                let nth = self
                    .appends
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if nth == 0 {
                    return Err(crate::error::OpenCompanyError::Store(
                        "journal unavailable".to_string(),
                    ));
                }
                let mut guard = self.seen.lock().unwrap();
                guard.push(event);
                Ok(EventSeq::new(guard.len() as u64))
            }
            async fn read_from(
                &self,
                _id: &CompanyId,
                _seq: EventSeq,
                _limit: usize,
            ) -> Result<Vec<StoredEvent>> {
                Ok(Vec::new())
            }
            fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, StoredEvent> {
                Box::pin(stream::empty())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(FailFirstLog::default());
        let failures = crate::harness::mcp_probe::McpFailureQueue::default();
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir.path())),
            store: Arc::new(FsCompanyStore::new(dir.path())),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(log.clone()),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: failures.clone(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record());

        for server in ["first", "second", "third"] {
            failures.push(McpFailure {
                server: server.into(),
                tool: "browse".into(),
                status: "tool_call_rejected".into(),
                hint: None,
                scrubbed_message: "server rejected the call".into(),
            });
        }

        let mut steps: Vec<TurnStep> = Vec::new();
        brain
            .surface_mcp_failures(&mut steps, Some("t1"))
            .await
            .expect("a journal error is best-effort, not fatal");

        // Every failure is re-skinned onto the timeline regardless…
        assert_eq!(steps.len(), 3, "all three failures surfaced as steps");
        // …and the two after the failed write still reached the journal. Before
        // this fix `seen` was empty: the `?` returned on `first` and `second` /
        // `third` were dropped with the drained batch.
        let seen = log.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the drain continued past the failed append");
        assert!(
            seen.iter().any(|e| matches!(
                e,
                CompanyEvent::McpCallFailed { server, .. } if server == "third"
            )),
            "the last failure in the batch was still journaled"
        );
    }

    // --- Approval parking (issue #172) --------------------------------------

    /// A brain over `dir` whose deps carry `requests` as the shared
    /// approval-request queue — the same handle every roster agent's
    /// `ApprovalPolicy` pushes onto.
    fn brain_with_approval_queue(
        dir: &std::path::Path,
        requests: crate::harness::policy::ApprovalRequestQueue,
    ) -> HarnessBrain {
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: requests,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    /// The regression for #172: a `RequireApproval` recorded during a turn is
    /// **parked** on the host, so it lands in the journal the Approvals page
    /// reads instead of being narrated away in chat and lost.
    ///
    /// `ParkingHost` panics on `emit_effect`, which pins the other half of the
    /// fix: the request must NOT be re-evaluated by the runtime gate (which
    /// allows — and so silently "executes" — the `Other` group most gated tool
    /// calls classify into).
    #[tokio::test]
    async fn approval_requests_are_parked_for_the_operator() {
        use crate::harness::policy::{ApprovalPolicy, ApprovalRequestQueue};
        use openhuman_core::openhuman::agent::tool_policy::{
            ToolCallContext, ToolPolicy, ToolPolicyDecision, ToolPolicyRequest,
        };

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());

        // Exactly what a supervised policy records when the agent reaches for a
        // gated tool mid-turn.
        let policy = ApprovalPolicy::new(
            &crate::company::Policy {
                mode: "supervised".to_string(),
                always_approve: Vec::new(),
                auto_approve_under_usd: None,
            },
            None,
        )
        .with_requests(requests.clone());
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        let request = ToolPolicyRequest::new(
            "composio_execute",
            args.clone(),
            ToolCallContext::session("s", "chat", "ceo", "call-1", 0),
        );
        assert!(
            matches!(
                policy.check(&request).await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "the fixture must reproduce a gated call"
        );
        assert_eq!(requests.queued(), 1, "the decision was recorded to park");

        let host = ParkingHost::default();
        brain
            .park_approval_requests(&host)
            .await
            .expect("the drain parks");

        let parked = host.parked();
        assert_eq!(parked.len(), 1, "one approval reached the operator");
        assert_eq!(parked[0].kind, "composio_execute");
        assert_eq!(
            parked[0].payload, args,
            "the call's arguments are preserved"
        );
        assert_eq!(requests.queued(), 0, "the queue is drained");
    }

    /// A second drain parks nothing: the queue is emptied, so a later cycle
    /// can't re-park a request the operator has already been shown.
    #[tokio::test]
    async fn draining_twice_parks_nothing_the_second_time() {
        use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
        use crate::ports::types::EffectGroup;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());
        requests.push(ApprovalRequest {
            tool: "media_generate_image".to_string(),
            reason: "supervised".to_string(),
            effect: Effect {
                kind: "media_generate_image".to_string(),
                group: EffectGroup::Spend,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: serde_json::json!({ "prompt": "a logo" }),
                agent: None,
                run_id: None,
            },
        });

        let host = ParkingHost::default();
        brain.park_approval_requests(&host).await.expect("drain");
        brain
            .park_approval_requests(&host)
            .await
            .expect("second drain");
        assert_eq!(host.parked().len(), 1, "parked once, not twice");
    }

    /// A host that fails to park the *first* effect it is handed, then behaves.
    /// Models a transient journal/IO fault mid-batch.
    #[derive(Default)]
    struct FlakyParkingHost {
        parked: std::sync::Mutex<Vec<Effect>>,
        seen: std::sync::atomic::AtomicUsize,
    }

    impl FlakyParkingHost {
        fn parked(&self) -> Vec<Effect> {
            self.parked.lock().expect("parked").clone()
        }
    }

    #[async_trait]
    impl CycleHost for FlakyParkingHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            panic!("an approval request must be parked, never re-evaluated as an effect");
        }
        async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
            if self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err(crate::OpenCompanyError::Store(
                    "journal on fire".to_string(),
                ));
            }
            let mut parked = self.parked.lock().expect("parked");
            parked.push(effect);
            Ok(ApprovalId::new(format!("appr-{}", parked.len())))
        }
    }

    /// One failed park must not take the rest of the batch — or the turn's reply
    /// — down with it. `drain` has already emptied the shared queue, so a `?`
    /// here would lose every later request forever and abort `run_cycle`,
    /// reproducing for the remainder of the batch exactly the silent
    /// disappearance this issue fixes.
    #[tokio::test]
    async fn a_failed_park_does_not_drop_the_rest_of_the_batch() {
        use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
        use crate::ports::types::EffectGroup;

        let dir = tempfile::tempdir().unwrap();
        let requests = ApprovalRequestQueue::default();
        let brain = brain_with_approval_queue(dir.path(), requests.clone());
        for tool in ["first_tool", "second_tool", "third_tool"] {
            requests.push(ApprovalRequest {
                tool: tool.to_string(),
                reason: "supervised".to_string(),
                effect: Effect {
                    kind: tool.to_string(),
                    group: EffectGroup::Other,
                    amount_usd: None,
                    established_thread: false,
                    first_time_counterparty: false,
                    payload: serde_json::json!({ "tool": tool }),
                    agent: None,
                    run_id: None,
                },
            });
        }

        let host = FlakyParkingHost::default();
        brain
            .park_approval_requests(&host)
            .await
            .expect("a park failure is logged, not propagated");

        // The first park failed; the two after it still reached the operator.
        let parked = host.parked();
        assert_eq!(parked.len(), 2, "the batch continued past the failure");
        assert_eq!(parked[0].kind, "second_tool");
        assert_eq!(parked[1].kind, "third_tool");
    }

    // --- Re-dispatching a granted call (issue #243) --------------------------

    /// A brain over the offline mock provider, wired to a real event log and a
    /// shared approval queue (whose grant set the runtime would mint into).
    fn brain_with_queue_and_events(
        dir: &std::path::Path,
        requests: crate::harness::policy::ApprovalRequestQueue,
        events: Arc<dyn crate::ports::EventLog>,
    ) -> HarnessBrain {
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: Some(events),
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: requests,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer: crate::company::steer::InflightRegistry::default(),
            delivery: None,
            search: None,
            workspace: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
    }

    fn approval_resolved(id: &str, verdict: Verdict) -> CompanyEvent {
        CompanyEvent::ApprovalResolved {
            approval_id: ApprovalId::new(id),
            verdict,
            by: crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::Operator,
                id: "owner".into(),
            },
        }
    }

    fn cycle_over(events: Vec<CompanyEvent>) -> CycleRequest {
        CycleRequest {
            cycle_id: "cyc-1".to_string(),
            company_id: CompanyId::new("acme"),
            events,
            event_seqs: Vec::new(),
            compressed_history: Vec::new(),
            roster: vec!["ceo".to_string()],
            context_index: Vec::new(),
        }
    }

    /// The arm that made #243 visible: an approved grant re-dispatches its agent
    /// with the exact arguments, answers on that agent's channel, and journals
    /// the reply.
    ///
    /// Before this arm existed, `ApprovalResolved` fell into `_ => {}`: no turn,
    /// no response, and the cycle ended on the "Acknowledged." fallback. The
    /// operator approved, read "Acknowledged.", and nothing ran — which looks
    /// exactly like success.
    ///
    /// `MockProvider` echoes the user message back, so the reply text IS the
    /// instruction the agent received — which is what makes argument fidelity
    /// assertable offline.
    #[tokio::test]
    async fn an_approved_grant_redispatches_its_agent_with_the_exact_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL", "to": "a@b.test" });
        requests
            .grants()
            .grant(crate::runtime::grants::GrantedCall {
                approval_id: ApprovalId::new("appr-1"),
                agent: "ceo".into(),
                tool: "composio_execute".into(),
                args: args.clone(),
                at_millis: now_millis(),
            });
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // A real bubble, on the GRANTING agent's channel — not the generic
        // "Acknowledged." fallback and not the operator channel.
        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "ceo");
        assert_ne!(bubble.text, "Acknowledged.");

        // The instruction carried the tool and the arguments VERBATIM. A model
        // that re-issues with drifted arguments re-parks (see the policy tests),
        // so the fidelity of this string is what makes the round-trip land.
        assert!(bubble.text.contains("composio_execute"), "{}", bubble.text);
        assert!(
            bubble.text.contains(&serde_json::to_string(&args).unwrap()),
            "the exact approved arguments must reach the agent: {}",
            bubble.text
        );
        assert!(
            bubble.text.contains("Do not modify them"),
            "{}",
            bubble.text
        );

        // The reply is journaled under the agent's own chat thread, so the echo
        // survives a console refresh rather than living only in this response.
        let stored = log
            .read_from(&CompanyId::new("acme"), crate::ports::EventSeq::new(0), 100)
            .await
            .unwrap();
        let replies: Vec<_> = stored
            .iter()
            .filter_map(|e| match &e.event {
                CompanyEvent::AgentReply {
                    chat_id,
                    agent_id,
                    text,
                    ..
                } => Some((chat_id.clone(), agent_id.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(replies.len(), 1, "the re-issued call's reply is journaled");
        assert_eq!(
            replies[0].0, "ceo",
            "routed into the granting agent's thread"
        );
        assert_eq!(replies[0].1, "ceo");
        assert_eq!(replies[0].2, bubble.text);
    }

    /// A DENIED approval runs no turn. "No" must never re-dispatch anything.
    #[tokio::test]
    async fn a_denied_approval_redispatches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        // A grant for a DIFFERENT approval is live, to prove the arm keys on the
        // resolved id rather than reaching for whatever is lying around.
        requests
            .grants()
            .grant(crate::runtime::grants::GrantedCall {
                approval_id: ApprovalId::new("appr-other"),
                agent: "ceo".into(),
                tool: "composio_execute".into(),
                args: serde_json::json!({}),
                at_millis: now_millis(),
            });
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-1", Verdict::Deny)]),
                &NoopHost,
            )
            .await
            .unwrap();

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(
            result.channel_responses[0].text, "Acknowledged.",
            "a deny falls through to the fallback, exactly as before #243"
        );
        assert!(
            log.read_from(&CompanyId::new("acme"), crate::ports::EventSeq::new(0), 100)
                .await
                .unwrap()
                .is_empty(),
            "nothing is journaled for a deny"
        );
    }

    /// An approved resolution with NO grant behind it is a silent no-op.
    ///
    /// This is the common case, not an edge: a native effect the runtime already
    /// executed, a legacy parked effect from before `Effect::agent` existed (it
    /// replays as `None` and mints nothing), a grant already consumed, and a
    /// grant already swept all land here. Every one of them must keep the exact
    /// pre-#243 behaviour rather than manufacturing a turn.
    #[tokio::test]
    async fn an_approval_with_no_grant_is_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<dyn crate::ports::EventLog> =
            Arc::new(crate::store::FsEventLog::new(dir.path().to_path_buf()));
        let requests = crate::harness::policy::ApprovalRequestQueue::default();
        let brain = brain_with_queue_and_events(dir.path(), requests, log.clone());

        let result = brain
            .run_cycle(
                cycle_over(vec![approval_resolved("appr-native", Verdict::Approve)]),
                &NoopHost,
            )
            .await
            .unwrap();

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
        assert!(
            log.read_from(&CompanyId::new("acme"), crate::ports::EventSeq::new(0), 100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // --- Steer disposition (issue #111) -------------------------------------

    use crate::company::steer::{InflightKind, InflightRegistry};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// A model that steers its OWN in-flight run on selected turns (via the
    /// shared registry), so the disposition matrix can be driven deterministically
    /// over an offline turn. It pops one queued action per [`invoke`](ChatModel::invoke)
    /// call and applies it against `key`, then echoes the last user message.
    struct SteeringProvider {
        steer: InflightRegistry,
        company: CompanyId,
        key: String,
        actions: StdMutex<VecDeque<SteerAction>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ChatModel<()> for SteeringProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(action) = self.actions.lock().unwrap().pop_front() {
                let key = if self.key.is_empty() {
                    self.steer
                        .list(&self.company)
                        .into_iter()
                        .next()
                        .map(|entry| entry.key)
                        .unwrap_or_default()
                } else {
                    self.key.clone()
                };
                let _ = self.steer.steer(&self.company, &key, action);
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("did: {message}")))
        }
    }

    impl HarnessModel for SteeringProvider {
        fn telemetry_provider_id(&self) -> String {
            "steering".to_string()
        }
    }

    /// A brain whose provider steers the dispatched card `key` with `actions`
    /// (one per turn). Returns the brain + its task store so a test can seed the
    /// card and read the disposition back.
    fn brain_that_steers_itself(
        dir: &std::path::Path,
        key: &str,
        actions: Vec<SteerAction>,
    ) -> (HarnessBrain, Arc<FsOps>, Arc<SteeringProvider>) {
        let steer = InflightRegistry::new();
        let tasks = Arc::new(FsOps::new(dir));
        let provider = Arc::new(SteeringProvider {
            steer: steer.clone(),
            company: CompanyId::new("acme"),
            key: key.to_string(),
            actions: StdMutex::new(actions.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let deps = HarnessDeps {
            provider: provider.clone(),
            provider_slug: "steering".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks.clone()),
            // Same handle as `tasks` (FsOps is both stores), so a steered run's
            // artifact side effect — or the absence of one — is observable.
            artifacts: Some(tasks.clone()),
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: orchestrator::DelegationQueue::default(),
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer,
            delivery: None,
            search: None,
            workspace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()),
            tasks,
            provider,
        )
    }

    /// Cancel mid-flight → the card returns to `todo`, the partial reply is
    /// DISCARDED, and only the operator cancellation note lands.
    #[tokio::test]
    async fn steer_cancel_returns_to_todo_and_discards_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, _) =
            brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Cancel]);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, COLUMN_TODO);
        let note = moved.note.expect("note");
        assert!(note.contains("cancelled while in flight"), "{note:?}");
        // The agent's partial reply must NOT be preserved on a cancel.
        assert!(
            !note.contains("did: "),
            "cancel discards the partial: {note:?}"
        );
    }

    /// Pause mid-flight → the card parks in the new `paused` column and the
    /// partial reply is PRESERVED in the note.
    #[tokio::test]
    async fn steer_pause_parks_in_paused_and_preserves_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, tasks, _) =
            brain_that_steers_itself(dir.path(), "t1", vec![SteerAction::Pause]);
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        assert_eq!(moved.column, "paused");
        let note = moved.note.expect("note");
        assert!(note.contains("[paused]"), "{note:?}");
        assert!(
            note.contains("did: "),
            "pause preserves the partial: {note:?}"
        );
    }

    /// Redirect on every turn → the run re-runs in-loop carrying the operator
    /// instruction, and the per-dispatch redirect cap (3) finalizes it to
    /// `in_review` instead of looping forever.
    #[tokio::test]
    async fn steer_redirect_reruns_and_the_cap_finalizes_to_in_review() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = || SteerAction::Redirect {
            instruction: "focus on the API".to_string(),
        };
        // Steer a redirect on the first several turns; the cap should stop it.
        let (brain, tasks, provider) = brain_that_steers_itself(
            dir.path(),
            "t1",
            vec![redirect(), redirect(), redirect(), redirect()],
        );
        tasks
            .upsert(&CompanyId::new("acme"), &card("t1", ""))
            .await
            .unwrap();

        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: "t1".into(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        let moved = only_card(&tasks).await;
        // Redirect budget exhausted → finalized, not looping.
        assert_eq!(moved.column, "in_review");
        let note = moved.note.expect("note");
        // The operator instruction was carried into the rerun, and the reruns
        // echoed it back through the "Operator redirect:" preamble.
        assert!(note.contains("focus on the API"), "{note:?}");
        assert!(
            note.contains("Operator redirect:"),
            "the rerun carried the operator instruction: {note:?}"
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "one initial turn plus three reruns"
        );
    }

    #[tokio::test]
    async fn steer_cancelled_delegation_returns_no_bubble() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, _, _) = brain_that_steers_itself(dir.path(), "", vec![SteerAction::Cancel]);

        let result = brain
            .run_delegation(
                Delegation::DelegateToDesk {
                    desk: "engineering".to_string(),
                    instruction: "investigate".to_string(),
                },
                None,
            )
            .await
            .expect("cancellation is handled");

        assert!(
            result.bubble.is_none() && result.desk_reply.is_none(),
            "cancelled delegation must not bubble or relay"
        );
    }

    // --- CEO-relay hand-back (delegate_to_desk second turn) ------------------

    /// A provider that simulates the orchestrator queuing a `delegate_to_desk`
    /// on its turns: on each invoke it pops the next scripted delegation (if any)
    /// onto the shared queue — exactly what the real tool call does — then echoes
    /// the last user message so a test can read the turn's reply. Sharing the
    /// queue handle with [`HarnessDeps::delegations`] is what lets the brain
    /// drain it after the turn.
    struct DelegatingProvider {
        queue: orchestrator::DelegationQueue,
        pushes: StdMutex<VecDeque<Vec<Delegation>>>,
        calls: std::sync::atomic::AtomicUsize,
        /// The same task store the brain writes through, so each invoke can
        /// snapshot the board **as that turn sees it** — the only way a test can
        /// observe a dispatched card mid-run (issue #204).
        tasks: Arc<FsOps>,
        /// `(column, assignee)` of the company's card at each invoke, in order.
        board: StdMutex<Vec<(String, String)>>,
        /// How this provider misbehaves, by invoke number.
        faults: TurnFaults,
        /// The same registry wired into [`HarnessDeps::steer`], so a scripted
        /// invoke can cancel its own in-flight delegation.
        steer: InflightRegistry,
    }

    /// How a [`DelegatingProvider`] misbehaves, keyed by 1-based invoke number
    /// (issue #213 review).
    #[derive(Default)]
    struct TurnFaults {
        /// Every invoke from here on ERRORS instead of answering, so a test can
        /// make a delegate's own run fail. A *from*, not an *on*: openhuman's
        /// agent loop retries a failed provider call within the same turn, so
        /// failing a single invoke only makes the turn succeed on its retry.
        fail_from: Option<usize>,
        /// Invokes that CANCEL their own in-flight delegation mid-run, so the
        /// delegated reply is discarded exactly as an operator cancel does.
        cancel_on: Vec<usize>,
        /// Desk keys the first turn's `delegate_to_desk` calls named and the
        /// tool REFUSED (issue #272). A refusal never becomes a `Delegation`,
        /// so this is how a test reproduces one without standing up the tool.
        refused_on_first: Vec<String>,
    }

    impl DelegatingProvider {
        /// The board snapshot each turn ran against, in invoke order.
        fn board(&self) -> Vec<(String, String)> {
            self.board.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChatModel<()> for DelegatingProvider {
        async fn invoke(
            &self,
            _state: &(),
            request: ModelRequest,
        ) -> tinyagents::Result<ModelResponse> {
            let invoke = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if self.faults.fail_from.is_some_and(|from| invoke >= from) {
                return Err(tinyagents::TinyAgentsError::Model(
                    "the delegate's provider fell over".to_string(),
                ));
            }
            if self.faults.cancel_on.contains(&invoke) {
                // The delegation's own in-flight entry, not the dispatched
                // card's — cancelling the card would end the whole run.
                let company = CompanyId::new("acme");
                if let Some(entry) = self
                    .steer
                    .list(&company)
                    .into_iter()
                    .find(|e| e.kind == InflightKind::Delegation)
                {
                    let _ = self.steer.steer(&company, &entry.key, SteerAction::Cancel);
                }
            }
            let snapshot = self
                .tasks
                .list(&CompanyId::new("acme"))
                .await
                .ok()
                .and_then(|cards| cards.into_iter().next())
                .map(|card| (card.column, card.assignee))
                .unwrap_or_default();
            self.board.lock().unwrap().push(snapshot);
            for delegation in self.pushes.lock().unwrap().pop_front().unwrap_or_default() {
                self.queue.push(delegation);
            }
            if invoke == 1 {
                for desk in &self.faults.refused_on_first {
                    self.queue.push_refusal(desk.clone());
                }
            }
            let message = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::User(_)))
                .map(|m| m.text())
                .unwrap_or_default();
            Ok(ModelResponse::assistant(format!("did: {message}")))
        }
    }

    impl HarnessModel for DelegatingProvider {
        fn telemetry_provider_id(&self) -> String {
            "delegating".to_string()
        }
    }

    /// A brain over the desk-bearing record whose provider is a
    /// [`DelegatingProvider`] scripted to push `pushes[i]` on invoke `i + 1`.
    /// Returns the brain plus the shared provider so a test can read the invoke
    /// count.
    fn brain_that_delegates(
        dir: &std::path::Path,
        pushes: Vec<Option<Delegation>>,
    ) -> (HarnessBrain, Arc<DelegatingProvider>) {
        brain_that_delegates_with(
            dir,
            pushes.into_iter().map(Vec::from_iter).collect(),
            TurnFaults::default(),
        )
    }

    /// [`brain_that_delegates`], but each invoke pushes a whole *set* of
    /// delegations (a single turn can queue several), and per-invoke faults can
    /// make a delegate's run fail or be cancelled mid-flight.
    fn brain_that_delegates_with(
        dir: &std::path::Path,
        pushes: Vec<Vec<Delegation>>,
        faults: TurnFaults,
    ) -> (HarnessBrain, Arc<DelegatingProvider>) {
        let queue = orchestrator::DelegationQueue::default();
        let tasks = Arc::new(FsOps::new(dir));
        let steer = InflightRegistry::new();
        let provider = Arc::new(DelegatingProvider {
            queue: queue.clone(),
            pushes: StdMutex::new(pushes.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            tasks: tasks.clone(),
            board: StdMutex::new(Vec::new()),
            faults,
            steer: steer.clone(),
        });
        let deps = HarnessDeps {
            provider: provider.clone(),
            provider_slug: "delegating".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: None,
            workspace_root: dir.to_path_buf(),
            model_override: None,
            tasks: Some(tasks),
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            artifacts: None,
            delegations: queue,
            workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
            approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            steer,
            delivery: None,
            search: None,
            workspace: None,
        };
        (
            HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_with_desk()),
            provider,
        )
    }

    /// (a) After a `delegate_to_desk`, the operator-facing reply is a SECOND
    /// orchestrator turn that relays the teammate's answer — one coherent
    /// bubble, not a disconnected sibling.
    #[tokio::test]
    async fn delegate_to_desk_relays_the_answer_in_a_second_orchestrator_turn() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 (orchestrator) queues a delegate_to_desk; invoke 2 is the desk
        // lead's turn; invoke 3 is the relay turn (queues nothing).
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "diagnose the outage".to_string(),
            })],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "why is the site down?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // The operator sees ONE bubble — the CEO's relay, not a separate teammate
        // sibling bubble.
        assert_eq!(result.channel_responses.len(), 1);
        let bubble = &result.channel_responses[0];
        assert_eq!(bubble.channel, "operator");
        // Three turns ran: orchestrator → desk lead → exactly one relay turn.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "orchestrator, desk lead, then exactly one relay turn"
        );
        // The relayed bubble carries the teammate's answer (the desk lead echoed
        // its instruction, and the relay prompt embeds that reply under an
        // `engineer replied:` frame) — proving the operator reply is the SECOND
        // turn relaying the teammate, not the pre-delegation first reply.
        assert!(
            bubble.text.contains("engineer replied:")
                && bubble.text.contains("diagnose the outage"),
            "the relay carries the teammate's answer: {:?}",
            bubble.text
        );
        // …and it is the relay turn, whose prompt framed the hand-back.
        assert!(
            bubble.text.contains("Relay their answer"),
            "the operator bubble is the relay turn: {:?}",
            bubble.text
        );
    }

    /// (b) The relay turn cannot re-delegate: a delegation it queues is
    /// discarded, so no further desk turn or relay runs (cost stays bounded to
    /// one extra turn).
    #[tokio::test]
    async fn the_relay_turn_cannot_re_delegate() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 queues a delegation; invoke 3 (the relay) ALSO tries to queue
        // one — which must be discarded, so no fourth/fifth turn runs.
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![
                Some(Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "first".to_string(),
                }),
                None, // the desk lead's turn queues nothing
                Some(Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "second".to_string(),
                }),
            ],
        );

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "handle it".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        // Exactly three turns: orchestrator, desk lead, relay. The relay's queued
        // delegation was dropped — no fourth (desk-lead) or fifth (relay) turn.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the relay turn's delegation is discarded — one extra turn, no loop"
        );
        // The discard actually emptied the queue (not left dirty for next cycle).
        assert_eq!(
            brain.deps.delegations.queued(),
            0,
            "the relay turn's queued delegation was discarded"
        );
        // Still exactly one operator bubble.
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
    }

    /// (c) A normal, non-delegating message still produces exactly one turn — the
    /// relay path is entered only when a `delegate_to_desk` actually answered.
    #[tokio::test]
    async fn a_non_delegating_message_runs_exactly_one_turn() {
        let dir = tempfile::tempdir().unwrap();
        // No scripted delegations → the orchestrator answers directly.
        let (brain, provider) = brain_that_delegates(dir.path(), Vec::new());

        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "status?".into(),
                    by: None,
                    chat: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no delegation → a single orchestrator turn, no relay"
        );
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
    }

    // --- Issue #204: a dispatched turn that delegates -----------------------

    /// Seeds one dispatched card (blank assignee → the orchestrator runs it,
    /// which is the shape that carries the delegation tools) and dispatches it.
    async fn dispatch_card(brain: &HarnessBrain, tasks: &Arc<FsOps>, id: &str) {
        let mut c = card(id, "");
        c.column = "in_progress".to_string();
        tasks.upsert(&CompanyId::new("acme"), &c).await.unwrap();
        brain
            .run_cycle(
                request(vec![CompanyEvent::TaskDispatched {
                    task_id: id.to_string(),
                    run_id: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");
    }

    /// The bug: a dispatched task the CEO delegated went straight to
    /// `in_review` under the CEO with a blank assignee, and the delegate never
    /// ran — `run_task` ran one turn and never drained the delegation queue.
    ///
    /// Now the delegate actually runs, is linked as the card's assignee, and
    /// the card only reaches `in_review` on the back of THEIR output.
    #[tokio::test]
    async fn a_dispatched_turn_that_delegates_runs_the_delegate_and_links_them_to_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "fetch my activity".to_string(),
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-deleg").await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the dispatched turn, then the delegate's own turn — the delegate must actually run"
        );

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.assignee, "engineer",
            "the delegate must be linked as the assignee, not left blank under the delegator"
        );
        assert_eq!(
            after.column, "in_review",
            "the card reaches review on the delegate's output"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("delegated to engineer: fetch my activity"),
            "the hand-off is recorded in the delegator's voice: {note}"
        );
        // The delegate's own turn produced the result block: the mock echoes the
        // instruction it was handed back under its own attribution.
        let (_, delegate_block) = note
            .split_once("[engineer] did:")
            .unwrap_or_else(|| panic!("the delegate's output is the card's result: {note}"));
        assert!(
            delegate_block.contains("fetch my activity"),
            "the delegate ran the instruction it was handed: {note}"
        );

        // …and while the delegate was working, the card showed THEM working it:
        // its second turn ran against a card already reassigned and still in
        // progress, not one parked in a terminal column.
        assert_eq!(
            provider.board()[1],
            ("in_progress".to_string(), "engineer".to_string()),
            "the delegate must be shown working the card while they work it"
        );
    }

    /// An errored hand-off must not STRAND the card (issue #213 review).
    ///
    /// `hand_card_over` has already persisted the card as `in_progress`
    /// reassigned to the delegate before their turn starts. If that turn then
    /// errors and the failure is propagated out of `run_task`, the settle and
    /// the final `upsert` are both skipped and the card sits in `in_progress`
    /// under the delegate with no result — and nothing re-dispatches it, because
    /// `task_enters_in_progress` only edge-fires on the *transition* into that
    /// column and the card is already there. Exactly the state issue #204 exists
    /// to eliminate, reintroduced through the error path.
    ///
    /// So an errored hand-off takes the same arm an errored turn does: settle
    /// `Failed` → `todo`, with the reason on the note.
    #[tokio::test]
    async fn a_hand_off_whose_delegate_errors_lands_in_todo_not_stranded_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 is the dispatched orchestrator turn (queues the hand-off);
        // invoke 2 is the delegate's own turn, which errors.
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "fetch my activity".to_string(),
            }]],
            TurnFaults {
                fail_from: Some(2),
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-boom").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, COLUMN_TODO,
            "an errored hand-off must return the card to To-do, never leave it stranded in \
             progress where nothing will re-dispatch it"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("hand-off failed:"),
            "the failure reason lands on the note: {note}"
        );
        assert!(
            note.contains("delegated to engineer: fetch my activity"),
            "the hand-off that was attempted is still recorded: {note}"
        );
        // The failure is the assignee's, not the operator's — a cancellation is
        // the only ending attributed to `operator`.
        assert!(
            !note.contains("[operator]"),
            "an errored run is not an operator cancellation: {note}"
        );
    }

    /// A hand-off an operator cancels mid-flight produced nothing, so the card
    /// returns to `todo` reported as the cancellation it actually was.
    ///
    /// The claim is only made because `run_delegation` reports the cancellation
    /// as a fact (`DelegationOutcome::cancelled`); a hand-off that ends empty
    /// for any other reason no longer reaches this arm at all (issue #213
    /// review finding 2).
    #[tokio::test]
    async fn a_cancelled_hand_off_returns_the_card_to_todo_as_a_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 is the dispatched orchestrator turn (queues the hand-off);
        // invoke 2 is the delegate's turn, which cancels itself mid-run.
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![Delegation::DelegateToDesk {
                desk: "eng_desk".to_string(),
                instruction: "fetch my activity".to_string(),
            }]],
            TurnFaults {
                cancel_on: vec![2],
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-cancel").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, COLUMN_TODO,
            "a cancelled hand-off must not read as finished, and must not strand in progress"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("the delegated run was cancelled before it produced anything"),
            "the cancellation is reported as the cause: {note}"
        );
        // A cancellation is the operator's act, so the block is theirs — not the
        // delegate's, who never said it.
        assert!(
            note.contains("[operator] the delegated run was cancelled"),
            "a cancellation is attributed to the operator: {note}"
        );
    }

    /// A later hand-off that ANSWERS must not be discarded by an earlier one
    /// that produced nothing (issue #213 review finding 3).
    ///
    /// Before this, the first hand-off owned the card unconditionally. A first
    /// hand-off cancelled mid-flight still produced a `TaskHandoff`, so a second
    /// hand-off in the same drain took the "does not own the card" arm: its
    /// answer was appended to the note, but the card still settled `Cancelled`
    /// -> `todo` off the first. Work that ran and produced output ended up
    /// filed under a card marked cancelled.
    #[tokio::test]
    async fn a_later_answering_hand_off_takes_the_card_over_from_an_earlier_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        // Invoke 1 is the dispatched orchestrator turn, queuing BOTH hand-offs.
        // Invoke 2 is the first delegate's run, cancelled mid-flight; invoke 3
        // is the second delegate's run, which answers.
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![vec![
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "first attempt".to_string(),
                },
                Delegation::DelegateToDesk {
                    desk: "eng_desk".to_string(),
                    instruction: "second attempt".to_string(),
                },
            ]],
            TurnFaults {
                cancel_on: vec![2],
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-two-handoffs").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, "in_review",
            "the card settles from the hand-off that actually produced work, not from the \
             cancelled one that preceded it"
        );
        assert_eq!(
            after.assignee, "engineer",
            "the delegate that produced the work owns the card"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("second attempt"),
            "the answering hand-off's output is the card's result: {note}"
        );
        assert!(
            !note.contains("the delegated run was cancelled before it produced anything"),
            "an earlier cancelled hand-off must not settle a card a later one completed: {note}"
        );
    }

    /// The compatibility half: a dispatched turn that delegates nothing still
    /// runs exactly one turn and settles under the agent that ran it.
    ///
    /// (Deliberately no assertion on `assignee` — linking the *non-delegating*
    /// working agent to the card is issue #205's fix, and this test must not
    /// pin the blank it leaves behind today.)
    #[tokio::test]
    async fn a_dispatched_turn_that_delegates_nothing_settles_exactly_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(dir.path(), Vec::new());
        dispatch_card(&brain, &provider.tasks.clone(), "t-plain").await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no delegation → one turn, no hand-off"
        );
        let after = only_card(&provider.tasks).await;
        assert_eq!(after.column, "in_review");
        assert!(
            after.note.expect("note").contains("[chief]"),
            "the agent that ran it owns the result"
        );
    }

    /// A hand-off to a desk nobody leads has nowhere to go: rather than
    /// stranding the card in `in_progress` waiting on a delegate that will
    /// never run, the delegator's own reply settles it exactly as before.
    #[tokio::test]
    async fn a_hand_off_to_an_unknown_desk_settles_under_the_delegator() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "ghost".to_string(),
                instruction: "look into it".to_string(),
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-ghost").await;

        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an unknown desk has no lead to run a second turn"
        );
        let after = only_card(&provider.tasks).await;
        assert_eq!(
            after.column, "in_review",
            "the card must not strand in progress waiting on a delegate that cannot run"
        );
        assert_ne!(after.assignee, "ghost");
    }

    /// Issue #272: settling under the delegator is the right *behaviour* (#213
    /// chose it so a card is never stranded), but it used to be silent — the
    /// board showed a card whose note claimed a hand-off and whose owner was the
    /// delegator, with nothing connecting the two. The undeliverable hand-off is
    /// now recorded on the card, so an operator reads the fact instead of
    /// inferring it from an absence.
    #[tokio::test]
    async fn a_hand_off_that_cannot_be_delivered_says_so_on_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::DelegateToDesk {
                desk: "ghost".to_string(),
                instruction: "look into it".to_string(),
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-loud").await;

        let note = only_card(&provider.tasks).await.note.expect("note");
        assert!(
            note.contains("hand-off to the \"ghost\" desk was not delivered"),
            "the card must name the hand-off that did not happen: {note}"
        );
        assert!(
            note.contains("this card is still with chief"),
            "the card must say who still owns it: {note}"
        );
    }

    /// Issue #272, the grounded half: the tool refused the invented target, so
    /// no `Delegation` was ever queued. The turn is still free to *say* it
    /// handed the work off — that is exactly what happened on the live company
    /// — so the board records the refusal independently of the turn's account
    /// of it. Without this the card settles under the delegator with a note
    /// that claims a hand-off and nothing anywhere contradicting it.
    #[tokio::test]
    async fn a_refused_hand_off_is_recorded_on_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates_with(
            dir.path(),
            vec![Vec::new()],
            TurnFaults {
                refused_on_first: vec!["writer".to_string()],
                ..TurnFaults::default()
            },
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-refused").await;

        let after = only_card(&provider.tasks).await;
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a refused hand-off runs no delegate"
        );
        assert_eq!(
            after.column, "in_review",
            "the card still settles under the delegator (#213); it is only no longer silent"
        );
        let note = after.note.expect("note");
        assert!(
            note.contains("hand-off to the \"writer\" desk was not delivered"),
            "the refused target must be named on the card: {note}"
        );
        assert!(
            note.contains("not a desk this company can hand work to"),
            "the cause must be on the card: {note}"
        );
    }

    /// The other half of #272's note: a delegation that never had a desk target
    /// (a `spawn_task`) must not pick up an undeliverable-hand-off line.
    #[tokio::test]
    async fn a_spawn_task_never_records_an_undeliverable_hand_off() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::SpawnTask {
                title: "Follow up".to_string(),
                note: None,
                assignee: None,
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-quiet").await;

        let cards = provider.tasks.list(&CompanyId::new("acme")).await.unwrap();
        let parent = cards.iter().find(|c| c.id == "t-quiet").expect("parent");
        assert!(
            !parent
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("was not delivered"),
            "a spawn_task has no desk target to fail: {:?}",
            parent.note
        );
    }

    /// A `spawn_task` queued by a *dispatched* turn now opens its card with the
    /// dispatched card as its parent — the lineage `run_delegation` could never
    /// stamp while the task path did not drain the queue (issue #185's
    /// `parent_task_id`).
    #[tokio::test]
    async fn a_task_spawned_by_a_dispatched_turn_records_its_parent_card() {
        let dir = tempfile::tempdir().unwrap();
        let (brain, provider) = brain_that_delegates(
            dir.path(),
            vec![Some(Delegation::SpawnTask {
                title: "Follow up next week".to_string(),
                note: None,
                assignee: None,
            })],
        );
        dispatch_card(&brain, &provider.tasks.clone(), "t-parent").await;

        let cards = provider.tasks.list(&CompanyId::new("acme")).await.unwrap();
        let spawned = cards
            .iter()
            .find(|c| c.title == "Follow up next week")
            .expect("the spawned card must actually be opened");
        assert_eq!(spawned.column, COLUMN_TODO);
        assert_eq!(
            spawned.parent_task_id.as_deref(),
            Some("t-parent"),
            "a card spawned inside a dispatch remembers the card it came from"
        );
        // The parent still settles on its own turn's reply — spawning follow-up
        // work is not a hand-off.
        let parent = cards.iter().find(|c| c.id == "t-parent").expect("parent");
        assert_eq!(parent.column, "in_review");
    }
}
