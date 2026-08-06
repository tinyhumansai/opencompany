//! Fold the harness progress stream into the scrubbed [`TurnStep`] timeline
//! surfaced in operator chat.
//!
//! During [`Agent::turn`](openhuman_core::openhuman::agent::Agent) the tinyagents
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
//!   tool *name* — never from arguments or output.
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
//!   [`cause_plain`](oh::tool_status::ClassifiedFailure::cause_plain), or an
//!   intrinsic tool's own message — never the remote error text.
//!
//! The unit test `planted_secret_never_reaches_serialized_steps` proves it end
//! to end: a secret planted in a tool's output, its nested arguments, and its
//! `display_detail` appears in **no** serialized step.
//!
//! Steps must also never enter the memory store — `memory_loop::outcome_chunk`
//! stays text-only — so a scrubbed detail can never be re-retrieved and
//! re-injected into a later turn.

use openhuman_core::openhuman as oh;
use serde_json::Value;

use oh::agent::progress::AgentProgress;
use oh::tool_status::{ClassifiedFailure, ToolFailureClass};

use crate::harness::policy::POLICY_NAME;
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
/// dense and unique across the run rather than restarting per turn and
/// overwriting earlier rows.
#[derive(Debug, Default)]
pub(crate) struct StepTrace {
    /// The next ordinal to hand out. Also the number of steps yielded so far.
    next: u32,
    /// `call_id` → the ordinal and label its start claimed, so the completion
    /// finalizes that row keeping the richer start-time label.
    running: std::collections::HashMap<String, (u32, String)>,
    /// Whether the most recent step is an open "Thinking" run.
    thinking_open: bool,
}

impl StepTrace {
    /// Feeds one progress event, yielding the ordinal + step to persist when the
    /// event maps to one.
    pub(crate) fn push(&mut self, event: &AgentProgress) -> Option<(u32, TurnStep)> {
        match event {
            AgentProgress::ToolCallStarted {
                call_id,
                tool_name,
                display_label,
                ..
            } => {
                self.thinking_open = false;
                let label = label_for(display_label.clone(), tool_name);
                let seq = self.claim();
                self.running.insert(call_id.clone(), (seq, label.clone()));
                Some((
                    seq,
                    TurnStep {
                        kind: TurnStepKind::ToolCall,
                        status: TurnStepStatus::Running,
                        label,
                        ..TurnStep::default()
                    },
                ))
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
                let (seq, label) = match self.running.remove(call_id) {
                    Some(found) => found,
                    // No observed start — surface it standalone, exactly as the
                    // fold does.
                    None => (self.claim(), humanize(tool_name)),
                };
                let mut step = TurnStep {
                    kind: TurnStepKind::ToolCall,
                    label,
                    elapsed_ms: Some(*elapsed_ms),
                    ..TurnStep::default()
                };
                done.apply(&mut step);
                Some((seq, step))
            }
            AgentProgress::ThinkingDelta { .. } if !self.thinking_open => {
                self.thinking_open = true;
                Some((
                    self.claim(),
                    TurnStep {
                        kind: TurnStepKind::Thinking,
                        status: TurnStepStatus::Ok,
                        label: "Thinking".to_string(),
                        ..TurnStep::default()
                    },
                ))
            }
            // Visible assistant text closes a thinking run without a step of its
            // own; everything else contributes nothing and does not break the
            // coalescing. Both match `fold_steps`.
            AgentProgress::TextDelta { .. } => {
                self.thinking_open = false;
                None
            }
            _ => None,
        }
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

/// OpenCompany's own orchestrator tools (issue #112 et al). Their output is
/// **OC-authored, operator-facing copy** — the tool's own `ToolResult` message
/// (e.g. "workflow needs exactly one trigger"), not a remote or untrusted body
/// — so it is safe *and* useful to surface verbatim (bounded) rather than
/// collapsing it to a class or a shape. Contrast `mcp_call_tool`, whose output
/// is a remote server body and therefore never leaves this module as content.
/// Keep this list in lockstep with
/// [`orchestrator_tools`](crate::harness::orchestrator::orchestrator_tools).
const INTRINSIC_TOOLS: &[&str] = &[
    "query_company",
    "spawn_task",
    "delegate_to_desk",
    "run_workflow",
    "create_workflow",
    "add_agent",
    // #186's pair. Missing here until #461 noticed the drift, so an
    // `assign_task` / `review_task` refusal ("agent is not on the roster")
    // rendered as a bare class instead of the sentence the tool wrote.
    "assign_task",
    "review_task",
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
/// `vendor/openhuman/src/openhuman/tinyagents/policy_denial.rs`.
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
/// the three places that can cut one:
///
/// * the per-tool char cap (`middleware.rs`),
/// * the shared byte budget (`tool_result_artifacts/mod.rs`),
/// * the artifact envelope that replaces an oversized result with a preview
///   plus a pointer (same file).
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
        None => oh::tool_status::classify(output, false),
    };

    Completed {
        status: TurnStepStatus::Error,
        detail,
        result: failure_result(tool_name, output, &classified),
        failure: Some(failure_of(classified.class)),
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
        ToolFailureClass::Timeout => TurnStepFailure::Timeout,
        ToolFailureClass::ServiceUnavailable | ToolFailureClass::ModelConnection => {
            TurnStepFailure::Unavailable
        }
        ToolFailureClass::Unknown => TurnStepFailure::Failed,
    }
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
fn failure_result(tool_name: &str, output: &str, classified: &ClassifiedFailure) -> Option<String> {
    if INTRINSIC_TOOLS.contains(&tool_name) {
        let message = output.trim();
        if !message.is_empty() {
            return Some(truncate(message, RESULT_MAX));
        }
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
mod tests {
    use super::*;
    use oh::tool_status::FailureCategory;

    /// An obvious fake, in the same shape `approval_display`'s tests use. Never
    /// a credential pattern that could be mistaken for a real one in a diff.
    const FAKE_SECRET: &str = "NOT-A-REAL-KEY-planted-for-tests";

    /// The refusal OpenHuman hands the model when *our* approval policy parks a
    /// call, reproduced in the shape `PolicyDenial::ApprovalRequired::render`
    /// produces. `PolicyDenial` is crate-private upstream, so this is a copy —
    /// which is exactly why
    /// [`approval_needle_still_appears_in_the_vendored_denial_render`] pins the
    /// needle against the real source.
    fn approval_refusal(tool: &str) -> String {
        format!(
            "Blocked: Tool '{tool}' requires approval under policy '{POLICY_NAME}'. \
             Reason: '{tool}' has an external effect and this company runs supervised. \
             Workaround: Ask the user to approve this action, then retry. \
             Relay this to the user: explain what was blocked and why."
        )
    }

    /// The lockstep [`INTRINSIC_TOOLS`] claims, made mechanical. Every
    /// orchestrator tool name is a `pub const`, so drift between the two lists
    /// now fails here instead of silently downgrading a tool's own sentence to
    /// a bare failure class (which is how `assign_task` / `review_task` sat
    /// missing from #186 until #461).
    #[test]
    fn intrinsic_tools_covers_every_orchestrator_tool() {
        use crate::harness::orchestrator::{
            ADD_AGENT_TOOL, ASSIGN_TASK_TOOL, CREATE_WORKFLOW_TOOL, QUERY_COMPANY_TOOL,
            REVIEW_TASK_TOOL, RUN_WORKFLOW_TOOL,
        };
        use crate::runtime::delegation_tools::{DELEGATE_TO_DESK_TOOL, SPAWN_TASK_TOOL};

        let expected = [
            QUERY_COMPANY_TOOL,
            SPAWN_TASK_TOOL,
            DELEGATE_TO_DESK_TOOL,
            RUN_WORKFLOW_TOOL,
            CREATE_WORKFLOW_TOOL,
            ADD_AGENT_TOOL,
            ASSIGN_TASK_TOOL,
            REVIEW_TASK_TOOL,
        ];
        for name in expected {
            assert!(
                INTRINSIC_TOOLS.contains(&name),
                "{name} is wired by `orchestrator_tools` but absent from INTRINSIC_TOOLS"
            );
        }
        // Exact, not just covering: a name here that no longer exists upstream
        // would surface a stale tool's output as OC-authored copy.
        assert_eq!(INTRINSIC_TOOLS.len(), expected.len(), "{INTRINSIC_TOOLS:?}");
    }

    /// Reads a file out of the vendored OpenHuman checkout, for the tests that
    /// couple a string needle to the source that produces it.
    fn vendored(relative: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("vendored source {} is unreadable: {err}", path.display()))
    }

    fn started(call_id: &str, tool: &str, label: Option<&str>) -> AgentProgress {
        AgentProgress::ToolCallStarted {
            call_id: call_id.to_string(),
            tool_name: tool.to_string(),
            // The tinyagents path sends Null here; mirror that.
            arguments: Value::Null,
            iteration: 1,
            display_label: label.map(str::to_string),
            display_detail: None,
        }
    }

    fn completed(
        call_id: &str,
        tool: &str,
        success: bool,
        output: &str,
        arguments: Option<Value>,
        failure: Option<ClassifiedFailure>,
    ) -> AgentProgress {
        AgentProgress::ToolCallCompleted {
            call_id: call_id.to_string(),
            tool_name: tool.to_string(),
            success,
            output_chars: output.chars().count(),
            output: output.to_string(),
            arguments,
            elapsed_ms: 42,
            iteration: 1,
            failure,
        }
    }

    fn thinking(delta: &str) -> AgentProgress {
        AgentProgress::ThinkingDelta {
            delta: delta.to_string(),
            iteration: 1,
        }
    }

    fn text(delta: &str) -> AgentProgress {
        AgentProgress::TextDelta {
            delta: delta.to_string(),
            iteration: 1,
        }
    }

    fn classified(class: ToolFailureClass, cause: &str) -> ClassifiedFailure {
        ClassifiedFailure {
            class,
            category: FailureCategory::Recoverable,
            cause_plain: cause.to_string(),
            next_action: "try again".to_string(),
            recoverable: true,
        }
    }

    /// Folds a single completed call and hands back its step.
    fn one(tool: &str, success: bool, output: &str, arguments: Option<Value>) -> TurnStep {
        let steps = fold_steps(vec![completed(
            "c1", tool, success, output, arguments, None,
        )]);
        assert_eq!(steps.len(), 1, "expected exactly one step: {steps:?}");
        steps.into_iter().next().expect("a step")
    }

    // -----------------------------------------------------------------------
    // Shape of the fold (unchanged by #411)
    // -----------------------------------------------------------------------

    #[test]
    fn pairs_started_and_completed_into_one_step() {
        let steps = fold_steps(vec![
            started("c1", "mcp_call_tool", Some("Searching the web")),
            completed(
                "c1",
                "mcp_call_tool",
                true,
                "ok",
                Some(serde_json::json!({"server": "brave", "tool": "search"})),
                None,
            ),
        ]);
        assert_eq!(steps.len(), 1, "one step for the pair: {steps:?}");
        assert_eq!(steps[0].kind, TurnStepKind::ToolCall);
        assert_eq!(steps[0].status, TurnStepStatus::Ok);
        assert_eq!(steps[0].label, "Searching the web");
        assert_eq!(steps[0].detail.as_deref(), Some("brave · search"));
        assert_eq!(steps[0].elapsed_ms, Some(42));
    }

    #[test]
    fn label_falls_back_to_humanized_tool_name() {
        let steps = fold_steps(vec![
            started("c1", "spawn_task", None),
            completed("c1", "spawn_task", true, "ok", None, None),
        ]);
        assert_eq!(steps[0].label, "Spawn task");
    }

    #[test]
    fn consecutive_thinking_coalesces_but_text_between_splits() {
        let steps = fold_steps(vec![
            thinking("let"),
            thinking(" me"),
            thinking(" think"),
            text("Here"),
            thinking("more"),
            thinking(" thought"),
        ]);
        let thinking_steps: Vec<_> = steps
            .iter()
            .filter(|s| s.kind == TurnStepKind::Thinking)
            .collect();
        assert_eq!(
            thinking_steps.len(),
            2,
            "two runs (split by the text delta): {steps:?}"
        );
        assert!(thinking_steps.iter().all(|s| s.label == "Thinking"));
        assert!(thinking_steps.iter().all(|s| s.detail.is_none()));
    }

    #[test]
    fn unmatched_started_stays_running() {
        let steps = fold_steps(vec![started("c1", "mcp_call_tool", Some("Searching"))]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, TurnStepStatus::Running);
        assert_eq!(steps[0].elapsed_ms, None);
    }

    #[test]
    fn caps_at_fifty_with_omission_note() {
        let mut events = Vec::new();
        for i in 0..60 {
            events.push(completed(
                &format!("c{i}"),
                "spawn_task",
                true,
                "ok",
                None,
                None,
            ));
        }
        let steps = fold_steps(events);
        assert_eq!(steps.len(), MAX_STEPS + 1, "50 steps + one omission note");
        let note = steps.last().unwrap();
        assert_eq!(note.kind, TurnStepKind::Note);
        assert_eq!(note.label, "10 more steps omitted");
    }

    /// A memory-served answer runs zero steps — the tell that distinguishes it
    /// from a tool-backed one — so an empty stream folds to an empty timeline.
    #[test]
    fn empty_stream_folds_to_no_steps() {
        assert!(fold_steps(Vec::new()).is_empty());
    }

    // -----------------------------------------------------------------------
    // #411: a parked call is not a failure
    // -----------------------------------------------------------------------

    /// The headline of issue #411. A call the approval gate parked is *waiting
    /// on a person* — the single most actionable state in the timeline — and it
    /// rendered as a crash carrying "Something went wrong with this action."
    #[test]
    fn a_parked_call_says_so_and_is_not_a_failure() {
        let step = one("send_email", false, &approval_refusal("send_email"), None);

        assert_eq!(step.status, TurnStepStatus::AwaitingApproval);
        assert!(
            !step.status.is_failure(),
            "a parked call must not be counted as failed"
        );
        assert_eq!(
            step.failure, None,
            "waiting on a human is not a failure kind"
        );
        assert_eq!(step.result.as_deref(), Some(AWAITING_APPROVAL_RESULT));
        assert!(
            !step.result.as_deref().unwrap().contains("went wrong"),
            "the generic copy must be gone: {:?}",
            step.result
        );
    }

    /// The regression this replaces, stated as a property: run the *same*
    /// refusal through the classifier the old code used, and it lands on
    /// `Unknown` — i.e. on "Something went wrong with this action.". That is
    /// why the park needs its own arm ahead of classification, and this test
    /// fails the day upstream grows a real arm for it (at which point the
    /// special case can go).
    #[test]
    fn the_classifier_alone_still_cannot_recognise_a_parked_call() {
        let refusal = approval_refusal("send_email");
        assert_eq!(
            oh::tool_status::classify(&refusal, false).class,
            ToolFailureClass::Unknown,
            "if this now classifies, replace the bespoke arm in `complete` with it"
        );
    }

    /// Both needles are required. A refusal from some *other* tool policy on the
    /// same host is a genuine block, not our park, and must keep reading as one.
    #[test]
    fn only_our_own_policys_park_claims_awaiting_approval() {
        assert!(is_awaiting_approval(&approval_refusal("send_email")));
        assert!(
            !is_awaiting_approval(
                "Blocked: Tool 'shell' requires approval under policy 'some-other-policy'."
            ),
            "another policy's approval block is not our park"
        );
        assert!(
            !is_awaiting_approval(
                "Blocked: Tool 'shell' was denied by policy 'opencompany-approval'."
            ),
            "a hard deny from our policy is a failure, not a park"
        );
    }

    /// COUPLING: the needle is a string taken from a vendored render, which is
    /// the anti-pattern this pins. If OpenHuman rewords
    /// `PolicyDenial::ApprovalRequired`, this fails in CI rather than silently
    /// returning every parked call to reading as a crash.
    #[test]
    fn approval_needle_still_appears_in_the_vendored_denial_render() {
        let source = vendored("vendor/openhuman/src/openhuman/tinyagents/policy_denial.rs");
        assert!(
            source.contains(APPROVAL_REQUIRED_NEEDLE),
            "'{APPROVAL_REQUIRED_NEEDLE}' is gone from PolicyDenial::render — \
             re-derive `is_awaiting_approval` against the new wording"
        );
    }

    /// COUPLING, the other half: the policy name in the refusal is *ours*, so
    /// it is pinned to the impl that emits it rather than to a second literal.
    #[test]
    fn the_policy_name_the_classifier_keys_on_is_the_one_the_policy_reports() {
        use crate::company::Policy;
        use crate::harness::policy::ApprovalPolicy;
        use oh::agent::tool_policy::ToolPolicy;

        let policy = ApprovalPolicy::new(&Policy::default(), None);
        assert_eq!(policy.name(), POLICY_NAME);
    }

    // -----------------------------------------------------------------------
    // #411: every failure says what it is
    // -----------------------------------------------------------------------

    /// The three the issue names by hand — unauthorized, timeout, blocked — plus
    /// the rest of the taxonomy, each arriving as a typed value the console can
    /// switch on instead of prose it has to read.
    #[test]
    fn each_failure_class_maps_to_its_own_operator_facing_kind() {
        for (class, expected) in [
            (
                ToolFailureClass::BadCredentials,
                TurnStepFailure::Unauthorized,
            ),
            (ToolFailureClass::Timeout, TurnStepFailure::Timeout),
            (
                ToolFailureClass::BlockedByPolicy,
                TurnStepFailure::BlockedByPolicy,
            ),
            (ToolFailureClass::Denied, TurnStepFailure::Declined),
            (ToolFailureClass::ApprovalExpired, TurnStepFailure::Declined),
            (
                ToolFailureClass::MissingPermission,
                TurnStepFailure::MissingPermission,
            ),
            (ToolFailureClass::MissingApp, TurnStepFailure::MissingApp),
            (
                ToolFailureClass::ServiceUnavailable,
                TurnStepFailure::Unavailable,
            ),
            (
                ToolFailureClass::ModelConnection,
                TurnStepFailure::Unavailable,
            ),
            (ToolFailureClass::Unknown, TurnStepFailure::Failed),
        ] {
            assert_eq!(failure_of(class), expected, "class {class:?}");
        }
    }

    /// An unauthorized call reads as unauthorized end to end — through the fold,
    /// not just through the mapping function — and its raw body stays out.
    #[test]
    fn an_unauthorized_call_says_unauthorized() {
        let steps = fold_steps(vec![completed(
            "c1",
            "mcp_call_tool",
            false,
            &format!("401 unauthorized token={FAKE_SECRET}"),
            Some(serde_json::json!({ "server": "github", "tool": "list_issues" })),
            None,
        )]);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert_eq!(steps[0].failure, Some(TurnStepFailure::Unauthorized));
        assert!(
            !serde_json::to_string(&steps).unwrap().contains(FAKE_SECRET),
            "the 401 body must not ride along"
        );
    }

    /// A timeout reads as a timeout even when the harness attached no
    /// classification of its own — the fallback runs the real classifier rather
    /// than the coarse `tool: failed (…)` string the old code produced.
    #[test]
    fn a_timeout_says_timeout_even_without_a_supplied_classification() {
        let step = one(
            "mcp_call_tool",
            false,
            "the request timed out after 30s",
            None,
        );
        assert_eq!(step.failure, Some(TurnStepFailure::Timeout));
        assert_eq!(
            step.result.as_deref(),
            Some("The action took too long and was stopped.")
        );
    }

    #[test]
    fn error_uses_cause_plain_when_present() {
        let steps = fold_steps(vec![completed(
            "c1",
            "mcp_call_tool",
            false,
            "HTTP 503 upstream exploded at https://x.test?token=SECRET",
            Some(serde_json::json!({"server": "brave", "tool": "search"})),
            Some(classified(
                ToolFailureClass::ServiceUnavailable,
                "The search service was temporarily unavailable.",
            )),
        )]);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert_eq!(steps[0].failure, Some(TurnStepFailure::Unavailable));
        assert_eq!(
            steps[0].result.as_deref(),
            Some("The search service was temporarily unavailable.")
        );
    }

    /// The workflow-create error-masking fix, carried forward: an intrinsic
    /// OpenCompany tool's failure surfaces its OWN message — the actionable
    /// reason — even when the classifier only offers the generic cause. It now
    /// lands in `result` ("what came back") rather than in `detail`.
    #[test]
    fn intrinsic_tool_failure_surfaces_oc_authored_reason() {
        let reason = "Couldn't create the workflow: a workflow needs exactly one trigger";
        let steps = fold_steps(vec![
            started("c1", "create_workflow", Some("Create Workflow")),
            completed(
                "c1",
                "create_workflow",
                false,
                reason,
                None,
                Some(classified(
                    ToolFailureClass::Unknown,
                    "Something went wrong",
                )),
            ),
        ]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert_eq!(
            steps[0].result.as_deref(),
            Some(reason),
            "the intrinsic tool's own message must win over the generic cause"
        );
    }

    // -----------------------------------------------------------------------
    // #411: what the step was doing
    // -----------------------------------------------------------------------

    /// The acceptance criterion, stated exactly as the issue does: two calls to
    /// the same tool must be distinguishable. Before #411 both of these rendered
    /// as the bare word "Read file".
    #[test]
    fn two_calls_to_the_same_tool_are_distinguishable() {
        let steps = fold_steps(vec![
            completed(
                "c1",
                "read_file",
                true,
                "…",
                Some(serde_json::json!({ "path": "docs/spec/README.md" })),
                None,
            ),
            completed(
                "c2",
                "read_file",
                true,
                "…",
                Some(serde_json::json!({ "path": "src/lib.rs" })),
                None,
            ),
        ]);
        assert_eq!(steps[0].detail.as_deref(), Some("path=docs/spec/README.md"));
        assert_eq!(steps[1].detail.as_deref(), Some("path=src/lib.rs"));
        assert_ne!(steps[0].detail, steps[1].detail);
    }

    /// ...including two calls to the *same remote* tool, where the routing
    /// fields are identical and only the nested arguments differ. Rendering
    /// those flat would show both as `server · tool` and lose the distinction
    /// entirely.
    #[test]
    fn two_mcp_calls_to_one_remote_tool_are_distinguishable() {
        let steps = fold_steps(vec![
            completed(
                "c1",
                "mcp_call_tool",
                true,
                "[]",
                Some(serde_json::json!({
                    "server": "github", "tool": "list_issues",
                    "arguments": { "repo": "opencompany", "state": "open" }
                })),
                None,
            ),
            completed(
                "c2",
                "mcp_call_tool",
                true,
                "[]",
                Some(serde_json::json!({
                    "server": "github", "tool": "list_issues",
                    "arguments": { "repo": "landing", "state": "closed" }
                })),
                None,
            ),
        ]);
        assert_eq!(
            steps[0].detail.as_deref(),
            Some("github · list_issues — repo=opencompany · state=open")
        );
        assert_eq!(
            steps[1].detail.as_deref(),
            Some("github · list_issues — repo=landing · state=closed")
        );
    }

    #[test]
    fn arguments_render_for_any_tool_not_just_a_whitelist() {
        let step = one(
            "some_other_tool",
            true,
            "ok",
            Some(serde_json::json!({ "anything": "at all" })),
        );
        assert_eq!(step.detail.as_deref(), Some("anything=at all"));
    }

    #[test]
    fn a_call_with_no_arguments_says_nothing() {
        assert!(one("spawn_task", true, "ok", None).detail.is_none());
        assert!(
            one("spawn_task", true, "ok", Some(serde_json::json!({})))
                .detail
                .is_none()
        );
    }

    /// Bounds, so one verbose field cannot crowd out the fields that carry the
    /// distinction — and so a multi-line argument cannot break its row.
    #[test]
    fn argument_rendering_is_bounded_and_single_line() {
        let step = one(
            "spawn_task",
            true,
            "ok",
            Some(serde_json::json!({
                "title": "x".repeat(200),
                "note": "first line\nsecond line",
                "a": 1, "b": 2, "c": 3, "d": 4,
            })),
        );
        let detail = step.detail.as_deref().unwrap();
        assert!(detail.chars().count() <= DETAIL_MAX + 1, "{detail}");
        assert!(!detail.contains('\n'), "must stay one line: {detail}");
        assert!(detail.contains('…'), "the long value is cut: {detail}");
    }

    #[test]
    fn deeply_nested_arguments_render_as_a_count_not_a_dump() {
        let step = one(
            "some_tool",
            true,
            "ok",
            Some(serde_json::json!({ "outer": { "inner": { "deep": "value" } } })),
        );
        assert_eq!(step.detail.as_deref(), Some("outer=1 field"));
    }

    // -----------------------------------------------------------------------
    // #411: what came back
    // -----------------------------------------------------------------------

    /// "How far have we come" was unanswerable when a success was a name and a
    /// duration. A collection answers it with a count; anything else with a
    /// size. Neither carries content.
    #[test]
    fn a_success_summarises_what_came_back_without_its_content() {
        let list = one(
            "mcp_call_tool",
            true,
            r#"[{"id":1},{"id":2},{"id":3}]"#,
            Some(serde_json::json!({ "server": "github", "tool": "list_issues" })),
        );
        assert_eq!(list.result.as_deref(), Some("3 items"));

        let object = one("mcp_call_tool", true, r#"{"a":1,"b":2}"#, None);
        assert_eq!(object.result.as_deref(), Some("2 fields"));

        let prose = one("mcp_call_tool", true, "a plain sentence came back", None);
        assert_eq!(prose.result.as_deref(), Some("26 characters"));

        let big = one("mcp_call_tool", true, &"x".repeat(4_200), None);
        assert_eq!(big.result.as_deref(), Some("4.2k characters"));

        let empty = one("mcp_call_tool", true, "   ", None);
        assert_eq!(empty.result, None, "nothing came back, so nothing is said");
    }

    /// An intrinsic OpenCompany tool's output is OC-authored operator copy, so a
    /// success shows it — the same argument that already let its *failures*
    /// through verbatim.
    #[test]
    fn an_intrinsic_tools_success_shows_its_own_message() {
        let step = one("query_company", true, "3 desks, 2 open cards", None);
        assert_eq!(step.result.as_deref(), Some("3 desks, 2 open cards"));
    }

    // -----------------------------------------------------------------------
    // #410 seen from here: a cut result is legible
    // -----------------------------------------------------------------------

    /// Issue #410's failure was invisible from the trace: the call succeeded,
    /// the answer was incomplete, and no status word can say both. The flag
    /// can.
    #[test]
    fn a_cut_result_is_flagged_as_truncated() {
        for output in [
            "…the first action\n\n[truncated by tool cap: 8123 more chars not shown]",
            "…\n\n[… 4096 bytes truncated by tool_result_budget — re-run with a narrower query \
             to see the rest …]",
            "[tool_result_preview]\ntool: composio_list_tools\noriginal_bytes: 90000\n",
        ] {
            let step = one("composio_list_tools", true, output, None);
            assert_eq!(
                step.status,
                TurnStepStatus::Ok,
                "a cut result still succeeded"
            );
            assert!(step.truncated, "not flagged as cut: {output}");
        }
    }

    #[test]
    fn a_complete_result_is_not_flagged() {
        assert!(!one("composio_list_tools", true, "[]", None).truncated);
    }

    /// COUPLING: all three markers are strings lifted from the vendored tool
    /// pipeline. If any is reworded, this fails rather than letting truncation
    /// go quiet again — which is precisely how #410 stayed hidden.
    #[test]
    fn truncation_markers_still_appear_in_the_vendored_tool_pipeline() {
        let sources = [
            vendored("vendor/openhuman/src/openhuman/tinyagents/middleware.rs"),
            vendored("vendor/openhuman/src/openhuman/agent/harness/tool_result_artifacts/mod.rs"),
        ]
        .concat();
        for marker in TRUNCATION_MARKERS {
            assert!(
                sources.contains(marker),
                "'{marker}' no longer appears in the vendored tool pipeline — \
                 re-derive `output_was_truncated` against the new wording"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Security
    // -----------------------------------------------------------------------

    /// SECURITY, and the acceptance criterion the issue spells out: a planted
    /// credential in **arguments** and in a **result** reaches neither the
    /// folded steps nor anything serialized from them.
    ///
    /// Arguments now ride this surface (bounded, and only through #372's
    /// redactor), so this covers the top level, a nested object, and an array —
    /// the three shapes `approval_display` itself is tested on — plus the two
    /// non-argument channels that were already refused: raw output, and the
    /// server-supplied `display_detail`.
    #[test]
    fn planted_secret_never_reaches_serialized_steps() {
        let events = vec![
            // `display_detail` carries the secret; we never read it.
            AgentProgress::ToolCallStarted {
                call_id: "c1".to_string(),
                tool_name: "mcp_call_tool".to_string(),
                arguments: Value::Null,
                iteration: 1,
                display_label: Some("Calling a remote tool".to_string()),
                display_detail: Some(format!("auth={FAKE_SECRET}")),
            },
            // Success: the secret is in the nested remote arguments (top level,
            // nested, and inside an array) AND in the output body.
            completed(
                "c1",
                "mcp_call_tool",
                true,
                &format!("remote said: {FAKE_SECRET}"),
                Some(serde_json::json!({
                    "server": "brave",
                    "tool": "search",
                    "arguments": {
                        "api_key": FAKE_SECRET,
                        "env": { "GITHUB_TOKEN": FAKE_SECRET },
                        "headers": [{ "Authorization": format!("Bearer {FAKE_SECRET}") }],
                    }
                })),
                None,
            ),
            // A failing call whose raw output also carries the secret.
            completed(
                "c2",
                "mcp_call_tool",
                false,
                &format!("401 unauthorized token={FAKE_SECRET}"),
                Some(serde_json::json!({
                    "server": "brave",
                    "tool": "search",
                    "arguments": { "password": FAKE_SECRET }
                })),
                None,
            ),
            // A parked call: the refusal text quotes the arguments back.
            completed(
                "c3",
                "send_email",
                false,
                &format!("{} body={FAKE_SECRET}", approval_refusal("send_email")),
                Some(serde_json::json!({ "to": "a@b.test", "client_secret": FAKE_SECRET })),
                None,
            ),
        ];
        let steps = fold_steps(events);
        let json = serde_json::to_string(&steps).expect("steps serialize");
        assert!(
            !json.contains(FAKE_SECRET),
            "a planted secret leaked into the serialized steps: {json}"
        );
        // ...and the redactor really ran, rather than the arguments simply
        // being dropped: the non-sensitive sibling survives.
        assert!(
            json.contains("a@b.test"),
            "this is a redactor, not a mute: {json}"
        );
    }

    /// The invariant the widened argument rendering must not weaken: a remote
    /// tool's **output** is a body we do not control, and none of it — not even
    /// bounded — becomes a result.
    #[test]
    fn a_remote_results_content_never_becomes_the_result_summary() {
        let step = one(
            "mcp_call_tool",
            true,
            "the remote said something quite specific and private",
            None,
        );
        let result = step.result.as_deref().unwrap();
        assert!(
            !result.contains("private") && !result.contains("remote said"),
            "a remote body's content leaked into the summary: {result}"
        );
        assert_eq!(result, "52 characters");
    }

    #[test]
    fn remote_tool_failure_stays_scrubbed() {
        let steps = fold_steps(vec![
            started("c1", "mcp_call_tool", Some("Calling a remote tool")),
            completed(
                "c1",
                "mcp_call_tool",
                false,
                &format!("401 unauthorized token={FAKE_SECRET}"),
                Some(serde_json::json!({ "server": "brave", "tool": "search" })),
                None,
            ),
        ]);
        let rendered = serde_json::to_string(&steps).unwrap();
        assert!(
            !rendered.contains(FAKE_SECRET),
            "remote output must never surface: {rendered}"
        );
    }

    // -----------------------------------------------------------------------
    // The incremental trace stays identical to the fold
    // -----------------------------------------------------------------------

    /// Materializes a [`StepTrace`] over `events` the way the run store does:
    /// each yield writes to its ordinal, replacing whatever was there.
    fn materialize(events: &[AgentProgress]) -> Vec<TurnStep> {
        let mut trace = StepTrace::default();
        let mut rows: Vec<Option<TurnStep>> = Vec::new();
        for event in events {
            if let Some((seq, step)) = trace.push(event) {
                let idx = seq as usize;
                if rows.len() <= idx {
                    rows.resize(idx + 1, None);
                }
                rows[idx] = Some(step);
            }
        }
        assert_eq!(
            rows.len() as u32,
            trace.emitted(),
            "ordinals must be dense — a gap means a row nothing ever writes"
        );
        rows.into_iter()
            .map(|row| row.expect("every claimed ordinal is written"))
            .collect()
    }

    /// Issue #242: the incremental trace and the final fold are the SAME
    /// timeline — which is what makes the chat bubble and the Attempts tab tell
    /// one story. #411 widened what a step carries, so the park and the cut
    /// result are in here too: a field enriched on one path only would split
    /// the two surfaces apart again.
    #[test]
    fn incremental_trace_converges_on_the_folded_timeline() {
        let events = vec![
            thinking("hmm"),
            thinking("still hmm"),
            started("c1", "mcp_call_tool", Some("Searching the web")),
            completed(
                "c1",
                "mcp_call_tool",
                true,
                "ok",
                Some(serde_json::json!({
                    "server": "brave", "tool": "search",
                    "arguments": { "query": "rust async" }
                })),
                None,
            ),
            text("here you go"),
            thinking("again"),
            started("c2", "spawn_task", None),
            completed(
                "c2",
                "spawn_task",
                false,
                "boom",
                None,
                Some(classified(ToolFailureClass::Timeout, "it timed out")),
            ),
            // A park.
            started("c3", "send_email", Some("Send email")),
            completed(
                "c3",
                "send_email",
                false,
                &approval_refusal("send_email"),
                Some(serde_json::json!({ "to": "a@b.test" })),
                None,
            ),
            // A cut result.
            completed(
                "c4",
                "composio_list_tools",
                true,
                "…\n\n[truncated by tool cap: 900 more chars not shown]",
                None,
                None,
            ),
            // A completion whose start was never observed — the standalone arm.
            completed("c5", "query_company", true, "ok", None, None),
        ];

        assert_eq!(materialize(&events), fold_steps(events.clone()));
    }

    /// The one deliberate divergence, stated as a property rather than left to
    /// be discovered: a tool call still in flight when the stream ends is
    /// persisted `Running`.
    #[test]
    fn an_unfinished_tool_call_is_persisted_as_running() {
        let events = vec![started("c1", "mcp_call_tool", Some("Searching the web"))];
        let rows = materialize(&events);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, TurnStepStatus::Running);
        assert_eq!(rows[0].label, "Searching the web");
        assert_eq!(rows, fold_steps(events));
    }

    /// Ordinals are run-scoped, not turn-scoped: a second turn on the same
    /// trace continues where the first stopped.
    #[test]
    fn ordinals_continue_across_turns_of_one_run() {
        let mut trace = StepTrace::default();
        let first = trace
            .push(&started("c1", "spawn_task", None))
            .expect("turn 1 step");
        assert_eq!(first.0, 0);
        let second = trace
            .push(&started("c9", "spawn_task", None))
            .expect("turn 2 step");
        assert_eq!(second.0, 1, "turn 2 must not reuse turn 1's ordinals");
        assert_eq!(trace.emitted(), 2);
    }

    // -----------------------------------------------------------------------
    // The live bus carries the same projection
    // -----------------------------------------------------------------------

    #[test]
    fn stream_event_from_maps_start_and_completion() {
        let mut thinking_open = false;
        let start = stream_event_from(
            &started("c1", "mcp_call_tool", Some("Searching")),
            0,
            &mut thinking_open,
        )
        .expect("start maps to a frame");
        assert_eq!(start.kind, "tool_call");
        assert_eq!(start.status, Some("running"));
        assert_eq!(start.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(start.label.as_deref(), Some("Searching"));

        let done = stream_event_from(
            &completed(
                "c1",
                "mcp_call_tool",
                true,
                "[1,2]",
                Some(serde_json::json!({ "server": "brave", "tool": "search" })),
                None,
            ),
            1,
            &mut thinking_open,
        )
        .expect("completion maps to a frame");
        assert_eq!(done.kind, "tool_result");
        assert_eq!(done.status, Some("ok"));
        assert_eq!(done.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(done.detail.as_deref(), Some("brave · search"));
        assert_eq!(done.result.as_deref(), Some("2 items"));
        assert_eq!(done.elapsed_ms, Some(42));
    }

    /// The live row must reach the same verdict the folded step does — a park
    /// that reads as an error for the seconds before the reply lands is the same
    /// bug, just briefer.
    #[test]
    fn stream_event_from_reports_a_park_as_a_park() {
        let frame = stream_event_from(
            &completed(
                "c1",
                "send_email",
                false,
                &approval_refusal("send_email"),
                None,
                None,
            ),
            0,
            &mut false,
        )
        .expect("frame");
        assert_eq!(frame.status, Some("awaiting_approval"));
        assert_eq!(frame.failure, None);
        assert_eq!(frame.result.as_deref(), Some(AWAITING_APPROVAL_RESULT));
    }

    #[test]
    fn stream_event_from_carries_the_typed_failure() {
        let frame = stream_event_from(
            &completed("c1", "mcp_call_tool", false, "401 unauthorized", None, None),
            0,
            &mut false,
        )
        .expect("frame");
        assert_eq!(frame.status, Some("error"));
        assert_eq!(frame.failure, Some(TurnStepFailure::Unauthorized));
    }

    #[test]
    fn stream_event_from_coalesces_thinking_and_ignores_text() {
        let mut open = false;
        let first = stream_event_from(&thinking("hmm"), 0, &mut open).expect("first delta → frame");
        assert_eq!(first.kind, "thinking");
        assert_eq!(first.label.as_deref(), Some("Thinking"));
        assert!(open, "run is now open");
        assert!(stream_event_from(&thinking("more"), 1, &mut open).is_none());
        assert!(stream_event_from(&text("hello"), 2, &mut open).is_none());
        assert!(!open, "text closed the run");
        assert!(stream_event_from(&thinking("again"), 3, &mut open).is_some());
    }

    /// The live frame is scrubbed exactly like the folded step.
    #[test]
    fn stream_event_from_never_leaks_remote_output() {
        let frame = stream_event_from(
            &completed(
                "c2",
                "mcp_call_tool",
                false,
                &format!("401 token={FAKE_SECRET}"),
                Some(serde_json::json!({
                    "server": "brave", "tool": "search",
                    "arguments": { "api_key": FAKE_SECRET }
                })),
                None,
            ),
            0,
            &mut false,
        )
        .expect("frame");
        let json = serde_json::to_string(&frame).expect("frame serialize");
        assert!(
            !json.contains(FAKE_SECRET),
            "a planted secret leaked into a live turn-stream frame: {json}"
        );
    }
}
