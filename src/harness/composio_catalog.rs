//! Issue #410 — how a Composio action catalogue is *narrowed* and *rendered*
//! for an agent, and why every cut it makes describes itself.
//!
//! ## The failure this exists to stop
//!
//! `composio_list_tools` serialized the backend's whole `ComposioToolsResponse`
//! with `serde_json::to_string_pretty` and passed it to
//! [`scrub`](crate::harness::mcp_probe::scrub) on the way out. `scrub` is the
//! MCP *message* sanitiser: its third pass caps at
//! [`SCRUB_MAX_BYTES`](crate::harness::mcp_probe::SCRUB_MAX_BYTES) — **300
//! bytes**, the right size for a one-line failure sentence and three orders of
//! magnitude too small for a catalogue.
//!
//! So a 260-action catalogue reached the model as this, verbatim:
//!
//! ```text
//! {
//!   "tools": [
//!     {
//!       "type": "function",
//!       "function": {
//!         "name": "GITHUB_ACTION_000",
//!         "description": "Performs repository operation number 0. This action…
//! ```
//!
//! The first action, half of its schema, and a bare `…`. Every Composio tool was
//! affected, `composio_execute` included — a successful provider call returned
//! 300 bytes of whatever it actually said.
//!
//! That is worse than a small listing. The agent can see that actions exist but
//! cannot read the name or the parameters of the one it needs, and — because the
//! fragment does not say it is a fragment — it has no reason to ask differently.
//! It reissues the identical call, gets the identical fragment, and is halted by
//! the repetition guard. **A silent cut is what turns one failure into a retry
//! loop.**
//!
//! The fix is in two halves. The *security* half of `scrub` (credential
//! redaction, URL-query stripping) is unconditional and moves to
//! [`redact`](crate::harness::mcp_probe::redact), which never shortens. The
//! *length* half moves here, where a body can be sized as a body and a cut can
//! describe itself.
//!
//! ## The shape
//!
//! Two axes, both generic — nothing here knows what a "GitHub" is:
//!
//! * **Narrowing.** `search` matches whitespace-separated words,
//!   case-insensitively, against the action slug *and* its description; every
//!   word must match (AND), so `list issues` finds `GITHUB_LIST_ISSUES` without
//!   also dragging in every action whose description mentions "list".
//!   `toolkits` still narrows by toolkit, and `limit` bounds the count.
//! * **Progressive disclosure.** [`Detail::Names`] (the default) renders one
//!   line per action — slug plus a clipped description. It is cheap enough that
//!   a whole toolkit fits, which is what lets an agent *discover* a slug.
//!   [`Detail::Schemas`] renders the full JSON parameter schema for the few
//!   actions that matched, which is what lets it *call* one. That is the
//!   two-step an agent actually reasons in: find the name, then read the
//!   arguments.
//!
//! ## The invariant
//!
//! [`render`] bounds its own output — by count *and* by
//! [`MAX_RENDER_BYTES`] — and **never cuts an entry in half**. It stops at a
//! whole-entry boundary, so it always knows exactly how many entries it
//! dropped, and it always says so, in the result, naming the argument to pass
//! next. The budget also sits below the *next* cut downstream — the agent
//! harness's shared 16 KiB per-tool-result budget, which truncates on a byte
//! boundary — so the notice the model reads is the one that counted itself,
//! never an anonymous byte slice. The turn tests assert that directly, on the
//! wire.
//!
//! One deliberate exception: the **first** entry is always emitted whole, even
//! if it alone exceeds the budget. A schemas listing whose only match is a
//! single huge schema must still deliver that schema; returning "0 of 1 shown"
//! would be a correctly-described way of being useless.
//!
//! ## Why this module is not behind the `composio` feature
//!
//! Everything here is pure: no network, no credential, no `openhuman` Composio
//! type. The live tools in [`composio`](crate::harness::composio) are gated on
//! the opt-in `composio` feature, and **CI never builds that feature** (it runs
//! `--features openhuman,tinycortex`; `--all-features` is a `cargo check`, not a
//! `cargo test`). Keeping the narrowing and the truncation notice out here means
//! the behaviour this issue is about is exercised by the test lane that actually
//! runs, rather than by a lane that only type-checks.

use serde_json::{Value, json};

use openhuman_core::openhuman as oh;

/// Default number of actions a [`Detail::Names`] listing renders.
///
/// Sized to hold a whole large toolkit in one call — the point of the names
/// view is that discovery does not need a second round-trip.
pub const NAMES_DEFAULT_LIMIT: usize = 200;

/// Ceiling an explicit `limit` is clamped to in [`Detail::Names`] mode.
pub const NAMES_MAX_LIMIT: usize = 400;

/// Default number of actions a [`Detail::Schemas`] listing renders. Small on
/// purpose: a single Composio parameter schema routinely runs to a few
/// kilobytes, so "show me five" is already a large result.
pub const SCHEMAS_DEFAULT_LIMIT: usize = 5;

/// Ceiling an explicit `limit` is clamped to in [`Detail::Schemas`] mode.
pub const SCHEMAS_MAX_LIMIT: usize = 20;

/// Byte budget for the rendered body, before the header and the truncation
/// notice are added.
///
/// Chosen to stay comfortably under the agent harness's shared per-tool-result
/// budget (16 KiB at the time of writing) so that **this** module's cut — the
/// one that counts what it dropped and names the argument to narrow with — is
/// the cut the model sees. If the two ever swap places the notice is still
/// emitted; it just risks being clipped, which the header (rendered first)
/// guards against.
pub const MAX_RENDER_BYTES: usize = 11 * 1024;

/// Characters of an action's description kept on a [`Detail::Names`] line.
const DESCRIPTION_PREVIEW_CHARS: usize = 140;

/// Byte budget for a Composio tool body this module cannot structure — an
/// action's provider output, a connections list.
///
/// Same order as [`MAX_RENDER_BYTES`] and for the same reason: large enough
/// that a real answer survives, small enough that the harness's own anonymous
/// cut never fires first.
pub const MAX_BODY_BYTES: usize = 12 * 1024;

/// Bounds an unstructured tool body, appending a trailer that says it was cut,
/// by how much, and what to do about it.
///
/// The counterpart to [`render`] for payloads with no entries to count: a
/// provider's action output. There is no generic argument that makes *that*
/// smaller — the narrowing lives in the action's own parameters — so the
/// trailer says exactly that rather than inventing an argument that does not
/// exist. Naming the size and the cause is still the difference between an
/// agent that adjusts and an agent that repeats itself.
///
/// `what` names the payload for the trailer, e.g. `"GITHUB_LIST_ISSUES output"`.
pub fn bound_body(body: String, what: &str) -> String {
    if body.len() <= MAX_BODY_BYTES {
        return body;
    }
    let mut end = MAX_BODY_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = body.len() - end;
    let mut out = body[..end].to_string();
    out.push_str(&format!(
        "\n\n[TRUNCATED — the {what} was {dropped} bytes longer than this and the rest was NOT \
         shown. What you can see above is incomplete; do not treat it as the whole answer, and do \
         not repeat this call unchanged. Ask the action itself for less — a page size, a limit, a \
         date range or a filter argument — or request a specific record by id.]\n"
    ));
    out
}

/// How much detail one listing renders per action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Detail {
    /// `SLUG — one-line description`. Cheap; the discovery view.
    #[default]
    Names,
    /// The full JSON parameter schema. Expensive; the call-it view.
    Schemas,
}

impl Detail {
    /// Parses the `detail` argument. Anything unrecognised (including a missing
    /// value) is [`Detail::Names`] — the cheap, complete view is the safe
    /// default, and a typo must never silently produce a 200-schema answer.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "schemas" | "schema" | "full" => Self::Schemas,
            _ => Self::Names,
        }
    }

    /// The default entry count for this mode.
    fn default_limit(self) -> usize {
        match self {
            Self::Names => NAMES_DEFAULT_LIMIT,
            Self::Schemas => SCHEMAS_DEFAULT_LIMIT,
        }
    }

    /// The ceiling an explicit `limit` is clamped to in this mode.
    fn max_limit(self) -> usize {
        match self {
            Self::Names => NAMES_MAX_LIMIT,
            Self::Schemas => SCHEMAS_MAX_LIMIT,
        }
    }
}

/// One callable Composio action, flattened out of the backend's function-schema
/// envelope so this module owns no upstream type (and so it compiles without
/// the `composio` feature).
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogAction {
    /// The action slug, e.g. `GITHUB_LIST_ISSUES` — what `composio_execute`
    /// takes.
    pub slug: String,
    /// The toolkit the slug belongs to, lowercased (`github`).
    pub toolkit: String,
    /// The human-readable description, or empty when the backend published
    /// none.
    pub description: String,
    /// The JSON schema of the action's input parameters, when published.
    pub parameters: Option<Value>,
}

/// A parsed `composio_list_tools` request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListRequest {
    /// Toolkit slugs the caller asked for (already intersected with the tenant
    /// allowlist by the caller).
    pub toolkits: Vec<String>,
    /// Lowercased search words. Every word must match the slug or the
    /// description.
    pub search: Vec<String>,
    /// How much per action to render.
    pub detail: Detail,
    /// Entry cap, already clamped to the mode's ceiling.
    pub limit: usize,
}

impl ListRequest {
    /// Parses the tool arguments. `toolkits` is *not* read here — the live tool
    /// intersects it with the tenant allowlist before the request is built, and
    /// that resolution is a security decision this module must not duplicate.
    pub fn parse(args: &Value, toolkits: Vec<String>) -> Self {
        let detail = Detail::parse(args.get("detail").and_then(Value::as_str));
        let search = args
            .get("search")
            .and_then(Value::as_str)
            .map(search_terms)
            .unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, detail.max_limit()))
            .unwrap_or_else(|| detail.default_limit());
        Self {
            toolkits,
            search,
            detail,
            limit,
        }
    }

    /// Whether `action` survives this request's `search` words. Toolkit
    /// narrowing happens upstream (server-side, and again against the
    /// allowlist), so it is deliberately not re-applied here.
    fn matches(&self, action: &CatalogAction) -> bool {
        self.search.iter().all(|term| {
            action.slug.to_ascii_lowercase().contains(term)
                || action.description.to_ascii_lowercase().contains(term)
                || action.toolkit.contains(term)
        })
    }
}

/// Splits a raw `search` argument into lowercased words.
///
/// Underscores are separators too, so `GITHUB_LIST_ISSUES` pasted back in as a
/// search term still matches itself (the words `github`, `list` and `issues`
/// are all contained in the slug), which is how the agent asks for one specific
/// schema after reading the names view.
fn search_terms(raw: &str) -> Vec<String> {
    raw.split(|c: char| c.is_whitespace() || c == '_' || c == ',')
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| term.to_ascii_lowercase())
        .collect()
}

/// The name of the tool this module renders for. Kept here so the rendered
/// guidance and the tool registration cannot drift apart.
pub const LIST_TOOLS_TOOL: &str = "composio_list_tools";

/// Renders the agent-facing listing for `actions` under `request`.
///
/// `actions` is the complete set the backend returned for the requested
/// toolkits, already filtered to the tenant allowlist. The rendering is
/// self-bounding: see the module docs for the invariant.
pub fn render(actions: &[CatalogAction], request: &ListRequest) -> String {
    let available = actions.len();
    let matched: Vec<&CatalogAction> = actions.iter().filter(|a| request.matches(a)).collect();

    if matched.is_empty() {
        return render_empty(available, request);
    }

    let (body, shown, last_shown, terse) = match request.detail {
        Detail::Schemas => {
            let (body, shown, last) = fill(&matched, request.limit, render_schema_block);
            (body, shown, last, false)
        }
        // Names mode prefers *completeness* over prose. Try slug + description
        // first; if that would drop entries, re-render slug-only, which is
        // roughly six times denser and usually fits the whole catalogue. "List
        // action names (cheap, complete), then fetch one schema on demand" is
        // the shape the issue asks for, and a complete list of slugs is what
        // makes the second step possible.
        Detail::Names => {
            let (body, shown, last) = fill(&matched, request.limit, render_name_line);
            if shown == matched.len() {
                (body, shown, last, false)
            } else {
                let (terse_body, terse_shown, terse_last) =
                    fill(&matched, request.limit, render_slug_line);
                if terse_shown > shown {
                    (terse_body, terse_shown, terse_last, true)
                } else {
                    (body, shown, last, false)
                }
            }
        }
    };

    let dropped = matched.len() - shown;
    let mut out = render_header(available, matched.len(), shown, request);
    if terse {
        out.push_str(
            "(Descriptions omitted so more of the list fits — ask for one action's description \
             and parameters with `detail: \"schemas\"`.)\n\n",
        );
    }
    out.push_str(&body);
    if dropped > 0 {
        out.push_str(&render_cut_notice(dropped, &last_shown, request));
    }
    out
}

/// Renders entries with `entry_of` until `limit` or [`MAX_RENDER_BYTES`] is
/// reached, never splitting one. Returns the body, how many were rendered, and
/// the slug the list was cut after.
///
/// The **first** entry always goes in whole, even if it alone blows the budget:
/// a schemas listing that matched exactly one huge schema must still hand it
/// over, and "0 of 1 shown" is a correctly-described way of being useless.
fn fill(
    matched: &[&CatalogAction],
    limit: usize,
    entry_of: fn(&CatalogAction) -> String,
) -> (String, usize, String) {
    let mut body = String::new();
    let mut shown = 0usize;
    let mut last_shown = String::new();
    for action in matched.iter().take(limit) {
        let entry = entry_of(action);
        if shown > 0 && body.len() + entry.len() > MAX_RENDER_BYTES {
            break;
        }
        body.push_str(&entry);
        last_shown = action.slug.clone();
        shown += 1;
    }
    (body, shown, last_shown)
}

/// The header every listing opens with: what was available, what matched, what
/// is being shown, and — always — how to get from here to a callable slug.
fn render_header(available: usize, matched: usize, shown: usize, request: &ListRequest) -> String {
    let scope = match request.toolkits.as_slice() {
        [] => "every connected toolkit".to_string(),
        toolkits => toolkits.join(", "),
    };
    let filter = if request.search.is_empty() {
        String::new()
    } else {
        format!(" matching `{}`", request.search.join(" "))
    };
    let mut header = format!(
        "Composio actions in {scope} — {available} available, {matched}{filter}, showing {shown}.\n"
    );
    match request.detail {
        Detail::Names => header.push_str(&format!(
            "Each line is `SLUG — description`. Read one action's parameters before calling it:\n  \
             {LIST_TOOLS_TOOL}({{\"search\": \"<SLUG>\", \"detail\": \"schemas\"}})\n\n"
        )),
        Detail::Schemas => header.push_str(
            "Each block is one action's callable slug and its JSON input schema. Pass the slug \
             to `composio_execute` as `tool`.\n\n",
        ),
    }
    header
}

/// One `Detail::Names` line.
fn render_name_line(action: &CatalogAction) -> String {
    let description = action
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if description.is_empty() {
        format!("{}\n", action.slug)
    } else {
        format!(
            "{} — {}\n",
            action.slug,
            oh::util::truncate_with_suffix(&description, DESCRIPTION_PREVIEW_CHARS, "…")
        )
    }
}

/// One `Detail::Names` line with the description dropped — the dense fallback
/// that lets a whole large catalogue be listed completely.
fn render_slug_line(action: &CatalogAction) -> String {
    format!("{}\n", action.slug)
}

/// One `Detail::Schemas` block: the slug, the description, and the verbatim
/// input schema. Rendered as compact JSON — pretty-printing a Composio schema
/// triples its size for no gain to a model that reads it as text.
fn render_schema_block(action: &CatalogAction) -> String {
    let schema = action
        .parameters
        .clone()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    let mut block = format!("## {}\n", action.slug);
    let description = action
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !description.is_empty() {
        block.push_str(&format!("{description}\n"));
    }
    block.push_str(&format!(
        "parameters: {}\n\n",
        serde_json::to_string(&schema).unwrap_or_else(|_| "{}".to_string())
    ));
    block
}

/// The notice a cut listing carries. Says it was cut, how much was dropped,
/// where the cut fell, and the exact arguments that make the next call smaller.
///
/// This is the whole point of the module: an agent that can read this has a
/// reason to change its request, so it never re-issues the identical call into
/// the repetition guard.
fn render_cut_notice(dropped: usize, last_shown: &str, request: &ListRequest) -> String {
    let max = request.detail.max_limit();
    let plural = if dropped == 1 { "" } else { "s" };
    format!(
        "\n[TRUNCATED — this listing is NOT complete. {dropped} more matching action{plural} were \
         not shown (the list was cut after `{last_shown}`). Do NOT repeat this call unchanged; it \
         will return the same partial list. Narrow it instead:\n  \
         • `search`: words matched against the action slug and description, e.g. \
         {LIST_TOOLS_TOOL}({{\"search\": \"list issues\"}})\n  \
         • `toolkits`: restrict to one toolkit, e.g. \
         {LIST_TOOLS_TOOL}({{\"toolkits\": [\"github\"]}})\n  \
         • `limit`: raise the cap (max {max}) if you truly need more at once.]\n"
    )
}

/// The answer when nothing matched. A completed listing that found nothing is a
/// fact, and saying so plainly — with the search that produced it — is what
/// stops the agent guessing a slug or retrying the same words.
fn render_empty(available: usize, request: &ListRequest) -> String {
    let scope = match request.toolkits.as_slice() {
        [] => "every connected toolkit".to_string(),
        toolkits => toolkits.join(", "),
    };
    if available == 0 {
        return format!(
            "Composio actions in {scope} — none. This toolkit has no callable actions available to \
             this company (it may not be connected). Check `composio_list_connections`, and do not \
             guess an action slug.\n"
        );
    }
    format!(
        "Composio actions in {scope} — {available} available, 0 matching `{search}`.\n\
         Nothing matched those words. Try fewer or different words (the search matches the action \
         slug and its description), or list everything with \
         {LIST_TOOLS_TOOL}({{\"toolkits\": [\"<slug>\"]}}). Do NOT guess a slug that was not \
         listed.\n",
        search = request.search.join(" ")
    )
}

/// The `parameters_schema` JSON for `composio_list_tools`.
///
/// Lives here so the argument names the model is told about are the same
/// literals [`ListRequest::parse`] reads and [`render_cut_notice`] tells it to
/// use. A filter the model never discovers is the same bug as no filter at all.
pub fn list_tools_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "toolkits": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Toolkit slugs to list actions for (e.g. `gmail`, `slack`, `github`). Omit to search every connected toolkit."
            },
            "search": {
                "type": "string",
                "description": "Narrow the listing to actions whose slug or description contains ALL of these words (case-insensitive), e.g. `list issues` or `send email`. Pass an exact slug here with `detail: \"schemas\"` to read just that action's parameters."
            },
            "detail": {
                "type": "string",
                "enum": ["names", "schemas"],
                "description": "`names` (default) lists `SLUG — description` lines: cheap, use it to discover the action you need. `schemas` returns the full JSON input schema for the matching actions: use it once you know the slug, before calling `composio_execute`."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum actions to return (default 200 for `names`, 5 for `schemas`)."
            }
        },
        "additionalProperties": false
    })
}

// ---------------------------------------------------------------------------
// The toolkit listing
// ---------------------------------------------------------------------------
//
// Same class of bug, one level up (issue #410 point 4): `composio_list_toolkits`
// serialized the backend's whole catalogue — Composio publishes several hundred
// toolkits, each with a name, a description, a categories array and a logo URL —
// as pretty JSON with no filter and no bound. It is rarer to blow the budget
// than the action listing is, but it is the *same* silent cut, so it gets the
// same treatment rather than a note in a follow-up issue.

/// Tool name for the toolkit listing, for the same reason [`LIST_TOOLS_TOOL`]
/// is a constant.
pub const LIST_TOOLKITS_TOOL: &str = "composio_list_toolkits";

/// Default number of toolkits one listing renders.
pub const TOOLKITS_DEFAULT_LIMIT: usize = 200;

/// Ceiling an explicit `limit` is clamped to when listing toolkits.
pub const TOOLKITS_MAX_LIMIT: usize = 500;

/// One integration this company could use, flattened out of the backend's
/// catalogue entry. The logo URL is deliberately dropped: it is display
/// metadata for the console, and it costs an agent tokens to read a URL it can
/// never act on.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogToolkit {
    /// Toolkit slug, e.g. `googlecalendar` — what `composio_authorize` and the
    /// `toolkits` argument take.
    pub slug: String,
    /// Human-readable name, or empty when the backend published none.
    pub name: String,
    /// Short description, or empty.
    pub description: String,
    /// Whether this company is connected to it, when known.
    pub connected: Option<bool>,
}

/// A parsed `composio_list_toolkits` request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolkitListRequest {
    /// Lowercased search words matched against slug, name and description.
    pub search: Vec<String>,
    /// Entry cap, already clamped.
    pub limit: usize,
}

impl ToolkitListRequest {
    /// Parses the tool arguments, defaulting and clamping rather than failing.
    pub fn parse(args: &Value) -> Self {
        let search = args
            .get("search")
            .and_then(Value::as_str)
            .map(search_terms)
            .unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, TOOLKITS_MAX_LIMIT))
            .unwrap_or(TOOLKITS_DEFAULT_LIMIT);
        Self { search, limit }
    }

    fn matches(&self, toolkit: &CatalogToolkit) -> bool {
        self.search.iter().all(|term| {
            toolkit.slug.to_ascii_lowercase().contains(term)
                || toolkit.name.to_ascii_lowercase().contains(term)
                || toolkit.description.to_ascii_lowercase().contains(term)
        })
    }
}

/// Renders the agent-facing toolkit listing, bounded and self-describing on the
/// same terms as [`render`].
pub fn render_toolkits(toolkits: &[CatalogToolkit], request: &ToolkitListRequest) -> String {
    let available = toolkits.len();
    let matched: Vec<&CatalogToolkit> = toolkits.iter().filter(|t| request.matches(t)).collect();

    if available == 0 {
        return "Composio toolkits — none available to this company. No integration can be used \
                or connected until the operator configures one.\n"
            .to_string();
    }
    if matched.is_empty() {
        return format!(
            "Composio toolkits — {available} available, 0 matching `{search}`.\n\
             Nothing matched those words. Try fewer or different words, or call \
             {LIST_TOOLKITS_TOOL}({{}}) to see everything. Do NOT guess a toolkit slug.\n",
            search = request.search.join(" ")
        );
    }

    let mut body = String::new();
    let mut shown = 0usize;
    let mut last_shown = "";
    for toolkit in matched.iter().take(request.limit) {
        let mut entry = toolkit.slug.clone();
        if !toolkit.name.is_empty() && !toolkit.name.eq_ignore_ascii_case(&toolkit.slug) {
            entry.push_str(&format!(" ({})", toolkit.name));
        }
        match toolkit.connected {
            Some(true) => entry.push_str(" [connected]"),
            Some(false) => entry.push_str(" [not connected]"),
            None => {}
        }
        let description = toolkit
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !description.is_empty() {
            entry.push_str(&format!(
                " — {}",
                oh::util::truncate_with_suffix(&description, DESCRIPTION_PREVIEW_CHARS, "…")
            ));
        }
        entry.push('\n');
        if shown > 0 && body.len() + entry.len() > MAX_RENDER_BYTES {
            break;
        }
        body.push_str(&entry);
        last_shown = &toolkit.slug;
        shown += 1;
    }

    let filter = if request.search.is_empty() {
        String::new()
    } else {
        format!(" matching `{}`", request.search.join(" "))
    };
    let mut out = format!(
        "Composio toolkits — {available} available, {matched}{filter}, showing {shown}.\n\
         Each line is `slug (name) — description`. List a toolkit's callable actions with \
         {LIST_TOOLS_TOOL}({{\"toolkits\": [\"<slug>\"]}}).\n\n",
        matched = matched.len()
    );
    out.push_str(&body);
    let dropped = matched.len() - shown;
    if dropped > 0 {
        let plural = if dropped == 1 { "" } else { "s" };
        out.push_str(&format!(
            "\n[TRUNCATED — this listing is NOT complete. {dropped} more matching toolkit{plural} \
             were not shown (the list was cut after `{last_shown}`). Do NOT repeat this call \
             unchanged; it will return the same partial list. Narrow it with \
             {LIST_TOOLKITS_TOOL}({{\"search\": \"<words>\"}}), or raise `limit` (max \
             {TOOLKITS_MAX_LIMIT}).]\n"
        ));
    }
    out
}

/// The `parameters_schema` JSON for `composio_list_toolkits`.
pub fn list_toolkits_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "search": {
                "type": "string",
                "description": "Narrow the listing to toolkits whose slug, name or description contains ALL of these words (case-insensitive), e.g. `calendar` or `issue tracker`."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum toolkits to return (default 200)."
            }
        },
        "additionalProperties": false
    })
}

/// The `composio_list_toolkits` description shown to the model.
pub fn list_toolkits_description() -> &'static str {
    "List the Composio toolkits (integrations such as Gmail, Slack, GitHub, Notion) available to \
     this company, with whether each is connected. Pass `search` words to narrow a large \
     catalogue; a truncated result always says so and how to narrow it. Use \
     `composio_list_tools` next to see one toolkit's callable actions. Read-only."
}

/// The `composio_list_tools` description shown to the model.
pub fn list_tools_description() -> &'static str {
    "Discover the callable Composio actions (Gmail, Slack, GitHub, Notion, …) this company can \
     use. Two steps: call it with `search` words (and optionally `toolkits`) to list matching \
     `SLUG — description` lines, then call it again with `detail: \"schemas\"` for the one slug \
     you need to read its parameters before `composio_execute`. Large catalogues are truncated; \
     the result always says so and how to narrow it. Read-only."
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalogue of `count` synthetic actions for `toolkit`, each with a
    /// realistically chunky description and parameter schema.
    fn catalogue(toolkit: &str, count: usize) -> Vec<CatalogAction> {
        (0..count)
            .map(|i| CatalogAction {
                slug: format!("{}_ACTION_{i:03}", toolkit.to_ascii_uppercase()),
                toolkit: toolkit.to_string(),
                description: format!(
                    "Performs operation {i} on the {toolkit} account. {}",
                    "Long upstream prose that Composio publishes for every action. ".repeat(4)
                ),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "owner": {"type": "string", "description": "x".repeat(200)},
                        "repo": {"type": "string", "description": "y".repeat(200)},
                    },
                    "required": ["owner"]
                })),
            })
            .collect()
    }

    fn request(search: &str, detail: Detail, toolkits: &[&str]) -> ListRequest {
        ListRequest {
            toolkits: toolkits.iter().map(|t| t.to_string()).collect(),
            search: search_terms(search),
            detail,
            limit: detail.default_limit(),
        }
    }

    /// The headline invariant: a listing too big to render says so, says how
    /// much it dropped, and names the argument that makes the next call
    /// smaller. Without this the agent has no reason to change its request.
    #[test]
    fn an_oversized_listing_says_it_was_cut_and_how_to_narrow_it() {
        let actions = catalogue("github", 300);
        let out = render(&actions, &request("", Detail::Names, &["github"]));

        assert!(out.contains("300 available"), "{out}");
        assert!(
            out.contains("TRUNCATED"),
            "the cut must be announced: {out}"
        );
        assert!(
            out.contains("100 more matching actions were not shown"),
            "the notice must count what it dropped: {out}"
        );
        assert!(
            out.contains("`search`") && out.contains("`toolkits`") && out.contains("`limit`"),
            "the notice must name the arguments that narrow it: {out}"
        );
        assert!(
            out.contains("Do NOT repeat this call unchanged"),
            "the notice must break the retry loop explicitly: {out}"
        );
    }

    /// Names mode prefers a *complete* list of slugs over a partial list with
    /// prose. A 150-action toolkit does not fit with descriptions (they alone
    /// run past the byte budget) but fits comfortably without them — so the
    /// agent gets every slug it might need, plus the pointer to `detail:
    /// "schemas"` for the one it picks.
    #[test]
    fn names_mode_drops_descriptions_rather_than_actions() {
        let actions = catalogue("github", 150);
        let out = render(&actions, &request("", Detail::Names, &["github"]));

        assert!(out.contains("showing 150"), "{out}");
        assert!(
            !out.contains("TRUNCATED"),
            "nothing had to be dropped: {out}"
        );
        assert!(out.contains("Descriptions omitted"), "{out}");
        assert!(
            out.contains("GITHUB_ACTION_149"),
            "the last slug must be present: {out}"
        );
        assert!(
            !out.contains("Long upstream prose"),
            "the dense fallback must not carry descriptions: {out}"
        );
        assert!(out.len() < 16 * 1024, "{} bytes", out.len());
    }

    /// The bound is real, not advisory — and it is a whole-entry bound, so the
    /// dropped count in the notice is exact rather than approximate.
    #[test]
    fn rendering_stays_inside_its_byte_budget_and_never_splits_an_entry() {
        // 300 schema blocks at ~500 bytes each is far past the budget.
        let actions = catalogue("gmail", 300);
        let mut request = request("", Detail::Schemas, &["gmail"]);
        request.limit = SCHEMAS_MAX_LIMIT;
        let out = render(&actions, &request);

        assert!(
            out.len() < 16 * 1024,
            "the rendered listing must stay under the harness tool-result cap: {} bytes",
            out.len()
        );
        // Every block that IS present is complete: its trailing newline pair and
        // its `parameters:` line both survived.
        let blocks = out.matches("## GMAIL_ACTION_").count();
        assert_eq!(
            out.matches("parameters: {").count(),
            blocks,
            "an entry was cut in half: {out}"
        );
        assert!(blocks >= 1, "at least one schema must be delivered: {out}");
    }

    /// A listing that fits is not decorated with a truncation notice — the
    /// notice has to mean something.
    #[test]
    fn a_complete_listing_carries_no_truncation_notice() {
        let actions = catalogue("linear", 12);
        let out = render(&actions, &request("", Detail::Names, &["linear"]));
        assert!(out.contains("showing 12"), "{out}");
        assert!(!out.contains("TRUNCATED"), "{out}");
    }

    /// Narrowing is generic: the same words find the one action in a
    /// hundred-slug catalogue for any toolkit, with no per-provider table.
    #[test]
    fn search_narrows_to_one_action_on_any_toolkit() {
        let mut actions = catalogue("github", 120);
        actions.push(CatalogAction {
            slug: "GITHUB_LIST_ISSUES".to_string(),
            toolkit: "github".to_string(),
            description: "List issues in a repository.".to_string(),
            parameters: Some(json!({"type": "object", "properties": {"repo": {"type": "string"}}})),
        });
        let mut other = catalogue("notion", 130);
        other.push(CatalogAction {
            slug: "NOTION_SEARCH_PAGES".to_string(),
            toolkit: "notion".to_string(),
            description: "Search pages in the workspace.".to_string(),
            parameters: Some(
                json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            ),
        });

        let github = render(
            &actions,
            &request("list issues", Detail::Schemas, &["github"]),
        );
        assert!(github.contains("GITHUB_LIST_ISSUES"), "{github}");
        assert!(
            github.contains("\"repo\""),
            "the schema must be present: {github}"
        );
        assert!(
            !github.contains("TRUNCATED"),
            "one match is not a cut: {github}"
        );

        let notion = render(
            &other,
            &request("search pages", Detail::Schemas, &["notion"]),
        );
        assert!(notion.contains("NOTION_SEARCH_PAGES"), "{notion}");
        assert!(notion.contains("\"query\""), "{notion}");
    }

    /// An exact slug read back from the names view resolves to that one schema
    /// — the second half of the two-step, and the reason `_` is a search
    /// separator.
    #[test]
    fn an_exact_slug_pasted_into_search_returns_that_schema() {
        let mut actions = catalogue("slack", 90);
        actions.push(CatalogAction {
            slug: "SLACK_POST_MESSAGE".to_string(),
            toolkit: "slack".to_string(),
            description: "Post a message to a channel.".to_string(),
            parameters: Some(
                json!({"type": "object", "properties": {"channel": {"type": "string"}}}),
            ),
        });
        let out = render(
            &actions,
            &request("SLACK_POST_MESSAGE", Detail::Schemas, &["slack"]),
        );
        assert!(out.contains("SLACK_POST_MESSAGE"), "{out}");
        assert!(out.contains("\"channel\""), "{out}");
        assert!(out.contains("showing 1"), "{out}");
    }

    /// No match is reported as a fact, with the words that produced it, and
    /// with an explicit instruction not to invent a slug.
    #[test]
    fn no_match_is_stated_plainly_rather_than_returned_empty() {
        let actions = catalogue("gmail", 40);
        let out = render(
            &actions,
            &request("quantum teleport", Detail::Names, &["gmail"]),
        );
        assert!(out.contains("0 matching `quantum teleport`"), "{out}");
        assert!(out.contains("Do NOT guess a slug"), "{out}");
        assert!(!out.contains("TRUNCATED"), "nothing was cut: {out}");
    }

    /// An empty catalogue is a different fact from an empty search, and points
    /// at the connection rather than at the search words.
    #[test]
    fn an_empty_catalogue_points_at_the_connection() {
        let out = render(&[], &request("", Detail::Names, &["github"]));
        assert!(out.contains("none"), "{out}");
        assert!(out.contains("composio_list_connections"), "{out}");
    }

    /// A single schema larger than the whole budget is still delivered — a
    /// correctly-described way of being useless is still useless.
    #[test]
    fn a_single_oversized_schema_is_still_delivered_whole() {
        let actions = vec![CatalogAction {
            slug: "GIANT_ACTION".to_string(),
            toolkit: "giant".to_string(),
            description: "One enormous schema.".to_string(),
            parameters: Some(json!({"type": "object", "blob": "z".repeat(MAX_RENDER_BYTES * 2)})),
        }];
        let out = render(&actions, &request("", Detail::Schemas, &["giant"]));
        assert!(out.contains("GIANT_ACTION"), "{out}");
        assert!(
            out.contains(&"z".repeat(1000)),
            "the only matching schema must survive whole"
        );
        assert!(!out.contains("TRUNCATED"), "nothing was dropped: {out}");
    }

    /// Argument parsing: the defaults are the cheap ones, an over-large `limit`
    /// is clamped rather than rejected, and an unknown `detail` degrades to the
    /// names view instead of dumping 200 schemas.
    #[test]
    fn arguments_default_and_clamp_rather_than_failing() {
        let names = ListRequest::parse(&json!({}), vec!["github".into()]);
        assert_eq!(names.detail, Detail::Names);
        assert_eq!(names.limit, NAMES_DEFAULT_LIMIT);
        assert!(names.search.is_empty());

        let schemas = ListRequest::parse(&json!({"detail": "schemas"}), Vec::new());
        assert_eq!(schemas.limit, SCHEMAS_DEFAULT_LIMIT);

        let clamped = ListRequest::parse(&json!({"detail": "schemas", "limit": 9999}), Vec::new());
        assert_eq!(clamped.limit, SCHEMAS_MAX_LIMIT);

        let typo = ListRequest::parse(&json!({"detail": "everything"}), Vec::new());
        assert_eq!(
            typo.detail,
            Detail::Names,
            "an unknown detail must not dump schemas"
        );

        let terms = ListRequest::parse(&json!({"search": "  List   Issues "}), Vec::new());
        assert_eq!(terms.search, vec!["list".to_string(), "issues".to_string()]);
    }

    /// The advertised argument names and the ones the parser reads are the same
    /// literals — a filter the model is told about but the tool ignores would
    /// be the same bug wearing a different hat.
    #[test]
    fn the_advertised_schema_matches_the_arguments_the_parser_reads() {
        let schema = list_tools_parameters_schema();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties object");
        for key in ["toolkits", "search", "detail", "limit"] {
            assert!(properties.contains_key(key), "`{key}` is not advertised");
        }
        let description = list_tools_description();
        assert!(description.contains("search"), "{description}");
        assert!(description.contains("schemas"), "{description}");
        assert!(
            description.contains("truncated"),
            "the model must be told results can be cut: {description}"
        );

        let toolkits = list_toolkits_parameters_schema();
        let properties = toolkits
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties object");
        for key in ["search", "limit"] {
            assert!(properties.contains_key(key), "`{key}` is not advertised");
        }
        assert!(list_toolkits_description().contains("search"));
    }

    // ── the root cause: a message cap applied to a body ────────────────

    /// The regression guard for #410's actual root cause.
    ///
    /// A tool body must never be sized by
    /// [`SCRUB_MAX_BYTES`](crate::harness::mcp_probe::SCRUB_MAX_BYTES). That
    /// constant is a 300-byte cap on a one-line operator message, and routing a
    /// successful Composio result through it is what turned a 260-action
    /// catalogue into "the first action and half of its schema, ending in `…`".
    /// This pins the two halves apart: redaction never shortens, the body bound
    /// is orders of magnitude larger than the message bound, and a bounded body
    /// says so.
    #[test]
    fn a_tool_body_is_bounded_as_a_body_not_as_a_message() {
        use crate::harness::mcp_probe::{SCRUB_MAX_BYTES, redact, scrub};

        let body = format!("secret-token {}", "payload ".repeat(4_000));
        let secrets = vec!["secret-token".to_string()];

        // Redaction still redacts — that half was never the bug.
        let redacted = redact(&body, &secrets);
        assert!(
            !redacted.contains("secret-token"),
            "the token survived redaction"
        );
        assert!(redacted.contains("•••"));
        // …and it does NOT shorten. The old path lost 99% of the payload here.
        assert!(
            redacted.len() > SCRUB_MAX_BYTES * 10,
            "redact must not apply the message cap: {} bytes",
            redacted.len()
        );
        assert!(
            scrub(&body, &secrets).len() <= SCRUB_MAX_BYTES + 3,
            "scrub keeps the message cap for the messages it was built for"
        );

        // The body bound is a body bound, and it announces itself.
        const {
            assert!(
                MAX_BODY_BYTES > SCRUB_MAX_BYTES * 20,
                "a body budget sized like a message budget is the bug"
            )
        };
        let bounded = bound_body(redacted.clone(), "`GITHUB_LIST_ISSUES` output");
        assert!(
            bounded.contains("TRUNCATED"),
            "an oversized body must say so"
        );
        assert!(
            bounded.contains("bytes longer than this"),
            "the notice must quantify what was lost: {bounded}"
        );
        assert!(bounded.contains("`GITHUB_LIST_ISSUES` output"), "{bounded}");

        // A body that fits is returned untouched — no decoration, no marker.
        let small = "a short provider response".to_string();
        assert_eq!(bound_body(small.clone(), "output"), small);
    }

    // ── the toolkit listing ────────────────────────────────────────────

    fn toolkit_catalogue(count: usize) -> Vec<CatalogToolkit> {
        (0..count)
            .map(|i| CatalogToolkit {
                slug: format!("toolkit{i:03}"),
                name: format!("Toolkit {i}"),
                description: "An integration with a long upstream description. ".repeat(4),
                connected: Some(i % 3 == 0),
            })
            .collect()
    }

    /// The toolkit catalogue is the same bug one level up, so it gets the same
    /// self-describing cut.
    #[test]
    fn an_oversized_toolkit_listing_says_it_was_cut_and_how_to_narrow_it() {
        let toolkits = toolkit_catalogue(400);
        let out = render_toolkits(&toolkits, &ToolkitListRequest::parse(&json!({})));
        assert!(out.contains("400 available"), "{out}");
        assert!(out.contains("TRUNCATED"), "{out}");
        assert!(
            out.contains("more matching toolkits were not shown"),
            "{out}"
        );
        assert!(
            out.contains("`search`") || out.contains("\"search\""),
            "{out}"
        );
        assert!(
            out.len() < 16 * 1024,
            "the listing must stay under the harness cap: {} bytes",
            out.len()
        );
    }

    /// Narrowing and the connected marker, which is what an agent actually
    /// needs before it reaches for `composio_authorize`.
    #[test]
    fn toolkit_search_narrows_and_marks_connection_state() {
        let mut toolkits = toolkit_catalogue(50);
        toolkits.push(CatalogToolkit {
            slug: "googlecalendar".to_string(),
            name: "Google Calendar".to_string(),
            description: "Read and write calendar events.".to_string(),
            connected: Some(true),
        });
        let out = render_toolkits(
            &toolkits,
            &ToolkitListRequest::parse(&json!({"search": "calendar"})),
        );
        assert!(
            out.contains("googlecalendar (Google Calendar) [connected]"),
            "{out}"
        );
        assert!(out.contains("showing 1"), "{out}");
        assert!(!out.contains("TRUNCATED"), "{out}");
    }

    /// A company with no integrations at all is a different fact from a search
    /// that matched nothing, and both are stated rather than returned empty.
    #[test]
    fn an_empty_toolkit_catalogue_and_an_empty_search_read_differently() {
        let none = render_toolkits(&[], &ToolkitListRequest::parse(&json!({})));
        assert!(none.contains("none available"), "{none}");

        let no_match = render_toolkits(
            &toolkit_catalogue(5),
            &ToolkitListRequest::parse(&json!({"search": "zzz"})),
        );
        assert!(no_match.contains("0 matching `zzz`"), "{no_match}");
        assert!(
            no_match.contains("Do NOT guess a toolkit slug"),
            "{no_match}"
        );
    }
}
