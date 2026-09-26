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
    /// This company's `(desk id, desk name)` pairs, so [`Self::session_key`]
    /// can canonicalise a selector without a store.
    ///
    /// A snapshot taken where the lane is built, which is per company and holds
    /// the record already. An ACP session is durable state in another process
    /// and this key is the only handle on it, so the alternative — resolving
    /// through the store on the turn path — would put a read in front of every
    /// prompt to answer a question the manifest answers.
    ///
    /// Staleness is benign in the only direction it can go: a desk's *id* never
    /// changes, and this maps toward the id, so a snapshot predating a new desk
    /// simply falls through to the verbatim selector — what this did before the
    /// key carried a chat at all.
    desks: Vec<(String, String)>,
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
            desks: Vec::new(),
        }
    }

    /// The company's desks, so an id and a display name key one session.
    pub fn with_desks(mut self, desks: Vec<(String, String)>) -> Self {
        self.desks = desks;
        self
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

    /// The session an agent's turns share, **per conversation** (issue #1890 H).
    ///
    /// Stable across turns, so the second question in a thread does not arrive
    /// with no memory of the first — and scoped to the channel, so two desks do
    /// not share one. The doc here used to claim that second property while the
    /// key did not have it: `{company}::{agent}` put every desk, every DM and
    /// every thread an ACP teammate answered in into a single durable
    /// conversation owned by an external process.
    ///
    /// # Why the channel and not the thread
    ///
    /// The rest of #1890 makes the *thread* the context boundary, and for the
    /// built-in harness it is: that binding is in-memory, so a new one costs
    /// nothing and a stale one is dropped by the process that owns it.
    ///
    /// An ACP session is neither. It is durable state inside another program,
    /// reached through a port that has no lifecycle at all — [`AcpAgent`] can
    /// `prompt` and `cancel`, and there is no create, close or reap on it
    /// anywhere. Keying per thread would therefore mint an unbounded number of
    /// external sessions on a busy channel with nothing able to collect them,
    /// and giving them a lifetime means designing one across a port whose
    /// implementation lives in the desktop shell rather than in this crate.
    ///
    /// A channel is bounded by the roster and the desk list and changes rarely,
    /// so this is the isolation that can be had without inventing that
    /// lifecycle. Thread-level scoping stays open on #1890 H, with this as the
    /// reason it is not closed here.
    fn session_key(&self, company: &CompanyId, agent_id: &str, chat_id: Option<&str>) -> String {
        match chat_id {
            // Folded through the same rule every other reader of a chat id
            // uses, so the four spellings of the General desk are one
            // conversation here too rather than four sessions.
            Some(chat) if !crate::server::chat_history::is_general_chat(Some(chat)) => {
                // …and a named desk's two spellings likewise. The key was
                // `(company, agent)` before #1890 H, where no selector could
                // disagree with itself; adding the chat introduced the
                // possibility that addressing one desk by id and by name mints
                // two sessions in the external agent, and losing the earlier
                // one's context is not a thing this issue may cost (codex on
                // #1972). Canonical is the id, the half a rename does not
                // change.
                let canonical = self
                    .desks
                    .iter()
                    .find(|(id, name)| {
                        id.eq_ignore_ascii_case(chat) || name.eq_ignore_ascii_case(chat)
                    })
                    .map(|(id, _)| id.as_str())
                    .unwrap_or(chat);
                format!("{}::{agent_id}::{canonical}", company.as_ref())
            }
            // The General desk, and every turn that names no conversation at
            // all — a dispatched card, a workflow node. Both keep the key this
            // function has always returned, so an existing session is still
            // found by the turns that were already sharing it.
            _ => format!("{}::{agent_id}", company.as_ref()),
        }
    }

    /// The slot that serialises one agent's turns.
    ///
    /// Deliberately **not** the session key (issue #1890 H). The two answer
    /// different questions: the session key says which conversation a turn
    /// belongs to, and this says which turns may not run at once. Widening this
    /// alongside the session would let one agent's desks prompt an external
    /// process concurrently — a change to how that process is driven, made as a
    /// side effect of a change about conversation scope, and not one this issue
    /// has any evidence for.
    fn lock_key(company: &CompanyId, agent_id: &str) -> String {
        format!("{}::{agent_id}", company.as_ref())
    }

    /// The live-stream context for a **chat** turn.
    ///
    /// Falls back to the answering agent's DM when the caller addressed no
    /// thread — the same rule `built_in`'s `LiveStream::On` applies.
    fn chat_ctx(company: &CompanyId, agent_id: &str, chat: ChatTarget<'_>) -> TurnStreamCtx {
        TurnStreamCtx {
            company: company.clone(),
            agent_id: agent_id.to_string(),
            route: LiveRoute::Chat {
                chat_id: crate::runtime::assignee::chat_or_dm(chat.chat_id, agent_id),
            },
            // Takes the whole `ChatTarget` rather than the id alone, so this
            // cannot go on answering with `None` while the caller holds the
            // sequence. An ACP agent answers operator messages in a shared chat
            // exactly as the built-in harness does, so without it two of its
            // turns in one thread share a single row-list and arming the second
            // erases the first — the failure this field exists to stop, left in
            // place for one of the two harnesses (Codex on #2069).
            message_seq: chat.message_seq.map(|seq| seq.value()),
        }
    }

    async fn run_once(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        chat_id: Option<&str>,
        stream: Option<TurnStreamCtx>,
    ) -> Result<TurnOutcome> {
        let key = self.session_key(company, agent_id, chat_id);
        let slot = self.turn_lock(&Self::lock_key(company, agent_id));
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
        let ctx = Self::chat_ctx(company, agent_id, chat);
        self.run_once(company, agent_id, message, chat.chat_id, Some(ctx))
            .await
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
        let ctx = Self::chat_ctx(company, agent_id, chat);
        self.steered(company, agent_id, message, control, chat.chat_id, Some(ctx))
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
        chat: ChatTarget<'_>,
        _run_sink: Option<Arc<crate::harness::run_trace::RunTraceSink>>,
    ) -> Result<TurnOutcome> {
        // No live context on purpose: a dispatched card's turn shows no chat
        // bubble, and its rows must not appear on whatever thread most
        // recently sent. Same rule as `built_in`'s `LiveStream::Off`.
        //
        // The **session** still follows the conversation when there is one
        // (#1890 H + I): an approval's re-issued call belongs to the thread it
        // was raised in, and answering it on whichever session was last used
        // would be the leak H closes, arriving through the one entry point that
        // streams nothing.
        self.steered(company, agent_id, message, control, chat.chat_id, None)
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
        self.run_once(company, agent_id, message, None, None).await
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
            // A workflow node answers a graph, not a message.
            message_seq: None,
        };
        self.run_once(company, agent_id, message, None, Some(ctx))
            .await
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
        chat_id: Option<&str>,
        stream: Option<TurnStreamCtx>,
    ) -> Result<TurnOutcome> {
        self.steered_with_grace(
            company,
            agent_id,
            message,
            control,
            chat_id,
            stream,
            CancelBounds::DEFAULT,
        )
        .await
    }

    /// [`Self::steered`] with both timing bounds made explicit — the post-cancel
    /// grace and the per-cancel-RPC bound — so the tests can expire them in
    /// milliseconds rather than waiting out the real windows.
    // One over the limit since #1890 H, and the one that pushed it there is the
    // conversation the session is keyed by. Bundling the timing bounds back
    // together would undo exactly what this function exists to expose.
    #[allow(clippy::too_many_arguments)]
    async fn steered_with_grace(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        control: &crate::company::steer::SteerControl,
        chat_id: Option<&str>,
        stream: Option<TurnStreamCtx>,
        bounds: CancelBounds,
    ) -> Result<TurnOutcome> {
        let CancelBounds {
            grace,
            rpc: cancel_rpc,
        } = bounds;
        let key = self.session_key(company, agent_id, chat_id);

        // Wait for this teammate's turn slot **steerably**. A turn cancelled
        // while it is still queued must never reach the adapter: its cancel
        // would name a session that, at that moment, may belong to whichever
        // turn is actually running, and would stop that one instead. So a
        // cancel here ends this turn where it stands — nothing was started, so
        // there is nothing to stop.
        //
        // Since issue #1890 H the slot is keyed per teammate while the session
        // is keyed per conversation, so that misfire is now only reachable
        // between two turns in the SAME channel — which share both. Two
        // channels' turns still queue on one slot, and a cancel from the queued
        // one names its own session rather than the running one's. The guard
        // stays: it is exactly right for the same-channel case, and refusing a
        // turn that was cancelled before it started is correct regardless.
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
        let slot = self.turn_lock(&Self::lock_key(company, agent_id));
        let _slot = match Arc::clone(&slot).try_lock_owned() {
            Ok(slot) => slot,
            Err(_) => {
                // Contended: this teammate is mid-turn — on this conversation
                // or on another of theirs, since the slot is per teammate.
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
#[path = "run_turn_test_fixtures.rs"]
mod run_turn_test_fixtures;
/// `fold`'s reduction of raw ACP updates into steps — pure, synchronous,
/// no `Scripted` agent needed. Split from the rest of this module's tests
/// because the combined inline module exceeded the 750-line file limit.
#[cfg(test)]
#[path = "run_turn_fold_tests.rs"]
mod tests_fold;
/// End-to-end coverage of [`AcpRunTurn`] through `&dyn RunTurn`: cancel,
/// steer, and the grace/hang/hold timing paths. See `tests_fold` above for
/// why this is split out.
#[cfg(test)]
#[path = "run_turn_seam_tests.rs"]
mod tests_seam;
/// `AcpRunTurn::session_key` coverage.
#[cfg(test)]
#[path = "run_turn_session_key_tests.rs"]
mod tests_session_key;
/// The live-frame/turn-stream projection: `drain_live`, `live_frame_from`,
/// and the folded-vs-live row-count invariant.
#[cfg(test)]
#[path = "run_turn_streaming_tests.rs"]
mod tests_streaming;
