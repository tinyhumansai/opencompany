//! Classifying a Composio credential check (issue #2275).
//!
//! A console that pastes a Composio API key wants to know whether it works
//! before it commits the company to it. The naive version of that — "the probe
//! failed, so the key is bad" — is the one that has to be avoided, because the
//! probe fails for at least four reasons that have nothing to do with the key,
//! and treating those as a bad key throws away a working credential on the
//! operator's behalf.
//!
//! So the probe is **classified, not boolean**, and only one class is
//! destructive. Everything here is a pure function over the error text: the
//! decision is one function, the operator-facing sentence is another, and
//! neither touches the network or a store. That split is what makes every
//! branch below testable without standing up a Composio backend, which is the
//! same arrangement `harness::built_in::mcp_probe::classify_mcp_error` already
//! uses for MCP transport errors.
//!
//! ## Branch order is load-bearing
//!
//! The order in [`classify`] is not stylistic; two of the arms exist because of
//! real bugs in the inference surface this is ported from
//! (`docs/modules/inference/connect-flow.md`):
//!
//! * **The proxy/WAF arm runs first** and answers [`ComposioProbeClass::Unknown`].
//!   Otherwise the word *authentication* inside `407 Proxy Authentication
//!   Required` matches the auth arm, and a corporate proxy sitting between the
//!   host and Composio deletes a perfectly valid key.
//! * **A `403` is auth only when credential wording is with it.** A WAF's bare
//!   `403 Forbidden` is about the request, not the key. And the digit tests use
//!   **word boundaries**, so `401`/`403` cannot match inside an identifier like
//!   `ca_1403`.
//!
//! ## There is deliberately no `Model` class
//!
//! The inference flow has one, because a provider can accept the key and reject
//! the model id. Composio has no model concept at all — there is nothing for
//! such a class to mean here, and an arm that could never fire is an arm nobody
//! can reason about.
//!
//! ## `Unknown` never quotes the upstream text
//!
//! [`describe`] returns a fixed string per class. The raw error can echo request
//! material — headers, a key fragment, a proxy's own error page — and the copy
//! it would land in is a banner an operator screenshots. The raw text belongs in
//! a tracing/debug channel and nowhere else; the caller
//! ([`server::ops::composio`](crate::server::ops::composio)) logs it at `debug`
//! and puts only [`describe`]'s sentence on the wire.

use serde::Serialize;

/// Why a Composio credential check did not come back clean.
///
/// Only [`Self::Auth`] says anything about the key itself. Every other variant
/// is a statement about the *connection*, so the key is kept and the operator
/// gets an advisory rather than having their paste thrown away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ComposioProbeClass {
    /// Composio rejected the credential. The **only** destructive class.
    Auth,
    /// Nothing answered, or what answered was not the Composio API.
    Endpoint,
    /// The account is out of credit or is being rate-limited.
    Quota,
    /// Composio did not answer in time.
    Timeout,
    /// The check did not complete, and nothing about it is a statement about
    /// the key — a proxy, a WAF, a captive portal, or a build with no Composio
    /// client in it at all.
    Unknown,
}

impl ComposioProbeClass {
    /// The stable wire spelling (`auth` / `endpoint` / `quota` / `timeout` /
    /// `unknown`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Endpoint => "endpoint",
            Self::Quota => "quota",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this class means the credential must **not** be stored.
    ///
    /// Exactly one class does. Callers branch on this rather than on the
    /// variant so that a class added later has to state its own destructiveness
    /// here, in one place, instead of being silently lumped in with the
    /// advisory ones at every call site.
    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Auth)
    }
}

impl std::fmt::Display for ComposioProbeClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Wording that, alongside a `403`, means the response is about the credential
/// rather than about the request.
///
/// `forbidden` is deliberately **not** here: that is the word a WAF uses, and
/// including it would collapse the whole 403 distinction back into the bug this
/// arm exists to avoid.
const CREDENTIAL_WORDING: &[&str] = &[
    "api key",
    "api-key",
    "apikey",
    "x-api-key",
    "credential",
    "unauthorized",
    "unauthenticated",
    "authentication",
    "invalid key",
    "invalid token",
];

/// A proxy, a WAF, or a gateway in the path — never a statement about the key.
const PROXY_MARKERS: &[&str] = &["proxy", "cloudflare", "bad gateway"];

/// Out of credit, or being throttled.
const QUOTA_MARKERS: &[&str] = &[
    "rate limit",
    "rate-limit",
    "too many requests",
    "out of credit",
    "insufficient credit",
    "quota",
];

/// Nothing answered, or the wrong thing did.
const ENDPOINT_MARKERS: &[&str] = &[
    "not found",
    "dns",
    "connection refused",
    "could not resolve",
    "name or service not known",
    "no such host",
];

/// Composio did not answer in time.
const TIMEOUT_MARKERS: &[&str] = &["timeout", "timed out", "deadline"];

/// Classify a raw probe error into a [`ComposioProbeClass`]. **Pure.**
///
/// The arm order is the contract — see the module docs for the two orderings
/// that exist because of real bugs. Matching is case-insensitive; HTTP status
/// codes are matched on **word boundaries** so an identifier that merely
/// contains the digits cannot be read as a status.
pub fn classify(err: &str) -> ComposioProbeClass {
    let text = err.to_ascii_lowercase();

    // 1. Proxy / WAF / gateway. FIRST, and non-destructive: `407 Proxy
    //    Authentication Required` carries the word the auth arm looks for, and
    //    a corporate proxy must never be able to delete a valid key.
    if has_code(&text, "407") || PROXY_MARKERS.iter().any(|m| text.contains(m)) {
        return ComposioProbeClass::Unknown;
    }

    // 2. The credential itself. A bare `403 Forbidden` from a WAF is not this;
    //    a 403 that also talks about a key or authentication is.
    if has_code(&text, "401")
        || (has_code(&text, "403") && CREDENTIAL_WORDING.iter().any(|w| text.contains(w)))
    {
        return ComposioProbeClass::Auth;
    }

    // 3. Credit / throttling. Before the endpoint arm so a `429` body that also
    //    says "not found" cannot be read as a wrong address.
    if has_code(&text, "429") || QUOTA_MARKERS.iter().any(|m| text.contains(m)) {
        return ComposioProbeClass::Quota;
    }

    // 4. Nothing there, or the wrong thing.
    if has_code(&text, "404") || ENDPOINT_MARKERS.iter().any(|m| text.contains(m)) {
        return ComposioProbeClass::Endpoint;
    }

    // 5. Answered too slowly, or not at all within the budget.
    if TIMEOUT_MARKERS.iter().any(|m| text.contains(m)) {
        return ComposioProbeClass::Timeout;
    }

    ComposioProbeClass::Unknown
}

/// The operator-facing sentence for a class. **Pure, and copy only.**
///
/// A fixed string per class, never an interpolation of the upstream error: see
/// the module docs. [`ComposioProbeClass::Auth`]'s copy is a refusal (nothing
/// was stored); every other class's copy is an advisory attached to a write
/// that **succeeded**, and says so, because colouring a completed save as a
/// failure is a lie about what happened.
pub fn describe(class: ComposioProbeClass) -> &'static str {
    match class {
        ComposioProbeClass::Auth => {
            "Composio rejected this API key. Check it at app.composio.dev and paste it again — \
             nothing was changed."
        }
        ComposioProbeClass::Endpoint => {
            "Saved, but nothing answered at Composio's API host, so the key could not be checked."
        }
        ComposioProbeClass::Quota => {
            "Saved. Composio answered that this account is out of credit or is being \
             rate-limited, so the key could not be checked."
        }
        ComposioProbeClass::Timeout => {
            "Saved, but Composio did not answer in time, so the key could not be checked."
        }
        ComposioProbeClass::Unknown => "Saved, but the check did not complete.",
    }
}

/// The same fact as [`describe`], with **no claim about a write**. Pure, copy
/// only, and a fixed string per class.
///
/// A second table rather than one shared sentence, because the two callers are
/// answering different questions and only one of them stored anything.
/// [`describe`] is an advisory hung on a completed write and says `Saved, …`;
/// this is the verdict of a check-only route
/// (`POST …/composio/api-key/test`), which writes nothing on any path. Handing
/// that route `describe`'s copy would have it report `Saved, …` about a call
/// that saved nothing — the confident-wrong-answer this whole surface refuses
/// elsewhere, and the sort of thing a screenshot of the banner would then
/// enshrine.
///
/// The class vocabulary is shared; only the framing differs. A class added to
/// [`ComposioProbeClass`] must gain a row in both, and the tests below pin that
/// every class has one and that the two tables never collide.
pub fn describe_verdict(class: ComposioProbeClass) -> &'static str {
    match class {
        ComposioProbeClass::Auth => {
            "Composio rejected this API key. Check it at app.composio.dev and paste it again."
        }
        ComposioProbeClass::Endpoint => "Nothing answered at Composio's API host.",
        ComposioProbeClass::Quota => {
            "Composio answered that this account is out of credit or is being rate-limited."
        }
        ComposioProbeClass::Timeout => "Composio did not answer in time.",
        ComposioProbeClass::Unknown => "The check did not complete.",
    }
}

/// Whether `text` contains `code` as a **word**, not as a run of digits inside
/// something else.
///
/// `ca_1403` and `request-4011` must not read as a `403` or a `401`. Both
/// neighbours have to be non-alphanumeric, which is what every real status line
/// gives (`composio answered 401 unauthorized`, `http 404`) and what an
/// identifier does not.
fn has_code(text: &str, code: &str) -> bool {
    text.match_indices(code).any(|(at, _)| {
        let before_ok = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_ok = text[at + code.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        before_ok && after_ok
    })
}

#[cfg(test)]
mod tests {
    use super::{ComposioProbeClass as C, classify, describe, describe_verdict, has_code};

    /// Every class, and the two orderings that exist because of real bugs.
    ///
    /// A table rather than one test per row: the decision is a single pure
    /// function over a string, and the thing worth pinning is the *whole*
    /// mapping — a reordered arm shows up here as several rows moving at once,
    /// which is exactly what a reviewer needs to see.
    #[test]
    fn the_classifier_covers_every_class_and_never_deletes_a_key_for_a_proxy() {
        let cases: &[(&str, C)] = &[
            // The bug the first arm exists for. "Authentication" is right there
            // in the text, and it is a proxy's word, not Composio's.
            ("407 Proxy Authentication Required", C::Unknown),
            ("HTTP 407", C::Unknown),
            ("Cloudflare: error 1020 access denied", C::Unknown),
            ("502 Bad Gateway", C::Unknown),
            ("proxy connect failed", C::Unknown),
            // A WAF's bare 403 says nothing about the key.
            ("403 Forbidden", C::Unknown),
            // The same code, with wording that *is* about the key.
            ("403 Forbidden: invalid api key", C::Auth),
            ("Composio answered 403 — unauthorized", C::Auth),
            // 401 needs no wording; it is the credential status.
            ("Composio answered 401 Unauthorized", C::Auth),
            ("401", C::Auth),
            // Word boundaries: an identifier that merely contains the digits.
            ("no connection ca_1403 on this account", C::Unknown),
            ("request 4011 failed", C::Unknown),
            ("trace 2401x aborted", C::Unknown),
            // Credit and throttling.
            ("429 Too Many Requests", C::Quota),
            ("rate limit exceeded", C::Quota),
            ("your account is out of credit", C::Quota),
            ("monthly quota reached", C::Quota),
            // Address / reachability.
            ("404 Not Found", C::Endpoint),
            ("error sending request: dns error", C::Endpoint),
            ("tcp connect error: connection refused", C::Endpoint),
            ("could not resolve backend.composio.dev", C::Endpoint),
            // Slow.
            ("operation timed out", C::Timeout),
            ("request timeout after 10s", C::Timeout),
            ("deadline exceeded", C::Timeout),
            // Everything else, including the no-client build's own reason.
            ("Composio is not compiled into this build", C::Unknown),
            ("", C::Unknown),
            ("something went sideways", C::Unknown),
        ];
        for (text, want) in cases {
            assert_eq!(
                classify(text),
                *want,
                "classify({text:?}) must be {want:?}, not {:?}",
                classify(text)
            );
        }
    }

    /// The whole reason the classifier is not a boolean: exactly one class may
    /// take a credential away.
    #[test]
    fn only_auth_is_destructive() {
        assert!(C::Auth.is_destructive());
        for class in [C::Endpoint, C::Quota, C::Timeout, C::Unknown] {
            assert!(
                !class.is_destructive(),
                "{class} must keep the key — only a rejected credential may be discarded"
            );
        }
    }

    /// `describe` is copy, and copy only. In particular the `unknown` sentence
    /// must not be a place an upstream error string could be interpolated — it
    /// lands in a banner an operator screenshots, and upstream text can echo
    /// request headers or a key fragment.
    #[test]
    fn describe_is_a_fixed_sentence_per_class_and_quotes_nothing() {
        let mut seen: Vec<&str> = Vec::new();
        for class in [C::Auth, C::Endpoint, C::Quota, C::Timeout, C::Unknown] {
            let copy = describe(class);
            assert!(!copy.is_empty(), "{class} has no copy");
            assert!(
                !seen.contains(&copy),
                "{class} reuses another class's sentence: {copy}"
            );
            seen.push(copy);
        }
        // The destructive class says nothing was saved; every advisory class
        // says the opposite, because the write did land.
        assert!(describe(C::Auth).contains("nothing was changed"));
        for class in [C::Endpoint, C::Quota, C::Timeout, C::Unknown] {
            assert!(
                describe(class).starts_with("Saved"),
                "{class} is an advisory on a completed write: {}",
                describe(class)
            );
        }
    }

    /// The check-only route's copy is the same facts with the write claim
    /// removed. A `Saved, …` sentence on a route that stores nothing is the
    /// exact misreport this second table exists to prevent.
    #[test]
    fn the_verdict_copy_never_claims_anything_was_saved() {
        let mut seen: Vec<&str> = Vec::new();
        for class in [C::Auth, C::Endpoint, C::Quota, C::Timeout, C::Unknown] {
            let copy = describe_verdict(class);
            assert!(!copy.is_empty(), "{class} has no verdict copy");
            assert!(
                !copy.contains("Saved"),
                "a check writes nothing, so its copy must not say it saved: {copy}"
            );
            assert!(
                !seen.contains(&copy),
                "{class} reuses another class's verdict: {copy}"
            );
            assert_ne!(
                copy,
                describe(class),
                "{class}: the two tables answer different questions and must not silently \
                 converge"
            );
            seen.push(copy);
        }
    }

    /// The boundary rule on its own, since it is what stops an opaque Composio
    /// id from being read as an HTTP status.
    #[test]
    fn status_codes_match_on_word_boundaries_only() {
        assert!(has_code("composio answered 401 unauthorized", "401"));
        assert!(has_code("401", "401"));
        assert!(has_code("http/1.1 403 forbidden", "403"));
        assert!(!has_code("ca_1403", "403"));
        assert!(!has_code("4011", "401"));
        assert!(!has_code("x401", "401"));
        // A second occurrence that *is* a word still counts.
        assert!(has_code("ca_1403 then 403 forbidden", "403"));
    }

    /// The wire spelling is a contract the console keys on.
    #[test]
    fn the_wire_spelling_is_lowercase_and_stable() {
        for (class, wire) in [
            (C::Auth, "auth"),
            (C::Endpoint, "endpoint"),
            (C::Quota, "quota"),
            (C::Timeout, "timeout"),
            (C::Unknown, "unknown"),
        ] {
            assert_eq!(class.as_str(), wire);
            assert_eq!(serde_json::to_value(class).unwrap(), wire);
            assert_eq!(class.to_string(), wire);
        }
    }
}
