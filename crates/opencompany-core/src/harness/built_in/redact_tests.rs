use super::*;

#[test]
fn redact_secrets_removes_the_value_but_keeps_the_prose() {
    // A one-time-secret link: the key is stripped, the surrounding sentence
    // (the context an agent needs) is kept.
    assert_eq!(
        redact_secrets("here it is https://ots.example/secret/AbCdEf123456 open it"),
        "here it is https://ots.example/secret/[REDACTED] open it"
    );
    // A bearer token in prose.
    assert_eq!(
        redact_secrets("auth with Bearer sk-verylongsecrettoken please"),
        "auth with Bearer [REDACTED] please"
    );
    // A JWT carries `.` between its base64url segments; the whole
    // credential is consumed, not just the header segment.
    assert_eq!(
        redact_secrets(
            "auth with Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.aSignature0123456789 please"
        ),
        "auth with Bearer [REDACTED] please"
    );
    // An opaque key with base64 punctuation (`+`, `/`, `~`, `=`) is also
    // consumed whole rather than leaking the part after the first such char.
    assert_eq!(
        redact_secrets("auth with Bearer aGVsbG8r/d29ybGQ=andtheRestOfTheKey please"),
        "auth with Bearer [REDACTED] please"
    );
    // A short bearer credential is still a secret: the MCP config accepts
    // any non-empty bearer value, so a token-shaped value like `s3cret`
    // (it contains a digit) is redacted despite being under the prose
    // length floor.
    assert_eq!(
        redact_secrets("auth with Bearer s3cret please"),
        "auth with Bearer [REDACTED] please"
    );
    // A digit-free credential is redacted from six characters on: a real
    // if weak value like `secret` must not persist, while prose words
    // after the marker stay short enough to survive.
    assert_eq!(
        redact_secrets("auth with Bearer secret please"),
        "auth with Bearer [REDACTED] please"
    );
    // The scheme is matched case-insensitively (RFC 9110's auth-scheme
    // ABNF), so lower- and upper-case `bearer` are credentials too.
    assert_eq!(
        redact_secrets("auth with bearer sk-longsecret please"),
        "auth with bearer [REDACTED] please"
    );
    assert_eq!(
        redact_secrets("auth with BEARER sk-longsecret please"),
        "auth with BEARER [REDACTED] please"
    );
    // Lower-case prose survives: `bond` (4) is under the digit-free floor.
    assert_eq!(
        redact_secrets("the bearer bond matures in June"),
        "the bearer bond matures in June"
    );
    // Lower-case "bearer" in its ordinary English sense: a plain word
    // after it is prose, not a credential, and must survive untouched.
    assert_eq!(
        redact_secrets("the standard bearer candidate won the race"),
        "the standard bearer candidate won the race"
    );
    assert_eq!(
        redact_secrets("the ring bearer walked down the aisle"),
        "the ring bearer walked down the aisle"
    );
    // A Markdown-backtick or quote wrapper around a credential (formatted
    // chat) must not defeat the scan.
    assert_eq!(
        redact_secrets("auth with Bearer `sk-verylongsecret` please"),
        "auth with Bearer `[REDACTED]` please"
    );
    assert_eq!(
        redact_secrets("auth with Bearer \"sk-verylongsecret\" please"),
        "auth with Bearer \"[REDACTED]\" please"
    );
    // The MCP config keeps the whole trimmed remainder as the bearer value,
    // so a credential can span space-separated fragments — every fragment
    // of the run is redacted, not just the first.
    assert_eq!(
        redact_secrets("auth with Bearer firstpart secondpart please"),
        "auth with Bearer [REDACTED] [REDACTED] please"
    );
    // Trailing prose after a single-token credential stays readable: a
    // short fragment like "please" (6) is under the continuation floor.
    assert_eq!(
        redact_secrets("auth with Bearer sk-abc123 please"),
        "auth with Bearer [REDACTED] please"
    );
    // No marker: borrowed through untouched, no allocation.
    assert!(matches!(
        redact_secrets("nothing secret here"),
        std::borrow::Cow::Borrowed(_)
    ));
    // Too short after the marker to be a secret: left alone, so ordinary
    // text like "Bearer or not" is not mangled.
    assert_eq!(redact_secrets("Bearer or not"), "Bearer or not");
    // The MCP config trims the value, so extra whitespace between the
    // marker and a credential is legal — and must not leave the credential
    // verbatim because the value scan stopped at the first space.
    assert_eq!(
        redact_secrets("auth with Bearer   sk-longsecret please"),
        "auth with Bearer   [REDACTED] please"
    );
    // Dots do not turn a short prose word into a secret: "key." is still
    // under the digit-free threshold, and plain "token" (no trailing dot)
    // is too.
    assert_eq!(redact_secrets("Bearer key. Please"), "Bearer key. Please");
    assert_eq!(redact_secrets("Bearer token please"), "Bearer token please");
}
