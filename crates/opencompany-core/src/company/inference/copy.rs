//! Shared turn-failure and save-refusal sentences (decision D-copy / X9,
//! 2026-09-15, `docs/key-reworks/README.md`).
//!
//! Before this module, near-identical fail-closed sentences were written out
//! by hand at each call site (`resolve_choice` in `inference.rs`, the agent
//! pin check in `harness/built_in/provider.rs`, the provider save routes in
//! `server/ops/inference/providers.rs` and `server/ops/team_agent.rs`), and
//! nothing kept them in agreement — two sites saying the same thing slightly
//! differently reads, to an operator hopping between an agent's error and the
//! company default's error, as two different products. One function per
//! sentence, called from every site that needs it, is the fix; the tests
//! below are what keep it a fix rather than a fourth copy.
//!
//! D-names-in-errors (X7) is why every function here takes a **display
//! name** — an agent's `role`/label, a provider's `label` — never a raw slug
//! or agent id. The id still rides in whatever structured data the caller
//! attaches (an `AgentPin` carries both); only the sentence a person reads is
//! restricted to names.

/// Why a provider a pin or the default named cannot serve a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderGone {
    /// No row with that slug exists any more (deleted, or never added).
    Removed,
    /// The row exists but its `enabled` flag is off.
    TurnedOff,
}

impl ProviderGone {
    fn word(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::TurnedOff => "turned off",
        }
    }
}

/// A provider save (add, edit, or set-default) sent no model.
///
/// "Choose a model for {Provider} before saving."
pub fn no_model_for_provider(provider_label: &str) -> String {
    format!("Choose a model for {provider_label} before saving.")
}

/// The console path every sentence below points to: the LLM page, under the
/// "API Keys" group of Connections (`frontend/src/views/connection-pages.ts`:
/// group `keys` is labelled "API Keys"; the `inference` page in it is
/// labelled "LLM" — verified against that file, not guessed).
const SETTINGS_PATH: &str = "Connections → API Keys → LLM";

/// Nothing resolves for this agent's turn: no pin, no full company default,
/// and the legacy chain gave nothing either.
///
/// "No model is chosen. Choose a provider and model for {Agent}, or set the
/// company default in Connections → API Keys → LLM."
pub fn nothing_resolved(agent_name: &str) -> String {
    format!(
        "No model is chosen. Choose a provider and model for {agent_name}, \
         or set the company default in {SETTINGS_PATH}."
    )
}

/// No agent is in view (a company-wide boot/status read, or an internal pass
/// with no single agent to name) and nothing resolves.
///
/// "No model is chosen for this company. Choose a default provider and model
/// in Connections → API Keys → LLM."
///
/// A `const` because `inference::NO_MODEL_CHOSEN` (2b's originally-named
/// symbol, kept for callers outside this module that still match error text
/// against it) needs a `&'static str` it can re-export, not a function call.
pub const COMPANY_NO_MODEL_CHOSEN: &str = "No model is chosen for this company. Choose a default \
     provider and model in Connections → API Keys → LLM.";

/// Same text as [`COMPANY_NO_MODEL_CHOSEN`], as an owned `String` for callers
/// building an [`crate::error::OpenCompanyError`] (which takes `String`).
pub fn nothing_resolved_for_company() -> String {
    COMPANY_NO_MODEL_CHOSEN.to_string()
}

/// A provider that would otherwise resolve has no credential.
///
/// "{Agent} uses {Provider}, which has no key. Add one in Connections → API
/// Keys → LLM, or choose another provider and model for {Agent}."
pub fn provider_has_no_key(agent_name: &str, provider_label: &str) -> String {
    format!(
        "{agent_name} uses {provider_label}, which has no key. Add one in \
         {SETTINGS_PATH}, or choose another provider and model for {agent_name}."
    )
}

/// An agent's own pair names a provider that is gone or switched off (F6:
/// fails closed, never falls back to the company default on its own).
///
/// "{Agent} uses {Provider}, which is removed. Choose another provider and
/// model for {Agent}, or clear its model to use the company default."
pub fn pair_broken(agent_name: &str, provider_label: &str, why: ProviderGone) -> String {
    let word = why.word();
    format!(
        "{agent_name} uses {provider_label}, which is {word}. Choose another \
         provider and model for {agent_name}, or clear its model to use the \
         company default."
    )
}

/// The company default names a provider that is gone or switched off.
///
/// "The company default uses {Provider}, which is removed. Choose a new
/// default in Connections → API Keys → LLM."
pub fn default_broken(provider_label: &str, why: ProviderGone) -> String {
    let word = why.word();
    format!(
        "The company default uses {provider_label}, which is {word}. Choose \
         a new default in {SETTINGS_PATH}."
    )
}

/// The wire codes `docs/key-reworks/in-use-guards.md` §5 names, the
/// reverse of [`classify`] (keys rework #2306, round-2 review KR-L2-03).
pub const NO_MODEL_CHOSEN_CODE: &str = "no_model_chosen";
pub const PAIR_PROVIDER_REMOVED_CODE: &str = "pair_provider_removed";
pub const PAIR_PROVIDER_OFF_CODE: &str = "pair_provider_off";
pub const DEFAULT_PROVIDER_REMOVED_CODE: &str = "default_provider_removed";
pub const DEFAULT_PROVIDER_OFF_CODE: &str = "default_provider_off";
pub const PROVIDER_NO_KEY_CODE: &str = "provider_no_key";
/// Reserved: no resolver path produces this sentence yet (a chosen model is
/// never re-validated against a live catalogue at turn time, by design —
/// see phase-2b/2d), so [`classify`] never returns it. Kept so the console's
/// `TURN_FAILURE_CODES` list and the backend's own set of codes agree on
/// what the wire vocabulary is, even though only one side can produce every
/// value in it today.
#[allow(dead_code)]
pub const MODEL_NOT_LISTED_CODE: &str = "model_not_listed";
/// The agent's turn never reached a model at all: it is bound to a harness
/// this host has no engine for, or to one whose last warm-up failed. Not a
/// resolution failure in the provider/model sense, but the same class of
/// thing to the person reading the thread — a named setting is wrong, and no
/// amount of resending clears it — so it travels the same wire fields.
pub const HARNESS_UNAVAILABLE_CODE: &str = "harness_unavailable";

/// The substring both of [`HarnessRouter::engine_for`]'s sentences carry, and
/// the only signal [`classify`] keys [`HARNESS_UNAVAILABLE_CODE`] on.
///
/// `harness/router_tests.rs` drives the real router and asserts the real
/// error still classifies, so a reworded sentence there fails CI instead of
/// quietly dropping this class back into the generic notice.
///
/// [`HarnessRouter::engine_for`]: crate::harness::router::HarnessRouter
const HARNESS_BOUND_MARKER: &str = "` is bound to harness `";

/// How the same sentences open. [`classify`] cuts from here so that whatever
/// `Display` prefix the error type put in front of them (`configuration
/// error: `) never reaches a person.
const HARNESS_SENTENCE_OPENER: &str = "agent `";

/// The two ways [`HarnessRouter::engine_for`]'s sentence continues right after
/// the closing backtick on the harness name — the only two shapes a real
/// router sentence takes. [`harness_binding`] requires the text to continue
/// with one of these, not just carry [`HARNESS_BOUND_MARKER`] and
/// [`HARNESS_SENTENCE_OPENER`] somewhere: those two substrings alone are
/// loose enough that an unrelated diagnostic which happens to quote something
/// shaped like `agent \`x\` is bound to harness \`y\`` — echoed tool output, a
/// provider error, adversarial message content — would otherwise misclassify
/// as a harness failure the turn never actually had (tinysweeper, PR #2401).
///
/// [`HARNESS_WARMUP_TAIL`] stops before its `: `, so it also matches the cut
/// sentence [`harness_binding`] itself emits.
///
/// [`HarnessRouter::engine_for`]: crate::harness::router::HarnessRouter
const HARNESS_WARMUP_TAIL: &str = "`, whose last warm-up failed";
const HARNESS_NO_ENGINE_TAIL: &str = "`, but ";

/// Appended to the harness sentence, which names the gap but not whether
/// waiting helps.
const HARNESS_RETRY_NOTE: &str = "Retrying will not help until that harness can run.";

/// One classified turn-time resolution failure — the exact sentence one of
/// this module's functions produced, plus the wire code and (when
/// recoverable) the agent id and provider slug the sentence names (keys
/// rework #2306, round-2 review KR-L2-03).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionFailure {
    pub code: &'static str,
    pub message: String,
    pub pair_agent_id: Option<String>,
    pub provider_slug: Option<String>,
}

/// A hidden trailer [`with_agent_marker`] appends and [`classify`] strips —
/// never `format!`-interpolated into a sentence a person reads. `\u{0}` is
/// not typable and cannot appear in a display name or a slug (both are
/// validated ASCII-ish identifiers), so `rsplit_once` on it can never
/// misfire against a name that happens to contain the marker text.
const AGENT_MARKER: char = '\u{0}';

/// Appends a hidden, non-displayed marker carrying `agent_id` to a sentence
/// this module already built, so [`classify`] can recover a real
/// `pairAgentId` for the one call site that has one in hand at the moment it
/// raises the error — `TenantProvider::resolve`'s pin pre-check
/// (`harness/built_in/provider.rs`), which is mid-resolution and still holds
/// the agent's raw id, unlike everything downstream of it. The sentence
/// itself is never displayed with this attached: `classify` strips it before
/// setting `message`, and every OTHER call site (the pin's own fallback in
/// `resolve_choice`, the company-default paths) has no id to attach and
/// calls the plain `pair_broken`/`default_broken` functions unmarked.
pub fn with_agent_marker(sentence: String, agent_id: &str) -> String {
    format!("{sentence}{AGENT_MARKER}{agent_id}")
}

/// Classifies a turn-abort's error text against the sentences this module
/// produces, so `server::operator::spawn_chat_turn` can tell a resolution
/// failure — a broken pin, a broken default, no key, no model chosen at all
/// — from every other failure class (a tool timeout, an empty response, a
/// rate limit), which must keep the host's existing generic wording.
///
/// Substring matching, in the same idiom `operator.rs`'s own
/// `provider_failure_sentence` already uses for other failure classes: there
/// is no structured error type left by the time a turn's error reaches this
/// point (the vendored turn loop converts everything to a plain
/// `anyhow::Error`), so the sentence text itself (plus, where present,
/// [`with_agent_marker`]'s hidden trailer) is the only signal.
///
/// `pair_agent_id` is filled in when the sentence carries
/// [`with_agent_marker`]'s trailer — today, only the pin pre-check — or, for
/// [`HARNESS_UNAVAILABLE_CODE`], when the sentence names the agent itself.
/// Every other caller of `pair_broken`/`provider_has_no_key` (the pin's own
/// unreachable-in-practice fallback in `resolve_choice`) has no id in hand
/// to attach, and `default_broken`'s callers never have an agent at all.
/// `provider_slug` is filled in only for `pair_provider_removed`, whose
/// sentence names the slug directly (there is no provider row left to read
/// a display label from in that one case — see [`pair_broken`]).
///
/// Order matters. The harness arm runs first because a router `detail` can
/// itself contain " uses "; after it, `default_broken` and `pair_broken`'s
/// sentences both contain " uses ", so the company-default prefix is checked
/// before the pair one.
/// The agent id a harness-binding failure names, and the sentence itself cut
/// free of any `Display` prefix — or `None` when `detail` is not one.
///
/// Reads the id out of the sentence rather than from a
/// [`with_agent_marker`] trailer: the router has only the raw id at the point
/// it fails, and the id is already in the text it wrote.
///
/// The warm-up shape is returned cut at `whose last warm-up failed`: the
/// reason after it is `{err}` from a lane's own warm-up, free-form text that
/// can carry a path, a command line or provider output, and chat is not where
/// that belongs. The reason stays on the run record and in the logs. The
/// no-engine shape keeps its tail, which is an authored `unavailable` string,
/// not a captured error — an invariant `harness/lanes.rs` holds by never
/// interpolating a failure it caught into the reason it records.
///
/// Idempotent by construction, which [`classify`] depends on: its own output
/// is a sentence of exactly this shape — the cut form included, which is why
/// the tail is matched without its `: ` — and both `MessageView::project` and
/// `chat_history` re-classify the stored text on every read.
fn harness_binding(detail: &str) -> Option<(String, String)> {
    let bound = detail.find(HARNESS_BOUND_MARKER)?;
    let opener = detail[..bound].rfind(HARNESS_SENTENCE_OPENER)?;
    let agent_id = detail[opener + HARNESS_SENTENCE_OPENER.len()..bound].trim();
    if agent_id.is_empty() {
        return None;
    }
    let name_start = bound + HARNESS_BOUND_MARKER.len();
    let tail_at = name_start + detail[name_start..].find('`')?;
    let tail = &detail[tail_at..];

    if let Some(rest) = tail.strip_prefix(HARNESS_WARMUP_TAIL) {
        if !(rest.starts_with(": ") || rest.starts_with('.')) {
            return None;
        }
        let named = detail[opener..tail_at + HARNESS_WARMUP_TAIL.len()].trim();
        return Some((agent_id.to_string(), format!("{named}.")));
    }
    if tail.starts_with(HARNESS_NO_ENGINE_TAIL) {
        return Some((agent_id.to_string(), detail[opener..].trim().to_string()));
    }
    None
}

pub fn classify(detail: &str) -> Option<ResolutionFailure> {
    // Strip the hidden marker, if present, before any pattern match runs —
    // so a marker's own bytes can never accidentally satisfy one.
    let (detail, pair_agent_id) = match detail.rsplit_once(AGENT_MARKER) {
        Some((sentence, id)) if !id.is_empty() => (sentence, Some(id.to_string())),
        _ => (detail, None),
    };

    fn found(
        code: &'static str,
        detail: &str,
        pair_agent_id: Option<String>,
        provider_slug: Option<String>,
    ) -> ResolutionFailure {
        ResolutionFailure {
            code,
            message: detail.trim().to_string(),
            pair_agent_id,
            provider_slug,
        }
    }

    if let Some((agent_id, sentence)) = harness_binding(detail) {
        let message = if sentence.contains(HARNESS_RETRY_NOTE) {
            sentence
        } else {
            format!("{sentence} {HARNESS_RETRY_NOTE}")
        };
        return Some(ResolutionFailure {
            code: HARNESS_UNAVAILABLE_CODE,
            message,
            pair_agent_id: Some(agent_id),
            provider_slug: None,
        });
    }
    if detail.contains("The company default uses ") {
        if detail.contains(", which is removed.") {
            return Some(found(DEFAULT_PROVIDER_REMOVED_CODE, detail, None, None));
        }
        if detail.contains(", which is turned off.") {
            return Some(found(DEFAULT_PROVIDER_OFF_CODE, detail, None, None));
        }
    }
    if detail.contains(", which has no key.") {
        return Some(found(PROVIDER_NO_KEY_CODE, detail, pair_agent_id, None));
    }
    if detail.contains(" uses ") && detail.contains(", which is removed.") {
        // The one sentence shaped this way with a real slug to recover:
        // `pair_broken`'s `Removed` arm names the raw slug, because there is
        // no row left to read a display label from.
        let slug = detail
            .split(" uses ")
            .nth(1)
            .and_then(|rest| rest.split(", which is").next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        return Some(found(
            PAIR_PROVIDER_REMOVED_CODE,
            detail,
            pair_agent_id,
            slug,
        ));
    }
    if detail.contains(" uses ") && detail.contains(", which is turned off.") {
        return Some(found(PAIR_PROVIDER_OFF_CODE, detail, pair_agent_id, None));
    }
    if detail.contains("No model is chosen") {
        return Some(found(NO_MODEL_CHOSEN_CODE, detail, None, None));
    }
    None
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
