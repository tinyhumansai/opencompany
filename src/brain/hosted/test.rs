//! Offline tests for [`HostedMedullaBrain`] over the in-memory
//! [`MockTransport`], plus end-to-end tests that drive a real
//! [`CompanyRuntime`](crate::company::runtime::CompanyRuntime) with the brain
//! wired in through the builder.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;
use crate::brain::medulla::MockTransport;
use crate::brain::medulla::wire::{
    self, EffectFrame, OrchErrorCode, Role, ToolCallFrame, UsageFrame,
};
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

/// A usage report for the cycle under test (issue #174), keyed on the same
/// deterministic dedupe id the server would derive.
fn usage_frame(index: usize, input: u64, output: u64, cost_usd: Option<f64>) -> InboundFrame {
    InboundFrame::Usage(UsageFrame {
        cycle_id: cid(),
        call_id: wire::call_id(&cid(), wire::USAGE_CALL_KIND, index),
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: 0,
        cost_usd,
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
async fn request_approval_refuses_later_effect_and_tool_frames() {
    let transport = Arc::new(MockTransport::new());
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
    let brain = brain(transport.clone());
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

// ── Issue #174: usage frames make the hosted path meterable ─────────────────

/// Without this the hosted path is structurally unmetered: the model runs
/// upstream, so a reported frame is the only way the host learns what it cost.
#[tokio::test]
async fn usage_frames_are_folded_into_the_cycle_total() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        cid(),
        vec![
            usage_frame(0, 900, 120, Some(0.014)),
            usage_frame(1, 300, 40, Some(0.006)),
        ],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.token_usage.input, 1_200);
    assert_eq!(result.token_usage.output, 160);
    assert!((result.token_usage.cost_usd - 0.02).abs() < 1e-9);
    // Usage is a report, not a request: nothing is acked or answered for it.
    assert!(transport.acks().is_empty());
    assert!(transport.tool_answers().is_empty());
}

/// Frame delivery is at-least-once, so a replayed usage report must not
/// double-charge the meter.
#[tokio::test]
async fn duplicate_usage_frame_is_counted_once() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        cid(),
        vec![
            usage_frame(0, 500, 50, Some(0.01)),
            usage_frame(0, 500, 50, Some(0.01)),
        ],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.token_usage.input, 500);
    assert_eq!(result.token_usage.output, 50);
    assert_eq!(result.token_usage.cost_usd, 0.01);
}

/// The managed passthrough bills backend-side and echoes no USD. Tokens still
/// count — a `costUsd`-less frame is usage, not noise.
#[tokio::test]
async fn a_usage_frame_without_cost_still_reports_tokens() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(cid(), vec![usage_frame(0, 42, 7, None)]);
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert_eq!(result.token_usage.input, 42);
    assert_eq!(result.token_usage.output, 7);
    assert_eq!(result.token_usage.cost_usd, 0.0);
}

/// A backend that does not emit the frame yet stays compatible: the cycle simply
/// reports zero, which the runtime meters as nothing rather than as a guess.
#[tokio::test]
async fn a_cycle_with_no_usage_frame_reports_zero() {
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        cid(),
        vec![effect_frame("send_dm", 0, json!({ "body": "hi" }))],
    );
    let brain = brain(transport.clone());
    let host = RecordingHost::executing();

    let result = brain.run_cycle(operator_request(), &host).await.unwrap();

    assert!(result.token_usage.is_zero());
}

/// The brain declares where its usage is metered so the console can tell "no
/// inference configured" from "inference ran but nothing was metered".
#[test]
fn cognition_reports_the_hosted_path_and_provider() {
    let cognition = brain(Arc::new(MockTransport::new())).cognition();
    assert_eq!(cognition.path, "hosted");
    assert_eq!(cognition.provider, crate::metering::MEDULLA_PROVIDER);
    assert_eq!(cognition.metering, UsageMetering::PerCycle);
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
use crate::runtime::RuntimeBuilder;

fn tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-hosted-")
        .tempdir()
        .expect("tempdir")
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

/// How many events a fresh company already has in its journal by the time it
/// finishes booting (issue #327).
///
/// Boot lays down the reserved workspace roots, and since #327 the workspace
/// store announces its own writes — one `WorkspaceChanged` per root, plus one
/// per explanatory note the scaffold provisions (`secrets/readme.md` and
/// `artifacts/readme.md`), is journalled before any operator message.
const BOOT_JOURNAL_EVENTS: u64 = crate::company::workspace_scaffold::SYSTEM_ROOTS.len() as u64 + 2;

/// The deterministic first-cycle id a real runtime for `Acme` produces: the
/// company id slugs to `acme`, and the first *cycle* event lands at the first
/// sequence boot did not already use.
fn runtime_cid() -> String {
    wire::cycle_id("opencompany:acme", "acme", BOOT_JOURNAL_EVENTS)
}

#[tokio::test]
async fn e2e_operator_message_drives_tool_call_and_gated_send_dm() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
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
}

#[tokio::test]
async fn e2e_supervised_effect_runs_without_policy_hitl() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        runtime_cid(),
        // Policy HITL is disabled even for a formerly-gated Sign effect.
        vec![effect_frame("filing.submit", 0, Value::Null)],
    );

    let rt = Arc::new(
        RuntimeBuilder::new(home.clone(), manifest("supervised"))
            .with_brain_mode(BrainMode::Hosted)
            .with_credential(SecretValue("th_live".into()))
            .with_transport(transport.clone())
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

    assert!(report.parked.is_empty());
    assert!(rt.pending_approvals().is_empty());
    assert!(report.responses.is_empty());

    // Medulla is told the effect completed instead of waiting on policy HITL.
    let acks = transport.acks();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].ok);
    assert!(acks[0].error.is_none());
}

/// Issue #174 end to end: a real runtime on the hosted brain records the tokens
/// and cost the wire reported, so the console's Usage view stops reading zero.
#[tokio::test]
async fn e2e_reported_usage_lands_on_the_usage_meter() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockTransport::new());
    // `cid()` and `runtime_cid()` are the same deterministic id: company `acme`,
    // first event at seq 0.
    transport.script_cycle(
        runtime_cid(),
        vec![
            usage_frame(0, 1_500, 260, Some(0.042)),
            effect_frame("send_dm", 0, json!({ "body": "on it" })),
        ],
    );

    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
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

    let samples = rt.usage().query(rt.id(), 0).await.unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].input_tokens, 1_500);
    assert_eq!(samples[0].output_tokens, 260);
    assert_eq!(samples[0].cost_usd, 0.042);
    assert_eq!(samples[0].provider, crate::metering::MEDULLA_PROVIDER);
    assert_eq!(
        samples[0].kind,
        crate::ports::usage::SampleKind::Inference,
        "hosted cycles meter as inference, not as an OAuth call"
    );

    // The spend also reaches Finances as an `inference.spend` ledger entry.
    let record = rt.store().load(rt.id()).await.unwrap().unwrap();
    assert!(
        record
            .ledger
            .iter()
            .any(|e| e.kind == crate::metering::INFERENCE_SPEND_KIND && e.amount_usd == -0.042)
    );
}

// ---------------------------------------------------------------------------
// Issue #176: hosted-path delegation (durable async hand-off, no local
// cognition) + handed-task awareness.
// ---------------------------------------------------------------------------

/// A manifest with an Engineering desk (`eng`) whose lead is `eng1`, plus a
/// hosted brain. Used to prove hosted `delegate_to_desk` resolves the desk and
/// records the hand-off against it.
fn desk_manifest() -> CompanyManifest {
    let toml_src = r#"
        [company]
        name = "Acme"

        [brain]
        mode = "hosted"

        [tools]
        allow = ["noop"]

        [policy]
        mode = "full"

        [[agent]]
        id = "chief"
        role = "Chief"
        tier = "orchestrator"

        [[agent]]
        id = "eng1"
        role = "Engineer"

        [[group_chat]]
        id = "eng"
        name = "Engineering"
        members = ["eng1"]
        "#;
    toml::from_str(toml_src).expect("valid manifest")
}

/// The hosted catalog registered with Medulla must advertise the delegation
/// tools on top of the manifest's own `tools.allow`, so a hosted company's
/// orchestrator can actually delegate.
#[tokio::test]
async fn e2e_hosted_catalog_advertises_delegation_tools() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockTransport::new());
    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        // Issue #1725: not "hi". A bare pleasantry is answered by the runtime
        // without reaching a brain, so no catalog would be registered at all.
        text: "ship the landing page".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    }])
    .await
    .unwrap();

    let registered = transport.registered_tools();
    assert_eq!(registered.len(), 1, "tools register exactly once");
    let names: Vec<&str> = registered[0].iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"noop"), "manifest tool kept: {names:?}");
    assert!(
        names.contains(&"spawn_task"),
        "spawn_task advertised: {names:?}"
    );
    assert!(
        names.contains(&"delegate_to_desk"),
        "delegate_to_desk advertised: {names:?}"
    );
}

/// Medulla emitting a `spawn_task` tool-call on the hosted path opens a durable
/// board card device-side and answers ok — no local cognition needed.
#[tokio::test]
async fn e2e_spawn_task_tool_call_opens_a_board_card() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        runtime_cid(),
        vec![tool_call_frame(
            "spawn_task",
            0,
            json!({ "title": "Ship the invoice flow", "assignee": "eng" }),
        )],
    );

    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "open a task to ship invoicing".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    }])
    .await
    .unwrap();

    // The tool was answered ok.
    let answers = transport.tool_answers();
    assert_eq!(answers.len(), 1);
    assert!(answers[0].ok, "spawn_task answered ok: {:?}", answers[0]);

    // A durable card landed on the board.
    let cards = rt.tasks().list(rt.id()).await.unwrap();
    assert_eq!(cards.len(), 1, "one card opened: {cards:?}");
    assert_eq!(cards[0].title, "Ship the invoice flow");
    assert_eq!(cards[0].assignee, "eng");
    assert_eq!(cards[0].column, "todo");
}

/// Medulla emitting a `delegate_to_desk` tool-call resolves the desk and records
/// a durable hand-off card assigned to that desk (so it surfaces when the desk
/// is asked directly). An unknown desk is a clean tool error, not a lost card.
#[tokio::test]
async fn e2e_delegate_to_desk_tool_call_writes_a_handoff_card() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        runtime_cid(),
        vec![
            tool_call_frame(
                "delegate_to_desk",
                0,
                json!({ "desk": "Engineering", "instruction": "build the invoice importer" }),
            ),
            tool_call_frame(
                "delegate_to_desk",
                1,
                json!({ "desk": "Nonexistent", "instruction": "do a thing" }),
            ),
        ],
    );

    let rt = RuntimeBuilder::new(home.clone(), desk_manifest())
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "have engineering build invoicing".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    }])
    .await
    .unwrap();

    let answers = transport.tool_answers();
    assert_eq!(answers.len(), 2);
    // First hand-off resolved the desk by name and succeeded.
    assert!(answers[0].ok, "known desk hands off ok: {:?}", answers[0]);
    // Unknown desk answered ok:false (clean error) and wrote no card.
    assert!(
        !answers[1].ok,
        "unknown desk is a clean error: {:?}",
        answers[1]
    );

    let cards = rt.tasks().list(rt.id()).await.unwrap();
    assert_eq!(cards.len(), 1, "only the known desk got a card: {cards:?}");
    // Assigned to the resolved desk id, with the lead recorded in the note.
    assert_eq!(cards[0].assignee, "eng");
    let note = cards[0].note.as_deref().unwrap_or_default();
    assert!(note.contains("eng1"), "note records the lead: {note}");
    assert!(
        note.contains("build the invoice importer"),
        "note carries the instruction"
    );
}

/// The same company with no usage frame on the wire: an honest zero, and no
/// fabricated sample.
#[tokio::test]
async fn e2e_a_cycle_without_usage_frames_meters_nothing() {
    let home_dir = tmp_home();
    let home = home_dir.path().to_path_buf();
    let transport = Arc::new(MockTransport::new());
    transport.script_cycle(
        runtime_cid(),
        vec![effect_frame("send_dm", 0, json!({ "body": "on it" }))],
    );

    let rt = RuntimeBuilder::new(home.clone(), manifest("full"))
        .with_brain_mode(BrainMode::Hosted)
        .with_credential(SecretValue("th_live".into()))
        .with_transport(transport.clone())
        .build()
        .await
        .unwrap();

    rt.run_cycle(vec![CompanyEvent::OperatorMessage {
        mentions: Vec::new(),
        parent: None,
        text: "hello".into(),
        by: None,
        chat: None,
        deliverable: None,
        attachments: Vec::new(),
    }])
    .await
    .unwrap();

    assert!(rt.usage().query(rt.id(), 0).await.unwrap().is_empty());
}
