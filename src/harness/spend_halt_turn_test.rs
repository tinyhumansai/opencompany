//! Issue #1032: end-to-end proof that a turn stopped by its in-turn spend brake
//! **says so** — and that it says something different from a turn that paused at
//! its step cap.
//!
//! The bug is a silence, and a silence built in by construction rather than
//! forgotten. #988 armed the brake: a teammate that declares a
//! `budget_usd_daily` gets a [`BudgetStopHook`] that halts a turn outrunning it.
//! But openhuman's tool loop consumes the hook's `StopDecision::Stop { reason }`
//! internally, stops iterating, and returns the run's text as an ordinary
//! `Ok(reply)` — so "I ran out of budget" and "I finished the work" arrive at the
//! operator as the same bubble. `Agent::last_turn_hit_cap()` cannot stand in
//! either: it is `false` for a spend halt, which is exactly what #988 pinned.
//!
//! Nothing shorter than a real turn can show this.
//! [`MockProvider`](crate::harness::provider::MockProvider) issues no tool
//! calls, so a turn against it is one iteration long and no between-iteration
//! hook ever fires. So this drives the **real** harness — real `build_agent`,
//! real [`HostedProvider`], real pool, real brain — and scripts exactly one
//! thing: the model's choices, over a loopback OpenAI-compatible endpoint. The
//! shape [`cap_turn_test`](super::cap_turn_test) and
//! `built_in::iteration_cap_turn_test` established.
//!
//! The lever that makes the brake fire is `prompt_tokens`: the stop-hook
//! middleware folds the usage the *provider* reports into openhuman's turn cost,
//! so a script that reports a million prompt tokens crosses a five-cent cap on
//! its first iteration.
//!
//! What each test is for:
//!
//! - the halt reaches [`TurnOutcome::halted_for_spend`] with the right figures,
//!   and — the negative control that stops a hardcoded `Some` passing — a turn
//!   that finishes reports `None`;
//! - a teammate that declared no budget can never report one, because no hook is
//!   installed for it;
//! - the brain emits the halt as a **second, unauthored** bubble;
//! - the spend notice and the step-cap notice do **not** cross-fire, asserted in
//!   both directions;
//! - and the notice never reaches memory, for the reason #926 established.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::CompanyManifest;
use crate::company::credentials::Credential;
use crate::harness::brain::{ITERATION_CAP_PAUSE_NOTICE, spend_halt_notice};
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

/// The declared daily cap the in-turn brake arms at, in USD.
///
/// Small enough that one scripted call crosses it, and quoted in the notice —
/// so the assertions below can look for `$0.05` and know where it came from.
const CAP_USD: f64 = 0.05;

/// `prompt_tokens` a *cheap* call reports — well inside [`CAP_USD`], so a turn
/// scripted with it finishes on its own.
const CHEAP_TOKENS: u64 = 12;

/// `prompt_tokens` an *expensive* call reports. On the `chat-v1` tier this
/// estimates to roughly $0.14, so the very first iteration crosses
/// [`CAP_USD`] and the brake halts the turn.
const EXPENSIVE_TOKENS: u64 = 1_000_000;

/// The tool-iteration cap a turn actually runs under, bound to the constant
/// rather than re-hardcoded so a vendor bump cannot quietly weaken the
/// cross-fire test below into scripting the wrong turn shape.
const CAP: usize = crate::harness::build::MAX_TOOL_ITERATIONS;

/// The answer the scripted model gives when it is allowed to finish.
const ANSWER: &str = "Spec published.";

/// A slice of the spend notice unique to the platform's voice, for proving the
/// notice is *absent* — from memory, and from a turn that finished.
///
/// A substring rather than the whole notice: an absence assertion on the full
/// string would pass the moment the wording changed by a comma.
const SPEND_MARKER: &str = "reached its spend cap partway through";

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
    seen: Mutex<Vec<Value>>,
    /// `prompt_tokens` echoed on every response — the knob that decides whether
    /// the budget hook fires, because the stop-hook middleware folds it into
    /// openhuman's turn cost.
    prompt_tokens: u64,
}

impl Script {
    /// How many model calls the turn actually made.
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
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

/// Serve the script on loopback and return its base URL plus the shared handle.
async fn spawn_script(turns: Vec<Turn>, prompt_tokens: u64) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        turns: Mutex::new(turns),
        seen: Mutex::new(Vec::new()),
        prompt_tokens,
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
                // Running off the end means the turn looped more than the script
                // expected. End it with text rather than hanging — the
                // call-count assertions are what report the mismatch.
                let next = next.unwrap_or_else(|| Turn::Say("ran off the script".to_string()));
                let message = match next {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(&tool, &args),
                };
                (
                    axum::http::StatusCode::OK,
                    Json(json!({
                        "choices": [{ "index": 0, "message": message }],
                        "usage": {
                            "prompt_tokens": script.prompt_tokens,
                            "completion_tokens": 4
                        }
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

/// A script that writes `n` distinct files and then answers.
///
/// **Distinct paths and bodies** on purpose: an identical successful tool batch
/// reissued back to back is what openhuman's repeat-progress guard halts on, and
/// a run stopped by *that* would prove nothing about the brake under test. The
/// writes must also succeed every time, or the repeated-failure breaker halts
/// the run for a third unrelated reason.
fn write_then_answer(n: usize) -> Vec<Turn> {
    let mut turns: Vec<Turn> = (1..=n)
        .map(|i| Turn::Call {
            tool: "file_write".to_string(),
            args: json!({ "path": format!("step-{i}.md"), "content": format!("step {i}") }),
        })
        .collect();
    turns.push(Turn::Say(ANSWER.to_string()));
    turns
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
/// approval — the gate under test is the spend brake, not the approval one.
///
/// `budget` is the teammate's declared `budget_usd_daily`. `None` renders the
/// key away entirely, which is the state in which #988 arms no hook at all —
/// the negative control this file needs, and not something a zero could stand
/// in for (a zero is a *malformed* cap, which `turn_spend_cap_usd` also ignores,
/// for a different reason).
fn manifest(budget: Option<f64>) -> CompanyManifest {
    let budget_line = match budget {
        Some(usd) => format!("budget_usd_daily = {usd}\n"),
        None => String::new(),
    };
    toml::from_str(&format!(
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
{budget_line}"#
    ))
    .expect("manifest parses")
}

fn record(budget: Option<f64>) -> CompanyRecord {
    CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: company(),
        manifest: manifest(budget),
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
///
/// `meter: Some(ops)` on purpose. The **pre-dispatch** daily-spend gate reads
/// it, and with a fresh store it reports zero spend — so the turn is dispatched
/// and the *in-turn* brake is the thing that stops it. A `None` meter would let
/// the turn run too, but for the wrong reason (that gate fails open), and the
/// test would no longer distinguish the two controls it exists to separate.
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
        // Left unset so the turn runs on the tier the manifest resolves
        // (`chat-v1`), which is a *priced* row in openhuman's tier table. The
        // budget hook reads an estimate off that table when the backend echoes
        // no charged amount, so pinning a made-up model name here would make the
        // spend figure depend on a pricing fallback instead of a stated rate.
        model_override: None,
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

/// The operator-channel bubbles a cycle produced.
fn operator_bubbles(responses: &[OutboundMessage]) -> Vec<&OutboundMessage> {
    responses
        .iter()
        .filter(|m| m.channel == "operator")
        .collect()
}

/// Everything the turn wrote back to memory.
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

// ---------------------------------------------------------------------------
// The flag
// ---------------------------------------------------------------------------

/// **The headline.** A turn halted by the in-turn spend brake reports the halt,
/// with the figures the operator needs, and still comes back `Ok` — because a
/// halt is a stop, not an error.
#[tokio::test]
async fn a_turn_halted_for_spend_reports_the_halt_and_its_figures() {
    let (base_url, script) = spawn_script(write_then_answer(CAP), EXPENSIVE_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, _ops) = deps_for(base_url, dir.path());
    let rec = record(Some(CAP_USD));
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
        .expect("a spend halt is a stop, not an error — the turn must return Ok");

    let halt = outcome
        .halted_for_spend
        .as_ref()
        .expect("the brake fired, so the turn must say so — that is the whole of #1032");
    assert_eq!(
        halt.agent, AGENT,
        "the halt names the teammate whose cap was reached"
    );
    assert_eq!(halt.cap_usd, CAP_USD, "measured against the declared cap");
    assert!(
        halt.spent_usd > 0.0,
        "the turn made a paid model call, so the spend cannot be zero: {}",
        halt.spent_usd
    );

    // It really was halted, not merely finished: the script offered `CAP` tool
    // rounds and the turn spent a small handful. (Left loose because the
    // vendored turn adds a wrap-up call after a partial run, and that call is
    // not the thing under test.)
    assert!(
        script.calls() < CAP,
        "expected the brake to cut the turn well short of its {CAP} scripted rounds, got {}",
        script.calls()
    );
    // And it is NOT an iteration-cap pause. #988 pinned this; the two notices
    // must never be interchangeable, because the operator's next move differs.
    assert!(
        !outcome.hit_iteration_cap,
        "a spend halt must not also report a step pause — they are different outcomes"
    );
}

/// **The negative control.** The same declared cap, a turn cheap enough to
/// finish, and the halt stays `None`.
///
/// Without this, `halted_for_spend` wired to a hardcoded `Some` at the read site
/// would pass every other test in this file.
#[tokio::test]
async fn a_turn_that_finishes_inside_its_budget_reports_no_halt() {
    let (base_url, script) = spawn_script(write_then_answer(2), CHEAP_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, _ops) = deps_for(base_url, dir.path());
    let rec = record(Some(CAP_USD));
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
        outcome.halted_for_spend.is_none(),
        "a turn that finished well inside its budget owes no halt notice: {:?}",
        outcome.halted_for_spend
    );
    assert!(
        outcome.reply.contains(ANSWER),
        "the control must actually finish to be a control at all: {}",
        outcome.reply
    );
    assert_eq!(
        script.calls(),
        3,
        "two tool rounds plus the answer — the fixture is armed, just not tripped"
    );
}

/// A teammate that declared **no** `budget_usd_daily` can never report a spend
/// halt, however expensive its turn — because #988 installs no hook for it.
///
/// Same million-token script as the headline test, with the manifest key omitted
/// instead of set. If this fails, either a hook is being armed for a teammate
/// who declared nothing, or a blanket default crept back in.
#[tokio::test]
async fn a_teammate_with_no_declared_budget_can_never_report_a_halt() {
    let (base_url, script) = spawn_script(write_then_answer(3), EXPENSIVE_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, _ops) = deps_for(base_url, dir.path());
    let rec = record(None);
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
        outcome.halted_for_spend.is_none(),
        "no cap was declared, so no brake exists to have fired: {:?}",
        outcome.halted_for_spend
    );
    assert!(
        outcome.reply.contains(ANSWER),
        "an undeclared teammate must not be halted at any cost: {}",
        outcome.reply
    );
    assert_eq!(
        script.calls(),
        4,
        "every scripted round should have run — nothing should have cut it short"
    );
}

// ---------------------------------------------------------------------------
// What the operator sees
// ---------------------------------------------------------------------------

/// A spend-halted chat turn reaches the operator as **two** bubbles: whatever
/// the agent had, then the system saying it was stopped for money.
///
/// The second bubble is unauthored — no teammate said it — and carries no steps,
/// because the timeline already rode in on the first.
#[tokio::test]
async fn a_spend_halted_chat_turn_says_so_in_a_second_bubble() {
    let (base_url, _script) = spawn_script(write_then_answer(CAP), EXPENSIVE_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain =
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record(Some(CAP_USD))).with_runs(ops);

    let result = brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");

    let bubbles = operator_bubbles(&result.channel_responses);
    assert_eq!(
        bubbles.len(),
        2,
        "a halted turn owes the operator the reply AND the halt notice: {:?}",
        bubbles.iter().map(|b| &b.text).collect::<Vec<_>>()
    );

    // The agent's text, attributed to the agent.
    assert_eq!(bubbles[0].agent.as_deref(), Some(AGENT));

    // The system's notice, attributed to nobody.
    let notice = &bubbles[1].text;
    assert!(
        bubbles[1].agent.is_none(),
        "the platform's words must not be put in the agent's mouth"
    );
    assert!(
        bubbles[1].steps.is_empty(),
        "the timeline is already on the reply bubble; repeating it doubles every row"
    );

    // It has to actually say the things the operator needs.
    assert!(
        notice.contains(SPEND_MARKER),
        "it must name the spend halt: {notice}"
    );
    assert!(
        notice.contains(AGENT),
        "it must name whose cap was reached, or the figures are unattributable: {notice}"
    );
    assert!(
        notice.contains("$0.05"),
        "it must quote the cap it was measured against: {notice}"
    );
    assert!(
        notice.contains("Nothing errored"),
        "it must say nothing failed, or a budget reads as a crash: {notice}"
    );
    // The one thing it must NOT say. A step pause is resumable with "continue";
    // a spend halt is not, and inviting the operator to reply "continue" here
    // would invite them to burn the rest of a budget that had already run out.
    assert!(
        !notice.contains("continue"),
        "the spend notice must never tell the operator to reply \"continue\": {notice}"
    );
    assert_ne!(
        notice, ITERATION_CAP_PAUSE_NOTICE,
        "the two notices must not be interchangeable — the operator's next action differs"
    );
}

/// The same cycle with the turn inside its budget: exactly **one** bubble.
///
/// The pair is the real assertion — a notice that fires on every turn is as
/// useless as one that never fires.
#[tokio::test]
async fn a_turn_inside_its_budget_says_nothing_extra() {
    let (base_url, _script) = spawn_script(write_then_answer(2), CHEAP_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain =
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record(Some(CAP_USD))).with_runs(ops);

    let result = brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");

    let bubbles = operator_bubbles(&result.channel_responses);
    assert_eq!(
        bubbles.len(),
        1,
        "a turn that finished owes no halt notice: {:?}",
        bubbles.iter().map(|b| &b.text).collect::<Vec<_>>()
    );
    assert!(
        !bubbles[0].text.contains(SPEND_MARKER),
        "and it must not be smuggled into the reply either"
    );
}

/// **The cross-fire assertion, both directions.** A step pause emits the step
/// notice and not the spend one; a spend halt emits the spend notice and not the
/// step one.
///
/// This is what stops the two becoming interchangeable. They are not two
/// spellings of one condition: a step pause means the work fits and the turn ran
/// out of room, so `"continue"` finishes it; a spend halt means the work costs
/// more than the budget allows, and asking again just spends more.
///
/// Run as one test over two cycles because the assertion IS the comparison —
/// split apart, each half could pass while the pair stayed wrong.
#[tokio::test]
async fn the_step_notice_and_the_spend_notice_do_not_cross_fire() {
    // Direction one: a turn that exhausts its iterations, by a teammate with no
    // budget to run out of. `CAP` tool rounds, then the tools-disabled wrap-up.
    let (base_url, _script) = spawn_script(write_then_answer(CAP), CHEAP_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record(None)).with_runs(ops);

    let result = brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");
    let texts: Vec<&str> = operator_bubbles(&result.channel_responses)
        .iter()
        .map(|b| b.text.as_str())
        .collect();
    assert!(
        texts.contains(&ITERATION_CAP_PAUSE_NOTICE),
        "a turn that ran out of steps must emit the STEP notice: {texts:?}"
    );
    assert!(
        texts.iter().all(|t| !t.contains(SPEND_MARKER)),
        "a step pause must not be reported as a spend halt: {texts:?}"
    );

    // Direction two: a turn halted for spend, by a teammate that declared a cap.
    let (base_url, _script) = spawn_script(write_then_answer(CAP), EXPENSIVE_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain =
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record(Some(CAP_USD))).with_runs(ops);

    let result = brain
        .run_cycle(chat("Write a short feature spec."), &NoopHost)
        .await
        .expect("cycle runs");
    let texts: Vec<&str> = operator_bubbles(&result.channel_responses)
        .iter()
        .map(|b| b.text.as_str())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains(SPEND_MARKER)),
        "a turn halted for money must emit the SPEND notice: {texts:?}"
    );
    assert!(
        !texts.contains(&ITERATION_CAP_PAUSE_NOTICE),
        "a spend halt must not be reported as a step pause: {texts:?}"
    );
}

// ---------------------------------------------------------------------------
// What memory keeps
// ---------------------------------------------------------------------------

/// The turn's memory write carries the agent's own text and **not** the
/// platform's notice.
///
/// This is why the notice is a sibling bubble rather than text appended to the
/// reply. `HarnessPool::run` persists `outcome.reply` to the context store, so
/// appending would file "you ran out of budget" as something the agent said, and
/// the memory loop would recall it into a later turn as prior work.
#[tokio::test]
async fn the_spend_notice_never_reaches_memory() {
    let (base_url, _script) = spawn_script(write_then_answer(CAP), EXPENSIVE_TOKENS).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let context = FsContextStore::new(dir.path());
    let brain =
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record(Some(CAP_USD))).with_runs(ops);

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
        bodies.iter().all(|b| !b.contains(SPEND_MARKER)),
        "the platform's halt notice must never be recalled as something the agent said: {bodies:?}"
    );
}

// ---------------------------------------------------------------------------
// The notice itself
// ---------------------------------------------------------------------------

/// The notice quotes the figures it was given, and stays distinct from the step
/// notice.
///
/// A direct test of the formatter so the wording contract does not rely on a
/// turn that takes seconds to drive.
#[test]
fn the_notice_quotes_the_spend_the_cap_and_the_teammate() {
    let notice = spend_halt_notice(&crate::harness::SpendHalt {
        agent: "researcher".to_string(),
        spent_usd: 4.02,
        cap_usd: 4.0,
    });
    assert!(notice.contains("researcher"), "{notice}");
    assert!(
        notice.contains("$4.02"),
        "the real spend, not the cap: {notice}"
    );
    assert!(notice.contains("$4.00"), "{notice}");
    assert!(
        !notice.contains("continue"),
        "a spend halt is not resumable by asking again: {notice}"
    );
}
