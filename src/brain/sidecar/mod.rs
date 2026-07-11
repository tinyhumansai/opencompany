//! [`SidecarBrain`]: a [`Brain`] that drives a local sidecar process.
//!
//! The sidecar brain speaks the *same* `/orchestration/v1` wire frames as
//! [`HostedMedullaBrain`](crate::brain::HostedMedullaBrain) — it reuses
//! [`wire`](crate::brain::medulla::wire) verbatim and the shared wire→kernel
//! mapping in [`effects`](crate::brain::medulla::effects) — but talks to a local
//! sidecar process over stdio/HTTP instead of the hosted orchestrator. Its one
//! distinctive addition is the **inference inversion**: the sidecar runs the
//! cognitive loop but has no model access, so it calls *back* into the Rust host
//! through an [`InferenceClient`] to run each model pass (offline in tests; under
//! the `tiny` feature, into the TinyAgents harness).
//!
//! Cycle drain shape, deduplication on `callId`, the effect→gate→ack flow, and
//! the tool/context routing are identical to the hosted brain, so a supervised
//! `Sign`/`Spend` effect parks an approval through the real kernel gate exactly
//! as it does under hosted cognition.
//!
//! The whole module is gated behind the `sidecar` feature; the default build
//! links none of it and the builder routes `sidecar` mode to the echo brain.

mod mock;
mod transport;
mod types;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::StreamExt;

use crate::Result;
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::types::{
    CompanyId, CompressedTrace, CycleRequest, CycleResult, EffectDisposition, LedgerEntry,
    OutboundMessage, TokenUsage, ToolCall,
};

use crate::brain::medulla::effects::{
    EffectOutcome, channel_message_from_effect, context_op_from_call, context_result_to_value,
    effect_from_frame, ledger_delta_from_effect, wire_event,
};
use crate::brain::medulla::wire::{
    EffectFrame, EffectResult, EventsRequest, ToolCallFrame, ToolManifestEntry, ToolResultFrame,
};

pub use mock::{MockInferenceClient, MockSidecarTransport, RecordedInference};
pub use transport::{
    InferenceClient, SidecarTransport, StdioSidecarTransport, TinyAgentsInferenceClient,
};
pub use types::{InferenceMessage, InferenceRequest, InferenceResponse, SidecarFrame};

/// The default cap on inference passes the brain will run within one cycle.
pub const DEFAULT_MAX_PASSES: usize = 12;

/// The local-sidecar brain: one company's cognition over a sidecar process, with
/// inference routed back into the host.
pub struct SidecarBrain {
    transport: Arc<dyn SidecarTransport>,
    inference: Arc<dyn InferenceClient>,
    session_id: String,
    counterpart: String,
    tool_catalog: Vec<ToolManifestEntry>,
    max_passes: usize,
    registered: AtomicBool,
}

impl SidecarBrain {
    /// Builds a sidecar brain for `company` addressed as `opencompany:<slug>`.
    ///
    /// `inference` is the host-fulfilled callback the sidecar reaches back
    /// through; `tool_catalog` is the device-tool manifest registered on the
    /// first cycle. The pass cap defaults to [`DEFAULT_MAX_PASSES`]; override it
    /// with [`with_max_passes`](Self::with_max_passes).
    pub fn new(
        transport: Arc<dyn SidecarTransport>,
        inference: Arc<dyn InferenceClient>,
        company: &CompanyId,
        slug: &str,
        tool_catalog: Vec<ToolManifestEntry>,
    ) -> Self {
        Self {
            transport,
            inference,
            session_id: company.as_ref().to_string(),
            counterpart: format!("opencompany:{slug}"),
            tool_catalog,
            max_passes: DEFAULT_MAX_PASSES,
            registered: AtomicBool::new(false),
        }
    }

    /// Overrides the inference-pass cap enforced per cycle.
    pub fn with_max_passes(mut self, max_passes: usize) -> Self {
        self.max_passes = max_passes.max(1);
        self
    }

    /// The sidecar session id (the company id).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The counterpart agent id (`opencompany:<slug>`).
    pub fn counterpart(&self) -> &str {
        &self.counterpart
    }

    /// Registers the device-tool manifest exactly once (on the first cycle).
    async fn ensure_registered(&self) -> Result<()> {
        if self.tool_catalog.is_empty() {
            return Ok(());
        }
        if self
            .registered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.transport
                .register_tools(self.tool_catalog.clone())
                .await?;
        }
        Ok(())
    }

    /// Services one tool-call frame, routing `context_*` tools to the context
    /// store and everything else to the manifest-enforcing tool provider.
    async fn service_tool_call(
        &self,
        host: &dyn CycleHost,
        call: &ToolCallFrame,
    ) -> Result<ToolResultFrame> {
        if let Some(op) = context_op_from_call(&call.name, &call.args) {
            return Ok(match host.context_op(op).await {
                Ok(result) => ToolResultFrame {
                    call_id: call.call_id.clone(),
                    ok: true,
                    result: Some(context_result_to_value(result)),
                    error: None,
                },
                Err(err) => ToolResultFrame {
                    call_id: call.call_id.clone(),
                    ok: false,
                    result: None,
                    error: Some(err.to_string()),
                },
            });
        }

        let tool_call = ToolCall {
            tool: call.name.clone(),
            args: call.args.clone(),
        };
        Ok(match host.call_tool(tool_call).await {
            Ok(result) => ToolResultFrame {
                call_id: call.call_id.clone(),
                ok: result.ok,
                result: Some(result.output),
                error: None,
            },
            Err(err) => ToolResultFrame {
                call_id: call.call_id.clone(),
                ok: false,
                result: None,
                error: Some(err.to_string()),
            },
        })
    }

    /// Passes an effect frame through the gate, acks it, and returns what it
    /// contributed. The gate runs *before* the ack, so a parked effect acks
    /// `ok:false` and the sidecar learns it must wait.
    async fn service_effect(
        &self,
        host: &dyn CycleHost,
        frame: &EffectFrame,
    ) -> Result<EffectOutcome> {
        let effect = effect_from_frame(frame);
        let disposition = host.emit_effect(effect.clone()).await?;

        let mut outcome = EffectOutcome::default();
        let ack = match &disposition {
            EffectDisposition::Executed => {
                outcome.channel_response = channel_message_from_effect(&effect);
                outcome.ledger_delta = ledger_delta_from_effect(&effect);
                EffectResult {
                    call_id: frame.call_id.clone(),
                    ok: true,
                    error: None,
                    result: None,
                }
            }
            EffectDisposition::PendingApproval(id) => EffectResult {
                call_id: frame.call_id.clone(),
                ok: false,
                error: Some(format!("pending approval ({id})")),
                result: None,
            },
            EffectDisposition::Denied { reason } => EffectResult {
                call_id: frame.call_id.clone(),
                ok: false,
                error: Some(reason.clone()),
                result: None,
            },
        };
        self.transport.ack_effect(ack).await?;
        Ok(outcome)
    }
}

#[async_trait]
impl Brain for SidecarBrain {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
        self.ensure_registered().await?;

        let mut channel_responses: Vec<OutboundMessage> = Vec::new();
        let mut new_traces = Vec::new();
        let mut ledger_deltas: Vec<LedgerEntry> = Vec::new();
        let mut token_usage = TokenUsage::default();

        for (index, event) in req.events.iter().enumerate() {
            let seq = req
                .event_seqs
                .get(index)
                .map(|s| s.value())
                .unwrap_or(index as u64);

            let request = EventsRequest {
                counterpart_agent_id: self.counterpart.clone(),
                session_id: self.session_id.clone(),
                event: wire_event(seq, event),
            };
            let accepted = self.transport.post_events(request).await?;
            let cycle_id = accepted.cycle_id;

            // Drain the cycle's frames, deduping on callId (at-least-once).
            let mut seen: HashSet<String> = HashSet::new();
            let mut passes = 0usize;
            let mut frames = self.transport.cycle_frames(&cycle_id);
            while let Some(frame) = frames.next().await {
                match frame? {
                    SidecarFrame::CycleComplete => break,
                    SidecarFrame::Effect(effect_frame) => {
                        if !seen.insert(effect_frame.call_id.clone()) {
                            continue;
                        }
                        let outcome = self.service_effect(host, &effect_frame).await?;
                        if let Some(message) = outcome.channel_response {
                            channel_responses.push(message);
                        }
                        if let Some(delta) = outcome.ledger_delta {
                            ledger_deltas.push(delta);
                        }
                    }
                    SidecarFrame::ToolCall(call) => {
                        if !seen.insert(call.call_id.clone()) {
                            continue;
                        }
                        let answer = self.service_tool_call(host, &call).await?;
                        self.transport.answer_tool_call(answer).await?;
                    }
                    SidecarFrame::Inference { call_id, request } => {
                        if !seen.insert(call_id.clone()) {
                            continue;
                        }
                        // Honor the pass cap: stop draining once the sidecar has
                        // asked for more inference passes than the budget allows.
                        if passes >= self.max_passes {
                            break;
                        }
                        passes += 1;
                        // The inference inversion: the host runs the model pass.
                        let response = self.inference.infer(request).await?;
                        token_usage.input += response.token_usage.input;
                        token_usage.output += response.token_usage.output;
                        self.transport.answer_inference(&call_id, response).await?;
                    }
                }
            }

            new_traces.push(CompressedTrace::now(
                &cycle_id,
                format!("sidecar cycle for `{}` (seq {seq})", self.session_id),
            ));
        }

        Ok(CycleResult {
            channel_responses,
            new_traces,
            ledger_deltas,
            token_usage,
        })
    }
}

impl std::fmt::Debug for SidecarBrain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarBrain")
            .field("session_id", &self.session_id)
            .field("counterpart", &self.counterpart)
            .field("tools", &self.tool_catalog.len())
            .field("max_passes", &self.max_passes)
            .field("registered", &self.registered.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(test)]
mod test;
