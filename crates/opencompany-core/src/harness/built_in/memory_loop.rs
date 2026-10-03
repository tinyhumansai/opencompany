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
//! the agent's own recall uses, so a hosted-memory overlay (or any base
//! backend) applies uniformly.
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
/// memory) is byte-identical to the pre-loop behaviour. With hits, the preamble
/// always accounts for **all** of them: whatever the budget could not carry is
/// named in a count marker, so "prior work was suppressed" never reads to the
/// model like "no prior work exists".
pub fn inject(message: &str, hits: &[ChunkHit]) -> String {
    inject_within(message, hits, MAX_HISTORY_CHARS)
}

/// [`inject`] with the total-preamble budget as a parameter, so the
/// nothing-fits path is reachable in a test without a 2000-character fixture.
fn inject_within(message: &str, hits: &[ChunkHit], history_budget: usize) -> String {
    if hits.is_empty() {
        return message.to_string();
    }
    let mut out = String::from("## Relevant prior work\n");
    // Bound both each snippet and the total preamble so a few large stored
    // replies can't blow the context window (see MAX_* constants).
    let mut remaining = history_budget;
    let mut skipped = 0usize;
    for hit in hits {
        // Flatten whitespace (split_whitespace / join(" ")) so embedded newlines
        // in stored snippets cannot cross the `## Task` boundary below.
        let snippet = truncate_chars(
            &hit.snippet.split_whitespace().collect::<Vec<_>>().join(" "),
            MAX_SNIPPET_CHARS,
        );
        let cost = snippet.chars().count() + 3; // "- " + "\n"
        if cost > remaining {
            // Skip, don't stop: a later, smaller hit may still fit, and every
            // hit that doesn't is accounted for by the marker below.
            skipped += 1;
            continue;
        }
        out.push_str("- ");
        out.push_str(&snippet);
        out.push('\n');
        remaining -= cost;
    }
    if skipped > 0 {
        // Deliberately charged to nobody: this one short line sits *outside*
        // `history_budget`, because the budget exists to bound retrieved text
        // and the marker is our own accounting. Letting it compete for room
        // would mean the marker could itself be the thing squeezed out — the
        // exact silence it exists to prevent. It also guarantees the
        // all-oversized case still emits a preamble rather than a bare message.
        out.push_str(&format!(
            "- […{skipped} more prior result(s) omitted for space]\n"
        ));
    }
    out.push_str("\n## Task\n");
    out.push_str(message);
    out
}

/// Truncates `s` to at most `max` characters (on a char boundary), appending an
/// ellipsis when anything was dropped.
///
/// The ellipsis is budgeted *inside* `max`: taking `max` characters and then
/// appending returns `max + 1`, so the cap would quietly exceed the bound it
/// advertises. Cutting counts characters, never bytes, so a multibyte snippet
/// can't split a codepoint.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// The context chunk recording one completed turn's outcome, labelled under
/// [`OUTCOME_LABEL_PREFIX`] and carrying both the task and the answer so a later
/// `search` matches on either side.
///
/// Both sides pass through [`redact_secrets`](super::redact::redact_secrets):
/// this write path stores the operator message verbatim (the motivating
/// `/secret/` and bearer-token leak), so the redaction lives here, at the
/// chunk's single construction point.
pub fn outcome_chunk(agent_id: &str, message: &str, reply: &str) -> ContextChunk {
    ContextChunk {
        label: format!("{OUTCOME_LABEL_PREFIX}/{agent_id}"),
        body: format!(
            "Task: {}\nOutcome: {}",
            super::redact::redact_secrets(message),
            super::redact::redact_secrets(reply)
        ),
    }
}

#[cfg(test)]
#[path = "memory_loop_tests.rs"]
mod tests;
