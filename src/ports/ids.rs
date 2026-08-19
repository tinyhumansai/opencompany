//! Dependency-free identifier and timestamp sources for the runtime.
//!
//! Phase 1 avoids pulling `uuid`/`ulid`/`chrono`. Minted string ids combine an
//! epoch-millis prefix with a process-global monotonic counter so they are
//! collision-safe in-process, human-readable in JSONL, and lexicographically
//! monotonic (both components are zero-padded hex).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Current wall-clock time as epoch milliseconds.
///
/// Returns `0` if the system clock is set before the Unix epoch (never in
/// practice); callers treat the value as an opaque monotonic-ish stamp.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mints a fresh process-unique id of the form `{millis:012x}-{counter:012x}`.
///
/// The counter is strictly increasing, so two calls always differ and — given
/// a non-decreasing clock — sort in mint order.
///
/// # Uniqueness is process-local
///
/// `COUNTER` starts at zero in every process and the millis prefix has
/// millisecond resolution, so two processes that start within the same
/// millisecond mint *identical* ids. Never use a minted id to name an entry in
/// a directory other processes share — `/tmp` above all. Tests that need a
/// private path must take one from `tempfile` (`tempfile::Builder::new()
/// .prefix("opencompany-…").tempdir()`), which asks the OS for a name no other
/// process can hold, rather than deriving one from `generate_id`.
///
/// # Never where unpredictability is required
///
/// A minted id is fully guessable from a prior one: the counter steps by one
/// and the prefix is the wall clock. It must not be used for a token, a
/// secret, a capability URL, or a nonce — anything whose safety rests on a
/// reader being unable to name the next value. Those come from the OS CSPRNG
/// through [`TokenSource`](crate::server::users::token::TokenSource); see
/// [`mint_session_token`](crate::server::users::token::mint_session_token) and
/// the x402 authorization nonce for the two shapes already in the tree.
pub fn generate_id() -> String {
    let millis = now_millis();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis:012x}-{counter:012x}")
}

/// The author a **host-authored notice** is journaled under (issue #966).
///
/// Not an agent, and deliberately not a destination either. Three sites emit
/// prose the runtime wrote itself — an approval-overflow notice, the
/// `"Acknowledged."` cycle fallback, and a failed-continuation report — and each
/// used to land as an ordinary `AgentReply` carrying `"operator"` in the author
/// field. That made them **byte-identical to a reply whose author was
/// overwritten** by the pre-#885 defect, so no reader could tell a correct
/// system row from a damaged agent row.
///
/// The value matches the literal `MessageView::project` already uses for the
/// `DeskTaskCompleted` marker, which is what routes it to the console's centred
/// system pill rather than a company bubble — so nothing on the read side has to
/// learn a new word.
///
/// **Forward-only.** Notices already journaled keep `"operator"` and stay
/// indistinguishable, permanently: the distinguishing information was never
/// written down, and nothing recovers it after the fact.
pub const SYSTEM_AUTHOR: &str = "system";

/// The agent id a confined turn runs under (issue #416).
///
/// Deliberately **not** a roster id: it names no teammate, carries no manifest
/// grants, and cannot be addressed.
///
/// Lives here rather than in `harness::confine` (issue #966) because the whole
/// harness is `#[cfg(feature = "openhuman")]`, and the chat-history attribution
/// audit — which compiles in the default build — has to recognise it as a
/// *known author*. `confine` re-exports it, so every existing
/// `confine::CONFINED_AGENT_ID` reference is unchanged. Copying the literal into
/// the audit instead would create exactly the silent twin the console's
/// `mapComposioCategory` carries a warning about.
pub const CONFINED_AGENT_ID: &str = "workflow-copilot";

/// The stem [`agent_slug`] falls back to when a display name yields nothing a
/// roster id may legally be.
pub const AGENT_SLUG_FALLBACK: &str = "teammate";

/// The longest slug [`agent_slug`] will return, in bytes (all ASCII, so also in
/// characters). A roster id becomes a workspace folder name and a path segment
/// in every search hit that quotes it; a name pasted from a paragraph should not
/// turn into a path nobody can read across.
const AGENT_SLUG_MAX: usize = 64;

/// Derives a readable, snake_case roster id from a teammate's display name.
///
/// `"Dana Designer"` becomes `dana_designer` — the same grammar the manifest
/// validator enforces on hand-authored `[[agent]].id`s (lowercase letters,
/// digits and underscores, starting with a letter), so a runtime-added teammate
/// and a blueprint one name their `Agents/<id>/` folder the same way. Before
/// this, runtime teammates took [`generate_id`] and read as
/// `Agents/019fad5ada20-000000000003/` (issue #686).
///
/// Deliberately **underscores, not hyphens** — unlike
/// [`company_id_from_name`](crate::runtime::company_id_from_name), whose output
/// is a company slug and answers to no such validator. The roster id grammar is
/// already set by the manifest, and a hyphen dialect would make a third id shape
/// to reason about rather than one fewer.
///
/// # Not unique on its own
///
/// This is pure normalization: two teammates named "Designer" both slug to
/// `designer`. Nothing should call it to *mint* an id —
/// [`CompanyRecord::mint_agent_id`](crate::ports::types::CompanyRecord::mint_agent_id)
/// is the minting entry point, and it resolves collisions against the roster
/// the slug has to be unique within.
///
/// # Degenerate names
///
/// A name with no ASCII letter to start on — `"***"`, `""`, `"24/7 Support"`,
/// an all-non-ASCII name — has no readable slug in it, and no transliteration is
/// attempted. Those return [`AGENT_SLUG_FALLBACK`], which mints as `teammate`,
/// `teammate_2`, … exactly like any other stem. That is the same trade
/// `company_id_from_name` makes with `"company"`.
pub fn agent_slug(display_name: &str) -> String {
    let mut slug = String::with_capacity(display_name.len().min(AGENT_SLUG_MAX));
    let mut prev_underscore = false;
    for ch in display_name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore {
            // Every other run of characters — spaces, punctuation, emoji, CJK —
            // collapses to a single separator rather than one per character.
            slug.push('_');
            prev_underscore = true;
        }
    }
    // Trim first, then cap: the cap must not be spent on separators that were
    // going to be dropped anyway. Every pushed character is ASCII, so slicing
    // by byte index can never split one.
    let trimmed = slug.trim_matches('_');
    let capped = trimmed[..trimmed.len().min(AGENT_SLUG_MAX)].trim_end_matches('_');
    // A slug that does not start with a lowercase letter would fail the
    // manifest's own `is_snake_case` check, so it is not a legal roster id at
    // all — digit-leading names land here alongside empty ones.
    if capped.starts_with(|c: char| c.is_ascii_lowercase()) {
        capped.to_string()
    } else {
        AGENT_SLUG_FALLBACK.to_string()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn generated_ids_are_distinct_and_monotonic() {
        let a = generate_id();
        let b = generate_id();
        assert_ne!(a, b);
        // Zero-padded fixed-width hex makes lexicographic order match mint order.
        assert!(b > a, "expected {b} > {a}");
    }

    #[test]
    fn agent_slug_lowercases_and_joins_words_with_underscores() {
        assert_eq!(agent_slug("Dana Designer"), "dana_designer");
        assert_eq!(agent_slug("Backend Engineer"), "backend_engineer");
        assert_eq!(agent_slug("QA2"), "qa2");
    }

    #[test]
    fn agent_slug_collapses_and_trims_separator_runs() {
        assert_eq!(agent_slug("Designer!!"), "designer");
        assert_eq!(agent_slug("  Head   of   Ops  "), "head_of_ops");
        assert_eq!(agent_slug("__Dana--Designer__"), "dana_designer");
        assert_eq!(agent_slug("Ana Maria (Growth)"), "ana_maria_growth");
    }

    /// A name with no ASCII letter to start on has no readable slug in it, and
    /// none is invented — it takes the shared stem instead.
    #[test]
    fn agent_slug_falls_back_for_names_with_no_legal_stem() {
        for name in ["***", "", "   ", "24/7 Support", "設計者", "🙂", "7", "_"] {
            assert_eq!(
                agent_slug(name),
                AGENT_SLUG_FALLBACK,
                "expected the fallback stem for {name:?}"
            );
        }
    }

    #[test]
    fn agent_slug_caps_length_without_a_trailing_separator() {
        // The cap is a hard byte cap, not a word boundary: a word straddling it
        // is truncated rather than dropped, which keeps the rule one sentence.
        let long = "Chief ".repeat(40);
        let slug = agent_slug(&long);
        assert_eq!(slug.len(), AGENT_SLUG_MAX);
        assert!(slug.starts_with("chief_chief_"), "{slug}");
        assert!(
            !slug.ends_with('_'),
            "cap left a dangling separator: {slug}"
        );

        // When the cap lands exactly on a separator, that separator goes with
        // it — a roster id never ends in `_`.
        let aligned = "abcdefg ".repeat(20);
        let slug = agent_slug(&aligned);
        assert_eq!(slug.len(), AGENT_SLUG_MAX - 1, "{slug}");
        assert!(slug.ends_with("abcdefg"), "{slug}");

        let dense = "a".repeat(100);
        assert_eq!(agent_slug(&dense).len(), AGENT_SLUG_MAX);
    }

    /// The one property that matters: whatever comes out is something the
    /// manifest validator would accept as a roster id. Coupled to the real
    /// `is_snake_case` rather than a copy of its rules, so a change to the
    /// grammar fails here instead of shipping ids the validator rejects.
    #[test]
    fn every_slug_satisfies_the_manifest_id_grammar() {
        let mut names: Vec<String> = [
            "Dana Designer",
            "Designer!!",
            "***",
            "",
            "24/7 Support",
            "設計者",
            "🙂 Growth 🙂",
            "O'Brien-Smith, Jr.",
            "___",
            "9Lives",
            "a",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        names.push("Chief ".repeat(40));
        names.push("a".repeat(100));
        for name in &names {
            let slug = agent_slug(name);
            assert!(
                crate::company::is_snake_case(&slug),
                "slug {slug:?} from name {name:?} is not a legal roster id"
            );
        }
    }
}
