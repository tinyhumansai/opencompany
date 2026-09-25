//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::http::StatusCode;
use serde_json::json;

use super::write_test_support::*;
use crate::company::CompanyManifest;

#[tokio::test]
async fn mcp_servers_crud_round_trips_and_token_is_write_only() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
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
    // Issue #566: a mutating MCP change reaches agents on the company's next turn
    // (the effective set is re-fingerprinted every `HarnessPool::ensure` cycle), so
    // the note must state the no-restart contract outright — not merely avoid one
    // stale phrase. Asserting the positive claim rejects any "restart required"
    // variant too, which a bare `!contains("restart the company")` would let pass.
    let note = added["note"].as_str().unwrap();
    assert!(
        note.contains("next turn"),
        "note should promise next-turn pickup: {note}"
    );
    assert!(
        note.contains("no restart needed"),
        "mutating MCP response must state no restart is needed: {note}"
    );

    // The token must NOT appear anywhere in the add response.
    assert!(
        !serde_json::to_string(&added)
            .unwrap()
            .contains("sk-write-only-abc"),
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
    assert_eq!(
        updated["server"]["authConfigured"], true,
        "token survives an update"
    );

    // Delete (runtime server) → 204, then it's gone.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/notion", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
}

/// Issue #1270: a build without the `mcp` feature must serve List A exactly as
/// before and answer the directory routes `not_wired`.
///
/// Gated on the absence of the feature rather than written once for both builds:
/// with `mcp` on, these routes reach a live registry and two upstream
/// directories over the network, which is not a thing a unit test may do. The
/// default `cargo test --locked` lane is what runs this, and it is the lane that
/// compiles the unwired half in the first place.
#[cfg(not(feature = "mcp"))]
#[tokio::test]
async fn without_the_mcp_feature_the_directory_is_not_wired_and_list_a_is_unchanged() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "notion", "endpoint": "https://notion.example/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // List A is served, and carries none of the registry-only keys.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["source"], "runtime");
    for key in ["serverId", "qualifiedName", "iconUrl", "transport"] {
        assert!(
            list[0].get(key).is_none(),
            "`{key}` must not appear without a registry install"
        );
    }

    // Every directory route answers the console's degrade signal.
    for (method, uri) in [
        ("GET", "/api/v1/company/mcp/registry/search?q=git"),
        (
            "GET",
            "/api/v1/company/mcp/registry/entry?qualifiedName=@a/b",
        ),
        ("POST", "/api/v1/company/mcp/registry/install"),
        ("POST", "/api/v1/company/mcp/registry/sid/connect"),
        ("POST", "/api/v1/company/mcp/registry/sid/disconnect"),
        ("PUT", "/api/v1/company/mcp/registry/sid/env"),
        ("DELETE", "/api/v1/company/mcp/registry/sid"),
    ] {
        let (status, body) = send(&state, method, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(body["code"], "not_wired", "{method} {uri}");
    }

    // And the List A delete still works with no install behind the row.
    let (status, _) = send(&state, "DELETE", "/api/v1/company/mcp/servers/notion", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn mcp_manifest_server_cannot_be_deleted_but_can_be_overridden() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
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
    // The mutating response carries reachability too (issue #568), so the console
    // reflects who can reach the server right after an edit, not only on reload.
    assert!(
        updated["server"]["reachableBy"].is_array(),
        "a mutating response also carries reachableBy"
    );
}

/// Issue #568: each listed server carries the agents whose *effective* grants
/// reach it — over the full runtime roster, manifest agents plus overlay
/// teammates. With a company `allow = ["*", "mcp:*"]`, an agent that declares
/// no `tools` (and every overlay teammate, which has no tools row) inherits the
/// wildcard and explicit MCP grant and reaches every server; an agent that
/// narrows itself to `mcp:notion` reaches only that server.
#[tokio::test]
async fn mcp_reachability_lists_reaching_agents_including_overlay() {
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"*\", \"mcp:*\"]\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\ntools = [\"mcp:notion\"]\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n[policy]\nmode = \"full\"\n\
         [[mcp_server]]\nname = \"notion\"\nendpoint = \"https://notion.example/mcp\"\n\
         [[mcp_server]]\nname = \"linear\"\nendpoint = \"https://linear.example/mcp\"\n",
    )
    .unwrap();
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    // A minted id, exactly as `POST …/team` gives an operator-added teammate —
    // the shape that used to reach the console's "Reachable by" line raw (#931).
    let overlay = crate::ports::types::OverlayAgent {
        provider: None,
        id: "019fa75dbc9b-000000000001".to_string(),
        name: "Helper".to_string(),
        role: "Assistant".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    };
    let state = state_with_manifest_and_overlays(&home, manifest, vec![overlay]).await;

    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    let reach = |name: &str| -> Vec<(String, String)> {
        let row = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("server `{name}` is listed"));
        let mut agents: Vec<(String, String)> = row["reachableBy"]
            .as_array()
            .expect("reachableBy serializes as an array")
            .iter()
            .map(|v| {
                (
                    v["id"].as_str().unwrap().to_string(),
                    v["name"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        agents.sort();
        agents
    };
    let pair = |id: &str, name: &str| (id.to_string(), name.to_string());

    // notion: the narrowed ceo, the wildcard-inheriting eng, and the overlay.
    // Issue #931: every row carries the display label the rest of the console
    // uses — a manifest agent's role, an overlay teammate's name — so the minted
    // overlay id is never what a reader sees.
    // The four baseline teammates every company inherits ask for `mcp:*`, so a
    // company granting it reaches them too. Listed rather than filtered out:
    // this asserts the whole reachable set, and hiding the half that is not
    // this manifest's own would leave the baseline free to drift unseen.
    assert_eq!(
        reach("notion"),
        vec![
            pair("019fa75dbc9b-000000000001", "Helper"),
            pair("ceo", "Chief"),
            pair("eng", "Engineer"),
            pair("operations", "Operations"),
            pair("page_builder", "Page Builder"),
            pair("researcher", "Researcher"),
            pair("writer", "Writer"),
        ]
    );
    // linear: only the wildcard holders — ceo scoped itself out of it.
    assert_eq!(
        reach("linear"),
        vec![
            pair("019fa75dbc9b-000000000001", "Helper"),
            pair("eng", "Engineer"),
            pair("operations", "Operations"),
            pair("page_builder", "Page Builder"),
            pair("researcher", "Researcher"),
            pair("writer", "Writer"),
        ],
        "ceo narrowed to mcp:notion, so it cannot reach linear"
    );
}

/// Issue #568: a server no agent's grants cover comes back with an **empty**
/// `reachableBy` — the signal the console flags loudly rather than showing a
/// healthy server that is silently unreachable. Here a narrow company
/// `allow = ["mcp:docs"]` reaches `docs` but never `notion`.
#[tokio::test]
async fn mcp_reachability_flags_a_server_no_agent_can_reach() {
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"mcp:docs\"]\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\ntools = [\"mcp:docs\"]\n[policy]\nmode = \"full\"\n\
         [[mcp_server]]\nname = \"docs\"\nendpoint = \"https://docs.example/mcp\"\n\
         [[mcp_server]]\nname = \"notion\"\nendpoint = \"https://notion.example/mcp\"\n",
    )
    .unwrap();
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, manifest).await;

    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = |name: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("server `{name}` is listed"))
            .clone()
    };
    assert_eq!(
        row("docs")["reachableBy"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["id"].as_str().unwrap(), v["name"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        vec![("ceo", "Chief")],
        "the company allow covers mcp:docs for the one agent"
    );
    assert!(
        row("notion")["reachableBy"].as_array().unwrap().is_empty(),
        "no agent's grants cover mcp:notion — the flagged zero case"
    );
}

/// Issue #568: a **disabled** server reaches nobody, however wide the grants.
/// `registry_for_agent` filters on `decl.enabled && grants_cover_server(..)`, so
/// an agent holding `mcp:docs` is handed no such tool while the server is off —
/// reporting it as reachable would be the console/harness disagreement this
/// feature exists to remove. Asserted on both readers: the mutating response
/// that turns the server off, and the later list.
#[tokio::test]
async fn mcp_reachability_is_empty_for_a_disabled_server() {
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[tools]\nallow = [\"*\", \"mcp:*\"]\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\ntools = [\"mcp:docs\"]\n[policy]\nmode = \"full\"\n\
         [[mcp_server]]\nname = \"docs\"\nendpoint = \"https://docs.example/mcp\"\n",
    )
    .unwrap();
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, manifest).await;

    let reach = |body: &serde_json::Value| -> Vec<String> {
        body["reachableBy"]
            .as_array()
            .expect("reachableBy serializes as an array")
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect()
    };

    // Enabled: the one agent's grant covers it, and so does the baseline's —
    // this company grants `mcp:*`, which the inherited teammates ask for. The
    // disabled assertion below is the one this test is about.
    let (status, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        reach(&list[0]),
        vec![
            "ceo".to_string(),
            "operations".to_string(),
            "page_builder".to_string(),
            "researcher".to_string(),
            "writer".to_string(),
        ]
    );

    // Disabling it empties reachability in the mutating response itself.
    let (status, updated) = send(
        &state,
        "PUT",
        "/api/v1/company/mcp/servers/docs",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["server"]["enabled"], false);
    assert!(
        reach(&updated["server"]).is_empty(),
        "a disabled server is handed to no agent, so it is reachable by none"
    );

    // And the list agrees on the next read — the grant is unchanged, the server is off.
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(list[0]["enabled"], false);
    assert!(
        reach(&list[0]).is_empty(),
        "the list reader applies the same enabled filter as the harness"
    );
}

/// Issue #2373 prerequisite (c): a build with `openhuman` but without `mcp` must
/// not answer a probe with a result it cannot act on.
///
/// `mcp` implies `openhuman`, so `--features openhuman` alone is the build where
/// the transport exists — the probe really would dial — while the agent-side
/// bridge tools in `harness::built_in::build`, which are `#[cfg(feature =
/// "mcp")]`, do not. Before the short-circuit, adding a server here ran a live
/// probe and reported its outcome, so a reachable endpoint produced a green
/// `Test connection` on a build that wires the server to nobody.
///
/// The endpoint below is a closed loopback port, which is the case that
/// distinguishes the two behaviours without touching the network: a real probe
/// would come back `error` (connection refused), so `unknown` can only mean the
/// probe was skipped. `rust-gated` runs `--features openhuman` with `--tests`
/// and no filter, which is the lane that executes this.
#[cfg(all(feature = "openhuman", not(feature = "mcp")))]
#[tokio::test]
async fn without_the_mcp_feature_a_probe_is_skipped_rather_than_reported() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, added) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({
            "name": "closed",
            "endpoint": "https://127.0.0.1:9/mcp",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let test = &added["test"];
    assert_eq!(
        test["status"], "unknown",
        "a build without `mcp` must not report a probe outcome: {added}"
    );
    assert!(
        test["message"]
            .as_str()
            .unwrap_or_default()
            .contains("`mcp` feature"),
        "the message must name the build rather than the endpoint: {added}"
    );
    assert_eq!(test["toolCount"], 0);

    // The read-back agrees with the mutation response, the same invariant
    // `mutation_response` keeps for a live probe.
    let (status, listed) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["health"]["status"], "unknown");
}
