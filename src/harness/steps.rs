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
//! ## Security (the whole reason this is a separate, unit-tested module)
//!
//! The wire shape carries **no raw tool arguments, no tool output, and no call
//! ids** — only a label, an optional scrubbed detail, and an elapsed time. Three
//! rules enforce that:
//!
//! * **Label** comes from the tool's server-computed `display_label`, else its
//!   tool *name* — never from arguments or output.
//! * **Detail on success** is *whitelist-only* enrichment: a fixed per-tool set
//!   of structural fields (`mcp_call_tool → server·tool`, `delegate_to_desk →
//!   desk`, `spawn_task → title`). An unknown tool contributes nothing, and the
//!   nested remote `arguments` of an MCP call are never read — that is exactly
//!   the re-injection surface this avoids.
//! * **Detail on failure** is the classifier's plain-language
//!   [`cause_plain`](oh::tool_status::ClassifiedFailure::cause_plain) when
//!   present, else the `sanitize_tool_output` **class** string (`"tool: failed
//!   (timeout)"`) — never the remote error text.
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

use oh::agent::hooks::sanitize_tool_output;
use oh::agent::progress::AgentProgress;
use oh::tool_status::ClassifiedFailure;

use crate::ports::types::{TurnStep, TurnStepKind, TurnStepStatus};
use crate::turn_stream::TurnStreamEvent;

/// Hard cap on the number of steps carried back to the operator. A runaway turn
/// (a tight tool loop) is truncated to this many, plus one omission note.
const MAX_STEPS: usize = 50;

/// A `spawn_task` title is truncated to this many chars before it becomes a
/// step detail — a title is agent-authored free text, so it is bounded even
/// though it is whitelisted.
const TITLE_MAX: usize = 80;

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
                    detail: None,
                    elapsed_ms: None,
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
                let status = if success {
                    TurnStepStatus::Ok
                } else {
                    TurnStepStatus::Error
                };
                let detail = if success {
                    enrich_detail(&tool_name, arguments.as_ref())
                } else {
                    error_detail(failure.as_ref(), &output, &tool_name)
                };

                if let Some(idx) = running.remove(&call_id) {
                    // Finalize the paired start in place, keeping its label.
                    let step = &mut steps[idx];
                    step.status = status;
                    step.elapsed_ms = Some(elapsed_ms);
                    step.detail = detail;
                } else {
                    // A completion with no observed start — surface it standalone.
                    steps.push(TurnStep {
                        kind: TurnStepKind::ToolCall,
                        status,
                        label: humanize(&tool_name),
                        detail,
                        elapsed_ms: Some(elapsed_ms),
                    });
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
                    detail: None,
                    elapsed_ms: None,
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
            detail: None,
            elapsed_ms: None,
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
                        detail: None,
                        elapsed_ms: None,
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
                let status = if *success {
                    TurnStepStatus::Ok
                } else {
                    TurnStepStatus::Error
                };
                let detail = if *success {
                    enrich_detail(tool_name, arguments.as_ref())
                } else {
                    error_detail(failure.as_ref(), output, tool_name)
                };
                let (seq, label) = match self.running.remove(call_id) {
                    Some(found) => found,
                    // No observed start — surface it standalone, exactly as the
                    // fold does.
                    None => (self.claim(), humanize(tool_name)),
                };
                Some((
                    seq,
                    TurnStep {
                        kind: TurnStepKind::ToolCall,
                        status,
                        label,
                        detail,
                        elapsed_ms: Some(*elapsed_ms),
                    },
                ))
            }
            AgentProgress::ThinkingDelta { .. } if !self.thinking_open => {
                self.thinking_open = true;
                Some((
                    self.claim(),
                    TurnStep {
                        kind: TurnStepKind::Thinking,
                        status: TurnStepStatus::Ok,
                        label: "Thinking".to_string(),
                        detail: None,
                        elapsed_ms: None,
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
                agent_id: None,
                chat_id: None,
                tool_call_id: Some(call_id.clone()),
                label: Some(label_for(display_label.clone(), tool_name)),
                detail: None,
                status: Some("running"),
                elapsed_ms: None,
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
            let (status, detail) = if *success {
                ("ok", enrich_detail(tool_name, arguments.as_ref()))
            } else {
                ("error", error_detail(failure.as_ref(), output, tool_name))
            };
            Some(TurnStreamEvent {
                kind: "tool_result",
                seq,
                agent_id: None,
                chat_id: None,
                tool_call_id: Some(call_id.clone()),
                // A label so a completion with no observed start still renders;
                // the common case pairs by `tool_call_id` and keeps the running
                // row's richer label.
                label: Some(humanize(tool_name)),
                detail,
                status: Some(status),
                elapsed_ms: Some(*elapsed_ms),
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
                agent_id: None,
                chat_id: None,
                tool_call_id: None,
                label: Some("Thinking".to_string()),
                detail: None,
                status: Some("ok"),
                elapsed_ms: None,
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

/// Whitelist-only success enrichment. Reads a fixed set of structural fields per
/// known tool from the arguments the crate captured on the *completed* event.
/// Any other tool — or a missing field — yields `None`. The nested remote
/// `arguments` of an MCP call are deliberately never read.
fn enrich_detail(tool_name: &str, arguments: Option<&Value>) -> Option<String> {
    let args = arguments?;
    match tool_name {
        "mcp_call_tool" => {
            let server = args.get("server").and_then(Value::as_str)?;
            let tool = args.get("tool").and_then(Value::as_str)?;
            Some(format!("{server} · {tool}"))
        }
        "delegate_to_desk" => args.get("desk").and_then(Value::as_str).map(str::to_string),
        "spawn_task" => args
            .get("title")
            .and_then(Value::as_str)
            .map(|title| truncate(title, TITLE_MAX)),
        _ => None,
    }
}

/// OpenCompany's own orchestrator tools (issue #112 et al). Their error text is
/// **OC-authored, operator-facing copy** — the tool's own `ToolResult::error`
/// message (e.g. "workflow needs exactly one trigger"), not a remote or
/// untrusted body — so on failure it is safe *and* useful to surface verbatim
/// (bounded) instead of collapsing to the generic classifier class. Contrast
/// `mcp_call_tool`, whose output is a remote server body and therefore stays
/// scrubbed to a class string. Keep this list in lockstep with
/// [`orchestrator_tools`](crate::harness::orchestrator::orchestrator_tools).
const INTRINSIC_TOOLS: &[&str] = &[
    "query_company",
    "spawn_task",
    "delegate_to_desk",
    "run_workflow",
    "create_workflow",
    "add_agent",
];

/// Bound on the OC-authored failure detail surfaced for an intrinsic tool.
const ERROR_DETAIL_MAX: usize = 200;

/// The detail for a failed tool call.
///
/// For an **intrinsic OpenCompany tool** the tool's own output *is* the
/// actionable, OC-authored reason, so a bounded copy of it is surfaced first —
/// this is what turns the useless generic "Something went wrong with this
/// action." into the real cause the operator needs (issue: workflow-create
/// error masking). For every other tool the rule is unchanged: the classifier's
/// plain-language cause when present, else the `sanitize_tool_output` **class**
/// string — never the raw remote error text.
fn error_detail(
    failure: Option<&ClassifiedFailure>,
    output: &str,
    tool_name: &str,
) -> Option<String> {
    if INTRINSIC_TOOLS.contains(&tool_name) {
        let msg = output.trim();
        if !msg.is_empty() {
            return Some(truncate(msg, ERROR_DETAIL_MAX));
        }
    }
    match failure {
        Some(f) if !f.cause_plain.trim().is_empty() => Some(f.cause_plain.clone()),
        _ => {
            let class = sanitize_tool_output(output, tool_name, false);
            (!class.trim().is_empty()).then_some(class)
        }
    }
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
    use oh::tool_status::{FailureCategory, ToolFailureClass};

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
    fn enriches_delegate_to_desk_and_spawn_task_from_whitelist() {
        let steps = fold_steps(vec![
            completed(
                "d1",
                "delegate_to_desk",
                true,
                "ok",
                Some(serde_json::json!({"desk": "engineering", "instruction": "ship it"})),
                None,
            ),
            completed(
                "s1",
                "spawn_task",
                true,
                "ok",
                Some(serde_json::json!({"title": "Draft the Q3 plan", "note": "secret-in-note"})),
                None,
            ),
        ]);
        assert_eq!(steps[0].detail.as_deref(), Some("engineering"));
        assert_eq!(steps[1].detail.as_deref(), Some("Draft the Q3 plan"));
    }

    #[test]
    fn unknown_tool_gets_no_detail() {
        let steps = fold_steps(vec![completed(
            "c1",
            "some_other_tool",
            true,
            "ok",
            Some(serde_json::json!({"anything": "at all"})),
            None,
        )]);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].detail.is_none(), "unknown tool enriches nothing");
    }

    #[test]
    fn spawn_task_title_is_truncated() {
        let long = "x".repeat(200);
        let steps = fold_steps(vec![completed(
            "s1",
            "spawn_task",
            true,
            "ok",
            Some(serde_json::json!({ "title": long })),
            None,
        )]);
        let detail = steps[0].detail.as_deref().unwrap();
        assert!(detail.ends_with('…'));
        assert_eq!(detail.chars().count(), TITLE_MAX + 1);
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
        assert_eq!(
            steps[0].detail.as_deref(),
            Some("The search service was temporarily unavailable.")
        );
    }

    #[test]
    fn error_without_failure_uses_sanitized_class_not_raw_output() {
        let steps = fold_steps(vec![completed(
            "c1",
            "mcp_call_tool",
            false,
            "connection refused talking to 10.0.0.5 with token=SUPERSECRET",
            None,
            None,
        )]);
        let detail = steps[0].detail.as_deref().unwrap();
        // A safe class string, never the raw remote text.
        assert_eq!(detail, "mcp_call_tool: failed (connection_error)");
        assert!(!detail.contains("SUPERSECRET"));
        assert!(!detail.contains("10.0.0.5"));
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

    /// SECURITY: a secret planted in a tool's raw output, its nested remote
    /// `arguments`, and its `display_detail` must appear in **no** serialized
    /// step. This is the wire-level guarantee the whole module exists to keep.
    #[test]
    fn planted_secret_never_reaches_serialized_steps() {
        const SECRET: &str = "sk-live-PLANTEDSECRET-abc123";
        let events = vec![
            // display_detail carries the secret; we never read it.
            AgentProgress::ToolCallStarted {
                call_id: "c1".to_string(),
                tool_name: "mcp_call_tool".to_string(),
                arguments: Value::Null,
                iteration: 1,
                display_label: Some("Calling a remote tool".to_string()),
                display_detail: Some(format!("auth={SECRET}")),
            },
            // Success: nested remote arguments carry the secret; output carries it.
            completed(
                "c1",
                "mcp_call_tool",
                true,
                &format!("remote said: {SECRET}"),
                Some(serde_json::json!({
                    "server": "brave",
                    "tool": "search",
                    "arguments": { "api_key": SECRET }
                })),
                None,
            ),
            // A failing call whose raw output also carries the secret.
            completed(
                "c2",
                "mcp_call_tool",
                false,
                &format!("401 unauthorized token={SECRET}"),
                Some(serde_json::json!({ "server": "brave", "tool": "search" })),
                None,
            ),
        ];
        let steps = fold_steps(events);
        let json = serde_json::to_string(&steps).expect("steps serialize");
        assert!(
            !json.contains(SECRET),
            "a planted secret leaked into the serialized steps: {json}"
        );
        assert!(!json.contains("api_key"), "nested arg keys must not leak");
    }

    /// A memory-served answer runs zero steps — the tell that distinguishes it
    /// from a tool-backed one — so an empty stream folds to an empty timeline.
    #[test]
    fn empty_stream_folds_to_no_steps() {
        assert!(fold_steps(Vec::new()).is_empty());
    }

    /// The workflow-create error-masking fix: an intrinsic OpenCompany tool's
    /// failure surfaces its OWN OC-authored message — the actionable reason —
    /// even when the classifier only offers the generic "Unknown" cause. This is
    /// what turns "Something went wrong with this action." into "…needs exactly
    /// one trigger" on the operator bubble.
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
                // The generic classifier cause an operator would otherwise see.
                Some(classified(
                    ToolFailureClass::Unknown,
                    "Something went wrong",
                )),
            ),
        ]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, TurnStepStatus::Error);
        assert_eq!(
            steps[0].detail.as_deref(),
            Some(reason),
            "the intrinsic tool's own message must win over the generic cause"
        );
    }

    /// The security invariant the intrinsic-tool branch must NOT weaken: a
    /// remote (`mcp_call_tool`) failure still scrubs to a class string — its raw
    /// output (a remote body that can carry a secret) never becomes the detail.
    #[test]
    fn remote_tool_failure_stays_scrubbed_to_class() {
        const SECRET: &str = "sk-live-REMOTE-9x";
        let steps = fold_steps(vec![
            started("c1", "mcp_call_tool", Some("Calling a remote tool")),
            completed(
                "c1",
                "mcp_call_tool",
                false,
                &format!("401 unauthorized token={SECRET}"),
                Some(serde_json::json!({ "server": "brave", "tool": "search" })),
                None,
            ),
        ]);
        let detail = steps[0].detail.clone().unwrap_or_default();
        assert!(
            !detail.contains(SECRET),
            "remote output must never surface as detail: {detail}"
        );
    }

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
    /// timeline. If they can drift, the persisted run trace and the chat bubble
    /// tell an operator two different stories about one turn — which is exactly
    /// the failure the shared scrubbing helpers exist to prevent.
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
                Some(serde_json::json!({ "server": "brave", "tool": "search" })),
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
            // A completion whose start was never observed — the standalone arm.
            completed("c3", "query_company", true, "ok", None, None),
        ];

        assert_eq!(materialize(&events), fold_steps(events.clone()));
    }

    /// The one deliberate divergence, stated as a property rather than left to
    /// be discovered: a tool call still in flight when the stream ends is
    /// persisted `Running`. The fold agrees here (it also leaves an unmatched
    /// start running), and that is what makes a killed run's partial trace
    /// honest instead of fabricated.
    #[test]
    fn an_unfinished_tool_call_is_persisted_as_running() {
        let events = vec![
            started("c1", "mcp_call_tool", Some("Searching the web")),
            // …and the host dies here.
        ];
        let rows = materialize(&events);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, TurnStepStatus::Running);
        assert_eq!(rows[0].label, "Searching the web");
        assert_eq!(rows, fold_steps(events));
    }

    /// Ordinals are run-scoped, not turn-scoped: a second turn on the same
    /// trace continues where the first stopped. Restarting per turn would make
    /// turn 2 overwrite turn 1's rows, since the store keys on
    /// `(run_id, step_seq)`.
    #[test]
    fn ordinals_continue_across_turns_of_one_run() {
        let mut trace = StepTrace::default();
        let first = trace
            .push(&started("c1", "spawn_task", None))
            .expect("turn 1 step");
        assert_eq!(first.0, 0);
        // …turn 1 ends, turn 2 begins on the same run.
        let second = trace
            .push(&started("c9", "spawn_task", None))
            .expect("turn 2 step");
        assert_eq!(second.0, 1, "turn 2 must not reuse turn 1's ordinals");
        assert_eq!(trace.emitted(), 2);
    }

    /// `stream_event_from` (the live-bus counterpart of `fold_steps`) maps a
    /// start to a `running` `tool_call` frame and a completion to an `ok`
    /// `tool_result` frame paired by `tool_call_id`, carrying the same scrubbed
    /// label/detail the folded timeline would.
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
                "ok",
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
        assert_eq!(done.elapsed_ms, Some(42));
    }

    /// Thinking coalesces live exactly as it folds: the FIRST delta of a run
    /// emits one `thinking` frame, consecutive deltas emit nothing, and visible
    /// text closes the run (no frame). This is what keeps the live step count
    /// tracking the final folded count instead of jumping up when the reply
    /// lands.
    #[test]
    fn stream_event_from_coalesces_thinking_and_ignores_text() {
        let mut open = false;
        let first = stream_event_from(&thinking("hmm"), 0, &mut open).expect("first delta → frame");
        assert_eq!(first.kind, "thinking");
        assert_eq!(first.label.as_deref(), Some("Thinking"));
        assert!(open, "run is now open");
        // A consecutive delta while the run is open adds nothing.
        assert!(stream_event_from(&thinking("more"), 1, &mut open).is_none());
        // Visible text closes the run without a frame of its own.
        assert!(stream_event_from(&text("hello"), 2, &mut open).is_none());
        assert!(!open, "text closed the run");
        // A fresh thinking run after that opens a new frame.
        assert!(stream_event_from(&thinking("again"), 3, &mut open).is_some());
    }

    /// The live frame is scrubbed exactly like the folded step: a failing remote
    /// call with a planted secret in its output never serializes that secret.
    #[test]
    fn stream_event_from_never_leaks_remote_output() {
        const SECRET: &str = "sk-live-STREAM-42";
        let frame = stream_event_from(
            &completed(
                "c2",
                "mcp_call_tool",
                false,
                &format!("401 token={SECRET}"),
                Some(serde_json::json!({ "server": "brave", "tool": "search" })),
                None,
            ),
            0,
            &mut false,
        )
        .expect("frame");
        let json = serde_json::to_string(&frame).expect("frame serialize");
        assert!(
            !json.contains(SECRET),
            "a planted secret leaked into a live turn-stream frame: {json}"
        );
    }
}
