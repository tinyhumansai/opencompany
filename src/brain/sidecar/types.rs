//! The sidecar protocol types.
//!
//! The sidecar brain reuses the hosted Medulla wire frames verbatim (see
//! [`wire`](crate::brain::medulla::wire)) for effects and tool calls, and adds
//! one direction the hosted contract does not have: **host-bound inference**.
//! The sidecar runs the cognitive loop locally but has no model access of its
//! own, so it calls *back* into the Rust host to run inference. These types
//! describe that inversion.

use serde::{Deserialize, Serialize};

use crate::ports::types::TokenUsage;

use crate::brain::medulla::wire::{EffectFrame, ToolCallFrame};

/// One message in an inference request, in the provider-neutral `role`/`content`
/// shape the host's harness expects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceMessage {
    /// The message author role (`system`/`user`/`assistant`).
    pub role: String,
    /// The message text.
    pub content: String,
}

/// A request the sidecar sends back to the host to run one inference pass.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceRequest {
    /// The conversation the host should complete.
    pub messages: Vec<InferenceMessage>,
    /// The sidecar session the request belongs to (the company id).
    pub session_id: String,
}

/// The host's answer to an [`InferenceRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceResponse {
    /// The completion text.
    pub text: String,
    /// The tokens the pass consumed, folded into the cycle's usage total.
    pub token_usage: TokenUsage,
}

/// A frame the sidecar pushes to the host during an in-flight cycle.
///
/// Mirrors [`InboundFrame`](crate::brain::medulla::transport::InboundFrame) but
/// adds the [`SidecarFrame::Inference`] direction: the sidecar asks the host to
/// run a model pass and continues once the host answers.
#[derive(Clone, Debug, PartialEq)]
pub enum SidecarFrame {
    /// An effect to gate then ack (`orch:effect:<kind>`).
    Effect(EffectFrame),
    /// A device tool to invoke then answer (`orch:tool_call`).
    ToolCall(ToolCallFrame),
    /// A host-bound inference request the host must answer to unblock the cycle.
    Inference {
        /// The correlation id the answer is keyed on.
        call_id: String,
        /// The conversation to complete.
        request: InferenceRequest,
    },
    /// The cycle has finished; stop consuming the stream.
    CycleComplete,
}
