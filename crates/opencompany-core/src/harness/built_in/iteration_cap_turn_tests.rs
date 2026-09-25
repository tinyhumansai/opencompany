//! End-to-end proof of issue #988's two halves, driven by a *model*: the turn's
//! tool-iteration ceiling really is [`MAX_TOOL_ITERATIONS`], and a teammate who
//! has declared a `budget_usd_daily` gets an in-turn brake that halts a turn
//! outrunning it — while a teammate who has declared none gets no such brake at
//! all, matching openhuman's own opt-in `GoalBudgetStopHook` posture rather
//! than a blanket ceiling this crate would invent and own alone.
//!
//! Neither half can be shown by a unit test. The cap lives on the vendored
//! session's config and is only spent by the real tool loop, and
//! openhuman's `BudgetStopHook` fires from inside
//! openhuman's `tinyagents` middleware off usage the *provider* reported — so a
//! test that never makes a provider call never fires it. The offline
//! [`MockProvider`](crate::harness::provider::MockProvider) cannot stand in
//! either: it issues no tool calls at all, so a turn against it is one
//! iteration long by construction.
//!
//! So this drives the **real** harness — real [`build_agent`], real
//! [`CompanyAgent::run`], real [`HostedProvider`] (which advertises
//! `tool_calling: true`, putting the turn on the production
//! `NativeToolDispatcher` path), real [`ApprovalPolicy`] under the default
//! `supervised` mode, real sandboxed file tools — and stubs exactly one thing,
//! at the one boundary that needs a credential: the model's choices, via a
//! scripted OpenAI-compatible endpoint on loopback (the shape
//! [`workspace_turn_test`](super::workspace_turn_test) and
//! [`search_turn_test`](super::search_turn_test) established).
//!
//! The scripted model reads a different file each iteration. Distinct arguments
//! are load-bearing: openhuman's repeat-progress guard halts a run that reissues
//! an *identical* successful tool batch, so a loop of one repeated call would
//! stop for a reason that has nothing to do with the cap under test.
//!
//! The load-bearing assertions are the two a shorter test cannot make:
//!
//! * a turn that spends **more than the old ceiling of 10** iterations now
//!   delivers its answer instead of pausing at a checkpoint; and
//! * a budget halt and an iteration-cap pause are **different outcomes** —
//!   openhuman reports the latter through `Agent::last_turn_hit_cap`, which
//!   stays `false` for the former. Part 1 of #926 makes the cap pause
//!   operator-visible, so the two must never be conflated; and
//! * a teammate with **no declared `budget_usd_daily`** gets no in-turn brake
//!   at all — a turn that would have blown past any invented blanket figure
//!   still finishes, because there is no hook installed to stop it.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::credentials::Credential;
use crate::company::{Agent as ManifestAgent, Policy};
use crate::harness::build::{MAX_TOOL_ITERATIONS, agent_workspace, build_agent};
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::{ApprovalPolicy, ApprovalRequestQueue};
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::{CompanyAgent, HarnessDeps};
use crate::ports::types::CompanyId;
use crate::runtime::delegation::ChatTarget;
use crate::store::{FsCompanyStore, FsContextStore};

/// The vendored `AgentConfig::default().max_tool_iterations` this crate used to
/// inherit by omission — the number #988 exists to leave behind.
///
/// Restated here rather than read from openhuman on purpose: the tests below
/// assert that a turn outruns *this* number, and a future vendored bump would
/// otherwise silently weaken them into asserting nothing.
const INHERITED_CAP: usize = 10;

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

/// What the scripted model does on each successive call.
#[derive(Clone, Debug)]
enum Turn {
    /// Emit a tool call with these literal arguments.
    Call { tool: String, args: Value },
    /// Finish the turn with plain assistant text.
    Say(&'static str),
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
struct Script {
    turns: Mutex<Vec<Turn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: Mutex<Vec<Value>>,
    /// `prompt_tokens` echoed on every response. The stop-hook middleware folds
    /// this into the turn's openhuman `TurnCost`, so it is the
    /// knob that decides whether the budget hook fires.
    prompt_tokens: u64,
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
                // Running off the end of the script means the turn looped more
                // than expected; end it with text rather than hanging.
                let next = next.unwrap_or(Turn::Say("ran off the end of the script"));
                let message = match next {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(&tool, &args),
                };
                Json(json!({
                    "choices": [{ "index": 0, "message": message }],
                    "usage": {
                        "prompt_tokens": script.prompt_tokens,
                        "completion_tokens": 4
                    }
                }))
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

/// How many times the scripted model was actually called this turn.
fn model_calls(script: &Script) -> usize {
    script.seen.lock().unwrap().len()
}

/// A script that reads `n` distinct files and then answers.
///
/// Distinct paths, not one path `n` times: an identical successful tool batch
/// reissued back to back is what openhuman's repeat-progress guard halts on, and
/// a run stopped by *that* would prove nothing about the iteration cap.
fn read_then_answer(n: usize, answer: &'static str) -> Vec<Turn> {
    let mut turns: Vec<Turn> = (0..n)
        .map(|i| Turn::Call {
            tool: "file_read".to_string(),
            args: json!({ "path": format!("note-{i:02}.md") }),
        })
        .collect();
    turns.push(Turn::Say(answer));
    turns
}

// ---------------------------------------------------------------------------
// The harness under test
// ---------------------------------------------------------------------------

/// Wire real dependencies against the scripted model. No search backend, no
/// workspace store, no meter — the two things under test are the turn's own
/// iteration ceiling and its in-turn spend brake, and neither reads any of them.
///
/// `meter: None` is a deliberate choice, not a shortcut: it is exactly the state
/// in which the **pre-dispatch** daily-spend gate documents itself as failing
/// open (`HarnessPool::run_inner` warns and runs the turn rather than bricking
/// the teammate). That is the host on which the in-turn brake is the only spend
/// control left standing, which is the condition #988 is about.
fn deps(model_url: String, dir: &std::path::Path) -> HarnessDeps {
    HarnessDeps {
        takeovers: Default::default(),
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(HostedProvider::new(HostedProviderConfig {
            base_url: model_url,
            credential: Credential::from_value("stub-key"),
            extra_headers: Vec::new(),
        })),
        provider_slug: "managed".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: None,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        // Keys rework, issue #2306, slice 2d: `HostedProvider` has no decl
        // and therefore no configured map, so a bare tier name is refused
        // outright rather than sent — this fixture's own tests assert on
        // iteration counts and cap behaviour, never on a priced spend
        // figure, so a stub id changes nothing they check.
        model_override: Some("stub-model".to_string()),
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        skills_registry: std::sync::Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: DelegationQueue::default(),
        workflow_runner: WorkflowRunnerHandle::default(),
        mcp_failures: McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: ApprovalRequestQueue::default(),
        approval_parker: None,
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
        search: None,
        tenant_search: None,
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    }
}

/// One real company agent, plus `files` notes seeded in its sandbox so every
/// scripted `file_read` succeeds.
///
/// A failing read would be a different experiment: repeated tool *failures* trip
/// openhuman's circuit breaker, which halts the run for a third reason on top of
/// the cap and the budget.
async fn company_agent(
    model_url: String,
    dir: &std::path::Path,
    budget_usd_daily: Option<f64>,
    notes: usize,
) -> CompanyAgent {
    let deps = deps(model_url, dir);
    let company = CompanyId::new("acme");
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "ceo".to_string(),
        role: "Chief Executive".to_string(),
        name: None,
        description: None,
        tier: None,
        tools: None,
        skills: None,
        delegates_to: Vec::new(),
        context: None,
        harness: None,
        budget_usd_daily,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    };
    // The manifest default. `file_read` reaches nothing outside the sandbox, so
    // it is auto-approved here — the point is that the turn is gated by the real
    // policy, not that the policy is switched off for the test.
    let policy = ApprovalPolicy::new(&Policy::default(), None);
    let agent = build_agent(
        &company,
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &["docs".to_string()],
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");

    let workspace = agent_workspace(&deps.workspace_root, &company, "ceo");
    std::fs::create_dir_all(&workspace).expect("workspace");
    for i in 0..notes {
        std::fs::write(
            workspace.join(format!("note-{i:02}.md")),
            format!("Note {i}.\n"),
        )
        .expect("seed note");
    }

    let runtime = crate::harness::openhuman_runtime::global(
        crate::harness::openhuman_runtime::RuntimeBoot::ephemeral(),
    )
    .await
    .expect("the OpenHuman runtime boots");
    // A fresh id per fixture: one test binary registers this agent many
    // times over, and a runtime id stays taken while a prior fixture's
    // handle is alive.
    let company = crate::ports::CompanyId::new(format!("test-{}", uuid::Uuid::new_v4().simple()));
    CompanyAgent::register(
        &runtime,
        &company,
        "ceo",
        "Chief Executive",
        budget_usd_daily,
        agent,
        None,
    )
    .expect("the agent registers")
}

/// Did the just-finished turn pause at the tool-iteration cap?
///
/// Read off the outcome the turn returned: the embed facade has no
/// `last_turn_hit_cap`, and the flag the pool derives from the progress
/// stream (`progress_pump::hit_iteration_cap`) IS the distinction Part 1 of
/// #926 surfaces to operators.
fn hit_cap(outcome: &crate::harness::built_in::TurnOutcome) -> bool {
    outcome.hit_iteration_cap
}

// ---------------------------------------------------------------------------
// Tests, split by topic (this file would otherwise exceed 750 lines).
// ---------------------------------------------------------------------------

#[path = "iteration_cap_turn_tests_budget.rs"]
mod tests_budget;
#[path = "iteration_cap_turn_tests_chat_leak.rs"]
mod tests_chat_leak;
