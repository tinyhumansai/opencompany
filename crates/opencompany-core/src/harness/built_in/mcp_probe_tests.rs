use super::*;
use crate::company::mcp::{AuthMaterial, McpServerDecl, McpSource};

fn anyhow_str(msg: &str) -> anyhow::Error {
    anyhow::anyhow!("{msg}")
}

fn plain_decl(name: &str, endpoint: &str) -> McpServerDecl {
    McpServerDecl {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        description: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: 30,
        enabled: true,
        source: McpSource::Runtime,
        auth: AuthMaterial::None,
        tool_policies: Default::default(),
        tool_inventory: Default::default(),
    }
}

fn oauth_decl(name: &str, endpoint: &str, access_token: &str) -> McpServerDecl {
    McpServerDecl {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        description: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: 30,
        enabled: true,
        source: McpSource::Runtime,
        auth: AuthMaterial::OAuth {
            access_token: access_token.to_string(),
            refresh_token: Some("rt-oauth-secret".to_string()),
            client_id: "client-abc".to_string(),
            client_secret: Some("cs-oauth-secret".to_string()),
            token_endpoint: "https://as.example/token".to_string(),
            expires_at: u64::MAX,
        },
        tool_policies: Default::default(),
        tool_inventory: Default::default(),
    }
}

/// SECURITY: an OAuth access token planted as the credential must NEVER
/// appear in a probed server's health message — even when the server
/// reflects the `Authorization` header back in an error body. This is the
/// regression guard for issue #90's "OAuth tokens are write-only, never in
/// any status/health/error response" invariant, driven through the REAL
/// vendored transport + the OAuth→bearer mapping.
#[tokio::test]
async fn oauth_token_never_leaks_into_probed_health() {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value;

    // Reflect the Authorization header back in a 500 on `initialize` — the
    // hostile shape that would leak the bearer through the raw error. `Json`
    // is last so it consumes the body after the header read.
    async fn handler(
        State(()): State<()>,
        headers: HeaderMap,
        _body: Json<Value>,
    ) -> axum::response::Response {
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

    let app = Router::new().route("/mcp", post(handler)).with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    const ACCESS: &str = "at-oauth-CANARY-9999";
    let endpoint = format!("http://{addr}/mcp");
    let decl = oauth_decl("oauthy", &endpoint, ACCESS);

    let health = probe_server(&decl).await;

    // The health message is surfaced to the operator — it must carry none of
    // the OAuth secrets (access token reflected by the server, and neither
    // the refresh token nor the client secret which feed the scrubber set).
    assert!(
        !health.message.contains(ACCESS),
        "probed health leaked the OAuth access token: {}",
        health.message
    );
    assert!(
        !health.message.contains("rt-oauth-secret"),
        "{}",
        health.message
    );
    assert!(
        !health.message.contains("cs-oauth-secret"),
        "{}",
        health.message
    );
}

// ---- scrub -------------------------------------------------------------

#[test]
fn scrub_replaces_known_secret() {
    let out = scrub(
        "token was sk-canary-123 here",
        &["sk-canary-123".to_string()],
    );
    assert!(!out.contains("sk-canary-123"), "{out}");
    assert!(out.contains("•••"), "{out}");
}

#[test]
fn scrub_strips_url_query_string() {
    // The lethal case: a reqwest error with the credential in the URL query.
    let msg = "error sending request for url (https://api.browserbase.com/mcp?projectId=pid&apiKey=qp-canary) failed";
    let out = scrub(msg, &[]);
    assert!(
        !out.contains("qp-canary"),
        "query-carried secret leaked: {out}"
    );
    assert!(!out.contains("projectId"), "{out}");
    assert!(out.contains("https://api.browserbase.com/mcp"), "{out}");
}

#[test]
fn scrub_strips_query_and_replaces_secret_together() {
    let msg = "url https://host/mcp?apiKey=qp-canary and bearer sk-canary";
    let out = scrub(msg, &["qp-canary".to_string(), "sk-canary".to_string()]);
    assert!(!out.contains("qp-canary"), "{out}");
    assert!(!out.contains("sk-canary"), "{out}");
}

#[test]
fn scrub_truncates_utf8_safely() {
    let long = "é".repeat(400); // 800 bytes
    let out = scrub(&long, &[]);
    assert!(out.len() <= SCRUB_MAX_BYTES + "…".len());
    assert!(out.ends_with('…'));
}

// ---- classify (string arms — no live dial) -----------------------------

#[test]
fn classify_403_is_token_rejected() {
    let class = classify_mcp_error(&anyhow_str("MCP HTTP 403 — Forbidden"), true, false);
    assert_eq!(class.status, McpStatus::NeedsConfig);
    assert_eq!(class.auth_hint.as_deref(), Some("token_rejected"));
}

#[test]
fn classify_404_is_not_mcp() {
    let class = classify_mcp_error(&anyhow_str("MCP HTTP 404 — Not Found"), false, false);
    assert_eq!(class.status, McpStatus::Error);
    assert_eq!(class.auth_hint, None);
}

#[test]
fn classify_5xx_is_server_error() {
    let class = classify_mcp_error(
        &anyhow_str("MCP HTTP 503 — Service Unavailable"),
        false,
        false,
    );
    assert_eq!(class.status, McpStatus::Error);
}

#[test]
fn classify_bare_401_string_needs_credential_when_none() {
    let class = classify_mcp_error(&anyhow_str("MCP HTTP 401 — Unauthorized"), false, false);
    assert_eq!(class.auth_hint.as_deref(), Some("credential_required"));
    let class = classify_mcp_error(&anyhow_str("MCP HTTP 401 — Unauthorized"), true, false);
    assert_eq!(class.auth_hint.as_deref(), Some("token_rejected"));
}

#[test]
fn classify_parse_failure_is_not_mcp() {
    let class = classify_mcp_error(
        &anyhow_str("Failed to parse MCP JSON response: expected value — body: <html>"),
        false,
        false,
    );
    assert_eq!(class.status, McpStatus::Error);
    assert_eq!(class.auth_hint, None);
}

#[test]
fn classify_json_rpc_error_only_in_call_context() {
    let msg = "MCP error: {\"code\":-32000,\"message\":\"bad args\"}";
    assert_eq!(
        classify_mcp_error(&anyhow_str(msg), false, true).status,
        McpStatus::Error
    );
    // Both are Error tier, but the message routing differs — assert the kind.
    assert_eq!(
        classify_mcp_error(&anyhow_str(msg), false, true).kind,
        FailureKind::ToolCallRejected
    );
    assert_eq!(
        classify_mcp_error(&anyhow_str(msg), false, false).kind,
        FailureKind::Unknown
    );
}

#[test]
fn classify_typed_401_dominates_string() {
    // A typed `Error::Unauthorized` with OAuth metadata → oauth_required,
    // even though the message string would otherwise be generic.
    let err = anyhow::Error::new(tinymcp::Error::Unauthorized {
        endpoint: "host".into(),
        resource_metadata: Some("https://host/.well-known/oauth".into()),
    });
    let class = classify_mcp_error(&err, false, false);
    assert_eq!(class.auth_hint.as_deref(), Some("oauth_required"));
    assert_eq!(class.status, McpStatus::NeedsConfig);
}

/// Issue #1260: the two OAuth states are distinguishable on the wire, and
/// they carry the two different actions an operator has to take.
#[test]
fn the_two_oauth_states_do_not_share_a_hint() {
    assert_eq!(
        FailureKind::OauthRequired.auth_hint().as_deref(),
        Some("oauth_required")
    );
    assert_eq!(
        FailureKind::StaticTokenRequired.auth_hint().as_deref(),
        Some("static_token_required")
    );
    assert_ne!(
        FailureKind::OauthRequired.auth_hint(),
        FailureKind::StaticTokenRequired.auth_hint(),
        "the console renders a Sign in button off this code; collapsing the two \
         is what put an unusable button on a Slack row"
    );
    // Both are a resting state the operator can fix, not a failure.
    assert_eq!(
        FailureKind::StaticTokenRequired.status(),
        McpStatus::NeedsConfig
    );
    assert_eq!(
        FailureKind::StaticTokenRequired.code(),
        "static_token_required"
    );
}

/// The message names the field the operator can actually use.
///
/// The previous text sent them to Connections, which carries no MCP server
/// row at all — a instruction that cannot be followed reads as a broken
/// feature rather than a missing token.
#[test]
fn the_credential_messages_name_a_field_that_exists() {
    for kind in [
        FailureKind::StaticTokenRequired,
        FailureKind::CredentialRequired,
    ] {
        let class = ProbeClass {
            status: kind.status(),
            auth_hint: kind.auth_hint(),
            kind,
        };
        let msg = operator_message("slack", &class, &anyhow_str("x"));
        assert!(
            msg.contains("Token field"),
            "{kind:?} must name the Token field: {msg}"
        );
        assert!(
            !msg.contains("in Connections"),
            "{kind:?} still points at Connections, which has no MCP row: {msg}"
        );
    }
}

/// A refinement only ever fires on the one kind it is about, and every
/// uncertain answer leaves the classification alone.
#[tokio::test]
async fn refinement_leaves_every_other_kind_untouched() {
    for kind in [
        FailureKind::CredentialRequired,
        FailureKind::TokenRejected,
        FailureKind::Timeout,
        FailureKind::Unreachable,
    ] {
        let class = ProbeClass {
            status: kind.status(),
            auth_hint: kind.auth_hint(),
            kind,
        };
        // An endpoint that cannot resolve: discovery fails, and a failed
        // discovery must never rewrite a verdict.
        let refined = refine_oauth_capability("https://127.0.0.1:1/mcp", class.clone()).await;
        assert_eq!(refined, class, "{kind:?} was rewritten by the refinement");
    }
}

/// Discovery that fails keeps `oauth_required` rather than downgrading.
///
/// The safe direction is the one that does not regress a server whose sign-in
/// works: a timeout or a transient must not replace a working Sign in button
/// with "paste a token".
#[tokio::test]
async fn an_unreachable_discovery_keeps_sign_in() {
    let class = ProbeClass {
        status: FailureKind::OauthRequired.status(),
        auth_hint: FailureKind::OauthRequired.auth_hint(),
        kind: FailureKind::OauthRequired,
    };
    let refined = refine_oauth_capability("https://127.0.0.1:1/mcp", class.clone()).await;
    assert_eq!(
        refined, class,
        "an unreachable discovery must leave the verdict as it was"
    );
}

#[test]
fn classify_typed_401_without_metadata_respects_credential_state() {
    let err = anyhow::Error::new(tinymcp::Error::Unauthorized {
        endpoint: "host".into(),
        resource_metadata: None,
    });
    assert_eq!(
        classify_mcp_error(&err, false, false).auth_hint.as_deref(),
        Some("credential_required")
    );
    assert_eq!(
        classify_mcp_error(&err, true, false).auth_hint.as_deref(),
        Some("token_rejected")
    );
}

#[test]
fn operator_message_is_actionable_and_scrubbed() {
    let class = classify_mcp_error(&anyhow_str("MCP HTTP 401 — Unauthorized"), false, false);
    let msg = scrub(
        &operator_message("browserbase", &class, &anyhow_str("x")),
        &[],
    );
    assert!(msg.contains("browserbase"), "{msg}");
    assert!(msg.to_lowercase().contains("credential"), "{msg}");
}

// ---- discovery records an inventory alongside the health --------------

use std::collections::HashMap as StdHashMap;
use std::sync::Mutex as StdMutex;

use crate::company::mcp_policy::{ToolTier, load_tool_inventory, tool_inventory_key};
use crate::ports::SecretStore;
use crate::ports::types::SecretValue;

#[derive(Default)]
struct RecordingSecrets {
    map: StdMutex<StdHashMap<String, String>>,
}

#[async_trait::async_trait]
impl SecretStore for RecordingSecrets {
    async fn get(&self, _c: &CompanyId, key: &str) -> crate::Result<Option<SecretValue>> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(key)
            .map(|v| SecretValue(v.clone())))
    }
    async fn set(&self, _c: &CompanyId, key: &str, value: SecretValue) -> crate::Result<()> {
        self.map.lock().unwrap().insert(key.to_string(), value.0);
        Ok(())
    }
}

/// A probe that cannot reach the server records health but must leave any
/// previous inventory alone: the tools it offered before an outage are the best
/// answer during one, and clearing them would drop every tool a stored tier
/// default reaches.
#[tokio::test]
async fn a_failed_probe_leaves_the_previous_inventory_standing() {
    let company = CompanyId::new("acme");
    let secrets = RecordingSecrets::default();
    let key = tool_inventory_key("fixture");
    secrets
        .set(
            &company,
            &key,
            SecretValue(
                serde_json::json!({
                    "tools": { "search_pages": "read_only" },
                    "discoveredAtMillis": 5u64
                })
                .to_string(),
            ),
        )
        .await
        .unwrap();

    // Nothing listens here, so the listing fails and returns no names.
    let decl = plain_decl("fixture", "http://127.0.0.1:1/mcp");
    let health = probe_and_record(&company, &decl, &secrets).await;
    assert_ne!(health.status, McpStatus::Ok);

    let kept = load_tool_inventory(&company, &secrets, &key).await;
    assert_eq!(kept.suggested("search_pages"), Some(ToolTier::ReadOnly));
    assert_eq!(kept.discovered_at_millis, 5);
}

/// A failed probe still persists its health, so the console reports the outage.
#[tokio::test]
async fn a_failed_probe_still_persists_its_health() {
    let company = CompanyId::new("acme");
    let secrets = RecordingSecrets::default();
    let decl = plain_decl("fixture", "http://127.0.0.1:1/mcp");
    let health = probe_and_record(&company, &decl, &secrets).await;

    let stored = crate::company::mcp::load_health(&company, "fixture", &secrets)
        .await
        .unwrap()
        .expect("health persisted");
    assert_eq!(stored.status, health.status);
}

/// A successful probe records what the server said about itself, off the same
/// handshake the listing already performed. The icon it advertises is not
/// fetched here — a loopback source is exactly what the outbound SSRF guard
/// refuses — so the record carries no icon and the console draws its letter
/// tile.
/// A loopback MCP server whose handshake describes itself, returning its address.
async fn self_describing_fixture() -> std::net::SocketAddr {
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    async fn handler(Json(body): Json<Value>) -> Json<Value> {
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let result = match body.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": {
                    "name": "fixture",
                    "title": "Fixture Docs",
                    "description": "Up-to-date documentation for any library.",
                    "websiteUrl": "https://fixture.example",
                    "icons": [{ "src": "http://127.0.0.1:1/icon.png", "sizes": "48x48" }],
                },
            }),
            "tools/list" => json!({
                "tools": [{
                    "name": "search_pages",
                    "description": "Searches the docs.",
                    "inputSchema": { "type": "object" }
                }]
            }),
            _ => return Json(json!({ "jsonrpc": "2.0" })),
        };
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/mcp", post(handler)))
            .await
            .unwrap();
    });

    addr
}

#[tokio::test]
async fn a_probe_records_what_the_server_says_about_itself() {
    let addr = self_describing_fixture().await;
    let company = CompanyId::new("acme");
    let secrets = RecordingSecrets::default();
    let decl = plain_decl("fixture", &format!("http://{addr}/mcp"));
    let health = probe_and_record(&company, &decl, &secrets).await;
    assert_eq!(health.status, McpStatus::Ok, "{}", health.message);

    let info = crate::company::mcp_server_info::load(&company, "fixture", &secrets).await;
    assert_eq!(info.title.as_deref(), Some("Fixture Docs"));
    assert_eq!(
        info.description.as_deref(),
        Some("Up-to-date documentation for any library.")
    );
    assert_eq!(info.website_url.as_deref(), Some("https://fixture.example"));
    assert!(
        info.icon_data_url.is_none(),
        "a private-host icon source must not be fetched"
    );
}

/// A probe that cannot reach the server must leave the description it recorded
/// earlier standing, for the same reason it leaves the inventory alone.
#[tokio::test]
async fn a_failed_probe_leaves_the_previous_description_standing() {
    use crate::company::mcp_server_info::{self, McpServerInfo};

    let company = CompanyId::new("acme");
    let secrets = RecordingSecrets::default();
    let stored = McpServerInfo {
        title: Some("Fixture Docs".to_string()),
        description: Some("Docs.".to_string()),
        website_url: None,
        icon_data_url: None,
    };
    mcp_server_info::save(&company, "fixture", &stored, &secrets)
        .await
        .unwrap();

    let decl = plain_decl("fixture", "http://127.0.0.1:1/mcp");
    assert_ne!(
        probe_and_record(&company, &decl, &secrets).await.status,
        McpStatus::Ok
    );
    assert_eq!(
        mcp_server_info::load(&company, "fixture", &secrets).await,
        stored
    );
}

/// A server whose handshake carries no usable icon keeps the logo stored for it
/// earlier, such as the directory's, while its own title still wins.
#[tokio::test]
async fn a_probe_keeps_a_stored_logo_the_handshake_does_not_replace() {
    use crate::company::mcp_server_info::{self, McpServerInfo};

    let addr = self_describing_fixture().await;
    let company = CompanyId::new("acme");
    let secrets = RecordingSecrets::default();
    let logo = "data:image/png;base64,iVBORw0KGgo=".to_string();
    mcp_server_info::save(
        &company,
        "fixture",
        &McpServerInfo {
            title: Some("Stored".to_string()),
            description: None,
            website_url: None,
            icon_data_url: Some(logo.clone()),
        },
        &secrets,
    )
    .await
    .unwrap();

    let decl = plain_decl("fixture", &format!("http://{addr}/mcp"));
    assert_eq!(
        probe_and_record(&company, &decl, &secrets).await.status,
        McpStatus::Ok
    );
    let info = mcp_server_info::load(&company, "fixture", &secrets).await;
    assert_eq!(info.title.as_deref(), Some("Fixture Docs"));
    assert_eq!(info.icon_data_url, Some(logo));
}
