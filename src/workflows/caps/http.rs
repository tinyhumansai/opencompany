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
use openhuman_core::openhuman as oh;

/// A tinyflows [`HttpClient`] backed by OpenHuman's SSRF-guarded
/// [`HttpRequestTool`].
pub struct GuardedHttpClient {
    tool: HttpRequestTool,
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
        }
    }
}

#[async_trait]
impl HttpClient for GuardedHttpClient {
    /// Issues the request described by `request` (the node's resolved config:
    /// `{ method, url, headers, body }`). `conn` is ignored in P1 — OpenCompany
    /// has no per-account HTTP connection registry yet, so a request acts as the
    /// company itself; threading a real credential is a documented follow-on.
    async fn request(&self, request: Value, _conn: Option<&str>) -> TfResult<Value> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
