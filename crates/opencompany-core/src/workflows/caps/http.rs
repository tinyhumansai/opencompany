//! The workflow [`HttpClient`]: an `http_request` node routes through
//! OpenHuman's [`HttpRequestTool`], so every request — and every redirect — is
//! validated by the upstream `url_guard` SSRF check.
//!
//! The client is constructed with the same exec-security policy and per-company
//! `web_allowed_domains` allowlist the Cell A `web` toolbelt uses: an **empty**
//! allowlist is open-public mode, while private / loopback / link-local /
//! metadata IPs are **always** rejected. This module never touches raw
//! `reqwest`; it is a pure pair of mapping shims around the tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::HttpClient;
use tinyflows::error::{EngineError, Result as TfResult};

use oh::config::HttpRequestConfig;
use oh::security::SecurityPolicy;
use openhuman_core as oh;
use tinytools::{Tool, ToolResult};

/// A tinyflows [`HttpClient`] backed by OpenHuman's SSRF-guarded
/// [`HttpRequestTool`].
pub struct GuardedHttpClient {
    tool: tinytools_std::network::HttpRequestTool,
    /// The emergency stop, consulted per request, for
    /// [`WorkflowToolInvoker`](super::tools::WorkflowToolInvoker)'s reason: a
    /// run admitted before the switch was pulled never re-consults admission.
    emergency: Option<std::sync::Arc<crate::policy::gate::ManifestApprovalGate>>,
}

impl GuardedHttpClient {
    /// Builds the client from the shared exec-security policy and the company's
    /// SSRF allowlist, sourcing the size/timeout limits from OpenHuman's own
    /// config defaults (one source of truth, no `0 → coerced` noise).
    pub fn new(security: Arc<SecurityPolicy>, allowed_domains: Vec<String>) -> Self {
        let defaults = HttpRequestConfig::default();
        Self {
            tool: oh::tools::http_request_tool(
                security,
                allowed_domains,
                defaults.max_response_size,
                defaults.timeout_secs,
            ),
            emergency: None,
        }
    }

    /// Installs the emergency stop [`request`](HttpClient::request) refuses
    /// against. `None` keeps the client dispatching exactly as before, which is
    /// what every construction site with no company to ask wants.
    pub fn with_emergency_gate(
        mut self,
        gate: Option<Arc<crate::policy::gate::ManifestApprovalGate>>,
    ) -> Self {
        self.emergency = gate;
        self
    }
}

#[async_trait]
impl HttpClient for GuardedHttpClient {
    /// Issues the request described by `request` (the node's resolved config:
    /// `{ method, url, headers, body }`). `conn` is ignored in P1 — OpenCompany
    /// has no per-account HTTP connection registry yet, so a request acts as the
    /// company itself; threading a real credential is a documented follow-on.
    async fn request(&self, request: Value, _conn: Option<&str>) -> TfResult<Value> {
        if self
            .emergency
            .as_ref()
            .is_some_and(|gate| gate.is_emergency())
        {
            return Err(EngineError::Capability(
                "http_request refused: the company is stopped and will run no work until an \
                 operator releases it"
                    .to_string(),
            ));
        }
        let args = to_tool_args(&request);
        let result = self
            .tool
            .execute(args)
            .await
            .map_err(|err| EngineError::Capability(format!("http_request failed: {err}")))?;
        from_tool_result(result)
    }
}

/// Maps an `http_request` node descriptor onto [`HttpRequestTool`] args. The
/// tool reads `body` as a string, so a non-string body is JSON-serialized.
fn to_tool_args(descriptor: &Value) -> Value {
    let mut args = serde_json::Map::new();
    if let Some(url) = descriptor.get("url") {
        args.insert("url".to_string(), url.clone());
    }
    if let Some(method) = descriptor.get("method") {
        args.insert("method".to_string(), method.clone());
    }
    if let Some(headers) = descriptor.get("headers") {
        args.insert("headers".to_string(), headers.clone());
    }
    match descriptor.get("body") {
        Some(Value::String(body)) => {
            args.insert("body".to_string(), json!(body));
        }
        Some(Value::Null) | None => {}
        // A structured body → the tool's string body carries its JSON encoding.
        Some(other) => {
            args.insert("body".to_string(), json!(other.to_string()));
        }
    }
    Value::Object(args)
}

/// Maps the tool result onto `{ status, body }`. An error result (SSRF-guard
/// denial, transport failure, or a non-2xx status) becomes an
/// [`EngineError::Capability`] so the `http_request` node fails loudly and its
/// `on_error`/retry policy governs it.
fn from_tool_result(result: ToolResult) -> TfResult<Value> {
    if result.is_error {
        return Err(EngineError::Capability(format!(
            "http_request: {}",
            result.output()
        )));
    }
    let output = result.output();
    let (status, body) = parse_http_output(&output);
    Ok(json!({ "status": status, "body": body }))
}

/// Best-effort parse of [`HttpRequestTool`]'s success text
/// (`"Status: <code> <reason>\nResponse Headers: …\n\nResponse Body:\n<body>"`)
/// into a numeric `status` and the raw response `body`. Falls back to a null
/// status and the whole output as the body when the shape is unrecognized.
fn parse_http_output(output: &str) -> (Value, String) {
    let status = output
        .strip_prefix("Status: ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|token| token.parse::<u64>().ok())
        .map(|code| json!(code))
        .unwrap_or(Value::Null);
    let body = output
        .split_once("\n\nResponse Body:\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_else(|| output.to_string());
    (status, body)
}

// ---------------------------------------------------------------------------
// Pre-flight: the part of the guard's verdict a dry run can reach (issue #1048)
// ---------------------------------------------------------------------------

/// Why a dry run refused a target, or `None` when this check reached no verdict.
///
/// **`None` means "not refused by the decidable subset", never "this would
/// work."** A dry run performs nothing, so it cannot know whether a host is up,
/// a credential is current, or a body parses. Treating `None` as success is the
/// false green issue #1048 is about; the caller says *not checked* instead.
///
/// # What is decided here, and what deliberately is not
///
/// Decided — pure functions of the URL and the company's own config, answerable
/// without performing anything:
///
/// * the shape of the URL — scheme, whitespace, userinfo, an IPv6 literal —
///   each of which the real guard refuses before it reads a host at all;
/// * a **private / loopback / link-local literal** in the URL, which the real
///   guard refuses *regardless of the company's allowlist*;
/// * the company's [`web_allowed_domains`] list, when it is unambiguous.
///
/// **Not** decided — DNS. The real path uses `validate_url_with_dns_check`,
/// which resolves the host to catch a public name pointing at a private address.
/// Resolving is itself a network effect and it fails offline, so a dry run must
/// not attempt it: a name that resolves privately still passes this check and is
/// refused by the real run. That is a *missing* refusal, which is honest, rather
/// than an invented one.
///
/// # Never stricter than the real guard
///
/// This *is* the real guard, minus its DNS step: the same allowlist
/// normalization (`normalize_allowed_domains`, fail-closed sentinel included)
/// and the same URL rules (`validate_url`) that `validate_url_with_dns_check`
/// runs before it resolves. It used to be a hand-maintained copy of those rules,
/// held to agreement by inspection and then by a behavioural test (#1075), and
/// it had already drifted both ways once — a trailing-dot host refused here and
/// allowed there, and an IPv4-compatible IPv6 literal read differently. Calling
/// the rules instead of copying them removes the drift rather than policing it.
///
/// `dry_run_refusal_matches_the_real_client` still drives the same URLs through
/// [`GuardedHttpClient`], so a future change to the real path's order of checks
/// is caught here too.
pub(super) fn preflight_refusal(request: &Value, allowed_domains: &[String]) -> Option<String> {
    let url = request.get("url")?.as_str()?;
    let allowed = tinytools_std::url_guard::normalize_allowed_domains(allowed_domains.to_vec());
    tinytools_std::url_guard::validate_url(url, &allowed)
        .err()
        .map(|refusal| refusal.to_string())
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
