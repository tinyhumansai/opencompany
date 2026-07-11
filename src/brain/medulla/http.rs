//! [`HttpSocketTransport`]: the networked [`MedullaTransport`], gated behind the
//! optional `medulla` feature.
//!
//! The two `POST` endpoints (`/orchestration/v1/events`, `/world-diff`) are
//! implemented over [`reqwest`] with a protocol-1 envelope and `Bearer`
//! credential; error envelopes decode to
//! [`OpenCompanyError::Orchestration`](crate::error::OpenCompanyError::Orchestration)
//! preserving the `ORCH_*` code. Every request body is scanned for a `model`
//! field before it is sent.
//!
//! The Socket.IO half — the effect/tool-call frame stream and the acks/answers
//! the client emits — is **stubbed** in this phase: [`Self::cycle_frames`]
//! reports an empty cycle and the emit-side methods are no-ops, so a networked
//! build is a compilable scaffold rather than a live client. All behavioral
//! coverage runs against
//! [`MockTransport`](super::mock::MockTransport) in the default build. Wiring a
//! real Socket.IO client is deferred; the [`MedullaTransport`] seam isolates the
//! choice so only this file changes.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Result;
use crate::ports::types::SecretValue;

use super::transport::{InboundFrame, MedullaTransport};
use super::wire::{
    self, ApiResponse, EffectResult, Envelope, EventsAccepted, EventsRequest, OrchErrorCode,
    ToolManifestEntry, ToolResultFrame, WorldDiffAccepted, WorldDiffRequest,
};

/// The networked transport speaking `/orchestration/v1` over HTTP + Socket.IO.
pub struct HttpSocketTransport {
    client: reqwest::Client,
    base_url: String,
    credential: SecretValue,
}

impl HttpSocketTransport {
    /// Builds a transport against `base_url` (e.g. `https://api.tinyhumans.ai`)
    /// authenticating with `credential`.
    pub fn new(base_url: impl Into<String>, credential: SecretValue) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            credential,
        }
    }

    /// Builds a full endpoint URL under `/orchestration/v1`.
    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/orchestration/v1/{}",
            self.base_url.trim_end_matches('/'),
            path
        )
    }

    /// Posts a protocol-1 envelope and decodes the `ApiResponse<T>` reply.
    async fn post_json<T: DeserializeOwned>(&self, path: &str, envelope: Value) -> Result<T> {
        // Belt-and-suspenders: never send a body carrying a `model` field.
        wire::assert_no_model(&envelope)?;

        let response = self
            .client
            .post(self.endpoint(path))
            .bearer_auth(self.credential.expose())
            .json(&envelope)
            .send()
            .await
            .map_err(|err| {
                OrchErrorCode::DeviceOffline.to_error(format!("POST {path} failed: {err}"))
            })?;

        let api: ApiResponse<T> = response.json().await.map_err(|err| {
            OrchErrorCode::ValidationError.to_error(format!("decode {path} response failed: {err}"))
        })?;
        api.into_result()
    }
}

impl std::fmt::Debug for HttpSocketTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSocketTransport")
            .field("base_url", &self.base_url)
            .field("credential", &"<redacted>")
            .finish()
    }
}

#[async_trait]
impl MedullaTransport for HttpSocketTransport {
    async fn post_events(&self, req: EventsRequest) -> Result<EventsAccepted> {
        let body = serde_json::to_value(Envelope::v1(req))?;
        self.post_json("events", body).await
    }

    async fn post_world_diff(&self, req: WorldDiffRequest) -> Result<WorldDiffAccepted> {
        req.validate()?;
        let body = serde_json::to_value(Envelope::v1(req))?;
        self.post_json("world-diff", body).await
    }

    async fn register_tools(&self, _tools: Vec<ToolManifestEntry>) -> Result<()> {
        // TODO(medulla): emit `orch:register_tools` over the Socket.IO client.
        Ok(())
    }

    fn cycle_frames(&self, _cycle_id: &str) -> BoxStream<'static, Result<InboundFrame>> {
        // TODO(medulla): bridge the `orch:effect:*` / `orch:tool_call` stream.
        // Until the socket client is wired, report an empty cycle so a networked
        // build is a compilable scaffold rather than a hang.
        stream::once(async { Ok(InboundFrame::CycleComplete) }).boxed()
    }

    async fn ack_effect(&self, _ack: EffectResult) -> Result<()> {
        // TODO(medulla): emit `orch:effect:result` over the Socket.IO client.
        Ok(())
    }

    async fn answer_tool_call(&self, _ans: ToolResultFrame) -> Result<()> {
        // TODO(medulla): emit `orch:tool_result` over the Socket.IO client.
        Ok(())
    }
}
