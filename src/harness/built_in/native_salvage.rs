//! Recovering a tool call a native-tool-calling model wrote as prose.
//!
//! # The leak
//!
//! On the native transport the harness sends `tools` in the request and reads
//! `message.tool_calls` back. A model that honours that contract is the
//! reliable path and nothing here runs. A model that *sometimes* honours it is
//! the problem this module exists for: the same agent, in the same thread, one
//! turn apart, was observed executing `read_ledger` correctly and then writing
//!
//! ```text
//! function_call:{"id":"call_3rY…","call":"read_ledger",
//!                "arguments":{"ledger":"tasks","query":"2026-09-01"}}
//! ```
//!
//! into the message body instead. The tool never ran, the sentence the model
//! had already written ("I'll check the tasks ledger…") became a promise it
//! could not keep, and the operator saw raw JSON in company chat.
//!
//! # Why this is in the provider, and not in a dispatcher
//!
//! OpenHuman has a `ToolDispatcher` seam with a text-parsing fallback, and
//! [`AttrTolerantXmlDispatcher`](super::tool_dispatcher::AttrTolerantXmlDispatcher)
//! extends it for #105's attribute-form open tags. That seam is **not on this
//! turn path**. Since openhuman #4249 removed the legacy engine, every turn runs
//! through the tinyagents harness, and its loop takes a turn's tool calls from
//! exactly one place — `response.tool_calls()` on the structured
//! [`ModelResponse`](tinyinference::model::ModelResponse)
//! (`agent_loop/run_loop.rs`). It never parses model text. The injected
//! `ToolDispatcher` still renders the transcript and the prompt protocol, but
//! its `parse_response` is reached only by the end-of-turn wrap-up check.
//!
//! So the last point at which a text-shaped call can still become a real one is
//! where this crate turns the wire payload into a `ModelResponse`:
//! [`model_response_from_payload`](super::provider). That is where this is
//! called from, and it is why a fix at the dispatcher seam would have parsed
//! perfectly and executed nothing.
//!
//! # What the vendored text parser would not have caught anyway
//!
//! Two independent reasons, both of which the observed shape trips:
//!
//! * **The name key.** It resolves a call's name from `name` or
//!   `function.name`. `call` — what the model above used — is not a name key it
//!   knows, so the object parses as JSON and is then rejected as not a call.
//! * **The surrounding prose.** Its whole-response JSON path requires the
//!   *entire* trimmed response to be one JSON value. An object embedded in a
//!   sentence, or in a fenced block under a lead-in line, only survives when a
//!   recognised tag marks it — and here nothing does.
//!
//! Widening either of those in a shared parser is what its own comments refuse,
//! and correctly: `{"name":"Alice","input":"hi"}` is an ordinary model reply,
//! and a parser with no idea which tools exist cannot tell it from a call.
//!
//! # The tag dialects
//!
//! The leak has a second shape, and it is not JSON at all: a model writing its
//! call as **markup**, either Claude's `<invoke name="…"><parameter name="…">`
//! form or a vendor dialect that wraps those same tags in a marker glyph —
//! DeepSeek's is
//!
//! ```text
//! <｜｜DSML｜｜tool_calls>
//! <｜｜DSML｜｜invoke name="workspace_search">
//! <｜｜DSML｜｜parameter name="query" string="true">team</｜｜DSML｜｜parameter>
//! </｜｜DSML｜｜invoke>
//! </｜｜DSML｜｜tool_calls>
//! ```
//!
//! observed in company chat against a `chat-v1`-tier model. Neither shape has a
//! `{` in it, so the object scan below never sees a candidate and the whole
//! turn's tool calls are lost — the agent then narrates results it never
//! received.
//!
//! tinyagents *can* parse the undecorated form
//! (`tool_calling::parse::TOOL_CALL_OPEN_TAGS`), which is a large part of why
//! this gap read as covered. It is not: that parser is reachable only through
//! OpenHuman's `ToolDispatcher`, and this turn path never asks it anything (see
//! above). The decorated form it would miss regardless — its open-tag table
//! matches `<invoke` literally, and the marker sits between the `<` and the
//! keyword.
//!
//! So the scan here matches a tag keyword through an optional **decoration**:
//! the run between `<` and the keyword, which must be short, unbroken, and
//! carry at least one non-ASCII character. That last condition is what keeps
//! `<invoker>` and `<parameterise>` from matching — a dialect marks its markers,
//! and an ASCII run in that position is an ordinary tag name.
//!
//! # What licenses the widening here
//!
//! This runs where the turn's own tool schemas are in hand. A candidate is
//! recovered only when its resolved name **is a tool this turn offered the
//! model**, which is a far stronger marker than any key-shape heuristic — and
//! it is a marker a shared parser structurally cannot use. It also makes the
//! recovery inert by construction on a turn that suppressed tools. Every other
//! reject stays as strict as the vendored path:
//!
//! * an argument key must be present, or the object must carry nothing but the
//!   name (and an optional `id`/`type`), so prose *about* a tool cannot fire it;
//! * arguments must resolve to a JSON object, stringified or not;
//! * a turn that already produced structured calls is never touched — this does
//!   not compete with the native channel, it only catches what that channel
//!   dropped.
//!
//! # This is a host-level compensation, and should not outlive its cause
//!
//! The defect underneath is upstream, in tinyagents. `NativeDialect` already
//! carries a text fallback for exactly this case — empty structured calls, parse
//! the text instead — and nothing in tinyagents ever reaches it, because its
//! dialects are only callable through OpenHuman's `ToolDispatcher`. Inside
//! tinyagents that fallback is dead code, which is why a gap this plain stayed
//! invisible: it looks covered.
//!
//! The recovery below would sit more correctly in tinyagents' own loop, and the
//! usual objection does not apply there. A free parser cannot validate a name
//! against a belt it has no access to, but the loop can: `self.tools.schemas()`
//! and the `response.tool_calls()` read that gives up on the turn are the same
//! struct, a few hundred lines apart. Fixed there, every consumer of the runtime
//! gets it rather than this host alone.
//!
//! It is here because tinyagents is a submodule of a submodule: landing it
//! upstream is three sequenced pull requests and two pointer bumps, against a
//! runtime other products depend on, while companies are shipping broken turns
//! now. When the upstream fix lands, **delete this** — a compensation kept past
//! its cause is just a second implementation of the same rule, and the two will
//! disagree eventually. `refuse_approval_siblings` in
//! [`provider`](super::provider) is the part that stays either way: the approval
//! boundary is this crate's policy, not the runtime's.
//!
//! # Ids are synthesized, and that is load-bearing
//!
//! A recovered call has no provider id, and the agent loop pairs each tool
//! result back to its opener by id. Without one, the result answers no call:
//! the transcript's assistant opener and the tool message disagree, the cycle is
//! dropped on its way back to the wire, and the model never learns that its tool
//! ran or what it returned — so it re-narrates the same intention on the next
//! iteration. The tool still runs, and the operator is still billed for it. That
//! is the most misleading failure available here, and giving both halves the
//! same synthesized id is what avoids it.

use std::borrow::Cow;
use std::collections::BTreeSet;

use serde_json::{Map, Value};
use tinyinference::model::ToolChoice;
use tinyinference::tool::{ToolCall, ToolSchema};

/// Object keys that name a tool **and say the object is a call**.
///
/// `call`, `tool`, `tool_name`, `function_name` are drift observed in the wild,
/// and none of them is a word ordinary JSON uses for anything else. A bare
/// object keyed this way is a request, not a description of one.
const CALL_NAME_KEYS: &[&str] = &["call", "tool", "tool_name", "function_name"];

/// Every key that can carry a tool name, read only once intent is established.
///
/// The extra one here is `name` — an ordinary English word, and the reason
/// [`CALL_NAME_KEYS`] exists separately. A model asked to *document* an offered
/// tool writes exactly the shape a call has:
///
/// ```text
/// Example: {"name":"write_file","arguments":{"path":"demo","content":"…"}}
/// ```
///
/// Belt membership proves the tool exists, not that this object asks for it to
/// run — and running it would mutate a workspace on the strength of a sentence
/// that said "example" (Codex review on #2011).
const NAME_KEYS: &[&str] = &["name", "call", "tool", "tool_name", "function_name"];

/// Object keys that may carry the tool **arguments**, in priority order.
///
/// Deliberately the same list the vendored text parser uses, so a call
/// recovered here and a call recovered there read their arguments identically.
const ARG_KEYS: &[&str] = &["arguments", "args", "parameters", "params", "input"];

/// Keys allowed to sit beside the name on a **no-argument** call.
///
/// A model writing a call with no arguments emits `{"call":"list_desks"}`, and
/// rejecting that would leave exactly the tools that need no input
/// unsalvageable. Permitting it costs a false positive on a bare
/// `{"name":"<a tool name>"}` object — so the object must carry nothing else,
/// which no ordinary JSON reply about a tool satisfies.
const BARE_CALL_ALLOWED_KEYS: &[&str] = &["id", "type", "index"];

/// Markers a model writes immediately before the object, which belong to the
/// call rather than to the narrative and so are stripped with it.
///
/// Matched case-insensitively, longest first so `function_call:` wins over
/// `call:`.
const CALL_MARKERS: &[&str] = &["function_call:", "tool_call:", "functioncall:", "call:"];

/// The tag keyword naming one call in the markup dialects.
const INVOKE_TAG: &str = "invoke";

/// The tag keyword naming one argument of a markup-dialect call.
const PARAMETER_TAG: &str = "parameter";

/// The envelope some dialects wrap their calls in. It carries no call of its
/// own; it is matched only so the leftover tags come out of the narrative with
/// the calls they wrapped.
const TOOL_CALLS_TAG: &str = "tool_calls";

/// How long a decoration between `<` and a tag keyword may be, in bytes.
///
/// `｜｜DSML｜｜` is 14. The cap is what keeps a scan for `<…invoke` from
/// reaching across a paragraph of prose to a keyword that has nothing to do
/// with the `<` it started from.
const MAX_TAG_DECORATION: usize = 32;

/// The names this turn actually **authorized** the model to call, as the set
/// the recovery validates against.
///
/// Taken from the turn's own `ModelRequest`, not from a build-time belt: a turn
/// that suppresses tools (`#1725`'s chat/small-talk path) advertises none, and
/// this set is then empty — so the recovery is inert exactly when the model was
/// never invited to call anything, with no separate flag to keep in step.
///
/// `tool_choice` narrows it, because the schemas alone are not the
/// authorization (Codex review on #2011). A request that sends tools *and*
/// `tool_choice: "none"` has told the model not to call any of them, and a
/// request naming one tool has authorized exactly that one; recovering against
/// the full schema list in either case would dispatch something this turn
/// explicitly did not ask for.
pub fn authorized_tool_names(tools: &[ToolSchema], choice: &ToolChoice) -> BTreeSet<String> {
    match choice {
        // Told not to call anything. Nothing is recoverable, whatever the
        // schemas say.
        ToolChoice::None => BTreeSet::new(),
        // Pinned to one tool: it is the only authorization this turn carries,
        // and only if it is actually on the wire.
        ToolChoice::Tool(name) => tools
            .iter()
            .map(|tool| tool.name.clone())
            .filter(|offered| offered == name)
            .collect(),
        ToolChoice::Auto | ToolChoice::Required => {
            tools.iter().map(|tool| tool.name.clone()).collect()
        }
    }
}

/// Recover tool calls a model wrote into `content` as text, when the turn's
/// structured `tool_calls` came back empty.
///
/// Returns the narrative with the recovered calls (and their markers and
/// now-empty code fences) removed, or `None` when nothing was recovered — in
/// which case the caller must leave `content` exactly as it was.
///
/// `offered` is what licenses reading an object out of prose at all; with an
/// empty set this always returns `None`.
pub fn recover_text_tool_calls(
    content: &str,
    offered: &BTreeSet<String>,
) -> Option<(String, Vec<ToolCall>)> {
    if offered.is_empty() || content.is_empty() {
        return None;
    }
    let (cleaned, calls) = salvage(content, offered)?;
    tracing::warn!(
        recovered = calls.len(),
        tools = ?calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        "[harness] the model wrote a tool call as text instead of using the native \
         tool-calling channel; recovered it from the message body. A model salvaged \
         on most turns is not honouring `tools` — consider mapping this tier to one \
         that does (Settings → Inference)."
    );
    Some((cleaned, calls))
}

/// The id given to a recovered call, by position within the response.
///
/// Both halves of the cycle — the assistant opener and the tool result — are
/// derived from this same [`ParsedToolCall`], so the two id sets match and the
/// cycle survives `pair_tool_cycles`. See the module docs.
fn salvaged_call_id(index: usize) -> String {
    format!("salvaged_call_{index}")
}

/// Recover every text-shaped call to a **known** tool from `text`.
///
/// Returns the narrative with the recovered calls (and their markers and now-
/// empty code fences) removed, or `None` when nothing was recovered — in which
/// case the caller must return the original text untouched.
fn salvage(text: &str, known: &BTreeSet<String>) -> Option<(String, Vec<ToolCall>)> {
    let mut found: Vec<Candidate> = Vec::new();

    for (start, end) in json_object_spans(text) {
        let Ok(value) = serde_json::from_str::<Value>(&text[start..end]) else {
            continue;
        };
        // Resolved before the accept decision, not after: an explicit marker is
        // one of the two things that can license a bare `name`-keyed object.
        let cut = marker_start(text, start);
        let marked = cut != start;
        let Some((name, arguments)) = as_known_call(&value, known, marked) else {
            continue;
        };
        found.push(Candidate {
            start: cut,
            end,
            name,
            arguments,
        });
    }

    found.extend(tag_call_candidates(text, known));

    // Left to right, and never twice over the same bytes: a JSON object written
    // as a `<parameter>` body is that call's argument, not a second call
    // beside it. The tag span opens first, so ordering by start is what makes
    // the enclosing call win.
    found.sort_by_key(|candidate| candidate.start);

    let mut calls = Vec::new();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut consumed = 0usize;
    for candidate in found {
        if candidate.start < consumed {
            continue;
        }
        consumed = candidate.end;
        calls.push(ToolCall {
            id: salvaged_call_id(calls.len()),
            name: candidate.name,
            arguments: candidate.arguments,
            // Recovered from a well-formed object whose arguments already
            // resolved to a JSON object, so there is nothing to declare
            // malformed. `Some(_)` here is the provider's channel for "the
            // model asked for this and its body would not parse".
            invalid: None,
        });
        cuts.push((candidate.start, candidate.end));
    }

    if calls.is_empty() {
        return None;
    }
    // Only once something was recovered: an envelope tag on a turn that
    // recovered nothing belongs to whatever the model was actually writing, and
    // deleting it would edit a reply this module never understood.
    cuts.extend(envelope_tag_spans(text));
    cuts.sort_by_key(|(start, _)| *start);
    Some((cut_and_tidy(text, &cuts), calls))
}

/// One recovery under consideration: the bytes it would remove, and the call
/// they would become.
struct Candidate {
    /// Where the removal starts — the object's own start, extended over a
    /// marker, or the opening tag.
    start: usize,
    /// One past the last byte of the removal.
    end: usize,
    name: String,
    arguments: Value,
}

/// A located tag: where it starts, where the text after its `>` resumes, and
/// the attribute run between the keyword and the `>`.
struct Tag<'a> {
    start: usize,
    after: usize,
    attrs: &'a str,
}

/// Every `<invoke name="…">…</invoke>` call to a **known** tool in `text`,
/// left to right, in either the bare or a decorated dialect.
///
/// An `<invoke>` is an unambiguous call marker in the way the `function` key
/// is, so a parameterless one recovers with empty arguments rather than needing
/// the bare-object check the `name`-keyed JSON shape needs. Belt membership is
/// still required: the tag says *a* call, the belt says *this* call.
fn tag_call_candidates(text: &str, known: &BTreeSet<String>) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut from = 0usize;

    while let Some(open) = find_tag(text, from, INVOKE_TAG, false) {
        // Advanced past the opening tag whatever happens below, so a tag this
        // rejects cannot be rematched on the next pass.
        from = open.after;
        // An unclosed tag is left verbatim rather than guessed at: the model
        // was cut off mid-call, and inventing its end would run a tool against
        // arguments nobody finished writing.
        let Some(close) = find_tag(text, open.after, INVOKE_TAG, true) else {
            break;
        };
        from = close.after;
        let Some(name) = attr(open.attrs, "name") else {
            continue;
        };
        if !known.contains(&name) {
            continue;
        }
        found.push(Candidate {
            start: open.start,
            end: close.after,
            name,
            arguments: tag_call_arguments(&text[open.after..close.start]),
        });
    }

    found
}

/// The `<parameter name="…">…</parameter>` children of one call body, as its
/// arguments object.
fn tag_call_arguments(body: &str) -> Value {
    let mut arguments = Map::new();
    let mut from = 0usize;

    while let Some(open) = find_tag(body, from, PARAMETER_TAG, false) {
        from = open.after;
        let Some(close) = find_tag(body, open.after, PARAMETER_TAG, true) else {
            break;
        };
        from = close.after;
        let Some(name) = attr(open.attrs, "name") else {
            continue;
        };
        arguments.insert(name, parameter_value(&body[open.after..close.start], open.attrs));
    }

    Value::Object(arguments)
}

/// One `<parameter>` body as a JSON value.
///
/// The body is text on the wire whatever the argument's declared type is, so a
/// structured argument only survives by being read back as JSON. DeepSeek's
/// dialect says which is which with `string="true"`, and that wins outright —
/// without it a query of `2026` becomes a number and a strictly-typed tool
/// rejects a call the model got right.
fn parameter_value(raw: &str, attrs: &str) -> Value {
    let raw = raw.trim();
    if attr(attrs, "string").is_some_and(|declared| declared.eq_ignore_ascii_case("true")) {
        return Value::String(raw.to_string());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value,
        Err(_) => Value::String(raw.to_string()),
    }
}

/// The spans of every `<tool_calls>` / `</tool_calls>` envelope tag, so the
/// wrapper does not outlive the calls it wrapped.
fn envelope_tag_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for closing in [false, true] {
        let mut from = 0usize;
        while let Some(tag) = find_tag(text, from, TOOL_CALLS_TAG, closing) {
            spans.push((tag.start, tag.after));
            from = tag.after;
        }
    }
    spans
}

/// The next `<keyword …>` (or `</keyword>`) at or after `from`.
fn find_tag<'a>(text: &'a str, from: usize, keyword: &str, closing: bool) -> Option<Tag<'a>> {
    let mut search = from;
    while let Some(offset) = text.get(search..)?.find('<') {
        let start = search + offset;
        // One byte past this `<`, so a `<` that opens nothing does not stall
        // the scan on itself.
        search = start + 1;
        if let Some(tag) = tag_at(text, start, keyword, closing) {
            return Some(tag);
        }
    }
    None
}

/// Read the tag `keyword` opens at `start`, or `None` if that is not the tag.
fn tag_at<'a>(text: &'a str, start: usize, keyword: &str, closing: bool) -> Option<Tag<'a>> {
    let rest = text.get(start + 1..)?;
    let rest = match closing {
        true => rest.strip_prefix('/')?,
        // A closing tag is not an opening one, so `</invoke>` must not answer
        // a search for `<invoke>` — otherwise a call's own end reads as the
        // start of another and the scan never terminates the span.
        false if rest.starts_with('/') => return None,
        false => rest,
    };
    let rest = undecorated(rest, keyword)?.strip_prefix(keyword)?;
    let stop = rest.find('>')?;
    let attrs = &rest[..stop];
    // The keyword has to end where the tag name ends: `<invoker>` is not an
    // `<invoke>` tag with the attribute `r`.
    if attrs
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    // A closing tag carries no attributes. Anything in that position means this
    // is not the tag it looks like.
    if closing && !attrs.trim().is_empty() {
        return None;
    }
    // A `<` inside what was taken for an attribute run means the `>` belongs to
    // a later tag and this one was never closed.
    if attrs.contains('<') {
        return None;
    }
    let after = text.len() - rest[stop + 1..].len();
    Some(Tag { start, after, attrs })
}

/// `rest` positioned at `keyword`, skipping a dialect's decoration if one sits
/// in front of it.
///
/// The decoration must be short, unbroken, and carry a non-ASCII character —
/// the marker glyph a dialect brands its tags with. Without that last
/// condition every `<parameterise>` in an ordinary sentence becomes a
/// `<parameter>` tag. See the module docs.
fn undecorated<'a>(rest: &'a str, keyword: &str) -> Option<&'a str> {
    if rest.starts_with(keyword) {
        return Some(rest);
    }
    let at = rest.find(keyword)?;
    let decoration = &rest[..at];
    if decoration.len() > MAX_TAG_DECORATION
        || decoration.chars().any(|ch| ch.is_whitespace() || ch == '>' || ch == '<')
        || decoration.is_ascii()
    {
        return None;
    }
    Some(&rest[at..])
}

/// The value of the `name="…"` style attribute `attr` in a tag's attribute run.
///
/// Both quote styles, because a dialect that writes its tags by hand is not
/// bound to either. An attribute whose name is the tail of another
/// (`string` inside `substring`) does not match: the character in front of it
/// must not continue a word.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(offset) = attrs.get(from..)?.find(name) {
        let at = from + offset;
        from = at + name.len();
        let standalone = attrs[..at]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_' && ch != '-');
        if !standalone {
            continue;
        }
        let Some(rest) = attrs[from..].trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => quote,
            _ => continue,
        };
        let value = &rest[quote.len_utf8()..];
        let Some(end) = value.find(quote) else {
            continue;
        };
        return Some(value[..end].to_string());
    }
    None
}

/// Interpret one JSON value as a call to a tool in `known`.
///
/// `None` for anything that is not unambiguously a call: an unknown or absent
/// name, arguments that are present but not an object, or a bare name-only
/// object carrying unrelated keys. Returns the resolved `(name, arguments)`.
fn as_known_call(value: &Value, known: &BTreeSet<String>, marked: bool) -> Option<(String, Value)> {
    // `{"type":"function","function":{"name":…,"arguments":…}}` — the OpenAI
    // wire shape written out longhand. The `function` key is an unambiguous
    // marker, so the inner object is read directly.
    if let Some(function) = value.get("function").and_then(Value::as_object) {
        let name = first_str(function, NAME_KEYS)?;
        if !known.contains(&name) {
            return None;
        }
        // A no-argument call is legitimate here without the bare-object check
        // the branch below needs: `function` is itself an unambiguous tool-call
        // marker, so there is no ordinary-JSON reading to protect against.
        return match resolve_args(function) {
            Arguments::Object(arguments) => Some((name, arguments)),
            Arguments::Missing => Some((name, empty_object())),
            Arguments::Unusable => None,
        };
    }

    let object = value.as_object()?;
    let name = first_str(object, NAME_KEYS)?;
    if !known.contains(&name) {
        return None;
    }
    // A bare object keyed only by the generic `name` is the shape a model uses
    // to *describe* a tool as much as to call one, so belt membership alone
    // must not dispatch it. Either the object says it is a call by the key it
    // used, or the model said so with a marker in front of it. See
    // [`NAME_KEYS`].
    let says_it_is_a_call = first_str(object, CALL_NAME_KEYS).is_some();
    if !says_it_is_a_call && !marked {
        return None;
    }

    match resolve_args(object) {
        // An explicit argument key resolving to an object: a call.
        Arguments::Object(arguments) => Some((name, arguments)),
        // No argument key at all. Only a call when the object carries nothing
        // beyond the name and the incidental keys a model stamps on it —
        // otherwise this is ordinary JSON that happens to name a tool.
        Arguments::Missing if is_bare_call(object) => Some((name, empty_object())),
        Arguments::Missing | Arguments::Unusable => None,
    }
}

/// What an object's argument key resolved to.
///
/// `Missing` and `Unusable` are kept apart because they mean opposite things:
/// nothing was claimed, versus something was claimed and could not be honoured.
/// Collapsing them would let a call whose arguments are a sentence run with an
/// empty object — the tool would execute against input the model never asked
/// for, which is worse than not running it.
enum Arguments {
    /// No key from [`ARG_KEYS`] is present.
    Missing,
    /// A key is present but does not resolve to a JSON object.
    Unusable,
    /// A key is present and resolved.
    Object(Value),
}

/// The first present [`NAME_KEYS`] entry whose value is a non-empty string.
fn first_str(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Resolve the first present [`ARG_KEYS`] entry, tolerating the stringified
/// form providers put on the wire.
fn resolve_args(object: &Map<String, Value>) -> Arguments {
    let Some(raw) = ARG_KEYS.iter().find_map(|key| object.get(*key)) else {
        return Arguments::Missing;
    };
    match raw {
        Value::Object(_) => Arguments::Object(raw.clone()),
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(value @ Value::Object(_)) => Arguments::Object(value),
            _ => Arguments::Unusable,
        },
        _ => Arguments::Unusable,
    }
}

/// Whether `object` is a name and nothing of consequence besides — the
/// no-argument call shape. See [`BARE_CALL_ALLOWED_KEYS`].
fn is_bare_call(object: &Map<String, Value>) -> bool {
    object.keys().all(|key| {
        NAME_KEYS.contains(&key.as_str()) || BARE_CALL_ALLOWED_KEYS.contains(&key.as_str())
    })
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// Extend a span's start backwards over a call marker the model wrote in front
/// of the object (`function_call:`), and over the whitespace between them.
///
/// Only ever moves within the current line: a marker on an earlier line is
/// narrative, not part of the call.
fn marker_start(text: &str, start: usize) -> usize {
    let before = &text[..start];
    let mut trimmed = before.trim_end_matches([' ', '\t']);
    // A model that writes the marker on its own line puts a break between it
    // and the object. That break is part of the call's layout, not narrative,
    // so allow the gap to carry exactly one line ending — leaving the marker
    // behind would put `function_call:` in the operator's reply, which is a
    // smaller version of the symptom this module exists to remove (CodeRabbit
    // review on #2011). A *blank* line is where it stops: two breaks mean the
    // marker belongs to the paragraph above rather than to this object.
    if let Some(head) = trimmed.strip_suffix('\n') {
        let head = head.strip_suffix('\r').unwrap_or(head);
        if !head.ends_with(['\n', '\r']) {
            trimmed = head.trim_end_matches([' ', '\t']);
        }
    }
    for marker in CALL_MARKERS {
        let Some(cut) = trimmed.len().checked_sub(marker.len()) else {
            continue;
        };
        // The markers are ASCII, but the narrative in front of one is not
        // necessarily — a sentence ending in `…` would put `cut` inside a
        // multi-byte character, and slicing there panics before the comparison
        // that would have rejected it can run.
        if !trimmed.is_char_boundary(cut) {
            continue;
        }
        if !trimmed[cut..].eq_ignore_ascii_case(marker) {
            continue;
        }
        // The marker has to be a word of its own. Without this, the `call:`
        // marker matches the tail of an ordinary word — `Recall: {…}` cuts from
        // inside "Recall" and leaves the operator reading "Re" (Codex review on
        // #2011).
        let delimited = trimmed[..cut]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        if delimited {
            return cut;
        }
    }
    start
}

/// Remove `cuts` from `text`, then tidy what removing them exposed: fenced
/// blocks left holding nothing, and the blank runs left behind.
///
/// `cuts` arrive in ascending order and never overlap, because
/// [`json_object_spans`] yields non-nested spans left to right.
fn cut_and_tidy(text: &str, cuts: &[(usize, usize)]) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut copied_to = 0;
    for (start, end) in cuts {
        // `marker_start` extends a span's start leftwards, which in principle
        // could reach back into the previous cut (`}function_call:{`). Clamping
        // keeps that from slicing backwards and panicking.
        let start = (*start).max(copied_to);
        kept.push_str(&text[copied_to..start]);
        copied_to = (*end).max(copied_to);
    }
    kept.push_str(&text[copied_to..]);
    collapse_blank_runs(drop_empty_fences(&kept).as_ref())
}

/// Drop fenced blocks whose body is now whitespace, which is what a fence
/// wrapping only the removed call becomes.
///
/// Scans fence markers pairwise — 1st opens, 2nd closes — and keeps any pair
/// with real content between them, so a fenced code block elsewhere in the
/// narrative is untouched. An unpaired trailing fence is left verbatim rather
/// than guessed at.
fn drop_empty_fences(text: &str) -> Cow<'_, str> {
    if !text.contains("```") {
        return Cow::Borrowed(text);
    }
    let fences: Vec<usize> = text.match_indices("```").map(|(index, _)| index).collect();
    let mut out: Option<String> = None;
    let mut copied_to = 0;
    for [open, close] in fences.as_chunks::<2>().0.iter().copied() {
        // The opener runs to the end of its own line — it may carry a language
        // tag, which is not body content — so the body is what follows the
        // first newline. A fence with no newline before its close has no body.
        // A fence pair with no newline between them is an inline code span,
        // not a block — ``` `` `do not delete` `` ``` has no opener line and no
        // body, and treating it as an empty block deletes text the salvage
        // never touched (Codex review on #2011). Only a pair whose opener ends
        // in a newline is a block this may drop.
        let after_open = &text[open + 3..close];
        let Some((_, body)) = after_open.split_once('\n') else {
            continue;
        };
        if !body.trim().is_empty() {
            continue;
        }
        let buffer = out.get_or_insert_with(String::new);
        buffer.push_str(&text[copied_to..open]);
        copied_to = close + 3;
    }
    match out {
        Some(mut buffer) => {
            buffer.push_str(&text[copied_to..]);
            Cow::Owned(buffer)
        }
        None => Cow::Borrowed(text),
    }
}

/// Collapse runs of blank lines to a single paragraph break and trim the ends,
/// so a removed call does not leave a hole in the narrative.
///
/// Line-wise rather than character-wise on purpose: a character scan that skips
/// whitespace following a newline also eats the **leading indentation** of the
/// next line, which silently reflows every indented list and code block the
/// narrative contains. Only a line that is entirely whitespace is dropped here;
/// a line with content keeps its indentation byte for byte.
fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_blank = false;
    let mut wrote_any = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            pending_blank = true;
            continue;
        }
        if wrote_any {
            out.push('\n');
            if pending_blank {
                out.push('\n');
            }
        }
        out.push_str(line);
        pending_blank = false;
        wrote_any = true;
    }
    // Only the ends are trimmed; interior indentation was never touched.
    out.trim_matches(['\n', ' ', '\t']).to_string()
}

/// Byte spans of every **top-level** balanced `{…}` run in `text`, left to
/// right.
///
/// String-aware: a brace inside a JSON string, and a quote escaped inside one,
/// do not move the depth — without which `{"path":"a{b"}` ends the span in the
/// wrong place and the candidate fails to parse for a reason that has nothing
/// to do with whether it was a call.
///
/// Nested objects are not yielded separately: the outermost run is the
/// candidate, and its children are reached by parsing it.
fn json_object_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' if depth > 0 => in_string = true,
            '{' => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    spans.push((start, index + ch.len_utf8()));
                }
            }
            _ => {}
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn belt() -> BTreeSet<String> {
        ["read_ledger", "list_desks", "write_file"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// What the provider does with a text-only response: hand the content and
    /// the turn's offered tools to the recovery.
    fn recover(text: &str) -> (String, Vec<ToolCall>) {
        match recover_text_tool_calls(text, &belt()) {
            Some((cleaned, calls)) => (cleaned, calls),
            // Nothing recovered: the caller leaves the content untouched.
            None => (text.to_string(), Vec::new()),
        }
    }

    /// The smoking gun, verbatim from the 1/9 QA round: a `function_call:`
    /// marker, a `call` name key, and prose on either side.
    #[test]
    fn function_call_marker_with_call_name_key_is_recovered() {
        let raw = "Let me proceed with querying the tasks ledger. \
                   function_call:{\"id\":\"call_3rY\",\"call\":\"read_ledger\",\
                   \"arguments\":{\"ledger\":\"tasks\",\"query\":\"2026-09-01\"}}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1, "the call must be recovered");
        assert_eq!(calls[0].name, "read_ledger");
        assert_eq!(calls[0].arguments["ledger"], "tasks");
        assert_eq!(
            calls[0].id, "salvaged_call_0",
            "a synthesized id is what keeps the cycle paired"
        );
        assert!(calls[0].invalid.is_none());
        assert!(
            !text.contains("function_call"),
            "the marker and object must not survive in chat: {text:?}"
        );
        assert!(
            text.starts_with("Let me proceed"),
            "the narrative must survive: {text:?}"
        );
    }

    /// The other observed shape: a lead-in sentence and a fenced block. The
    /// fence must go with the call rather than being left empty in chat.
    #[test]
    fn fenced_block_is_recovered_and_the_empty_fence_removed() {
        let raw = "I'll share the company's task board.\n\n```json\n\
                   {\n  \"call\": \"read_ledger\",\n  \"arguments\": {\n    \
                   \"ledger\": \"tasks\"\n  }\n}\n```";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_ledger");
        assert_eq!(text, "I'll share the company's task board.");
    }

    /// The false positive a shared parser refuses to risk, and the reason this
    /// one checks the turn's tools: an ordinary JSON answer naming a person.
    #[test]
    fn a_plain_json_reply_is_not_a_call() {
        let raw = "{\"name\":\"Alice\",\"input\":\"hi\"}";
        let (text, calls) = recover(raw);

        assert!(calls.is_empty(), "an unoffered name must never dispatch");
        assert_eq!(text, raw, "and the reply must reach chat unchanged");
    }

    /// Prose *about* a tool, carrying its name in a JSON object with unrelated
    /// keys, is not a call to it.
    #[test]
    fn narrating_a_tool_name_is_not_a_call() {
        let raw = "The step ran: {\"name\":\"read_ledger\",\"duration_ms\":51}";
        let (_, calls) = recover(raw);

        assert!(
            calls.is_empty(),
            "an offered name with unrelated keys and no arguments is narrative"
        );
    }

    /// A tool that takes no arguments still has to be callable, or the recovery
    /// would work for every tool except the simplest ones.
    #[test]
    fn a_bare_no_argument_call_is_recovered() {
        let raw = "{\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_desks");
        assert_eq!(calls[0].arguments, empty_object());
        assert!(text.is_empty(), "the whole message was the call: {text:?}");
    }

    /// The OpenAI wire shape written out longhand, including the stringified
    /// `arguments` providers use.
    #[test]
    fn the_function_wrapper_shape_with_stringified_arguments_is_recovered() {
        let raw = "Writing that file now. {\"type\":\"function\",\"function\":\
                   {\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"x\\\"}\"}}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "x");
        assert_eq!(text, "Writing that file now.");
    }

    /// Two calls in one message get distinct ids, or the second would answer
    /// the first's result and the cycle would be dropped as mismatched.
    #[test]
    fn two_recovered_calls_get_distinct_ids() {
        let raw = "{\"call\":\"list_desks\"}\nthen\n\
                   {\"call\":\"read_ledger\",\"arguments\":{\"ledger\":\"tasks\"}}";
        let (_, calls) = recover(raw);

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "salvaged_call_0");
        assert_eq!(calls[1].id, "salvaged_call_1");
    }

    /// A brace inside a JSON string must not end the span early — the case a
    /// naive depth counter gets wrong, and a file path is the likeliest way to
    /// hit it in this codebase.
    #[test]
    fn a_brace_inside_a_string_does_not_split_the_span() {
        let raw = "{\"call\":\"write_file\",\"arguments\":{\"path\":\"a{b\",\
                   \"content\":\"}\"}}";
        let (_, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["path"], "a{b");
        assert_eq!(calls[0].arguments["content"], "}");
    }

    /// A turn that offered no tools recovers nothing — which is what makes the
    /// tool-suppressed chat path inert without a second flag to keep in step.
    #[test]
    fn a_turn_that_offered_no_tools_never_recovers() {
        let raw = "{\"call\":\"read_ledger\",\"arguments\":{\"ledger\":\"tasks\"}}";
        assert!(recover_text_tool_calls(raw, &BTreeSet::new()).is_none());
    }

    /// A fenced code block the narrative actually needs is not collateral.
    #[test]
    fn a_fenced_block_with_content_survives() {
        let raw = "Here is the fix:\n\n```rust\nlet x = 1;\n```\n\n\
                   {\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert!(
            text.contains("let x = 1;"),
            "an unrelated code block must survive: {text:?}"
        );
    }

    /// Arguments that are not an object are not dispatchable, and guessing an
    /// empty object for them would run the tool with the wrong input.
    #[test]
    fn non_object_arguments_are_refused() {
        let raw = "{\"call\":\"read_ledger\",\"arguments\":\"the tasks one\"}";
        let (_, calls) = recover(raw);

        assert!(calls.is_empty());
    }

    /// Same rule inside the `function` wrapper. That branch skips the
    /// bare-object check, so without this it would silently substitute an
    /// empty object and run the tool on input the model never asked for.
    #[test]
    fn non_object_arguments_are_refused_inside_the_function_wrapper() {
        let raw = "Calling it: {\"function\":{\"name\":\"read_ledger\",\
                   \"arguments\":\"the tasks one\"}}";
        let (text, calls) = recover(raw);

        assert!(calls.is_empty());
        assert_eq!(
            text, raw,
            "and a refused call stays visible, not silently dropped"
        );
    }

    /// Tidying the hole a removed call leaves must not reflow the narrative
    /// around it. Indentation is content — a character scan that skips
    /// whitespace after a newline eats every indented list and code block.
    #[test]
    fn indentation_in_the_surviving_narrative_is_preserved() {
        let raw = "Steps:\n  1. Review releases\n      - check the ledger\n\n\n\
                   {\"call\":\"list_desks\"}\n\nDone.";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(
            text,
            "Steps:\n  1. Review releases\n      - check the ledger\n\nDone."
        );
    }

    /// A model that writes the marker on its own line above the object: the
    /// marker is part of the call's layout, so it must go with it rather than
    /// be left behind in the operator's reply.
    #[test]
    fn a_marker_on_its_own_line_is_removed_with_the_object() {
        let raw = "Let me look that up.\nfunction_call:\n{\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(
            text, "Let me look that up.",
            "the marker must not survive into chat"
        );
    }

    /// The same, with CRLF line endings.
    #[test]
    fn a_marker_on_its_own_crlf_line_is_removed_with_the_object() {
        let raw = "Let me look that up.\r\nfunction_call:\r\n{\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert!(
            !text.contains("function_call"),
            "the marker must not survive into chat: {text:?}"
        );
    }

    /// Where consuming the gap stops. A blank line between the two means the
    /// word belongs to the paragraph above, so it stays as narrative — the
    /// object is still recovered, but nothing is eaten out of the prose.
    #[test]
    fn a_blank_line_leaves_the_word_above_as_narrative() {
        let raw = "Here is what I found.\ncall:\n\n{\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(text, "Here is what I found.\ncall:");
    }

    /// The false positive that belt membership alone cannot stop: a model asked
    /// to *document* an offered tool writes the exact shape a call has.
    ///
    /// `write_file` is on the belt and the object carries real `arguments`, so
    /// every structural check passes. Only the absence of an intent signal — no
    /// call-family key, no marker — separates this from a request, and running
    /// it would write a file because a sentence said "example".
    #[test]
    fn a_documented_example_keyed_only_by_name_is_not_dispatched() {
        let raw = "Example: {\"name\":\"write_file\",\"arguments\":\
                   {\"path\":\"demo\",\"content\":\"hi\"}}";
        let (text, calls) = recover(raw);

        assert!(
            calls.is_empty(),
            "a bare `name` object with no marker must not run a tool"
        );
        assert_eq!(text, raw, "and the example must reach chat unchanged");
    }

    /// The same object, with the model saying it is a call. Either signal is
    /// enough — here it is the marker.
    #[test]
    fn a_name_keyed_object_behind_a_marker_is_dispatched() {
        let raw = "Writing it now. function_call:{\"name\":\"write_file\",\
                   \"arguments\":{\"path\":\"demo\"}}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(text, "Writing it now.");
    }

    /// And here it is the key: `call` is not a word ordinary JSON uses, so it
    /// carries the intent by itself. This is the 1/9 fenced shape, which had no
    /// marker at all.
    #[test]
    fn a_call_keyed_object_needs_no_marker() {
        let raw = "Here is the board.\n\n```json\n{\"call\":\"read_ledger\",\
                   \"arguments\":{\"ledger\":\"tasks\"}}\n```";
        let (_, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_ledger");
    }

    /// `Recall:` ends in the `call:` marker. Cutting on a bare suffix match
    /// takes two characters of an ordinary word with it and leaves the operator
    /// reading "Re".
    #[test]
    fn a_marker_matched_inside_a_word_is_not_stripped() {
        let raw = "Recall: {\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1, "the object is still a call");
        assert_eq!(text, "Recall:", "but the word must survive intact");
    }

    /// An inline triple-backtick span has no opener line and no body, so the
    /// empty-fence sweep must leave it alone rather than delete its contents.
    #[test]
    fn an_inline_backtick_span_is_not_swept_as_an_empty_fence() {
        let raw = "Important ```do not delete``` and now: {\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert!(
            text.contains("do not delete"),
            "an inline code span must survive: {text:?}"
        );
    }

    /// `tool_choice: "none"` authorizes nothing, whatever schemas rode along.
    #[test]
    fn tool_choice_none_authorizes_nothing() {
        let schemas = [ToolSchema::new("read_ledger", "d", serde_json::json!({}))];
        assert!(authorized_tool_names(&schemas, &ToolChoice::None).is_empty());
    }

    /// A pinned `tool_choice` authorizes that tool and no sibling on the wire.
    #[test]
    fn a_pinned_tool_choice_authorizes_only_that_tool() {
        let schemas = [
            ToolSchema::new("read_ledger", "d", serde_json::json!({})),
            ToolSchema::new("write_file", "d", serde_json::json!({})),
        ];
        let authorized =
            authorized_tool_names(&schemas, &ToolChoice::Tool("read_ledger".to_string()));

        assert_eq!(authorized.len(), 1);
        assert!(authorized.contains("read_ledger"));
        assert!(
            !authorized.contains("write_file"),
            "a sibling schema is not authorized by a pinned choice"
        );
    }

    /// A marker search that slices by byte offset must not panic when the
    /// narrative in front of the object ends in a multi-byte character.
    #[test]
    fn a_multi_byte_character_before_the_object_does_not_panic() {
        let raw = "Checking the ledger… {\"call\":\"list_desks\"}";
        let (text, calls) = recover(raw);

        assert_eq!(calls.len(), 1);
        assert_eq!(text, "Checking the ledger…");
    }
}
