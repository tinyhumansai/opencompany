//! [`EchoBrain`]: the offline, dependency-free default [`Brain`].
//!
//! It has zero TinyAgents dependency and is the Phase-1 default cognition seam.
//! Each cycle it echoes every operator message back, emits one trivial
//! `Other`-group effect to exercise the [`CycleHost`] approval path, and
//! records a single [`CompressedTrace`]. It guarantees at least one channel
//! response per cycle, satisfying the runtime's ≥1-response invariant cheaply.

use async_trait::async_trait;

use crate::Result;
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::types::{
    CompanyEvent, CompressedTrace, CycleRequest, CycleResult, Effect, EffectGroup, OutboundMessage,
    TokenUsage,
};

/// The offline echo brain: turns operator messages into acknowledgements.
#[derive(Clone, Copy, Debug, Default)]
pub struct EchoBrain;

impl EchoBrain {
    /// Creates an echo brain.
    pub fn new() -> Self {
        Self
    }

    /// The trivial effect emitted each cycle to prove the gate path is wired.
    fn heartbeat_effect() -> Effect {
        Effect {
            kind: "echo.noop".to_string(),
            group: EffectGroup::Other,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
        }
    }
}

#[async_trait]
impl Brain for EchoBrain {
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
        let mut channel_responses = Vec::new();
        for event in &req.events {
            if let CompanyEvent::OperatorMessage { text, .. } = event {
                channel_responses.push(OutboundMessage {
                    channel: "operator".to_string(),
                    text: format!("You said: {text}"),
                });
            }
        }
        if channel_responses.is_empty() {
            channel_responses.push(OutboundMessage {
                channel: "operator".to_string(),
                text: "Acknowledged.".to_string(),
            });
        }

        // Exercise the approval/effect path; the disposition is informational
        // for the echo brain, so we don't branch on it.
        let _ = host.emit_effect(Self::heartbeat_effect()).await?;

        let trace = CompressedTrace::now(
            req.cycle_id.clone(),
            format!("echo cycle handled {} event(s)", req.events.len()),
        );

        Ok(CycleResult {
            channel_responses,
            new_traces: vec![trace],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::{
        CompanyId, ContextOp, ContextOpResult, EffectDisposition, ToolCall, ToolResult,
    };

    /// A minimal host that records emitted effects and auto-executes them.
    #[derive(Default)]
    struct RecordingHost {
        effects: std::sync::Mutex<Vec<Effect>>,
    }

    #[async_trait]
    impl CycleHost for RecordingHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }

        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }

        async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition> {
            self.effects.lock().unwrap().push(effect);
            Ok(EffectDisposition::Executed)
        }
    }

    fn request(events: Vec<CompanyEvent>) -> CycleRequest {
        CycleRequest {
            cycle_id: "cycle-1".to_string(),
            company_id: CompanyId::new("acme"),
            events,
            event_seqs: Vec::new(),
            compressed_history: Vec::new(),
            roster: Vec::new(),
            context_index: Vec::new(),
        }
    }

    #[tokio::test]
    async fn echoes_operator_message_and_records_trace() {
        let brain = EchoBrain::new();
        let host = RecordingHost::default();
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "hi".into(),
                    by: None,
                    chat: None,
                }]),
                &host,
            )
            .await
            .unwrap();

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        assert_eq!(result.channel_responses[0].text, "You said: hi");
        assert_eq!(result.new_traces.len(), 1);
        // The heartbeat effect flowed through the host.
        assert_eq!(host.effects.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn guarantees_a_response_with_no_events() {
        let brain = EchoBrain::new();
        let host = RecordingHost::default();
        let result = brain.run_cycle(request(Vec::new()), &host).await.unwrap();
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
    }
}
