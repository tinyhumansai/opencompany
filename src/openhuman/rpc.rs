//! The [`OpenHumanRpc`] transport seam: a JSON-RPC channel to `openhuman-core`.
//!
//! openhuman-core exposes a single JSON-RPC endpoint at `POST /rpc` plus a few
//! REST probes (`GET /health`, `/schema`, `/events`). This module defines the
//! transport *trait* and its wire envelopes so the [`OpenHumanToolProvider`]
//! and [`OpenHumanChannelAdapter`] can be exercised entirely offline against
//! [`MockOpenHumanRpc`] in the default build; the real HTTP client lives behind
//! the `openhuman-rpc` feature in [`super::http_client`].
//!
//! [`OpenHumanToolProvider`]: super::tools::OpenHumanToolProvider
//! [`OpenHumanChannelAdapter`]: super::channel::OpenHumanChannelAdapter

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

/// A JSON-RPC transport to `openhuman-core`.
///
/// Object-safe so it can be held as `Arc<dyn OpenHumanRpc>` and shared between
/// the tool provider and channel adapters of one company.
#[async_trait]
pub trait OpenHumanRpc: Send + Sync {
    /// Invokes a JSON-RPC method (`POST /rpc`), returning its `result` value.
    ///
    /// `method` is the fully-qualified wire name, e.g. `openhuman.tools_invoke`;
    /// build it with [`rpc_method`]. A protocol- or transport-level failure is
    /// surfaced as [`OpenCompanyError::OpenHuman`](crate::OpenCompanyError::OpenHuman).
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value>;

    /// Liveness probe (`GET /health`).
    ///
    /// Returns `Ok(false)` — never `Err` — when openhuman-core is unreachable,
    /// so a boot-time degradation decision can distinguish "down" from a genuine
    /// caller error.
    async fn health(&self) -> Result<bool>;
}

/// Formats the JSON-RPC wire method name for an openhuman namespace/function.
///
/// openhuman-core names methods `openhuman.<namespace>_<function>`, e.g.
/// `rpc_method("tools", "invoke") == "openhuman.tools_invoke"`.
pub fn rpc_method(namespace: &str, function: &str) -> String {
    format!("openhuman.{namespace}_{function}")
}

/// A JSON-RPC 2.0 request envelope.
///
/// Only constructed by the feature-gated HTTP client; the default build carries
/// it as a shared wire type.
#[cfg_attr(not(feature = "openhuman-rpc"), allow(dead_code))]
#[derive(Debug, Serialize)]
pub(crate) struct RpcRequest<'a> {
    /// Protocol marker, always `"2.0"`.
    pub jsonrpc: &'a str,
    /// A per-connection monotonic request id.
    pub id: u64,
    /// The fully-qualified method name.
    pub method: &'a str,
    /// The method parameters.
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response envelope.
///
/// Decoded by the feature-gated HTTP client (and the offline envelope test).
#[cfg_attr(not(feature = "openhuman-rpc"), allow(dead_code))]
#[derive(Debug, Deserialize)]
pub(crate) struct RpcResponse {
    /// The successful result, if any.
    #[serde(default)]
    pub result: serde_json::Value,
    /// The error, if the call failed.
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Deserialize)]
pub struct RpcError {
    /// The protocol error code.
    pub code: i64,
    /// A human-readable error message.
    pub message: String,
}

/// An in-memory [`OpenHumanRpc`] for offline tests.
///
/// Register canned results per method with [`with_result`](Self::with_result);
/// an unregistered method resolves to an
/// [`OpenCompanyError::OpenHuman`](crate::OpenCompanyError::OpenHuman) error,
/// which lets tests exercise the degradation/fallback paths. Every `call` is
/// recorded so a test can assert both the parameters and the *count* (e.g. that
/// an ungranted invocation issues zero RPC calls).
#[derive(Debug, Default)]
pub struct MockOpenHumanRpc {
    handlers: StdMutex<HashMap<String, serde_json::Value>>,
    calls: StdMutex<Vec<(String, serde_json::Value)>>,
    healthy: AtomicBool,
}

impl MockOpenHumanRpc {
    /// Creates a healthy mock with no registered methods.
    pub fn new() -> Self {
        Self {
            handlers: StdMutex::new(HashMap::new()),
            calls: StdMutex::new(Vec::new()),
            healthy: AtomicBool::new(true),
        }
    }

    /// Registers a canned `result` for `method` (the fully-qualified wire name).
    pub fn with_result(self, method: &str, result: serde_json::Value) -> Self {
        self.handlers
            .lock()
            .expect("mock handlers poisoned")
            .insert(method.to_string(), result);
        self
    }

    /// Marks the mock unhealthy so [`health`](OpenHumanRpc::health) returns
    /// `Ok(false)`.
    pub fn unhealthy(self) -> Self {
        self.healthy.store(false, Ordering::SeqCst);
        self
    }

    /// The number of `call`s issued so far.
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("mock calls poisoned").len()
    }

    /// A snapshot of every `(method, params)` pair issued so far.
    pub fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().expect("mock calls poisoned").clone()
    }
}

#[async_trait]
impl OpenHumanRpc for MockOpenHumanRpc {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.calls
            .lock()
            .expect("mock calls poisoned")
            .push((method.to_string(), params));
        match self
            .handlers
            .lock()
            .expect("mock handlers poisoned")
            .get(method)
            .cloned()
        {
            Some(result) => Ok(result),
            None => Err(crate::OpenCompanyError::OpenHuman {
                code: -32601,
                message: format!("mock has no handler for method `{method}`"),
            }),
        }
    }

    async fn health(&self) -> Result<bool> {
        Ok(self.healthy.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn rpc_method_formats_namespace_and_function() {
        assert_eq!(rpc_method("tools", "invoke"), "openhuman.tools_invoke");
        assert_eq!(rpc_method("channels", "send"), "openhuman.channels_send");
    }

    #[test]
    fn response_envelope_defaults_missing_fields() {
        let ok: RpcResponse = serde_json::from_str(r#"{"result": {"ok": true}}"#).unwrap();
        assert!(ok.error.is_none());
        assert_eq!(ok.result["ok"], true);

        let err: RpcResponse =
            serde_json::from_str(r#"{"error": {"code": -1, "message": "boom"}}"#).unwrap();
        assert!(err.result.is_null());
        assert_eq!(err.error.unwrap().message, "boom");
    }

    #[tokio::test]
    async fn mock_returns_registered_result_and_records_calls() {
        let rpc =
            MockOpenHumanRpc::new().with_result("openhuman.tools_list", serde_json::json!([]));
        let out = rpc
            .call("openhuman.tools_list", serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.is_array());
        assert_eq!(rpc.call_count(), 1);
        assert_eq!(rpc.calls()[0].0, "openhuman.tools_list");
    }

    #[tokio::test]
    async fn mock_unknown_method_errors() {
        let rpc = MockOpenHumanRpc::new();
        let err = rpc
            .call("openhuman.tools_list", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::OpenCompanyError::OpenHuman { code, .. } if code == -32601));
    }

    #[tokio::test]
    async fn mock_health_reflects_flag() {
        assert!(MockOpenHumanRpc::new().health().await.unwrap());
        assert!(!MockOpenHumanRpc::new().unhealthy().health().await.unwrap());
    }
}
