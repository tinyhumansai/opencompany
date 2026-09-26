use super::*;

/// Shared with [`super::blocked_tests`], which builds the same declarations and
/// then attaches a policy to them.
pub(super) fn decl(name: &str, endpoint: &str) -> McpServerDecl {
    McpServerDecl {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        description: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: 30,
        enabled: true,
        source: crate::company::mcp::McpSource::Runtime,
        auth: AuthMaterial::None,
        tool_policies: Default::default(),
        tool_inventory: Default::default(),
    }
}

pub(super) fn grants(g: &[&str]) -> Vec<String> {
    g.iter().map(|s| s.to_string()).collect()
}

#[test]
fn empty_decls_yield_no_registry() {
    assert!(registry_for_agent(&[], &grants(&["mcp:*"])).is_none());
}

#[test]
fn server_name_strips_markdown_fences() {
    // Models wrap identifiers in markdown when answering in prose style;
    // the fence characters belong to the answer, not the server name
    // (seen live: server="werkplaats`" -> "unknown mcp server `werkplaats``").
    let mk = |v: &str| serde_json::json!({ "server": v });
    let parsed = |v: &str| required_string_arg(&mk(v), "server").unwrap();
    assert_eq!(parsed("werkplaats"), "werkplaats");
    assert_eq!(parsed("werkplaats`"), "werkplaats");
    assert_eq!(parsed("`werkplaats`"), "werkplaats");
    assert_eq!(parsed("*werkplaats*"), "werkplaats");
    assert_eq!(parsed("werkplaats."), "werkplaats");
    assert_eq!(parsed("werk"), "werk");
    assert!(required_string_arg(&mk("```"), "server").is_err());
}

#[test]
fn ungranted_agent_gets_no_registry() {
    let decls = vec![decl("notion", "https://notion.example/mcp")];
    // No mcp grant at all.
    assert!(registry_for_agent(&decls, &grants(&["email.send"])).is_none());
}

#[test]
fn wildcard_grant_admits_all_enabled_servers() {
    let decls = vec![
        decl("notion", "https://notion.example/mcp"),
        decl("linear", "https://linear.example/mcp"),
    ];
    let reg = registry_for_agent(&decls, &grants(&["mcp:*"])).expect("registry");
    let mut names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["linear", "notion"]);
}

#[test]
fn named_grant_scopes_to_that_server() {
    let decls = vec![
        decl("notion", "https://notion.example/mcp"),
        decl("linear", "https://linear.example/mcp"),
    ];
    let reg = registry_for_agent(&decls, &grants(&["mcp:notion"])).expect("registry");
    let names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["notion"]);
}

#[test]
fn disabled_server_is_excluded() {
    let mut d = decl("notion", "https://notion.example/mcp");
    d.enabled = false;
    assert!(registry_for_agent(&[d], &grants(&["mcp:*"])).is_none());
}

#[test]
fn gitbooks_default_server_never_leaks_in() {
    // OpenHuman's Config::default seeds a `gitbooks` server; the registry we
    // build for a tenant agent must NOT contain it.
    let decls = vec![decl("notion", "https://notion.example/mcp")];
    let reg = registry_for_agent(&decls, &grants(&["mcp:*"])).expect("registry");
    assert!(reg.get("gitbooks").is_none(), "gitbooks must not leak in");
}

#[test]
fn auth_material_maps_onto_transport_config() {
    let bearer = auth_config(&AuthMaterial::Bearer("tok".into()));
    assert!(matches!(bearer, McpAuthConfig::BearerToken { .. }));
    // An OAuth credential resolves to the same bearer path, carrying exactly
    // its (already-refreshed) access token and nothing else.
    let oauth = auth_config(&AuthMaterial::OAuth {
        access_token: "at".into(),
        refresh_token: None,
        client_id: "cid".into(),
        client_secret: None,
        token_endpoint: "https://as.example/token".into(),
        expires_at: 0,
    });
    assert!(matches!(oauth, McpAuthConfig::BearerToken { token } if token == "at"));
    let header = auth_config(&AuthMaterial::Header {
        name: "X-Key".into(),
        value: "v".into(),
    });
    assert!(matches!(header, McpAuthConfig::Header { .. }));
    let query = auth_config(&AuthMaterial::QueryParam {
        name: "apiKey".into(),
        value: "qp".into(),
    });
    assert!(matches!(query, McpAuthConfig::QueryParam { .. }));
    assert!(matches!(
        auth_config(&AuthMaterial::None),
        McpAuthConfig::None
    ));
}

#[tokio::test]
async fn list_servers_tool_never_emits_a_credential() {
    let mut d = decl("notion", "https://notion.example/mcp");
    d.auth = AuthMaterial::Bearer("sk-super-secret-token".into());
    let reg = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");
    let tool = OcMcpListServersTool::new(reg);
    let result = tool.execute(json!({})).await.expect("execute");

    // The whole serialized result (JSON + markdown) must not carry the token.
    let json_out = serde_json::to_string(&result).unwrap();
    assert!(
        !json_out.contains("sk-super-secret-token"),
        "list-servers output leaked a credential: {json_out}"
    );
    // But it still reports the server + that auth is configured.
    assert!(json_out.contains("notion"));
    assert!(json_out.contains("auth_configured"));
}

/// End-to-end: drive `mcp_call_tool` against an in-process axum MCP server
/// (plain JSON `initialize` / `tools/list` / `tools/call`, no new deps). The
/// bearer token reaches the *server* over the wire (auth is wired), but the
/// agent-visible `ToolResult` never carries it. This is the regression guard
/// for the "credentials never surface to the agent" invariant.
#[tokio::test]
async fn call_tool_through_agent_path_never_leaks_bearer() {
    use std::sync::Mutex as StdMutex;

    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};
    use oh::security::SecurityPolicy;
    use oh::tools::McpCallTool;

    #[derive(Default)]
    struct Seen {
        auth: StdMutex<Option<String>>,
    }

    async fn handler(
        State(seen): State<Arc<Seen>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
            *seen.auth.lock().unwrap() = Some(auth.to_string());
        }
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": { "name": "fixture", "version": "0" }
            }),
            "tools/list" => json!({
                "tools": [{
                    "name": "echo",
                    "description": "Echoes input.",
                    "inputSchema": { "type": "object" }
                }]
            }),
            "tools/call" => json!({
                "content": [{ "type": "text", "text": "remote ran ok, no secrets here" }],
                "isError": false
            }),
            // A notification (e.g. notifications/initialized) — ack only.
            _ => return Json(json!({ "jsonrpc": "2.0" })),
        };
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/mcp", post(handler))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let endpoint = format!("http://{addr}/mcp");
    let mut d = decl("fixture", &endpoint);
    d.auth = AuthMaterial::Bearer("sk-super-secret-xyz".into());
    let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");
    let tool = McpCallTool::new(registry, Arc::new(SecurityPolicy::default()));

    let result = tool
        .execute(json!({ "server": "fixture", "tool": "echo", "arguments": {} }))
        .await
        .expect("mcp_call_tool");

    // Auth WAS wired: the server received the bearer over the wire.
    assert_eq!(
        seen.auth.lock().unwrap().as_deref(),
        Some("Bearer sk-super-secret-xyz"),
        "the transport must send the configured bearer"
    );
    // But the agent-visible result never carries the token.
    let out = serde_json::to_string(&result).unwrap();
    assert!(
        !out.contains("sk-super-secret-xyz"),
        "mcp_call_tool result leaked a credential: {out}"
    );
    assert!(result.output().contains("remote ran ok"));
}

/// An empty raw request inherits the company belt at the builder seam. The
/// scrubber must receive those effective grants too, or an MCP credential
/// echoed by a server can reach the agent-visible failure even though the
/// registry correctly wires that server.
#[test]
fn granted_secrets_follows_effective_grants() {
    let mut server = decl("fixture", "http://127.0.0.1:1/mcp");
    server.auth = AuthMaterial::Bearer("inherited-canary".into());
    let inherited = granted_secrets(std::slice::from_ref(&server), &grants(&["*", "mcp:*"]));
    assert_eq!(inherited, vec!["inherited-canary"]);

    let omitted = granted_secrets(std::slice::from_ref(&server), &grants(&["*"]));
    assert!(omitted.is_empty());
}

/// SECURITY CANARY: a server that **reflects the submitted credential** in a
/// non-401 error body must not leak it anywhere the `OcMcpCallTool` decorator
/// surfaces — not the agent-visible result, and not the drained failure. This
/// is the regression guard for leak vector #1 (upstream `MCP HTTP {status} —
/// {body}` echoing the body) driven through the REAL vendored transport.
#[tokio::test]
async fn oc_call_tool_scrubs_reflected_credential() {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};
    use oh::security::SecurityPolicy;

    // On tools/call, reflect the Authorization header back in a 500 body — the
    // exact hostile shape that would leak the token through upstream's
    // `MCP HTTP 500 — {body}` surfacing.
    async fn handler(
        State(()): State<()>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {},
                            "serverInfo": { "name": "fixture", "version": "0" } }
            }))
            .into_response(),
            "tools/list" => Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": [{ "name": "echo", "description": "e",
                                        "inputSchema": { "type": "object" } }] }
            }))
            .into_response(),
            "tools/call" => {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("boom — received {auth}"),
                )
                    .into_response()
            }
            _ => Json(json!({ "jsonrpc": "2.0" })).into_response(),
        }
    }

    let app = Router::new().route("/mcp", post(handler)).with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    const CANARY: &str = "sk-canary-REFLECTED-9999";
    let endpoint = format!("http://{addr}/mcp");
    let mut d = decl("fixture", &endpoint);
    d.auth = AuthMaterial::Bearer(CANARY.into());
    let secrets = granted_secrets(std::slice::from_ref(&d), &grants(&["mcp:*"]));
    let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");

    let queue = McpFailureQueue::default();
    let tool = OcMcpCallTool::new(
        registry,
        Arc::new(SecurityPolicy::default()),
        secrets,
        queue.clone(),
        McpMetering::off(),
        Default::default(),
    );

    let result = tool
        .execute(json!({ "server": "fixture", "tool": "echo", "arguments": {} }))
        .await
        .expect("mcp_call_tool");

    // The agent-visible result is an error, but carries NO canary.
    assert!(result.is_error, "a failed call must be an error result");
    let out = serde_json::to_string(&result).unwrap();
    assert!(
        !out.contains(CANARY),
        "OcMcpCallTool result leaked the reflected credential: {out}"
    );

    // The drained failure is recorded, classified, and scrubbed.
    let failures = queue.drain();
    assert_eq!(failures.len(), 1, "the failure was queued");
    assert_eq!(failures[0].server, "fixture");
    assert_eq!(failures[0].status, "server_error");
    let serialized = format!("{:?}", failures[0]);
    assert!(
        !serialized.contains(CANARY),
        "the drained failure leaked the reflected credential: {serialized}"
    );
}

/// A completed MCP call is counted, and a failed one is not (issue #698).
///
/// The rule this exercises — `mcp:` namespacing — is unit-tested in
/// `crate::metering::oauth`. What only this test can reach is the wiring:
/// that the success branch calls the meter at all, that it passes *this*
/// company and agent rather than a default, and that the failure branch
/// stays silent. Deleting the `if let Some(meter)` block, moving it to the
/// `Err` arm, or threading the wrong field all pass every other test in the
/// tree.
///
/// Both outcomes are driven through one fixture whose `tools/call` succeeds
/// or fails on the tool name, because "counts a success" is only half the
/// contract: `connections` is the count of providers seen, so a metered
/// failure would mint a connection row for a server that never answered.
#[tokio::test]
async fn a_completed_mcp_call_is_metered_and_a_failed_one_is_not() {
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::Mutex;

    use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};

    #[derive(Default)]
    struct RecordingMeter {
        samples: Mutex<Vec<(String, UsageSample)>>,
    }

    #[async_trait]
    impl UsageMeter for RecordingMeter {
        async fn record(&self, company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
            self.samples
                .lock()
                .unwrap()
                .push((company.to_string(), sample.clone()));
            Ok(())
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Ok(Vec::new())
        }
    }

    async fn handler(State(()): State<()>, Json(body): Json<Value>) -> axum::response::Response {
        use axum::response::IntoResponse;
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {},
                            "serverInfo": { "name": "fixture", "version": "0" } }
            }))
            .into_response(),
            "tools/list" => Json(json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": [
                    { "name": "echo", "description": "e", "inputSchema": { "type": "object" } },
                    { "name": "boom", "description": "b", "inputSchema": { "type": "object" } }
                ] }
            }))
            .into_response(),
            "tools/call" => {
                let called = body
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if called == "boom" {
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
                }
                Json(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [{ "type": "text", "text": "ok" }] }
                }))
                .into_response()
            }
            _ => Json(json!({ "jsonrpc": "2.0" })).into_response(),
        }
    }

    let app = Router::new().route("/mcp", post(handler)).with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let endpoint = format!("http://{addr}/mcp");
    let registry =
        registry_for_agent(&[decl("fixture", &endpoint)], &grants(&["mcp:*"])).expect("registry");

    let meter = Arc::new(RecordingMeter::default());
    let tool = OcMcpCallTool::new(
        registry,
        Arc::new(SecurityPolicy::default()),
        Vec::new(),
        McpFailureQueue::default(),
        McpMetering {
            company: CompanyId::new("acme"),
            agent: "ceo".to_string(),
            meter: Some(meter.clone()),
        },
        Default::default(),
    );

    let ok = tool
        .execute(json!({ "server": "fixture", "tool": "echo", "arguments": {} }))
        .await
        .expect("mcp_call_tool");
    assert!(!ok.is_error, "the fixture's `echo` succeeds: {ok:?}");

    {
        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples.len(), 1, "one completed call, one sample");
        let (company, sample) = &samples[0];
        assert_eq!(company, "acme", "the sample is scoped to this company");
        assert_eq!(sample.agent, "ceo", "attributed to the calling agent");
        assert_eq!(sample.kind, SampleKind::OauthCall);
        // Namespaced, so this row cannot merge with a Composio toolkit that
        // happens to share the server's name.
        assert_eq!(sample.provider, "mcp:fixture");
        assert_eq!(sample.input_tokens, 0);
        assert_eq!(sample.output_tokens, 0);
        assert_eq!(sample.cost_usd, 0.0);
    }

    let failed = tool
        .execute(json!({ "server": "fixture", "tool": "boom", "arguments": {} }))
        .await
        .expect("mcp_call_tool");
    assert!(failed.is_error, "the fixture's `boom` fails: {failed:?}");
    assert_eq!(
        meter.samples.lock().unwrap().len(),
        1,
        "a call that never reached the server must not mint a connection row"
    );
}

/// The query-parameter credential is **appended** to an endpoint that already
/// carries a (non-secret) query string, reaching the server on the wire —
/// the BrowserBase shape (`?projectId=…` in the URL, `apiKey` as the
/// credential). Proves the upstream `request.query()` path composes rather
/// than replaces, and that our mapping wires it.
#[tokio::test]
async fn query_param_auth_appends_to_existing_query_on_the_wire() {
    use std::sync::Mutex as StdMutex;

    use axum::extract::State;
    use axum::http::Uri;
    use axum::routing::post;
    use axum::{Json, Router};

    #[derive(Default)]
    struct Seen {
        query: StdMutex<Option<String>>,
    }

    async fn handler(
        State(seen): State<Arc<Seen>>,
        uri: Uri,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        if let Some(q) = uri.query() {
            *seen.query.lock().unwrap() = Some(q.to_string());
        }
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let method = body.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "serverInfo": { "name": "fixture", "version": "0" }
            }),
            "tools/list" => json!({ "tools": [] }),
            _ => return Json(json!({ "jsonrpc": "2.0" })),
        };
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/mcp", post(handler))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // The non-secret project id stays in the endpoint; the secret rides as a
    // query-parameter credential.
    let endpoint = format!("http://{addr}/mcp?projectId=pid-123");
    let mut d = decl("browserbase", &endpoint);
    d.auth = AuthMaterial::QueryParam {
        name: "apiKey".into(),
        value: "qp-secret-abc".into(),
    };
    let registry = registry_for_agent(&[d], &grants(&["mcp:*"])).expect("registry");
    // list_tools drives initialize + tools/list over the wire.
    let _ = registry
        .list_tools("browserbase")
        .await
        .expect("list_tools");

    let query = seen
        .query
        .lock()
        .unwrap()
        .clone()
        .expect("server saw a query");
    assert!(
        query.contains("projectId=pid-123"),
        "kept the existing id: {query}"
    );
    assert!(
        query.contains("apiKey=qp-secret-abc"),
        "appended the credential: {query}"
    );
}

// -- McpRuntime tests (origin/main) --

use std::process::Command;

use oh::mcp::registry::types::{CommandKind, Transport};

const NODE_STUB: &str = r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (value) => process.stdout.write(JSON.stringify(value) + '\n');
rl.on('line', (line) => {
  const request = JSON.parse(line);
  if (!request.id) return;
  if (request.method === 'initialize') {
send({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'test', version: '1' } } });
  } else if (request.method === 'tools/list') {
send({ jsonrpc: '2.0', id: request.id, result: { tools: [{ name: 'echo', description: 'Echo text', inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] } }] } });
  } else if (request.method === 'tools/call') {
send({ jsonrpc: '2.0', id: request.id, result: { content: [{ type: 'text', text: 'echo: ' + request.params.arguments.text }] } });
  }
});
"#;

#[tokio::test]
async fn install_connect_call_disconnect_round_trip() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping MCP runtime test because node is unavailable");
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let script = temp.path().join("mcp-stub.cjs");
    std::fs::write(&script, NODE_STUB).expect("write node stub");
    let runtime = McpRuntime::new(temp.path().join("workspace"));
    let server = InstalledServer {
        server_id: uuid::Uuid::new_v4().to_string(),
        qualified_name: "test-node-echo".to_string(),
        display_name: "Test Node Echo".to_string(),
        description: None,
        icon_url: None,
        command_kind: CommandKind::Binary,
        command: "node".to_string(),
        args: vec![script.to_string_lossy().into_owned()],
        env_keys: vec![],
        config: None,
        installed_at: 0,
        last_connected_at: None,
        transport: Transport::Stdio,
        enabled: true,
    };

    runtime.install(&server, &HashMap::new()).expect("install");
    let tools = runtime.connect(&server.server_id).await.expect("connect");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let result = runtime
        .call_tool(
            &server.server_id,
            "echo",
            serde_json::json!({"text": "hello"}),
        )
        .await
        .expect("call");
    assert_eq!(result["content"][0]["text"], "echo: hello");

    assert!(
        runtime
            .disconnect(&server.server_id)
            .await
            .expect("disconnect")
    );
    assert!(
        runtime
            .uninstall(&server.server_id)
            .await
            .expect("uninstall")
    );
    assert!(runtime.list().expect("list").is_empty());
}

/// `get` on an install that was never persisted reports `McpServerNotFound`
/// — the "genuinely absent" half of the store-error split.
#[test]
fn get_on_an_absent_server_reports_not_found() {
    let temp = tempfile::tempdir().expect("tempdir");
    let runtime = McpRuntime::new(temp.path().join("workspace"));
    let error = runtime
        .get("no-such-server")
        .expect_err("an absent install must not resolve");
    assert!(
        matches!(error, OpenCompanyError::McpServerNotFound(ref id) if id == "no-such-server"),
        "absent install must be McpServerNotFound, got: {error:?}"
    );
}

/// `get` on a store that fails to read must NOT be reported as a missing
/// server — the caller would be told to reinstall something that is there.
/// Truncating the SQLite file beneath the runtime's open connection forces
/// the next `get_server` read to fail, and the error must surface as a
/// `Store` error rather than the blanket `McpServerNotFound` the pre-split
/// code produced for every failure.
#[test]
fn get_on_a_store_that_fails_to_read_reports_store_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let runtime = McpRuntime::new(workspace.clone());
    // Open the host so the store and its schema exist on disk.
    runtime.list().expect("open the mcp store");
    // Corrupt the file beneath the open connection: the query can no longer
    // be satisfied, so `get_server` must fail with a store error.
    let db = workspace.join("mcp_clients").join("mcp_clients.db");
    std::fs::write(&db, b"this is not a sqlite database").expect("corrupt the store file");
    let error = runtime
        .get("some-server")
        .expect_err("a store that cannot read must not resolve");
    assert!(
        matches!(error, OpenCompanyError::Store(_)),
        "a failing store read must surface as Store, got: {error:?}"
    );
}

/// The declared family enumerates through `mcp_list_servers`.
#[test]
fn a_declared_only_agent_is_pointed_at_the_declared_enumeration_tools() {
    let brief = capability_brief(true, false);
    assert!(brief.contains("mcp_list_servers"), "{brief}");
    assert!(
        !brief.contains("mcp_registry_installed_list"),
        "it holds no registry tool, so naming one sends it at a tool it cannot see: {brief}"
    );
}

/// The registry family enumerates through different tools, and an agent holding
/// only that family used to be told nothing at all.
#[test]
fn a_registry_only_agent_is_pointed_at_the_registry_enumeration_tools() {
    let brief = capability_brief(false, true);
    assert!(
        !brief.is_empty(),
        "a registry-only agent still gets the brief"
    );
    assert!(brief.contains("mcp_registry_installed_list"), "{brief}");
    assert!(brief.contains("mcp_registry_list_tools"), "{brief}");
    assert!(
        !brief.contains("mcp_list_servers"),
        "`mcp_list_servers` is not on this agent's belt: {brief}"
    );
}

/// Both families wired names both enumeration paths.
#[test]
fn an_agent_holding_both_families_is_pointed_at_both() {
    let brief = capability_brief(true, true);
    assert!(brief.contains("mcp_list_servers"), "{brief}");
    assert!(brief.contains("mcp_registry_installed_list"), "{brief}");
}

/// No MCP family wired means no brief, rather than one naming nothing.
#[test]
fn an_agent_with_no_mcp_family_gets_no_capability_brief() {
    assert_eq!(capability_brief(false, false), "");
}
