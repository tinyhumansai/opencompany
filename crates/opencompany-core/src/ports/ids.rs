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

/// Milliseconds in one UTC day.
///
/// `pub(crate)`: `server::graphql::usage` also buckets by UTC day and reused
/// this constant from its old home in `server::graphql` — kept reachable
/// under its new one rather than growing a second copy (P3-7, keys rework
/// #2306 review).
pub(crate) const MILLIS_PER_DAY: u64 = 86_400_000;

/// The `(year, month, day)` of an epoch day, via Hinnant's public-domain
/// `civil_from_days`. Kept local so [`iso8601`] needs no date dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Formats epoch-millis as an RFC-3339 / ISO-8601 UTC timestamp (second
/// precision) — the crate's one formatter for this shape: the console's
/// GraphQL read plane (`updatedAt`/`at`), analytics event timestamps, the
/// account-key fan-out's health record, and every other dependency-free
/// `now_rfc3339`-style helper in the tree all call this rather than growing a
/// second copy of the civil-date arithmetic.
///
/// Moved here from `server::graphql` (keys rework #2306, P3-7 review): a
/// pure, dependency-free formatter belongs at the `ports` layer every other
/// layer can already reach, not under `server` — `src/company/` must never
/// import from `server`, and the account-key fan-out
/// (`company::company_key::fan_out`) needs exactly this formatter for its
/// own health record.
pub fn iso8601(at_millis: u64) -> String {
    let (y, m, d) = civil_from_days((at_millis / MILLIS_PER_DAY) as i64);
    let secs = (at_millis % MILLIS_PER_DAY) / 1000;
    let (h, min, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
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
/// [`mint_session_token`](crate::server::users::token::mint_session_token) for
/// the shape already in the tree.
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
/// and a blueprint one name their `agents/<id>/` folder the same way. Before
/// this, runtime teammates took [`generate_id`] and read as
/// `agents/019fad5ada20-000000000003/` (issue #686).
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
#[path = "ids_tests.rs"]
mod tests;
