//! [`HttpOpenHumanRpc`]: the real HTTP transport to openhuman-core.
//!
//! Compiled only under the `openhuman-rpc` feature — the trait, envelopes, and
//! [`MockOpenHumanRpc`](super::rpc::MockOpenHumanRpc) keep the default build
//! network-free while every provider/adapter stays testable offline.
//!
//! Attach to a running openhuman-core with [`HttpOpenHumanRpc::attach`], passing
//! the base URL (from `OPENCOMPANY_OPENHUMAN_URL`) and the per-launch bearer.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::openhuman::rpc::{OpenHumanRpc, RpcRequest, RpcResponse};
use crate::ports::types::SecretValue;

/// A JSON-RPC client that talks to openhuman-core over HTTP.
pub struct HttpOpenHumanRpc {
    base_url: String,
    bearer: SecretValue,
    http: reqwest::Client,
    id: AtomicU64,
}

impl HttpOpenHumanRpc {
    /// Attaches to an already-running openhuman-core at `base_url`, using
    /// `bearer` as the per-launch `Authorization: Bearer` credential.
    pub fn attach(base_url: impl Into<String>, bearer: SecretValue) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bearer,
            http: reqwest::Client::new(),
            id: AtomicU64::new(1),
        }
    }

    fn rpc_url(&self) -> String {
        format!("{}/rpc", self.base_url)
    }
}

/// Wraps a transport error into the crate error type.
fn transport(err: impl std::fmt::Display) -> OpenCompanyError {
    OpenCompanyError::OpenHuman {
        code: -32000,
        message: err.to_string(),
    }
}

#[async_trait]
impl OpenHumanRpc for HttpOpenHumanRpc {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: self.id.fetch_add(1, Ordering::Relaxed),
            method,
            params,
        };
        let response = self
            .http
            .post(self.rpc_url())
            .bearer_auth(self.bearer.expose())
            .json(&request)
            .send()
            .await
            .map_err(transport)?;
        let envelope: RpcResponse = response.json().await.map_err(transport)?;
        if let Some(err) = envelope.error {
            return Err(OpenCompanyError::OpenHuman {
                code: err.code,
                message: err.message,
            });
        }
        Ok(envelope.result)
    }

    async fn health(&self) -> Result<bool> {
        // Any transport error degrades to "unhealthy" so boot never fails on it.
        match self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
        {
            Ok(response) => Ok(response.status().is_success()),
            Err(_) => Ok(false),
        }
    }
}
