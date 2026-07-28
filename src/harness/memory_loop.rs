//! The retrieve→inject→store memory loop for embedded company agents.
//!
//! openhuman's own turn recalls the agent's *learned context* (preferences,
//! observations, reflections) from memory. This module adds the
//! OpenCompany-orchestrator layer the memory spec calls for: before a turn it
//! retrieves the top-K prior task outcomes relevant to the incoming message and
//! injects them as context, and after the turn it stores the outcome so it
//! compounds into later turns.
//!
//! The **store** half is the load-bearing part: the harness wires no
//! memory-store tool, so without this nothing persists a completed task — the
//! compounding loop stays open and every turn starts cold. Retrieval reads and
//! storage writes go through the same [`ContextStore`](crate::ports::ContextStore)
//! the agent's own recall uses, so an `OPENCOMPANY_MEMORY=tinycortex` overlay
//! (or any base backend) applies uniformly.
//!
//! The helpers here are pure so they unit-test without a live agent;
//! [`HarnessPool::run`](super::HarnessPool::run) wires them around the turn.

use crate::ports::types::{ChunkHit, ContextChunk};

/// How many prior-outcome chunks to inject before a turn.
pub const RETRIEVE_TOP_K: usize = 5;

/// Max characters of any single retrieved snippet injected as prior work; longer
/// snippets are truncated with an ellipsis.
pub const MAX_SNIPPET_CHARS: usize = 500;

/// Max total characters of the injected "relevant prior work" preamble. Once
/// reached, remaining hits are dropped so a few large stored replies can't blow
/// the model's context window or inflate cost.
pub const MAX_HISTORY_CHARS: usize = 2000;

/// Label prefix for stored task outcomes, so they are listable by prefix and
/// never collide with the agent's namespaced learned-context entries.
pub const OUTCOME_LABEL_PREFIX: &str = "task-outcome";

/// Builds the message actually handed to the agent: the original message,
/// prefixed with a compact "relevant prior work" preamble when retrieval
/// returned anything.
///
/// With no hits the message is returned unchanged, so a cold-start turn (empty
/// memory) is byte-identical to the pre-loop behaviour.
pub fn inject(message: &str, hits: &[ChunkHit]) -> String {
    if hits.is_empty() {
        return message.to_string();
    }
    let mut out = String::from("## Relevant prior work\n");
    // Bound both each snippet and the total preamble so a few large stored
    // replies can't blow the context window (see MAX_* constants).
    let mut remaining = MAX_HISTORY_CHARS;
    let mut injected = false;
    for hit in hits {
        let snippet = truncate_chars(hit.snippet.trim(), MAX_SNIPPET_CHARS);
        let cost = snippet.chars().count() + 3; // "- " + "\n"
        if cost > remaining {
            break;
        }
        out.push_str("- ");
        out.push_str(&snippet);
        out.push('\n');
        remaining -= cost;
        injected = true;
    }
    // Every hit was empty or over budget — fall back to the bare message so a
    // cold-equivalent turn stays unchanged.
    if !injected {
        return message.to_string();
    }
    out.push_str("\n## Task\n");
    out.push_str(message);
    out
}

/// Truncates `s` to at most `max` characters (on a char boundary), appending an
/// ellipsis when anything was dropped.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// The context chunk recording one completed turn's outcome, labelled under
/// [`OUTCOME_LABEL_PREFIX`] and carrying both the task and the answer so a later
/// `search` matches on either side.
pub fn outcome_chunk(agent_id: &str, message: &str, reply: &str) -> ContextChunk {
    ContextChunk {
        label: format!("{OUTCOME_LABEL_PREFIX}/{agent_id}"),
        body: format!("Task: {message}\nOutcome: {reply}"),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::ChunkAddr;

    fn hit(snippet: &str) -> ChunkHit {
        ChunkHit {
            addr: ChunkAddr::new("addr"),
            snippet: snippet.to_string(),
            score: 1.0,
        }
    }

    #[test]
    fn inject_with_no_hits_is_unchanged() {
        assert_eq!(inject("do the thing", &[]), "do the thing");
    }

    #[test]
    fn inject_truncates_an_oversized_snippet() {
        let big = "x".repeat(10_000);
        let out = inject("do it", &[hit(&big)]);
        // The 10k snippet is capped to MAX_SNIPPET_CHARS, not injected whole.
        assert!(
            out.chars().count() < 10_000,
            "oversized snippet must truncate"
        );
        assert!(out.contains('…'), "truncation is marked with an ellipsis");
        assert!(out.trim_end().ends_with("do it"));
    }

    #[test]
    fn inject_stops_at_the_total_history_budget() {
        // Many mid-size snippets: the injected preamble stays within budget.
        let hits: Vec<ChunkHit> = (0..50).map(|_| hit(&"y".repeat(400))).collect();
        let out = inject("go", &hits);
        let injected = out.chars().count() - "go".chars().count();
        assert!(
            injected <= MAX_HISTORY_CHARS + MAX_SNIPPET_CHARS,
            "history is budget-capped, got {injected} injected chars"
        );
    }

    #[test]
    fn inject_prepends_a_preamble_and_keeps_the_task() {
        let out = inject("ship it", &[hit("Task: plan\nOutcome: drafted plan")]);
        assert!(out.starts_with("## Relevant prior work\n"));
        assert!(out.contains("drafted plan"));
        assert!(out.trim_end().ends_with("ship it"));
    }

    #[test]
    fn outcome_chunk_labels_and_carries_both_sides() {
        let chunk = outcome_chunk("ceo", "plan the launch", "here is the plan");
        assert_eq!(chunk.label, "task-outcome/ceo");
        assert!(chunk.body.contains("plan the launch"));
        assert!(chunk.body.contains("here is the plan"));
    }
}
