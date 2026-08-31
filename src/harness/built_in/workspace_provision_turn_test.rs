//! End-to-end proof that an agent granted `files` and **not** `shell` can write
//! a relative path on a company that has never run (issue #409).
//!
//! The policy-level tests in [`build`](crate::harness::build) pin what the guard
//! does with a missing directory. They cannot tell you whether a real agent ever
//! reaches that state, and the whole bug is a state that only exists *before
//! anything has run*: the first thing a `shell` command does is create the
//! working directory as a side effect, so an agent holding that grant never sees
//! it. The difference between a working and a broken agent was a grant that
//! looks unrelated to files.
//!
//! So this drives the **real** brain through `run_cycle` on a `TaskDispatched`
//! event, with a real `HarnessPool`, real `build_agent`, real `HostedProvider`
//! (`tool_calling: true`, so the turn runs on the production native tool loop),
//! and a workspace root that starts out completely empty. Only the model's
//! choices are scripted, against a loopback OpenAI-compatible endpoint.
//!
//! Three shapes are covered, matching the three ways an agent's storage first
//! comes into existence: a manifest teammate on a company that has never run, a
//! teammate on a company whose bundle was just minted, and an overlay teammate
//! added at runtime with no manifest row at all.

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
use crate::harness::{HarnessBrain, HarnessDeps, HarnessPool};
use crate::ports::brain::{Brain, CycleHost};
use crate::ports::tasks::{COLUMN_IN_PROGRESS, TaskRecord, TaskStore};
use crate::ports::types::{
    ApprovalId, CompanyEvent, CompanyId, CompanyRecord, ContextOp, ContextOpResult, CycleRequest,
    Effect, EffectDisposition, OverlayAgent, ToolCall, ToolResult,
};
use crate::store::{FsCompanyStore, FsContextStore, FsOps};

/// The manifest teammate this fixture dispatches to.
const AGENT: &str = "writer";

/// A teammate that exists only as a runtime overlay — no manifest row.
const OVERLAY_AGENT: &str = "analyst";

/// The relative path the scripted model writes to. Deliberately nested: the
/// parent does not exist either, which is the ordinary case for a first write.
const REL_PATH: &str = "notes/hello.md";

const BODY: &str = "the first thing this agent ever wrote";

/// The refusal this issue is about, verbatim enough to match on.
const ESCAPE_REFUSAL: &str = "escapes workspace";

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

/// A loopback OpenAI-compatible endpoint that always tries to write one file.
///
/// Deliberately **stateless** rather than a scripted queue of turns. These tests
/// run more than one cycle against the same endpoint, and a turn does not always
/// take the same number of model round trips — a queue silently slides out of
/// alignment and hands the second cycle the first cycle's closing text, so the
/// write under test never happens and the test reports a missing file instead of
/// the truth. The rule here is per-request and cannot drift: **answer with the
/// tool call, unless we have just been handed a tool result, in which case
/// close the turn with text.**
struct Script {
    /// The relative path the model asks to write on every turn.
    path: String,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: Mutex<Vec<Value>>,
}

/// Whether the request's last message is a tool result — i.e. the model has
/// already had its call answered and should now finish.
fn just_got_a_tool_result(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.last())
        .and_then(|last| last.get("role"))
        .and_then(Value::as_str)
        == Some("tool")
}

async fn spawn_script(path: &str) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        path: path.to_string(),
        seen: Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&script);
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let script = Arc::clone(&handle);
            async move {
                script.seen.lock().unwrap().push(body.clone());
                let message = if just_got_a_tool_result(&body) {
                    json!({ "role": "assistant", "content": "Wrote the note." })
                } else {
                    let args = json!({ "path": script.path, "content": BODY });
                    json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call-file_write",
                            "type": "function",
                            "function": { "name": "file_write", "arguments": args.to_string() }
                        }]
                    })
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

/// Every tool name the scripted model was offered.
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

/// Everything the harness fed back to the model as a `tool` message — i.e. what
/// the tool actually answered. This is where a refusal shows up.
fn tool_results(script: &Script) -> Vec<String> {
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
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| m.get("content").and_then(Value::as_str).map(str::to_string))
        .collect()
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

/// A one-agent company granted `files` and **nothing else**. No `shell`: that
/// omission is the entire point — with `shell` the workspace is created as a
/// side effect of the first command and the bug is invisible.
fn manifest() -> CompanyManifest {
    toml::from_str(&format!(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["files"]

[[agent]]
id = "{AGENT}"
role = "Writer"
"#
    ))
    .expect("manifest parses")
}

fn company() -> CompanyId {
    CompanyId::new("acme")
}

fn record(overlays: Vec<OverlayAgent>) -> CompanyRecord {
    CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: company(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: overlays,
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

/// Wire a real brain against the scripted endpoint, rooted at `dir`.
fn build_brain(
    base_url: String,
    dir: &std::path::Path,
    overlays: Vec<OverlayAgent>,
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
        // The agent workspaces hang off here. Nothing has created a single
        // directory under it — that is the precondition under test.
        workspace_root: dir.join("harness"),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.join("harness"),
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
    (
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record(overlays)),
        ops,
    )
}

fn card(id: &str, assignee: &str) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        title: "Write the first note".to_string(),
        note: None,
        column: COLUMN_IN_PROGRESS.to_string(),
        priority: "medium".to_string(),
        assignee: assignee.to_string(),
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
    CycleRequest {
        cycle_id: "cycle-1".to_string(),
        company_id: company(),
        events: vec![CompanyEvent::TaskDispatched {
            task_id: task_id.to_string(),
            run_id: None,
        }],
        event_seqs: Vec::new(),
        policy: None,
    }
}

/// Run one dispatched card to `assignee` with the model scripted to write
/// [`REL_PATH`], and hand back what the tool answered plus the workspace root.
async fn run_write_turn(
    dir: &std::path::Path,
    assignee: &str,
    overlays: Vec<OverlayAgent>,
) -> (Arc<Script>, std::path::PathBuf) {
    let (base_url, script) = spawn_script(REL_PATH).await;

    let (brain, ops) = build_brain(base_url, dir, overlays);
    TaskStore::upsert(&*ops, &company(), &card("t-1", assignee))
        .await
        .unwrap();

    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let workspace =
        crate::harness::build::agent_workspace(&dir.join("harness"), &company(), assignee);
    (script, workspace)
}

/// The tool answered, the bytes landed, and nothing was refused as an escape.
fn assert_the_write_landed(script: &Script, workspace: &std::path::Path) {
    let results = tool_results(script);
    assert!(
        !results.iter().any(|r| r.contains(ESCAPE_REFUSAL)),
        "the write was refused as a sandbox escape: {results:?}"
    );
    let written = workspace.join(REL_PATH);
    assert!(
        written.is_file(),
        "nothing was written at {} (tool said: {results:?})",
        written.display()
    );
    assert_eq!(
        std::fs::read_to_string(&written).expect("read back"),
        BODY,
        "the file exists but does not hold what the agent wrote"
    );
}

// ---------------------------------------------------------------------------
// The headline
// ---------------------------------------------------------------------------

/// A company that has never run. No workspace root, no company directory, no
/// agent directory — nothing on disk at all. The agent holds `files` and not
/// `shell`, so nothing incidental has created its working directory, and every
/// relative write used to come back *"Resolved parent path escapes workspace"*.
#[tokio::test]
async fn an_agent_with_files_and_no_shell_writes_on_a_company_that_has_never_run() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !dir.path().join("harness").exists(),
        "precondition: the workspace root does not exist"
    );

    let (script, workspace) = run_write_turn(dir.path(), AGENT, Vec::new()).await;

    // The grant shape this issue turns on: file tools present, shell absent.
    let advertised = advertised_tools(&script);
    assert!(
        advertised.contains(&"file_write".to_string()),
        "file_write was never advertised: {advertised:?}"
    );
    assert!(
        !advertised.contains(&"shell".to_string()),
        "this agent must NOT hold shell — with it the bug is invisible: {advertised:?}"
    );

    assert_the_write_landed(&script, &workspace);
}

/// The same proof for a teammate that has **no manifest row** — an overlay agent
/// added at runtime through the console or the orchestrator's `add_agent` tool.
/// Adding one writes a store record and touches no filesystem at all, so its
/// workspace is minted only when the roster is next rebuilt.
#[tokio::test]
async fn an_overlay_teammate_added_at_runtime_writes_on_its_first_turn() {
    let dir = tempfile::tempdir().unwrap();
    let overlay = OverlayAgent {
        id: OVERLAY_AGENT.to_string(),
        name: "Analyst".to_string(),
        role: "Analyst".to_string(),
        description: Some("Reads the numbers.".to_string()),
        tools: None,
        model: None,
        harness: None,
    };

    let (script, workspace) = run_write_turn(dir.path(), OVERLAY_AGENT, vec![overlay]).await;

    assert!(
        workspace.ends_with(std::path::Path::new(OVERLAY_AGENT).join("workspace")),
        "the overlay teammate must get its own sandbox: {}",
        workspace.display()
    );
    assert_the_write_landed(&script, &workspace);
}

/// A workspace that disappears *after* the roster was built is repaired before
/// the next turn's tools run.
///
/// This is the half `build_agent` alone cannot cover. A roster is built once and
/// then cached behind fingerprints — and handed across an in-place rebuild — so
/// on the second cycle `build_agent` never runs again. The runtime does write
/// its own bookkeeping into the workspace (session transcripts, the TinyAgents
/// journal), which re-creates the directory as a side effect, but that happens
/// at the *end* of a turn: without a dispatch-time guarantee the write in the
/// very next turn is refused, exactly as it was on a company that had never run.
///
/// Real shapes this covers: a restored or wiped data directory under a live
/// host, an operator clearing the tree, a boot that raced a not-yet-mounted
/// volume.
#[tokio::test]
async fn a_workspace_removed_under_a_live_host_is_repaired_before_the_next_write() {
    let dir = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script(REL_PATH).await;

    let (brain, ops) = build_brain(base_url, dir.path(), Vec::new());
    TaskStore::upsert(&*ops, &company(), &card("t-1", AGENT))
        .await
        .unwrap();
    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("first cycle");

    let workspace =
        crate::harness::build::agent_workspace(&dir.path().join("harness"), &company(), AGENT);
    assert_the_write_landed(&script, &workspace);

    // The tree goes away under a running host. The roster in the pool is
    // unchanged, so nothing rebuilds it.
    std::fs::remove_dir_all(&workspace).expect("remove the workspace");
    assert!(!workspace.exists());

    // A second card, because the first one settled: the point is a *later* turn
    // on the same live pool, not a re-run of the same dispatch.
    TaskStore::upsert(&*ops, &company(), &card("t-2", AGENT))
        .await
        .unwrap();
    brain
        .run_cycle(dispatch("t-2"), &NoopHost)
        .await
        .expect("second cycle");

    assert_the_write_landed(&script, &workspace);
}

/// Provisioning is not a licence: the same freshly-provisioned agent still
/// cannot reach outside its sandbox. A `..` traversal is refused, and the
/// refusal reaches the model rather than silently succeeding.
#[tokio::test]
async fn a_traversal_from_a_provisioned_workspace_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script("../../../../loot.md").await;

    let (brain, ops) = build_brain(base_url, dir.path(), Vec::new());
    TaskStore::upsert(&*ops, &company(), &card("t-1", AGENT))
        .await
        .unwrap();
    brain
        .run_cycle(dispatch("t-1"), &NoopHost)
        .await
        .expect("cycle runs");

    let results = tool_results(&script);
    assert!(
        results
            .iter()
            .any(|r| r.contains("Path not allowed by security policy")),
        "a traversal must be refused: {results:?}"
    );
    // The refusal reads nothing like the missing-workspace one, because it is
    // caught by the string-level check before any path is resolved.
    assert!(
        !results.iter().any(|r| r.contains(ESCAPE_REFUSAL)),
        "a `..` traversal must not be reported as a resolved-parent escape: {results:?}"
    );
    assert!(
        !dir.path().join("loot.md").exists(),
        "the traversal wrote outside the sandbox"
    );
}
