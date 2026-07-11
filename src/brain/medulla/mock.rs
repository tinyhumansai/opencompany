//! [`MockTransport`]: an in-memory [`MedullaTransport`] for offline tests.
//!
//! It records every call the brain makes (posts, tool registrations, acks, tool
//! answers) for later assertion, replays a scripted frame plan per cycle, and
//! can inject an orchestration error to exercise the error-mapping path. No
//! network, no async runtime assumptions beyond the trait's `async` methods.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};

use crate::Result;

use super::transport::{InboundFrame, MedullaTransport};
use super::wire::{
    self, EffectResult, EventsAccepted, EventsRequest, OrchErrorCode, ToolManifestEntry,
    ToolResultFrame, WorldDiffAccepted, WorldDiffRequest,
};

/// Everything the mock recorded, for test assertions.
#[derive(Default)]
struct Recorded {
    posted_events: Vec<EventsRequest>,
    posted_world_diffs: Vec<WorldDiffRequest>,
    registered_tools: Vec<Vec<ToolManifestEntry>>,
    acks: Vec<EffectResult>,
    tool_answers: Vec<ToolResultFrame>,
}

/// An in-memory transport that scripts cycle frames and records brain calls.
#[derive(Default)]
pub struct MockTransport {
    /// Scripted frames keyed by cycle id.
    plans: Mutex<HashMap<String, Vec<InboundFrame>>>,
    /// An error to return from the next (and every) `post_events`.
    post_events_error: Mutex<Option<OrchErrorCode>>,
    /// Recorded calls.
    recorded: Mutex<Recorded>,
}

impl MockTransport {
    /// Creates an empty mock with no scripted cycles.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripts the frames [`Self::cycle_frames`] will yield for `cycle_id`.
    ///
    /// A trailing [`InboundFrame::CycleComplete`] is appended automatically if
    /// the plan does not already end with one, so the brain's drain loop always
    /// terminates.
    pub fn script_cycle(&self, cycle_id: impl Into<String>, mut frames: Vec<InboundFrame>) {
        if !matches!(frames.last(), Some(InboundFrame::CycleComplete)) {
            frames.push(InboundFrame::CycleComplete);
        }
        self.plans.lock().unwrap().insert(cycle_id.into(), frames);
    }

    /// Makes every subsequent `post_events` fail with `code`.
    pub fn fail_post_events(&self, code: OrchErrorCode) {
        *self.post_events_error.lock().unwrap() = Some(code);
    }

    /// The `EventsRequest`s the brain posted, in order.
    pub fn posted_events(&self) -> Vec<EventsRequest> {
        self.recorded.lock().unwrap().posted_events.clone()
    }

    /// The `WorldDiffRequest`s the brain posted, in order.
    pub fn posted_world_diffs(&self) -> Vec<WorldDiffRequest> {
        self.recorded.lock().unwrap().posted_world_diffs.clone()
    }

    /// The tool manifests the brain registered, in order.
    pub fn registered_tools(&self) -> Vec<Vec<ToolManifestEntry>> {
        self.recorded.lock().unwrap().registered_tools.clone()
    }

    /// The effect acks the brain emitted, in order.
    pub fn acks(&self) -> Vec<EffectResult> {
        self.recorded.lock().unwrap().acks.clone()
    }

    /// The tool answers the brain emitted, in order.
    pub fn tool_answers(&self) -> Vec<ToolResultFrame> {
        self.recorded.lock().unwrap().tool_answers.clone()
    }
}

#[async_trait]
impl MedullaTransport for MockTransport {
    async fn post_events(&self, req: EventsRequest) -> Result<EventsAccepted> {
        // Fidelity: real transports reject a smuggled `model` field before the
        // POST. The typed body never has one, so this always passes here.
        let body = serde_json::to_value(wire::Envelope::v1(req.clone()))?;
        wire::assert_no_model(&body)?;

        if let Some(code) = self.post_events_error.lock().unwrap().clone() {
            return Err(code.to_error("mock post_events failure"));
        }

        let cycle_id = wire::cycle_id(&req.counterpart_agent_id, &req.session_id, req.event.seq);
        self.recorded.lock().unwrap().posted_events.push(req);
        Ok(EventsAccepted {
            accepted: true,
            cycle_id,
        })
    }

    async fn post_world_diff(&self, req: WorldDiffRequest) -> Result<WorldDiffAccepted> {
        req.validate()?;
        let body = serde_json::to_value(wire::Envelope::v1(req.clone()))?;
        wire::assert_no_model(&body)?;
        self.recorded.lock().unwrap().posted_world_diffs.push(req);
        Ok(WorldDiffAccepted {
            accepted: true,
            duplicates: 0,
            tick_scheduled: true,
        })
    }

    async fn register_tools(&self, tools: Vec<ToolManifestEntry>) -> Result<()> {
        self.recorded.lock().unwrap().registered_tools.push(tools);
        Ok(())
    }

    fn cycle_frames(&self, cycle_id: &str) -> BoxStream<'static, Result<InboundFrame>> {
        let frames = self
            .plans
            .lock()
            .unwrap()
            .get(cycle_id)
            .cloned()
            .unwrap_or_else(|| vec![InboundFrame::CycleComplete]);
        stream::iter(frames.into_iter().map(Ok)).boxed()
    }

    async fn ack_effect(&self, ack: EffectResult) -> Result<()> {
        self.recorded.lock().unwrap().acks.push(ack);
        Ok(())
    }

    async fn answer_tool_call(&self, ans: ToolResultFrame) -> Result<()> {
        self.recorded.lock().unwrap().tool_answers.push(ans);
        Ok(())
    }
}

#[cfg(test)]
mod test;
