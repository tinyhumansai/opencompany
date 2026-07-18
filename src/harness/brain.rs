//! [`HarnessBrain`]: the cognition [`Brain`] backed by the embedded OpenHuman
//! runtime.
//!
//! Where [`EchoBrain`](crate::brain::EchoBrain) turns every operator message
//! into `"You said: …"`, `HarnessBrain` routes it to a live openhuman
//! [`Agent`](openhuman_core::openhuman::agent::Agent) through a
//! [`HarnessPool`], so the reply comes from the hosted brain and the turn's
//! token/cost usage is metered into the company ledger.
//!
//! v1 is **single-responder**: one agent (the first on the roster, or the id
//! given to [`HarnessBrain::with_responder`]) answers every operator message.
//! Desk routing — resolving which roster member owns a given group chat — is the
//! WS3 chat handler's job and lands separately.
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::types::{
    CompanyEvent, CompanyRecord, CompressedTrace, CycleRequest, CycleResult, OutboundMessage,
    TokenUsage,
};

/// A [`Brain`] that answers with a live openhuman agent turn.
pub struct HarnessBrain {
    pool: Arc<HarnessPool>,
    deps: HarnessDeps,
    record: CompanyRecord,
    responder: String,
}

impl HarnessBrain {
    /// Builds a harness brain for `record`, answering with the first roster
    /// agent. The pool is shared so the roster is built once and reused across
    /// cycles.
    pub fn new(pool: Arc<HarnessPool>, deps: HarnessDeps, record: CompanyRecord) -> Self {
        let responder = record
            .manifest
            .agents
            .first()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        Self {
            pool,
            deps,
            record,
            responder,
        }
    }

    /// Overrides which roster agent answers operator messages.
    pub fn with_responder(mut self, agent_id: impl Into<String>) -> Self {
        self.responder = agent_id.into();
        self
    }
}

#[async_trait]
impl Brain for HarnessBrain {
    async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
        // Idempotent — builds the roster on the first cycle, a no-op after.
        self.pool.ensure(&self.record, &self.deps).await?;

        let mut channel_responses = Vec::new();
        for event in &req.events {
            if let CompanyEvent::OperatorMessage { text, .. } = event {
                // `run` executes the turn on the openhuman runtime and meters
                // its usage into the ledger/meter through `deps`. The reply is
                // the agent's own text.
                let reply = self
                    .pool
                    .run(&self.record.id, &self.responder, text, &self.deps)
                    .await?;
                channel_responses.push(OutboundMessage {
                    channel: "operator".to_string(),
                    text: reply,
                });
            }
        }
        // The runtime requires at least one channel response per cycle.
        if channel_responses.is_empty() {
            channel_responses.push(OutboundMessage {
                channel: "operator".to_string(),
                text: "Acknowledged.".to_string(),
            });
        }

        let trace = CompressedTrace::now(
            req.cycle_id.clone(),
            format!("harness cycle handled {} event(s)", req.events.len()),
        );

        // No `ledger_deltas` / `token_usage` here on purpose: `HarnessPool::run`
        // is the single cost-accounting site (it writes the ledger entry and the
        // usage sample through `deps`), so surfacing the same spend again would
        // double-count it.
        Ok(CycleResult {
            channel_responses,
            new_traces: vec![trace],
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::harness::provider::MockProvider;
    use crate::ports::brain::CycleHost;
    use crate::ports::types::{
        CompanyId, ContextOp, ContextOpResult, Effect, EffectDisposition, ToolCall, ToolResult,
    };
    use crate::store::{FsCompanyStore, FsContextStore, FsOps};

    /// A `CycleHost` that auto-executes anything the brain asks for; the harness
    /// brain v1 makes no host calls, so it stays inert.
    #[derive(Default)]
    struct NoopHost;

    #[async_trait]
    impl CycleHost for NoopHost {
        async fn call_tool(&self, _call: ToolCall) -> Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                output: serde_json::Value::Null,
            })
        }
        async fn context_op(&self, _op: ContextOp) -> Result<ContextOpResult> {
            Ok(ContextOpResult::Text(String::new()))
        }
        async fn emit_effect(&self, _effect: Effect) -> Result<EffectDisposition> {
            Ok(EffectDisposition::Executed)
        }
    }

    fn record() -> CompanyRecord {
        let manifest = toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Runs Acme."
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
        }
    }

    fn brain_over_mock(dir: &std::path::Path) -> HarnessBrain {
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(FsContextStore::new(dir)),
            store: Arc::new(FsCompanyStore::new(dir)),
            meter: Some(Arc::new(FsOps::new(dir))),
            workspace_root: dir.to_path_buf(),
            model_override: None,
        };
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record())
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
    async fn operator_message_gets_an_agent_reply() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(
                request(vec![CompanyEvent::OperatorMessage {
                    text: "status?".into(),
                    by: None,
                }]),
                &NoopHost,
            )
            .await
            .expect("cycle runs");

        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].channel, "operator");
        // The mock provider prefixes the routed message, proving the turn ran
        // through the openhuman agent rather than an echo.
        assert!(
            result.channel_responses[0].text.contains("status?"),
            "{:?}",
            result.channel_responses[0].text
        );
        assert_eq!(result.new_traces.len(), 1);
        // Single cost-accounting site: the cycle result carries no ledger delta.
        assert!(result.ledger_deltas.is_empty());
    }

    #[tokio::test]
    async fn no_events_still_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        let result = brain
            .run_cycle(request(Vec::new()), &NoopHost)
            .await
            .expect("cycle runs");
        assert_eq!(result.channel_responses.len(), 1);
        assert_eq!(result.channel_responses[0].text, "Acknowledged.");
    }

    #[test]
    fn responder_defaults_to_first_roster_agent() {
        let dir = tempfile::tempdir().unwrap();
        let brain = brain_over_mock(dir.path());
        assert_eq!(brain.responder, "ceo");
        let brain = brain.with_responder("cfo");
        assert_eq!(brain.responder, "cfo");
    }
}
