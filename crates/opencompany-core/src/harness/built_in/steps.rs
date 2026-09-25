//! Fold the harness progress stream into the scrubbed [`TurnStep`] timeline
//! surfaced in operator chat.
//!
//! During [`Agent::turn`](openhuman_core::agent::Agent) the tinyagents
//! observability bridge emits a stream of
//! [`AgentProgress`](oh::agent::progress::AgentProgress) events — tool calls
//! starting/completing, thinking/text deltas, cost updates, sub-agent lifecycle.
//! [`CompanyAgent::run`](crate::harness::CompanyAgent) drains that stream into a
//! `Vec<AgentProgress>` and hands it here; [`fold_steps`] turns it into the
//! compact, **scrubbed** [`TurnStep`] list that rides back on the operator
//! bubble.
//!
//! Compiled only under `feature = "openhuman"`.
//!
//! ## What a step answers (issue #411)
//!
//! A step used to be a name, a duration and — on failure — the words "Something
//! went wrong with this action." Three different, differently-fixable failures
//! rendered identically, and the host knew which was which the whole time. A
//! step now answers three questions instead of none:
//!
//! * **What was it doing** — [`TurnStep::detail`], a bounded rendering of the
//!   call's arguments, so two reads of two different files stop looking alike.
//! * **What came back** — [`TurnStep::result`]: an intrinsic tool's own message,
//!   or for everything else a *shape* (`"12 items"`, `"2.4k characters"`) and
//!   never its content.
//! * **Why it stopped** — [`TurnStep::failure`], a **typed**
//!   [`TurnStepFailure`], mapped from OpenHuman's `ToolFailureClass` in one
//!   exhaustive `match`. The console renders a known state; it never
//!   pattern-matches prose.
//!
//! Plus two states that were previously invisible:
//!
//! * **Parked, not broken.** A call the approval policy gated is
//!   [`TurnStepStatus::AwaitingApproval`], not an error, and is not counted as
//!   one. See [`is_awaiting_approval`] for how it is recognised — and for why
//!   the two needles it keys on are each pinned by a test.
//! * **Cut, not complete.** A result the harness truncated sets
//!   [`TurnStep::truncated`] (issue #410). A success whose answer is incomplete
//!   is a state no status word can express, which is exactly how #410 stayed
//!   hidden.
//!
//! ## Security (the whole reason this is a separate, unit-tested module)
//!
//! The wire shape carries **no raw tool output and no call ids**. Arguments are
//! carried, but only through the host's *existing* redactor:
//!
//! * **Label** comes from the tool's server-computed `display_label`, else its
//!   tool *name* — never from arguments or output. The loop does not ask a tool
//!   for that label, so [`StepLabels`] restores it from the built tool set
//!   before the event is folded; both halves are registry-derived, and neither
//!   widens what a label may contain.
//! * **Detail** (arguments) is passed through
//!   [`approval_display::redact`](crate::runtime::approval_display::redact) —
//!   issue #372's host-side redactor — and then bounded. This is a deliberate
//!   widening of the old whitelist-only rule, and it is **not** a second
//!   redaction surface: an approval card already shows an operator this exact
//!   object, because a gated call's arguments *are* the parked effect's payload
//!   ([`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for)).
//!   Re-deriving a stricter rule here would have created the drift the issue
//!   forbids. It follows that #372's documented limit applies here too: the
//!   denylist matches on **keys**, so a credential hidden in free text under a
//!   benign key is shown, on both surfaces, by the same rule.
//! * **Result on success** is never the output's content for a remote tool —
//!   only its shape. An intrinsic OpenCompany tool's output *is*
//!   OpenCompany-authored operator copy, so it is surfaced bounded, exactly as
//!   its failure message already was.
//! * **Result on failure** is the classifier's plain-language
//!   [`cause_plain`](oh::tools::status::ClassifiedFailure::cause_plain), or an
//!   intrinsic tool's own message — never the remote error text.
//! * **One upstream verdict is re-read** (issue #924). `ENOENT` is the same
//!   operating-system error whether a binary is not installed or a file is not
//!   there, and upstream classifies on message text alone, so it calls both
//!   [`MissingApp`](oh::tools::status::ToolFailureClass::MissingApp) — telling a
//!   server operator to "install or open the app" when a note is simply absent.
//!   For a tool that resolves a path in this process there is no app to install,
//!   so [`PATH_ONLY_TOOLS`] names those and [`refine_missing_app`] reports
//!   [`TurnStepFailure::NotFound`] instead. Nothing else is re-classified.
//!
//! The unit test `planted_secret_never_reaches_serialized_steps` proves it end
//! to end: a secret planted in a tool's output, its nested arguments, and its
//! `display_detail` appears in **no** serialized step.
//!
//! Steps must also never enter the memory store — `memory_loop::outcome_chunk`
//! stays text-only — so a scrubbed detail can never be re-retrieved and
//! re-injected into a later turn.

use std::collections::HashMap;
use std::sync::Arc;

use openhuman_core as oh;
use serde_json::Value;

use oh::agent::progress::AgentProgress;
use oh::tools::status::{ClassifiedFailure, ToolFailureClass};
use tinytools::humanize_tool_name;

use crate::harness::policy::POLICY_NAME;
use crate::ports::deep_trace::TurnStepDetail;
use crate::ports::types::{TurnStep, TurnStepFailure, TurnStepKind, TurnStepStatus};
use crate::runtime::approval_display;
use crate::turn_stream::TurnStreamEvent;

/// Hard cap on the number of steps carried back to the operator. A runaway turn
/// (a tight tool loop) is truncated to this many, plus one omission note.
const MAX_STEPS: usize = 50;

/// Fold an ordered progress stream into the scrubbed [`TurnStep`] timeline.
///
/// * Pairs each `ToolCallStarted` with its `ToolCallCompleted` by `call_id` into
///   one step; an unmatched start stays [`Running`](TurnStepStatus::Running).
/// * Coalesces a run of consecutive `ThinkingDelta`s into one label-only
///   "Thinking" step.
/// * Ignores every other event (text deltas, iteration/cost updates, sub-agent
///   lifecycle) — they carry nothing an operator-facing timeline needs and
///   would only add noise.
/// * Caps the result at [`MAX_STEPS`], appending a note when steps were dropped.
pub fn fold_steps(events: Vec<AgentProgress>) -> Vec<TurnStep> {
    let mut steps: Vec<TurnStep> = Vec::new();
    // call_id → index of its (still-running) step in `steps`, so the matching
    // `ToolCallCompleted` can finalize it in place. Removed on match so a reused
    // id never double-folds.
    let mut running: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // Whether the most recently emitted step is the open "Thinking" run, so
    // consecutive thinking deltas coalesce into it.
    let mut thinking_open = false;

    for event in events {
        match event {
            AgentProgress::ToolCallStarted {
                call_id,
                tool_name,
                display_label,
                ..
            } => {
                thinking_open = false;
                // NOTE: `arguments` is `Null` on the tinyagents path here — real
                // args arrive on `ToolCallCompleted`, so we do not enrich yet.
                let step = TurnStep {
                    kind: TurnStepKind::ToolCall,
                    status: TurnStepStatus::Running,
                    label: label_for(display_label, &tool_name),
                    ..TurnStep::default()
                };
                running.insert(call_id, steps.len());
                steps.push(step);
            }
            AgentProgress::ToolCallCompleted {
                call_id,
                tool_name,
                success,
                output,
                arguments,
                elapsed_ms,
                failure,
                ..
            } => {
                thinking_open = false;
                let done = complete(
                    &tool_name,
                    success,
                    &output,
                    arguments.as_ref(),
                    failure.as_ref(),
                );

                if let Some(idx) = running.remove(&call_id) {
                    // Finalize the paired start in place, keeping its label.
                    let step = &mut steps[idx];
                    step.elapsed_ms = Some(elapsed_ms);
                    done.apply(step);
                } else {
                    // A completion with no observed start — surface it standalone.
                    let mut step = TurnStep {
                        kind: TurnStepKind::ToolCall,
                        label: humanize(&tool_name),
                        elapsed_ms: Some(elapsed_ms),
                        ..TurnStep::default()
                    };
                    done.apply(&mut step);
                    steps.push(step);
                }
            }
            // The first thinking delta of a run opens one label-only step.
            // Consecutive deltas (the guard is already false) fall through to the
            // catch-all below and fold into that same step.
            AgentProgress::ThinkingDelta { .. } if !thinking_open => {
                steps.push(TurnStep {
                    kind: TurnStepKind::Thinking,
                    status: TurnStepStatus::Ok,
                    label: "Thinking".to_string(),
                    ..TurnStep::default()
                });
                thinking_open = true;
            }
            AgentProgress::TextDelta { .. } => {
                // Visible assistant text breaks a thinking run without adding a
                // step of its own (the reply text is the bubble body).
                thinking_open = false;
            }
            // Everything else (iteration/cost updates, args-delta fragments,
            // sub-agent lifecycle, task-board, turn-started) contributes no
            // operator-facing step. It also does not break thinking coalescing.
            _ => {}
        }
    }

    if steps.len() > MAX_STEPS {
        let omitted = steps.len() - MAX_STEPS;
        steps.truncate(MAX_STEPS);
        steps.push(TurnStep {
            kind: TurnStepKind::Note,
            status: TurnStepStatus::Ok,
            label: format!(
                "{omitted} more step{} omitted",
                if omitted == 1 { "" } else { "s" }
            ),
            ..TurnStep::default()
        });
    }

    steps
}

/// The incremental counterpart of [`fold_steps`]: the same projection, but
/// yielding each step **as it happens** with the ordinal it occupies, so a
/// durable trace can be written during the turn instead of only at its end
/// (issue #242).
///
/// The difference that matters is *when*, not *what*. [`fold_steps`] can only
/// produce anything once the turn is over, which is why killing the host
/// mid-run used to leave no trace at all. This yields:
///
/// * a `ToolCallStarted` → a new ordinal carrying a
///   [`Running`](TurnStepStatus::Running) step;
/// * its matching `ToolCallCompleted` → **the same ordinal again**, finalized.
///   `RunStore::append_run_step` is keyed on `(run_id, step_seq)` and replaces
///   on a match, so re-yielding the ordinal finalizes the row in place exactly
///   as `fold_steps` finalizes the entry in place;
/// * a completion with no observed start → its own new ordinal, standalone;
/// * the first `ThinkingDelta` of a run → one coalesced "Thinking" ordinal,
///   with consecutive deltas yielding nothing.
///
/// Materialize every yield in ordinal order and the result is byte-identical to
/// `fold_steps` over the same event stream (pinned by
/// `incremental_trace_converges_on_the_folded_timeline`), with **two** deliberate
/// exceptions:
///
/// * **An unfinished call.** A start whose completion never arrives — because
///   the process died mid-tool-call — stays persisted as `Running`. That is not
///   a divergence to paper over, it is the whole point: the persisted trace
///   records what was actually observed, and "this tool call was still in
///   flight" is the truth about a killed run.
/// * **Past 50 steps, the two lengths part.** `fold_steps` truncates at
///   [`MAX_STEPS`] and appends an omission note, because it builds a chat bubble
///   and a bubble that scrolls forever is unreadable. The trace has no such
///   limit here; [`run_trace::MAX_RUN_STEPS`](super::run_trace::MAX_RUN_STEPS)
///   bounds what is persisted, an order of magnitude higher. A record of what an
///   attempt did should not be truncated at the length a *message* wants to be.
///   Note that the convergence test runs a handful of events, so it pins the
///   shared prefix, not this boundary.
///
/// **Run-scoped, not turn-scoped.** One instance spans every turn of an
/// attempt — the redirect re-runs and a delegate's turn — so ordinals stay
/// Bytes of accumulated reasoning that trigger an interim flush.
///
/// A thinking run is re-emitted under its own ordinal whenever it crosses this,
/// and once more when it closes. Per-delta would be one store write per token;
/// close-only would lose the whole thought if the host died mid-run, which is
/// the failure the incremental trace exists to prevent.
const DEEP_THINK_FLUSH_BYTES: usize = 2 * 1024;

/// dense and unique across the run rather than restarting per turn and
/// overwriting earlier rows.
#[derive(Debug, Default)]
pub(crate) struct StepTrace {
    /// The next ordinal to hand out. Also the number of steps yielded so far.
    next: u32,
    /// `call_id` → the ordinal and label its start claimed, so the completion
    /// finalizes that row keeping the richer start-time label. In deep mode it
    /// also carries the start event's detail, whose `display_detail` and
    /// `iteration` the store would otherwise discard when the completion
    /// replaces the row at the same ordinal.
    running: std::collections::HashMap<String, (u32, String, Option<TurnStepDetail>)>,
    /// Whether the most recent step is an open "Thinking" run.
    thinking_open: bool,
    /// Whether to yield the unredacted companion alongside each step.
    ///
    /// A flag rather than a separate type because the two projections must share
    /// ONE state machine: the ordinals, the `running` map and the thinking
    /// coalescing all have to agree, and two machines reading the same event
    /// stream would eventually disagree about which ordinal a step got.
    deep: bool,
    /// The ordinal of the open thinking run, and the reasoning text accumulated
    /// into it so far.
    ///
    /// A thinking run is many `ThinkingDelta` events that fold to ONE step, so
    /// the text has to accumulate somewhere and be re-emitted under the same
    /// ordinal. The store replaces on `(run_id, step_seq)`, so re-emitting
    /// finalizes in place rather than duplicating.
    thinking_buf: Option<(u32, String)>,
    /// Number of reasoning bytes added since the last interim flush.
    thinking_pending_bytes: usize,
}

impl StepTrace {
    /// A trace that also yields the unredacted companion of each step.
    ///
    /// Off by default (`StepTrace::default()`), so a caller that never asks for
    /// deep detail cannot accidentally accumulate reasoning text.
    pub(crate) fn deep() -> Self {
        Self {
            deep: true,
            ..Self::default()
        }
    }

    /// Feeds one progress event, yielding the ordinal, the scrubbed step, and —
    /// when this trace is [`deep`](Self::deep) — its unredacted companion.
    ///
    /// All three come from ONE call on purpose. The alternative, a second pass
    /// over the same events, would be a second state machine that has to agree
    /// with this one about ordinals and about where a thinking run starts and
    /// ends; when it eventually disagreed, a detail would be filed against the
    /// wrong step. Returning them together makes the alignment structural.
    /// Usually zero or one record; **two** when a tool call closes an open
    /// thinking run, because the run's accumulated reasoning has to be
    /// finalized under its own ordinal before the tool's step is emitted.
    /// Dropping that tail would lose the reasoning immediately preceding a tool
    /// call, which is the part worth reading.
    pub(crate) fn push(
        &mut self,
        event: &AgentProgress,
    ) -> Vec<(u32, TurnStep, Option<TurnStepDetail>)> {
        match event {
            AgentProgress::ToolCallStarted {
                call_id,
                tool_name,
                display_label,
                display_detail,
                iteration,
                ..
            } => {
                let closing = self.close_thinking();
                self.thinking_open = false;
                let label = label_for(display_label.clone(), tool_name);
                let seq = self.claim();
                // NOTE: `arguments` is `Null` here on the tinyagents path —
                // the crate emits real arguments on the *completed* event —
                // so a started step has nothing unredacted to add beyond the
                // harness's own label.
                let start_detail = self.deep.then(|| {
                    crate::ports::deep_trace::bound_detail(TurnStepDetail {
                        display_detail: display_detail.clone(),
                        iteration: Some(*iteration),
                        ..TurnStepDetail::default()
                    })
                });
                self.running
                    .insert(call_id.clone(), (seq, label.clone(), start_detail.clone()));
                let mut out = Vec::new();
                out.extend(closing);
                out.push((
                    seq,
                    TurnStep {
                        kind: TurnStepKind::ToolCall,
                        status: TurnStepStatus::Running,
                        label,
                        ..TurnStep::default()
                    },
                    start_detail,
                ));
                out
            }
            AgentProgress::ToolCallCompleted {
                call_id,
                tool_name,
                success,
                output,
                arguments,
                elapsed_ms,
                failure,
                ..
            } => {
                self.thinking_open = false;
                let done = complete(
                    tool_name,
                    *success,
                    output,
                    arguments.as_ref(),
                    failure.as_ref(),
                );
                let (seq, label, start_detail) = match self.running.remove(call_id) {
                    Some(found) => found,
                    // No observed start — surface it standalone, exactly as the
                    // fold does.
                    None => (self.claim(), humanize(tool_name), None),
                };
                let mut step = TurnStep {
                    kind: TurnStepKind::ToolCall,
                    label,
                    elapsed_ms: Some(*elapsed_ms),
                    ..TurnStep::default()
                };
                done.apply(&mut step);
                // The whole point of the deep store: `output` and `arguments`
                // here are what the tool actually received and returned, before
                // `complete` reduced them to a shape and a redacted summary.
                let detail = self.deep.then(|| {
                    let mut detail = TurnStepDetail {
                        arguments: arguments
                            .as_ref()
                            .filter(|a| !a.is_null())
                            .map(|a| a.to_string()),
                        output: (!output.is_empty()).then(|| output.clone()),
                        ..TurnStepDetail::default()
                    };
                    // The store replaces the whole row on completion, so fold in
                    // the start-only metadata (label detail, iteration) before
                    // persisting — otherwise a finalized call loses them.
                    if let Some(start) = &start_detail {
                        detail.display_detail = start.display_detail.clone();
                        detail.iteration = start.iteration;
                    }
                    crate::ports::deep_trace::bound_detail(detail)
                });
                vec![(seq, step, detail.filter(|d| !d.is_empty()))]
            }
            AgentProgress::ThinkingDelta { delta, .. } if !self.thinking_open => {
                self.thinking_open = true;
                let seq = self.claim();
                if self.deep {
                    // The first delta is emitted in its own detail below, so it
                    // must NOT also enter `thinking_buf`: later threshold flushes
                    // and `close_thinking` re-emit that buffer, and the sink
                    // appends each emission to the stored prefix. Counting it
                    // twice is what used to turn `"first second"` into
                    // `"firstfirst second"`.
                    self.thinking_pending_bytes = 0;
                    self.thinking_buf = Some((seq, String::new()));
                }
                vec![(
                    seq,
                    TurnStep {
                        kind: TurnStepKind::Thinking,
                        status: TurnStepStatus::Ok,
                        label: "Thinking".to_string(),
                        ..TurnStep::default()
                    },
                    self.deep.then(|| {
                        crate::ports::deep_trace::bound_detail(TurnStepDetail {
                            reasoning: Some(delta.clone()),
                            ..TurnStepDetail::default()
                        })
                    }),
                )]
            }
            // Every delta after the first in a run. It yields no NEW step — the
            // run already claimed one — but it does carry text, so in deep mode
            // it re-emits the SAME ordinal with the bytes accumulated since the
            // previous flush. The first delta already left as its own emission,
            // and the sink appends each flush to the stored prefix, so the row
            // converges rather than duplicating.
            //
            // Re-emitting per delta would be one store write per token. Flushing
            // only when the run closes would lose the reasoning entirely if the
            // host died mid-thought, which is the failure the incremental trace
            // exists to prevent. So it flushes on a threshold and again at close.
            AgentProgress::ThinkingDelta { delta, .. } => {
                if !self.deep {
                    return Vec::new();
                }
                let Some((seq, buf)) = self.thinking_buf.as_mut() else {
                    return Vec::new();
                };
                let seq = *seq;
                buf.push_str(delta);
                self.thinking_pending_bytes =
                    self.thinking_pending_bytes.saturating_add(delta.len());
                if self.thinking_pending_bytes < DEEP_THINK_FLUSH_BYTES {
                    return Vec::new();
                }
                self.thinking_pending_bytes = 0;
                let reasoning = std::mem::take(buf);
                vec![(
                    seq,
                    TurnStep {
                        kind: TurnStepKind::Thinking,
                        status: TurnStepStatus::Ok,
                        label: "Thinking".to_string(),
                        ..TurnStep::default()
                    },
                    Some(crate::ports::deep_trace::bound_detail(TurnStepDetail {
                        reasoning: Some(reasoning),
                        ..TurnStepDetail::default()
                    })),
                )]
            }
            // Visible assistant text closes a thinking run without a step of its
            // own; everything else contributes nothing and does not break the
            // coalescing. Both match `fold_steps`.
            AgentProgress::TextDelta { .. } => {
                let closing = self.close_thinking();
                self.thinking_open = false;
                closing.into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Finalizes an open thinking run, yielding its accumulated reasoning under
    /// the ordinal the run already claimed.
    ///
    /// Called wherever a run ends — visible text, or the next tool call. Returns
    /// `None` when nothing is open, when this trace is not deep, or when the run
    /// accumulated no text.
    fn close_thinking(&mut self) -> Option<(u32, TurnStep, Option<TurnStepDetail>)> {
        let (seq, buf) = self.thinking_buf.take()?;
        self.thinking_pending_bytes = 0;
        if buf.is_empty() {
            return None;
        }
        Some((
            seq,
            TurnStep {
                kind: TurnStepKind::Thinking,
                status: TurnStepStatus::Ok,
                label: "Thinking".to_string(),
                ..TurnStep::default()
            },
            Some(crate::ports::deep_trace::bound_detail(TurnStepDetail {
                reasoning: Some(buf),
                ..TurnStepDetail::default()
            })),
        ))
    }

    /// Flushes a thinking run that has no event left to close it.
    ///
    /// [`close_thinking`](Self::close_thinking) is driven by the stream —
    /// visible text or a tool call. A turn that *ends* mid-thought — a reply,
    /// an abort, an error — has neither, so the tail accumulated below
    /// [`DEEP_THINK_FLUSH_BYTES`] would sit in `thinking_buf` forever and the
    /// stored deep trace would record only the first delta plus any full
    /// threshold chunks. The collector calls this when the stream drains, so
    /// precisely the failed/interrupted turns worth diagnosing keep their
    /// closing reasoning. No-op when nothing is open or the run said nothing.
    pub(crate) fn finish(&mut self) -> Vec<(u32, TurnStep, Option<TurnStepDetail>)> {
        let closing = self.close_thinking();
        self.thinking_open = false;
        closing.into_iter().collect()
    }

    /// How many ordinals have been handed out. Test-only: the sink tracks what
    /// actually landed in the store, which is not the same number when a write
    /// fails or the cap bites.
    #[cfg(test)]
    pub(crate) fn emitted(&self) -> u32 {
        self.next
    }

    /// Takes the next ordinal.
    fn claim(&mut self) -> u32 {
        let seq = self.next;
        self.next = self.next.saturating_add(1);
        seq
    }
}

/// Map one live [`AgentProgress`] event to a scrubbed [`TurnStreamEvent`] for
/// the transient [`turn_stream`](crate::turn_stream) bus, or `None` for events
/// with no operator-facing live frame (text/thinking/args deltas, iteration and
/// cost updates, sub-agent lifecycle, turn markers).
///
/// This is the live counterpart of [`fold_steps`] and shares its exact
/// scrubbing helpers ([`label_for`], [`enrich_detail`], [`error_detail`]), so
/// the live stream carries the identical no-raw-arguments / no-raw-output
/// projection the final folded timeline does — the two views can never disagree
/// and neither can leak. `seq` is the caller's monotonic per-turn counter, for
/// client-side ordering/dedup.
pub(crate) fn stream_event_from(
    event: &AgentProgress,
    seq: u64,
    thinking_open: &mut bool,
) -> Option<TurnStreamEvent> {
    match event {
        AgentProgress::ToolCallStarted {
            call_id,
            tool_name,
            display_label,
            ..
        } => {
            *thinking_open = false;
            Some(TurnStreamEvent {
                kind: "tool_call",
                seq,
                tool_call_id: Some(call_id.clone()),
                label: Some(label_for(display_label.clone(), tool_name)),
                status: Some("running"),
                ..TurnStreamEvent::default()
            })
        }
        AgentProgress::ToolCallCompleted {
            call_id,
            tool_name,
            success,
            output,
            arguments,
            elapsed_ms,
            failure,
            ..
        } => {
            *thinking_open = false;
            let done = complete(
                tool_name,
                *success,
                output,
                arguments.as_ref(),
                failure.as_ref(),
            );
            Some(TurnStreamEvent {
                kind: "tool_result",
                seq,
                tool_call_id: Some(call_id.clone()),
                // A label so a completion with no observed start still renders;
                // the common case pairs by `tool_call_id` and keeps the running
                // row's richer label.
                label: Some(humanize(tool_name)),
                detail: done.detail,
                result: done.result,
                failure: done.failure,
                truncated: done.truncated,
                status: Some(done.status.wire_word()),
                elapsed_ms: Some(*elapsed_ms),
                ..TurnStreamEvent::default()
            })
        }
        // The first thinking delta of a run opens ONE coalesced "Thinking" frame,
        // exactly as `fold_steps` opens one "Thinking" step; consecutive deltas
        // fall through to the catch-all and emit nothing, so the live timeline
        // shows the same thinking rows the final folded one does (they were
        // otherwise missing live — the count jumped up when the reply landed).
        AgentProgress::ThinkingDelta { .. } if !*thinking_open => {
            *thinking_open = true;
            Some(TurnStreamEvent {
                kind: "thinking",
                seq,
                label: Some("Thinking".to_string()),
                status: Some("ok"),
                ..TurnStreamEvent::default()
            })
        }
        // Visible assistant text closes a thinking run (the reply is the bubble
        // body), matching `fold_steps`; it adds no step of its own.
        AgentProgress::TextDelta { .. } => {
            *thinking_open = false;
            None
        }
        _ => None,
    }
}

/// The curated step labels of one agent's tools, keyed by tool name.
///
/// # Why this exists
///
/// A tool states its own operator-facing step label through
/// [`Tool::display_label`](tinytools::Tool::display_label): the managed
/// web search calls itself "Exa web search", and a BYO belt names the provider
/// actually wired behind it ("Brave web search", "SearXNG web search").
/// **Nothing asks it.** The crate-level `ToolStarted` event carries a call id
/// and a tool *name* and nothing else, so the bridge that projects it into an
/// [`AgentProgress`] fills `display_label` with a humanized form of that name —
/// the same "Web Search" for every provider, on every tenant. The tool's own
/// answer reaches no one, and `label_for` below then faithfully renders a
/// label the tool never chose.
///
/// OpenCompany assembles the tool set ([`build_agent`](crate::harness::build)),
/// so it is the one layer that *can* answer the question the loop does not ask.
/// It captures each tool's label once, at build time, and puts it back onto the
/// event as that event enters the collector. Everything downstream is unchanged
/// and stays consistent by construction: the folded [`TurnStep`]s, the live
/// stream frames, and the durable run trace all read the one rewritten event.
///
/// # What it holds, exactly
///
/// Labels are captured with `Value::Null` arguments — which is precisely what
/// the loop itself has at `ToolStarted`, since it emits `arguments: Null` there
/// too. So this map carries exactly what a loop that *did* ask would have
/// computed at that moment, and no more: a tool that varies its label by
/// argument contributes its argument-free form, the same one the loop would
/// have gotten.
///
/// Only labels that **differ** from the humanized tool name are kept, so the map
/// holds deliberate overrides rather than a second copy of the default. An agent
/// whose tools all accept the default carries an empty map and costs nothing.
#[derive(Debug, Default, Clone)]
pub struct StepLabels(Arc<HashMap<String, String>>);

impl StepLabels {
    /// Capture the curated labels of `tools`.
    pub fn from_tools(tools: &[Box<dyn tinytools::Tool>]) -> Self {
        let curated = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.name();
                tool.display_label(&Value::Null)
                    .filter(|label| !label.trim().is_empty())
                    // A label equal to the default is not an override; keeping it
                    // would only make `apply` rewrite an event into itself.
                    .filter(|label| *label != humanize_tool_name(name))
                    .map(|label| (name.to_string(), label))
            })
            .collect();
        Self(Arc::new(curated))
    }

    /// Restore the tool's own label on a tool-call start.
    ///
    /// Every other event passes through untouched. Applied once per event, at
    /// the collector, so the three consumers of that stream cannot disagree
    /// about what a step is called.
    pub fn apply(&self, event: AgentProgress) -> AgentProgress {
        match event {
            AgentProgress::ToolCallStarted {
                call_id,
                tool_name,
                arguments,
                iteration,
                display_label,
                display_detail,
            } => {
                let display_label = self.resolve(&tool_name, display_label);
                AgentProgress::ToolCallStarted {
                    call_id,
                    tool_name,
                    arguments,
                    iteration,
                    display_label,
                    display_detail,
                }
            }
            // A sub-agent's registry is a filtered view of this same parent tool
            // set (openhuman's sub-agent runner narrows it per archetype), so the
            // one map is correct for both scopes. `fold_steps` renders only
            // parent-scope rows today; the run trace and any later nested view
            // read the same corrected event rather than a stale humanized one.
            AgentProgress::SubagentToolCallStarted {
                agent_id,
                task_id,
                call_id,
                tool_name,
                arguments,
                iteration,
                display_label,
                display_detail,
            } => {
                let display_label = self.resolve(&tool_name, display_label);
                AgentProgress::SubagentToolCallStarted {
                    agent_id,
                    task_id,
                    call_id,
                    tool_name,
                    arguments,
                    iteration,
                    display_label,
                    display_detail,
                }
            }
            other => other,
        }
    }

    /// The label to carry for `tool_name`, given what the loop supplied.
    ///
    /// A curated label replaces the loop's *default* — the humanized tool name,
    /// or nothing at all. It does **not** replace a label the loop actually
    /// chose for this call: the unknown-tool row reads "`<name>` (unavailable)",
    /// and a loop that one day computes a real per-call label should outrank a
    /// build-time snapshot. Deferring is safe in both directions, because a name
    /// the loop labels specially is either absent from this map (it never
    /// registered as a tool) or better described by the call than by the
    /// registry.
    fn resolve(&self, tool_name: &str, from_loop: Option<String>) -> Option<String> {
        let Some(curated) = self.0.get(tool_name) else {
            return from_loop;
        };
        let loop_chose_it = from_loop.as_deref().is_some_and(|label| {
            !label.trim().is_empty() && label != humanize_tool_name(tool_name)
        });
        if loop_chose_it {
            from_loop
        } else {
            Some(curated.clone())
        }
    }
}

/// The label for a tool step: the server-computed `display_label` when it is a
/// non-blank string, else a humanized form of the tool name. Never derived from
/// arguments or output.
fn label_for(display_label: Option<String>, tool_name: &str) -> String {
    display_label
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| humanize(tool_name))
}

/// Turn a `snake_case` / `kebab-case` tool name into a short human label
/// ("mcp_call_tool" → "Mcp call tool"). Structural only — the input is a tool
/// identifier, never user/remote text.
fn humanize(tool_name: &str) -> String {
    let spaced = tool_name.replace(['_', '-'], " ");
    let trimmed = spaced.trim();
    if trimmed.is_empty() {
        return "Tool".to_string();
    }
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Tool".to_string(),
    }
}

/// The tools whose own `ToolResult` message is surfaced verbatim (bounded by
/// [`RESULT_MAX`]) instead of being collapsed to a failure class.
///
/// **Membership rule** — a name belongs here when BOTH hold:
///
/// 1. The message is **OC-authored, operator-facing copy** — the tool wrote the
///    sentence itself (e.g. "a workflow needs exactly one trigger"), rather than
///    relaying a remote or untrusted body. Contrast `mcp_call_tool`, whose
///    output is a remote server's body and therefore never leaves this module
///    as content.
/// 2. The message is **free of host paths and raw store errors** on every one of
///    the tool's failure exits. This half is not a formality: what lands here is
///    shown on the console step timeline AND written into the persisted turn
///    trace, so a `{e}` interpolation of
///    [`OpenCompanyError`](crate::error::OpenCompanyError) — whose `StoreIo`
///    Display embeds an absolute host path — would publish the host's
///    filesystem layout the moment its tool joined this list.
///
/// Rule 2 was implicit while the list held only orchestrator tools, and issue
/// #887 is what made it load-bearing: the whole workspace family interpolated
/// the store error, so each tool's exits had to be audited and sanitised
/// (`workspace_tools::store_reason`) BEFORE its name was added here. Adding a
/// name is therefore an audit of that tool's exits, not a one-line edit.
///
/// The orchestrator half must stay in lockstep with
/// [`orchestrator_tools`](crate::harness::orchestrator::orchestrator_tools); the
/// workspace half with
/// [`workspace_tools`](crate::harness::workspace_tools::workspace_tools). Both
/// are pinned mechanically by `intrinsic_tools_covers_every_oc_authored_tool`.
const INTRINSIC_TOOLS: &[&str] = &[
    // The exact confirmation that the explicit request was queued is useful
    // operator-facing state, not remote content to collapse.
    crate::harness::approval_tool::REQUEST_APPROVAL_TOOL,
    "query_company",
    "spawn_task",
    "delegate_to_desk",
    // #884's sibling of `delegate_to_desk`. Its refusals are the same shape —
    // whole sentences naming the teammates the caller may actually reach — and
    // one collapsed to a bare failure class is a refusal the agent cannot act on.
    "delegate_to_teammate",
    "run_workflow",
    // #418's `run_workflow` companion — its full-output pages are the same kind
    // of OC-authored, agent-facing text as the other intrinsics, safe to surface
    // verbatim rather than collapsed to a class.
    "read_run_output",
    "create_workflow",
    // #661 (M7)'s trio, and they need this more than most: their refusals are
    // whole sentences telling the agent what to do instead ("read it first",
    // "that one is the operator's to change"), and a refusal collapsed to a
    // bare failure class is a refusal the agent cannot act on.
    "read_workflow",
    "update_workflow",
    "delete_workflow",
    "add_agent",
    // #186's pair. Missing here until #461 noticed the drift, so an
    // `assign_task` / `review_task` refusal ("agent is not on the roster")
    // rendered as a bare class instead of the sentence the tool wrote.
    "assign_task",
    "review_task",
    // #887's family. `workspace_read` is the one the issue was filed against —
    // it writes five different sentences for its five failure exits and the
    // operator saw the same catch-all for all of them, which is precisely why
    // the underlying fault could not be diagnosed. The other six are here on
    // the same audit: every exit is a sentence the tool wrote, and since the
    // sanitisation commit none of them carries a host path or a raw store
    // error.
    "workspace_list",
    "workspace_read",
    "workspace_search",
    "workspace_create",
    "workspace_write",
    "workspace_rename",
    "workspace_delete",
];

/// Bound on an OC-authored message surfaced as a step result.
const RESULT_MAX: usize = 200;

/// Bound on the whole rendered argument line.
const DETAIL_MAX: usize = 140;

/// Bound on one rendered argument *value*, so a single long field cannot
/// crowd out the fields that distinguish this call from the last one.
const ARG_VALUE_MAX: usize = 48;

/// How many of an argument object's fields are rendered before the rest become
/// a `+N more` tail. Enough to tell two calls apart; not a payload dump.
const MAX_ARG_FIELDS: usize = 4;

/// How deep the argument renderer descends. One level, so an MCP call's nested
/// remote arguments render as fields rather than as `{3 fields}`, and nothing
/// below that is walked.
const MAX_ARG_DEPTH: usize = 1;

/// The needle that identifies OpenHuman's approval-gate refusal, taken from the
/// `PolicyDenial::ApprovalRequired` render in
/// `vendor/openhuman/crates/openhuman-core/src/agent/tinyagents/policy_denial.rs`.
///
/// This is a string classifier, which is the anti-pattern — the mitigation is
/// that `approval_needle_still_appears_in_the_vendored_denial_render` reads
/// that file and fails if the wording drifts, so a rename turns CI red instead
/// of silently returning every parked call to reading as a crash.
const APPROVAL_REQUIRED_NEEDLE: &str = "requires approval under policy";

/// What the operator is told when a call is parked. The most actionable line in
/// the timeline: nothing is broken, and the next move is theirs.
const AWAITING_APPROVAL_RESULT: &str = "Parked — waiting on your approval before it can run.";

/// Markers the OpenHuman tool pipeline stamps into a result it **cut**, from
/// the places that cut one and say so:
///
/// * the shared byte budget (`tool_result_artifacts/mod.rs`),
/// * the artifact envelope that replaces an oversized result with a preview
///   plus a pointer (same file).
///
/// `truncated by tool cap:` is kept though the vendored source no longer
/// produces it — upstream dropped that wording moving to uncapped tool
/// summaries (`6865c81eb`). It stays because this classifies **results**, not
/// source: a persisted trace or a reply captured before that change still
/// carries the phrase, and a classifier that forgets it would silently
/// re-label old cut output as whole. It costs one `contains` per result.
///
/// Same string-classifier caveat, same mitigation:
/// `truncation_markers_still_appear_in_the_vendored_tool_pipeline` reads both
/// vendored files and fails on drift.
const TRUNCATION_MARKERS: &[&str] = &[
    "truncated by tool cap:",
    "truncated by tool_result_budget",
    "[tool_result_preview]",
];

/// Everything a completed tool call contributes to its step, resolved once and
/// applied identically to the folded timeline, the persisted trace, and the
/// live stream — so the three views can never tell an operator different
/// stories about the same call.
struct Completed {
    status: TurnStepStatus,
    detail: Option<String>,
    result: Option<String>,
    failure: Option<TurnStepFailure>,
    truncated: bool,
}

impl Completed {
    /// Stamps this outcome onto a step, leaving `kind`, `label` and
    /// `elapsed_ms` — which the caller owns — untouched.
    fn apply(self, step: &mut TurnStep) {
        step.status = self.status;
        step.detail = self.detail;
        step.result = self.result;
        step.failure = self.failure;
        step.truncated = self.truncated;
    }
}

/// Resolve one `ToolCallCompleted` into its status, its typed failure, what it
/// was doing, and what came back.
fn complete(
    tool_name: &str,
    success: bool,
    output: &str,
    arguments: Option<&Value>,
    failure: Option<&ClassifiedFailure>,
) -> Completed {
    let detail = describe_call(tool_name, arguments);

    if success {
        return Completed {
            status: TurnStepStatus::Ok,
            detail,
            result: summarize_result(tool_name, output),
            failure: None,
            // Only claimed on a success. "The result was cut" is a statement
            // about a result; a failure's output is an error message, and
            // reporting a clipped error as a truncated answer would be a
            // different, false claim.
            truncated: output_was_truncated(output),
        };
    }

    // A gated call is not a failure. Checked before classification because the
    // classifier has no arm for it and lands it on `Unknown` — which is the
    // literal "Something went wrong with this action." this issue is about.
    if is_awaiting_approval(output) {
        return Completed {
            status: TurnStepStatus::AwaitingApproval,
            detail,
            result: Some(AWAITING_APPROVAL_RESULT.to_string()),
            failure: None,
            truncated: false,
        };
    }

    // The classifier is the taxonomy. When the harness already ran it, reuse
    // its verdict; when it did not, run it here rather than falling back to the
    // coarse `sanitize_tool_output` class string, which could only ever say
    // "failed (error)".
    let classified = match failure {
        Some(f) => f.clone(),
        None => oh::tools::status::classify(output, false),
    };

    let (failure, cause_override) = refine_missing_app(tool_name, &classified);

    Completed {
        status: TurnStepStatus::Error,
        detail,
        result: failure_result(tool_name, output, &classified, cause_override),
        failure: Some(failure),
        truncated: false,
    }
}

/// Map OpenHuman's failure class onto the operator-facing vocabulary.
///
/// Exhaustive on purpose: a class added upstream fails to compile here rather
/// than silently folding into "something went wrong", which is the exact
/// regression this issue is fixing.
fn failure_of(class: ToolFailureClass) -> TurnStepFailure {
    match class {
        ToolFailureClass::Denied | ToolFailureClass::ApprovalExpired => TurnStepFailure::Declined,
        ToolFailureClass::BlockedByPolicy => TurnStepFailure::BlockedByPolicy,
        ToolFailureClass::BadCredentials => TurnStepFailure::Unauthorized,
        ToolFailureClass::MissingPermission => TurnStepFailure::MissingPermission,
        ToolFailureClass::MissingApp => TurnStepFailure::MissingApp,
        ToolFailureClass::NotFound => TurnStepFailure::NotFound,
        ToolFailureClass::Unsupported => TurnStepFailure::Unsupported,
        ToolFailureClass::Timeout => TurnStepFailure::Timeout,
        ToolFailureClass::ServiceUnavailable | ToolFailureClass::ModelConnection => {
            TurnStepFailure::Unavailable
        }
        ToolFailureClass::Unknown => TurnStepFailure::Failed,
    }
}

/// Tools that resolve a caller-supplied path **in this process** and can never
/// invoke an external program.
///
/// The list exists because one operating-system error means two different
/// things. `ENOENT` — "No such file or directory (os error 2)" — is what the
/// kernel says both when a binary you tried to spawn is not installed and when
/// a file you tried to read is not there. Upstream's classifier sees only the
/// message text, so it routes every `ENOENT` to
/// [`ToolFailureClass::MissingApp`], whose remediation copy is "Install or open
/// the app, then try again."
///
/// For a tool on this list there is no app to install: it opens a path and
/// returns bytes. So its `ENOENT` is re-read as
/// [`TurnStepFailure::NotFound`] here (issue #924, where `grep` on a company
/// note path and `read_skill_resource` on an absent `references/` file both
/// rendered as "App unavailable" on a server tenant with nothing installable).
///
/// **Keyed on the tool, not on the message.** Six of upstream's seven
/// `MissingApp` needles ("command not found", "executable not found", …) name a
/// program unambiguously; only the bare `ENOENT` string is shared. But sniffing
/// for those needles cannot separate the cases either, because
/// `Command::new(program)` on a missing binary yields the bare `ENOENT` with
/// none of them — a genuinely missing `git` would then be relabelled a missing
/// file. Which tool ran is the signal that actually distinguishes them.
///
/// **Under-inclusive by design.** A path tool missing from this list keeps
/// today's behaviour rather than gaining a new wrong one, so the failure mode of
/// drift is a stale label, never a false `NotFound` on a real missing program.
/// [`every_path_tool_on_the_belt_is_listed`] fails when the belt grows one this
/// list does not name.
const PATH_ONLY_TOOLS: &[&str] = &[
    "file_read",
    "file_write",
    "edit",
    "list",
    "glob",
    "grep",
    crate::harness::skills::READ_SKILL_RESOURCE_TOOL,
];

/// Whether `tool_name` reads a path in-process, per [`PATH_ONLY_TOOLS`].
fn is_path_only_tool(tool_name: &str) -> bool {
    PATH_ONLY_TOOLS.contains(&tool_name)
}

/// Re-read a `MissingApp` verdict that came from a path tool as `NotFound`.
///
/// Returns the class to report and the plain-language cause to show, so the
/// label and the sentence beside it can never disagree. Every other verdict is
/// returned untouched — this narrows one misrouted class, it does not
/// re-classify.
fn refine_missing_app<'a>(
    tool_name: &str,
    classified: &'a ClassifiedFailure,
) -> (TurnStepFailure, Option<&'a str>) {
    if matches!(classified.class, ToolFailureClass::MissingApp) && is_path_only_tool(tool_name) {
        return (
            TurnStepFailure::NotFound,
            Some("The file or folder this action asked for does not exist."),
        );
    }
    (failure_of(classified.class), None)
}

/// Whether the model was handed OpenHuman's *approval-required* refusal by
/// **our** policy.
///
/// Both needles are required. The vendored phrase alone would also match a
/// different `ToolPolicy` on the same host; the policy name alone appears in
/// every denial our policy issues, including hard denies. Together they mean
/// precisely "OpenCompany's approval gate parked this call", which is the only
/// thing that may claim [`TurnStepStatus::AwaitingApproval`].
fn is_awaiting_approval(output: &str) -> bool {
    output.contains(APPROVAL_REQUIRED_NEEDLE) && output.contains(POLICY_NAME)
}

/// Whether the harness cut this result before the agent could read all of it.
fn output_was_truncated(output: &str) -> bool {
    TRUNCATION_MARKERS
        .iter()
        .any(|marker| output.contains(marker))
}

/// The plain-language cause for a failed call.
///
/// An intrinsic tool's own message wins — it is OC-authored and names the real
/// problem ("a workflow needs exactly one trigger"), where the classifier can
/// only offer a category. Everything else gets the classifier's `cause_plain`,
/// never the raw output.
///
/// `cause_override` replaces `cause_plain` when this crate re-read the upstream
/// verdict ([`refine_missing_app`]); it ranks below an intrinsic tool's own
/// message for the same reason `cause_plain` does.
fn failure_result(
    tool_name: &str,
    output: &str,
    classified: &ClassifiedFailure,
    cause_override: Option<&str>,
) -> Option<String> {
    if INTRINSIC_TOOLS.contains(&tool_name) {
        let message = output.trim();
        if !message.is_empty() {
            return Some(truncate(message, RESULT_MAX));
        }
    }
    if let Some(cause) = cause_override {
        return Some(cause.to_string());
    }
    let cause = classified.cause_plain.trim();
    (!cause.is_empty()).then(|| cause.to_string())
}

/// **What the step was doing**: the call's arguments, redacted by #372's
/// host-side redactor and rendered as one bounded line.
///
/// `None` when the call took no arguments, or when nothing survived rendering —
/// a step with nothing to say says nothing rather than an empty dash.
fn describe_call(tool_name: &str, arguments: Option<&Value>) -> Option<String> {
    let args = arguments?;
    if matches!(args, Value::Null) {
        return None;
    }
    // Redact FIRST. Everything below only ever reads the redacted copy, so no
    // rendering path can reach around the denylist.
    let redacted = approval_display::redact(args);

    // `mcp_call_tool` is reshaped rather than rendered flat: its own fields
    // name the remote tool (`brave · search`) and its `arguments` field holds
    // the call that actually distinguishes one invocation from the next. Flat
    // rendering would spend the whole line on the routing fields and show the
    // interesting one as `{2 fields}`.
    if tool_name == "mcp_call_tool" {
        let head = match (
            redacted.get("server").and_then(Value::as_str),
            redacted.get("tool").and_then(Value::as_str),
        ) {
            (Some(server), Some(tool)) => Some(format!("{server} · {tool}")),
            (Some(server), None) => Some(server.to_string()),
            (None, Some(tool)) => Some(tool.to_string()),
            (None, None) => None,
        };
        let nested = redacted
            .get("arguments")
            .and_then(|value| render_value(value, 0));
        let line = match (head, nested) {
            (Some(head), Some(nested)) => format!("{head} — {nested}"),
            (Some(head), None) => head,
            (None, Some(nested)) => nested,
            (None, None) => return None,
        };
        return Some(truncate(&line, DETAIL_MAX));
    }

    render_value(&redacted, 0).map(|line| truncate(&line, DETAIL_MAX))
}

/// Render an **already-redacted** value as one compact line, or `None` when it
/// carries nothing worth showing.
fn render_value(value: &Value, depth: usize) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => {
            // Collapsed to one line first, then bounded: a multi-line argument
            // must not break the row it is rendered into.
            let flat = text.replace(['\n', '\r', '\t'], " ");
            let trimmed = flat.trim();
            (!trimmed.is_empty()).then(|| truncate(trimmed, ARG_VALUE_MAX))
        }
        Value::Array(items) => Some(count_of(items.len(), "item")),
        Value::Object(map) => {
            if map.is_empty() {
                return None;
            }
            if depth >= MAX_ARG_DEPTH {
                return Some(count_of(map.len(), "field"));
            }
            let mut parts = Vec::new();
            for (key, item) in map.iter().take(MAX_ARG_FIELDS) {
                if let Some(rendered) = render_value(item, depth + 1) {
                    parts.push(format!("{key}={rendered}"));
                }
            }
            if parts.is_empty() {
                return None;
            }
            let omitted = map.len().saturating_sub(MAX_ARG_FIELDS);
            if omitted > 0 {
                parts.push(format!("+{omitted} more"));
            }
            Some(parts.join(" · "))
        }
    }
}

/// **What came back**, on a success.
///
/// An intrinsic OpenCompany tool's output is OC-authored copy and is surfaced
/// bounded. Every other tool's output is a remote body: only its *shape* is
/// reported, never its content.
fn summarize_result(tool_name: &str, output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if INTRINSIC_TOOLS.contains(&tool_name) {
        return Some(truncate(trimmed, RESULT_MAX));
    }
    Some(shape_of(trimmed))
}

/// A content-free description of how much came back: a count when the body is a
/// JSON collection (the shape that answers "how far did it get"), else a
/// character size.
fn shape_of(output: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        match value {
            Value::Array(items) => return count_of(items.len(), "item"),
            Value::Object(map) if !map.is_empty() => return count_of(map.len(), "field"),
            _ => {}
        }
    }
    let chars = output.chars().count();
    if chars < 1_000 {
        count_of(chars, "character")
    } else {
        format!("{:.1}k characters", chars as f64 / 1_000.0)
    }
}

/// `"1 item"` / `"12 items"` — pluralised counting used by both the argument
/// renderer and the result shape, so the two read alike.
fn count_of(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// UTF-8-safe truncation to at most `max` chars, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[path = "steps/steps_fixtures_tests.rs"]
mod steps_fixtures_tests;
#[cfg(test)]
#[path = "steps/steps_bus_deep_trace_tests.rs"]
mod tests_bus_deep_trace;
#[cfg(test)]
#[path = "steps/steps_execution_tests.rs"]
mod tests_execution;
#[cfg(test)]
#[path = "steps/steps_fold_tests.rs"]
mod tests_fold;
#[cfg(test)]
#[path = "steps/steps_trace_security_tests.rs"]
mod tests_trace_security;
