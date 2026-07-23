//! Offline tests for [`SidecarBrain`] over the mock sidecar transport and mock
//! inference client, plus an end-to-end test that drives a real
//! [`CompanyRuntime`](crate::company::runtime::CompanyRuntime) with the brain
//! injected through the builder. No test touches the network or a Node process.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;
use crate::brain::medulla::wire::{self, EffectFrame, OrchErrorCode, ToolCallFrame};
use crate::ports::types::{
    ApprovalId, ChunkAddr, ChunkHit, CompanyEvent, ContextOp, ContextOpResult, Effect,
    EffectDisposition, ToolResult,
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

fn brain(
    transport: Arc<MockSidecarTransport>,
    inference: Arc<MockInferenceClient>,
) -> SidecarBrain {
    SidecarBrain::new(
        transport,
        inference,
        &CompanyId::new("acme"),
        "acme",
        vec![ToolManifestEntry {
            name: "noop".into(),
            description: None,
            input_schema: None,
        }],
    )
}

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
            chat: None,
        }],
        event_seqs: Vec::new(),
        compressed_history: Vec::new(),
        roster: Vec::new(),
        context_index: Vec::new(),
    }
}

fn effect_frame(kind: &str, index: usize, payload: Value) -> SidecarFrame {
    SidecarFrame::Effect(EffectFrame {
        kind: kind.into(),
        cycle_id: cid(),
        call_id: wire::call_id(&cid(), kind, index),
        payload,
    })
}

fn tool_call_frame(name: &str, index: usize, args: Value) -> SidecarFrame {
    SidecarFrame::ToolCall(ToolCallFrame {
        cycle_id: cid(),
        call_id: wire::call_id(&cid(), "tool", index),
        name: name.into(),
        args,
        timeout_ms: wire::DEFAULT_TOOL_TIMEOUT_MS,
    })
}

fn inference_frame(index: usize, prompt: &str) -> SidecarFrame {
    SidecarFrame::Inference {
        call_id: wire::call_id(&cid(), "infer", index),
        request: InferenceRequest {
            messages: vec![InferenceMessage {
                role: "user".into(),
                content: prompt.into(),
            }],
            session_id: "acme".into(),
        },
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn posts_event_and_registers_tools_once() {
    let transport = Arc::new(MockSidecarTransport::new());
    let inference = Arc::new(MockInferenceClient::new());
    let brain = brain(transport.clone(), inference);
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();
    brain.run_cycle(operator_request(), &host).await.unwrap();

    let posted = transport.posted_events();
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0].counterpart_agent_id, "opencompany:acme");
    assert_eq!(posted[0].session_id, "acme");
    assert_eq!(posted[0].event.kind, "operator.message");
    // The device-tool manifest was registered exactly once.
    assert_eq!(transport.registered_tools().len(), 1);
}

#[tokio::test]
async fn inference_frame_invokes_host_callback_and_answers() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(cid(), vec![inference_frame(0, "what next?")]);
    let inference = Arc::new(
        MockInferenceClient::new()
            .with_text("do the thing")
            .with_tokens(11, 7),
    );
    let brain = brain(transport.clone(), inference.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    // The host-bound inference callback fired with the sidecar's prompt.
    let requests = inference.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].content, "what next?");

    // The completion was answered back to the sidecar, keyed on its call id.
    let answers = transport.inference_answers();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].response.text, "do the thing");

    // Token usage accumulated into the cycle result.
    assert_eq!(result.token_usage.input, 11);
    assert_eq!(result.token_usage.output, 7);
}

#[tokio::test]
async fn executed_send_dm_becomes_a_channel_response_and_acks_ok() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        cid(),
        vec![effect_frame(
            "send_dm",
            0,
            json!({ "to": "operator", "body": "hello from the sidecar" }),
        )],
    );
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.channel_responses.len(), 1);
    assert_eq!(result.channel_responses[0].channel, "operator");
    assert_eq!(result.channel_responses[0].text, "hello from the sidecar");

    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].ok);
    // The effect passed through the gate before the ack.
    assert_eq!(host.effects.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn parked_effect_acks_not_ok_with_pending_approval() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(cid(), vec![effect_frame("filing.submit", 0, Value::Null)]);
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
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
    assert!(result.channel_responses.is_empty());
}

#[tokio::test]
async fn tool_call_frame_routes_to_call_tool_and_answers() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(cid(), vec![tool_call_frame("noop", 0, json!({ "q": 1 }))]);
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();

    let calls = host.tool_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, "noop");

    let answers = transport.tool_answers();
    assert_eq!(answers.len(), 1);
    assert!(answers[0].ok);
}

#[tokio::test]
async fn context_device_tool_routes_to_context_op() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        cid(),
        vec![tool_call_frame(
            "context_search",
            0,
            json!({ "query": "roadmap", "limit": 3 }),
        )],
    );
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
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
    assert!(host.tool_calls.lock().unwrap().is_empty());
    assert_eq!(transport.tool_answers().len(), 1);
}

#[tokio::test]
async fn duplicate_frame_is_handled_once() {
    let transport = Arc::new(MockSidecarTransport::new());
    let payload = json!({ "to": "operator", "body": "dup" });
    transport.script_cycle(
        cid(),
        vec![
            effect_frame("send_dm", 0, payload.clone()),
            effect_frame("send_dm", 0, payload),
        ],
    );
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(host.effects.lock().unwrap().len(), 1);
    assert_eq!(transport.acks().len(), 1);
    assert_eq!(result.channel_responses.len(), 1);
}

#[tokio::test]
async fn max_passes_caps_inference_frames() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        cid(),
        vec![
            inference_frame(0, "one"),
            inference_frame(1, "two"),
            inference_frame(2, "three"),
        ],
    );
    let inference = Arc::new(MockInferenceClient::new());
    let brain = brain(transport.clone(), inference.clone()).with_max_passes(2);
    let host = RecordingHost::executing();

    brain.run_cycle(operator_request(), &host).await.unwrap();

    // Only two inference passes ran before the cap stopped the drain.
    assert_eq!(inference.requests().len(), 2);
    assert_eq!(transport.inference_answers().len(), 2);
}

#[tokio::test]
async fn orchestration_error_on_post_events_propagates_with_code() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.fail_post_events(OrchErrorCode::DeviceOffline);
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
    let host = RecordingHost::executing();

    let err = brain
        .run_cycle(operator_request(), &host)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "ORCH_DEVICE_OFFLINE");
}

#[test]
fn debug_does_not_expose_internals_beyond_labels() {
    let brain = brain(
        Arc::new(MockSidecarTransport::new()),
        Arc::new(MockInferenceClient::new()),
    );
    let rendered = format!("{brain:?}");
    assert!(rendered.contains("SidecarBrain"));
    assert!(rendered.contains("acme"));
}

// ---------------------------------------------------------------------------
// End-to-end test through a real CompanyRuntime
// ---------------------------------------------------------------------------

use crate::company::CompanyManifest;
use crate::ports::types::{Actor, ActorKind, Verdict};
use crate::runtime::RuntimeBuilder;

fn tmp_home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "opencompany-sidecar-{}",
        crate::ports::generate_id()
    ))
}

fn manifest(policy_mode: &str) -> CompanyManifest {
    let toml_src = format!(
        r#"
        [company]
        name = "Acme"

        [brain]
        mode = "sidecar"

        [tools]
        allow = ["noop"]

        [policy]
        mode = "{policy_mode}"
        "#
    );
    toml::from_str(&toml_src).expect("valid manifest")
}

fn runtime_cid() -> String {
    wire::cycle_id("opencompany:acme", "acme", 0)
}

fn sidecar_brain_for(
    transport: Arc<MockSidecarTransport>,
    inference: Arc<MockInferenceClient>,
) -> Arc<dyn Brain> {
    Arc::new(SidecarBrain::new(
        transport,
        inference,
        &CompanyId::new("acme"),
        "acme",
        vec![ToolManifestEntry {
            name: "noop".into(),
            description: None,
            input_schema: None,
        }],
    ))
}

#[tokio::test]
async fn e2e_inference_then_gated_send_dm_drives_a_channel_response() {
    let home = tmp_home();
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        runtime_cid(),
        vec![
            inference_frame(0, "how are we doing?"),
            effect_frame("send_dm", 0, json!({ "to": "operator", "body": "on it" })),
        ],
    );
    let inference = Arc::new(
        MockInferenceClient::new()
            .with_text("plan ready")
            .with_tokens(5, 3),
    );

    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_brain(sidecar_brain_for(transport.clone(), inference.clone()))
        .build()
        .await
        .unwrap();

    let report = rt
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "how are we doing".into(),
            by: None,
            chat: None,
        }])
        .await
        .unwrap();

    // The inference callback fired through the real runtime.
    assert_eq!(inference.requests().len(), 1);
    assert_eq!(transport.inference_answers().len(), 1);

    // The gated send_dm produced an operator channel response.
    assert_eq!(report.responses.len(), 1);
    assert_eq!(report.responses[0].channel, "operator");
    assert_eq!(report.responses[0].text, "on it");

    // The effect flowed through the gate and acked ok:true.
    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].ok);

    // A compressed trace was persisted to the fs-backed MemoryStore.
    let traces = rt.memory.recent_traces(rt.id(), 10).await.unwrap();
    assert!(!traces.is_empty());

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn e2e_supervised_effect_parks_through_the_real_gate() {
    let home = tmp_home();
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        runtime_cid(),
        vec![effect_frame("filing.submit", 0, Value::Null)],
    );
    let inference = Arc::new(MockInferenceClient::new());

    let rt = RuntimeBuilder::new(home.clone(), manifest("supervised"))
        .with_brain(sidecar_brain_for(transport.clone(), inference))
        .build()
        .await
        .unwrap();

    let report = rt
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            text: "file it".into(),
            by: None,
            chat: None,
        }])
        .await
        .unwrap();

    // The Sign-group effect parked under supervised policy: an approval is
    // queued and no channel response emitted.
    assert_eq!(report.parked.len(), 1);
    assert_eq!(rt.pending_approvals().len(), 1);
    assert!(report.responses.is_empty());

    // The sidecar was told the effect is pending, not that it succeeded.
    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(!acks[0].ok);

    // Resolving the approval drains the queue.
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
