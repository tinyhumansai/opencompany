//! End-to-end proof that `publish_artifact` (issue #244) actually works when a
//! *model* drives it — and that the one-nudge follow-up behaves as designed.
//!
//! The unit tests in [`publish`](crate::harness::publish) pin the tool's own
//! behaviour. They cannot tell you whether it is reachable from a real dispatch:
//! whether it survives `build_agent`'s grant gates, is advertised on the wire,
//! passes the [`ApprovalPolicy`] gate, dispatches through openhuman's native
//! tool loop, and — the part nothing else can reach — whether the brain drains
//! the queue, records by identity, and runs **exactly one** follow-up turn when
//! the agent wrote files and published none of them.
//!
//! So this drives the **real** brain through `run_cycle` on a `TaskDispatched`
//! event, with a real `HarnessPool`, real `build_agent`, real `HostedProvider`
//! (which advertises `tool_calling: true`, putting the turn on the production
//! `NativeToolDispatcher` path), real `FsOps` task and artifact stores, and a
//! real agent workspace on disk. Only the model's *choices* are scripted.
//!
//! The nudge is what makes the scripted endpoint load-bearing rather than
//! convenient: it is a second model turn, so nothing short of a real turn loop
//! can show that it fires, that it fires once, and that a decline is recorded
//! rather than retried. Local inference is not configured, so a loopback
//! OpenAI-compatible endpoint is the lever.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::CompanyManifest;
use crate::company::credentials::Credential;
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::publish::PUBLISH_ARTIFACT_TOOL;
use crate::harness::{HarnessBrain, HarnessDeps, HarnessPool};
use crate::ports::artifacts::{ArtifactRecord, ArtifactStore};
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, TaskRecord, TaskStore};
use crate::ports::types::{
    ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult, CycleRequest,
    Effect, EffectDisposition, ToolCall, ToolResult,
};
use crate::store::{FsCompanyStore, FsContextStore, FsOps};

/// The agent this fixture dispatches to. Its workspace is
/// `{root}/acme/ceo/workspace`.
const AGENT: &str = "ceo";

/// A marker unique to the nudge instruction, used to count nudge turns.
const NUDGE_MARKER: &str = "published none of them";

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

/// What the scripted model does on each successive call.
#[derive(Clone, Debug)]
enum Turn {
    /// Emit a tool call with these literal arguments.
    Call { tool: &'static str, args: Value },
    /// Finish the turn with plain assistant text.
    Say(&'static str),
    /// Take the endpoint down for good — the provider-fault path.
    ///
    /// **Sticky**, deliberately. A single failed response is not a failed turn:
    /// the client retries, and the script would then hand the retry the next
    /// scripted entry, so the turn would *succeed* and the test would prove
    /// nothing about containment. Poisoning the endpoint models what a real
    /// provider outage looks like to one turn.
    Boom,
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
struct Script {
    turns: Mutex<Vec<Turn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: Mutex<Vec<Value>>,
    /// Set once [`Turn::Boom`] is reached; every later request fails too.
    poisoned: std::sync::atomic::AtomicBool,
}

/// Serve the script on loopback and return its base URL plus the shared handle.
async fn spawn_script(turns: Vec<Turn>) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        turns: Mutex::new(turns),
        seen: Mutex::new(Vec::new()),
        poisoned: std::sync::atomic::AtomicBool::new(false),
    });
    let handle = Arc::clone(&script);
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let script = Arc::clone(&handle);
            async move {
                script.seen.lock().unwrap().push(body.clone());
                if script.poisoned.load(std::sync::atomic::Ordering::SeqCst) {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({ "error": { "message": "the model fell over" } })),
                    );
                }
                let next = {
                    let mut turns = script.turns.lock().unwrap();
                    if turns.is_empty() {
                        None
                    } else {
                        Some(turns.remove(0))
                    }
                };
                // Running off the end means the loop turned more than expected;
                // end it with text rather than hanging.
                let next = next.unwrap_or(Turn::Say("done"));
                let message = match next {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(tool, &args),
                    Turn::Boom => {
                        script
                            .poisoned
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({ "error": { "message": "the model fell over" } })),
                        );
                    }
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

/// Every tool name the scripted model was offered across the whole cycle.
fn advertised_tools(script: &Script) -> Vec<String> {
    let mut names: Vec<String> = script
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|body| body.get("tools").and_then(Value::as_array).cloned())
        .flatten()
        .filter_map(|tool| {
            tool.get("function")?
                .get("name")?
                .as_str()
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// How many **nudge turns** ran.
///
/// Counts requests whose **last** message is the user message carrying the
/// nudge instruction — that is, the opening request of each nudge turn.
///
/// The obvious discriminator does not work, and the reason is worth recording:
/// the follow-up turn **continues the same conversation**, so its first request
/// already carries the primary turn's assistant and tool messages. Counting
/// "requests that mention the nudge" would count the nudge turn's own tool
/// round trips and report two where one ran — the exact assertion these tests
/// exist to get right.
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

/// The nudge instruction the model was actually sent, if any.
fn nudge_text(script: &Script) -> Option<String> {
    script
        .seen
        .lock()
        .unwrap()
        .iter()
        .flat_map(|body| {
            body.get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|m| m.get("content").and_then(Value::as_str).map(str::to_string))
        .find(|c| c.contains(NUDGE_MARKER))
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// An inert `CycleHost` — this test is about the harness, not the effect gate.
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

/// A one-agent company. `grants` controls whether the file/publish surface is
/// wired at all.
fn manifest(grants: &str) -> CompanyManifest {
    toml::from_str(&format!(
        r#"
[company]
name = "Acme"

[policy]
# `full` so an ordinary turn is not parked for approval — the gate under test
# here is the grant + store gate, not the approval one.
mode = "full"

[tools]
allow = [{grants}]

[[agent]]
id = "{AGENT}"
role = "Chief Executive"
tier = "orchestrator"
"#
    ))
    .expect("manifest parses")
}

/// Wire a real brain against the scripted endpoint, with task and artifact
/// stores on disk.
fn brain(base_url: String, grants: &str, dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
    brain_with(base_url, grants, dir, true)
}

/// The same brain with **no artifact store** (issue #445).
///
/// The fail-closed case: nothing can record a deliverable, so `build_agent`
/// does not offer the tool and the brain never claims the publish queue.
fn brain_without_artifacts(base_url: String, dir: &std::path::Path) -> (HarnessBrain, Arc<FsOps>) {
    brain_with(base_url, "\"*\"", dir, false)
}

fn brain_with(
    base_url: String,
    grants: &str,
    dir: &std::path::Path,
    with_artifacts: bool,
) -> (HarnessBrain, Arc<FsOps>) {
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
        artifacts: with_artifacts.then(|| ops.clone() as Arc<dyn ArtifactStore>),
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
    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: manifest(grants),
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
    };
    (
        // Issue #339: the run store is wired here so a dispatch carrying a
        // `run_id` has an attempt to record — and therefore an attempt the
        // card's output link can name. Inert for every dispatch that passes
        // `run_id: None` (which is all of the pre-#339 tests below), because
        // `open_trace` needs both halves before it opens a sink.
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record).with_runs(ops.clone()),
        ops,
    )
}

/// Mints the `Pending` attempt row the dispatch choke point would have minted,
/// so a cycle can be dispatched under a real run id (issue #339).
async fn mint_run(ops: &Arc<FsOps>, run_id: &str, task_id: &str) {
    use crate::ports::RunStore;
    use crate::ports::runs::NewRun;
    RunStore::create_run(&**ops, &company(), NewRun::for_task(run_id, task_id, AGENT))
        .await
        .expect("mint the attempt row");
}

fn company() -> CompanyId {
    CompanyId::new("acme")
}

/// A dispatched card, already in the column dispatch happens from.
fn card(id: &str) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        title: "Draft the launch spec".to_string(),
        note: None,
        column: COLUMN_IN_PROGRESS.to_string(),
        priority: "medium".to_string(),
        assignee: AGENT.to_string(),
        updated_at_millis: 1,
        origin_chat_id: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        bounced: None,
    }
}

fn dispatch(task_id: &str) -> CycleRequest {
    dispatch_run(task_id, None)
}

/// A dispatch under a named attempt (issue #339) — what a real choke point
/// sends, and the only shape that can produce an output stamp.
fn dispatch_as(task_id: &str, run_id: &str) -> CycleRequest {
    dispatch_run(task_id, Some(run_id))
}

fn dispatch_run(task_id: &str, run_id: Option<&str>) -> CycleRequest {
    CycleRequest {
        cycle_id: "cycle-1".to_string(),
        company_id: company(),
        events: vec![CompanyEvent::TaskDispatched {
            task_id: task_id.to_string(),
            run_id: run_id.map(str::to_string),
        }],
        event_seqs: Vec::new(),
        policy: None,
    }
}

async fn artifacts_on(ops: &Arc<FsOps>, task_id: &str) -> Vec<ArtifactRecord> {
    ArtifactStore::list(&**ops, &company(), Some(task_id))
        .await
        .expect("list")
}

async fn card_after(ops: &Arc<FsOps>, task_id: &str) -> TaskRecord {
    TaskStore::list(&**ops, &company())
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.id == task_id)
        .expect("card")
}

/// Write a file, then publish it — the shape most of these scripts start with.
fn write(path: &'static str, content: &'static str) -> Turn {
    Turn::Call {
        tool: "file_write",
        args: json!({ "path": path, "content": content }),
    }
}

fn publish(path: &'static str) -> Turn {
    Turn::Call {
        tool: PUBLISH_ARTIFACT_TOOL,
        args: json!({ "path": path }),
    }
}

// ---------------------------------------------------------------------------
// The headline
// ---------------------------------------------------------------------------

/// A model, driving a real dispatch, writes a file and publishes it — and the
/// record lands with the file's content and its path as identity.
///
/// Nothing hands the tool the path out of band: the model emits the same
/// relative path it wrote to, which is what proves the two tools share one
/// sandbox view.
#[tokio::test]
async fn a_real_dispatch_publishes_a_file_the_agent_wrote() {
    let (base_url, script) = spawn_script(vec![
        write("launch.md", "# Launch spec\nShip on Friday."),
        publish("launch.md"),
        Turn::Say("Drafted the launch spec and published it."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    // The tool reached the wire under its real name.
    let advertised = advertised_tools(&script);
    assert!(
        advertised.contains(&PUBLISH_ARTIFACT_TOOL.to_string()),
        "publish_artifact was never advertised: {advertised:?}"
    );

    let artifacts = artifacts_on(&ops, "t-1").await;
    assert_eq!(artifacts.len(), 1, "one published file, one record");
    let record = &artifacts[0];
    assert_eq!(record.source.as_deref(), Some("launch.md"));
    assert_eq!(record.title, "launch.md");
    assert_eq!(record.versions.len(), 1);
    assert_eq!(
        record.versions[0].body, "# Launch spec\nShip on Friday.",
        "the stored body must be the file, not the reply"
    );
    // The reply is emphatically NOT the artifact — that is the whole change.
    assert!(
        !record.versions[0].body.contains("Drafted the launch spec"),
        "the chat reply must not be captured as a deliverable"
    );

    // No nudge: everything that changed was published.
    assert_eq!(nudge_turns(&script), 0, "nothing was left unpublished");
}

/// A re-run that republishes the same path appends a **version** to the same
/// record rather than opening a second one — the identity contract, proven
/// through two real dispatches.
#[tokio::test]
async fn a_re_run_republishing_the_same_path_adds_a_version() {
    let (base_url, script) = spawn_script(vec![
        // Run 1.
        write("launch.md", "# Launch spec\nDraft."),
        publish("launch.md"),
        Turn::Say("First draft published."),
        // Run 2.
        write("launch.md", "# Launch spec\nRevised."),
        publish("launch.md"),
        Turn::Say("Revised and republished."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("run 1");
    // Re-dispatch: the card comes back to In Progress the way a re-run starts.
    let mut again = card_after(&ops, "t-1").await;
    again.column = COLUMN_IN_PROGRESS.to_string();
    TaskStore::upsert(&*ops, &company(), &again).await.unwrap();
    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("run 2");

    let artifacts = artifacts_on(&ops, "t-1").await;
    assert_eq!(
        artifacts.len(),
        1,
        "one path, one record — never a duplicate"
    );
    let record = &artifacts[0];
    assert_eq!(record.versions.len(), 2);
    assert_eq!(record.versions[0].body, "# Launch spec\nDraft.");
    assert_eq!(record.versions[1].body, "# Launch spec\nRevised.");
    assert_eq!(nudge_turns(&script), 0);
}

/// An agent with no file grant is never offered the tool — and naming it anyway
/// does not publish anything. Advertisement is a hint; the grant is the control.
#[tokio::test]
async fn an_ungranted_agent_is_never_offered_the_publish_tool() {
    let (base_url, script) = spawn_script(vec![
        publish("launch.md"),
        Turn::Say("I could not publish anything."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"web\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let advertised = advertised_tools(&script);
    assert!(
        !advertised.contains(&PUBLISH_ARTIFACT_TOOL.to_string()),
        "an agent with no file grant must not be offered it: {advertised:?}"
    );
    assert!(
        artifacts_on(&ops, "t-1").await.is_empty(),
        "naming an ungranted tool must publish nothing"
    );
}

// ---------------------------------------------------------------------------
// The nudge
// ---------------------------------------------------------------------------

/// **The nudge lands.** Turn one writes a file and forgets to publish it; the
/// follow-up turn publishes it and the record appears.
///
/// Also pins the two properties the design turns on: **exactly one** nudge, and
/// both turns recorded against the one dispatch rather than a second attempt.
#[tokio::test]
async fn the_nudge_recovers_a_deliverable_the_agent_forgot_to_publish() {
    let (base_url, script) = spawn_script(vec![
        // Turn 1: writes, does not publish.
        write("launch.md", "# Launch spec\nShip on Friday."),
        Turn::Say("I've drafted the launch spec."),
        // The nudge turn.
        publish("launch.md"),
        Turn::Say("Published it — that one is the deliverable."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(nudge_turns(&script), 1, "exactly one nudge, never two");

    // The nudge carried the context it needs, because turns share none.
    let nudge = nudge_text(&script).expect("the nudge was sent");
    assert!(nudge.contains("launch.md"), "{nudge}");
    assert!(
        nudge.contains("Draft the launch spec"),
        "the brief: {nudge}"
    );
    assert!(
        nudge.contains("I've drafted the launch spec."),
        "the agent's own reply: {nudge}"
    );

    let artifacts = artifacts_on(&ops, "t-1").await;
    assert_eq!(artifacts.len(), 1, "the nudge recovered the deliverable");
    assert_eq!(artifacts[0].source.as_deref(), Some("launch.md"));
    assert_eq!(
        artifacts[0].versions[0].body,
        "# Launch spec\nShip on Friday."
    );

    // The card still reports the *primary* reply — the nudge is bookkeeping and
    // must not overwrite what the agent actually answered.
    let after = card_after(&ops, "t-1").await;
    let note = after.note.expect("note");
    assert!(note.contains("I've drafted the launch spec."), "{note}");
    // And because it published, there is no decline line.
    assert!(!note.contains("unpublished:"), "{note}");
}

/// **A clean decline.** The agent says the files were scratch, publishes
/// nothing, and that is the end of it: no artifact, no error, and the reason
/// kept on the card where somebody can read it.
#[tokio::test]
async fn a_declined_nudge_records_no_artifact_and_keeps_the_reason() {
    let (base_url, script) = spawn_script(vec![
        write("scratch.txt", "half-finished thinking"),
        Turn::Say("I looked into it; the answer is Friday."),
        // The nudge turn declines in prose.
        Turn::Say("That was just working notes, not a deliverable."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    // A decline must not fail the cycle.
    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("a decline is a clean outcome, not an error");

    assert_eq!(nudge_turns(&script), 1);
    assert!(
        artifacts_on(&ops, "t-1").await.is_empty(),
        "a decline produces no deliverable"
    );

    let note = card_after(&ops, "t-1").await.note.expect("note");
    assert!(
        note.contains("unpublished: scratch.txt"),
        "the files must be named on the card: {note}"
    );
    assert!(
        note.contains("That was just working notes"),
        "the *reason* is the point — it must be addressable: {note}"
    );
    // The primary reply is still there; the decline was appended, not swapped in.
    assert!(note.contains("the answer is Friday"), "{note}");
}

/// **The one-nudge bound, under the condition that would break a counter.** The
/// nudge turn writes *more* files and still publishes nothing. There is no
/// second nudge, and the warning's file list is the **pre-nudge** diff — a
/// scratch file written while answering must not become evidence against the
/// agent.
#[tokio::test]
async fn a_nudge_that_writes_more_files_does_not_trigger_a_second_nudge() {
    let (base_url, script) = spawn_script(vec![
        write("scratch.txt", "half-finished thinking"),
        Turn::Say("Looked into it."),
        // The nudge turn writes another file and still publishes nothing.
        write("more-scratch.txt", "notes about the notes"),
        Turn::Say("Both of those are working notes."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(
        nudge_turns(&script),
        1,
        "a second nudge must be impossible, not merely unlikely"
    );
    assert!(artifacts_on(&ops, "t-1").await.is_empty());

    let note = card_after(&ops, "t-1").await.note.expect("note");
    assert!(note.contains("unpublished: scratch.txt"), "{note}");
    assert!(
        !note.contains("more-scratch.txt"),
        "a file written while answering the nudge must not pollute the record: {note}"
    );
}

/// No nudge when there is nothing to nudge about: the agent wrote nothing at
/// all. Asserted on the scripted endpoint's own call count, so an extra turn
/// cannot hide.
#[tokio::test]
async fn a_run_that_wrote_nothing_is_never_nudged() {
    let (base_url, script) = spawn_script(vec![Turn::Say("The answer is Friday.")]).await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(nudge_turns(&script), 0);
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "exactly one model call: the turn itself"
    );
    assert!(artifacts_on(&ops, "t-1").await.is_empty());
    // …and no phantom decline line on the card.
    let note = card_after(&ops, "t-1").await.note.expect("note");
    assert!(!note.contains("unpublished:"), "{note}");
}

/// No nudge when the agent published everything it wrote — the gate is
/// `changed − staged`, not "did anything change".
#[tokio::test]
async fn a_run_that_published_everything_is_never_nudged() {
    let (base_url, script) = spawn_script(vec![
        write("a.md", "one"),
        write("b.md", "two"),
        publish("a.md"),
        publish("b.md"),
        Turn::Say("Both published."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(nudge_turns(&script), 0);
    assert_eq!(artifacts_on(&ops, "t-1").await.len(), 2);
}

/// **A nudge that cannot run is contained.** The provider fails the follow-up
/// turn; the run still completes cleanly, its card still lands, and the
/// fallback warning still describes the pre-nudge state.
///
/// This is the property that makes the nudge safe to add at all: it is
/// bookkeeping bolted onto a run whose work is already done, and it must never
/// be able to fail or delay that run.
#[tokio::test]
async fn a_nudge_turn_that_fails_never_fails_the_run() {
    let (base_url, script) = spawn_script(vec![
        write("scratch.txt", "thinking"),
        Turn::Say("Looked into it."),
        // The nudge turn's very first call falls over.
        Turn::Boom,
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("a failed nudge must not fail the run");

    // Attempted, not asserted to a count: the provider retries a failed request
    // internally, so one *nudge turn* can be several requests. The exact-one
    // bound is pinned by the tests above, where the turn succeeds and the count
    // is unambiguous; what matters here is only that the failure was contained.
    assert!(nudge_turns(&script) >= 1, "the nudge was attempted");
    assert!(artifacts_on(&ops, "t-1").await.is_empty());

    let after = card_after(&ops, "t-1").await;
    // The run still landed where a completed run lands.
    assert_eq!(after.column, crate::ports::tasks::COLUMN_IN_REVIEW);
    let note = after.note.expect("note");
    assert!(
        note.contains("Looked into it."),
        "the primary reply survives a failed nudge: {note}"
    );
    // No decline line, because the agent never got to answer.
    assert!(
        !note.contains("unpublished:"),
        "a nudge that never ran cannot have been declined: {note}"
    );
}

// ---------------------------------------------------------------------------
// Issue #339: the card's link to what the run produced
// ---------------------------------------------------------------------------
//
// Epic #183 §6. A task that finished used to end as a chat message — prose in a
// conversation with nothing durable to open, share, or hand to somebody else.
// These drive the same real dispatch as the tests above, under a real attempt
// row, and assert the stamp the card comes back carrying.

/// The headline: a run that published a file leaves the card pointing at that
/// file **at the version this run wrote**, under the attempt that wrote it.
#[tokio::test]
async fn a_published_card_links_to_the_version_its_run_wrote() {
    let (base_url, _script) = spawn_script(vec![
        write("launch.md", "# Launch spec\nShip on Friday."),
        publish("launch.md"),
        Turn::Say("Drafted the launch spec and published it."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();
    mint_run(&ops, "run-1", "t-1").await;

    brain
        .run_cycle(dispatch_as("t-1", "run-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let output = card_after(&ops, "t-1")
        .await
        .output
        .expect("a successful run stamps its card");
    assert_eq!(output.source.run_id(), Some("run-1"));
    assert_eq!(
        output.source.attempt(),
        Some(1),
        "the first attempt at this card"
    );
    assert_eq!(output.artifacts.len(), 1, "got {:?}", output.artifacts);

    let linked = &output.artifacts[0];
    assert_eq!(linked.version, 1, "pinned at what this run wrote");
    assert_eq!(linked.title, "launch.md");
    assert_eq!(linked.kind, crate::ports::artifacts::ArtifactKind::Markdown);
    // The link resolves: the id names a record that is actually on the card.
    let stored = artifacts_on(&ops, "t-1").await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, linked.artifact_id);
    assert!(stored[0].version(linked.version).is_some());
}

/// *"Every card has a link including tasks that produced no file"* — the
/// acceptance criterion most easily lost, because the obvious implementation
/// stamps nothing when there is no artifact.
///
/// The agent here writes nothing at all (so no nudge fires) and simply answers.
/// The trace **is** the deliverable, and the stamp says so.
#[tokio::test]
async fn a_task_that_produced_no_file_still_links_to_its_trace() {
    let (base_url, script) =
        spawn_script(vec![Turn::Say("Checked it — nothing needed changing.")]).await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();
    mint_run(&ops, "run-1", "t-1").await;

    brain
        .run_cycle(dispatch_as("t-1", "run-1"), &NoopHost)
        .await
        .expect("cycle runs");

    assert_eq!(
        nudge_turns(&script),
        0,
        "nothing was written to nudge about"
    );
    assert!(artifacts_on(&ops, "t-1").await.is_empty(), "no deliverable");

    let output = card_after(&ops, "t-1")
        .await
        .output
        .expect("no artifact must not mean no link");
    assert_eq!(output.source.run_id(), Some("run-1"));
    assert!(output.artifacts.is_empty());
    assert!(output.workflows.is_empty());
}

/// *"The link is pinned to the attempt that produced it, and a retried task
/// links to the successful one."*
///
/// Two real dispatches under two real attempts. The second republishes, so the
/// stamp moves wholesale to the second attempt and the second version — nothing
/// merges, and no read-time query has to decide which attempt wins.
#[tokio::test]
async fn a_re_run_repins_the_link_to_the_attempt_that_last_succeeded() {
    let (base_url, _script) = spawn_script(vec![
        write("launch.md", "# Launch spec\nDraft."),
        publish("launch.md"),
        Turn::Say("First draft published."),
        write("launch.md", "# Launch spec\nRevised."),
        publish("launch.md"),
        Turn::Say("Revised and republished."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();
    mint_run(&ops, "run-1", "t-1").await;
    brain
        .run_cycle(dispatch_as("t-1", "run-1"), &NoopHost)
        .await
        .expect("run 1");

    let first = card_after(&ops, "t-1").await.output.expect("stamped");
    assert_eq!(first.artifacts[0].version, 1);

    // Re-dispatch the way a re-run starts: the card comes back to In Progress.
    let mut again = card_after(&ops, "t-1").await;
    again.column = COLUMN_IN_PROGRESS.to_string();
    TaskStore::upsert(&*ops, &company(), &again).await.unwrap();
    mint_run(&ops, "run-2", "t-1").await;
    brain
        .run_cycle(dispatch_as("t-1", "run-2"), &NoopHost)
        .await
        .expect("run 2");

    let second = card_after(&ops, "t-1").await.output.expect("re-stamped");
    assert_eq!(second.source.run_id(), Some("run-2"));
    assert_eq!(second.source.attempt(), Some(2));
    assert_eq!(second.artifacts.len(), 1, "one path, one record, one link");
    assert_eq!(
        second.artifacts[0].version, 2,
        "the link must name what THIS run wrote, not the record's first draft"
    );
    assert_eq!(
        second.artifacts[0].artifact_id, first.artifacts[0].artifact_id,
        "republishing one path extends one record"
    );
    // The earlier attempt is not erased — it is still reachable as its own run.
    assert_ne!(second.source.run_id(), first.source.run_id());
}

/// *"A retried task links to the successful one."* The failing half: a later
/// attempt that fails must leave the previous success's link standing, because
/// a card whose link vanished on a retry is worse than a card with no link at
/// all — the deliverable still exists and the operator can no longer find it.
///
/// The second dispatch runs against a poisoned endpoint, so the turn errors:
/// the real `TaskRunEnd::Failed` path, not a simulated one.
#[tokio::test]
async fn a_failed_retry_does_not_erase_the_link_to_the_success_before_it() {
    let (base_url, _script) = spawn_script(vec![
        write("launch.md", "# Launch spec\nShipped."),
        publish("launch.md"),
        Turn::Say("Published."),
        // Attempt 2 falls over before it produces anything.
        Turn::Boom,
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();
    mint_run(&ops, "run-1", "t-1").await;
    brain
        .run_cycle(dispatch_as("t-1", "run-1"), &NoopHost)
        .await
        .expect("run 1");
    let succeeded = card_after(&ops, "t-1").await.output.expect("stamped");

    let mut again = card_after(&ops, "t-1").await;
    again.column = COLUMN_IN_PROGRESS.to_string();
    TaskStore::upsert(&*ops, &company(), &again).await.unwrap();
    mint_run(&ops, "run-2", "t-1").await;
    brain
        .run_cycle(dispatch_as("t-1", "run-2"), &NoopHost)
        .await
        .expect("a failed attempt still completes its cycle");

    let after = card_after(&ops, "t-1").await;
    assert_eq!(
        after.column,
        crate::ports::tasks::COLUMN_TODO,
        "a failed attempt returns the card to To-do"
    );
    let still = after
        .output
        .expect("the success's link survives the failure");
    assert_eq!(still.source.run_id(), succeeded.source.run_id());
    assert_eq!(still.artifacts, succeeded.artifacts);
}

/// *"A task whose output was later edited by a human still resolves, and says
/// so."*
///
/// The host half of that promise: the pin does not move when an operator
/// appends a version, so the card still names the revision the run produced
/// while the record grows past it. The console reads exactly this difference —
/// pinned version versus latest — to say a human edited it since.
#[tokio::test]
async fn an_operator_edit_leaves_the_pinned_link_naming_what_the_run_wrote() {
    use crate::ports::artifacts::ArtifactAuthor;

    let (base_url, _script) = spawn_script(vec![
        write("launch.md", "# Launch spec\nAgent draft."),
        publish("launch.md"),
        Turn::Say("Published."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();
    mint_run(&ops, "run-1", "t-1").await;
    brain
        .run_cycle(dispatch_as("t-1", "run-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let pinned = card_after(&ops, "t-1").await.output.expect("stamped");
    assert_eq!(pinned.artifacts[0].version, 1);

    // A human edits the deliverable after the run — the console's "Edit as
    // operator" path, which appends rather than overwriting.
    let mut record = artifacts_on(&ops, "t-1").await.remove(0);
    record.push_version(
        "# Launch spec\nOperator's wording.",
        ArtifactAuthor::Operator,
        "operator",
        99,
        Some("operator edit before approval".to_string()),
    );
    ArtifactStore::upsert(&*ops, &company(), &record)
        .await
        .unwrap();

    // The card is untouched: nothing about an edit rewrites the run's stamp.
    let after = card_after(&ops, "t-1").await.output.expect("still stamped");
    assert_eq!(
        after.artifacts[0].version, 1,
        "the pin does not follow edits"
    );

    // And it still resolves — to the agent's revision, not the human's.
    let stored = artifacts_on(&ops, "t-1").await.remove(0);
    let resolved = stored
        .version(after.artifacts[0].version)
        .expect("a pinned version can never dangle: versions are append-only");
    assert_eq!(resolved.author, ArtifactAuthor::Agent);
    assert_eq!(resolved.body, "# Launch spec\nAgent draft.");
    // The newer version is what the console names in its "edited since" line.
    assert_eq!(stored.latest().unwrap().version, 2);
    assert_eq!(stored.latest().unwrap().author, ArtifactAuthor::Operator);
}

/// A dispatch with no attempt row has nothing addressable to point at, so it
/// stamps nothing rather than inventing an id. Fail silent, not closed: the
/// card lands exactly as it did before #339, and the artifact is still
/// recorded.
#[tokio::test]
async fn a_dispatch_with_no_attempt_row_stamps_nothing() {
    let (base_url, _script) = spawn_script(vec![
        write("launch.md", "# Launch spec"),
        publish("launch.md"),
        Turn::Say("Published."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    // `run_id: None` — the choke point minted no row.
    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let after = card_after(&ops, "t-1").await;
    assert!(after.output.is_none(), "no attempt, no link to it");
    assert_eq!(
        artifacts_on(&ops, "t-1").await.len(),
        1,
        "the deliverable is still recorded — only the card's link is missing"
    );
    assert_eq!(after.column, crate::ports::tasks::COLUMN_IN_REVIEW);
}

// ---------------------------------------------------------------------------
// Issue #445: publishing outside a task run
// ---------------------------------------------------------------------------

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

/// Every card on the board, oldest-id first — a chat publish mints one, so the
/// tests have to find a card they did not create.
async fn all_cards(ops: &Arc<FsOps>) -> Vec<TaskRecord> {
    let mut cards = TaskStore::list(&**ops, &company()).await.expect("list");
    cards.sort_by(|a, b| a.id.cmp(&b.id));
    cards
}

/// **The headline for #445.** A model publishes during a *conversation* — no
/// card, no dispatch, no run — and the file must end up somewhere the operator
/// can open.
///
/// Before this the tool returned success, the queue was never drained, and the
/// next turn cleared it. Everything was green and the deliverable was gone, so
/// this asserts on the **stored artifact**, not on the receipt.
#[tokio::test]
async fn a_conversation_publish_is_recorded_on_a_card_it_mints() {
    let (base_url, _script) = spawn_script(vec![
        write("brief.md", "# Brief\nThe thing you asked for."),
        publish("brief.md"),
        Turn::Say("Written up and published."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());

    // No card exists, and none is dispatched — this is purely a chat turn.
    assert!(all_cards(&ops).await.is_empty());

    brain
        .run_cycle(chat("Draft me a brief."), &NoopHost)
        .await
        .expect("cycle runs");

    // A card was minted to carry the deliverable…
    let cards = all_cards(&ops).await;
    assert_eq!(
        cards.len(),
        1,
        "publishing in a conversation must mint exactly one card: {cards:?}"
    );
    let card = &cards[0];
    assert_eq!(
        card.column, COLUMN_IN_REVIEW,
        "finished agent work awaiting a person"
    );
    assert_eq!(card.assignee, AGENT);
    assert_eq!(card.title, "brief.md", "titled from what was published");
    assert!(
        card.note
            .as_deref()
            .unwrap_or_default()
            .contains("conversation"),
        "the card must explain why it exists: {:?}",
        card.note
    );
    // …and it is reachable by the one route the console actually reads.
    let artifacts = artifacts_on(&ops, &card.id).await;
    assert_eq!(artifacts.len(), 1, "the deliverable must be on the card");
    assert_eq!(artifacts[0].source.as_deref(), Some("brief.md"));
    assert_eq!(
        artifacts[0].versions[0].body, "# Brief\nThe thing you asked for.",
        "the stored body must be the file the agent wrote"
    );
}

/// The receipt the *model* was handed must name the destination it actually
/// got. This reads the wire, so it pins what the agent was told — the sentence
/// that, when wrong, is laundered into a false claim to the operator.
#[tokio::test]
async fn a_conversation_publish_receipt_does_not_promise_a_task_tab() {
    let (base_url, script) = spawn_script(vec![
        write("brief.md", "# Brief"),
        publish("brief.md"),
        Turn::Say("Published."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    brain
        .run_cycle(chat("Draft me a brief."), &NoopHost)
        .await
        .expect("cycle runs");
    let _ = &ops;

    // Find the tool result the model was sent back.
    let receipts: Vec<String> = script
        .seen
        .lock()
        .unwrap()
        .iter()
        .flat_map(|body| {
            body.get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| m.get("content").and_then(Value::as_str).map(str::to_string))
        .filter(|c| c.contains("Published `brief.md`"))
        .collect();

    assert!(
        !receipts.is_empty(),
        "the model was never handed a publish receipt"
    );
    for receipt in &receipts {
        assert!(
            !receipt.contains("this task's Artifacts tab"),
            "a chat turn has no task, so the receipt must not name one: {receipt}"
        );
        assert!(
            receipt.contains("card"),
            "the receipt must name where the file actually went: {receipt}"
        );
    }
}

/// The other half of the floor: with no artifact store there is no claim, so
/// the tool must **refuse** rather than stage into a queue nothing drains.
///
/// `build_agent` already declines to wire the tool without a store, so this
/// proves the two gates agree — the agent cannot publish, by either route.
#[tokio::test]
async fn a_conversation_without_an_artifact_store_cannot_publish() {
    let (base_url, script) = spawn_script(vec![
        write("brief.md", "# Brief"),
        publish("brief.md"),
        Turn::Say("Tried to publish."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain_without_artifacts(base_url, dir.path());

    brain
        .run_cycle(chat("Draft me a brief."), &NoopHost)
        .await
        .expect("cycle runs");

    assert!(
        all_cards(&ops).await.is_empty(),
        "nothing to record means no card is minted"
    );
    // The tool is not even advertised — the failure is closed at both ends.
    let advertised = advertised_tools(&script);
    assert!(
        !advertised.contains(&PUBLISH_ARTIFACT_TOOL.to_string()),
        "publish_artifact must not be offered without a store: {advertised:?}"
    );
}

/// A dispatched card must behave **exactly** as it did before #445 — the
/// artifact lands on the card that was dispatched, and no extra card appears.
#[tokio::test]
async fn a_task_run_still_files_onto_its_own_card_and_mints_nothing() {
    let (base_url, _script) = spawn_script(vec![
        write("launch.md", "# Launch spec"),
        publish("launch.md"),
        Turn::Say("Published."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (brain, ops) = brain(base_url, "\"*\"", dir.path());
    TaskStore::upsert(&*ops, &company(), &card("t-1"))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let cards = all_cards(&ops).await;
    assert_eq!(
        cards.len(),
        1,
        "a task run must not mint a second card: {cards:?}"
    );
    assert_eq!(cards[0].id, "t-1");
    assert_eq!(artifacts_on(&ops, "t-1").await.len(), 1);
}
