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
use oh::tools::{HttpRequestTool, Tool, ToolResult};
use openhuman_core as oh;

/// A tinyflows [`HttpClient`] backed by OpenHuman's SSRF-guarded
/// [`HttpRequestTool`].
pub struct GuardedHttpClient {
    tool: HttpRequestTool,
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
            tool: HttpRequestTool::new(
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
/// Every rule below mirrors one the real guard applies, so anything refused here
/// is refused by a real run too. That direction is the one that matters: a dry
/// run that wrongly refuses blocks a working graph and cannot be told from a
/// real refusal without arming it anyway. Where a rule is ambiguous — a
/// malformed allowlist, an unparseable URL — this returns `None` and checks
/// nothing rather than guessing.
///
/// That invariant held only by inspection until #1075, and inspection had already
/// missed a break: allowlist *entries* were normalized with
/// `trim_end_matches('.')` while the **host** was not, so `https://example.com./x`
/// against `["example.com"]` was refused here and allowed by the real guard —
/// exactly the blocked-working-graph failure the paragraph above rules out.
///
/// `dry_run_refusal_matches_the_real_client` pins the agreement **behaviourally**,
/// by driving the same URLs through [`GuardedHttpClient`], so this stays correct
/// when upstream changes its internals rather than only when someone re-reads
/// them. It is two-directional as of #1075: a one-directional "both refuse"
/// comparison structurally cannot see this copy becoming *too strict*, which is
/// why the trailing-dot break survived it.
pub(super) fn preflight_refusal(request: &Value, allowed_domains: &[String]) -> Option<String> {
    let url = request.get("url")?.as_str()?.trim();

    // The real guard refuses all three before it reads a host.
    if url.is_empty() {
        return Some("URL cannot be empty".to_string());
    }
    if url.chars().any(char::is_whitespace) {
        return Some("URL cannot contain whitespace".to_string());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Some("Only http:// and https:// URLs are allowed".to_string());
    }

    let host = match host_of(url) {
        Ok(host) => host,
        Err(reason) => return Some(reason.to_string()),
    };

    if is_private_or_local_host(&host) {
        return Some(format!("Blocked local/private host: {host}"));
    }

    // An empty list is open-public mode upstream, so there is nothing to refuse.
    if allowed_domains.is_empty() {
        return None;
    }
    // Upstream substitutes a fail-closed sentinel when *every* entry is
    // malformed, which would refuse all traffic. Rather than reproduce that
    // subtlety — the easiest place for a copy to invent a refusal the real run
    // would not make — an allowlist this cannot read cleanly is left unchecked.
    let normalized: Vec<String> = allowed_domains
        .iter()
        .filter_map(|d| normalize(d))
        .collect();
    if normalized.len() != allowed_domains.len() {
        return None;
    }
    let allowed = normalized.iter().any(|domain| {
        domain == "*"
            || host == *domain
            || host
                .strip_suffix(domain.as_str())
                .is_some_and(|prefix| prefix.ends_with('.'))
    });
    (!allowed).then(|| format!("URL not in allowed domains: {host}"))
}

/// The host of an `http`/`https` URL as the real guard's `extract_host` reads
/// it: lowercased, port stripped, **trailing dot stripped**, and rejected —
/// not read past — when the authority carries userinfo or an IPv6 literal.
///
/// Every `Err` is a rule the real guard applies before the allowlist, so the
/// message is its message. This used to read past userinfo and unwrap IPv6
/// brackets (a *missing* refusal) and to leave a trailing dot on the host
/// (a *false* refusal, since [`normalize`] strips one from allowlist entries).
/// See #1075.
fn host_of(url: &str) -> Result<String, &'static str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or("Only http:// and https:// URLs are allowed")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("URL must include a host");
    }
    if authority.contains('@') {
        return Err("URL userinfo is not allowed");
    }
    if authority.starts_with('[') {
        return Err("IPv6 hosts are not supported in http_request");
    }
    let host = authority
        .split(':')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_lowercase();
    if host.is_empty() {
        return Err("URL must include a valid host");
    }
    Ok(host)
}

/// Mirrors the real guard's allowlist entry normalization.
fn normalize(raw: &str) -> Option<String> {
    let mut d = raw.trim().to_lowercase();
    if let Some(stripped) = d.strip_prefix("https://") {
        d = stripped.to_string();
    } else if let Some(stripped) = d.strip_prefix("http://") {
        d = stripped.to_string();
    }
    if let Some((host, _)) = d.split_once('/') {
        d = host.to_string();
    }
    d = d.trim_start_matches('.').trim_end_matches('.').to_string();
    if let Some((host, _)) = d.split_once(':') {
        d = host.to_string();
    }
    (!d.is_empty() && !d.chars().any(char::is_whitespace)).then_some(d)
}

/// Mirrors the real guard's private/local rule.
fn is_private_or_local_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if bare == "localhost"
        || bare.ends_with(".localhost")
        || bare.rsplit('.').next().is_some_and(|tld| tld == "local")
    {
        return true;
    }
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => is_non_global_v4(v4),
        Ok(std::net::IpAddr::V6(v6)) => {
            let segs = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (segs[0] & 0xfe00) == 0xfc00
                || (segs[0] & 0xffc0) == 0xfe80
                || (segs[0] == 0x2001 && segs[1] == 0x0db8)
                || v6.to_ipv4_mapped().is_some_and(is_non_global_v4)
        }
        Err(_) => false,
    }
}

fn is_non_global_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || a >= 240
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51)
        || (a == 203 && b == 0)
        || (a == 198 && (18..=19).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run admitted before the switch was pulled must not keep making
    /// requests. Admission is taken once for the whole run, so the request
    /// itself has to ask.
    ///
    /// Refused before the URL is even resolved: the point is that no packet
    /// leaves, not that a bad one is rejected.
    #[tokio::test]
    async fn a_stopped_company_refuses_a_node_http_request() {
        use tinyflows::caps::HttpClient;
        let gate = Arc::new(crate::policy::gate::ManifestApprovalGate::new(
            crate::company::Policy {
                mode: "full".to_string(),
                always_approve: Vec::new(),
                auto_approve_under_usd: None,
                approval_ttl_hours: None,
            },
        ));
        let client = GuardedHttpClient::new(Arc::new(SecurityPolicy::default()), Vec::new())
            .with_emergency_gate(Some(gate.clone()));

        gate.set_emergency(true);
        let refused = client
            .request(
                json!({ "method": "GET", "url": "https://api.test/x" }),
                None,
            )
            .await
            .expect_err("a stopped company must make no request");
        assert!(
            matches!(refused, EngineError::Capability(ref m) if m.contains("is stopped")),
            "{refused:?}"
        );

        gate.set_emergency(false);
        let allowed = client
            .request(
                json!({ "method": "GET", "url": "https://api.test/x" }),
                None,
            )
            .await;
        assert!(
            !matches!(allowed, Err(EngineError::Capability(ref m)) if m.contains("is stopped")),
            "released, so the stop no longer refuses it: {allowed:?}"
        );
    }

    #[test]
    fn to_tool_args_maps_method_url_headers_and_stringifies_body() {
        let descriptor = json!({
            "method": "POST",
            "url": "https://api.test/x",
            "headers": { "Content-Type": "application/json" },
            "body": { "q": "hi" }
        });
        let args = to_tool_args(&descriptor);
        assert_eq!(args["method"], "POST");
        assert_eq!(args["url"], "https://api.test/x");
        assert_eq!(args["headers"]["Content-Type"], "application/json");
        // A structured body is carried as its JSON string encoding.
        assert_eq!(args["body"], json!("{\"q\":\"hi\"}"));

        // A string body passes through unchanged; a missing body is omitted.
        let str_body = to_tool_args(&json!({ "url": "u", "body": "raw" }));
        assert_eq!(str_body["body"], "raw");
        let no_body = to_tool_args(&json!({ "url": "u" }));
        assert!(no_body.get("body").is_none());
    }

    #[test]
    fn parse_http_output_extracts_status_and_body() {
        let output =
            "Status: 200 OK\nResponse Headers: content-type: json\n\nResponse Body:\n{\"ok\":true}";
        let (status, body) = parse_http_output(output);
        assert_eq!(status, json!(200));
        assert_eq!(body, "{\"ok\":true}");
    }

    #[test]
    fn from_tool_result_maps_success_and_error() {
        let ok = from_tool_result(ToolResult::success(
            "Status: 201 Created\n\nResponse Body:\nhi",
        ))
        .unwrap();
        assert_eq!(ok["status"], 201);
        assert_eq!(ok["body"], "hi");

        let err = from_tool_result(ToolResult::error("URL is not allowed: 127.0.0.1")).unwrap_err();
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("127.0.0.1")),
            "{err:?}"
        );
    }
}
