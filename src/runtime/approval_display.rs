//! Issue #372: projecting a parked [`Effect`]'s payload into something an
//! operator can read on the approval card — safely, and **host-side**.
//!
//! # Why the payload has to be shown at all
//!
//! An approval card that says only `Shell` asks the operator to authorise an
//! action they cannot see. The only rational responses are to approve
//! everything or refuse everything, and both defeat the gate. The payload of a
//! harness-projected effect *is* the thing being consented to — it is the
//! verbatim tool-call argument object
//! ([`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for)),
//! and issue #243 even lets the operator amend it before approving. So the
//! default here is **show it in full**: a `shell` command and a `glob` pattern
//! reach the card exactly as the agent wrote them.
//!
//! # The redaction rule, stated once
//!
//! The denylist below is a *safety net around* that default, not the policy.
//! An object entry whose key names a credential — by segment or by suffix, on a
//! normalised form of the key — is replaced by a fixed [`REDACTED`] marker; the
//! entry itself stays so the shape of the payload is still legible. Everything
//! else is copied through. Matching is deliberately over-eager (a
//! `session_id` is redacted; `author` is not, because `authorization` is a
//! whole segment and `author` is not a suffix of it) — over-redaction is the
//! safe direction, an unlisted key holding a secret is not.
//!
//! Redaction runs at **projection**, in
//! [`CompanyRuntime::pending_approvals`](crate::company::runtime::CompanyRuntime::pending_approvals),
//! so a secret never crosses the wire at all. This mirrors the guarantee
//! `src/harness/steps.rs` keeps for turn steps, and is tested the same way:
//! [`planted_secret_never_reaches_display_payload`] plants a fake credential at
//! the top level, nested in an object, and inside an array, and asserts it
//! appears nowhere in the serialized result.
//!
//! # Bounds
//!
//! A payload is agent-authored free text and is therefore bounded before it is
//! handed to a browser: strings are truncated at [`MAX_STRING_CHARS`] (on a
//! character boundary — never a byte slice), and the walk fails **closed** to
//! [`UNRENDERABLE`] past [`MAX_DEPTH`] or [`MAX_NODES`] rather than emitting an
//! unbounded blob.

use serde_json::{Map, Value};

use crate::ports::types::Effect;

/// What replaces a value whose key names a credential.
pub const REDACTED: &str = "[redacted]";

/// What replaces a subtree too deep or too large to render.
pub const UNRENDERABLE: &str = "[unrenderable]";

/// Longest string carried to the card, in characters. Anything longer is
/// truncated and marked with an ellipsis.
const MAX_STRING_CHARS: usize = 2_000;

/// Deepest nesting walked before failing closed.
const MAX_DEPTH: usize = 8;

/// Most values walked before failing closed. Bounds breadth the way
/// [`MAX_DEPTH`] bounds depth — a flat 100k-entry array is as unrenderable as a
/// deeply nested one.
const MAX_NODES: usize = 512;

/// Single normalised segments that name a credential. A key is redacted when
/// any of its segments equals one of these, or ends with one (so `mytoken` and
/// `x-api-key` are both caught).
const DENY_SEGMENTS: &[&str] = &[
    "secret",
    "secrets",
    "token",
    "tokens",
    "password",
    "passwords",
    "passwd",
    "apikey",
    "apikeys",
    "authorization",
    "bearer",
    "credential",
    "credentials",
    "session",
    "cookie",
    "cookies",
    "signature",
    "privatekey",
];

/// Multi-segment phrases that name a credential only when their segments are
/// adjacent. Kept apart from [`DENY_SEGMENTS`] so `key`, `access` and `client`
/// stay usable on their own — `key.rotate`'s payload should still be readable.
const DENY_PHRASES: &[&[&str]] = &[
    &["api", "key"],
    &["private", "key"],
    &["public", "key"],
    &["secret", "key"],
    &["access", "key"],
    &["access", "token"],
    &["client", "secret"],
    &["auth", "key"],
    &["signing", "key"],
];

/// The operator-facing view of a parked effect's payload, or `None` when there
/// is nothing to show.
///
/// `None` means "this effect carries no arguments" — a null, an empty object,
/// an empty array or an empty string — and the console renders the card exactly
/// as it did before this field existed. That is the same shape an **old host**
/// produces (the field is omitted from the wire entirely), which is what makes
/// the change additive in both directions.
pub fn display_payload(effect: &Effect) -> Option<Value> {
    if is_empty(&effect.payload) {
        return None;
    }
    Some(redact(&effect.payload))
}

/// The redaction rule above, applied to a **bare** value.
///
/// The one seam that lets a second operator-facing surface reuse this rather
/// than growing a redactor of its own. Issue #411's turn-step trace calls it to
/// show *what a tool call was doing* — and those arguments are literally the
/// same object [`display_payload`] shows on the approval card, because
/// [`ApprovalPolicy::effect_for`](crate::harness::policy::ApprovalPolicy::effect_for)
/// projects a gated call's arguments straight into `Effect::payload`. One rule,
/// one denylist, one set of bounds: a key added to [`DENY_SEGMENTS`] tightens
/// both surfaces at once, and neither can drift away from the other.
///
/// Unlike [`display_payload`] this makes no "is there anything worth showing"
/// judgement — an empty object redacts to an empty object. The caller decides
/// what to do with nothing, because "nothing" means different things on a card
/// (omit the section) and in a trace (the call took no arguments).
pub fn redact(value: &Value) -> Value {
    let mut budget = MAX_NODES;
    walk(value, 0, &mut budget)
}

/// Whether a payload carries nothing worth putting on a card.
fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Recursively copies `value`, redacting credential-named entries and failing
/// closed past the depth/node bounds.
fn walk(value: &Value, depth: usize, budget: &mut usize) -> Value {
    if depth > MAX_DEPTH || *budget == 0 {
        return Value::String(UNRENDERABLE.to_string());
    }
    *budget -= 1;

    match value {
        Value::String(s) => Value::String(truncate(s)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| walk(item, depth + 1, budget))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, item) in map {
                if is_sensitive_key(key) {
                    // The key stays so the payload's shape is still legible;
                    // only the value is dropped. The walk does NOT descend —
                    // a redacted subtree could hide the secret one level down.
                    out.insert(key.clone(), Value::String(REDACTED.to_string()));
                    continue;
                }
                out.insert(key.clone(), walk(item, depth + 1, budget));
            }
            Value::Object(out)
        }
        // Numbers, booleans and null are copied verbatim: they are bounded by
        // construction and carry no free text.
        other => other.clone(),
    }
}

/// Truncates on a **character** boundary, reserving room for the ellipsis.
///
/// Byte-slicing here would panic mid-codepoint on any payload with non-ASCII
/// text, which agent-authored arguments routinely carry.
fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_STRING_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_STRING_CHARS).collect();
    out.push('…');
    out
}

/// Whether an object key names a credential.
///
/// The key is normalised first — lowercased, camelCase split, every
/// non-alphanumeric run treated as a separator — so `clientSecret`,
/// `CLIENT_SECRET`, `client-secret` and `client.secret` all reduce to the same
/// segments. Then:
///
/// * any segment equal to, or ending with, a [`DENY_SEGMENTS`] entry, or
/// * any adjacent run of segments matching a [`DENY_PHRASES`] entry
///
/// redacts. Suffix matching is why `mytoken` is caught; whole-segment matching
/// is why `author` is not (it is neither equal to nor suffixed by
/// `authorization`).
fn is_sensitive_key(key: &str) -> bool {
    let segments = normalise_key(key);
    if segments
        .iter()
        .any(|seg| DENY_SEGMENTS.iter().any(|deny| seg.ends_with(deny)))
    {
        return true;
    }
    DENY_PHRASES.iter().any(|phrase| {
        segments
            .windows(phrase.len())
            .any(|window| window.iter().zip(phrase.iter()).all(|(a, b)| a == b))
    })
}

/// Splits a key into lowercase alphanumeric segments, breaking on
/// non-alphanumeric runs **and** on camelCase humps.
fn normalise_key(key: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut prev_lower_or_digit = false;

    for ch in key.chars() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            prev_lower_or_digit = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower_or_digit && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
        current.extend(ch.to_lowercase());
        prev_lower_or_digit = ch.is_lowercase() || ch.is_numeric();
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::EffectGroup;

    /// An obvious fake. Never a real credential shape that could be mistaken
    /// for one in a log or a diff.
    const FAKE_SECRET: &str = "NOT-A-REAL-KEY-planted-for-tests";

    fn effect(payload: Value) -> Effect {
        Effect {
            kind: "shell".into(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload,
            agent: Some("engineer".into()),
            run_id: None,
        }
    }

    fn rendered(payload: Value) -> String {
        serde_json::to_string(&display_payload(&effect(payload)).expect("a payload")).unwrap()
    }

    /// SECURITY: the guarantee this module exists to keep. A planted credential
    /// at the top level, nested in an object, and inside an array must appear
    /// in **no** serialized output.
    #[test]
    fn planted_secret_never_reaches_display_payload() {
        let out = rendered(serde_json::json!({
            "command": "deploy.sh",
            "api_key": FAKE_SECRET,
            "env": { "GITHUB_TOKEN": FAKE_SECRET, "HOME": "/workspace" },
            "headers": [
                { "Authorization": format!("Bearer {FAKE_SECRET}") },
                { "name": "accept", "value": "application/json" }
            ],
        }));

        assert!(!out.contains(FAKE_SECRET), "secret leaked into: {out}");
        // The non-sensitive siblings survive — this is a redactor, not a mute.
        assert!(out.contains("deploy.sh"));
        assert!(out.contains("application/json"));
        assert_eq!(out.matches(REDACTED).count(), 3);
    }

    /// The point of #372: a shell command reaches the card verbatim.
    #[test]
    fn shell_command_is_shown_in_full() {
        let payload = serde_json::json!({
            "command": "cargo test --locked --features openhuman -- --nocapture",
        });
        let out = display_payload(&effect(payload)).unwrap();
        assert_eq!(
            out["command"],
            Value::String("cargo test --locked --features openhuman -- --nocapture".into())
        );
    }

    /// ...and so do a glob's pattern and path.
    #[test]
    fn glob_pattern_and_path_are_shown_in_full() {
        let out = display_payload(&effect(
            serde_json::json!({ "pattern": "**/*.rs", "path": "src/runtime" }),
        ))
        .unwrap();
        assert_eq!(out["pattern"], Value::String("**/*.rs".into()));
        assert_eq!(out["path"], Value::String("src/runtime".into()));
    }

    /// The denylist must not eat ordinary words that merely share a prefix.
    #[test]
    fn ordinary_keys_survive_the_denylist() {
        for key in [
            // `author` is the canonical false positive: a bare-substring rule
            // over `authorization` would eat it.
            "author",
            "authors",
            "authored_by",
            "command",
            "pattern",
            "path",
            "keyword",
            "keys",
            "accessed_at",
            "client_name",
            "amount_usd",
        ] {
            assert!(!is_sensitive_key(key), "{key} should not be redacted");
        }
    }

    /// ...and must catch every spelling of a credential key we can name.
    #[test]
    fn credential_keys_are_redacted_in_every_spelling() {
        for key in [
            "api_key",
            "apiKey",
            "APIKEY",
            "x-api-key",
            "GITHUB_TOKEN",
            "access_token",
            "clientSecret",
            "CLIENT_SECRET",
            "client.secret",
            "Authorization",
            "password",
            "passwd",
            "private_key",
            "credentials",
            "session",
            "mytoken",
            "bearer",
        ] {
            assert!(is_sensitive_key(key), "{key} should be redacted");
        }
    }

    /// A redacted key is not descended into — a secret one level below a
    /// credential-named object must not escape through the child walk.
    #[test]
    fn a_redacted_subtree_is_not_descended() {
        let out = rendered(serde_json::json!({
            "credentials": { "nested": { "deeper": FAKE_SECRET } },
        }));
        assert!(!out.contains(FAKE_SECRET));
        assert!(!out.contains("deeper"));
    }

    #[test]
    fn long_strings_are_truncated_on_a_character_boundary() {
        // Multi-byte throughout: a byte slice here would panic mid-codepoint.
        let long: String = "é".repeat(MAX_STRING_CHARS + 500);
        let out = display_payload(&effect(serde_json::json!({ "command": long }))).unwrap();
        let shown = out["command"].as_str().unwrap();
        assert_eq!(shown.chars().count(), MAX_STRING_CHARS + 1);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn a_string_at_the_cap_is_left_alone() {
        let exact = "x".repeat(MAX_STRING_CHARS);
        let out =
            display_payload(&effect(serde_json::json!({ "command": exact.clone() }))).unwrap();
        assert_eq!(out["command"], Value::String(exact));
    }

    #[test]
    fn excessive_depth_fails_closed() {
        let mut deep = serde_json::json!(FAKE_SECRET);
        for _ in 0..(MAX_DEPTH + 4) {
            deep = serde_json::json!({ "next": deep });
        }
        let out = serde_json::to_string(&display_payload(&effect(deep)).unwrap()).unwrap();
        assert!(!out.contains(FAKE_SECRET), "secret escaped the depth bound");
        assert!(out.contains(UNRENDERABLE));
    }

    #[test]
    fn excessive_breadth_fails_closed() {
        let wide: Vec<Value> = (0..(MAX_NODES + 50))
            .map(|i| Value::String(format!("item-{i}")))
            .collect();
        let out = serde_json::to_string(
            &display_payload(&effect(serde_json::json!({ "items": wide }))).unwrap(),
        )
        .unwrap();
        assert!(out.contains(UNRENDERABLE));
    }

    #[test]
    fn empty_payloads_render_nothing() {
        for payload in [
            Value::Null,
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!("   "),
        ] {
            assert!(display_payload(&effect(payload)).is_none());
        }
    }

    /// Scalars are copied verbatim — the amount an operator is approving is
    /// exactly the number the agent asked for.
    #[test]
    fn scalars_are_copied_verbatim() {
        let out = display_payload(&effect(
            serde_json::json!({ "amount_usd": 42.5, "dry_run": false, "note": null }),
        ))
        .unwrap();
        assert_eq!(out["amount_usd"], serde_json::json!(42.5));
        assert_eq!(out["dry_run"], Value::Bool(false));
        assert_eq!(out["note"], Value::Null);
    }
}
