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
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceStore};
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
    }
}

fn note(id: &str, name: &str, parent: &str) -> WorkspaceNode {
    WorkspaceNode {
        id: id.to_string(),
        name: name.to_string(),
        kind: NodeKind::File,
        parent_id: Some(parent.to_string()),
        updated_at_millis: crate::ports::now_millis(),
    }
}

/// A one-agent company, with `grants` controlling the workspace surface.
fn manifest(grants: &str) -> CompanyManifest {
    toml::from_str(&format!(
        r#"
[company]
name = "Acme"

[policy]
# `full` so an ordinary turn is not parked; the write tool's own
# compare-and-swap token is what guards the write in this mode.
mode = "full"

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
        .create(&id, &folder("f-std", "Standards"), None)
        .await
        .unwrap();
    store
        .create(
            &id,
            &note("n-eng", "Engineering standards.md", "f-std"),
            Some("# Engineering\nReview every PR before merge."),
        )
        .await
        .unwrap();

    let deps = HarnessDeps {
        provider: Arc::new(HostedProvider::new(HostedProviderConfig {
            base_url,
            credential: Credential::from_value("stub-key"),
            extra_headers: Vec::new(),
        })),
        provider_slug: "managed".to_string(),
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: None,
        workspace_root: dir.to_path_buf(),
        model_override: Some("stub-model".to_string()),
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        skills_registry: std::sync::Arc::from([]),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: DelegationQueue::default(),
        workflow_runner: WorkflowRunnerHandle::default(),
        mcp_failures: McpFailureQueue::default(),
        approval_requests: ApprovalRequestQueue::default(),
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
        workflow_source_dir: None,
        plan: None,
        media: None,
        composio: None,
        steer: crate::company::steer::InflightRegistry::default(),
        delivery: None,
        workspace: Some(store.clone()),
        // Issue #238's metered search is off in this fixture: the turn under
        // test exercises the #237 workspace path only, and no managed search
        // backend is the fail-closed default outside the runtime builder.
        search: None,
    };

    let record = CompanyRecord {
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
        template_provenance: None,
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
            args: json!({ "path": "Standards/Engineering standards.md" }),
        },
        Turn::WriteWithObservedRev {
            path: "Standards/Engineering standards.md",
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
            None,
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
        joined.contains("Standards/Engineering standards.md"),
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
    assert_eq!(node.name, "Engineering standards.md");
}

/// The compare-and-swap guard, proven through a real turn: a model writing with
/// a revision that is not current is refused, the note is untouched, and the
/// refusal is fed back so the agent can recover rather than retry blindly.
#[tokio::test]
async fn a_real_turn_is_refused_when_it_writes_with_a_stale_revision() {
    let (base_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "Standards/Engineering standards.md" }),
        },
        // Pretend the operator edited the note between read and write.
        Turn::WriteWithObservedRev {
            path: "Standards/Engineering standards.md",
            content: "clobbered",
            delta: -1,
        },
        Turn::Say("I could not apply that edit."),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, store) = harness(base_url, "\"workspace\"", dir.path()).await;

    let (before, _) = store.read(&record.id, "n-eng").await.unwrap().unwrap();

    pool.run(&record.id, "ceo", "Rewrite the standards.", &deps, None)
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
            args: json!({ "path": "Standards/Engineering standards.md" }),
        },
        // Nothing stops a model naming a tool it was never offered. Under a
        // bare `*` the write must be *refused*, not merely left unadvertised —
        // advertisement is a hint, the grant check is the control.
        Turn::WriteWithObservedRev {
            path: "Standards/Engineering standards.md",
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
            None,
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
            args: json!({ "path": "Standards/Engineering standards.md" }),
        },
        Turn::Say("first"),
        Turn::Call {
            tool: "workspace_read",
            args: json!({ "path": "Standards/Engineering standards.md" }),
        },
        Turn::Say("second"),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, store) = harness(base_url, "\"*\"", dir.path()).await;

    pool.run(&record.id, "ceo", "What is our standard?", &deps, None)
        .await
        .expect("first turn");

    // The operator edits the note in the console — the same store handle.
    store
        .write(&record.id, "n-eng", "# Engineering\nDeploy on green only.")
        .await
        .unwrap();

    pool.run(&record.id, "ceo", "And now?", &deps, None)
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
