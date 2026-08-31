//! Running a company's turn on an **ACP agent** instead of the embedded
//! OpenHuman harness.
//!
//! ## What this unlocks
//!
//! [`RunTurn`] is the seam between "the company cycle" and "an agent runs a
//! turn". It had exactly one implementation, `HarnessRunTurn`, which drives an
//! in-process OpenHuman agent and therefore needs an inference credential and
//! the whole vendored runtime. A second implementation over ACP serves three
//! things at once:
//!
//! - **A desktop company with no key.** The embedded host runs a turn on the
//!   operator's own `claude-code-acp`, against their existing subscription.
//!   Nothing to configure on first run, which is a materially different product
//!   from one that opens on a credential form.
//! - **Reverse dispatch.** A cloud host hands a task to a runner on someone's
//!   machine; the runner is an ACP agent as far as this is concerned.
//! - **Any other harness.** Codex, and anything else that speaks ACP.
//!
//! ## Why a port rather than an ACP client in here
//!
//! The transport differs per caller — a subprocess over stdio for the desktop,
//! a WebSocket for a runner — and neither belongs in the host crate. The port
//! itself ([`AcpAgent`], [`AcpAgentFactory`], `AcpTurn`, `AcpUpdate`) lives at
//! [`crate::ports::acp`], ungated, because the desktop shell that supplies the
//! stdio implementation deliberately does not enable the `openhuman` feature
//! this module lives behind — see that module's own docs for why. What
//! belongs here is [`AcpRunTurn`]: the adapter that folds whatever an
//! `AcpAgent` reports into this crate's own [`TurnStep`] shape, a genuine
//! `openhuman` dependency the port itself has none of.
//!
//! ## The mapping, and where it is lossy
//!
//! ACP's `session/update` variants and OpenCompany's [`TurnStep`] were designed
//! for different things, and the join is not total:
//!
//! | `sessionUpdate` | becomes |
//! |---|---|
//! | `agent_message_chunk` | appended to the reply |
//! | `agent_thought_chunk` | one coalesced `Thinking` step |
//! | `tool_call` | a `ToolCall` step, `Running` |
//! | `tool_call_update` | that step's status and result |
//! | `plan`, `available_commands_update`, … | dropped |
//!
//! Dropped rather than approximated: a `plan` is a task board, and inventing
//! `TurnStep`s for its entries would put rows on the operator's timeline that
//! no tool call produced.
//!
//! ## Execution state, before the result
//!
//! The same updates are published onto the transient
//! [`turn_stream`](crate::turn_stream) bus **as they arrive**, so the console
//! renders an ACP teammate's tool calls while the turn is still running —
//! exactly what the built-in harness's collector does with `AgentProgress`
//! (`built_in::steps::stream_event_from`), and what an ACP-run teammate had
//! none of: it sat silent for the whole turn and then produced a finished
//! timeline, which on a long coding turn is indistinguishable from a hang.
//!
//! The live frames and the folded [`TurnStep`]s are **the same events read
//! twice**, not two derivations that could drift: [`live_frame_from`] and
//! [`fold`] switch on the identical [`AcpUpdate`] stream, and the transport
//! still buffers everything it observes. A dropped live frame (a lagging
//! console) is therefore cosmetic — the authoritative timeline arrives folded
//! on the reply.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::harness::TurnOutcome;
pub use crate::ports::acp::{AcpAgent, AcpAgentFactory, AcpObserver, AcpTurn, AcpUpdate};
use crate::ports::types::{CompanyId, TurnStep, TurnStepKind, TurnStepStatus};
use crate::runtime::delegation::{ChatTarget, RunTurn};
use crate::turn_stream::{LiveRoute, TurnStreamCtx, TurnStreamEvent};
use serde_json::Value;

/// [`RunTurn`] over an [`AcpAgent`].
pub struct AcpRunTurn {
    agent: Arc<dyn AcpAgent>,
    /// One lock per session key, so a teammate runs one turn at a time *here*
    /// as well as in the transport.
    ///
    /// The transport holds its own lock over the same property, and both are
    /// wanted, because they protect different things. The transport's guards
    /// transport state — the update buffer and the observer registry — for any
    /// caller at all. This one exists because **cancellation only makes sense
    /// at this layer**: `session/cancel` names a session, not a turn, so a
    /// cancel forwarded by a turn that has not started yet lands on whichever
    /// turn currently owns the session and stops *that* one instead
    /// (PR #1904 review).
    ///
    /// Holding the slot here means a turn only ever forwards a cancel while
    /// its own prompt is the one in flight, and a turn cancelled while still
    /// queued simply never runs.
    turn_locks: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Per-turn state the live mapping carries between updates.
///
/// Mirrors the two locals the built-in harness's collector keeps
/// (`built_in/mod.rs`'s `seq` + `thinking_open`): a monotonic sequence the
/// console orders and dedups frames by, and whether a run of thoughts is
/// already open so a burst of `agent_thought_chunk`s coalesces into one row
/// rather than hundreds.
#[derive(Default)]
struct LiveState {
    seq: u64,
    thinking_open: bool,
    /// The tool calls this turn has published a `running` row for.
    ///
    /// [`fold`] drops an update for a call it never saw start ("a step with no
    /// label is worse on a timeline than no step"), and the live view has to
    /// drop the same one or the two disagree about how many rows the turn
    /// had — a row that appears live and is gone from the finished timeline
    /// reads as work that was undone.
    started: std::collections::HashSet<String>,
}

fn safe_result(result: Option<&str>) -> Option<String> {
    let text = result?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        match value {
            Value::Array(items) => return Some(count_of(items.len(), "item")),
            Value::Object(fields) if !fields.is_empty() => {
                return Some(count_of(fields.len(), "field"));
            }
            _ => {}
        }
    }
    Some(count_of(text.chars().count(), "character"))
}

fn count_of(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Map one [`AcpUpdate`] to the live frame the console renders, or `None` for
/// an update with no operator-facing row.
///
/// The live counterpart of [`fold`], and deliberately the same shape: a
/// `tool_call` opens a `running` row, a **terminal** `tool_call_update` flips
/// it in place by `toolCallId`, and the first thought of a run opens one
/// coalesced `Thinking` row that visible assistant text closes.
///
/// A non-terminal `tool_call_update` (`pending` / `in_progress`) emits
/// nothing: the row it would carry is already on screen as `running` from its
/// `tool_call`, and re-publishing it as a second `tool_call` frame would
/// either duplicate the row or overwrite the label the console is showing.
/// [`fold`] treats those statuses the same way — it leaves the step
/// `Running` — so the two views still agree.
///
/// Assistant text adds no row here for the same reason it adds no step in
/// [`fold`]: the reply *is* the bubble body. Nothing on this bus carries the
/// text itself.
fn live_frame_from(update: &AcpUpdate, state: &mut LiveState) -> Option<TurnStreamEvent> {
    let seq = state.seq;
    match update {
        AcpUpdate::ToolCall { id, title } => {
            state.thinking_open = false;
            state.started.insert(id.clone());
            Some(TurnStreamEvent {
                kind: "tool_call",
                seq,
                tool_call_id: Some(id.clone()),
                label: Some(title.clone()),
                status: Some(TurnStepStatus::Running.wire_word()),
                ..TurnStreamEvent::default()
            })
        }
        AcpUpdate::ToolCallUpdate { id, status, result } => {
            // Closed first, and unconditionally: `fold` clears its own
            // thinking run on *any* tool-call update, including the ones it
            // then drops, so clearing it only on the updates that publish a
            // row would leave the live view opening a second `Thinking` row
            // where the folded timeline opens none.
            state.thinking_open = false;
            let status = match status.as_str() {
                "completed" => TurnStepStatus::Ok,
                "failed" => TurnStepStatus::Error,
                // Not done yet — see the doc comment.
                _ => return None,
            };
            if !state.started.contains(id) {
                // An update for a call this turn never saw start, exactly as
                // `fold` treats one. See `LiveState::started`.
                return None;
            }
            Some(TurnStreamEvent {
                kind: "tool_result",
                seq,
                tool_call_id: Some(id.clone()),
                result: safe_result(result.as_deref()),
                status: Some(status.wire_word()),
                ..TurnStreamEvent::default()
            })
        }
        AcpUpdate::ThoughtChunk if !state.thinking_open => {
            state.thinking_open = true;
            Some(TurnStreamEvent {
                kind: "thinking",
                seq,
                label: Some("Thinking".to_string()),
                status: Some(TurnStepStatus::Ok.wire_word()),
                ..TurnStreamEvent::default()
            })
        }
        AcpUpdate::ThoughtChunk => None,
        AcpUpdate::MessageChunk(_) => {
            state.thinking_open = false;
            None
        }
    }
}

/// The [`AcpObserver`] that publishes a turn's updates onto the live bus.
///
/// `None` when the turn has no console surface to stream to — a dispatched
/// card's background turn, whose steps are folded into its note and must not
/// reach the chat timeline (the same reason `built_in` has
/// `LiveStream::Off`). Building no observer at all, rather than one that
/// publishes to nowhere, is what keeps the transport from doing per-update
/// work for a turn nobody is watching.
fn observer_for(ctx: Option<TurnStreamCtx>) -> Option<AcpObserver> {
    let ctx = ctx?;
    let state = std::sync::Mutex::new(LiveState::default());
    Some(Arc::new(move |update: &AcpUpdate| {
        // The transport calls this from its wire-reading task, so the lock is
        // held only for the mapping itself and never across an await.
        let mut state = match state.lock() {
            Ok(state) => state,
            // A poisoned lock means a previous call panicked mid-mapping.
            // Losing live frames is cosmetic (the fold still carries the
            // authoritative timeline), so this drops the frame rather than
            // propagating a panic into the transport's read loop and killing
            // the turn.
            Err(_) => return,
        };
        let Some(frame) = live_frame_from(update, &mut state) else {
            return;
        };
        state.seq += 1;
        drop(state);

        let frame = frame.with_agent(ctx.agent_id.clone());
        let frame = match &ctx.route {
            LiveRoute::Chat { chat_id } => frame.with_chat(chat_id.clone()),
            LiveRoute::Workflow { run_id, node_id } => {
                frame.with_workflow(run_id.clone(), node_id.clone())
            }
        };
        crate::turn_stream::publish(&ctx.company, frame);
    }))
}

impl AcpRunTurn {
    pub fn new(agent: Arc<dyn AcpAgent>) -> Self {
        Self {
            agent,
            turn_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// This session key's turn slot, created on first use.
    fn turn_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.turn_locks.lock().expect("acp turn locks");
        Arc::clone(
            locks
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// The session an agent's turns share.
    ///
    /// Per (company, agent) so two desks do not share a conversation, and
    /// stable across turns so the second question in a thread does not arrive
    /// with no memory of the first.
    fn session_key(company: &CompanyId, agent_id: &str) -> String {
        format!("{}::{agent_id}", company.as_ref())
    }

    /// The live-stream context for a **chat** turn.
    ///
    /// Falls back to the default desk when the caller addressed none, so the
    /// live rows land on the same thread the durable reply does — byte for
    /// byte the rule `built_in`'s `LiveStream::On` applies, because a frame
    /// routed to a different thread than its reply is worse than no frame.
    fn chat_ctx(company: &CompanyId, agent_id: &str, chat_id: Option<&str>) -> TurnStreamCtx {
        TurnStreamCtx {
            company: company.clone(),
            agent_id: agent_id.to_string(),
            route: LiveRoute::Chat {
                chat_id: chat_id
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::server::ops::language::DEFAULT_DESK.to_string()),
            },
        }
    }

    async fn run_once(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        stream: Option<TurnStreamCtx>,
    ) -> Result<TurnOutcome> {
        let key = Self::session_key(company, agent_id);
        let slot = self.turn_lock(&key);
        // An unsteerable turn simply waits its turn; there is no cancel to
        // race, so nothing more is needed here.
        let _slot = slot.lock().await;
        let observer = observer_for(stream);
        let turn = self
            .agent
            .prompt(company, &key, message, observer.as_ref())
            .await?;
        Ok(fold(turn))
    }
}

/// How a turn ended, coarsened from ACP's raw `stopReason` string into the
/// shapes this fold treats differently.
///
/// `EndTurn` is the only one that means "the agent said everything it meant
/// to say"; every other value means the reply in hand — if any — is partial,
/// and the fold must say so rather than let it pass for an ordinary answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKind {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other,
}

/// Classifies ACP's raw `stopReason` into [`StopKind`].
///
/// `max_tokens` and `max_turn_requests` stay distinct variants (PR #1880
/// review) even though the note either produces reads similarly: only
/// `max_turn_requests` is this protocol's analog of openhuman's
/// tool-iteration cap — the number of agent/tool round trips in the turn hit
/// a limit — and only that one may set
/// [`TurnOutcome::hit_iteration_cap`](crate::harness::TurnOutcome::hit_iteration_cap).
/// `max_tokens` is a token-generation budget on a single response, unrelated
/// to how many tool calls ran; folding it into the same flag would make a
/// workflow node's `LimitStop { limit: "max_tool_iterations" }` misreport
/// which cap actually stopped the turn.
fn classify_stop_reason(raw: &str) -> StopKind {
    match raw {
        "end_turn" => StopKind::EndTurn,
        "max_tokens" => StopKind::MaxTokens,
        "max_turn_requests" => StopKind::MaxTurnRequests,
        "refusal" => StopKind::Refusal,
        "cancelled" => StopKind::Cancelled,
        _ => StopKind::Other,
    }
}

/// The short, fixed note surfaced when a turn stopped for a reason other than
/// `end_turn`. Landed as its own [`TurnStep`] of kind
/// [`TurnStepKind::Note`], never concatenated into
/// [`TurnOutcome::reply`](crate::harness::TurnOutcome::reply) (PR #1880
/// review) — the reply is the agent's own words, and folding a
/// platform-generated notice into it would leave the operator unable to tell
/// how much of the text the agent actually said. `EndTurn` returns `None`;
/// callers only invoke this for a non-`EndTurn` [`StopKind`].
///
/// Every arm is a **fixed** string — none of them interpolate
/// `raw_stop_reason`, even the `Other` arm, which used to (PR #1880 review).
/// A `stopReason` this fold does not recognise is unvalidated, unbounded text
/// straight off the wire from an external ACP agent — the same class of risk
/// the module doc already calls out for a tool call's `title` — and this
/// `Note` step is not a private log line: it becomes an engine transcript
/// entry (`workflows/caps::transcript_from_steps` maps `Note` to
/// `"agent_message"`), which can be replayed as prior context for later
/// engine reasoning. Interpolating the raw value there would hand an
/// external agent a channel to inject diagnostic text, newlines, or an
/// oversized payload into durable, operator- and agent-visible history. The
/// raw value is still worth knowing for debugging — see `fold`'s bounded
/// `tracing::warn!` right before this is called for `Other`.
fn stop_reason_note(kind: StopKind) -> Option<String> {
    match kind {
        StopKind::EndTurn => None,
        StopKind::MaxTokens => Some("[stopped: hit the token limit before finishing]".to_string()),
        StopKind::MaxTurnRequests => {
            Some("[stopped: hit the tool-call limit before finishing]".to_string())
        }
        StopKind::Refusal => Some("[stopped: the agent declined to continue]".to_string()),
        StopKind::Cancelled => Some("[stopped: cancelled before finishing]".to_string()),
        StopKind::Other => Some("[stopped: unrecognized stop reason]".to_string()),
    }
}

/// Bound on the raw `stopReason` logged for `StopKind::Other` (PR #1880
/// review). A log line is a reasonable place for the diagnostic value —
/// unlike a `TurnStep` or an engine error message, it is not replayed as
/// context and not returned to any client — but it is still unvalidated wire
/// text, so it gets the same UTF-8-safe char-count bound the rest of the crate
/// applies before logging or persisting external content, sized for "enough
/// to recognise the reason, not enough to flood the log".
const UNKNOWN_STOP_REASON_LOG_CHARS: usize = 120;

/// Builds the reply for a turn that produced no `MessageChunk` text.
///
/// Never returns an empty string: a blank reply from a tool-only turn, or one
/// cut short before the agent said anything, would read on the operator's
/// timeline as "the agent had nothing to say" rather than what actually
/// happened. Says only **that** tools ran, never **what** they were (PR #1880
/// review) — a tool call's `title` comes verbatim off the wire from the
/// external ACP agent, with no host-side bounding or redaction (unlike the
/// built-in harness's server-computed step label), so it can carry arbitrary
/// upstream content. The titles themselves are already on the operator's
/// timeline as this turn's [`TurnStep`]s; restating them in a field meant to
/// read as the agent's own words would only duplicate that exposure for no
/// new information.
fn synthesize_empty_reply(steps: &[TurnStep]) -> &'static str {
    let ran_tools = steps.iter().any(|step| step.kind == TurnStepKind::ToolCall);
    if ran_tools {
        "[no reply text — see steps]"
    } else {
        // A clean end with no text and no tool calls. Still never blank.
        "[no reply]"
    }
}

/// Folds a turn's updates into the outcome the company cycle expects.
///
/// Separate from the trait impl so it is testable without an agent, and because
/// this — not the plumbing — is where the semantics live.
pub fn fold(turn: AcpTurn) -> TurnOutcome {
    let mut reply = String::new();
    let mut steps: Vec<TurnStep> = Vec::new();
    // Where each tool call's step landed, so a later update finds it. A tool
    // call that never completes keeps the `Running` status it was created with,
    // which is exactly what that status means.
    let mut positions: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut thinking = false;

    for update in turn.updates {
        match update {
            AcpUpdate::MessageChunk(text) => {
                // Visible assistant text closes an open thinking run, so a
                // turn that thinks, says something, then thinks again shows
                // two `Thinking` rows rather than one that spans the answer.
                //
                // This arm used to fall straight through to `reply`, which
                // made this fold the odd one out twice over (PR #1904
                // review): the built-in harness's own `fold_steps` closes a
                // thinking run on `TextDelta` for exactly this reason, and
                // the live mapper beside this one closes it on
                // `MessageChunk`. Leaving it open here meant the live
                // timeline could show a second `Thinking` row that vanished
                // when the reply landed and replaced it — the operator
                // watching work disappear.
                thinking = false;
                reply.push_str(&text);
            }
            AcpUpdate::ThoughtChunk => {
                // One step for a run of thoughts, not one per chunk: a model
                // emits these by the hundred, and a timeline of them is noise.
                if !thinking {
                    thinking = true;
                    steps.push(TurnStep {
                        kind: TurnStepKind::Thinking,
                        status: TurnStepStatus::Ok,
                        label: "Thinking".to_string(),
                        ..TurnStep::default()
                    });
                }
            }
            AcpUpdate::ToolCall { id, title } => {
                thinking = false;
                positions.insert(id, steps.len());
                steps.push(TurnStep {
                    kind: TurnStepKind::ToolCall,
                    status: TurnStepStatus::Running,
                    label: title,
                    ..TurnStep::default()
                });
            }
            AcpUpdate::ToolCallUpdate { id, status, result } => {
                thinking = false;
                let Some(&index) = positions.get(&id) else {
                    // An update for a call we never saw start. Dropped rather
                    // than synthesised: a step with no label is worse on a
                    // timeline than no step.
                    continue;
                };
                let step = &mut steps[index];
                step.status = match status.as_str() {
                    "completed" => TurnStepStatus::Ok,
                    "failed" => TurnStepStatus::Error,
                    // `pending` and `in_progress` both mean "not done".
                    _ => TurnStepStatus::Running,
                };
                if result.is_some() {
                    step.result = safe_result(result.as_deref());
                }
            }
        }
    }

    // Issue #1853: `stop_reason` is ACP's own signal for how the turn ended,
    // and the old fold never read it — a tool-only turn folded to `reply ==
    // ""`, and a max_tokens/refusal/cancelled turn folded identically to a
    // clean `end_turn`, indistinguishable to the operator from an ordinary
    // answer.
    let kind = classify_stop_reason(&turn.stop_reason);

    if kind == StopKind::Other {
        // Diagnostic only (PR #1880 review) — never the source for anything
        // durable. `stop_reason_note`'s `Other` arm and `abnormal_stop` below
        // both deliberately drop the raw value; this bounded copy is the only
        // place it survives, and only in a log line, char-capped so a
        // malformed/oversized `stopReason` cannot flood it either.
        let bounded: String = turn
            .stop_reason
            .chars()
            .take(UNKNOWN_STOP_REASON_LOG_CHARS)
            .collect();
        tracing::warn!(
            stop_reason = %bounded,
            "[harness::acp] unrecognized ACP stop reason"
        );
    }

    if reply.trim().is_empty() {
        reply = synthesize_empty_reply(&steps).to_string();
    }

    // PR #1880 review: the stop-reason notice is platform-generated, not
    // agent-authored, so it lands as its own step rather than blurring into
    // `reply` above.
    let note = stop_reason_note(kind);
    if let Some(note) = &note {
        steps.push(TurnStep {
            kind: TurnStepKind::Note,
            status: TurnStepStatus::Ok,
            label: note.clone(),
            ..TurnStep::default()
        });
    }

    TurnOutcome {
        reply,
        steps,
        // A max_turn_requests stop is exactly the shape issue #926 describes:
        // the tool loop was cut off by a budget rather than the model
        // choosing to stop. `max_tokens` is a different budget — a single
        // response's token limit — and is deliberately excluded (PR #1880
        // review): downstream (`workflows/caps`) reports this flag as
        // "stopped at the max_tool_iterations cap", which would misdescribe a
        // token-limited stop. Every other `StopKind` is not a cap —
        // `Refusal`/`Cancelled`/`Other` are surfaced as a step note instead,
        // and `EndTurn` needs no flag at all.
        hit_iteration_cap: matches!(kind, StopKind::MaxTurnRequests),
        // PR #1880 review: `Refusal`/`Cancelled`/`Other` are not a resumable
        // cap either — there is no checkpoint to continue from, unlike
        // `hit_iteration_cap` above — so `HarnessAgentRunner` must not settle
        // these as a plain `Succeeded`/`StopReason::Finished` the way it used
        // to when `hit_iteration_cap == false` was the only signal it read.
        // Reuses `note`'s text: both sinks want the same short, fixed,
        // non-wire-derived notice, and `stop_reason_note`'s `Other` arm is
        // already the one place that keeps the raw `stopReason` out of it.
        abnormal_stop: matches!(
            kind,
            StopKind::Refusal | StopKind::Cancelled | StopKind::Other
        )
        .then(|| note.clone().unwrap_or_default()),
        // Issue #1032: nor is there a spend halt to report. The stop hooks are
        // installed around THIS crate's `agent.turn`, and an ACP turn does not
        // run through it — the external process bills and stops on its own
        // terms, which this side neither arms nor observes.
        halted_for_spend: None,
        // Issue #1846: same reasoning — `classify_turn` never runs for an ACP
        // turn, so there is no budget-exhausted wire shape to classify here.
        // The external process's own budget handling (if any) is opaque to
        // this side.
        budget_paused: None,
    }
}

#[async_trait]
impl RunTurn for AcpRunTurn {
    async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        chat: ChatTarget<'_>,
    ) -> Result<TurnOutcome> {
        // `chat_id` only: the live bus routes a chat turn's frames by channel
        // (`LiveRoute::Chat`), and #1890's `thread_root` narrows *which
        // conversation inside it* the durable reply hangs from — a dimension
        // the transient timeline does not carry.
        let ctx = Self::chat_ctx(company, agent_id, chat.chat_id);
        self.run_once(company, agent_id, message, Some(ctx)).await
    }

    async fn run_steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &crate::company::steer::SteerControl,
        chat: ChatTarget<'_>,
        _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        let ctx = Self::chat_ctx(company, agent_id, chat.chat_id);
        self.steered(company, agent_id, message, control, Some(ctx))
            .await
    }

    async fn run_steered_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &crate::company::steer::SteerControl,
        // Still dropped, and this is the one place worth saying why: the sink
        // takes `oh::AgentProgress` (`RunTraceSink::record`), which is what
        // the built-in collector has and an ACP fold does not. So a dispatched
        // card run by an ACP teammate persists no step trace under its attempt
        // row — its timeline lives only in the card's note. Closing that needs
        // a `TurnStep`-shaped entry point on the sink, which means owning step
        // ordinals and the running→finalized rewrite from a second producer.
        _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        // No live context on purpose: a dispatched card's turn shows no chat
        // bubble, and its rows must not appear on whatever thread most
        // recently sent. Same rule as `built_in`'s `LiveStream::Off`.
        self.steered(company, agent_id, message, control, None)
            .await
    }

    /// Overridden to suppress the live stream.
    ///
    /// The trait default forwards to [`run`](RunTurn::run) with no chat id,
    /// which — now that `run` streams — would publish a workflow node's tool
    /// calls onto the **default desk's** chat timeline, attributing them to a
    /// thread the node has nothing to do with. That is the misattribution
    /// `built_in` grew `run_background` to avoid, and inheriting the default
    /// here would reintroduce it for ACP alone.
    async fn run_background(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        self.run_once(company, agent_id, message, None).await
    }

    /// A workflow agent node, streaming onto the run-trace sheet.
    ///
    /// Routed by the workflow run + node rather than a chat thread (issue
    /// #1702's dimension), so an ACP-run node's tool calls appear live on the
    /// sheet the same way a `built_in`-run node's do.
    async fn run_background_workflow(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
        workflow_run_id: &str,
        node_id: &str,
    ) -> Result<TurnOutcome> {
        let ctx = TurnStreamCtx {
            company: company.clone(),
            agent_id: agent_id.to_string(),
            route: LiveRoute::Workflow {
                run_id: workflow_run_id.to_string(),
                node_id: node_id.to_string(),
            },
        };
        self.run_once(company, agent_id, message, Some(ctx)).await
    }
}

/// The two windows a cancelled turn is bounded by.
///
/// One value rather than two parameters because they are only ever chosen
/// together, and because the test-visible entry point that takes them was one
/// argument over `clippy::too_many_arguments` once the live-stream context
/// joined it — a signature that long is worth grouping rather than silencing.
#[derive(Clone, Copy)]
struct CancelBounds {
    /// How long a cancelled turn may keep running before the waiter gives up.
    ///
    /// Cancellation in ACP is cooperative: `session/cancel` is a notification,
    /// and a harness inside a long tool call only notices when that call
    /// returns. So the post-cancel wait stays, but it is bounded — a cancelled
    /// turn that has not drained its output within this window is abandoned,
    /// not waited on forever. The window is generous enough for a slow tool
    /// call to finish and its updates to flush.
    grace: Duration,
    /// Bound on a single `session/cancel` round trip. A cancel that never
    /// answers — a wedged host, a dead subprocess — must not pin the steered
    /// turn forever; the grace wait is what actually reaps a turn that ignores
    /// the cancel, and this bound just keeps the attempt to tell it from
    /// blocking that.
    rpc: Duration,
}

impl CancelBounds {
    /// What a real turn runs under. The tests substitute milliseconds.
    const DEFAULT: Self = Self {
        grace: Duration::from_secs(30),
        rpc: Duration::from_secs(5),
    };
}

impl AcpRunTurn {
    /// A turn that can be cancelled while it runs.
    ///
    /// The turn and the steer check race each other. A cancel forwards
    /// `session/cancel` and then **keeps waiting** rather than abandoning the
    /// turn: ACP cancellation is cooperative, the agent still answers with
    /// `stopReason: "cancelled"`, and dropping the future here would leave a
    /// harness mid-tool-call with nothing reading its output. That wait is
    /// bounded by [`CancelBounds::grace`]: a turn that ignores the cancel past
    /// the grace window is abandoned with an error, not awaited forever.
    async fn steered(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &crate::company::steer::SteerControl,
        stream: Option<TurnStreamCtx>,
    ) -> Result<TurnOutcome> {
        self.steered_with_grace(
            company,
            agent_id,
            message,
            control,
            stream,
            CancelBounds::DEFAULT,
        )
        .await
    }

    /// [`Self::steered`] with both timing bounds made explicit — the post-cancel
    /// grace and the per-cancel-RPC bound — so the tests can expire them in
    /// milliseconds rather than waiting out the real windows.
    async fn steered_with_grace(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &crate::company::steer::SteerControl,
        stream: Option<TurnStreamCtx>,
        bounds: CancelBounds,
    ) -> Result<TurnOutcome> {
        let CancelBounds {
            grace,
            rpc: cancel_rpc,
        } = bounds;
        let key = Self::session_key(company, agent_id);

        // Wait for this teammate's turn slot **steerably**. A turn cancelled
        // while it is still queued must never reach the adapter: its cancel
        // would name the session, which at that moment belongs to whichever
        // turn is actually running, and would stop that one instead. So a
        // cancel here ends this turn where it stands — nothing was started, so
        // there is nothing to stop.
        //
        // `SteerControl` is poll-shaped rather than awaitable, so this polls
        // on the same cadence the running turn does below.
        // The refusal below applies **only to a turn that had to queue**, and
        // the distinction is the whole point rather than an optimisation.
        //
        // The hazard is a cancel forwarded by a turn that has not started: it
        // names the session, so it stops whichever turn currently owns it. That
        // can only happen when another turn owns the slot. On a free slot there
        // is no other turn, and a pending cancel keeps its long-standing
        // meaning — start, forward the cancel, let the agent wind down and
        // report (`stopReason: "cancelled"`), which is an `Ok` outcome the
        // caller settles as cancelled rather than failed.
        let slot = self.turn_lock(&key);
        let _slot = match Arc::clone(&slot).try_lock_owned() {
            Ok(slot) => slot,
            Err(_) => {
                // Contended: somebody else is mid-turn on this session.
                let queued = loop {
                    tokio::select! {
                        slot = Arc::clone(&slot).lock_owned() => break slot,
                        () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                            if control.pending().is_some() {
                                return Err(OpenCompanyError::InvalidRequest(
                                    "the turn was cancelled before it started".to_string(),
                                ));
                            }
                        }
                    }
                };
                // Asked once more on acquiring, because the poll above can lose
                // the race it exists to win: a cancel arriving on a slot that
                // frees before the next tick takes the lock branch and never
                // looks at the control (PR #1904 review). Everything past this
                // point talks to the agent.
                if control.pending().is_some() {
                    return Err(OpenCompanyError::InvalidRequest(
                        "the turn was cancelled before it started".to_string(),
                    ));
                }
                queued
            }
        };

        // Held for the whole select below: the observer must outlive every
        // branch, including the post-cancel grace wait, or a turn that keeps
        // producing updates after a cancel would stop streaming them at the
        // moment the operator most wants to see what it is still doing.
        let observer = observer_for(stream);
        let turn = self.agent.prompt(company, &key, message, observer.as_ref());
        tokio::pin!(turn);

        loop {
            tokio::select! {
                outcome = &mut turn => return Ok(fold(outcome?)),
                () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                    // `pending`, not `take`: the disposition site after the turn
                    // reads the action to decide what happens to the card, and
                    // consuming it here would leave it with nothing to read.
                    if control.pending().is_some() {
                        // Advisory. Told, then waited for — see above. The RPC
                        // itself is bounded so a cancel that never answers (a
                        // wedged host, a dead subprocess) cannot block the turn;
                        // both outcomes below are logged and the flow continues.
                        match tokio::time::timeout(cancel_rpc, self.agent.cancel(company, &key))
                            .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                tracing::warn!(%err, "[harness::acp] cancel failed for session {key}");
                            }
                            Err(_elapsed) => {
                                tracing::warn!("[harness::acp] cancel timed out for session {key}");
                            }
                        }
                        match tokio::time::timeout(grace, &mut turn).await {
                            Ok(outcome) => return Ok(fold(outcome?)),
                            Err(_elapsed) => {
                                // The agent ignored the cancel past the grace
                                // window. The port has no abort/reset seam —
                                // `cancel` is all there is — so the best this
                                // side can do is nudge once more and drop the
                                // turn. Dropping the future ends the reader on
                                // this session; the agent's own `session/cancel`
                                // handling (or the host reaping the subprocess)
                                // is the recovery path for the work it still
                                // holds. A later turn on the same key opens a
                                // fresh `session/prompt`, which the agent treats
                                // as a new turn rather than an overlap. The
                                // nudge is bounded the same way: it is best
                                // effort, and the abandonment is the point.
                                let _ = tokio::time::timeout(
                                    cancel_rpc,
                                    self.agent.cancel(company, &key),
                                )
                                .await;
                                return Err(OpenCompanyError::Harness(format!(
                                    "the agent did not stop within {}s of a cancel; \
                                     abandoning the turn",
                                    grace.as_secs()
                                )));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn turn(updates: Vec<AcpUpdate>) -> AcpTurn {
        AcpTurn {
            updates,
            stop_reason: "end_turn".to_string(),
        }
    }

    fn turn_with_stop_reason(updates: Vec<AcpUpdate>, stop_reason: &str) -> AcpTurn {
        AcpTurn {
            updates,
            stop_reason: stop_reason.to_string(),
        }
    }

    #[test]
    fn a_max_turn_requests_stop_is_the_tool_step_cap() {
        // Issue #1853 established that a stop must not fold identically to a
        // clean end_turn — the operator needs a cap signal. PR #1880 review:
        // `max_turn_requests` is ACP's analog of openhuman's tool-iteration
        // cap, and is the only stop reason that may set `hit_iteration_cap`,
        // because `workflows/caps` reports that flag as "stopped at the
        // max_tool_iterations cap".
        let outcome = fold(turn_with_stop_reason(vec![], "max_turn_requests"));
        assert!(
            outcome.hit_iteration_cap,
            "max_turn_requests is the tool-step cap, not a clean finish"
        );
        assert!(
            !outcome.reply.trim().is_empty(),
            "a capped turn must say so, not fold to a blank reply"
        );
    }

    #[test]
    fn a_max_tokens_stop_is_not_the_tool_step_cap() {
        // A token-generation budget on a single response is a different cap
        // than the tool-iteration one (PR #1880 review) — conflating them
        // would make a workflow node's `LimitStop{"max_tool_iterations"}`
        // misreport which cap actually stopped the turn.
        let outcome = fold(turn_with_stop_reason(vec![], "max_tokens"));
        assert!(
            !outcome.hit_iteration_cap,
            "a max_tokens stop is not the tool-iteration cap"
        );
        assert!(
            !outcome.reply.trim().is_empty(),
            "a capped turn must say so, not fold to a blank reply"
        );
    }

    #[test]
    fn acp_results_are_reduced_to_shape_not_remote_text() {
        let secret = "API key: do-not-publish";
        let outcome = fold(turn(vec![
            AcpUpdate::ToolCall {
                id: "t".into(),
                title: "Read".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "t".into(),
                status: "completed".into(),
                result: Some(secret.into()),
            },
        ]));
        assert_eq!(outcome.steps[0].result.as_deref(), Some("23 characters"));
        assert!(!outcome.steps[0].result.as_deref().unwrap().contains(secret));
    }
    #[test]
    fn a_tool_only_turn_gets_a_generic_reply_not_raw_tool_titles() {
        // No MessageChunk at all — the agent's entire turn was tool calls.
        // PR #1880 review: the reply must not copy the tools' raw ACP titles
        // — unlike the built-in harness's step label, a title comes straight
        // off the wire with no host-side bounding, and the timeline (already
        // carrying each ToolCall step's own title) is where that content
        // belongs, not a field meant to read as the agent's own words.
        let outcome = fold(turn(vec![
            AcpUpdate::ToolCall {
                id: "t1".into(),
                title: "Read".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "t1".into(),
                status: "completed".into(),
                result: Some("2.4 kB".into()),
            },
            AcpUpdate::ToolCall {
                id: "t2".into(),
                title: "Write".into(),
            },
        ]));
        assert_eq!(outcome.reply, "[no reply text — see steps]");
        assert_eq!(outcome.steps[0].label, "Read");
        assert_eq!(outcome.steps[1].label, "Write");
        // A clean end_turn needs no stop-reason note on top of the synthesis.
        assert!(!outcome.reply.contains("[stopped"));
    }

    #[test]
    fn a_refusal_is_surfaced_as_a_note_step_and_the_cap_stays_false() {
        // The agent had prose to say, then declined to continue. The note
        // must land regardless — a refusal is not a clean finish even when
        // there is a reply to read. PR #1880 review: it lands as a `Note`
        // step, not appended onto the agent's own reply text.
        let outcome = fold(turn_with_stop_reason(
            vec![AcpUpdate::MessageChunk("I can't help with that.".into())],
            "refusal",
        ));
        assert_eq!(
            outcome.reply, "I can't help with that.",
            "the agent's own prose is kept verbatim, with nothing appended"
        );
        assert!(
            outcome.steps.iter().any(|s| s.kind == TurnStepKind::Note
                && s.label == "[stopped: the agent declined to continue]"),
            "the refusal must be surfaced as a step, not silently swallowed: {:?}",
            outcome.steps
        );
        assert!(
            !outcome.hit_iteration_cap,
            "a refusal is not an iteration-cap pause"
        );
        // PR #1880 review: `hit_iteration_cap == false` used to be the only
        // signal `HarnessAgentRunner` read, so a refusal settled a workflow
        // node `Succeeded`/`Finished` — indistinguishable from the agent
        // having actually answered. This is the outcome-level fix, not just
        // the note above: see `workflows::caps::mod::test::an_abnormal_acp_stop_fails_the_workflow_node`
        // for the assertion that it actually stops the graph.
        assert_eq!(
            outcome.abnormal_stop.as_deref(),
            Some("[stopped: the agent declined to continue]"),
            "a refusal must carry a distinct abnormal-stop outcome, not just a note"
        );
    }

    #[test]
    fn a_cancelled_turn_also_carries_an_abnormal_stop() {
        // Same shape as refusal, different trigger: an operator-initiated (or
        // upstream) cancel is just as much "not a resumable cap, not a clean
        // finish" as a refusal is.
        let outcome = fold(turn_with_stop_reason(vec![], "cancelled"));
        assert_eq!(
            outcome.abnormal_stop.as_deref(),
            Some("[stopped: cancelled before finishing]")
        );
        assert!(!outcome.hit_iteration_cap);
    }

    #[test]
    fn an_end_turn_reply_is_left_verbatim() {
        // The ordinary case — and the one the pre-existing seam test already
        // pins — must not gain a note or any other alteration just because
        // this fold now reads `stop_reason`.
        let outcome = fold(turn(vec![AcpUpdate::MessageChunk("all done".into())]));
        assert_eq!(outcome.reply, "all done");
        assert!(!outcome.hit_iteration_cap);
        assert_eq!(
            outcome.abnormal_stop, None,
            "a clean end_turn is not an abnormal stop"
        );
    }

    #[test]
    fn a_max_turn_requests_stop_is_a_cap_not_an_abnormal_stop() {
        // The cap path (issue #926 / #1880's `hit_iteration_cap` split) and
        // the abnormal-stop path (this PR's review) are deliberately
        // disjoint: a capped turn has a real, resumable checkpoint, which is
        // exactly what `abnormal_stop` says there is none of.
        let outcome = fold(turn_with_stop_reason(vec![], "max_turn_requests"));
        assert!(outcome.hit_iteration_cap);
        assert_eq!(
            outcome.abnormal_stop, None,
            "the cap flag already covers this stop; abnormal_stop must stay None"
        );
    }

    #[test]
    fn an_unrecognized_stop_reason_is_surfaced_not_swallowed() {
        // A stop_reason this fold has never heard of must not silently pass
        // for a clean end_turn — it is carried into a note step so the
        // operator (and whoever reads the ticket) can see the turn stopped
        // abnormally.
        //
        // PR #1880 review: the raw string itself must NOT appear — an
        // unrecognized `stopReason` is unvalidated, unbounded text straight
        // off the wire from an external ACP agent, and this note step is not
        // a private log line: `workflows/caps::transcript_from_steps` maps a
        // `Note` step to `"agent_message"` in the engine transcript, which
        // can be replayed as prior context for later engine reasoning. The
        // fixed notice below carries the abnormal-stop signal without
        // reopening that channel.
        let raw = "some_new_reason_acp_added_later__with_diagnostic_junk_🔥";
        let outcome = fold(turn_with_stop_reason(
            vec![AcpUpdate::MessageChunk("partial thought".into())],
            raw,
        ));
        assert_eq!(outcome.reply, "partial thought");
        assert!(
            outcome.steps.iter().any(|s| s.kind == TurnStepKind::Note
                && s.label == "[stopped: unrecognized stop reason]"),
            "an unrecognized stop must still be surfaced as a step: {:?}",
            outcome.steps
        );
        assert!(
            outcome.steps.iter().all(|s| !s.label.contains(raw)),
            "the raw wire value must never appear in a persisted step: {:?}",
            outcome.steps
        );
        assert!(!outcome.hit_iteration_cap);
        assert_eq!(
            outcome.abnormal_stop.as_deref(),
            Some("[stopped: unrecognized stop reason]"),
            "an unrecognized stop must carry a distinct abnormal-stop outcome, not just a note"
        );
        assert!(
            !outcome
                .abnormal_stop
                .as_deref()
                .unwrap_or_default()
                .contains(raw),
            "the raw wire value must never appear in the abnormal-stop message either"
        );
    }

    #[test]
    fn classify_stop_reason_maps_the_known_shapes() {
        assert_eq!(classify_stop_reason("end_turn"), StopKind::EndTurn);
        assert_eq!(classify_stop_reason("max_tokens"), StopKind::MaxTokens);
        assert_eq!(
            classify_stop_reason("max_turn_requests"),
            StopKind::MaxTurnRequests
        );
        assert_eq!(classify_stop_reason("refusal"), StopKind::Refusal);
        assert_eq!(classify_stop_reason("cancelled"), StopKind::Cancelled);
        assert_eq!(classify_stop_reason("anything_else"), StopKind::Other);
        assert_eq!(classify_stop_reason(""), StopKind::Other);
    }

    #[test]
    fn message_chunks_concatenate_in_order() {
        // ACP streams a reply in pieces; the outcome carries one string.
        let outcome = fold(turn(vec![
            AcpUpdate::MessageChunk("Hello".into()),
            AcpUpdate::MessageChunk(", ".into()),
            AcpUpdate::MessageChunk("world".into()),
        ]));
        assert_eq!(outcome.reply, "Hello, world");
        assert!(outcome.steps.is_empty(), "text alone produces no steps");
    }

    #[test]
    fn a_run_of_thoughts_becomes_one_step() {
        // A model emits these by the hundred. One step per chunk would bury the
        // tool calls an operator is actually reading the timeline for.
        let outcome = fold(turn(vec![
            AcpUpdate::ThoughtChunk,
            AcpUpdate::ThoughtChunk,
            AcpUpdate::ThoughtChunk,
        ]));
        assert_eq!(outcome.steps.len(), 1);
        assert_eq!(outcome.steps[0].kind, TurnStepKind::Thinking);
        assert_eq!(outcome.steps[0].label, "Thinking");
    }

    #[test]
    fn thinking_resumes_as_a_new_step_after_a_tool_call() {
        // Two separate bouts of reasoning either side of a call are two steps —
        // coalescing them would put the thinking in the wrong order relative to
        // the work it bracketed.
        let outcome = fold(turn(vec![
            AcpUpdate::ThoughtChunk,
            AcpUpdate::ToolCall {
                id: "t1".into(),
                title: "Read".into(),
            },
            AcpUpdate::ThoughtChunk,
        ]));
        let kinds: Vec<_> = outcome.steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TurnStepKind::Thinking,
                TurnStepKind::ToolCall,
                TurnStepKind::Thinking
            ]
        );
    }

    #[test]
    fn a_tool_call_takes_its_final_status_and_result() {
        let outcome = fold(turn(vec![
            AcpUpdate::ToolCall {
                id: "t1".into(),
                title: "Read a file".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "t1".into(),
                status: "completed".into(),
                result: Some("2.4 kB".into()),
            },
        ]));
        assert_eq!(outcome.steps.len(), 1, "the update amends, never appends");
        assert_eq!(outcome.steps[0].label, "Read a file");
        assert_eq!(outcome.steps[0].status, TurnStepStatus::Ok);
        assert_eq!(outcome.steps[0].result.as_deref(), Some("6 characters"));
    }

    #[test]
    fn a_failed_tool_call_is_an_error_step() {
        let outcome = fold(turn(vec![
            AcpUpdate::ToolCall {
                id: "t1".into(),
                title: "Write".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "t1".into(),
                status: "failed".into(),
                result: Some("permission denied".into()),
            },
        ]));
        assert_eq!(outcome.steps[0].status, TurnStepStatus::Error);
        assert!(outcome.steps[0].status.is_failure());
    }

    #[test]
    fn a_tool_call_that_never_completes_stays_running() {
        // Exactly what `Running` means: started, no completion seen by the end
        // of the turn. Marking it `Ok` would report work that never finished as
        // having succeeded.
        let outcome = fold(turn(vec![AcpUpdate::ToolCall {
            id: "t1".into(),
            title: "Long thing".into(),
        }]));
        assert_eq!(outcome.steps[0].status, TurnStepStatus::Running);
    }

    #[test]
    fn several_tool_calls_are_amended_independently() {
        // Interleaved calls are ordinary — an agent starts two and they finish
        // out of order. Each update has to find its own step.
        let outcome = fold(turn(vec![
            AcpUpdate::ToolCall {
                id: "a".into(),
                title: "First".into(),
            },
            AcpUpdate::ToolCall {
                id: "b".into(),
                title: "Second".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "b".into(),
                status: "completed".into(),
                result: None,
            },
            AcpUpdate::ToolCallUpdate {
                id: "a".into(),
                status: "failed".into(),
                result: None,
            },
        ]));
        assert_eq!(outcome.steps.len(), 2);
        assert_eq!(outcome.steps[0].label, "First");
        assert_eq!(outcome.steps[0].status, TurnStepStatus::Error);
        assert_eq!(outcome.steps[1].label, "Second");
        assert_eq!(outcome.steps[1].status, TurnStepStatus::Ok);
    }

    #[test]
    fn an_update_for_an_unknown_call_is_dropped_rather_than_invented() {
        // A step with no label is worse on a timeline than no step at all.
        let outcome = fold(turn(vec![AcpUpdate::ToolCallUpdate {
            id: "ghost".into(),
            status: "completed".into(),
            result: Some("x".into()),
        }]));
        assert!(outcome.steps.is_empty());
    }

    /// An agent that answers from a script, so the trait impl can be driven.
    ///
    /// `hang` makes `prompt` never resolve (the grace-expiry path) and
    /// `cancel_fails` makes `cancel` error (the logged-failure path). `cancels`
    /// counts cancel calls so a test can assert the grace path nudged twice.
    ///
    /// `hold_for_cancel` makes `prompt` wait until the first `cancel` arrives —
    /// the shape of a turn that is mid-tool-call when the operator steers, which
    /// is exactly the window the advisory cancel exists for. Without the gate a
    /// prompt that resolves immediately exits the loop before the steer check
    /// ever runs, and the cancel path goes unexercised. `cancel_hangs` makes
    /// `cancel` never answer (the bounded-RPC path).
    struct Scripted {
        turn: AcpTurn,
        /// Milliseconds to hold the turn open before answering — how a test
        /// owns the session's slot for a *bounded* window, so a second turn
        /// genuinely queues and then genuinely gets in.
        holds_ms: u64,
        hang: bool,
        hold_for_cancel: bool,
        cancel_hangs: bool,
        cancel_fails: bool,
        cancels: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        cancel_started: tokio::sync::Notify,
    }

    impl Scripted {
        fn answering(updates: Vec<AcpUpdate>) -> Self {
            Self {
                turn: AcpTurn {
                    updates,
                    stop_reason: "end_turn".into(),
                },
                holds_ms: 0,
                hang: false,
                hold_for_cancel: false,
                cancel_hangs: false,
                cancel_fails: false,
                cancels: Default::default(),
                cancel_started: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait]
    impl AcpAgent for Scripted {
        async fn prompt(
            &self,
            _c: &CompanyId,
            _k: &str,
            _m: &str,
            observer: Option<&AcpObserver>,
        ) -> Result<AcpTurn> {
            // Observed before the hang/hold gates, so a steer test still sees
            // the frames a real transport would have already published by the
            // time the operator reaches for cancel.
            if let Some(observer) = observer {
                for update in &self.turn.updates {
                    observer(update);
                }
            }
            if self.holds_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.holds_ms)).await;
            }
            if self.hang {
                std::future::pending::<()>().await;
            }
            if self.hold_for_cancel {
                self.cancel_started.notified().await;
            }
            Ok(self.turn.clone())
        }
        async fn cancel(&self, _c: &CompanyId, _k: &str) -> Result<()> {
            self.cancels
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.cancel_started.notify_waiters();
            if self.cancel_hangs {
                std::future::pending::<()>().await;
            }
            if self.cancel_fails {
                return Err(OpenCompanyError::Harness("cancel rejected".into()));
            }
            Ok(())
        }
    }

    /// The claim the whole slice rests on: this is usable anywhere the
    /// OpenHuman implementation is.
    ///
    /// Driven through `&dyn RunTurn` rather than through the concrete type,
    /// because that is how the company cycle holds it (`DelegationRunner` takes
    /// `&'a dyn RunTurn`). A type that satisfied the trait but was not
    /// object-safe would compile here and fail at the one site that matters.
    #[tokio::test]
    async fn it_is_usable_through_the_run_turn_seam() {
        let agent = Arc::new(Scripted::answering(vec![
            AcpUpdate::ThoughtChunk,
            AcpUpdate::ToolCall {
                id: "t1".into(),
                title: "Read".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "t1".into(),
                status: "completed".into(),
                result: Some("4 items".into()),
            },
            AcpUpdate::MessageChunk("all done".into()),
        ]));
        let run_turn: &dyn RunTurn = &AcpRunTurn::new(agent);

        let outcome = run_turn
            .run(&CompanyId::new("acme"), "ceo", "go", ChatTarget::default())
            .await
            .expect("a turn runs");

        assert_eq!(outcome.reply, "all done");
        assert_eq!(outcome.steps.len(), 2);
        assert_eq!(outcome.steps[1].status, TurnStepStatus::Ok);
        assert_eq!(outcome.steps[1].result.as_deref(), Some("7 characters"));
    }

    #[tokio::test]
    async fn a_steered_turn_still_returns_an_outcome() {
        // Cancellation in ACP is cooperative: the agent still answers, with
        // `stopReason: "cancelled"`. Abandoning the future on a steer would
        // leave a harness mid-tool-call with nothing reading its output, so the
        // contract is that a steered turn still produces an outcome.
        let agent = Arc::new(Scripted::answering(vec![AcpUpdate::MessageChunk(
            "partial".into(),
        )]));
        let run_turn: &dyn RunTurn = &AcpRunTurn::new(agent);
        let control = crate::company::steer::SteerControl::new();
        control.request(crate::company::steer::SteerAction::Cancel);

        let outcome = run_turn
            .run_steered(
                &CompanyId::new("acme"),
                "ceo",
                "go",
                &control,
                ChatTarget::default(),
                None,
            )
            .await
            .expect("a steered turn still answers");
        assert_eq!(outcome.reply, "partial");
        // The pending action survives for the disposition site to read, which
        // is what decides where the card lands.
        assert!(
            control.pending().is_some(),
            "the steer must not be consumed here"
        );
    }

    #[tokio::test]
    async fn a_failed_cancel_is_logged_and_the_turn_still_drains() {
        // `session/cancel` can fail (the subprocess is mid-shutdown, say), but
        // that must not turn a cancelled turn into a failure of its own: the
        // cancel is advisory, the error is logged, and the turn still answers.
        // The prompt holds until the cancel arrives so the steer check is
        // actually reached — a prompt that resolves first would exit the loop
        // and leave the cancel path unexercised.
        let mut agent = Scripted::answering(vec![AcpUpdate::MessageChunk("done".into())]);
        agent.cancel_fails = true;
        agent.hold_for_cancel = true;
        let cancels = agent.cancels.clone();
        let agent = Arc::new(agent);
        let run_turn: &dyn RunTurn = &AcpRunTurn::new(agent);
        let control = crate::company::steer::SteerControl::new();
        control.request(crate::company::steer::SteerAction::Cancel);

        let outcome = run_turn
            .run_steered(
                &CompanyId::new("acme"),
                "ceo",
                "go",
                &control,
                ChatTarget::default(),
                None,
            )
            .await
            .expect("a failed cancel still ends in a turn");
        assert_eq!(outcome.reply, "done");
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the failed cancel was still attempted exactly once"
        );
    }

    #[tokio::test]
    async fn a_hung_cancel_rpc_does_not_block_the_turn() {
        // A cancellation RPC that never answers — a wedged host, a dead
        // subprocess — must not pin the steered turn forever. Both cancel calls
        // are bounded, so the turn still settles on the grace schedule.
        let mut agent = Scripted::answering(vec![AcpUpdate::MessageChunk("done".into())]);
        agent.cancel_hangs = true;
        agent.hold_for_cancel = true;
        let agent = Arc::new(agent);
        let run_turn = AcpRunTurn::new(agent);
        let control = crate::company::steer::SteerControl::new();
        control.request(crate::company::steer::SteerAction::Cancel);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run_turn.steered_with_grace(
                &CompanyId::new("acme"),
                "ceo",
                "go",
                &control,
                None,
                CancelBounds {
                    grace: Duration::from_millis(20),
                    rpc: Duration::from_millis(50),
                },
            ),
        )
        .await
        .expect("the turn settles despite a hung cancel RPC")
        .expect("the release of the prompt lets the turn answer");

        assert_eq!(outcome.reply, "done");
    }

    #[tokio::test]
    async fn a_cancelled_turn_that_ignores_the_cancel_is_abandoned() {
        // A harness inside a tool call that never returns is the one case the
        // cooperative wait must not honour: past the grace window the waiter
        // drops the turn with an error, and nudges `cancel` once more on the
        // way out — the only drain lever the port exposes.
        let agent = Arc::new(Scripted {
            turn: AcpTurn {
                updates: vec![],
                stop_reason: "end_turn".into(),
            },
            holds_ms: 0,
            hang: true,
            hold_for_cancel: false,
            cancel_hangs: false,
            cancel_fails: false,
            cancels: Default::default(),
            cancel_started: tokio::sync::Notify::new(),
        });
        let cancels = agent.cancels.clone();
        let run_turn = AcpRunTurn::new(agent);
        let control = crate::company::steer::SteerControl::new();
        control.request(crate::company::steer::SteerAction::Cancel);

        let err = run_turn
            .steered_with_grace(
                &CompanyId::new("acme"),
                "ceo",
                "go",
                &control,
                None,
                CancelBounds {
                    grace: Duration::from_millis(20),
                    rpc: Duration::from_millis(50),
                },
            )
            .await
            .expect_err("a hung turn is abandoned, not awaited");
        assert!(
            format!("{err}").contains("abandoning the turn"),
            "the error names the abandonment: {err}"
        );
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one cancel on the steer, one best-effort nudge on the way out"
        );
    }

    #[tokio::test]
    async fn a_turn_cancelled_before_it_starts_never_reaches_the_agent() {
        // The sharp edge serialising turns introduced (PR #1904 review):
        // `session/cancel` names a *session*, not a turn. A queued turn that
        // forwarded its cancel would stop whichever turn currently owns the
        // session — an unrelated turn, still working.
        //
        // Driven by holding the slot with a turn that never finishes, so the
        // second turn is unambiguously still queued when it is cancelled.
        let agent = Arc::new(Scripted {
            turn: AcpTurn {
                updates: vec![],
                stop_reason: "end_turn".into(),
            },
            holds_ms: 0,
            hang: true,
            hold_for_cancel: false,
            cancel_hangs: false,
            cancel_fails: false,
            cancels: Default::default(),
            cancel_started: tokio::sync::Notify::new(),
        });
        let cancels = agent.cancels.clone();
        let run_turn = Arc::new(AcpRunTurn::new(agent));
        let company = CompanyId::new("acme");

        // The lock owner: hangs forever, holding the slot.
        let owner = {
            let run_turn = Arc::clone(&run_turn);
            let company = company.clone();
            tokio::spawn(async move {
                let control = crate::company::steer::SteerControl::new();
                run_turn
                    .run_steered(
                        &company,
                        "ceo",
                        "first",
                        &control,
                        ChatTarget::default(),
                        None,
                    )
                    .await
            })
        };
        // Let it take the slot before the queued turn asks for it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let queued = crate::company::steer::SteerControl::new();
        queued.request(crate::company::steer::SteerAction::Cancel);
        let err = run_turn
            .run_steered(
                &company,
                "ceo",
                "second",
                &queued,
                ChatTarget::default(),
                None,
            )
            .await
            .expect_err("a turn cancelled while queued does not run");

        assert!(
            format!("{err}").contains("cancelled before it started"),
            "the error says it never started: {err}"
        );
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "and no cancel reached the agent, which would have stopped the OTHER turn"
        );

        owner.abort();
    }

    #[tokio::test]
    async fn a_cancel_landing_as_the_slot_frees_still_stops_the_turn() {
        // The race the 250ms poll cannot win alone (PR #1904 review): the
        // cancel arrives while this turn is queued, and the slot frees BEFORE
        // the next tick — so `lock_owned()` wins the select and the control is
        // never consulted. Without the check on acquiring, a cancelled turn
        // would reach the agent.
        //
        // The owner holds for 50ms against a 250ms poll, so the lock branch
        // wins deterministically.
        let mut owner_agent = Scripted::answering(vec![AcpUpdate::MessageChunk("first".into())]);
        owner_agent.holds_ms = 50;
        let agent = Arc::new(owner_agent);
        let cancels = agent.cancels.clone();
        let run_turn = Arc::new(AcpRunTurn::new(agent));
        let company = CompanyId::new("acme");

        let owner = {
            let run_turn = Arc::clone(&run_turn);
            let company = company.clone();
            tokio::spawn(async move {
                let control = crate::company::steer::SteerControl::new();
                run_turn
                    .run_steered(
                        &company,
                        "ceo",
                        "first",
                        &control,
                        ChatTarget::default(),
                        None,
                    )
                    .await
            })
        };
        // Long enough that the owner holds the slot, short enough that it is
        // still holding it when the queued turn asks.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let queued = crate::company::steer::SteerControl::new();
        queued.request(crate::company::steer::SteerAction::Cancel);
        let err = run_turn
            .run_steered(
                &company,
                "ceo",
                "second",
                &queued,
                ChatTarget::default(),
                None,
            )
            .await
            .expect_err("a turn cancelled while queued does not run");

        assert!(
            format!("{err}").contains("cancelled before it started"),
            "the error says it never started: {err}"
        );
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "and no cancel reached the agent, which would have stopped the OTHER turn"
        );
        owner.await.expect("owner joins").expect("owner answers");
    }

    #[tokio::test]
    async fn a_pending_cancel_on_a_free_slot_still_runs_and_is_forwarded() {
        // The other side of that boundary, and the reason the refusal is
        // scoped to queued turns only. With no other turn on the session there
        // is nothing a forwarded cancel could stop by mistake, so a pending
        // cancel keeps its long-standing meaning: the turn runs, the cancel
        // goes to the agent, and the agent winds down and reports — an `Ok`
        // outcome the caller settles as cancelled rather than failed.
        let mut agent = Scripted::answering(vec![AcpUpdate::MessageChunk("done".into())]);
        agent.hold_for_cancel = true;
        let agent = Arc::new(agent);
        let cancels = agent.cancels.clone();
        let run_turn = AcpRunTurn::new(agent);

        let control = crate::company::steer::SteerControl::new();
        control.request(crate::company::steer::SteerAction::Cancel);

        let outcome = run_turn
            .run_steered(
                &CompanyId::new("acme"),
                "ceo",
                "go",
                &control,
                ChatTarget::default(),
                None,
            )
            .await
            .expect("an uncontended turn still runs and returns its outcome");

        assert_eq!(outcome.reply, "done");
        assert_eq!(
            cancels.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the cancel was forwarded, because this turn was the one running"
        );
    }

    #[test]
    fn a_session_key_separates_agents_and_companies() {
        // Two desks sharing a session would share a conversation, and one
        // company's turn would carry another's context.
        let acme = CompanyId::new("acme");
        let globex = CompanyId::new("globex");
        assert_ne!(
            AcpRunTurn::session_key(&acme, "ceo"),
            AcpRunTurn::session_key(&acme, "cto")
        );
        assert_ne!(
            AcpRunTurn::session_key(&acme, "ceo"),
            AcpRunTurn::session_key(&globex, "ceo")
        );
        // Stable across turns, or the second question in a thread arrives with
        // no memory of the first.
        assert_eq!(
            AcpRunTurn::session_key(&acme, "ceo"),
            AcpRunTurn::session_key(&acme, "ceo")
        );
    }
    /// Drains the live frames a turn published, giving up once the bus goes
    /// quiet — a turn that published nothing must be provable, not merely
    /// unobserved, so this returns an empty vec rather than hanging.
    async fn drain_live(
        stream: &mut futures::stream::BoxStream<'static, crate::turn_stream::LiveFrame>,
    ) -> Vec<crate::turn_stream::TurnStreamEvent> {
        use futures::StreamExt;
        let mut frames = Vec::new();
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(50), stream.next()).await
        {
            if let Some(event) = frame.as_turn() {
                frames.push(event.clone());
            }
        }
        frames
    }

    /// The updates a coding turn produces: a thought, a tool call that runs
    /// and then completes, and the answer.
    fn a_working_turn() -> Vec<AcpUpdate> {
        vec![
            AcpUpdate::ThoughtChunk,
            AcpUpdate::ThoughtChunk,
            AcpUpdate::ToolCall {
                id: "c1".into(),
                title: "Read src/main.rs".into(),
            },
            AcpUpdate::ToolCallUpdate {
                id: "c1".into(),
                status: "in_progress".into(),
                result: None,
            },
            AcpUpdate::ToolCallUpdate {
                id: "c1".into(),
                status: "completed".into(),
                result: Some("42 lines".into()),
            },
            AcpUpdate::MessageChunk("done".into()),
        ]
    }

    #[tokio::test]
    async fn a_chat_turn_streams_its_execution_state_onto_the_watching_thread() {
        // The gap this closes: an ACP turn used to be observable only once it
        // was over. On a five-minute coding turn that is indistinguishable
        // from a hang, while a `built_in` teammate beside it shows every tool
        // call as it starts.
        let company = CompanyId::new("acme-live-chat");
        let mut bus = crate::turn_stream::subscribe(&company);

        let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(a_working_turn())));
        let outcome = run_turn
            .run(&company, "ceo", "go", ChatTarget::channel(Some("design")))
            .await
            .expect("the turn answers");

        let frames = drain_live(&mut bus).await;
        let kinds: Vec<&str> = frames.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            vec!["thinking", "tool_call", "tool_result"],
            "one coalesced thinking row, the call, and its completion"
        );

        // Routed to the thread that asked, and labelled with the desk that
        // answered — a frame on the wrong thread is worse than no frame.
        assert!(frames.iter().all(
            |f| f.chat_id.as_deref() == Some("design") && f.agent_id.as_deref() == Some("ceo")
        ));
        // Ordered and dedupable by the console.
        assert_eq!(
            frames.iter().map(|f| f.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        let call = &frames[1];
        assert_eq!(call.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(call.label.as_deref(), Some("Read src/main.rs"));
        assert_eq!(call.status, Some("running"));

        let result = &frames[2];
        assert_eq!(
            result.tool_call_id.as_deref(),
            Some("c1"),
            "the completion pairs back to its row"
        );
        assert_eq!(result.status, Some("ok"));
        assert_eq!(result.result.as_deref(), Some("8 characters"));

        // And the live view did not replace the durable one.
        assert_eq!(outcome.reply, "done");
        assert_eq!(
            outcome
                .steps
                .iter()
                .filter(|s| s.kind == TurnStepKind::ToolCall)
                .count(),
            1,
            "the same updates still fold into the timeline that rides the reply"
        );
    }

    #[tokio::test]
    async fn an_unaddressed_chat_turn_streams_onto_the_default_desk() {
        // Where the durable reply lands is where the live rows must land: an
        // API client that omits `chat` still gets a coherent timeline.
        let company = CompanyId::new("acme-live-default");
        let mut bus = crate::turn_stream::subscribe(&company);

        let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(vec![AcpUpdate::ToolCall {
            id: "c1".into(),
            title: "Search".into(),
        }])));
        run_turn
            .run(&company, "ceo", "go", ChatTarget::default())
            .await
            .expect("the turn answers");

        let frames = drain_live(&mut bus).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].chat_id.as_deref(),
            Some(crate::server::ops::language::DEFAULT_DESK)
        );
    }

    #[tokio::test]
    async fn a_dispatched_card_turn_streams_nothing_onto_the_console() {
        // A dispatched card shows no chat bubble and its steps are folded into
        // the card's own note. Streaming them would put rows on whatever
        // thread most recently sent — the misattribution `LiveStream::Off`
        // exists to prevent.
        let company = CompanyId::new("acme-live-card");
        let mut bus = crate::turn_stream::subscribe(&company);

        let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(a_working_turn())));
        let control = crate::company::steer::SteerControl::new();
        run_turn
            .run_steered_background(&company, "ceo", "go", &control, None)
            .await
            .expect("the turn answers");

        assert!(
            drain_live(&mut bus).await.is_empty(),
            "a background turn publishes nothing"
        );
    }

    #[tokio::test]
    async fn a_workflow_node_streams_onto_its_run_rather_than_a_desk() {
        // The trait default for `run_background_workflow` forwards to `run`
        // with no chat id — which, now that `run` streams, would publish a
        // node's tool calls onto the DEFAULT DESK. This asserts the override
        // that keeps them on the run-trace sheet instead.
        let company = CompanyId::new("acme-live-workflow");
        let mut bus = crate::turn_stream::subscribe(&company);

        let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(vec![AcpUpdate::ToolCall {
            id: "c1".into(),
            title: "Fetch".into(),
        }])));
        run_turn
            .run_background_workflow(&company, "ceo", "go", None, "run-7", "node-2")
            .await
            .expect("the turn answers");

        let frames = drain_live(&mut bus).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].workflow_run_id.as_deref(), Some("run-7"));
        assert_eq!(frames[0].node_id.as_deref(), Some("node-2"));
        assert!(
            frames[0].chat_id.is_none(),
            "a node has no chat thread to attribute to"
        );
    }

    /// How many rows a console holding these frames ends up showing.
    ///
    /// A tool call is two frames and one row: `tool_call` opens it and
    /// `tool_result` flips that same row in place, paired by `toolCallId`
    /// (`app-shell.tsx`'s `onTurnEvent`). Counting frames instead of rows
    /// would make the live view look like it shows twice the work.
    fn rendered_rows(frames: &[TurnStreamEvent]) -> usize {
        let paired: std::collections::HashSet<&str> = frames
            .iter()
            .filter_map(|f| f.tool_call_id.as_deref())
            .collect();
        paired.len() + frames.iter().filter(|f| f.tool_call_id.is_none()).count()
    }

    #[test]
    fn the_live_rows_and_the_folded_steps_stay_in_step() {
        // The two views are the same updates read twice, and the property that
        // matters is that neither invents or drops a row the other has. A
        // non-terminal `tool_call_update` is the one that could: it leaves the
        // folded step `Running` and must publish no second row.
        let updates = a_working_turn();
        let mut state = LiveState::default();
        let frames: Vec<_> = updates
            .iter()
            .filter_map(|u| live_frame_from(u, &mut state))
            .collect();

        let outcome = fold(turn(updates));
        assert_eq!(
            rendered_rows(&frames),
            outcome.steps.len(),
            "one live row per folded step: {frames:?} vs {:?}",
            outcome.steps
        );
        assert_eq!(
            frames.iter().filter(|f| f.kind == "tool_call").count(),
            outcome
                .steps
                .iter()
                .filter(|s| s.kind == TurnStepKind::ToolCall)
                .count()
        );
    }

    #[tokio::test]
    async fn a_completion_for_a_call_nobody_saw_start_publishes_no_row() {
        // `fold` drops these ("a step with no label is worse on a timeline
        // than no step"), so the live view must too — a row that appears while
        // the turn runs and is missing from the finished timeline reads as
        // work that was undone.
        let company = CompanyId::new("acme-live-ghost");
        let mut bus = crate::turn_stream::subscribe(&company);

        let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(vec![
            AcpUpdate::ToolCallUpdate {
                id: "ghost".into(),
                status: "completed".into(),
                result: Some("x".into()),
            },
        ])));
        let outcome = run_turn
            .run(&company, "ceo", "go", ChatTarget::channel(Some("design")))
            .await
            .expect("the turn answers");

        assert!(drain_live(&mut bus).await.is_empty());
        assert!(
            outcome
                .steps
                .iter()
                .all(|s| s.kind != TurnStepKind::ToolCall)
        );
    }

    #[test]
    fn thinking_around_assistant_text_folds_and_streams_the_same_way() {
        // The divergence PR #1904's review caught: the live mapper closed a
        // thinking run on assistant text and `fold` did not, so this sequence
        // streamed two `Thinking` rows and folded one — the second row
        // vanishing the moment the reply replaced the live timeline.
        let updates = vec![
            AcpUpdate::ThoughtChunk,
            AcpUpdate::MessageChunk("partly there. ".into()),
            AcpUpdate::ThoughtChunk,
            AcpUpdate::MessageChunk("done".into()),
        ];

        let mut state = LiveState::default();
        let live = updates
            .iter()
            .filter_map(|u| live_frame_from(u, &mut state))
            .count();

        let outcome = fold(turn(updates));
        let folded = outcome
            .steps
            .iter()
            .filter(|s| s.kind == TurnStepKind::Thinking)
            .count();

        assert_eq!(folded, 2, "text closes a thinking run, so this is two");
        assert_eq!(live, folded, "and the live view says the same");
        assert_eq!(outcome.reply, "partly there. done");
    }

    #[test]
    fn a_burst_of_thoughts_is_one_row_until_something_else_happens() {
        // A model emits these by the hundred; a timeline of them is noise.
        // Mirrors `fold`'s own coalescing so the live view does not show a
        // different number of thinking rows than the finished one.
        let mut state = LiveState::default();
        let thoughts = vec![AcpUpdate::ThoughtChunk; 5];
        let frames: Vec<_> = thoughts
            .iter()
            .filter_map(|u| live_frame_from(u, &mut state))
            .collect();
        assert_eq!(frames.len(), 1);

        // Text closes the run, so the next thought opens a new row — exactly
        // what `fold` does with its own `thinking` flag.
        assert!(live_frame_from(&AcpUpdate::MessageChunk("hi".into()), &mut state).is_none());
        assert!(live_frame_from(&AcpUpdate::ThoughtChunk, &mut state).is_some());
    }
}
