//! Offline mocks for the sidecar seams.
//!
//! [`MockSidecarTransport`] scripts a [`SidecarFrame`] plan per cycle and records
//! every call the brain makes (posts, tool registrations, acks, tool answers,
//! inference answers). [`MockInferenceClient`] returns a canned completion and
//! records the requests it received. Together they drive the whole sidecar brain
//! offline — no network, no Node process.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};

use crate::Result;
use crate::ports::types::TokenUsage;

use crate::brain::medulla::wire::{
    self, EffectResult, EventsAccepted, EventsRequest, OrchErrorCode, ToolManifestEntry,
    ToolResultFrame,
};

use super::transport::{InferenceClient, SidecarTransport};
use super::types::{InferenceRequest, InferenceResponse, SidecarFrame};

/// An inference answer the mock transport recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedInference {
    /// The correlation id answered.
    pub call_id: String,
    /// The completion returned.
    pub response: InferenceResponse,
}

/// Everything the mock transport recorded, for test assertions.
#[derive(Default)]
struct Recorded {
    posted_events: Vec<EventsRequest>,
    registered_tools: Vec<Vec<ToolManifestEntry>>,
    acks: Vec<EffectResult>,
    tool_answers: Vec<ToolResultFrame>,
    inference_answers: Vec<RecordedInference>,
}

/// An in-memory [`SidecarTransport`] that scripts cycle frames and records calls.
#[derive(Default)]
pub struct MockSidecarTransport {
    /// Scripted frames keyed by cycle id.
    plans: Mutex<HashMap<String, Vec<SidecarFrame>>>,
    /// An error to return from every `post_events`.
    post_events_error: Mutex<Option<OrchErrorCode>>,
    /// Recorded calls.
    recorded: Mutex<Recorded>,
}

impl MockSidecarTransport {
    /// Creates an empty mock with no scripted cycles.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripts the frames [`Self::cycle_frames`] yields for `cycle_id`.
    ///
    /// A trailing [`SidecarFrame::CycleComplete`] is appended automatically so
    /// the brain's drain loop always terminates.
    pub fn script_cycle(&self, cycle_id: impl Into<String>, mut frames: Vec<SidecarFrame>) {
        if !matches!(frames.last(), Some(SidecarFrame::CycleComplete)) {
            frames.push(SidecarFrame::CycleComplete);
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

    /// The inference answers the brain emitted, in order.
    pub fn inference_answers(&self) -> Vec<RecordedInference> {
        self.recorded.lock().unwrap().inference_answers.clone()
    }
}

#[async_trait]
impl SidecarTransport for MockSidecarTransport {
    async fn post_events(&self, req: EventsRequest) -> Result<EventsAccepted> {
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

    async fn register_tools(&self, tools: Vec<ToolManifestEntry>) -> Result<()> {
        self.recorded.lock().unwrap().registered_tools.push(tools);
        Ok(())
    }

    fn cycle_frames(&self, cycle_id: &str) -> BoxStream<'static, Result<SidecarFrame>> {
        let frames = self
            .plans
            .lock()
            .unwrap()
            .get(cycle_id)
            .cloned()
            .unwrap_or_else(|| vec![SidecarFrame::CycleComplete]);
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

    async fn answer_inference(&self, call_id: &str, resp: InferenceResponse) -> Result<()> {
        self.recorded
            .lock()
            .unwrap()
            .inference_answers
            .push(RecordedInference {
                call_id: call_id.to_string(),
                response: resp,
            });
        Ok(())
    }
}

/// An offline [`InferenceClient`] that returns a canned completion and records
/// every request it received.
pub struct MockInferenceClient {
    text: String,
    token_usage: TokenUsage,
    requests: Mutex<Vec<InferenceRequest>>,
}

impl Default for MockInferenceClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockInferenceClient {
    /// Builds a client that always answers with an empty completion.
    pub fn new() -> Self {
        Self {
            text: String::new(),
            token_usage: TokenUsage::default(),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Sets the canned completion text.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Sets the token usage each completion reports.
    pub fn with_tokens(mut self, input: u64, output: u64) -> Self {
        self.token_usage = TokenUsage { input, output };
        self
    }

    /// The inference requests this client received, in order.
    pub fn requests(&self) -> Vec<InferenceRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl InferenceClient for MockInferenceClient {
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse> {
        self.requests.lock().unwrap().push(req);
        Ok(InferenceResponse {
            text: self.text.clone(),
            token_usage: self.token_usage,
        })
    }
}
