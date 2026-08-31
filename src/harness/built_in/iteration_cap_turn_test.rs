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
use crate::runtime::delegation::with_chat_only_hint;
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
        // Left unset so the turn runs on the tier the manifest resolves
        // (`chat-v1`), which is a *priced* row in openhuman's tier table. The
        // budget hook reads an estimate off that table when the backend echoes no
        // charged amount, so pinning a made-up model name here would make the
        // spend figure depend on a pricing fallback instead of a stated rate.
        model_override: None,
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
        global: false,
        id: "ceo".to_string(),
        role: "Chief Executive".to_string(),
        name: None,
        description: None,
        tier: None,
        tools: None,
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
        policy,
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

    CompanyAgent {
        agent_id: "ceo".to_string(),
        role: "Chief Executive".to_string(),
        budget_usd_daily,
        step_labels: crate::harness::steps::StepLabels::from_tools(agent.tools()),
        agent: tokio::sync::Mutex::new(agent),
        bound_chat: tokio::sync::Mutex::new(None),
    }
}

/// Did the just-finished turn pause at the tool-iteration cap?
///
/// openhuman's own answer, read off the same session the turn ran on. This is
/// the distinction Part 1 of #926 surfaces to operators, and the reason the
/// budget halt below has to be measured against it rather than against a
/// substring of some reply.
async fn hit_cap(agent: &CompanyAgent) -> bool {
    agent.agent.lock().await.last_turn_hit_cap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The in-turn brake arms **only** when the teammate declares a daily cap — and
/// a malformed manifest value is ignored rather than forwarded.
///
/// This is deliberate and matches upstream: openhuman constructs
/// `BudgetStopHook` nowhere and applies only an opt-in token-based goal hook, so
/// this crate, like upstream, refuses to invent a blanket per-turn number no
/// operator can see or change. A teammate with no declared budget is not
/// hard-stopped mid-turn. Forwarding a malformed value would be worse than
/// ignoring it: the vendored hook fails closed on a non-finite or non-positive
/// cap, so a zero would silently halt every turn that teammate ever ran at its
/// first iteration.
#[tokio::test]
async fn the_budget_brake_arms_only_when_a_daily_cap_is_declared() {
    let (model_url, _script) = spawn_script(vec![Turn::Say("hi")], 12).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = company_agent(model_url, dir.path(), None, 0).await;

    // No declared budget → no hook armed.
    assert_eq!(agent.turn_spend_cap_usd(), None);

    // A declared daily cap arms the brake at exactly that value — one cap bounds
    // the worst-case overshoot rather than "one turn, of unknown size".
    agent.budget_usd_daily = Some(2.0);
    assert_eq!(agent.turn_spend_cap_usd(), Some(2.0));

    agent.budget_usd_daily = Some(500.0);
    assert_eq!(agent.turn_spend_cap_usd(), Some(500.0));

    // Malformed values arm nothing rather than a fail-closed hook: such a
    // teammate is already refused pre-dispatch (`spent >= cap` holds at zero
    // spend), so no hook is the safe and honest choice.
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        agent.budget_usd_daily = Some(bad);
        assert_eq!(
            agent.turn_spend_cap_usd(),
            None,
            "a manifest cap of {bad} must not reach the hook"
        );
    }
}

/// The headline proof, and the one that fails without the fix: a turn whose work
/// takes more than the inherited ten iterations — read the standards, read the
/// checklist, read the prior spec, … — now **delivers its answer** instead of
/// pausing at a checkpoint the operator has to resume.
///
/// Twelve reads is deliberately just past the old ceiling and well inside the
/// new one, so this measures the ceiling rather than the size of the script.
#[tokio::test]
async fn a_turn_past_the_old_ten_iteration_ceiling_now_finishes() {
    let reads = INHERITED_CAP + 2;
    let (model_url, script) = spawn_script(read_then_answer(reads, "Spec published."), 12).await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, reads).await;

    let (outcome, _usages) = agent
        .run("Draft and publish the pricing spec.")
        .await
        .expect("the turn runs");

    assert!(
        outcome.reply.contains("Spec published."),
        "the turn did not deliver its answer — it paused instead: {}",
        outcome.reply
    );
    // The script's reads really happened: one model call per tool round plus the
    // final answer. Without this the assertion above could pass on a turn that
    // never looped at all.
    assert_eq!(
        model_calls(&script),
        reads + 1,
        "expected {reads} tool rounds plus a final answer"
    );
    assert!(
        !hit_cap(&agent).await,
        "a turn that finished must not report an iteration-cap pause"
    );
}

/// A turn that outruns its money is halted **inside** the turn — and the halt is
/// not an iteration-cap pause.
///
/// Both halves matter. Before #988 nothing in this crate could stop a running
/// turn except the iteration ceiling itself: the plan-level token ceiling and
/// the teammate's `budget_usd_daily` are both pre-dispatch, so a turn that
/// started under a cap could finish arbitrarily far over it. Raising the ceiling
/// without this hook would have removed the last brake rather than added one.
///
/// And the two stops must stay distinguishable. openhuman only reports
/// `hit_cap` when the run actually reached `max_tool_iterations` with no final
/// response, so a hook-driven halt that stops on the first iteration — nowhere
/// near the ceiling — reads as `false`. That is what Part 1 of #926 needs: it
/// renders the cap pause to the operator and must not label a budget halt as
/// one.
#[tokio::test]
async fn a_budget_halt_stops_the_turn_and_is_not_an_iteration_cap_pause() {
    // One model call reports a million prompt tokens. On the `chat-v1` tier
    // that estimates to ~$0.14, so the very first iteration crosses a $0.05 cap
    // and the hook halts a turn the script was willing to run for twelve more
    // rounds.
    let reads = INHERITED_CAP + 2;
    let (model_url, script) =
        spawn_script(read_then_answer(reads, "Spec published."), 1_000_000).await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), Some(0.05), reads).await;

    let (outcome, _usages) = agent
        .run("Draft and publish the pricing spec.")
        .await
        .expect("the turn runs");

    assert!(
        !outcome.reply.contains("Spec published."),
        "the budget hook did not stop the turn — it ran to the script's answer: {}",
        outcome.reply
    );
    // Halted early, not merely slowed: the script offered `reads + 1` rounds and
    // the turn spent a small handful. (The exact count is left loose because the
    // vendored turn adds a closing wrap-up call after a partial run, and that
    // call is not the thing under test.)
    let calls = model_calls(&script);
    assert!(
        calls < reads,
        "expected the turn to halt well short of its {reads} scripted rounds, got {calls}"
    );
    assert!(
        !hit_cap(&agent).await,
        "a budget halt must NOT be reported as an iteration-cap pause — Part 1 of #926 \
         renders that pause to the operator and the two are different outcomes"
    );
}

/// The negative case the budget-halt test above needs to be meaningful: a
/// teammate who has declared **no** `budget_usd_daily` gets no in-turn brake at
/// all, so a turn that would have blown past any invented blanket figure still
/// finishes.
///
/// Same script as the budget-halt test — a million reported prompt tokens per
/// call, which would trip even a generous fixed ceiling on the very first
/// iteration — with the manifest budget omitted instead of set. If this test
/// fails, either a hook is being armed for a teammate who declared nothing, or
/// some other default crept back in.
#[tokio::test]
async fn a_turn_with_no_declared_budget_gets_no_in_turn_brake_at_any_cost() {
    let reads = INHERITED_CAP + 2;
    let (model_url, script) =
        spawn_script(read_then_answer(reads, "Spec published."), 1_000_000).await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, reads).await;
    assert_eq!(
        agent.turn_spend_cap_usd(),
        None,
        "the fixture must actually be undeclared for this test to prove anything"
    );

    let (outcome, _usages) = agent
        .run("Draft and publish the pricing spec.")
        .await
        .expect("the turn runs");

    assert!(
        outcome.reply.contains("Spec published."),
        "a turn with no declared budget was halted anyway — a brake armed \
         without one being declared: {}",
        outcome.reply
    );
    assert_eq!(
        model_calls(&script),
        reads + 1,
        "expected every scripted round to run — nothing should have cut it short"
    );
    assert!(
        !hit_cap(&agent).await,
        "the script stayed well under the iteration ceiling; a cap pause here \
         would mean something other than the intended reply mechanism stopped \
         the turn"
    );
}

/// The contrast case, so the assertion above is a distinction rather than a
/// constant: a turn that really does exhaust [`MAX_TOOL_ITERATIONS`] **does**
/// report the cap.
///
/// Without this test `!hit_cap` in the budget case could hold because nothing
/// ever sets it.
#[tokio::test]
async fn exhausting_the_raised_cap_still_reports_an_iteration_cap_pause() {
    let reads = MAX_TOOL_ITERATIONS + 5;
    let (model_url, script) = spawn_script(read_then_answer(reads, "Spec published."), 12).await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, reads).await;

    let (outcome, _usages) = agent
        .run("Draft and publish the pricing spec.")
        .await
        .expect("the turn runs");

    assert!(
        hit_cap(&agent).await,
        "a turn that never stopped calling tools must report the cap: {}",
        outcome.reply
    );
    // It got all the way to the raised ceiling before pausing — the cap moved
    // with the constant rather than staying at the inherited ten. `>=` rather
    // than `==` because the vendored turn adds a wrap-up call on top of the
    // loop's own rounds to compose the checkpoint.
    let calls = model_calls(&script);
    assert!(
        calls >= MAX_TOOL_ITERATIONS,
        "the turn paused after {calls} model calls, short of the stated \
         {MAX_TOOL_ITERATIONS} — the raised cap is not in effect"
    );
}

// ---------------------------------------------------------------------------
// Issue #1725 — the greeting fast-path + context-isolation regression.
//
// The direct reproduction of the screenshot bug, end to end through the real
// `CompanyAgent::run_with_steer` turn path against the scripted model: a task
// that fetched content (the "sport story ranking" HTML) leaves the agent's
// in-memory history full, and a bare "hi" on a NEW chat used to run the whole
// agentic loop AND reply against the prior task's replayed content. The four
// unit tests cover the mechanisms individually (per-turn tool/memory/goal
// suppression, the greeting classifier, the chat-only hint, per-conversation
// history); this is the one test that exercises them together and asserts the
// observable symptoms the screenshot showed.
// ---------------------------------------------------------------------------

/// A live turn-stream routing context for `chat`, so the pool binds this
/// agent's history to that thread (issue #1725).
fn stream_for(chat: &str) -> crate::turn_stream::TurnStreamCtx {
    crate::turn_stream::TurnStreamCtx {
        company: CompanyId::new("acme"),
        agent_id: "ceo".to_string(),
        route: crate::turn_stream::LiveRoute::Chat {
            chat_id: chat.to_string(),
        },
    }
}

/// A task fetches content and sets the agent's context; then a bare "hi" on a
/// different chat must run **no** tools, open no loop, and carry **nothing**
/// from the prior task. Reverting the fix (see the module note) makes the "hi"
/// turn offer its tools and replay the fetched content — the screenshot bug.
#[tokio::test]
async fn a_greeting_after_a_task_runs_no_tools_and_leaks_no_prior_context() {
    // A body distinctive enough that its presence in a later turn's model
    // request is unambiguous — this stands in for the replayed ranking HTML.
    const FETCHED: &str = "SPORTBALL_RANKING_HTML_MARKER_9F3A";

    let (model_url, script) = spawn_script(
        vec![
            // Task A: read the ranking note (a tool round), then answer.
            Turn::Call {
                tool: "file_read".to_string(),
                args: json!({ "path": "note-00.md" }),
            },
            Turn::Say("Ranked the sport stories."),
            // The bare greeting on a fresh chat: one plain reply, no tool call
            // scripted — so if the turn tries to loop it runs off the script.
            Turn::Say("Hi! How can I help you today?"),
        ],
        12,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, 1).await;
    // Overwrite the seeded note with the distinctive fetched body.
    let workspace = agent_workspace(dir.path(), &CompanyId::new("acme"), "ceo");
    std::fs::write(
        workspace.join("note-00.md"),
        format!("{FETCHED}\n<html>ranked sport stories</html>\n"),
    )
    .expect("seed the fetched note");

    // ── Task A on chat "sports": a real work turn — tools attach and run. ──
    let (outcome_a, _usage_a) = agent
        .run_with_steer(
            "rank the sport stories and read the ranking html",
            None,
            Some(stream_for("sports")),
            None,
            None,
            None,
        )
        .await
        .expect("task A runs");
    assert!(
        !outcome_a.steps.is_empty(),
        "task A must actually run a tool step (the fetch) — otherwise the \
         isolation below proves nothing"
    );
    {
        let seen = script.seen.lock().unwrap();
        // The fetched content really entered the model's context on task A.
        assert!(
            seen.iter().any(|r| r.to_string().contains(FETCHED)),
            "task A's fetched content must reach the model on its own turn"
        );
        // And task A was offered its tools (the contrast the greeting breaks).
        let a_tools = seen
            .first()
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        assert!(a_tools > 0, "a real work turn must be offered its tools");
    }

    let calls_before = model_calls(&script);

    // ── A bare "hi" on a DIFFERENT chat — the greeting fast path. ──
    let (outcome_b, _usage_b) = with_chat_only_hint(
        true,
        agent.run_with_steer("hi", None, Some(stream_for("smalltalk")), None, None, None),
    )
    .await
    .expect("the greeting runs");

    // 1) Zero tool steps ran — the greeting never entered the agentic loop.
    assert!(
        outcome_b.steps.is_empty(),
        "a greeting must run no tool steps, got {:?}",
        outcome_b.steps
    );
    // 2) Exactly one model call — no tool-loop iterations.
    assert_eq!(
        model_calls(&script) - calls_before,
        1,
        "the greeting must be a single model call, not a loop"
    );

    let greeting_req = script
        .seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the greeting produced a model request");

    // 3) The greeting turn was offered NO tools (suppress_tools).
    let greeting_tools = greeting_req
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        greeting_tools, 0,
        "a chat-only turn must be sent an empty tool schema"
    );

    // 4) NOTHING from task A leaked into the greeting's context — no replayed
    //    fetched HTML, no prior-task active-goal block. This is the screenshot
    //    bug, asserted directly.
    let greeting_str = greeting_req.to_string();
    assert!(
        !greeting_str.contains(FETCHED),
        "task A's fetched content must NOT replay into an unrelated greeting"
    );
    assert!(
        !greeting_str.contains("[active_goal]"),
        "no prior task's goal may steer the greeting"
    );

    // 5) It still answered — abstain-or-reduce, never a silent non-answer.
    assert!(
        !outcome_b.reply.trim().is_empty(),
        "the greeting still gets a reply"
    );
}

/// A background task (`stream: None` — the same shape `run_background` and
/// `run_steered_background` hand `run_with_steer`, since neither carries a
/// chat thread) must not leave the agent bound to whichever chat happened to
/// stream last. Reverting the unthreaded-turn branch in `run_with_steer`
/// leaves `bound_chat` pointed at "sports" after the background turn runs, so
/// the operator's SECOND turn on that same chat reads `switched == false`,
/// skips the clear-and-reseed, and inherits the background task's fetched
/// content — the cross-context leak review found on #1725.
#[tokio::test]
async fn a_background_turn_does_not_leak_into_the_next_turn_on_its_bound_chat() {
    const FETCHED: &str = "BACKGROUND_TASK_MARKER_71B2";

    let (model_url, script) = spawn_script(
        vec![
            // Chat "sports", turn 1: a plain reply — binds bound_chat to "sports".
            Turn::Say("Sure, tracking the sports desk."),
            // The background task: a tool round, then an answer.
            Turn::Call {
                tool: "file_read".to_string(),
                args: json!({ "path": "note-00.md" }),
            },
            Turn::Say("Background task done."),
            // Chat "sports", turn 2 — the SAME chat id as turn 1.
            Turn::Say("Sounds good."),
        ],
        12,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, 1).await;
    // Overwrite the seeded note with the distinctive background-task body.
    let workspace = agent_workspace(dir.path(), &CompanyId::new("acme"), "ceo");
    std::fs::write(
        workspace.join("note-00.md"),
        format!("{FETCHED}\n<html>background task content</html>\n"),
    )
    .expect("seed the fetched note");

    // ── Chat "sports", turn 1: binds bound_chat to "sports". ──
    agent
        .run_with_steer(
            "hello from sports",
            None,
            Some(stream_for("sports")),
            None,
            None,
            None,
        )
        .await
        .expect("chat turn 1 runs");

    // ── The background task: unthreaded — `stream: None`, same shared Agent. ──
    let (outcome_bg, _usage_bg) = agent
        .run_with_steer("run the background task", None, None, None, None, None)
        .await
        .expect("background task runs");
    assert!(
        !outcome_bg.steps.is_empty(),
        "the background task must actually run a tool step (the fetch) — \
         otherwise the isolation below proves nothing"
    );
    {
        let seen = script.seen.lock().unwrap();
        assert!(
            seen.iter().any(|r| r.to_string().contains(FETCHED)),
            "the background task's fetched content must reach the model on \
             its own turn"
        );
    }

    let calls_before = model_calls(&script);

    // ── Chat "sports", turn 2 — same chat id the background task ran under
    //    no binding for, so this must be treated as a switch and re-seed. ──
    agent
        .run_with_steer(
            "still there?",
            None,
            Some(stream_for("sports")),
            None,
            None,
            None,
        )
        .await
        .expect("chat turn 2 runs");

    assert_eq!(
        model_calls(&script) - calls_before,
        1,
        "chat turn 2 must be a single model call, not a loop"
    );

    let turn2_req = script
        .seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("chat turn 2 produced a model request");

    assert!(
        !turn2_req.to_string().contains(FETCHED),
        "the background task's fetched content must NOT leak into the next \
         turn on the chat it happened to be bound to before the background \
         task ran"
    );
}
