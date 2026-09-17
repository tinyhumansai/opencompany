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
use crate::ports::brain::CycleHost;
use crate::ports::tasks::{COLUMN_IN_PROGRESS, TaskRecord, TaskStore, TaskTitle};
use crate::ports::types::{
    ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult, CycleRequest,
    Effect, EffectDisposition, ToolCall, ToolResult,
};
use crate::store::{FsCompanyStore, FsContextStore, FsOps};

/// The agent this fixture dispatches to. Its workspace is
/// `{root}/acme/ceo/workspace`.
pub(crate) const AGENT: &str = "ceo";

/// A marker unique to the nudge instruction, used to count nudge turns.
pub(crate) const NUDGE_MARKER: &str = "published none of them";

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

/// What the scripted model does on each successive call.
#[derive(Clone, Debug)]
pub(crate) enum Turn {
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
pub(crate) struct Script {
    pub(crate) turns: Mutex<Vec<Turn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    pub(crate) seen: Mutex<Vec<Value>>,
    /// Set once [`Turn::Boom`] is reached; every later request fails too.
    pub(crate) poisoned: std::sync::atomic::AtomicBool,
}

/// Serve the script on loopback and return its base URL plus the shared handle.
pub(crate) async fn spawn_script(turns: Vec<Turn>) -> (String, Arc<Script>) {
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
pub(crate) fn tool_call_message(tool: &str, args: &Value) -> Value {
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
pub(crate) fn advertised_tools(script: &Script) -> Vec<String> {
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
pub(crate) fn nudge_turns(script: &Script) -> usize {
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
pub(crate) fn nudge_text(script: &Script) -> Option<String> {
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
pub(super) struct NoopHost;

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
pub(crate) fn manifest(grants: &str) -> CompanyManifest {
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
pub(crate) fn brain(
    base_url: String,
    grants: &str,
    dir: &std::path::Path,
) -> (HarnessBrain, Arc<FsOps>) {
    brain_with(base_url, grants, dir, true)
}

/// The same brain with **no artifact store** (issue #445).
///
/// The fail-closed case: nothing can record a deliverable, so `build_agent`
/// does not offer the tool and the brain never claims the publish queue.
pub(crate) fn brain_without_artifacts(
    base_url: String,
    dir: &std::path::Path,
) -> (HarnessBrain, Arc<FsOps>) {
    brain_with(base_url, "\"*\"", dir, false)
}

pub(crate) fn brain_with(
    base_url: String,
    grants: &str,
    dir: &std::path::Path,
    with_artifacts: bool,
) -> (HarnessBrain, Arc<FsOps>) {
    let ops = Arc::new(FsOps::new(dir));
    let deps = HarnessDeps {
        emergency_gate: None,
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
        overlay_desk_hive: Vec::new(),
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
pub(crate) async fn mint_run(ops: &Arc<FsOps>, run_id: &str, task_id: &str) {
    use crate::ports::RunStore;
    use crate::ports::runs::NewRun;
    RunStore::create_run(&**ops, &company(), NewRun::for_task(run_id, task_id, AGENT))
        .await
        .expect("mint the attempt row");
}

pub(crate) fn company() -> CompanyId {
    CompanyId::new("acme")
}

/// A dispatched card, already in the column dispatch happens from.
pub(crate) fn card(id: &str) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        title: TaskTitle::authored("Draft the launch spec"),
        note: None,
        column: COLUMN_IN_PROGRESS.to_string(),
        priority: "medium".to_string(),
        assignee: AGENT.to_string(),
        updated_at_millis: 1,
        origin: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
    }
}

pub(crate) fn dispatch(task_id: &str) -> CycleRequest {
    dispatch_run(task_id, None)
}

/// A dispatch under a named attempt (issue #339) — what a real choke point
/// sends, and the only shape that can produce an output stamp.
pub(crate) fn dispatch_as(task_id: &str, run_id: &str) -> CycleRequest {
    dispatch_run(task_id, Some(run_id))
}

pub(crate) fn dispatch_run(task_id: &str, run_id: Option<&str>) -> CycleRequest {
    CycleRequest {
        cycle_id: "cycle-1".to_string(),
        company_id: company(),
        events: vec![CompanyEvent::TaskDispatched {
            task_id: task_id.to_string(),
            run_id: run_id.map(str::to_string),
            origin_chat_id: None,
            origin_parent: None,
        }],
        event_seqs: Vec::new(),
        policy: None,
    }
}

pub(crate) async fn artifacts_on(ops: &Arc<FsOps>, task_id: &str) -> Vec<ArtifactRecord> {
    ArtifactStore::list(&**ops, &company(), Some(task_id))
        .await
        .expect("list")
}

pub(crate) async fn card_after(ops: &Arc<FsOps>, task_id: &str) -> TaskRecord {
    TaskStore::list(&**ops, &company())
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.id == task_id)
        .expect("card")
}

/// Write a file, then publish it — the shape most of these scripts start with.
pub(crate) fn write(path: &'static str, content: &'static str) -> Turn {
    Turn::Call {
        tool: "file_write",
        args: json!({ "path": path, "content": content }),
    }
}

pub(crate) fn publish(path: &'static str) -> Turn {
    Turn::Call {
        tool: PUBLISH_ARTIFACT_TOOL,
        args: json!({ "path": path }),
    }
}
