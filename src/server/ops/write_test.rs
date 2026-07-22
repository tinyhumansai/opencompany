//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("opencompany-ops-{}", crate::ports::generate_id()))
}

fn manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
    )
    .unwrap()
}

async fn state_with_company(home: &std::path::Path) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    // Every route needs a principal now; the harness signs in as an admin so
    // tests keep asserting write behavior rather than auth.
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send_auth(state, method, uri, body, None).await
}

async fn send_auth(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    } else {
        // No explicit credential: sign in as the harness admin. Every route
        // needs a principal now, so an unauthenticated request would only ever
        // assert 401 rather than the behavior under test.
        request = request.header("cookie", crate::server::test_support::fixed_cookie("acme"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn tasks_crud_round_trips_under_both_scopes() {
    let home = home();
    let state = state_with_company(&home).await;

    // Create via the single-company alias.
    let (status, task) = send(
        &state,
        "POST",
        "/api/v1/company/tasks",
        Some(json!({"title": "Q2 brief", "priority": "high"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["title"], "Q2 brief");
    assert_eq!(task["column"], "backlog");
    let id = task["id"].as_str().unwrap().to_string();

    // Drag (PATCH column) via the {id} scope.
    let (status, moved) = send(
        &state,
        "PATCH",
        &format!("/api/v1/companies/acme/tasks/{id}"),
        Some(json!({"column": "done"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(moved["column"], "done");

    // List (GET) reflects the write — the board the console reads.
    let (status, board) = send(&state, "GET", "/api/v1/company/tasks", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = board.as_array().expect("array of cards");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id);
    assert_eq!(rows[0]["column"], "done");

    // Delete.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Second delete is a 404.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/tasks/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn memory_create_and_delete_journals_event() {
    let home = home();
    let state = state_with_company(&home).await;

    let (status, fact) = send(
        &state,
        "POST",
        "/api/v1/company/memory",
        Some(json!({"kind": "preference", "title": "Tone", "body": "Warm"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fact["kind"], "preference");
    let id = fact["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/memory/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn workspace_create_write_move_and_cycle_rejection() {
    let home = home();
    let state = state_with_company(&home).await;

    let (_, folder) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Brand", "kind": "folder"})),
    )
    .await;
    let folder_id = folder["id"].as_str().unwrap().to_string();

    let (status, file) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "voice.md", "kind": "file", "parentId": folder_id, "content": "# Voice"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let file_id = file["id"].as_str().unwrap().to_string();

    // Overwrite content.
    let (status, ack) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/workspace/file/{file_id}"),
        Some(json!({"content": "# Voice v2"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ack["updatedAt"].is_number());

    // Explicit `"parentId": null` moves the file back to the workspace root.
    let (status, moved) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/workspace/{file_id}"),
        Some(json!({"parentId": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        moved.get("parentId").is_none(),
        "node moved to root has no parentId"
    );

    // Cycle rejection: move a folder under its own child.
    let (_, child) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({"name": "Sub", "kind": "folder", "parentId": folder_id})),
    )
    .await;
    let child_id = child["id"].as_str().unwrap().to_string();
    let (status, body) = send(
        &state,
        "PATCH",
        &format!("/api/v1/company/workspace/{folder_id}"),
        Some(json!({"parentId": child_id})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    // Recursive delete.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/workspace/{folder_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn skills_install_toggle_custom_and_builtin_uninstall_conflict() {
    let home = home();
    let state = state_with_company(&home).await;

    // Install from registry, carrying the entry's metadata so the host persists
    // a real SKILL.md the agent can act on (not a content-less slug).
    let (status, skill) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/install",
        Some(json!({
            "name": "Web Research",
            "description": "Answer a question from multiple sources with citations.",
            "category": "Research"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(skill["source"], "registry");
    assert!(skill["enabled"].as_bool().unwrap());
    // The install response reflects the persisted custom_doc (parsed back), so a
    // non-empty description proves content was stored — the fix for the agent
    // never receiving registry skills.
    assert_eq!(skill["name"], "Web Research");
    assert_eq!(
        skill["description"],
        "Answer a question from multiple sources with citations."
    );

    // Uninstall the registry skill: 204.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/skills/web-research/uninstall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Uninstalling an unknown/built-in skill is a 409.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/skills/onboard/uninstall",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");

    // Author a custom skill.
    let (status, custom) = send(
        &state,
        "POST",
        "/api/v1/company/skills",
        Some(json!({"name": "My Skill", "description": "Does a thing", "category": "Ops"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(custom["source"], "custom");
    assert_eq!(custom["name"], "My Skill");

    // Toggle it off.
    let (status, toggled) = send(
        &state,
        "PUT",
        "/api/v1/company/skills/my-skill",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!toggled["enabled"].as_bool().unwrap());

    // `GET …/skills` returns the effective set: here (no source dir) that's the
    // deltas — the custom skill, now disabled.
    let (status, list) = send(&state, "GET", "/api/v1/company/skills", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = list.as_array().expect("a JSON array of skills");
    let my_skill = rows
        .iter()
        .find(|s| s["id"] == "my-skill")
        .expect("the custom skill is listed");
    assert_eq!(my_skill["source"], "custom");
    assert_eq!(my_skill["name"], "My Skill");
    assert!(!my_skill["enabled"].as_bool().unwrap());

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn team_overlay_add_delete_and_manifest_delete_conflict() {
    let home = home();
    let state = state_with_company(&home).await;

    // Add an overlay teammate.
    let (status, member) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana", "role": "Designer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member["role"], "Designer");
    let id = member["id"].as_str().unwrap().to_string();

    // Deleting a manifest teammate is a 409.
    let (status, body) = send(&state, "DELETE", "/api/v1/company/team/ceo", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");

    // Deleting the overlay teammate is a 204.
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/team/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Toggle an inbox on.
    let (status, ack) = send(
        &state,
        "PUT",
        "/api/v1/company/team/ceo/inbox",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["key"], "ceo");

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn inbox_read_marks_and_reports_unread() {
    use crate::ports::inbox::EmailRecord;
    let home = home();
    let state = state_with_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    for i in 0..2 {
        runtime
            .inbox()
            .append(
                runtime.id(),
                &EmailRecord {
                    id: format!("m{i}"),
                    inbox: "ceo".into(),
                    from_name: "S".into(),
                    from_email: "s@x.test".into(),
                    subject: "hi".into(),
                    body: "yo".into(),
                    at_millis: i,
                    read: false,
                    outbound: false,
                },
            )
            .await
            .unwrap();
    }

    // Mark one read; one remains unread.
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/inboxes/ceo/read",
        Some(json!({"ids": ["m0"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 1);

    // Mark the rest.
    let (status, body) = send(&state, "POST", "/api/v1/company/inboxes/ceo/read", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], 0);

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn chat_accepts_desk_id_and_replies() {
    let home = home();
    let state = state_with_company(&home).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({"message": "hello", "chat": "Creative studio"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["responses"].is_array());

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn credential_route_rejects_foreign_tenant() {
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, StaticPlatformVerifier,
    };
    use std::collections::HashSet;

    let home = home();
    // Platform mode: `acme` is owned by `tenant:acme`.
    let verifier = std::sync::Arc::new(StaticPlatformVerifier::new("plat-secret"));
    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_platform_auth(PlatformAuthConfig::new(verifier));
    let id = CompanyId::new("acme");
    let runtime = RuntimeBuilder::new(home.clone(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    state
        .registry()
        .insert(id.clone(), std::sync::Arc::new(runtime));
    state.set_owner(id.clone(), "tenant:acme");

    let token = |tenant: &str| {
        StaticPlatformVerifier::tenant_token(&PlatformClaims {
            tenant: tenant.to_string(),
            scopes: HashSet::from(["operator".to_string()]),
            companies: None,
        })
    };

    // A foreign tenant cannot set acme's domain (credential route is scoped).
    let (status, _) = send_auth(
        &state,
        "PUT",
        "/api/v1/companies/acme/domain",
        Some(json!({"domain": "acme.test"})),
        Some(&token("tenant:evil")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The owning tenant succeeds.
    let (status, _) = send_auth(
        &state,
        "PUT",
        "/api/v1/companies/acme/domain",
        Some(json!({"domain": "acme.test"})),
        Some(&token("tenant:acme")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn unknown_company_scope_is_404() {
    let home = home();
    let state = state_with_company(&home).await;
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/companies/ghost/tasks",
        Some(json!({"title": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    tokio::fs::remove_dir_all(&home).await.ok();
}

// ---------------------------------------------------------------------------
// MCP servers (issue #50)
// ---------------------------------------------------------------------------

/// A manifest that declares one committed `[[mcp_server]]` — used to assert the
/// manifest-server guards (cannot delete; overridable).
fn mcp_manifest() -> CompanyManifest {
    toml::from_str(
        "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n[[mcp_server]]\nname = \"docs\"\nendpoint = \"https://docs.example/mcp\"\n",
    )
    .unwrap()
}

/// Boots an fs-backed company from a caller-supplied manifest (mirrors
/// `state_with_company`, which pins the default manifest).
async fn state_with_manifest(home: &std::path::Path, manifest: CompanyManifest) -> AppState {
    use crate::ports::CompanyStore;
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id, std::sync::Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

#[tokio::test]
async fn mcp_servers_crud_round_trips_and_token_is_write_only() {
    let home = home();
    let state = state_with_company(&home).await;

    // Cold: no servers.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0);

    // Add a runtime server WITH a token.
    let (status, added) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({
            "name": "notion",
            "endpoint": "https://notion.example/mcp",
            "token": "sk-write-only-abc",
            "allowedTools": ["search"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(added["server"]["name"], "notion");
    assert_eq!(added["server"]["source"], "runtime");
    assert_eq!(added["server"]["authConfigured"], true);
    assert!(added["note"].as_str().unwrap().contains("rebuild"));

    // The token must NOT appear anywhere in the add response.
    assert!(
        !serde_json::to_string(&added).unwrap().contains("sk-write-only-abc"),
        "add response leaked the token"
    );

    // GET reflects it, still without the token.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = serde_json::to_string(&list).unwrap();
    assert!(body.contains("notion"));
    assert!(body.contains("\"authConfigured\":true"));
    assert!(!body.contains("sk-write-only-abc"), "list leaked the token");

    // Duplicate add is a 409.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "notion", "endpoint": "https://notion.example/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Non-http endpoint is a 400.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "bad", "endpoint": "ftp://x/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Disable via PUT.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/notion",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["enabled"], false);
    assert_eq!(updated["server"]["authConfigured"], true, "token survives an update");

    // Delete (runtime server) → 204, then it's gone.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/notion", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);

    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn mcp_manifest_server_cannot_be_deleted_but_can_be_overridden() {
    let home = home();
    let state = state_with_manifest(&home, mcp_manifest()).await;

    // The manifest server shows up as `manifest`.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["name"], "docs");
    assert_eq!(list[0]["source"], "manifest");

    // Deleting a manifest server is a 409.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/docs", None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // But it can be disabled via a runtime override — still badged manifest.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/docs",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["source"], "manifest");
    assert_eq!(updated["server"]["enabled"], false);

    tokio::fs::remove_dir_all(&home).await.ok();
}

/// Without the `openhuman` feature there is no MCP transport, so live discovery
/// is "not wired". (Under the feature it would attempt a real network call.)
#[cfg(not(feature = "openhuman"))]
#[tokio::test]
async fn mcp_discovery_is_not_wired_without_the_feature() {
    let home = home();
    let state = state_with_manifest(&home, mcp_manifest()).await;
    let (status, body) = send(&state, "GET", "/api/v1/company/mcp/servers/docs/tools", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_wired");
    tokio::fs::remove_dir_all(&home).await.ok();
}
