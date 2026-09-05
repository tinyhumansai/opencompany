//! Guarding a suppressed turn's reply against a text-shaped tool call it
//! never had to begin with (#2094).
//!
//! ## The leak
//!
//! `is_chat_only_turn()` sets `overrides.suppress_tools = true`
//! (`mod.rs:1282`, the #1725 cheap-reply fast path): the provider request
//! carries an empty tool schema, so the model cannot enter the tool loop.
//! Nothing else about the turn changes. The system prompt is built exactly
//! once, on the turn that finds `self.history.is_empty()`
//! (`vendor/openhuman` `core_turn.rs`, `build_system_prompt`), and every later
//! turn — suppressed or not — reuses those bytes verbatim so the inference
//! backend's KV-cache prefix survives; that reuse is stated as deliberate
//! there ("only baked into the system prompt on the very first turn"). So the
//! turn that decides to suppress tools cannot also rewrite the workspace /
//! ledger / sandbox / skills tool briefs already frozen into that prompt
//! without either rebuilding it every turn (paying the cache-invalidation cost
//! of a suppressed turn onto every ordinary turn that follows it) or adding a
//! per-turn system-prompt override to openhuman's `TurnOverrides` — a change
//! to the vendored crate this repo only consumes, not a fix reachable from
//! here. So the model is told at length about tools it does not have on the
//! wire this turn, and does the obvious thing: writes the call out as text.
//! That text becomes the entire reply the operator reads.
//!
//! ## Why this is not `native_salvage`
//!
//! [`super::native_salvage::recover_text_tool_calls`] keys on
//! `authorized_tool_names`, which is empty on a suppressed turn *by
//! construction* — see that function's own doc comment, which calls the
//! resulting no-op deliberate: recovering a call this turn never authorized
//! would execute an action the operator explicitly declined to pay for
//! ("Just chatting"). This module is the opposite of recovery. It never
//! parses a call out to run one — it only keeps one from reaching the
//! operator verbatim. That also makes it a different defect from issue #2092
//! and PR #2093: those are about a turn that *offered* tools and got a
//! text-shaped call back anyway. This is about a turn that offered none and
//! told the model otherwise in its prompt.
//!
//! ## Detection, deliberately coarse
//!
//! A genuine "Just chatting" answer has no structural reason to contain an
//! XML-ish `<tool_call>` / `<tool_calls>` / `<invoke>` / `<function_call>`
//! tag, or a `function_call:{…}` / `tool_call:{…}` text prefix (the shape
//! [`super::native_salvage`]'s own docs show a real model once wrote in the
//! authorized case) — those are markup that leaked out of the model's own
//! tool-calling chat template, not English. So detection deliberately does
//! not try to surgically excise a matched span and stitch the surrounding
//! prose back together: on a turn this short (#1725's fast path never has
//! much prose to save), a best-effort splice risks handing the operator a
//! grammatically broken half-sentence, which is arguably worse than the bug.
//! Instead, any match flags the *whole reply* as compromised and swaps in a
//! short, honest line rather than fabricate a stitched answer.
//!
//! The pattern is intentionally wider than #105's
//! [`super::tool_dispatcher::AttrTolerantXmlDispatcher`] normalization (which
//! only widens the bare `<tool_call>` family so it can still be *executed*):
//! nothing here executes what it finds, so there is no cost to also catching
//! a model's own special-token wrapper around the keyword — the exact shape
//! #2094 reproduced was `<｜｜DSML｜｜tool_calls>` /
//! `<｜｜DSML｜｜invoke name="workspace_search">`, neither of which is the plain
//! `<tool_call id="…">` attribute form #105 normalizes.

use std::sync::LazyLock;

use regex::Regex;

/// An open or close tag whose name contains one of the tool-call family
/// keywords, tolerant of a short run of filler characters immediately before
/// the keyword (a model's own special-token wrapper, e.g. `｜｜DSML｜｜`) and of
/// an attribute run after it. Matched case-insensitively.
///
/// Wider on purpose than #105's `TOOL_CALL_ATTR_OPEN_RE`: that pattern must
/// stay narrow because a false positive there mangles a tag before the
/// vendored parser sees it and can silently swallow a real call. A false
/// positive here only ever swaps a paragraph of chat for [`FALLBACK_REPLY`],
/// so the pattern can afford to catch more.
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?[^>\s]{0,32}(tool_calls?|tool-call|invoke|function_call)[^>]{0,64}>")
        .expect("static tool-call tag pattern must compile")
});

/// The plain-text call marker family `native_salvage::CALL_MARKERS` recovers
/// on an authorized turn (`function_call:{…}`, `tool_call:{…}`, …) — the same
/// shape is still tool-call markup, not an answer, when nothing authorized it.
static PLAIN_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(function_call|functioncall|tool_calls?)\s*:\s*\{")
        .expect("static plain-text call marker pattern must compile")
});

/// What the operator sees in place of leaked tool-call markup.
const FALLBACK_REPLY: &str =
    "I'd need to look that up — ask me again without \"Just chatting\" so I can use my tools.";

/// If `reply` looks like a text-shaped tool call rather than a chat answer,
/// replace it with [`FALLBACK_REPLY`]; otherwise return it untouched.
///
/// Callers must only apply this to the reply of a turn that actually ran with
/// `suppress_tools` set (#1725) — the fallback assumes the turn had no tool
/// available to satisfy the request, which is not a safe assumption about an
/// ordinary tool-authorized turn's reply (that path is `native_salvage`'s, not
/// this one).
pub fn guard_suppressed_reply(reply: String) -> String {
    if TAG_RE.is_match(&reply) || PLAIN_CALL_RE.is_match(&reply) {
        tracing::warn!(
            chars = reply.chars().count(),
            "[harness] chat-only turn's reply looked like a text-shaped tool call; \
             replacing it with a fallback instead of showing the operator raw markup (#2094)"
        );
        FALLBACK_REPLY.to_string()
    } else {
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact repro shape from #2094: a native model's own DSML-style
    /// special-token wrapper around `tool_calls` / `invoke` / `parameter`.
    #[test]
    fn guard_replaces_the_reported_dsml_markup() {
        let leaked = "<\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}tool_calls>\n\
             <\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}invoke name=\"workspace_search\">\n\
             <\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}parameter name=\"query\" string=\"true\">team.md</\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}parameter>\n\
             </\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}invoke>\n\
             </\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}tool_calls>"
            .to_string();
        assert_eq!(guard_suppressed_reply(leaked), FALLBACK_REPLY);
    }

    #[test]
    fn guard_replaces_a_bare_tool_call_tag() {
        let leaked =
            r#"<tool_call id="call_1">{"name":"workspace_search","arguments":{}}</tool_call>"#
                .to_string();
        assert_eq!(guard_suppressed_reply(leaked), FALLBACK_REPLY);
    }

    #[test]
    fn guard_replaces_an_invoke_tag() {
        let leaked = r#"<invoke name="workspace_search">{"query":"team.md"}</invoke>"#.to_string();
        assert_eq!(guard_suppressed_reply(leaked), FALLBACK_REPLY);
    }

    #[test]
    fn guard_replaces_a_plain_text_function_call_marker() {
        let leaked =
            r#"function_call:{"call":"read_ledger","arguments":{"ledger":"tasks"}}"#.to_string();
        assert_eq!(guard_suppressed_reply(leaked), FALLBACK_REPLY);
    }

    #[test]
    fn guard_leaves_an_ordinary_chat_reply_untouched() {
        let reply = "The growth desk is staffed by Priya and Sam.".to_string();
        assert_eq!(guard_suppressed_reply(reply.clone()), reply);
    }

    #[test]
    fn guard_leaves_empty_and_whitespace_replies_untouched() {
        assert_eq!(guard_suppressed_reply(String::new()), "");
        assert_eq!(guard_suppressed_reply("   \n".to_string()), "   \n");
    }
}
