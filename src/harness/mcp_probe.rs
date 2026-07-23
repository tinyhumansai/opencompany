//! MCP probing, error classification, and credential scrubbing (the
//! error-hardening cell).
//!
//! This is the one place that turns a raw MCP transport failure into an
//! operator-facing [`McpHealth`] the console can render, and it is the security
//! choke point for the "a credential must NEVER appear in any error/health/
//! response/agent-output" invariant.
//!
//! Three leak vectors are closed by [`scrub`]:
//!
//! 1. Upstream's `MCP HTTP {status} — {text}` embeds the raw response *body*.
//! 2. A `reqwest::Error`'s `Display` embeds the **full request URL including the
//!    query string** — lethal with [`AuthMaterial::QueryParam`], where the
//!    credential lives in the URL.
//!
//! [`AuthMaterial::QueryParam`]: crate::company::mcp::AuthMaterial::QueryParam
//! 3. Agent-visible endpoints could echo a URL query.
//!
//! [`scrub`] therefore (a) replaces every known credential substring with
//! `•••`, (b) strips the query string off any embedded URL, and (c)
//! UTF-8-safely truncates — and it is applied at **every** surfacing seam.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::sync::{Arc, Mutex};

use openhuman_core::openhuman as oh;

use crate::company::mcp::{McpHealth, McpServerDecl, McpStatus};
use crate::harness::mcp::registry_from_decls;
use crate::ports::now_millis;

/// The maximum byte length of any scrubbed, surfaced message.
pub const SCRUB_MAX_BYTES: usize = 300;

/// The shape of an MCP failure, driving both the status tier and the operator
/// message. Derived by [`classify_mcp_error`] from the typed error (downcast)
/// and the upstream bail-string arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureKind {
    /// 401 with no/unknown credential — the operator hasn't supplied auth yet.
    CredentialRequired,
    /// The server advertises an OAuth challenge (browser sign-in), unsupported.
    OauthRequired,
    /// A credential WAS sent but the server refused it (wrong/expired, or 403).
    TokenRejected,
    /// The request did not complete within the timeout.
    Timeout,
    /// The host was unreachable (DNS failure, connection refused, no route).
    Unreachable,
    /// The TLS handshake failed (bad/self-signed cert, protocol mismatch).
    Tls,
    /// Reachable, but not an MCP endpoint (404 wrong path, HTML/JSON that
    /// doesn't parse as an MCP reply).
    NotMcp,
    /// The server returned a 5xx.
    ServerError,
    /// A tool call was rejected by the server (JSON-RPC error in call context).
    ToolCallRejected,
    /// Anything not otherwise recognised.
    Unknown,
}

/// The classification of one MCP failure: the coarse [`McpStatus`] tier, the
/// stable auth-hint code (when it's a credential problem), and the internal
/// [`FailureKind`] that selects the operator message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeClass {
    /// The coarse badge tier.
    pub status: McpStatus,
    /// A stable auth-failure reason code, when applicable.
    pub auth_hint: Option<String>,
    kind: FailureKind,
}

impl FailureKind {
    /// A stable machine code for this failure, used as the [`McpFailure`] and
    /// [`CompanyEvent::McpCallFailed`](crate::ports::types::CompanyEvent) status
    /// string.
    fn code(self) -> &'static str {
        match self {
            FailureKind::CredentialRequired => "credential_required",
            FailureKind::OauthRequired => "oauth_required",
            FailureKind::TokenRejected => "token_rejected",
            FailureKind::Timeout => "timeout",
            FailureKind::Unreachable => "unreachable",
            FailureKind::Tls => "tls",
            FailureKind::NotMcp => "not_mcp",
            FailureKind::ServerError => "server_error",
            FailureKind::ToolCallRejected => "tool_call_rejected",
            FailureKind::Unknown => "error",
        }
    }

    /// The badge tier this failure maps to. Auth problems are `NeedsConfig` (a
    /// valid resting state the operator can fix), everything else is `Error`.
    fn status(self) -> McpStatus {
        match self {
            FailureKind::CredentialRequired
            | FailureKind::OauthRequired
            | FailureKind::TokenRejected => McpStatus::NeedsConfig,
            _ => McpStatus::Error,
        }
    }

    /// The stable auth-hint wire code, when this is a credential problem.
    fn auth_hint(self) -> Option<String> {
        match self {
            FailureKind::CredentialRequired => Some("credential_required".to_string()),
            FailureKind::OauthRequired => Some("oauth_required".to_string()),
            FailureKind::TokenRejected => Some("token_rejected".to_string()),
            _ => None,
        }
    }
}

impl ProbeClass {
    /// The stable machine code for this classification (mirrors the internal
    /// [`FailureKind`]) — the status string carried on an [`McpFailure`] and the
    /// `McpCallFailed` event.
    pub fn code(&self) -> String {
        self.kind.code().to_string()
    }
}

/// One MCP tool-call failure observed during an agent turn, pushed onto the
/// [`McpFailureQueue`] by [`crate::harness::mcp::OcMcpCallTool`] and drained by
/// the [`HarnessBrain`](crate::harness::HarnessBrain) after the turn. Every
/// string field is already scrubbed at construction — this is safe to persist,
/// return, or show an operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpFailure {
    /// The MCP server the failing call targeted.
    pub server: String,
    /// The remote tool the agent tried to call.
    pub tool: String,
    /// A stable status code (from [`ProbeClass::code`]).
    pub status: String,
    /// The auth-failure reason code, when the failure was a credential problem.
    pub hint: Option<String>,
    /// A short, scrubbed, operator-facing message.
    pub scrubbed_message: String,
}

/// A shared, in-memory queue of MCP tool-call failures — the exact
/// [`DelegationQueue`](crate::harness::orchestrator::DelegationQueue) pattern.
/// Cheap to [`Clone`] (a shared handle); the tool built into the agent and the
/// brain that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
#[derive(Clone, Default)]
pub struct McpFailureQueue {
    inner: Arc<Mutex<Vec<McpFailure>>>,
}

impl McpFailureQueue {
    /// Records a failure.
    pub fn push(&self, failure: McpFailure) {
        self.inner.lock().expect("mcp failure queue").push(failure);
    }

    /// Empties the queue (called before an orchestrator turn so a prior turn's
    /// failures never leak into this one — mirrors `DelegationQueue::clear`).
    pub fn clear(&self) {
        self.inner.lock().expect("mcp failure queue").clear();
    }

    /// Drains every queued failure (FIFO), emptying the queue.
    pub fn drain(&self) -> Vec<McpFailure> {
        let mut guard = self.inner.lock().expect("mcp failure queue");
        std::mem::take(&mut *guard)
    }

    /// The number of queued failures (test/observability).
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("mcp failure queue").len()
    }
}

/// Classify a raw MCP transport error into a [`ProbeClass`].
///
/// `auth_configured` is whether the server has a stored credential — it
/// disambiguates a bare 401 (credential required) from a rejected one (token
/// rejected). `in_call_context` is true when the error came from a *tool call*
/// (via [`crate::harness::mcp`]'s `OcMcpCallTool`) rather than a connect/probe;
/// only then is a JSON-RPC `MCP error:` treated as a tool-call rejection.
///
/// Order matters: the **typed** downcasts (401, reqwest) win over string arms so
/// classification survives `?`/`.context()` wrapping and never depends on
/// fragile prose for the security-relevant cases.
pub fn classify_mcp_error(
    err: &anyhow::Error,
    auth_configured: bool,
    in_call_context: bool,
) -> ProbeClass {
    let kind = classify_kind(err, auth_configured, in_call_context);
    ProbeClass {
        status: kind.status(),
        auth_hint: kind.auth_hint(),
        kind,
    }
}

fn classify_kind(err: &anyhow::Error, auth_configured: bool, in_call_context: bool) -> FailureKind {
    // 1. Typed 401 — the security-critical case. Read the typed
    //    `resource_metadata` (not the message) to decide OAuth vs static auth.
    if let Some(unauthorized) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<oh::mcp_client::McpUnauthorizedError>())
    {
        return if unauthorized.resource_metadata.is_some() {
            FailureKind::OauthRequired
        } else if auth_configured {
            FailureKind::TokenRejected
        } else {
            FailureKind::CredentialRequired
        };
    }

    // 2. Typed transport error — timeout / connect / TLS. reqwest's `Display`
    //    leaks the full URL, but we only read its typed predicates here; the
    //    message is scrubbed before it ever surfaces.
    if let Some(req) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
    {
        if req.is_timeout() {
            return FailureKind::Timeout;
        }
        if looks_like_tls(err) {
            return FailureKind::Tls;
        }
        if req.is_connect() {
            return FailureKind::Unreachable;
        }
        return FailureKind::Unreachable;
    }

    // 3. String arms over the upstream bail! messages (whole chain).
    let full = format!("{err:#}");
    if let Some(code) = http_status_in(&full) {
        return match code {
            401 => {
                if auth_configured {
                    FailureKind::TokenRejected
                } else {
                    FailureKind::CredentialRequired
                }
            }
            403 => FailureKind::TokenRejected,
            404 => FailureKind::NotMcp,
            500..=599 => FailureKind::ServerError,
            _ => FailureKind::Unknown,
        };
    }
    if full.contains("Failed to parse MCP JSON response") {
        return FailureKind::NotMcp;
    }
    if in_call_context && full.contains("MCP error:") {
        return FailureKind::ToolCallRejected;
    }
    if looks_like_tls(err) {
        return FailureKind::Tls;
    }
    FailureKind::Unknown
}

/// The HTTP status code embedded in an upstream `MCP HTTP {status} — …` message,
/// if present.
fn http_status_in(text: &str) -> Option<u16> {
    let after = text.split("MCP HTTP ").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Whether the error chain smells like a TLS/certificate failure.
fn looks_like_tls(err: &anyhow::Error) -> bool {
    let full = format!("{err:#}").to_ascii_lowercase();
    [
        "tls",
        "certificate",
        "handshake",
        "ssl",
        "self-signed",
        "self signed",
    ]
    .iter()
    .any(|needle| full.contains(needle))
}

/// A short, actionable, operator-facing message for a classified failure. The
/// caller MUST pass the result through [`scrub`] before persisting or surfacing
/// it — `err` may embed a body or URL.
pub fn operator_message(server: &str, class: &ProbeClass, err: &anyhow::Error) -> String {
    match class.kind {
        FailureKind::CredentialRequired => format!(
            "MCP server '{server}' needs a credential. Add its API token (or query-parameter key) in Connections, then Test again."
        ),
        FailureKind::OauthRequired => format!(
            "MCP server '{server}' uses OAuth sign-in, which isn't supported yet — a pasted token won't be accepted."
        ),
        FailureKind::TokenRejected => format!(
            "MCP server '{server}' rejected the credential — it's wrong or expired. Update it and Test again."
        ),
        FailureKind::Timeout => format!(
            "MCP server '{server}' didn't respond in time. Check the endpoint is reachable, or raise its timeout."
        ),
        FailureKind::Unreachable => format!(
            "Couldn't reach MCP server '{server}'. Check the endpoint URL is correct and the host is online."
        ),
        FailureKind::Tls => format!(
            "MCP server '{server}' has a TLS/certificate problem — the secure connection couldn't be established."
        ),
        FailureKind::NotMcp => format!(
            "MCP server '{server}' didn't respond as an MCP endpoint. Check the URL points at the server's MCP path."
        ),
        FailureKind::ServerError => format!(
            "MCP server '{server}' returned a server error. It's likely down — try again later."
        ),
        FailureKind::ToolCallRejected => format!(
            "MCP server '{server}' rejected the call: {err}. It may need different arguments or credentials — tell the operator, don't retry blindly."
        ),
        FailureKind::Unknown => format!("MCP server '{server}' couldn't be used: {err}."),
    }
}

/// Probe a single server end-to-end: build a one-server registry (auth
/// **included** — unlike upstream's no-auth test probe), list its tools
/// (inheriting the injection-safety filter), and classify the outcome into a
/// **scrubbed** [`McpHealth`].
///
/// A disabled decl is probed as if enabled (the caller decides whether to probe;
/// probing must reflect the configured auth regardless of the exposed flag).
pub async fn probe_server(decl: &McpServerDecl) -> McpHealth {
    let secrets = decl.auth.secret_values();
    let auth_configured = decl.auth.is_configured();

    let mut probe = decl.clone();
    probe.enabled = true; // the probe reflects config, not the exposed flag
    let registry = registry_from_decls(std::slice::from_ref(&probe));

    match registry.list_tools(&decl.name).await {
        Ok(tools) => {
            let count = tools.len() as u32;
            McpHealth {
                status: McpStatus::Ok,
                message: format!(
                    "{count} tool{} available.",
                    if count == 1 { "" } else { "s" }
                ),
                tool_count: count,
                checked_at_millis: now_millis(),
                auth_hint: None,
            }
        }
        Err(err) => {
            let class = classify_mcp_error(&err, auth_configured, false);
            let message = scrub(&operator_message(&decl.name, &class, &err), &secrets);
            McpHealth {
                status: class.status,
                message,
                tool_count: 0,
                checked_at_millis: now_millis(),
                auth_hint: class.auth_hint,
            }
        }
    }
}

/// Scrub a message so it can be safely persisted, returned, or shown to an agent.
///
/// Three passes, in order:
/// 1. Replace every known credential substring (from `secrets`) with `•••`.
/// 2. Strip the query string (and fragment) off **every** embedded URL — this is
///    what kills the `reqwest` full-URL leak when the credential rides in a
///    query parameter.
/// 3. UTF-8-safely truncate to [`SCRUB_MAX_BYTES`].
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret.as_str(), "•••");
        }
    }
    out = strip_url_queries(&out);
    utf8_truncate(&out, SCRUB_MAX_BYTES)
}

/// Remove the entire endpoint credential surface from an agent-visible endpoint
/// string: strip its query + fragment. Kept separate so [`crate::harness::mcp`]'s
/// list-servers tool can sanitize endpoints without the full [`scrub`] pipeline.
pub fn strip_endpoint(endpoint: &str) -> String {
    let cut = endpoint.find(['?', '#']).unwrap_or(endpoint.len());
    endpoint[..cut].to_string()
}

/// Cut the query/fragment off any `http(s)://…` URL embedded anywhere in `text`.
fn strip_url_queries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = find_url_start(rest) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // The URL token runs until the next whitespace.
        let url_end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let url = &tail[..url_end];
        let cut = url.find(['?', '#']).unwrap_or(url.len());
        out.push_str(&url[..cut]);
        rest = &tail[url_end..];
    }
    out.push_str(rest);
    out
}

/// The byte offset of the earliest `http://` or `https://` in `s`.
fn find_url_start(s: &str) -> Option<usize> {
    match (s.find("http://"), s.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Truncate `s` to at most `max_bytes` on a char boundary, appending `…` when it
/// was cut. Never panics mid-codepoint.
fn utf8_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anyhow_str(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{}", msg.to_string())
    }

    // ---- scrub -------------------------------------------------------------

    #[test]
    fn scrub_replaces_known_secret() {
        let out = scrub(
            "token was sk-canary-123 here",
            &["sk-canary-123".to_string()],
        );
        assert!(!out.contains("sk-canary-123"), "{out}");
        assert!(out.contains("•••"), "{out}");
    }

    #[test]
    fn scrub_strips_url_query_string() {
        // The lethal case: a reqwest error with the credential in the URL query.
        let msg = "error sending request for url (https://api.browserbase.com/mcp?projectId=pid&apiKey=qp-canary) failed";
        let out = scrub(msg, &[]);
        assert!(
            !out.contains("qp-canary"),
            "query-carried secret leaked: {out}"
        );
        assert!(!out.contains("projectId"), "{out}");
        assert!(out.contains("https://api.browserbase.com/mcp"), "{out}");
    }

    #[test]
    fn scrub_strips_query_and_replaces_secret_together() {
        let msg = "url https://host/mcp?apiKey=qp-canary and bearer sk-canary";
        let out = scrub(msg, &["qp-canary".to_string(), "sk-canary".to_string()]);
        assert!(!out.contains("qp-canary"), "{out}");
        assert!(!out.contains("sk-canary"), "{out}");
    }

    #[test]
    fn scrub_truncates_utf8_safely() {
        let long = "é".repeat(400); // 800 bytes
        let out = scrub(&long, &[]);
        assert!(out.len() <= SCRUB_MAX_BYTES + "…".len());
        assert!(out.ends_with('…'));
    }

    #[test]
    fn strip_endpoint_drops_query() {
        assert_eq!(
            strip_endpoint("https://host/mcp?apiKey=secret&projectId=pid"),
            "https://host/mcp"
        );
        assert_eq!(strip_endpoint("https://host/mcp"), "https://host/mcp");
    }

    // ---- classify (string arms — no live dial) -----------------------------

    #[test]
    fn classify_403_is_token_rejected() {
        let class = classify_mcp_error(&anyhow_str("MCP HTTP 403 — Forbidden"), true, false);
        assert_eq!(class.status, McpStatus::NeedsConfig);
        assert_eq!(class.auth_hint.as_deref(), Some("token_rejected"));
    }

    #[test]
    fn classify_404_is_not_mcp() {
        let class = classify_mcp_error(&anyhow_str("MCP HTTP 404 — Not Found"), false, false);
        assert_eq!(class.status, McpStatus::Error);
        assert_eq!(class.auth_hint, None);
    }

    #[test]
    fn classify_5xx_is_server_error() {
        let class = classify_mcp_error(
            &anyhow_str("MCP HTTP 503 — Service Unavailable"),
            false,
            false,
        );
        assert_eq!(class.status, McpStatus::Error);
    }

    #[test]
    fn classify_bare_401_string_needs_credential_when_none() {
        let class = classify_mcp_error(&anyhow_str("MCP HTTP 401 — Unauthorized"), false, false);
        assert_eq!(class.auth_hint.as_deref(), Some("credential_required"));
        let class = classify_mcp_error(&anyhow_str("MCP HTTP 401 — Unauthorized"), true, false);
        assert_eq!(class.auth_hint.as_deref(), Some("token_rejected"));
    }

    #[test]
    fn classify_parse_failure_is_not_mcp() {
        let class = classify_mcp_error(
            &anyhow_str("Failed to parse MCP JSON response: expected value — body: <html>"),
            false,
            false,
        );
        assert_eq!(class.status, McpStatus::Error);
        assert_eq!(class.auth_hint, None);
    }

    #[test]
    fn classify_json_rpc_error_only_in_call_context() {
        let msg = "MCP error: {\"code\":-32000,\"message\":\"bad args\"}";
        assert_eq!(
            classify_mcp_error(&anyhow_str(msg), false, true).status,
            McpStatus::Error
        );
        // Both are Error tier, but the message routing differs — assert the kind.
        assert_eq!(
            classify_mcp_error(&anyhow_str(msg), false, true).kind,
            FailureKind::ToolCallRejected
        );
        assert_eq!(
            classify_mcp_error(&anyhow_str(msg), false, false).kind,
            FailureKind::Unknown
        );
    }

    #[test]
    fn classify_typed_401_dominates_string() {
        // A typed McpUnauthorizedError with OAuth metadata → oauth_required,
        // even though the message string would otherwise be generic.
        let err = anyhow::Error::new(oh::mcp_client::McpUnauthorizedError {
            endpoint: "host".into(),
            resource_metadata: Some("https://host/.well-known/oauth".into()),
        });
        let class = classify_mcp_error(&err, false, false);
        assert_eq!(class.auth_hint.as_deref(), Some("oauth_required"));
        assert_eq!(class.status, McpStatus::NeedsConfig);
    }

    #[test]
    fn classify_typed_401_without_metadata_respects_credential_state() {
        let err = anyhow::Error::new(oh::mcp_client::McpUnauthorizedError {
            endpoint: "host".into(),
            resource_metadata: None,
        });
        assert_eq!(
            classify_mcp_error(&err, false, false).auth_hint.as_deref(),
            Some("credential_required")
        );
        assert_eq!(
            classify_mcp_error(&err, true, false).auth_hint.as_deref(),
            Some("token_rejected")
        );
    }

    #[test]
    fn operator_message_is_actionable_and_scrubbed() {
        let class = classify_mcp_error(&anyhow_str("MCP HTTP 401 — Unauthorized"), false, false);
        let msg = scrub(
            &operator_message("browserbase", &class, &anyhow_str("x")),
            &[],
        );
        assert!(msg.contains("browserbase"), "{msg}");
        assert!(msg.to_lowercase().contains("credential"), "{msg}");
    }
}
