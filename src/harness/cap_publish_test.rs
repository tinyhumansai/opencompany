//! Issue #989 (Part 2a of #926): a **chat** turn that pauses at its
//! tool-iteration cap runs the same #244 unpublished-work scan and nudge a
//! dispatched task already gets.
//!
//! Two features already exist and, before this, never met:
//!
//! - Part 1 (#926) taught the chat path to say "I paused" —
//!   `ITERATION_CAP_PAUSE_NOTICE`, proven end to end in
//!   [`cap_turn_test`](crate::harness::cap_turn_test).
//! - #244 taught the **task-dispatch** path (`run_task`) to scan the agent's
//!   sandbox for files it wrote and never published, and to ask about them in
//!   one follow-up turn — proven end to end in
//!   [`publish_turn_test`](crate::harness::publish_turn_test).
//!
//! A capped turn returns `Ok` with a checkpoint reply ("Done so far / Next
//! steps"), so nothing downstream treated it as interrupted, and the scan only
//! ever ran on the dispatch path — so a chat turn that hit the cap after
//! writing a file got the pause notice and nothing else. The file just sat in
//! its sandbox with nothing anywhere saying so.
//!
//! This reuses the same lever [`cap_turn_test`] and [`publish_turn_test`] both
//! do: a real [`HarnessBrain`], real `build_agent`, real [`HostedProvider`]
//! against a scripted loopback endpoint, real `FsOps` stores, and a real agent
//! workspace on disk — only the model's *choices* are scripted.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::CompanyManifest;
use crate::company::credentials::Credential;
use crate::harness::build::agent_workspace;
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::publish::PUBLISH_ARTIFACT_TOOL;
use crate::harness::{HarnessBrain, HarnessDeps, HarnessPool};
use crate::ports::artifacts::ArtifactStore;
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::tasks::TaskStore;
use crate::ports::types::{
    ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult, CycleRequest,
    Effect, EffectDisposition, ToolCall, ToolResult,
};
use crate::store::{FsCompanyStore, FsContextStore, FsOps};

/// The agent every test here talks to.
const AGENT: &str = "ceo";

/// `build_agent` states this explicitly via `set_max_tool_iterations`
/// (issue #988) — the same constant
/// [`cap_turn_test`](crate::harness::cap_turn_test) binds to and checks the
/// observed call count against, for the same reason: if a vendor bump or
/// another `set_max_tool_iterations` call moves the effective cap without
/// moving the constant, this test must fail loudly rather than silently
/// script the wrong turn shape and pass for it.
const CAP: usize = crate::harness::build::MAX_TOOL_ITERATIONS;

/// The checkpoint the scripted model writes when asked to wrap up.
const CHECKPOINT: &str = "Done so far: read the standards and drafted most of the outline. \
Next steps: finish the outline and publish it.";

/// A slice of [`publish::nudge_instruction`](crate::harness::publish::nudge_instruction)
/// unique to it, used to detect a nudge turn on the wire the same way
/// [`publish_turn_test`](crate::harness::publish_turn_test) does.
const NUDGE_MARKER: &str = "published none of them";

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Turn {
    Call { tool: &'static str, args: Value },
    Say(&'static str),
}

struct Script {
    turns: Mutex<Vec<Turn>>,
    seen: Mutex<Vec<Value>>,
}

impl Script {
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

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
                let next = next.unwrap_or(Turn::Say("done"));
                let message = match next {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(tool, &args),
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

/// How many **nudge turns** ran — counts requests whose last message is the
/// user message carrying the nudge instruction, the same discriminator
/// [`publish_turn_test::nudge_turns`](crate::harness::publish_turn_test) uses
/// and for the same reason: the follow-up turn continues the same
/// conversation, so counting "requests that mention the nudge" would also
/// count its own tool round trips.
fn nudge_turns(script: &Script) -> usize {
    script
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|body| {
            body.get("messages")
                .and_then(Value::as_array)
                .and_then(|m| m.last())
                .is_some_and(|last| {
                    last.get("role").and_then(Value::as_str) == Some("user")
                        && last
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|c| c.contains(NUDGE_MARKER))
                })
        })
        .count()
}

/// One successful `file_write` to a distinct path — distinct per step so
/// openhuman's repeated-failure breaker never halts the run (a breaker halt is
/// not a cap hit) and so no no-progress heuristic reads the ten calls as a loop.
fn step(n: usize) -> Turn {
    Turn::Call {
        tool: "file_write",
        args: json!({ "path": format!("step-{n}.md"), "content": format!("step {n}") }),
    }
}

/// One `file_read` of a distinct pre-existing file — distinct per step for
/// the same reason [`step`] writes to a distinct path: openhuman's
/// no-progress/loop heuristic can read ten identical calls as a stall and cut
/// the turn short through a different breaker than the iteration cap, which
/// would prove nothing about the cap-pause path this issue is about.
fn read_note(n: usize) -> Turn {
    Turn::Call {
        tool: "file_read",
        args: json!({ "path": format!("notes-{n}.md") }),
    }
}

/// `CAP` tool round trips that WRITE, then the tools-disabled wrap-up call —
/// the shape that reaches the cap having left something unpublished.
fn capped_script_writing() -> Vec<Turn> {
    let mut turns: Vec<Turn> = (1..=CAP).map(step).collect();
    turns.push(Turn::Say(CHECKPOINT));
    turns
}

/// `CAP` tool round trips that only READ pre-existing files, then the
/// wrap-up call — the shape that reaches the cap having changed nothing.
fn capped_script_reading() -> Vec<Turn> {
    let mut turns: Vec<Turn> = (1..=CAP).map(read_note).collect();
    turns.push(Turn::Say(CHECKPOINT));
    turns
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

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

/// A one-agent company on `full` policy (an ordinary turn is not parked for
/// approval — the gate under test is the iteration cap and the publish scan,
/// not the approval one) with every tool granted, so both `file_write` /
/// `file_read` and `publish_artifact` are on the wire.
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

/// Real deps pointed at the scripted endpoint, with task and artifact stores
/// wired — unlike [`cap_turn_test::deps_for`](crate::harness::cap_turn_test),
/// this needs `artifacts: Some(..)` too, or the publish claim this issue
/// depends on is never taken and `publish_artifact` is never even offered.
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
        artifacts: Some(ops.clone() as Arc<dyn ArtifactStore>),
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

// ---------------------------------------------------------------------------
// The headline
// ---------------------------------------------------------------------------

/// **The headline.** A chat turn spends its whole iteration budget writing
/// files and never publishes any of them. It must get the same one follow-up
/// nudge the task-dispatch path already gets.
///
/// Before issue #989 this scan never ran on the chat path at all, so the
/// assertion below (`nudge_turns == 1`) is the prove-red target: on the
/// pre-fix code the turn still reaches the cap and returns its checkpoint, but
/// no nudge ever fires — `nudge_turns(&script)` reads `0` and `script.calls()`
/// stops at `CAP + 1` (the wrap-up, and nothing after it).
#[tokio::test]
async fn a_capped_chat_turn_that_wrote_unpublished_work_gets_the_nudge() {
    let mut turns = capped_script_writing();
    // The nudge turn: a clean decline. The recovery case — the nudge
    // *publishing* the file — is its own test below.
    turns.push(Turn::Say(
        "step-10.md is a work-in-progress outline, not the finished draft yet.",
    ));
    let (base_url, script) = spawn_script(turns).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops);

    brain
        .run_cycle(chat("Draft the launch outline."), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(
        nudge_turns(&script),
        1,
        "a capped chat turn that wrote unpublished work must get exactly one nudge"
    );
    assert_eq!(
        script.calls(),
        CAP + 2,
        "expected {CAP} tool iterations, one wrap-up call, and one nudge turn: {} calls",
        script.calls()
    );
}

/// **The recovery case.** The nudge turn does not just get asked — it can
/// actually publish, and the file it recovers must be filed exactly like any
/// other conversation publish (issue #445): a card minted to carry it, the
/// artifact reachable on that card.
///
/// This is the property that makes the nudge worth adding rather than a
/// cosmetic "did you mean to publish?" with no way to act on it: the publish
/// claim is still live when the nudge runs, so a `publish_artifact` call here
/// stages and drains exactly like the primary turn's own would.
#[tokio::test]
async fn the_nudge_can_recover_the_file_a_capped_turn_wrote() {
    let mut turns = capped_script_writing();
    turns.push(Turn::Call {
        tool: PUBLISH_ARTIFACT_TOOL,
        args: json!({ "path": "step-10.md" }),
    });
    turns.push(Turn::Say("Published the outline draft."));
    let (base_url, script) = spawn_script(turns).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain =
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops.clone());

    brain
        .run_cycle(chat("Draft the launch outline."), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(nudge_turns(&script), 1);

    let cards = TaskStore::list(&*ops, &company()).await.expect("list");
    assert_eq!(
        cards.len(),
        1,
        "the nudge's publish must mint a card the same way a primary-turn \
         conversation publish would: {cards:?}"
    );
    let artifacts = ArtifactStore::list(&*ops, &company(), Some(&cards[0].id))
        .await
        .expect("list artifacts");
    assert_eq!(
        artifacts.len(),
        1,
        "the file the nudge published must actually be recorded, not silently dropped \
         once the primary turn's own drain already ran"
    );
    assert_eq!(artifacts[0].source.as_deref(), Some("step-10.md"));
}

// ---------------------------------------------------------------------------
// The negative controls
// ---------------------------------------------------------------------------

/// **No false positives on the common case.** A capped turn that wrote
/// nothing — it only *read* an existing file, ten times over, to reach the
/// cap — must not be nudged. The cap is about to be hit routinely once #988
/// raises it into everyday reach; a nudge on every capped turn would be noise
/// nobody could act on.
#[tokio::test]
async fn a_capped_chat_turn_that_wrote_nothing_gets_no_nudge() {
    let (base_url, script) = spawn_script(capped_script_reading()).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    // Pre-existing files the scripted turn only reads. `file_read` cannot move
    // their mtime or size, so the #244 scan's diff sees no change at all —
    // proving the negative control is real read traffic, not an empty script.
    let workspace = agent_workspace(dir.path(), &company(), AGENT);
    std::fs::create_dir_all(&workspace).expect("create workspace");
    for n in 1..=CAP {
        std::fs::write(
            workspace.join(format!("notes-{n}.md")),
            format!("pre-existing note {n}"),
        )
        .expect("seed notes file");
    }

    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops);

    brain
        .run_cycle(chat("Summarize the notes."), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(
        nudge_turns(&script),
        0,
        "a capped turn that wrote nothing must not be nudged"
    );
    assert_eq!(
        script.calls(),
        CAP + 1,
        "expected {CAP} tool iterations plus exactly one wrap-up call, and nothing after it: \
         {} calls",
        script.calls()
    );
}

/// **Existing #244 non-capped behaviour is unchanged.** A chat turn that
/// finishes well inside the cap and leaves unpublished work is not this
/// issue's scope — #244 never scanned an ordinary chat reply before, and this
/// change must not make it start. Widening chat-turn coverage beyond the
/// cap-pause trigger is a separate, unscoped change.
#[tokio::test]
async fn an_uncapped_chat_turn_with_unpublished_work_gets_no_nudge() {
    let (base_url, script) = spawn_script(vec![step(1), step(2), Turn::Say(CHECKPOINT)]).await;
    let dir = tempfile::tempdir().unwrap();
    let (deps, ops) = deps_for(base_url, dir.path());
    let brain = HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()).with_runs(ops);

    brain
        .run_cycle(chat("Draft the launch outline."), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(
        script.calls(),
        3,
        "well inside the cap: two tool calls plus the reply, no wrap-up and no nudge"
    );
    assert_eq!(
        nudge_turns(&script),
        0,
        "an ordinary chat reply's unpublished files are #244's existing, unchanged scope — \
         issue #989 only widens the cap-pause path"
    );
}
