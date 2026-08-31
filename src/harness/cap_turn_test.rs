//! Issue #926: end-to-end proof that a turn which runs out of tool iterations
//! **says so** — and that the way it says so cannot poison the next turn.
//!
//! The bug this pins is a silence, which is why nothing shorter than a real
//! turn loop can show it. Hitting the cap is not an error and never surfaces as
//! one: openhuman stops the tool loop, makes one extra tools-disabled call
//! asking the model for a resumable "Done so far / Next steps" checkpoint, and
//! returns that as an ordinary `Ok(reply)`. So the operator receives a tidy
//! plan, no deliverable, and nothing anywhere saying the turn was cut off —
//! reported by QA as agents "getting permanently stuck mid-task".
//!
//! [`MockProvider`](crate::harness::provider::MockProvider) cannot reach this:
//! it never issues a tool call, so it can never reach an iteration cap. The
//! lever is the same loopback OpenAI-compatible endpoint
//! [`publish_turn_test`](crate::harness::publish_turn_test) uses — a real
//! [`HostedProvider`], real `build_agent`, real tool dispatch, real pool, real
//! brain. Only the model's *choices* are scripted.
//!
//! What each test is for:
//!
//! - the flag is `true` on a capped turn, and — the negative control that stops
//!   a hardcoded `true` passing — `false` on a turn that simply finishes;
//! - a capped turn is still `Ok` with a non-empty reply, pinning "a cap is a
//!   pause, not an error" so a future reader does not go looking for an error
//!   arm to match on;
//! - the brain emits the pause as a **second** bubble;
//! - and the memory the turn writes back carries the agent's checkpoint but
//!   **not** the platform's notice.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::CompanyManifest;
use crate::company::credentials::Credential;
use crate::harness::brain::ITERATION_CAP_PAUSE_NOTICE;
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::memory_loop;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::{HarnessBrain, HarnessDeps, HarnessPool};
use crate::ports::ContextStore;
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::types::{
    ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult, CycleRequest,
    Effect, EffectDisposition, OutboundMessage, ToolCall, ToolResult,
};
use crate::store::{FsCompanyStore, FsContextStore, FsOps};

/// The agent every test here talks to.
const AGENT: &str = "ceo";

/// The tool-iteration cap a turn actually runs under.
///
/// `build_agent` states this explicitly via `set_max_tool_iterations`
/// (issue #988) rather than falling back to openhuman's
/// `AgentConfig::default()` (`config/schema/agent.rs`,
/// `default_agent_max_tool_iterations`, which is 10 and no longer what any
/// built agent runs under).
///
/// Bound to [`MAX_TOOL_ITERATIONS`](crate::harness::build::MAX_TOOL_ITERATIONS)
/// rather than re-hardcoded, and it is checked, not assumed: the scripts below
/// depend on knowing exactly which model call is the wrap-up, and
/// [`capped_script`] asserts the observed call count matches. If a vendor bump
/// or another `set_max_tool_iterations` call moves the effective cap without
/// moving the constant, these tests fail with a message that says the cap
/// moved — rather than silently scripting the wrong turn shape and passing for
/// the wrong reason.
const CAP: usize = crate::harness::build::MAX_TOOL_ITERATIONS;

/// The checkpoint the scripted model writes when it is asked to wrap up.
///
/// Distinctive so the memory assertion can look for the agent's words rather
/// than for "some text landed".
const CHECKPOINT: &str = "Done so far: read the standards and the prior spec. \
Next steps: draft the spec and publish it.";

/// A slice of [`ITERATION_CAP_PAUSE_NOTICE`] unique to the platform's voice.
///
/// Used to prove the notice is *absent* from memory. A substring rather than
/// the whole notice because an absence assertion on the full string would pass
/// the moment the wording changed by a comma.
const NOTICE_MARKER: &str = "pause, not a finished answer";

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

/// What the scripted model does on each successive call.
#[derive(Clone, Debug)]
enum Turn {
    /// Emit a native tool call with these literal arguments.
    Call { tool: String, args: Value },
    /// Finish with plain assistant text.
    Say(String),
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
struct Script {
    turns: Mutex<Vec<Turn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: Mutex<Vec<Value>>,
}

impl Script {
    /// How many model calls the turn actually made.
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// Whether the wrap-up call was made with tools **withheld** — the tell that
    /// openhuman took the cap branch rather than simply running out of script.
    fn last_call_had_tools(&self) -> bool {
        self.seen
            .lock()
            .unwrap()
            .last()
            .and_then(|body| body.get("tools").and_then(Value::as_array).cloned())
            .is_some_and(|tools| !tools.is_empty())
    }
}

/// Serve the script on loopback and return its base URL plus the shared handle.
async fn spawn_script(turns: Vec<Turn>) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        turns: Mutex::new(turns),
        seen: Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&script);
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let script = Arc::clone(&handle);
            async move {
                script.seen.lock().unwrap().push(body.clone());
                let next = {
                    let mut turns = script.turns.lock().unwrap();
                    if turns.is_empty() {
                        None
                    } else {
                        Some(turns.remove(0))
                    }
                };
                // Running off the end means the turn looped more than the
                // script expected. End it with text rather than hanging — the
                // call-count assertions are what report the mismatch.
                let next = next.unwrap_or_else(|| Turn::Say("done".to_string()));
                let message = match next {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(&tool, &args),
                };
                (
                    axum::http::StatusCode::OK,
                    Json(json!({
                        "choices": [{ "index": 0, "message": message }],
                        "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), script)
}

/// One assistant message carrying a native `tool_calls` array — the shape the
/// provider's `tool_calling: true` profile puts the turn loop on.
fn tool_call_message(tool: &str, args: &Value) -> Value {
    json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": format!("call-{tool}"),
            "type": "function",
            "function": { "name": tool, "arguments": args.to_string() }
        }]
    })
}

/// One successful `file_write`.
///
/// A **distinct path and body per step** on purpose: the tool has to succeed
/// every time, or openhuman's repeated-failure breaker halts the run — and a
/// breaker halt is explicitly *not* a cap hit, so the test would prove nothing.
/// Distinct arguments also keep any no-progress heuristic from reading ten
/// identical calls as a loop.
fn step(n: usize) -> Turn {
    Turn::Call {
        tool: "file_write".to_string(),
        args: json!({ "path": format!("step-{n}.md"), "content": format!("step {n}") }),
    }
}

/// A script that reaches the cap: exactly [`CAP`] tool round trips, then the
/// checkpoint the tools-disabled wrap-up call asks for.
///
/// The wrap-up is what makes the `Say` land where it does — at call `CAP + 1`,
/// after the loop has already paused.
fn capped_script() -> Vec<Turn> {
    let mut turns: Vec<Turn> = (1..=CAP).map(step).collect();
    turns.push(Turn::Say(CHECKPOINT.to_string()));
    turns
}

/// A script that finishes normally, well inside the cap — the negative control.
fn uncapped_script() -> Vec<Turn> {
    vec![step(1), step(2), Turn::Say(CHECKPOINT.to_string())]
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// An inert `CycleHost` — these tests are about the turn, not the effect gate.
struct NoopHost;

#[async_trait]
impl CycleHost for NoopHost {
    async fn call_tool(&self, _call: ToolCall) -> crate::Result<ToolResult> {
        Ok(ToolResult {
            ok: true,
            output: Value::Null,
        })
    }
    async fn context_op(&self, _op: ContextOp) -> crate::Result<ContextOpResult> {
        Ok(ContextOpResult::Text(String::new()))
    }
    async fn emit_effect(&self, _effect: Effect) -> crate::Result<EffectDisposition> {
        Ok(EffectDisposition::Executed)
    }
    async fn park_effect(&self, _effect: Effect) -> crate::Result<ApprovalId> {
        Ok(ApprovalId::new("appr-parked"))
    }
}

fn company() -> CompanyId {
    CompanyId::new("acme")
}

/// A one-agent company on `full` policy, so an ordinary turn is not parked for
/// approval — the gate under test is the iteration cap, not the approval one.
fn manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["*"]

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"
"#,
    )
    .expect("manifest parses")
}

fn record() -> CompanyRecord {
    CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: company(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// Real deps pointed at the scripted endpoint, with a workspace on disk so
/// `file_write` genuinely succeeds.
fn deps_for(base_url: String, dir: &std::path::Path) -> (HarnessDeps, Arc<FsOps>) {
    let ops = Arc::new(FsOps::new(dir));
    let deps = HarnessDeps {
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(HostedProvider::new(HostedProviderConfig {
            base_url,
            credential: Credential::from_value("stub-key"),
            extra_headers: Vec::new(),
        })),
        provider_slug: "managed".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: Some(ops.clone()),
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: Some("stub-model".to_string()),
        tasks: Some(ops.clone()),
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        skills_registry: Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: DelegationQueue::default(),
        workflow_runner: WorkflowRunnerHandle::default(),
        mcp_failures: McpFailureQueue::default(),
        pending_publishes: Default::default(),
        workflow_refs: Default::default(),
        run_outputs: Default::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: ApprovalRequestQueue::default(),
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
        workflow_source_dir: None,
        plan: None,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        workspace: None,
        search: None,
        tenant_search: None,
        workflow_runs: None,
        deep_trace: None,
    };
    (deps, ops)
}

/// An operator message, the shape a chat turn arrives as.
fn chat(text: &str) -> CycleRequest {
    CycleRequest {
        cycle_id: "cycle-1".to_string(),
        company_id: company(),
        events: vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: text.to_string(),
            by: None,
            chat: None,
            parent: None,
            deliverable: None,
            attachments: Vec::new(),
        }],
        event_seqs: Vec::new(),
        policy: None,
    }
}

/// Everything the turn wrote back to memory, newest-agnostic.
async fn memory_bodies(context: &FsContextStore) -> Vec<String> {
    let metas = context
        .list(&company(), memory_loop::OUTCOME_LABEL_PREFIX)
        .await
        .expect("list memory");
    let mut bodies = Vec::new();
    for meta in metas {
        bodies.push(
            context
                .peek(&company(), &meta.addr, None)
                .await
                .expect("peek memory"),
        );
    }
    bodies
}

/// The operator-channel bubbles a cycle produced.
fn operator_bubbles(responses: &[OutboundMessage]) -> Vec<&OutboundMessage> {
    responses
        .iter()
        .filter(|m| m.channel == "operator")
        .collect()
}

// ---------------------------------------------------------------------------
// The flag
// ---------------------------------------------------------------------------

/// **The headline.** A turn that spends its whole iteration budget on tool
/// calls reports that it paused — and still comes back `Ok` with the
/// checkpoint, because a cap is a pause and not an error.
///
/// Three assertions in one test on purpose: they are three readings of the same
/// single turn, and splitting them would run the same eleven-call loop three
/// times to learn nothing more.
#[tokio::test]
async fn a_turn_that_exhausts_its_iteration_budget_reports_the_pause() {
    let (base_url, script) = spawn_script(capped_script()).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, _ops) = deps_for(base_url, dir.path());
    let rec = record();
    let pool = HarnessPool::new();
    pool.ensure(&rec, &deps).await.expect("pool ensures");

    let outcome = pool
        .run(
            &rec.id,
            AGENT,
            "Write a short feature spec.",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("a cap is a pause, not an error — the turn must return Ok");

    // 1. The flag the whole feature hangs on.
    assert!(
        outcome.hit_iteration_cap,
        "a turn that spent all {CAP} iterations on tool calls must report the cap"
    );

    // 2. It is still an ordinary, useful reply.
    assert!(
        !outcome.reply.trim().is_empty(),
        "a capped turn still carries the checkpoint openhuman asked the model for"
    );
    assert_eq!(
        outcome.reply.trim(),
        CHECKPOINT,
        "the reply is the model's checkpoint, verbatim"
    );

    // 3. The turn shape really was the cap branch, not the script running dry.
    //    `CAP` tool round trips plus exactly one tools-disabled wrap-up.
    assert_eq!(
        script.calls(),
        CAP + 1,
        "expected {CAP} tool iterations plus one wrap-up call; if this moved, so did \
         openhuman's max_tool_iterations default and `CAP` needs updating"
    );
    assert!(
        !script.last_call_had_tools(),
        "the wrap-up call must withhold tools — that is what makes it a checkpoint \
         request rather than another iteration"
    );
}

/// **The negative control.** The same fixture, a turn that finishes on its own,
/// and the flag stays `false`.
///
/// Without this, `hit_iteration_cap: true` hardcoded at the read site would
/// pass every other test in this file.
#[tokio::test]
async fn a_turn_that_finishes_on_its_own_reports_no_pause() {
    let (base_url, script) = spawn_script(uncapped_script()).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, _ops) = deps_for(base_url, dir.path());
    let rec = record();
    let pool = HarnessPool::new();
    pool.ensure(&rec, &deps).await.expect("pool ensures");

    let outcome = pool
        .run(
            &rec.id,
            AGENT,
            "Write a short feature spec.",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn runs");

    assert!(
        !outcome.hit_iteration_cap,
        "two tool calls is well inside the cap — reporting a pause here would put the \
         notice on turns that finished perfectly well"
    );
    assert_eq!(outcome.reply.trim(), CHECKPOINT);
    assert!(
        script.calls() < CAP,
        "the control must finish inside the cap to be a control at all: {} calls",
        script.calls()
    );
}

// ---------------------------------------------------------------------------
// What the operator sees
// ---------------------------------------------------------------------------

/// A capped chat turn reaches the operator as **two** bubbles: the agent's
/// checkpoint, then the system saying it was cut off.
///
/// The second bubble is unauthored — no teammate said it — and carries no
/// steps, because the timeline already rode in on the first.
#[tokio::test]
async fn a_capped_chat_turn_says_so_in_a_second_bubble() {
    let (base_url, _script) = spawn_script(capped_script()).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops);

    let result = brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");

    let bubbles = operator_bubbles(&result.channel_responses);
    assert_eq!(
        bubbles.len(),
        2,
        "a capped turn owes the operator the reply AND the pause notice: {:?}",
        bubbles.iter().map(|b| &b.text).collect::<Vec<_>>()
    );

    // The agent's answer, attributed to the agent.
    assert_eq!(bubbles[0].text.trim(), CHECKPOINT);
    assert_eq!(bubbles[0].agent.as_deref(), Some(AGENT));

    // The system's notice, attributed to nobody.
    assert_eq!(bubbles[1].text, ITERATION_CAP_PAUSE_NOTICE);
    assert!(
        bubbles[1].agent.is_none(),
        "the platform's words must not be put in the agent's mouth"
    );
    assert!(
        bubbles[1].steps.is_empty(),
        "the timeline is already on the reply bubble; repeating it doubles every row"
    );

    // It has to actually say the three things the operator needs.
    let notice = ITERATION_CAP_PAUSE_NOTICE;
    assert!(notice.contains("pause"), "it must name the pause: {notice}");
    assert!(
        notice.contains("Nothing errored"),
        "it must say nothing failed, or a budget reads as a crash: {notice}"
    );
    assert!(
        notice.contains("continue"),
        "it must tell the operator how to carry on: {notice}"
    );
}

/// The same cycle, uncapped: exactly **one** bubble.
///
/// The pair is the real assertion — a notice that fires on every turn is as
/// useless as one that never fires.
#[tokio::test]
async fn an_uncapped_chat_turn_says_nothing_extra() {
    let (base_url, _script) = spawn_script(uncapped_script()).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops);

    let result = brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");

    let bubbles = operator_bubbles(&result.channel_responses);
    assert_eq!(
        bubbles.len(),
        1,
        "a turn that finished owes no pause notice: {:?}",
        bubbles.iter().map(|b| &b.text).collect::<Vec<_>>()
    );
    assert!(
        !bubbles[0].text.contains(NOTICE_MARKER),
        "and it must not be smuggled into the reply either"
    );
}

// ---------------------------------------------------------------------------
// What memory keeps
// ---------------------------------------------------------------------------

/// The turn's memory write carries the agent's checkpoint and **not** the
/// platform's notice.
///
/// This is why the notice is a sibling bubble rather than text appended to the
/// reply. `HarnessPool::run` persists `outcome.reply` to the context store, so
/// appending would file "you hit the step limit" as something the agent said —
/// and the memory loop would recall it into a later turn as prior work.
#[tokio::test]
async fn the_pause_notice_never_reaches_memory() {
    let (base_url, _script) = spawn_script(capped_script()).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let context = FsContextStore::new(dir.path());
    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops);

    brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");

    let bodies = memory_bodies(&context).await;
    assert!(
        !bodies.is_empty(),
        "the turn must have written its outcome back, or this proves nothing"
    );
    assert!(
        bodies.iter().any(|b| b.contains(CHECKPOINT)),
        "the agent's checkpoint is what compounds into later turns: {bodies:?}"
    );
    assert!(
        bodies.iter().all(|b| !b.contains(NOTICE_MARKER)),
        "the platform's pause notice must never be recalled as something the agent \
         said: {bodies:?}"
    );
}
