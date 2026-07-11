//! The [`MedullaTransport`] seam: the abstraction the hosted brain drives.
//!
//! `HostedMedullaBrain` never depends on a concrete network client. It talks to
//! a `MedullaTransport`, which abstracts the two `POST` endpoints, the outbound
//! effect/tool-call stream, and the acks/answers the client sends back. The
//! default build ships the in-memory [`MockTransport`](super::mock::MockTransport);
//! the networked `HttpSocketTransport` lands behind a feature in a later batch.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::Result;

use super::wire::{
    EffectFrame, EffectResult, EventsAccepted, EventsRequest, ToolCallFrame, ToolManifestEntry,
    ToolResultFrame, WorldDiffAccepted, WorldDiffRequest,
};

/// A frame arriving out-of-band for an in-flight cycle.
///
/// The brain consumes [`MedullaTransport::cycle_frames`] until it sees
/// [`InboundFrame::CycleComplete`], routing effects through the approval gate
/// and tool calls through the host in between.
#[derive(Clone, Debug, PartialEq)]
pub enum InboundFrame {
    /// An effect to run then ack (`orch:effect:<kind>`).
    Effect(EffectFrame),
    /// A device tool to invoke then answer (`orch:tool_call`).
    ToolCall(ToolCallFrame),
    /// The cycle has finished; stop consuming the stream.
    CycleComplete,
}

/// The transport the hosted brain speaks `/orchestration/v1` through.
///
/// Implementations bridge the synchronous [`Brain`](crate::ports::Brain) port to
/// the asynchronous wire: the brain posts events, then drains
/// [`Self::cycle_frames`] for the returned cycle, acking effects and answering
/// tool calls as frames arrive.
#[async_trait]
pub trait MedullaTransport: Send + Sync {
    /// Ingests one event (`POST /events`) and returns the wake acknowledgement.
    async fn post_events(&self, req: EventsRequest) -> Result<EventsAccepted>;

    /// Uploads world-state notes (`POST /world-diff`).
    async fn post_world_diff(&self, req: WorldDiffRequest) -> Result<WorldDiffAccepted>;

    /// Registers the device tool manifest (`orch:register_tools`).
    async fn register_tools(&self, tools: Vec<ToolManifestEntry>) -> Result<()>;

    /// The out-of-band frame stream for `cycle_id`.
    ///
    /// Yields effects and tool calls as they arrive, terminated by
    /// [`InboundFrame::CycleComplete`] when the cycle finishes.
    fn cycle_frames(&self, cycle_id: &str) -> BoxStream<'static, Result<InboundFrame>>;

    /// Acks an effect (`orch:effect:result`).
    async fn ack_effect(&self, ack: EffectResult) -> Result<()>;

    /// Answers a device tool call (`orch:tool_result`).
    async fn answer_tool_call(&self, ans: ToolResultFrame) -> Result<()>;
}
