//! End-to-end proof that the workspace tools (issue #237) actually work when a
//! *model* drives them — not just when a unit test calls `execute()` directly.
//!
//! Unit tests in [`workspace_tools`](crate::harness::workspace_tools) pin the
//! tools' own behaviour. They cannot tell you whether a tool is reachable from a
//! real turn: whether it survives `build_agent`'s grant gates, is advertised on
//! the wire in the shape the provider emits, passes the [`ApprovalPolicy`] gate,
//! dispatches through openhuman's native tool loop, and hands its result back
//! into the model's context. Every one of those is a place the wiring can be
//! silently wrong while the tools themselves are perfect.
//!
//! So this drives the **real** harness — real `HarnessPool`, real
//! `build_agent`, real `HostedProvider` (which advertises `tool_calling: true`,
//! putting the turn on the production `NativeToolDispatcher` path), real
//! `ApprovalPolicy`, real `FsOps`-backed `WorkspaceStore` — and stubs only the
//! one thing that needs a credential: the model's *choices*. A scripted
//! OpenAI-compatible endpoint on loopback returns the `tool_calls` a model would
//! return.
//!
//! The load-bearing detail is that the stub reads the revision token **out of
//! the conversation it is sent**, exactly as a model would. Nothing hands it the
//! value out of band. If the read tool stopped emitting `rev=…`, or the tool
//! result stopped reaching the model's context, the scripted write would fail to
//! find a revision and the test would fail rather than quietly passing.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use crate::company::CompanyManifest;
use crate::company::credentials::Credential;
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};
use crate::store::{FsCompanyStore, FsContextStore, FsOps};

/// What the scripted model does on each successive call.
#[derive(Clone, Debug)]
enum Turn {
    /// Emit a tool call with these literal arguments.
    Call { tool: &'static str, args: Value },
    /// Emit a tool call whose args are built from the revision the conversation
    /// has already carried back (`rev=<n>` in a tool result), proving the token
    /// really travels read → model → write.
    WriteWithObservedRev {
        path: &'static str,
        content: &'static str,
        /// Offset applied to the observed revision. `0` writes with the current
        /// revision (must land); a non-zero value fakes a stale read.
        delta: i64,
    },
    /// Finish the turn with plain assistant text.
    Say(&'static str),
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
struct Script {
    turns: Mutex<Vec<Turn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: Mutex<Vec<Value>>,
}

/// Sent as `expected_updated_at` when [`observed_rev`] finds no `rev=` token at
/// all, i.e. the revision never reached the model's context.
///
/// Without it these tests pass for the wrong reason: a missing revision used to
/// fall back to `0`, and `0` is refused with the very "changed since you read
/// it" message the stale-write test asserts on — so the test stayed green even
/// when the read → model → write round trip it exists to prove was broken. No
/// real note can carry this revision, so asserting it never appears in a tool
/// result turns that silent pass into a failure.
const UNOBSERVED_REV: u64 = u64::MAX;

/// Pull the most recent `rev=<digits>` out of the conversation the stub was
/// sent. This is the model's-eye view: the revision is only available because
/// `workspace_read`'s result was fed back into the context.
fn observed_rev(body: &Value) -> Option<u64> {
    let messages = body.get("messages")?.as_array()?;
    let mut found = None;
    for message in messages {
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        let mut rest = content;
        while let Some(at) = rest.find("rev=") {
            let digits: String = rest[at + 4..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(value) = digits.parse::<u64>() {
                found = Some(value);
            }
            rest = &rest[at + 4..];
        }
    }
    found
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
                // Running off the end of the script means the turn looped more
                // than expected; end it with text rather than hanging.
                let next = next.unwrap_or(Turn::Say("done"));
                let message = match next {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(tool, &args),
                    Turn::WriteWithObservedRev {
                        path,
                        content,
                        delta,
                    } => {
                        let rev = match observed_rev(&body) {
                            Some(rev) => (rev as i64 + delta).max(0) as u64,
                            None => UNOBSERVED_REV,
                        };
                        tool_call_message(
                            "workspace_write",
                            &json!({
                                "path": path,
                                "content": content,
                                "expected_updated_at": rev,
                            }),
                        )
                    }
                };
                Json(json!({
                    "choices": [{ "index": 0, "message": message }],
                    "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
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

fn folder(id: &str, name: &str) -> WorkspaceNode {
    WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::Folder,
        parent_id: None,
        updated_at_millis: crate::ports::now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    }
}

fn note(id: &str, name: &str, parent: &str) -> WorkspaceNode {
    WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::File,
        parent_id: Some(parent.to_string()),
        updated_at_millis: crate::ports::now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    }
}

/// A one-agent company, with `grants` controlling the workspace surface.
fn manifest(grants: &str) -> CompanyManifest {
    // `full` so an ordinary turn is not parked; the write tool's own
    // compare-and-swap token is what guards the write in this mode.
    manifest_in_mode(grants, "full")
}

fn manifest_in_mode(grants: &str, mode: &str) -> CompanyManifest {
    toml::from_str(&format!(
        r#"
[company]
name = "Acme"

[policy]
mode = "{mode}"

[tools]
allow = [{grants}]

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"
"#
    ))
    .expect("manifest parses")
}

/// Wire a real harness against the scripted endpoint and a seeded workspace.
///
/// Returns the pool, deps, record and the live store so a test can read back
/// what the turn actually persisted.
async fn harness(
    base_url: String,
    grants: &str,
    dir: &std::path::Path,
) -> (
    HarnessPool,
    HarnessDeps,
    CompanyRecord,
    Arc<dyn WorkspaceStore>,
) {
    let store: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir));
    let id = CompanyId::new("acme");
    store
        .create(&id, &folder("f-std", "standards"), None)
        .await
        .unwrap();
    store
        .create(
            &id,
            &note("n-eng", "engineering-standards.md", "f-std"),
            Some("# Engineering\nReview every PR before merge."),
        )
        .await
        .unwrap();

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
        meter: None,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
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
        workspace: Some(store.clone()),
        // Issue #238's metered search is off in this fixture: the turn under
        // test exercises the #237 workspace path only, and no managed search
        // backend is the fail-closed default outside the runtime builder.
        search: None,
        tenant_search: None,
        workflow_runs: None,
        deep_trace: None,
    };

    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id,
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

    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await.expect("pool ensures");
    (pool, deps, record, store)
}

/// Every tool name the scripted model was offered across the whole turn.
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

/// Every tool *result* the harness fed back to the model.
fn tool_results(script: &Script) -> Vec<String> {
    script
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|body| body.get("messages").and_then(Value::as_array).cloned())
        .flatten()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| m.get("content").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// The headline proof: a model, driving a real turn, discovers the workspace,
/// reads a note, and revises it — with the revision token making the full
/// round trip through the model's own context.
#[tokio::test]
async fn a_real_turn_lists_reads_and_revises_a_workspace_note() {
    let (base_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_list",
            args: json!({}),
        },
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/engineering-standards.md" }),
        },
        Turn::WriteWithObservedRev {
            path: "standards/engineering-standards.md",
            content: "# Engineering\nReview every PR before merge.\nShip on Fridays.",
            delta: 0,
        },
        Turn::Say("Updated the engineering standards."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    // An explicit `workspace` grant — writes must be opted into by name.
    let (pool, deps, record, store) = harness(base_url, "\"workspace\"", dir.path()).await;

    let outcome = pool
        .run(
            &record.id,
            "ceo",
            "Add a Friday shipping rule to our standards.",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn runs");
    assert!(
        outcome.reply.contains("Updated the engineering standards."),
        "{}",
        outcome.reply
    );

    // The tools reached the wire under their real names.
    let advertised = advertised_tools(&script);
    for tool in ["workspace_list", "workspace_read", "workspace_write"] {
        assert!(
            advertised.contains(&tool.to_string()),
            "`{tool}` was never advertised to the model: {advertised:?}"
        );
    }

    // The model saw the seeded tree, the note body, and the revision token.
    let results = tool_results(&script);
    let joined = results.join("\n---\n");
    assert!(
        joined.contains("standards/engineering-standards.md"),
        "the listing never reached the model: {joined}"
    );
    assert!(
        joined.contains("Review every PR before merge."),
        "the note body never reached the model: {joined}"
    );
    assert!(
        joined.contains("BEGIN WORKSPACE NOTE"),
        "the untrusted-content fence is missing: {joined}"
    );
    assert!(
        joined.contains("Overwrote the workspace note"),
        "the write did not report success: {joined}"
    );

    // And — the point of the whole exercise — the edit is durable in the store
    // the console reads from, not just in the transcript.
    let (node, body) = store
        .read(&record.id, "n-eng")
        .await
        .unwrap()
        .expect("note still present");
    assert_eq!(
        body,
        "# Engineering\nReview every PR before merge.\nShip on Fridays."
    );
    assert_eq!(node.name, "engineering-standards.md");
}

/// The compare-and-swap guard, proven through a real turn: a model writing with
/// a revision that is not current is refused, the note is untouched, and the
/// refusal is fed back so the agent can recover rather than retry blindly.
#[tokio::test]
async fn a_real_turn_is_refused_when_it_writes_with_a_stale_revision() {
    let (base_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/engineering-standards.md" }),
        },
        // Pretend the operator edited the note between read and write.
        Turn::WriteWithObservedRev {
            path: "standards/engineering-standards.md",
            content: "clobbered",
            delta: -1,
        },
        Turn::Say("I could not apply that edit."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, store) = harness(base_url, "\"workspace\"", dir.path()).await;

    let (before, _) = store.read(&record.id, "n-eng").await.unwrap().unwrap();

    pool.run(
        &record.id,
        "ceo",
        "Rewrite the standards.",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    let joined = tool_results(&script).join("\n---\n");
    // Before anything else: the write must have been built from a revision the
    // model actually saw. Otherwise the refusal below proves nothing — a
    // never-observed revision is refused with the same message.
    assert!(
        !joined.contains(&UNOBSERVED_REV.to_string()),
        "no `rev=` reached the model, so the refusal below is not evidence of a \
         stale-revision check: {joined}"
    );
    assert!(
        joined.contains("changed since you read it"),
        "the stale write was not refused: {joined}"
    );

    let (after, body) = store.read(&record.id, "n-eng").await.unwrap().unwrap();
    assert_eq!(
        body, "# Engineering\nReview every PR before merge.",
        "a stale write clobbered the note"
    );
    // A write that failed only after touching metadata would leave the body
    // intact and still bump the revision, invalidating every other agent's
    // token for no reason.
    assert_eq!(
        after.updated_at_millis, before.updated_at_millis,
        "a refused write must not bump the revision"
    );
}

/// The grant asymmetry, proven through a real turn: under a bare `*` the model
/// is offered the read tools and NOT `workspace_write`, so it cannot revise
/// operator-owned guidance even if it tries.
#[tokio::test]
async fn a_wildcard_grant_turn_can_read_but_is_never_offered_the_write_tool() {
    let (base_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/engineering-standards.md" }),
        },
        // Nothing stops a model naming a tool it was never offered. Under a
        // bare `*` the write must be *refused*, not merely left unadvertised —
        // advertisement is a hint, the grant check is the control.
        Turn::WriteWithObservedRev {
            path: "standards/engineering-standards.md",
            content: "clobbered",
            delta: 0,
        },
        Turn::Say("Our standard is to review every PR before merge."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, store) = harness(base_url, "\"*\"", dir.path()).await;

    let outcome = pool
        .run(
            &record.id,
            "ceo",
            "What is our review standard?",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn runs");
    assert!(
        outcome.reply.contains("review every PR"),
        "{}",
        outcome.reply
    );

    let advertised = advertised_tools(&script);
    assert!(
        advertised.contains(&"workspace_read".to_string()),
        "a `*` grant must still confer reads: {advertised:?}"
    );
    assert!(
        !advertised.contains(&"workspace_write".to_string()),
        "a bare `*` grant must NEVER offer the write tool: {advertised:?}"
    );

    let (_, body) = store.read(&record.id, "n-eng").await.unwrap().unwrap();
    assert_eq!(
        body, "# Engineering\nReview every PR before merge.",
        "the unadvertised write tool was called anyway and went through — a bare \
         `*` must refuse it at the grant check, not just omit it from the list"
    );
}

/// Freshness through a real turn: an edit landing between two turns changes
/// what the agent quotes next turn, with no agent rebuild. This is what the
/// per-call store read buys over a session-cached snapshot.
#[tokio::test]
async fn an_edit_between_turns_changes_what_the_next_turn_reads() {
    let (base_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/engineering-standards.md" }),
        },
        Turn::Say("first"),
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/engineering-standards.md" }),
        },
        Turn::Say("second"),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, store) = harness(base_url, "\"*\"", dir.path()).await;

    pool.run(
        &record.id,
        "ceo",
        "What is our standard?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("first turn");

    // The operator edits the note in the console — the same store handle.
    store
        .write(
            &record.id,
            "n-eng",
            "# Engineering\nDeploy on green only.",
            WorkspaceOrigin::Operator,
        )
        .await
        .unwrap();

    pool.run(
        &record.id,
        "ceo",
        "And now?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("second turn");

    let results = tool_results(&script);
    let before = results
        .iter()
        .any(|r| r.contains("Review every PR before merge."));
    let after = results.iter().any(|r| r.contains("Deploy on green only."));
    assert!(
        before,
        "the first turn never saw the original body: {results:?}"
    );
    assert!(
        after,
        "the second turn did not pick up the operator's edit — the tools are caching: {results:?}"
    );
}

/// Issue #417, through the one path that can actually prove it: the harness's
/// own tool-result budget, applied by the real middleware.
///
/// The unit tests in [`workspace_tools`](crate::harness::workspace_tools) can
/// only assert that a read *renders* under some number. They cannot see the
/// second bound — `ToolOutputMiddleware`, fed from
/// [`TOOL_RESULT_BUDGET_BYTES`](crate::harness::build::TOOL_RESULT_BUDGET_BYTES)
/// via `AgentBuilder::context_config` — which cuts every tool result on its way
/// into the model's context. That bound is what made the old 64 KiB read cap a
/// data-loss bug: the module reported nothing dropped, and the model got the
/// first ~16 KiB and an anonymous byte marker.
///
/// So this reads a 20 KiB note through the whole pipeline and asserts on the
/// bytes the *model* received. Two properties, and the second is the one no
/// unit test can reach:
///
/// 1. The read never invites a rewrite of a note it only partly returned.
/// 2. The result arrives whole — closing fence last, and no
///    `tool_result_budget` marker, meaning the outer cut never fired at all.
///
/// (2) failing is the exact shape of the original bug: an unterminated fence
/// means the untrusted-content region was left open, and it means the module's
/// idea of what it returned and the model's idea of what it received have come
/// apart again.
#[tokio::test]
async fn an_oversized_note_reaches_the_model_whole_and_read_only() {
    let (base_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/Big standard.md" }),
        },
        Turn::Say("I read what I could of it."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, store) = harness(base_url, "\"workspace\"", dir.path()).await;

    // Larger than the read cap, smaller than the old 64 KiB one — the window
    // in which a note used to be silently shortened and then overwritten.
    let body = "The operator wrote this and expects to keep it. ".repeat(440);
    assert!(
        body.len() > 16 * 1024 && body.len() < 64 * 1024,
        "{}",
        body.len()
    );
    store
        .create(
            &record.id,
            &note("n-big", "Big standard.md", "f-std"),
            Some(&body),
        )
        .await
        .unwrap();

    pool.run(
        &record.id,
        "ceo",
        "What does the big standard say?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    let results = tool_results(&script);
    let read = results
        .iter()
        .find(|r| r.contains("BEGIN WORKSPACE NOTE"))
        .unwrap_or_else(|| panic!("the read result never reached the model: {results:?}"));

    // (1) The model is told it may not write, and is never handed the sentence
    // that drove the overwrite.
    assert!(read.contains("CANNOT be overwritten"), "{read}");
    assert!(
        !read.contains("complete new body"),
        "the model was invited to rewrite a note it only partly received: {read}"
    );

    // (2) The result the model got is the result the module rendered.
    assert!(
        !read.contains("truncated by tool_result_budget"),
        "the harness cut the read result — the two bounds still disagree: {read}"
    );
    let at = read
        .find("--- BEGIN WORKSPACE NOTE ")
        .expect("the read is fenced");
    let nonce = read[at + "--- BEGIN WORKSPACE NOTE ".len()..]
        .split_whitespace()
        .next()
        .expect("the fence carries a nonce");
    assert!(
        read.trim_end()
            .ends_with(&format!("--- END WORKSPACE NOTE {nonce} ---")),
        "the model never received the closing fence, so the untrusted-content region it was \
         warned about was left open: {read}"
    );

    // Not vacuous: the body really did travel, and really was shortened.
    assert!(read.contains("The operator wrote this and expects to keep it."));
    assert!(
        read.contains(&format!("of {} bytes", body.len())),
        "the header should say how much of the note exists: {read}"
    );
}

// ---------------------------------------------------------------------------
// The approval boundary, driven by a model (issues #443, #444)
// ---------------------------------------------------------------------------

/// Re-`ensure` the pool against the same deps under a different policy mode.
///
/// The fixture above is `full` on purpose — it exists to prove the workspace
/// tools work, and parking every call would get in the way. The gate is only
/// observable under `supervised`, which is also the **default** mode a company
/// gets, so it is the mode these last tests care about.
async fn supervised(deps: &HarnessDeps, grants: &str) -> (HarnessPool, CompanyRecord) {
    let mut record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: manifest_in_mode(grants, "supervised"),
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
    record.manifest.tools.allow = manifest(grants).tools.allow;
    let pool = HarnessPool::new();
    pool.ensure(&record, deps).await.expect("pool ensures");
    (pool, record)
}

/// End-to-end, through a model: workspace reads and writes both run without
/// policy-generated HITL.
///
/// The last clause is issue #444's headline, and nothing shorter than this can
/// show it. The two halves of the gate live in different modules and disagreed
/// about this one tool: `is_external_effect` refused to exempt `workspace_write`
/// because it overwrites guidance the operator wrote, while the standing-grant
/// rule read its `Other` group — a group it lands in only because the name
/// carries no consequence word — and offered it for a week. This drives one real
/// turn and asks both halves about the same call.
#[tokio::test]
async fn a_supervised_turn_reads_and_writes_the_workspace_without_policy_hitl() {
    let dir = tempfile::tempdir().unwrap();
    let (base, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "standards/engineering-standards.md" }),
        },
        Turn::WriteWithObservedRev {
            path: "standards/engineering-standards.md",
            content: "# Engineering\nRewritten.",
            delta: 0,
        },
        Turn::Say("done"),
    ])
    .await;
    let (_pool, deps, _record, store) = harness(base, "\"workspace\"", dir.path()).await;
    let (pool, record) = supervised(&deps, "\"workspace\"").await;

    pool.run(
        &record.id,
        "ceo",
        "tidy the standards",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("the turn runs");
    // Issue #439: no boundary index — this turn ran outside any claim, so its
    // requests are in the `Unscoped` bucket and `drain` reads exactly them.
    let parked = deps
        .approval_requests
        .drain(crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN);

    assert!(parked.requests.is_empty(), "{parked:?}");
    assert!(
        tool_results(&script).len() >= 2,
        "both the read and write must have run and fed a result back"
    );
    let (_, content) = store
        .read(&record.id, "n-eng")
        .await
        .expect("workspace read succeeds")
        .expect("standards note remains");
    assert_eq!(content, "# Engineering\nRewritten.");
}

/// The other side of the same boundary, so the feature is not proved dead:
/// The same policy-HITL-off boundary applies to the agent's own sandbox.
#[tokio::test]
async fn a_write_to_the_agents_own_workspace_runs_without_policy_hitl() {
    let dir = tempfile::tempdir().unwrap();
    let (base, script) = spawn_script(vec![
        Turn::Call {
            tool: "file_write",
            args: json!({ "path": "notes.md", "content": "draft" }),
        },
        Turn::Say("done"),
    ])
    .await;
    let (_pool, deps, _record, _store) =
        harness(base, "\"files\", \"workspace\"", dir.path()).await;
    let (pool, record) = supervised(&deps, "\"files\", \"workspace\"").await;

    pool.run(
        &record.id,
        "ceo",
        "jot a note",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("the turn runs");
    // Issue #439: no boundary index — this turn ran outside any claim, so its
    // requests are in the `Unscoped` bucket and `drain` reads exactly them.
    let parked = deps
        .approval_requests
        .drain(crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN);

    assert!(parked.requests.is_empty(), "{parked:?}");
    assert!(
        tool_results(&script)
            .iter()
            .all(|result| !result.contains("error")),
        "the file write must succeed: {:?}",
        tool_results(&script)
    );
    let note =
        crate::harness::build::agent_workspace(dir.path(), &record.id, "ceo").join("notes.md");
    assert_eq!(std::fs::read_to_string(note).unwrap(), "draft");
}

/// Issue #443, through the turn loop: the reads that used to park.
///
/// `file_read` and `grep` are pure reads of the agent's own workspace, and both
/// parked under the DEFAULT mode — not by anyone's decision, but because the
/// read-only rule matched a name *prefix* and neither begins with a read-only
/// word. Nobody had reported them. They were found by asking the same question
/// of every registered tool, which is the mechanism this lane adds.
#[tokio::test]
async fn a_supervised_turn_reads_its_own_workspace_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("seed.md"), "hello").ok();
    let (base, script) = spawn_script(vec![
        Turn::Call {
            tool: "grep",
            args: json!({ "pattern": "hello", "path": "." }),
        },
        Turn::Call {
            tool: "file_read",
            args: json!({ "path": "seed.md" }),
        },
        Turn::Say("done"),
    ])
    .await;
    let (_pool, deps, _record, _store) = harness(base, "\"files\"", dir.path()).await;
    let (pool, record) = supervised(&deps, "\"files\"").await;

    pool.run(
        &record.id,
        "ceo",
        "what do we have?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("the turn runs");
    // Issue #439: no boundary index — this turn ran outside any claim, so its
    // requests are in the `Unscoped` bucket and `drain` reads exactly them.
    let parked = deps
        .approval_requests
        .drain(crate::harness::policy::MAX_APPROVAL_REQUESTS_PER_TURN);

    assert!(
        parked.requests.is_empty(),
        "reading the agent's own workspace must not interrupt an operator: {:?}",
        parked
            .requests
            .iter()
            .map(|r| r.tool.clone())
            .collect::<Vec<_>>()
    );
    // Not vacuous: both reads were genuinely offered to the model and both
    // came back with a result, so the calls reached the gate and returned.
    let offered = advertised_tools(&script);
    for tool in ["grep", "file_read"] {
        assert!(
            offered.contains(&tool.to_string()),
            "`{tool}` was never on the belt, so this proves nothing: {offered:?}"
        );
    }
    // `tool_results` reads every request body the stub saw, and each body
    // repeats the conversation so far — so this counts cumulatively rather
    // than once per call. Two distinct calls is the floor.
    assert!(
        tool_results(&script).len() >= 2,
        "both reads must have run and fed a result back"
    );
}
