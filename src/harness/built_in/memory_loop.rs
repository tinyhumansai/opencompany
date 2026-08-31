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
/// Both sides pass through [`redact_secrets`](super::memory::redact_secrets):
/// this write path stores the operator message verbatim (the motivating
/// `/secret/` and bearer-token leak), and it bypasses
/// [`Memory::store`](crate::harness::built_in::memory::OcMemory), so the
/// redaction has to live here, at the chunk's single construction point.
pub fn outcome_chunk(agent_id: &str, message: &str, reply: &str) -> ContextChunk {
    ContextChunk {
        label: format!("{OUTCOME_LABEL_PREFIX}/{agent_id}"),
        body: format!(
            "Task: {}\nOutcome: {}",
            super::memory::redact_secrets(message),
            super::memory::redact_secrets(reply)
        ),
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
    fn a_truncated_snippet_reserves_room_for_its_own_ellipsis() {
        let big = "x".repeat(10_000);
        let cut = truncate_chars(&big, MAX_SNIPPET_CHARS);
        assert_eq!(
            cut.chars().count(),
            MAX_SNIPPET_CHARS,
            "the marker is budgeted inside the cap, not added on top of it"
        );
        assert!(cut.ends_with('…'));
        // Multibyte input cuts on a character boundary, never mid-codepoint.
        let multibyte = "é".repeat(10_000);
        let cut = truncate_chars(&multibyte, MAX_SNIPPET_CHARS);
        assert_eq!(cut.chars().count(), MAX_SNIPPET_CHARS);
    }

    /// The count the "omitted for space" marker names, if the preamble has one.
    fn omitted_count(out: &str) -> Option<usize> {
        let line = out.lines().find(|l| l.contains("omitted for space"))?;
        line.trim_start_matches("- […")
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    /// Lines carrying an actual retrieved snippet (not the marker).
    fn shown_count(out: &str) -> usize {
        out.lines()
            .filter(|l| l.starts_with("- ") && !l.contains("omitted for space"))
            .count()
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
    fn inject_names_how_many_hits_the_budget_dropped() {
        let hits: Vec<ChunkHit> = (0..50).map(|_| hit(&"y".repeat(400))).collect();
        let out = inject("go", &hits);
        let shown = shown_count(&out);
        assert!(shown > 0, "some prior work fits in the budget");
        assert!(shown < 50, "not all of it does — this is the drop path");
        assert_eq!(
            omitted_count(&out),
            Some(50 - shown),
            "every hit is either shown or counted: {out}"
        );
    }

    #[test]
    fn inject_accounts_for_every_nearly_fitting_hit() {
        // Sized so the budget runs out partway through rather than cleanly.
        let hits: Vec<ChunkHit> = (0..9).map(|i| hit(&"z".repeat(230 + i))).collect();
        let out = inject("go", &hits);
        let shown = shown_count(&out);
        let omitted = omitted_count(&out).unwrap_or(0);
        assert_eq!(shown + omitted, 9, "no hit vanishes unaccounted: {out}");
        let injected = out.chars().count() - "go".chars().count();
        assert!(
            injected <= MAX_HISTORY_CHARS + MAX_SNIPPET_CHARS,
            "still budget-capped, got {injected} injected chars"
        );
    }

    #[test]
    fn inject_with_nothing_fitting_still_says_prior_work_existed() {
        // A budget too small for any hit: the preamble must still appear
        // carrying only the marker, so suppression is distinguishable from a
        // genuinely cold memory.
        let hits: Vec<ChunkHit> = (0..3).map(|_| hit("a stored outcome")).collect();
        let out = inject_within("go", &hits, 2);
        assert!(out.starts_with("## Relevant prior work\n"), "{out}");
        assert_eq!(omitted_count(&out), Some(3), "{out}");
        assert_eq!(shown_count(&out), 0, "nothing fit: {out}");
        assert!(out.trim_end().ends_with("go"));
        assert_ne!(out, "go", "a suppressed preamble is not a bare message");
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

    #[test]
    fn outcome_chunk_redacts_secrets_on_both_sides() {
        // This write path bypasses `Memory::store`, so the operator message
        // and the reply must be redacted here — an operator message carrying
        // a one-time-secret URL or bearer token must not persist verbatim.
        let chunk = outcome_chunk(
            "ceo",
            "open https://ots.example/secret/AbCdEf123456",
            "opened it with Bearer sk-verylongsecrettoken",
        );
        assert!(chunk.body.contains("/secret/[REDACTED]"));
        assert!(chunk.body.contains("Bearer [REDACTED]"));
        assert!(!chunk.body.contains("AbCdEf123456"));
        assert!(!chunk.body.contains("sk-verylongsecrettoken"));
    }

    #[test]
    fn multiline_snippets_are_flattened_before_injection() {
        // Embedded newlines in a stored snippet must be collapsed before
        // truncation; otherwise they cross the `## Task` boundary and break
        // the preamble structure (CWE-74 — injection of untrusted whitespace
        // into the prompt format).
        let out = inject("now", &[hit("Task: plan\nOutcome: drafted\n\nNotes: good")]);
        let preamble = out.split("## Task").next().unwrap();
        let injected_line = preamble
            .lines()
            .find(|l| l.starts_with("- "))
            .expect("the multiline snippet produces an injected line");
        assert!(
            !injected_line.contains('\n'),
            "newlines must be collapsed, got: {injected_line:?}"
        );
        assert!(
            injected_line.contains("Task: plan Outcome: drafted Notes: good"),
            "all whitespace runs are collapsed to single spaces: {injected_line:?}"
        );
    }
}
