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

    async fn park_effect(&self, effect: Effect) -> Result<ApprovalId> {
        self.effects.lock().unwrap().push(effect);
        Ok(ApprovalId::new("appr-parked"))
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
            mentions: Vec::new(),
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }],
        event_seqs: Vec::new(),
        policy: None,
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

/// `with_cost` and `with_tokens` must compose in either order. `with_tokens`
/// used to replace the whole `TokenUsage`, so cost-first ordering silently reset
/// `cost_usd` to zero and any fixture written that way metered a free cycle —
/// the exact under-reporting issue #174 is about. The tokens-first ordering is
/// already covered by `cycle_folds_multi_pass_usage`; this pins the reverse.
#[tokio::test]
async fn mock_inference_cost_survives_a_later_with_tokens() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(cid(), vec![inference_frame(0, "what next?")]);
    let inference = Arc::new(
        MockInferenceClient::new()
            .with_text("done")
            // Cost first — the ordering that used to lose the cost.
            .with_cost(0.005)
            .with_tokens(10, 4),
    );
    let brain = brain(transport.clone(), inference.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.token_usage.input, 10);
    assert_eq!(result.token_usage.output, 4);
    assert!(
        (result.token_usage.cost_usd - 0.005).abs() < 1e-9,
        "with_tokens must not clear a cost set by with_cost, got {}",
        result.token_usage.cost_usd
    );
}

/// Issue #174: the host's inference client is the only side that sees what a pass
/// cost, so the cycle total must carry the cost too — that is what the runtime
/// meters onto the Usage and Finances surfaces.
#[tokio::test]
async fn cycle_usage_carries_the_cost_of_every_pass() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        cid(),
        vec![inference_frame(0, "first"), inference_frame(1, "second")],
    );
    let inference = Arc::new(
        MockInferenceClient::new()
            .with_text("done")
            .with_tokens(10, 4)
            .with_cost(0.005),
    );
    let brain = brain(transport.clone(), inference.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    // Two passes fold into one cycle total.
    assert_eq!(result.token_usage.input, 20);
    assert_eq!(result.token_usage.output, 8);
    assert!((result.token_usage.cost_usd - 0.01).abs() < 1e-9);
    assert!(!result.token_usage.is_zero());

    // The sidecar reports usage per cycle; the provider that served it belongs to
    // the host's client, not the brain.
    let cognition = brain.cognition();
    assert_eq!(cognition.path, "sidecar");
    assert_eq!(cognition.metering, UsageMetering::PerCycle);
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
async fn request_approval_refuses_later_effect_and_tool_frames() {
    let transport = Arc::new(MockSidecarTransport::new());
    transport.script_cycle(
        cid(),
        vec![
            tool_call_frame(
                crate::ports::types::REQUEST_APPROVAL_EFFECT_KIND,
                0,
                json!({ "title": "Send the message", "question": "May I send it?" }),
            ),
            effect_frame(
                "send_dm",
                1,
                json!({ "to": "operator", "body": "too early" }),
            ),
            tool_call_frame("noop", 2, json!({ "too": "late" })),
        ],
    );
    let brain = brain(transport.clone(), Arc::new(MockInferenceClient::new()));
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert!(result.channel_responses.is_empty());
    assert!(host.effects.lock().unwrap().is_empty());
    assert_eq!(host.tool_calls.lock().unwrap().len(), 1);
    assert_eq!(transport.acks().len(), 1);
    assert!(!transport.acks()[0].ok);
    assert_eq!(transport.tool_answers().len(), 2);
    assert!(transport.tool_answers()[0].ok);
    assert!(!transport.tool_answers()[1].ok);
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
use crate::runtime::RuntimeBuilder;

fn tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-sidecar-")
        .tempdir()
        .expect("tempdir")
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
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockSidecarTransport::new());
    // Scripted for whichever cycle the runtime opens, not a hand-computed id.
    // Pinning the id here coupled this test to the event seq the runtime
    // assigns; when that moved, the brain drained an empty cycle and every
    // assertion below failed against a plausible zero (issue #800).
    transport.script_any_cycle(vec![
        inference_frame(0, "how are we doing?"),
        effect_frame("send_dm", 0, json!({ "to": "operator", "body": "on it" })),
    ]);
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
            mentions: Vec::new(),
            parent: None,
            text: "how are we doing".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

    // The runtime's cycle was actually found. Asserted before the effect
    // assertions because an unmatched cycle makes every one of them fail with
    // a zero that looks like a broken effect rather than a missed plan.
    assert!(
        transport.unmatched_cycles().is_empty(),
        "the brain opened a cycle nothing scripted: {:?}",
        transport.unmatched_cycles()
    );

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
}

#[tokio::test]
async fn e2e_supervised_effect_runs_without_policy_hitl() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockSidecarTransport::new());
    // Whichever cycle the runtime opens — see the sibling test (issue #800).
    transport.script_any_cycle(vec![effect_frame("filing.submit", 0, Value::Null)]);
    let inference = Arc::new(MockInferenceClient::new());

    let rt = Arc::new(
        RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain(sidecar_brain_for(transport.clone(), inference))
            .build()
            .await
            .unwrap(),
    );

    let report = rt
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "file it".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .unwrap();

    assert!(
        transport.unmatched_cycles().is_empty(),
        "the brain opened a cycle nothing scripted: {:?}",
        transport.unmatched_cycles()
    );

    // Policy HITL is disabled on the production runtime gate. The Sign-group
    // effect executes immediately even under supervised mode; only an explicit
    // approval-producing tool creates a card.
    assert!(report.parked.is_empty());
    assert!(rt.pending_approvals().is_empty());
    assert_eq!(report.executed_effects.len(), 1);
    assert!(report.responses.is_empty());

    // The sidecar is told the effect succeeded rather than receiving a pending
    // approval disposition.
    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].ok);
}

/// The trap that hid #800 for as long as the sidecar lane went unrun.
///
/// An unscripted cycle drains empty, which is a legitimate case — but it is
/// indistinguishable from a fixture whose cycle id disagrees with the
/// runtime's. Every downstream assertion then fails as "expected 1, saw 0" and
/// points at the effect rather than at the miss. The mock records the id so a
/// test can tell the two apart, and this pins that it does.
#[tokio::test]
async fn an_unscripted_cycle_is_recorded_as_a_miss() {
    let transport = MockSidecarTransport::new();

    // Nothing scripted at all: the miss is recorded.
    let mut frames = transport.cycle_frames("cyc:nobody:scripted:this:7");
    while frames.next().await.is_some() {}
    assert_eq!(
        transport.unmatched_cycles(),
        vec!["cyc:nobody:scripted:this:7"]
    );

    // A scripted fallback answers the next cycle, and is NOT a miss.
    let transport = MockSidecarTransport::new();
    transport.script_any_cycle(vec![effect_frame("noop", 0, Value::Null)]);
    let mut frames = transport.cycle_frames("cyc:whatever:the:runtime:1");
    while frames.next().await.is_some() {}
    assert!(transport.unmatched_cycles().is_empty());

    // It is one-shot: the cycle after it drains empty, and is still not a miss,
    // because a plan *was* scripted — a replay would re-emit the effect into a
    // later cycle (the approval-resolution one), which is its own defect.
    let mut frames = transport.cycle_frames("cyc:whatever:the:runtime:2");
    let mut count = 0usize;
    while let Some(frame) = frames.next().await {
        if !matches!(frame.unwrap(), SidecarFrame::CycleComplete) {
            count += 1;
        }
    }
    assert_eq!(count, 0, "the fallback replayed into a later cycle");
    assert!(transport.unmatched_cycles().is_empty());
}
