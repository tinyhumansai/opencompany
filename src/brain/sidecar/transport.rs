//! The sidecar transport and inference seams.
//!
//! [`SidecarTransport`] models the local sidecar process the same way
//! [`MedullaTransport`](crate::brain::medulla::transport::MedullaTransport)
//! models the hosted orchestrator: the brain posts events, drains a frame
//! stream, and answers effects/tool-calls. The distinctive addition is
//! [`InferenceClient`], the callback the *host* fulfils so the sidecar can run a
//! model pass without owning credentials — the "inference inversion".
//!
//! Both traits ship with offline mocks (see [`mock`](super::mock)); the real
//! stdio transport and the TinyAgents-backed inference client are documented
//! stubs that compile but return [`Unimplemented`](crate::OpenCompanyError::Unimplemented)
//! until the Node sidecar package and the `tiny` harness are wired.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::Result;

use crate::brain::medulla::wire::{
    EffectResult, EventsAccepted, EventsRequest, ToolManifestEntry, ToolResultFrame,
};

use super::types::{InferenceRequest, InferenceResponse, SidecarFrame};

/// The callback the host fulfils to run one inference pass for the sidecar.
///
/// The sidecar has no model access; when it needs a completion it emits a
/// [`SidecarFrame::Inference`](super::types::SidecarFrame::Inference) and the
/// brain routes it here. A mock returns a canned completion offline; under the
/// `tiny` feature the production client routes to the TinyAgents harness.
#[async_trait]
pub trait InferenceClient: Send + Sync {
    /// Runs one inference pass and returns its completion.
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;
}

/// The transport the sidecar brain speaks to the local sidecar process.
///
/// Mirrors [`MedullaTransport`](crate::brain::medulla::transport::MedullaTransport)
/// for events, effects, and tool calls, and adds
/// [`answer_inference`](Self::answer_inference) for the host-bound callback.
#[async_trait]
pub trait SidecarTransport: Send + Sync {
    /// Ingests one event and returns the wake acknowledgement.
    async fn post_events(&self, req: EventsRequest) -> Result<EventsAccepted>;

    /// Registers the device tool manifest with the sidecar.
    async fn register_tools(&self, tools: Vec<ToolManifestEntry>) -> Result<()>;

    /// The out-of-band frame stream for `cycle_id`.
    ///
    /// Yields effects, tool calls, and inference requests as they arrive,
    /// terminated by [`SidecarFrame::CycleComplete`].
    fn cycle_frames(&self, cycle_id: &str) -> BoxStream<'static, Result<SidecarFrame>>;

    /// Acks an effect after the gate has ruled on it.
    async fn ack_effect(&self, ack: EffectResult) -> Result<()>;

    /// Answers a device tool call.
    async fn answer_tool_call(&self, ans: ToolResultFrame) -> Result<()>;

    /// Answers a host-bound inference request, unblocking the cycle.
    async fn answer_inference(&self, call_id: &str, resp: InferenceResponse) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Real (stubbed) implementations
// ---------------------------------------------------------------------------

/// The stdio/local-HTTP transport to a running Node sidecar process.
///
/// The Rust-side protocol, brain loop, and inference inversion are complete and
/// tested against [`MockSidecarTransport`](super::mock::MockSidecarTransport).
/// The process launch itself waits on the `@tinyhumansai/medulla-v1` Node
/// package: every method returns
/// [`Unimplemented`](crate::OpenCompanyError::Unimplemented) until that package
/// lands, so the feature graph is real without shipping a half-working launcher.
#[derive(Debug, Default)]
pub struct StdioSidecarTransport {
    _private: (),
}

impl StdioSidecarTransport {
    /// Builds a stdio transport. Inert until the Node sidecar package exists.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[async_trait]
impl SidecarTransport for StdioSidecarTransport {
    async fn post_events(&self, _req: EventsRequest) -> Result<EventsAccepted> {
        Err(crate::OpenCompanyError::Unimplemented(
            "stdio sidecar transport",
        ))
    }

    async fn register_tools(&self, _tools: Vec<ToolManifestEntry>) -> Result<()> {
        Err(crate::OpenCompanyError::Unimplemented(
            "stdio sidecar transport",
        ))
    }

    fn cycle_frames(&self, _cycle_id: &str) -> BoxStream<'static, Result<SidecarFrame>> {
        Box::pin(futures::stream::once(async {
            Err(crate::OpenCompanyError::Unimplemented(
                "stdio sidecar transport",
            ))
        }))
    }

    async fn ack_effect(&self, _ack: EffectResult) -> Result<()> {
        Err(crate::OpenCompanyError::Unimplemented(
            "stdio sidecar transport",
        ))
    }

    async fn answer_tool_call(&self, _ans: ToolResultFrame) -> Result<()> {
        Err(crate::OpenCompanyError::Unimplemented(
            "stdio sidecar transport",
        ))
    }

    async fn answer_inference(&self, _call_id: &str, _resp: InferenceResponse) -> Result<()> {
        Err(crate::OpenCompanyError::Unimplemented(
            "stdio sidecar transport",
        ))
    }
}

/// The production inference client that routes back into the TinyAgents harness.
///
/// Under the `tiny` feature this reaches the vendored harness; without it the
/// client returns a clear "enable the `tiny` feature" error rather than
/// panicking, so a `sidecar`-only build still links and fails loudly.
#[derive(Debug, Default)]
pub struct TinyAgentsInferenceClient {
    _private: (),
}

impl TinyAgentsInferenceClient {
    /// Builds the harness-backed inference client.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(feature = "tiny")]
#[async_trait]
impl InferenceClient for TinyAgentsInferenceClient {
    async fn infer(&self, _req: InferenceRequest) -> Result<InferenceResponse> {
        // The real routing into the vendored TinyAgents harness lands with the
        // harness integration; the seam is in place so nothing else changes.
        Err(crate::OpenCompanyError::Unimplemented(
            "tinyagents inference client",
        ))
    }
}

#[cfg(not(feature = "tiny"))]
#[async_trait]
impl InferenceClient for TinyAgentsInferenceClient {
    async fn infer(&self, _req: InferenceRequest) -> Result<InferenceResponse> {
        Err(crate::OpenCompanyError::Config(
            "sidecar inference requires the `tiny` feature to route to the TinyAgents harness"
                .to_string(),
        ))
    }
}
