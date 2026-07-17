//! Offline tests for [`HostedMedullaBrain`] over the in-memory
//! [`MockTransport`], plus end-to-end tests that drive a real
//! [`CompanyRuntime`](crate::company::runtime::CompanyRuntime) with the brain
//! wired in through the builder.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;
use crate::brain::medulla::MockTransport;
use crate::brain::medulla::wire::{self, EffectFrame, OrchErrorCode, Role, ToolCallFrame};
use crate::ports::types::{
    ApprovalId, ChunkAddr, ChunkHit, CompanyEvent, ContextOp, ContextOpResult, Effect,
    EffectDisposition, ToolResult, Verdict,
};

// ---------------------------------------------------------------------------
// Test host
// ---------------------------------------------------------------------------

/// A [`CycleHost`] that records callbacks and returns canned dispositions.
struct RecordingHost {
    disposition: EffectDisposition,
    tool_result: ToolResult,
    effects: Mutex<Vec<Effect>>,
    tool_calls: Mutex<Vec<ToolCall>>,
    context_ops: Mutex<Vec<ContextOp>>,
}

impl RecordingHost {
    fn executing() -> Self {
        Self {
            disposition: EffectDisposition::Executed,
            tool_result: ToolResult {
                ok: true,
                output: json!({ "ran": true }),
            },
            effects: Mutex::new(Vec::new()),
            tool_calls: Mutex::new(Vec::new()),
            context_ops: Mutex::new(Vec::new()),
        }
    }

    fn parking() -> Self {
        Self {
            disposition: EffectDisposition::PendingApproval(ApprovalId::new("appr-1")),
            ..Self::executing()
        }
    }
}

#[async_trait]
impl CycleHost for RecordingHost {
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult> {
        self.tool_calls.lock().unwrap().push(call);
        Ok(self.tool_result.clone())
    }

    async fn context_op(&self, op: ContextOp) -> Result<ContextOpResult> {
        self.context_ops.lock().unwrap().push(op);
        Ok(ContextOpResult::Hits(vec![ChunkHit {
            addr: ChunkAddr::new("c1"),
            snippet: "hit".into(),
            score: 1.0,
        }]))
    }

    async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition> {
        self.effects.lock().unwrap().push(effect);
        Ok(self.disposition.clone())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn brain(transport: Arc<MockTransport>) -> HostedMedullaBrain {
    HostedMedullaBrain::new(
        transport,
        &CompanyId::new("acme"),
        "acme",
        SecretValue("th_super_secret".into()),
        vec![ToolManifestEntry {
            name: "noop".into(),
            description: None,
            input_schema: None,
        }],
    )
}

/// The deterministic cycle id for a first operator event on company `acme`.
fn cid() -> String {
    wire::cycle_id("opencompany:acme", "acme", 0)
}

fn operator_request() -> CycleRequest {
    CycleRequest {
        cycle_id: "unused".into(),
        company_id: CompanyId::new("acme"),
        events: vec![CompanyEvent::OperatorMessage {
            text: "hi".into(),
            by: None,
        }],
        event_seqs: Vec::new(),
        compressed_history: Vec::new(),
        roster: Vec::new(),
        context_index: Vec::new(),
    }
}

fn effect_frame(kind: &str, index: usize, payload: Value) -> InboundFrame {
    InboundFrame::Effect(EffectFrame {
        kind: kind.into(),
        cycle_id: cid(),
        call_id: wire::call_id(&cid(), kind, index),
        payload,
    })
}

fn tool_call_frame(name: &str, index: usize, args: Value) -> InboundFrame {
    InboundFrame::ToolCall(ToolCallFrame {
        cycle_id: cid(),
        call_id: wire::call_id(&cid(), "tool", index),
        name: name.into(),
        args,
        timeout_ms: wire::DEFAULT_TOOL_TIMEOUT_MS,
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn posts_one_normalized_event_without_a_model_field() {
    let transport = Arc::new(MockTransport::new());
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();

    let posted = transport.posted_events();
    assert_eq!(posted.len(), 1);
    let event = &posted[0].event;
    assert_eq!(event.seq, 0);
    assert_eq!(event.role, Role::User);
    assert_eq!(event.sender, "operator");
    assert_eq!(event.body, "hi");
    assert_eq!(event.kind, "operator.message");
    assert_eq!(posted[0].counterpart_agent_id, "opencompany:acme");
    assert_eq!(posted[0].session_id, "acme");

    // The serialized wire body must never carry a `model` field.
    let body = serde_json::to_value(wire::Envelope::v1(posted[0].clone())).unwrap();
    assert!(wire::assert_no_model(&body).is_ok());

    // The device-tool manifest was registered exactly once.
    assert_eq!(transport.registered_tools().len(), 1);
}

#[tokio::test]
async fn register_tools_fires_only_on_the_first_cycle() {
    let transport = Arc::new(MockTransport::new());
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();
    brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(transport.registered_tools().len(), 1);
}

#[tokio::test]
async fn executed_send_dm_becomes_a_channel_response_and_acks_ok() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        cid(),
        vec![effect_frame(
            "send_dm",
            0,
            json!({ "to": "operator", "body": "hello from medulla" }),
        )],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.channel_responses.len(), 1);
    assert_eq!(result.channel_responses[0].channel, "operator");
    assert_eq!(result.channel_responses[0].text, "hello from medulla");

    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].ok);
    // The effect passed through the gate before the ack.
    assert_eq!(host.effects.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn duplicate_effect_frame_is_handled_once() {
    let transport = Arc::new(MockTransport::new());
    // Two frames sharing a callId: the replay must be ignored.
    let payload = json!({ "to": "operator", "body": "dup" });
    transport.script_cycle(
        cid(),
        vec![
            effect_frame("send_dm", 0, payload.clone()),
            effect_frame("send_dm", 0, payload),
        ],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(host.effects.lock().unwrap().len(), 1);
    assert_eq!(transport.acks().len(), 1);
    assert_eq!(result.channel_responses.len(), 1);
}

#[tokio::test]
async fn tool_call_frame_routes_to_call_tool_and_answers() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(cid(), vec![tool_call_frame("noop", 0, json!({ "q": 1 }))]);
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();

    let calls = host.tool_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, "noop");

    let answers = transport.tool_answers();
    assert_eq!(answers.len(), 1);
    assert!(answers[0].ok);
    assert!(answers[0].result.is_some());
}

#[tokio::test]
async fn context_device_tool_routes_to_context_op() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        cid(),
        vec![tool_call_frame(
            "context_search",
            0,
            json!({ "query": "roadmap", "limit": 3 }),
        )],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();

    let ops = host.context_ops.lock().unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        ContextOp::Search { query, limit } => {
            assert_eq!(query, "roadmap");
            assert_eq!(*limit, 3);
        }
        other => panic!("expected a context search, got {other:?}"),
    }
    // The tool_call was not forwarded to the tool provider.
    assert!(host.tool_calls.lock().unwrap().is_empty());
    assert_eq!(transport.tool_answers().len(), 1);
}

#[tokio::test]
async fn parked_effect_acks_not_ok_with_pending_approval() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(cid(), vec![effect_frame("filing.submit", 0, Value::Null)]);
    let brain = brain(transport.clone());
    let host = RecordingHost::parking();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(!acks[0].ok);
    assert!(
        acks[0]
            .error
            .as_deref()
            .unwrap()
            .contains("pending approval")
    );
    // A parked effect yields no channel response and no world-diff.
    assert!(result.channel_responses.is_empty());
    assert!(transport.posted_world_diffs().is_empty());
}

#[tokio::test]
async fn orchestration_error_on_post_events_propagates_with_code() {
    let transport = Arc::new(MockTransport::new());
    transport.fail_post_events(OrchErrorCode::InsufficientBalance);
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let err = brain
        .run_cycle(operator_request(), &host)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "ORCH_INSUFFICIENT_BALANCE");
}

#[tokio::test]
async fn spend_effect_records_ledger_delta_and_posts_world_diff() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        cid(),
        vec![effect_frame(
            "x402.spend",
            0,
            json!({ "amountUsd": 4.25, "memo": "api call" }),
        )],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.ledger_deltas.len(), 1);
    assert_eq!(result.ledger_deltas[0].amount_usd, 4.25);
    assert_eq!(result.ledger_deltas[0].kind, "x402.spend");
    // Spend is notable, so a world-diff was uploaded.
    let diffs = transport.posted_world_diffs();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].entries.len(), 1);
    assert_eq!(diffs[0].session_id, "acme");
}

#[test]
fn debug_redacts_the_credential() {
    let transport = Arc::new(MockTransport::new());
    let brain = brain(transport);
    let rendered = format!("{brain:?}");
    assert!(!rendered.contains("th_super_secret"));
    assert!(rendered.contains("redacted"));
}

// ---------------------------------------------------------------------------
// End-to-end tests through a real CompanyRuntime
// ---------------------------------------------------------------------------

use crate::app::config::BrainMode;
use crate::company::CompanyManifest;
use crate::ports::types::{Actor, ActorKind};
use crate::runtime::RuntimeBuilder;

fn tmp_home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "opencompany-hosted-{}",
        crate::ports::generate_id()
    ))
}

fn manifest(policy_mode: &str) -> CompanyManifest {
    let toml_src = format!(
        r#"
        [company]
        name = "Acme"

        [brain]
        mode = "hosted"

        [tools]
        allow = ["noop"]

        [policy]
        mode = "{policy_mode}"
        "#
    );
    toml::from_str(&toml_src).expect("valid manifest")
}

/// The deterministic first-cycle id a real runtime for `Acme` produces: the
/// company id slugs to `acme`, the first event lands at seq 0.
fn runtime_cid() -> String {
    wire::cycle_id("opencompany:acme", "acme", 0)
}

#[tokio::test]
async fn e2e_operator_message_drives_tool_call_and_gated_send_dm() {
    let home = tmp_home();
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        runtime_cid(),
        vec![
            tool_call_frame("noop", 0, json!({ "q": "status" })),
            effect_frame("send_dm", 0, json!({ "to": "operator", "body": "on it" })),
        ],
    );

    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    let report = rt
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "how are we doing".into(),
            by: None,
        }])
        .await
        .unwrap();

    // The gated send_dm produced a channel response routed to the operator.
    assert_eq!(report.responses.len(), 1);
    assert_eq!(report.responses[0].channel, "operator");
    assert_eq!(report.responses[0].text, "on it");

    // The effect flowed through the gate and acked ok:true.
    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].ok);

    // The device tool was serviced and answered.
    assert_eq!(transport.tool_answers().len(), 1);

    // Exactly one event was posted for the operator message.
    assert_eq!(transport.posted_events().len(), 1);

    // A compressed trace was persisted to the fs-backed MemoryStore.
    let traces = rt.memory.recent_traces(rt.id(), 10).await.unwrap();
    assert!(!traces.is_empty());

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn e2e_supervised_effect_parks_and_acks_not_ok() {
    let home = tmp_home();
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        runtime_cid(),
        // A Sign-group effect always parks under supervised policy.
        vec![effect_frame("filing.submit", 0, Value::Null)],
    );

    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    let report = rt
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "file it".into(),
            by: None,
        }])
        .await
        .unwrap();

    // The effect parked: an approval is queued and no channel response emitted.
    assert_eq!(report.parked.len(), 1);
    assert_eq!(rt.pending_approvals().len(), 1);
    assert!(report.responses.is_empty());

    // Medulla was told the effect is pending, not that it succeeded.
    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(!acks[0].ok);
    assert!(
        acks[0]
            .error
            .as_deref()
            .unwrap()
            .contains("pending approval")
    );

    // Resolving the approval executes the parked effect and drains the queue.
    let approval_id = report.parked[0].clone();
    rt.resolve_approval(
        &approval_id,
        Verdict::Approve,
        Actor {
            kind: ActorKind::Operator,
            id: "owner".into(),
        },
    )
    .await
    .unwrap();
    assert!(rt.pending_approvals().is_empty());

    tokio::fs::remove_dir_all(&home).await.ok();
}
