//! Issue #410 — end-to-end proof that an agent with a connected provider can
//! *discover* the action it needs and *call* it, unaided, on more than one
//! toolkit.
//!
//! The acceptance in #410 is behavioural, and the unit tests in
//! [`composio_catalog`](crate::harness::composio_catalog) structurally cannot
//! reach it: they pin how a listing renders, not whether the rendering ever
//! reaches a model's context intact. The failure being fixed happened *on the
//! way out of the tool* — a successful result was passed through the MCP
//! **message** sanitiser, whose 300-byte cap left the model the first action,
//! half of its schema, and a bare `…`. Run against the pre-fix code these tests
//! fail with exactly that fragment.
//!
//! So this drives the **real** harness — real `HarnessPool`, real `build_agent`,
//! real `HostedProvider` on the native tool-calling path, real `ApprovalPolicy`,
//! real `ComposioClient` — and stubs exactly two things, both at a network
//! boundary and neither of which we own:
//!
//! * the **model's choices**, via a scripted OpenAI-compatible endpoint on
//!   loopback (the shape [`search_turn_test`](super::search_turn_test)
//!   established); and
//! * the **Composio backend**, via a second loopback endpoint serving the real
//!   `/agent-integrations/composio/tools` and `/execute` routes with a
//!   *synthesised* catalogue of 120 GitHub and 140 Notion actions. **No Composio
//!   credential exists here** — the tools talk to the managed backend, so
//!   stubbing the managed backend stubs the whole provider chain, and
//!   synthesising the catalogue is the only honest way to prove a generic fix
//!   without two live hundred-action connections.
//!
//! The load-bearing assertions are the ones a unit test cannot make:
//!
//! 1. every Composio tool result the model sees stays under the harness's
//!    16 KiB shared budget, so **our** self-describing cut is the cut, not an
//!    anonymous downstream byte slice;
//! 2. a genuinely oversized listing says it was cut, by how much, and which
//!    argument makes it smaller;
//! 3. the agent gets from "I need to list issues" to a real
//!    `composio_execute(GITHUB_LIST_ISSUES)` with no human supplying the slug —
//!    and does the same on a second, larger, non-GitHub toolkit.
//!
//! Gated on the `composio` feature, which CI builds (`--all-features`) but never
//! *runs*; the narrowing and truncation logic these tests exercise therefore
//! also carries its own tests in [`composio_catalog`], which the
//! `--features openhuman,tinymemory` test lane does run.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::Query;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::company::CompanyManifest;
use crate::company::credentials::Credential;
use crate::harness::composio::TenantComposio;
use crate::harness::mcp_probe::McpFailureQueue;
use crate::harness::orchestrator::{DelegationQueue, WorkflowRunnerHandle};
use crate::harness::policy::ApprovalRequestQueue;
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::store::{FsCompanyStore, FsContextStore};

/// The harness's shared per-tool-result byte budget
/// (`openhuman::context::DEFAULT_TOOL_RESULT_BUDGET_BYTES`). A Composio result
/// at or above this is cut by machinery that neither counts what it dropped nor
/// names an argument to narrow with — which is the whole bug.
const HARNESS_TOOL_RESULT_BUDGET_BYTES: usize = 16 * 1024;

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

fn tool_call_message(tool: &str, args: &Value) -> Value {
    json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": format!("call-{tool}-{}", args.to_string().len()),
            "type": "function",
            "function": { "name": tool, "arguments": args.to_string() }
        }]
    })
}

async fn spawn_script(turns: Vec<Turn>) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        turns: Mutex::new(turns),
        seen: Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&script);
    let app = Router::new().route(
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

// ---------------------------------------------------------------------------
// The stubbed Composio backend
// ---------------------------------------------------------------------------

/// A synthesised action, shaped like a real Composio function schema: a long
/// upstream description and a parameter schema with per-property prose. Sizes
/// are deliberately realistic — this is what makes the catalogue genuinely
/// oversized rather than artificially so.
fn action(toolkit: &str, slug: &str, summary: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": slug,
            "description": format!(
                "{summary} This action operates on the connected {toolkit} account. \
                 {}",
                "Upstream publishes several sentences of prose for every action, which is \
                 exactly why a whole toolkit's catalogue does not fit in one tool result. "
                    .repeat(2)
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "owner": { "type": "string", "description": "o".repeat(180) },
                    "target": { "type": "string", "description": "t".repeat(180) },
                    "state": { "type": "string", "enum": ["open", "closed", "all"] }
                },
                "required": ["owner"]
            }
        }
    })
}

/// 120 GitHub actions, one of which is the one an agent asked to "list the open
/// issues" actually needs. The needle sorts nowhere near first — the pre-fix
/// cut kept whatever sorted first, so a needle at index 97 is the honest case.
fn github_catalogue() -> Vec<Value> {
    let mut out: Vec<Value> = (0..120)
        .map(|i| {
            action(
                "github",
                &format!("GITHUB_ACTION_{i:03}"),
                &format!("Performs repository operation number {i}."),
            )
        })
        .collect();
    out[97] = action(
        "github",
        "GITHUB_LIST_REPOSITORY_ISSUES",
        "List the issues on a repository, optionally filtered by state.",
    );
    out
}

/// 140 Notion actions — the "at least one large toolkit that is not GitHub"
/// half of the acceptance criteria.
fn notion_catalogue() -> Vec<Value> {
    let mut out: Vec<Value> = (0..140)
        .map(|i| {
            action(
                "notion",
                &format!("NOTION_ACTION_{i:03}"),
                &format!("Performs workspace operation number {i}."),
            )
        })
        .collect();
    out[131] = action(
        "notion",
        "NOTION_SEARCH_NOTION_PAGE",
        "Search the pages in a workspace by query text.",
    );
    out
}

/// What the stub observed, so the tests can assert on the wire rather than on
/// the model's narration.
#[derive(Default)]
struct ComposioStub {
    /// Every `toolkits=` query string the tool sent to `/tools`.
    tool_queries: Mutex<Vec<Option<String>>>,
    /// Every action slug `/execute` was asked to run.
    executed: Mutex<Vec<String>>,
    /// How many times `/tools` was called at all.
    list_calls: AtomicUsize,
}

async fn spawn_composio_backend() -> (String, Arc<ComposioStub>) {
    let stub = Arc::new(ComposioStub::default());

    let tools_stub = Arc::clone(&stub);
    let execute_stub = Arc::clone(&stub);
    let app = Router::new()
        .route(
            "/agent-integrations/composio/tools",
            get(move |Query(params): Query<Vec<(String, String)>>| {
                let stub = Arc::clone(&tools_stub);
                async move {
                    stub.list_calls.fetch_add(1, Ordering::SeqCst);
                    let requested = params
                        .iter()
                        .find(|(k, _)| k == "toolkits")
                        .map(|(_, v)| v.clone());
                    stub.tool_queries.lock().unwrap().push(requested.clone());
                    // Mirror the real backend: `toolkits=` narrows server-side,
                    // its absence returns every enabled toolkit's actions.
                    let mut tools: Vec<Value> = Vec::new();
                    let wants = |name: &str| {
                        requested
                            .as_deref()
                            .map(|r| r.split(',').any(|t| t.eq_ignore_ascii_case(name)))
                            .unwrap_or(true)
                    };
                    if wants("github") {
                        tools.extend(github_catalogue());
                    }
                    if wants("notion") {
                        tools.extend(notion_catalogue());
                    }
                    Json(json!({ "success": true, "data": { "tools": tools } }))
                }
            }),
        )
        .route(
            "/agent-integrations/composio/execute",
            post(move |Json(body): Json<Value>| {
                let stub = Arc::clone(&execute_stub);
                async move {
                    let tool = body
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    stub.executed.lock().unwrap().push(tool.clone());
                    Json(json!({
                        "success": true,
                        "data": {
                            "data": { "ran": tool, "items": ["#1 flaky test", "#2 docs typo"] },
                            "successful": true,
                            "costUsd": 0.0
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
    (format!("http://{addr}"), stub)
}

// ---------------------------------------------------------------------------
// The harness under test
// ---------------------------------------------------------------------------

/// A one-agent company that explicitly grants `composio` (the catch-all `*`
/// deliberately does not) and runs in `full` mode. The scripted actions use
/// the provider's curated read slugs, so the policy can classify and run them
/// rather than conservatively parking an unknown action mid-test.
fn manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[tools]
allow = ["composio"]

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"
"#,
    )
    .expect("manifest parses")
}

async fn harness(
    model_url: String,
    composio_url: String,
    dir: &std::path::Path,
) -> (HarnessPool, HarnessDeps, CompanyRecord) {
    let deps = HarnessDeps {
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
        // An empty toolkit allowlist is "defer to the backend" (open mode) —
        // the worst case for catalogue size, and the case a newly-connected
        // provider lands in.
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        composio: Some(TenantComposio::new(
            composio_url,
            Credential::from_value("stub-tenant-token"),
            Vec::new(),
        )),
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    };

    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
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
    };

    let pool = HarnessPool::new();
    pool.ensure(&record, &deps).await.expect("pool ensures");
    (pool, deps, record)
}

/// Every tool *result* the harness fed back to the model, in order.
fn tool_results(script: &Script) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for body in script.seen.lock().unwrap().iter() {
        for message in body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if message.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            if let Some(content) = message.get("content").and_then(Value::as_str)
                && !seen.iter().any(|s| s == content)
            {
                seen.push(content.to_string());
            }
        }
    }
    seen
}

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

/// Every distinct `system` message the harness actually sent to the model —
/// the composed system prompt as it went over the wire, so a test can assert
/// what an agent was really told rather than what a brief function returns in
/// isolation.
fn system_prompts(script: &Script) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for body in script.seen.lock().unwrap().iter() {
        for message in body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if message.get("role").and_then(Value::as_str) != Some("system") {
                continue;
            }
            if let Some(content) = message.get("content").and_then(Value::as_str)
                && !seen.iter().any(|s| s == content)
            {
                seen.push(content.to_string());
            }
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The headline acceptance: an agent with live connections to two large
/// toolkits — 120 GitHub actions and 140 Notion actions, 260 in one open-mode
/// catalogue — discovers the right slug on **each** and calls it, with no human
/// supplying a slug and no provider-specific code anywhere in the path.
///
/// The script is the agent's reasoning, written out: list what exists, narrow to
/// the words in the task, read that one action's parameters, call it. Every step
/// uses only information the previous step's *result* gave it.
#[tokio::test]
async fn an_agent_discovers_and_calls_an_action_unaided_on_two_large_toolkits() {
    let (model_url, script) = spawn_script(vec![
        // 1. What can I do at all? (open mode: 260 actions)
        Turn::Call {
            tool: "composio_list_tools",
            args: json!({}),
        },
        // 2. The task said "issues" — narrow to it and read the parameters.
        Turn::Call {
            tool: "composio_list_tools",
            args: json!({ "search": "list issues", "detail": "schemas" }),
        },
        // 3. Call it with the arguments the schema named.
        Turn::Call {
            tool: "composio_execute",
            args: json!({
                "tool": "GITHUB_LIST_REPOSITORY_ISSUES",
                "arguments": { "owner": "acme", "state": "open" }
            }),
        },
        // 4-5. The same two steps on a different, larger, non-GitHub toolkit.
        Turn::Call {
            tool: "composio_list_tools",
            args: json!({ "toolkits": ["notion"], "search": "search pages", "detail": "schemas" }),
        },
        Turn::Call {
            tool: "composio_execute",
            args: json!({
                "tool": "NOTION_SEARCH_NOTION_PAGE",
                "arguments": { "owner": "acme", "target": "roadmap" }
            }),
        },
        Turn::Say("Two open issues, and the roadmap page."),
    ])
    .await;
    let (composio_url, stub) = spawn_composio_backend().await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record) = harness(model_url, composio_url, dir.path()).await;

    let outcome = pool
        .run(
            &record.id,
            "ceo",
            "List our open GitHub issues and find the roadmap page in Notion.",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn runs");
    assert!(
        outcome.reply.contains("Two open issues"),
        "the turn did not complete: {}",
        outcome.reply
    );

    let advertised = advertised_tools(&script);
    for tool in ["composio_list_tools", "composio_execute"] {
        assert!(
            advertised.contains(&tool.to_string()),
            "`{tool}` was never advertised to the model: {advertised:?}"
        );
    }

    let results = tool_results(&script);
    let joined = results.join("\n=== next tool result ===\n");

    // The discovery step told the agent the listing was incomplete AND how to
    // narrow it. Without this it has no reason to change its request.
    assert!(
        results[0].contains("260 available"),
        "the first listing must report the true catalogue size: {}",
        results[0]
    );
    assert!(
        results[0].contains("TRUNCATED") && results[0].contains("`search`"),
        "the oversized listing must describe its own cut: {}",
        results[0]
    );

    // The narrowed step delivered exactly the schema needed to call it.
    assert!(
        joined.contains("GITHUB_LIST_REPOSITORY_ISSUES"),
        "the needle slug never reached the model: {joined}"
    );
    assert!(
        joined.contains("\"state\"") && joined.contains("\"open\""),
        "the parameter schema never reached the model: {joined}"
    );
    assert!(
        joined.contains("NOTION_SEARCH_NOTION_PAGE"),
        "the second toolkit's needle never reached the model: {joined}"
    );

    // Both calls actually reached the provider — the acceptance is "calls it",
    // not "talks about calling it".
    let executed = stub.executed.lock().unwrap().clone();
    assert_eq!(
        executed,
        vec![
            "GITHUB_LIST_REPOSITORY_ISSUES".to_string(),
            "NOTION_SEARCH_NOTION_PAGE".to_string()
        ],
        "the agent did not reach both providers: {executed:?}; tool results: {results:#?}"
    );

    // The generic fix, stated as a wire fact: the second Notion listing carried
    // `toolkits=notion`, so narrowing happened server-side too rather than by
    // fetching 260 actions and throwing 259 away.
    let queries = stub.tool_queries.lock().unwrap().clone();
    assert!(
        queries.iter().any(|q| q.as_deref() == Some("notion")),
        "the toolkit narrowing never reached the backend: {queries:?}"
    );

    // Nothing the model saw was big enough for the harness's own cut to fire —
    // so every cut in this turn was one that counted itself and said how to ask
    // for less.
    for (index, result) in results.iter().enumerate() {
        assert!(
            result.len() < HARNESS_TOOL_RESULT_BUDGET_BYTES,
            "tool result {index} is {} bytes — at or past the harness budget, which cuts \
             anonymously: {result}",
            result.len()
        );
    }
}

/// The retry-loop guard, stated directly: an agent that repeats the identical
/// oversized listing gets the identical result *and* an explicit instruction not
/// to. Before the fix the repeated result was a silent fragment, which is why
/// the agent had no reason to stop and eventually hit the repetition guard.
#[tokio::test]
async fn a_repeated_oversized_listing_still_tells_the_agent_to_narrow_instead() {
    let (model_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "composio_list_tools",
            args: json!({}),
        },
        Turn::Call {
            tool: "composio_list_tools",
            args: json!({ "toolkits": ["github", "notion"] }),
        },
        Turn::Say("I need to narrow the listing."),
    ])
    .await;
    let (composio_url, _stub) = spawn_composio_backend().await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record) = harness(model_url, composio_url, dir.path()).await;
    pool.run(
        &record.id,
        "ceo",
        "What can you do?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    let results = tool_results(&script);
    assert!(
        results.len() >= 2,
        "expected two distinct listings: {results:#?}"
    );
    for result in &results {
        assert!(
            result.contains("Do NOT repeat this call unchanged"),
            "a cut listing must break the loop it would otherwise cause: {result}"
        );
        assert!(
            result.len() < HARNESS_TOOL_RESULT_BUDGET_BYTES,
            "{} bytes",
            result.len()
        );
    }
}

/// A newly-connected provider with a large catalogue works with no
/// provider-specific change: the same words that found the GitHub action find
/// the Notion one, and the toolkit slug is never hardcoded anywhere in the path.
#[tokio::test]
async fn a_narrowed_listing_on_an_unknown_toolkit_needs_no_provider_specific_code() {
    let (model_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "composio_list_tools",
            args: json!({ "toolkits": ["notion"], "search": "operation number 42" }),
        },
        Turn::Say("Found it."),
    ])
    .await;
    let (composio_url, _stub) = spawn_composio_backend().await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record) = harness(model_url, composio_url, dir.path()).await;
    pool.run(
        &record.id,
        "ceo",
        "Find the Notion action.",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    let joined = tool_results(&script).join("\n");
    assert!(
        joined.contains("NOTION_ACTION_042"),
        "the search did not find the action on an arbitrary toolkit: {joined}"
    );
    assert!(
        !joined.contains("TRUNCATED"),
        "a single match is not a cut: {joined}"
    );
}

/// Issue #1759, wired end to end: the capability-grounding + Composio-routing
/// brief is not just a pure function — it reaches the model. This drives the
/// real harness (real `build_agent`, real `HostedProvider`) and reads the system
/// prompt off the wire, proving the agent is actually told to route GitHub /
/// connected SaaS through `composio_execute` and NOT to hand-roll `http_request`
/// against a provider API. A unit test on `composio_brief` cannot make this
/// claim; it pins the text, not whether the text was ever composed into a turn.
#[tokio::test]
async fn the_composio_routing_brief_reaches_the_model_system_prompt() {
    let (model_url, script) = spawn_script(vec![Turn::Say("Understood.")]).await;
    let (composio_url, _stub) = spawn_composio_backend().await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record) = harness(model_url, composio_url, dir.path()).await;
    pool.run(
        &record.id,
        "ceo",
        "What can you do on GitHub?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    let system = system_prompts(&script).join("\n");
    // The routing rule reached the model.
    assert!(
        system.contains("composio_execute"),
        "the system prompt must name the Composio call path: {system}"
    );
    assert!(
        system.contains("http_request") && system.contains("api.github.com"),
        "the system prompt must warn off hand-rolling provider APIs: {system}"
    );
    // The grounding half reached it too.
    assert!(
        system.to_lowercase().contains("no browser"),
        "the system prompt must ground the agent against promising browser actions: {system}"
    );
}
