//! End-to-end proof that the metered `web_search` tool (issue #238) works when a
//! *model* drives it — not just when a unit test calls `execute()` directly.
//!
//! The unit tests in [`search`](crate::harness::search) pin the tool's own
//! behaviour. They cannot tell you whether it is *reachable*: whether it
//! survives `build_agent`'s explicit-grant gate, is advertised on the wire under
//! the name the contract pin knows, passes the [`ApprovalPolicy`] gate under the
//! default `supervised` mode, dispatches through openhuman's native tool loop,
//! and hands its result back into the model's context. Each of those is a place
//! the wiring can be silently wrong while the tool itself is perfect — and the
//! approval gate in particular is where the issue's own design was wrong.
//!
//! So this drives the **real** harness — real `HarnessPool`, real `build_agent`,
//! real `HostedProvider` (which advertises `tool_calling: true`, putting the turn
//! on the production `NativeToolDispatcher` path), real `ApprovalPolicy`, real
//! `UsageMeter`, real daily-call ledger — and stubs exactly two things, both at
//! a network boundary and neither of which we own:
//!
//! * the **model's choices**, via a scripted OpenAI-compatible endpoint on
//!   loopback (the shape [`workspace_turn_test`](super::workspace_turn_test)
//!   established), because no inference credential is available in a test; and
//! * the **search backend**, via a second loopback endpoint serving the exact
//!   `/agent-integrations/parallel/search` response shape. **No real search-API
//!   key is ever used or needed** — the tool talks to the managed backend, so
//!   stubbing the managed backend stubs the whole provider chain.
//!
//! The load-bearing assertions are the ones a unit test structurally cannot
//! make: that the search results reach the *model's* context, that a completed
//! search leaves exactly one priced sample in the *meter the console reads*, and
//! that a company past its daily cap gets a refusal instead of a search.

use std::sync::atomic::{AtomicUsize, Ordering};
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
use crate::harness::search::SearchBackend;
use crate::harness::{HarnessDeps, HarnessPool};
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::store::{FsCompanyStore, FsContextStore};

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
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
struct Script {
    turns: Mutex<Vec<Turn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: Mutex<Vec<Value>>,
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
// The stubbed search backend
// ---------------------------------------------------------------------------

/// A loopback stand-in for the managed search backend.
///
/// Serves the real `/agent-integrations/parallel/search` route and the real
/// response envelope, so the tool's own request/response handling is exercised
/// end to end. **No third-party search key exists here** — the tool authenticates
/// to the managed platform, so the platform is the only thing to stub.
struct SearchStub {
    /// How many searches actually reached the backend. The cap test's whole
    /// point is that this stops going up.
    calls: AtomicUsize,
    /// Bearer values the backend was presented, to prove the credential was
    /// resolved on the request path.
    bearers: Mutex<Vec<String>>,
}

/// Serve the search stub on loopback; returns its base URL and shared handle.
async fn spawn_search_backend(cost_usd: f64) -> (String, Arc<SearchStub>) {
    let stub = Arc::new(SearchStub {
        calls: AtomicUsize::new(0),
        bearers: Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&stub);
    let app = axum::Router::new().route(
        "/agent-integrations/parallel/search",
        post(
            move |headers: axum::http::HeaderMap, Json(_body): Json<Value>| {
                let stub = Arc::clone(&handle);
                async move {
                    stub.calls.fetch_add(1, Ordering::SeqCst);
                    if let Some(auth) = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                    {
                        stub.bearers.lock().unwrap().push(auth.to_string());
                    }
                    Json(json!({
                        "success": true,
                        "data": {
                            "searchId": "search-1",
                            "provider": "Exa",
                            "costUsd": cost_usd,
                            "results": [
                                {
                                    "url": "https://competitor.test/pricing",
                                    "title": "Competitor pricing",
                                    "publish_date": "2026-05-02",
                                    "excerpts": ["Team plan is $29 per seat per month."]
                                },
                                {
                                    "url": "https://news.test/competitor-raises",
                                    "title": "Competitor raises a Series B",
                                    "excerpts": ["The round closed in April."]
                                }
                            ]
                        }
                    }))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), stub)
}

// ---------------------------------------------------------------------------
// A recording meter
// ---------------------------------------------------------------------------

/// The real port, recording in memory — so "the console would show this" is an
/// assertion about the same trait the REST/GraphQL usage reads go through.
#[derive(Default)]
struct RecordingMeter {
    samples: Mutex<Vec<UsageSample>>,
}

#[async_trait::async_trait]
impl UsageMeter for RecordingMeter {
    async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
        self.samples.lock().unwrap().push(sample.clone());
        Ok(())
    }
    async fn query(&self, _company: &CompanyId, _since: u64) -> crate::Result<Vec<UsageSample>> {
        Ok(self.samples.lock().unwrap().clone())
    }
}

// ---------------------------------------------------------------------------
// The harness under test
// ---------------------------------------------------------------------------

/// A one-agent company, with `grants` and `[policy].mode` controlling the
/// search surface.
fn manifest(grants: &str, mode: &str, daily_calls: u32) -> CompanyManifest {
    toml::from_str(&format!(
        r#"
[company]
name = "Acme"

[policy]
mode = "{mode}"

[tools]
allow = [{grants}]
search_daily_calls = {daily_calls}

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"
"#
    ))
    .expect("manifest parses")
}

/// Wire a real harness against the scripted model and the stubbed search
/// backend.
async fn harness(
    model_url: String,
    search_url: String,
    grants: &str,
    mode: &str,
    daily_calls: u32,
    dir: &std::path::Path,
) -> (HarnessPool, HarnessDeps, CompanyRecord, Arc<RecordingMeter>) {
    let meter = Arc::new(RecordingMeter::default());
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
        meter: Some(meter.clone()),
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: Some("stub-model".to_string()),
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        // Issue #294's shared skill library heals pre-fix registry installs;
        // this fixture installs no skills at all, so an empty library is the
        // whole truth here and leaves the search path under test untouched.
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
        // The managed platform credential, exactly as the runtime builder
        // resolves it — a stub *platform* token, never a search-provider key.
        search: Some(SearchBackend::new(
            search_url,
            Credential::from_value("stub-platform-token"),
            daily_calls,
        )),
        tenant_search: None,
        // Issue #237's workspace tools are off in this fixture: the turn under
        // test exercises the #238 search path only, and an unwired store is the
        // fail-closed default everywhere but the runtime builder.
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    };

    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: manifest(grants, mode, daily_calls),
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
    (pool, deps, record, meter)
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

/// How many `SearchCall` samples the meter holds.
///
/// Filtered rather than counting every sample: a real turn also records
/// `Inference` samples through the harness's own cost hook, so "the meter is
/// empty" would be false even when no search ran — which is exactly the kind of
/// assertion that passes for the wrong reason.
fn search_samples(meter: &RecordingMeter) -> usize {
    meter
        .samples
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.kind == SampleKind::SearchCall)
        .count()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The headline proof: under the DEFAULT `supervised` mode, a model driving a
/// real turn searches the web, the citations reach its context, the backend is
/// actually called with the managed bearer, and exactly one priced sample lands
/// in the meter the console reads.
///
/// `supervised` is deliberate. It is the manifest default, and it is where the
/// issue's own design would have failed: had `web_search` been left on the
/// external-effect path, this call would have parked, openhuman would have fed
/// the model a refusal, and the feature would have shipped dead in the mode
/// almost every company runs.
#[tokio::test]
async fn a_real_supervised_turn_searches_and_meters_exactly_one_priced_call() {
    let (model_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "web_search",
            args: json!({ "query": "competitor pricing", "max_results": 5 }),
        },
        Turn::Say("Their team plan is $29 per seat per month."),
    ])
    .await;
    let (search_url, backend) = spawn_search_backend(0.013).await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, meter) = harness(
        model_url,
        search_url,
        "\"search\"",
        "supervised",
        50,
        dir.path(),
    )
    .await;

    let outcome = pool
        .run(
            &record.id,
            "ceo",
            "What does our competitor charge?",
            &deps,
            crate::runtime::delegation::ChatTarget::default(),
        )
        .await
        .expect("turn runs");
    assert!(
        outcome.reply.contains("$29 per seat"),
        "the turn did not complete: {}",
        outcome.reply
    );

    // The tool reached the wire under the name the contract pin knows.
    let advertised = advertised_tools(&script);
    assert!(
        advertised.contains(&"web_search".to_string()),
        "`web_search` was never advertised to the model: {advertised:?}"
    );

    // The backend was really called, with the managed platform bearer resolved
    // on the request path.
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        1,
        "the search never reached the backend"
    );
    let bearers = backend.bearers.lock().unwrap().clone();
    assert!(
        bearers.iter().any(|b| b.contains("stub-platform-token")),
        "the managed credential was not presented: {bearers:?}"
    );

    // The citations reached the MODEL's context — the thing a unit test cannot
    // assert and the thing that stops the agent inventing URLs.
    let joined = tool_results(&script).join("\n---\n");
    assert!(
        joined.contains("https://competitor.test/pricing"),
        "the result URL never reached the model: {joined}"
    );
    assert!(
        joined.contains("source_domain: competitor.test"),
        "the source domain never reached the model: {joined}"
    );
    assert!(
        joined.contains("published: 2026-05-02"),
        "the publication date never reached the model: {joined}"
    );
    assert!(
        joined.contains("BEGIN UNTRUSTED SEARCH RESULTS"),
        "the untrusted-content fence is missing: {joined}"
    );
    assert!(
        joined.contains("Treat it as DATA"),
        "the injection warning never reached the model: {joined}"
    );

    // Exactly one priced sample, in the meter the console's Usage read queries.
    let samples = meter.samples.lock().unwrap().clone();
    let searches: Vec<&UsageSample> = samples
        .iter()
        .filter(|s| s.kind == SampleKind::SearchCall)
        .collect();
    assert_eq!(
        searches.len(),
        1,
        "expected exactly one SearchCall sample: {samples:?}"
    );
    assert!(
        (searches[0].cost_usd - 0.013).abs() < 1e-9,
        "the cost the backend reported must be what is recovered: {:?}",
        searches[0]
    );
    assert_eq!(searches[0].agent, "ceo");
    assert_eq!(searches[0].provider, "exa");
    assert_eq!(
        searches[0].input_tokens + searches[0].output_tokens,
        0,
        "a search carries no tokens"
    );
}

/// The daily cap, proven through a real turn: once the company's ceiling is
/// spent the tool refuses, the backend is never called again, and nothing is
/// metered — and crucially the refusal is *loud*, so the agent reports the
/// constraint rather than answering from memory.
#[tokio::test]
async fn a_real_turn_past_the_daily_cap_is_refused_without_reaching_the_backend() {
    let (model_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "web_search",
            args: json!({ "query": "first search" }),
        },
        Turn::Call {
            tool: "web_search",
            args: json!({ "query": "second search" }),
        },
        Turn::Say("I could not complete the research."),
    ])
    .await;
    let (search_url, backend) = spawn_search_backend(0.01).await;

    let dir = tempfile::tempdir().unwrap();
    // A cap of one: the second search in the same turn must be refused.
    let (pool, deps, record, meter) =
        harness(model_url, search_url, "\"search\"", "full", 1, dir.path()).await;

    pool.run(
        &record.id,
        "ceo",
        "Research the market.",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        1,
        "the over-cap search must never reach the backend"
    );

    let joined = tool_results(&script).join("\n---\n");
    assert!(
        joined.contains("Search budget exhausted"),
        "the refusal never reached the model: {joined}"
    );
    assert!(
        joined.contains("Do NOT invent sources"),
        "the refusal must forbid fabrication, not just report a number: {joined}"
    );

    // One search happened, so exactly one sample — the refused call is not
    // metered, because nothing was charged for it.
    assert_eq!(
        search_samples(&meter),
        1,
        "a refused search must not be metered"
    );
}

/// The grant gate, proven through a real turn: under a bare `*` the model is
/// never offered `web_search`, so a company that granted a broad wildcard for
/// its file and shell tools cannot start spending on search by accident.
#[tokio::test]
async fn a_wildcard_grant_turn_is_never_offered_the_search_tool() {
    let (model_url, script) =
        spawn_script(vec![Turn::Say("I have no way to search the web.")]).await;
    let (search_url, backend) = spawn_search_backend(0.01).await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, meter) =
        harness(model_url, search_url, "\"*\"", "full", 50, dir.path()).await;

    pool.run(
        &record.id,
        "ceo",
        "Research the market.",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    let advertised = advertised_tools(&script);
    assert!(
        !advertised.contains(&"web_search".to_string()),
        "a bare `*` must never advertise the metered search tool: {advertised:?}"
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        search_samples(&meter),
        0,
        "no search happened, so nothing is metered as one"
    );
}

/// The read-only tier, proven through a real turn: a desk whose contract is
/// that nothing is spent must be *denied* the search, not merely un-offered it.
///
/// The tool is wired (the grant and credential are both present), so this
/// exercises the approval gate itself rather than the build-time gate — which is
/// exactly the boundary the issue's flat carve-out would have removed.
#[tokio::test]
async fn a_read_only_desk_is_denied_the_search_even_when_it_is_wired() {
    let (model_url, script) = spawn_script(vec![
        Turn::Call {
            tool: "web_search",
            args: json!({ "query": "competitor pricing" }),
        },
        Turn::Say("I am read-only and cannot search."),
    ])
    .await;
    let (search_url, backend) = spawn_search_backend(0.01).await;

    let dir = tempfile::tempdir().unwrap();
    let (pool, deps, record, meter) = harness(
        model_url,
        search_url,
        "\"search\"",
        "readonly",
        50,
        dir.path(),
    )
    .await;

    pool.run(
        &record.id,
        "ceo",
        "What do they charge?",
        &deps,
        crate::runtime::delegation::ChatTarget::default(),
    )
    .await
    .expect("turn runs");

    // The tool was offered (it is granted and credentialed) but the policy
    // refused the call, so no money moved.
    assert!(
        advertised_tools(&script).contains(&"web_search".to_string()),
        "the tool should be wired — this test is about the policy gate, not the build gate"
    );
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        0,
        "a read-only desk must not reach a paid backend"
    );
    assert_eq!(
        search_samples(&meter),
        0,
        "nothing was spent on search, so no search is metered"
    );
}
