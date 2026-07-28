//! Issue #105 — attribute-tolerant `<tool_call>` open-tag normalization.
//!
//! ## The leak
//!
//! The staging model emits tool calls as
//! `<tool_call id="call_2">{"name":…,"arguments":{…}}</tool_call>` — with an
//! `id` **attribute** on the open tag. OpenHuman's XML tool-call parser
//! (`vendor/openhuman/src/openhuman/agent/harness/parse.rs`) matches open tags
//! only against the exact bare literals `<tool_call>`, `<toolcall>`,
//! `<tool-call>`, `<invoke>` via `haystack.find(tag)`. Because each literal
//! includes the closing `>`, an open tag that carries any attribute before the
//! `>` (`<tool_call id=…>`) never matches. Only `<invoke …>` has an
//! attribute-tolerant path in the vendored parser (`find_invoke_attr_tag`).
//!
//! So `<tool_call …>` with **any** attribute falls through: no tag is matched,
//! the whole response is returned as narrative text, and no tool is dispatched.
//! That is the visible symptom of #105 — a raw `<tool_call>` block shows up in
//! company chat and the tool the model asked for never runs.
//!
//! ## The fix
//!
//! [`AttrTolerantXmlDispatcher`] wraps the vendored
//! [`XmlToolDispatcher`](oh::agent::dispatcher::XmlToolDispatcher) and overrides
//! exactly one method — [`parse_response`](ToolDispatcher::parse_response). It
//! normalizes the response text so an attribute-form open tag of the
//! `tool_call` family becomes its bare form (`<tool_call id="x">` →
//! `<tool_call>`), then hands the cleaned response to the inner dispatcher,
//! whose parser now matches it. Every other trait method delegates straight to
//! the inner dispatcher, so the prompt protocol, result formatting, and message
//! conversion are byte-identical to before.
//!
//! This restores **actual tool execution** — it is not a cosmetic strip of the
//! tag from chat. The normalization is additive: bare `<tool_call>` and
//! `<invoke …>` forms already parse and are left untouched (the regex is a
//! no-op on a bare tag, and `<invoke>` — whose attributes carry the tool name —
//! is deliberately excluded from the pattern).
//!
//! Compiled only under `feature = "openhuman"`, alongside the rest of the
//! harness.

use std::borrow::Cow;
use std::sync::LazyLock;

use openhuman_core::openhuman as oh;
use regex::Regex;

use oh::agent::dispatcher::{
    ParsedToolCall, ToolDispatcher, ToolExecutionResult, XmlToolDispatcher,
};
use oh::context::prompt::ToolCallFormat;
use oh::inference::provider::{ChatMessage, ChatResponse, ConversationMessage};
use oh::tools::{Tool, ToolSpec};

/// Matches an **open** tag of the `tool_call` family that carries one or more
/// attributes before its `>` — e.g. `<tool_call id="call_2">`,
/// `<toolcall foo='bar'>`, `<tool-call x>`. Capture group 1 is the tag name so
/// the replacement can rebuild the bare form.
///
/// Deliberately scoped:
/// - The required `\s` after the name means the pattern fires **only when
///   there is whitespace (i.e. an attribute run)** after the tag name. A bare
///   `<tool_call>` — name immediately followed by `>` — does **not** match, so
///   normalization is a true `Borrowed` no-op on the already-working forms.
/// - The same `\s` requirement excludes the plural JSON key `<tool_calls>` (the
///   `s` is not whitespace) — matching the vendored parser, which also excludes
///   it.
/// - `<invoke …>` is **not** in the alternation: the vendored parser already
///   parses its attributes, and its attributes carry the tool name, so stripping
///   them would break it.
/// - Closing tags (`</tool_call>`) never match — the `<` is followed by `/`,
///   not the tag name.
static TOOL_CALL_ATTR_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(tool_call|toolcall|tool-call)\s[^>]*>").unwrap());

/// Rewrite every attribute-form `tool_call`-family open tag in `s` to its bare
/// form. Cheap `Borrowed` no-op when the text contains no such tag.
fn normalize_tool_call_open_tags(s: &str) -> Cow<'_, str> {
    TOOL_CALL_ATTR_OPEN_RE.replace_all(s, "<${1}>")
}

/// A [`ToolDispatcher`] that tolerates attributes on `tool_call`-family open
/// tags before delegating to the vendored [`XmlToolDispatcher`].
///
/// See the module docs for the root cause (#105). Holds an inner
/// [`XmlToolDispatcher`] and overrides only
/// [`parse_response`](ToolDispatcher::parse_response); all other behaviour is
/// the inner dispatcher's, verbatim.
#[derive(Default)]
pub struct AttrTolerantXmlDispatcher(XmlToolDispatcher);

impl AttrTolerantXmlDispatcher {
    /// Build a wrapper around a fresh [`XmlToolDispatcher`].
    pub fn new() -> Self {
        Self(XmlToolDispatcher)
    }
}

impl ToolDispatcher for AttrTolerantXmlDispatcher {
    fn parse_response(&self, response: &ChatResponse) -> (String, Vec<ParsedToolCall>) {
        let original = response.text_or_empty();
        let normalized = normalize_tool_call_open_tags(original);
        match normalized {
            // No attribute-form tag present — nothing to rewrite, so hand the
            // untouched response straight to the inner parser (the common path).
            Cow::Borrowed(_) => self.0.parse_response(response),
            // At least one attribute-form open tag was stripped: rebuild the
            // response with the cleaned text (preserving every other field) and
            // let the inner parser, which now matches the bare tag, dispatch.
            Cow::Owned(cleaned) => {
                tracing::debug!(
                    "[harness] normalized attribute-form <tool_call> open tag(s) before dispatch"
                );
                let cleaned_response = ChatResponse {
                    text: Some(cleaned),
                    ..response.clone()
                };
                self.0.parse_response(&cleaned_response)
            }
        }
    }

    fn format_results(&self, results: &[ToolExecutionResult]) -> ConversationMessage {
        self.0.format_results(results)
    }

    fn prompt_instructions(&self, tools: &[Box<dyn Tool>]) -> String {
        self.0.prompt_instructions(tools)
    }

    fn prompt_instructions_for_specs(&self, specs: &[ToolSpec]) -> Option<String> {
        self.0.prompt_instructions_for_specs(specs)
    }

    fn to_provider_messages(&self, history: &[ConversationMessage]) -> Vec<ChatMessage> {
        self.0.to_provider_messages(history)
    }

    fn should_send_tool_specs(&self) -> bool {
        self.0.should_send_tool_specs()
    }

    fn tool_call_format(&self) -> ToolCallFormat {
        self.0.tool_call_format()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`ChatResponse`] carrying `text` the way the hosted provider does
    /// (`chat_response_from_payload` in `provider.rs:585`): text set, no native
    /// tool calls, no usage, no reasoning.
    fn response(text: &str) -> ChatResponse {
        ChatResponse {
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    /// The smoking gun (#105): an attribute-form `<tool_call id="…">` open tag.
    ///
    /// Pre-fix, the vendored parser matched no tag, so `calls` was empty and the
    /// raw `<tool_call …>` block was returned as narrative text (the leak). With
    /// the wrapper, the attribute is stripped, the tool is dispatched, and no
    /// raw tag survives in the returned text.
    #[test]
    fn attribute_form_tool_call_is_parsed_and_dispatched() {
        let dispatcher = AttrTolerantXmlDispatcher::default();
        let raw = r#"<tool_call id="call_2">{"name":"write_file","arguments":{"path":"x","content":"y"}}</tool_call>"#;

        let (text, calls) = dispatcher.parse_response(&response(raw));

        assert_eq!(calls.len(), 1, "the attribute-form call must dispatch");
        assert_eq!(calls[0].name, "write_file");
        assert!(
            !text.contains("<tool_call"),
            "no raw <tool_call tag may leak into the narrative text: {text:?}"
        );
    }

    /// The already-working bare form must still parse unchanged (no regression):
    /// the normalization is a no-op on a tag with no attributes.
    #[test]
    fn bare_tool_call_still_parses() {
        let dispatcher = AttrTolerantXmlDispatcher::default();
        let raw = r#"<tool_call>{"name":"foo","arguments":{}}</tool_call>"#;

        let (_text, calls) = dispatcher.parse_response(&response(raw));

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "foo");
    }

    /// The `<invoke name="…">` attribute form (whose attributes carry the tool
    /// name) must be delegated untouched — proving the wrapper does not strip
    /// `invoke` attributes and break the vendored invoke parser.
    #[test]
    fn invoke_attribute_form_still_parses() {
        let dispatcher = AttrTolerantXmlDispatcher::default();
        let raw = r#"<invoke name="bar">{"arguments":{}}</invoke>"#;

        let (_text, calls) = dispatcher.parse_response(&response(raw));

        assert_eq!(calls.len(), 1, "invoke must still dispatch: {calls:?}");
        assert_eq!(calls[0].name, "bar");
    }

    /// Plain narrative text with no tags dispatches nothing and is returned
    /// unchanged.
    #[test]
    fn plain_text_is_untouched() {
        let dispatcher = AttrTolerantXmlDispatcher::default();
        let raw = "Just a normal sentence with no tool calls.";

        let (text, calls) = dispatcher.parse_response(&response(raw));

        assert!(calls.is_empty());
        assert_eq!(text.trim(), raw);
    }

    /// Direct check on the normalizer: attribute forms collapse to bare, bare is
    /// a no-op (`Borrowed`), `<invoke …>` and closing tags are left alone.
    #[test]
    fn normalizer_scopes_correctly() {
        // Attribute form → bare.
        assert_eq!(
            normalize_tool_call_open_tags(r#"<tool_call id="call_2">body</tool_call>"#),
            "<tool_call>body</tool_call>"
        );
        // Bare open tag → borrowed no-op.
        assert!(matches!(
            normalize_tool_call_open_tags("<tool_call>body</tool_call>"),
            Cow::Borrowed(_)
        ));
        // The plural JSON key is not a tool_call open tag → untouched.
        assert!(matches!(
            normalize_tool_call_open_tags("<tool_calls>[]</tool_calls>"),
            Cow::Borrowed(_)
        ));
        // invoke is excluded → untouched.
        assert!(matches!(
            normalize_tool_call_open_tags(r#"<invoke name="bar">x</invoke>"#),
            Cow::Borrowed(_)
        ));
        // Hyphen + underscore variants both collapse.
        assert_eq!(
            normalize_tool_call_open_tags(r#"<tool-call x="1">b</tool-call>"#),
            "<tool-call>b</tool-call>"
        );
        assert_eq!(
            normalize_tool_call_open_tags(r#"<toolcall y='2'>b</toolcall>"#),
            "<toolcall>b</toolcall>"
        );
    }
}
