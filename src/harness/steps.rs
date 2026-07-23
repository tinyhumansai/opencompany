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

/// The detail for a failed tool call: the classifier's plain-language cause when
/// present, else the `sanitize_tool_output` **class** string. Never the raw
/// remote error text.
fn error_detail(
    failure: Option<&ClassifiedFailure>,
    output: &str,
    tool_name: &str,
) -> Option<String> {
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
}
