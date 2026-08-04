//! Cross-cutting tests for the GraphQL read plane: a four-case suite per query
//! and a committed SDL snapshot that freezes the read contract for WS7.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

pub(crate) fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-gql-")
        .tempdir()
        .expect("tempdir")
}

pub(crate) fn manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap()
}

pub(crate) async fn state_with_company(home: &std::path::Path) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    // Every route needs a principal now; the harness signs in as an admin so
    // tests can keep asserting resolver behavior rather than auth.
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

pub(crate) async fn query(app: axum::Router, body: &str) -> serde_json::Value {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn companies_query_lists_the_company() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ companies { id name lifecycle pendingApprovals } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["companies"][0]["id"], "acme");
    assert_eq!(value["data"]["companies"][0]["name"], "Acme");
    assert_eq!(value["data"]["companies"][0]["lifecycle"], "running");
}

#[tokio::test]
async fn company_query_by_id_resolves() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id: \"acme\") { id pendingApprovals } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["company"]["id"], "acme");
}

#[tokio::test]
async fn company_query_without_id_resolves_the_sole_company() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_company(&home).await);
    let value = query(app, r#"{"query":"{ company { id } }"}"#).await;
    assert_eq!(value["data"]["company"]["id"], "acme");
}

#[tokio::test]
async fn unknown_company_query_is_null() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_company(&home).await);
    let value = query(app, r#"{"query":"{ company(id: \"ghost\") { id } }"}"#).await;
    assert!(value["data"]["company"].is_null());
}

#[tokio::test]
async fn approvals_field_is_empty_before_any_park() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id: \"acme\") { approvals { id kind } } }"}"#,
    )
    .await;
    assert_eq!(
        value["data"]["company"]["approvals"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

// ---------------------------------------------------------------------------
// Manifest-derived + store-backed reads, over a fuller company.
// ---------------------------------------------------------------------------

fn rich_manifest() -> CompanyManifest {
    toml::from_str(
        r#"
[company]
name = "Acme"
[policy]
mode = "full"
[[agent]]
id = "maya"
role = "Marketing Lead"
description = "Runs campaigns."
[[group_chat]]
id = "general"
name = "General"
description = "Company-wide desk."
members = ["maya"]
[[connection]]
provider = "slack"
reason = "Post updates."
"#,
    )
    .unwrap()
}

async fn state_with_rich_company(home: &std::path::Path) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: rich_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), rich_manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    // Every route needs a principal now; the harness signs in as an admin so
    // tests can keep asserting resolver behavior rather than auth.
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

#[tokio::test]
async fn team_lists_manifest_teammates() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ team { id role name inboxEnabled } } }"}"#,
    )
    .await;
    let team = value["data"]["company"]["team"].as_array().unwrap();
    assert_eq!(team.len(), 1);
    assert_eq!(team[0]["id"], "maya");
    assert_eq!(team[0]["role"], "Marketing Lead");
    assert!(team[0]["name"].is_null());
}

/// Issue #343: the GraphQL roster resolves the **effective** cap and its
/// attribution, so the two reads of the same roster cannot drift.
///
/// The REST handler is the console's consumer, but this resolver reads the same
/// record and has its own copy of the merge — which is exactly how a surface
/// ends up reporting a manifest cap the dispatch gate stopped enforcing. Both
/// halves are asserted here: a manifest teammate whose cap was overridden, and
/// an overlay teammate that the pre-#343 arm hardcoded to `null`.
#[tokio::test]
async fn team_reports_the_effective_cap_and_its_attribution() {
    use crate::ports::types::{Actor, ActorKind, BudgetOverride, OverlayAgent};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;

    let id = CompanyId::new("acme");
    let store = FsCompanyStore::new(home.clone());
    let mut record = store.load(&id).await.unwrap().unwrap();
    record.overlay_agents.push(OverlayAgent {
        id: "jamie".to_string(),
        name: "Jamie".to_string(),
        role: "Growth".to_string(),
        description: None,
    });
    let admin = Actor {
        kind: ActorKind::User,
        id: "user-admin".to_string(),
    };
    record.overlay_budgets = vec![
        BudgetOverride {
            agent_id: "maya".to_string(),
            budget_usd_daily: Some(7.5),
            set_by: admin.clone(),
            at_millis: 1_700_000_000_000,
        },
        BudgetOverride {
            agent_id: "jamie".to_string(),
            budget_usd_daily: Some(2.0),
            set_by: admin,
            at_millis: 1_700_000_000_001,
        },
    ];
    store.save(&record).await.unwrap();

    let value = query(
        router(state),
        r#"{"query":"{ company(id:\"acme\"){ team { id budgetUsdDaily budgetSetBy budgetSetAtMillis } } }"}"#,
    )
    .await;
    let team = value["data"]["company"]["team"].as_array().unwrap();

    let maya = team.iter().find(|m| m["id"] == "maya").unwrap();
    assert_eq!(maya["budgetUsdDaily"], 7.5, "{maya}");
    assert_eq!(maya["budgetSetBy"], "user-admin", "{maya}");
    assert_eq!(maya["budgetSetAtMillis"], 1_700_000_000_000f64, "{maya}");

    let jamie = team.iter().find(|m| m["id"] == "jamie").unwrap();
    assert_eq!(
        jamie["budgetUsdDaily"], 2.0,
        "an overlay teammate is no longer hardcoded uncapped: {jamie}"
    );
}

/// Issue #343: the three budget states stay distinct **on the wire**, where the
/// console reads them.
///
/// `effective_budget` keeping them apart in Rust is necessary but not
/// sufficient — GraphQL flattens `Option<f64>` to a JSON value, and that is
/// where the states can collapse without any Rust type changing:
///
/// - a stored `Some(0.0)` must serialize as numeric **`0`**, not `null`. If it
///   arrives as `null` the console renders "no cap" for a teammate an admin
///   deliberately muted, and the operator has no way to see the mute they set.
/// - an explicit `None` must serialize as **`null` with the attribution still
///   present**. The attribution is the only thing distinguishing "an admin
///   uncapped this teammate" from "nobody ever set anything" — the two states
///   whose difference the whole `Option<Option<f64>>` wire shape exists to carry.
/// - a manifest cap with **no** override must serialize as the manifest number
///   with **no** attribution, so the console never invents a "set by" line for a
///   value that came out of `company.toml`.
///
/// All three are asserted against `Value::Null` / a numeric literal rather than
/// with `is_some()`, because `assert!(v.is_null())` and an absent key are the
/// same thing in `serde_json` — and "absent" is a fourth state the console would
/// read as uncapped.
#[tokio::test]
async fn team_keeps_zero_explicit_null_and_manifest_only_caps_distinct() {
    use crate::ports::types::{Actor, ActorKind, BudgetOverride, OverlayAgent};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;

    let id = CompanyId::new("acme");
    let store = FsCompanyStore::new(home.clone());
    let mut record = store.load(&id).await.unwrap().unwrap();

    // `maya` (manifest) gets a manifest cap and NO override — the fallback arm.
    record.manifest.agents[0].budget_usd_daily = Some(4.25);

    // Two overlay teammates carry the two override states.
    record.overlay_agents.push(OverlayAgent {
        id: "zeroed".to_string(),
        name: "Zeroed".to_string(),
        role: "Growth".to_string(),
        description: None,
    });
    record.overlay_agents.push(OverlayAgent {
        id: "uncapped".to_string(),
        name: "Uncapped".to_string(),
        role: "Ops".to_string(),
        description: None,
    });
    let admin = Actor {
        kind: ActorKind::User,
        id: "user-admin".to_string(),
    };
    record.overlay_budgets = vec![
        BudgetOverride {
            agent_id: "zeroed".to_string(),
            budget_usd_daily: Some(0.0),
            set_by: admin.clone(),
            at_millis: 1_700_000_000_000,
        },
        BudgetOverride {
            agent_id: "uncapped".to_string(),
            budget_usd_daily: None,
            set_by: admin,
            at_millis: 1_700_000_000_001,
        },
    ];
    store.save(&record).await.unwrap();

    let value = query(
        router(state),
        r#"{"query":"{ company(id:\"acme\"){ team { id budgetUsdDaily budgetSetBy budgetSetAtMillis } } }"}"#,
    )
    .await;
    let team = value["data"]["company"]["team"].as_array().unwrap();

    // 1. `Some(0.0)` — numeric zero, attributed. Not null, not absent.
    // `as_f64` is `None` for both null and an absent key, so this one assertion
    // rules out all three ways a zero cap could stop being a number.
    let zeroed = team.iter().find(|m| m["id"] == "zeroed").unwrap();
    assert_eq!(
        zeroed["budgetUsdDaily"].as_f64(),
        Some(0.0),
        "a zero cap must arrive as numeric 0 — null or absent would render a \
         muted teammate as uncapped: {zeroed}"
    );
    assert_eq!(zeroed["budgetSetBy"], "user-admin", "{zeroed}");
    assert_eq!(
        zeroed["budgetSetAtMillis"], 1_700_000_000_000f64,
        "{zeroed}"
    );

    // 2. Explicit `None` — null cap, attribution still present.
    let uncapped = team.iter().find(|m| m["id"] == "uncapped").unwrap();
    assert_eq!(
        uncapped["budgetUsdDaily"],
        serde_json::Value::Null,
        "an explicitly-uncapped override must arrive as a null cap: {uncapped}"
    );
    assert_eq!(
        uncapped["budgetSetBy"], "user-admin",
        "attribution is what tells an admin-set uncap apart from no override at \
         all — it must survive the cap being null: {uncapped}"
    );
    assert_eq!(
        uncapped["budgetSetAtMillis"], 1_700_000_000_001f64,
        "{uncapped}"
    );

    // 3. Manifest cap, no override — the manifest number, no attribution.
    let maya = team.iter().find(|m| m["id"] == "maya").unwrap();
    assert_eq!(
        maya["budgetUsdDaily"].as_f64(),
        Some(4.25),
        "with no override stored the manifest value must come through: {maya}"
    );
    assert_eq!(
        maya["budgetSetBy"],
        serde_json::Value::Null,
        "a manifest cap has no operator to attribute it to: {maya}"
    );
    assert_eq!(
        maya["budgetSetAtMillis"],
        serde_json::Value::Null,
        "a manifest cap has no set-at timestamp: {maya}"
    );
}

#[tokio::test]
async fn chats_list_the_manifest_desks() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ chats { id name members } } }"}"#,
    )
    .await;
    let chats = value["data"]["company"]["chats"].as_array().unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["id"], "general");
    assert_eq!(chats[0]["members"][0], "maya");
}

/// Issue #65: `AgentReply`s answering the console's default thread are
/// journaled with `chat_id == "main"` (the frontend's thread id), not
/// `"General"`. The General desk's `Chat.history` must still find them
/// alongside a reply journaled the "canonical" way, so the operator
/// transcript is never split by which id a given turn happened to use.
#[tokio::test]
async fn chat_history_finds_agent_replies_under_general_and_main() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .events()
        .append(
            runtime.id(),
            crate::ports::types::CompanyEvent::AgentReply {
                task_id: None,
                chat_id: "General".to_string(),
                agent_id: "maya".to_string(),
                text: "canonical id".to_string(),
                steps: Vec::new(),
            },
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            runtime.id(),
            crate::ports::types::CompanyEvent::AgentReply {
                task_id: None,
                chat_id: "main".to_string(),
                agent_id: "maya".to_string(),
                text: "console default-thread id".to_string(),
                steps: Vec::new(),
            },
        )
        .await
        .unwrap();

    let app = router(state);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ chat(id:\"general\"){ history(first: 10) { items { text } } } } }"}"#,
    )
    .await;
    let texts: Vec<&str> = value["data"]["company"]["chat"]["history"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert!(texts.contains(&"canonical id"), "missing: {texts:?}");
    assert!(
        texts.contains(&"console default-thread id"),
        "missing: {texts:?}"
    );
}

/// Issue #246 + #65: the card a reply opened is projected on **both** history
/// surfaces, from the one shared `MessageView` field. The console reads REST,
/// but GraphQL is the paginated surface, and #65 exists precisely because the
/// two drifting apart is how a transcript ends up meaning different things
/// depending on which door you came in.
#[tokio::test]
async fn chat_history_projects_the_card_a_reply_opened() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    for (text, task_id) in [
        ("opened a card", Some("t-77".to_string())),
        ("opened nothing", None),
    ] {
        runtime
            .events()
            .append(
                runtime.id(),
                crate::ports::types::CompanyEvent::AgentReply {
                    task_id,
                    chat_id: "General".to_string(),
                    agent_id: "maya".to_string(),
                    text: text.to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .unwrap();
    }

    let app = router(state);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ chat(id:\"general\"){ history(first: 10) { items { text taskId } } } } }"}"#,
    )
    .await;
    let items = value["data"]["company"]["chat"]["history"]["items"]
        .as_array()
        .unwrap();
    let opened = items
        .iter()
        .find(|m| m["text"] == "opened a card")
        .expect("the card-opening reply is in history");
    assert_eq!(opened["taskId"], "t-77");
    let plain = items
        .iter()
        .find(|m| m["text"] == "opened nothing")
        .expect("the ordinary reply is in history");
    assert!(
        plain["taskId"].is_null(),
        "an ordinary reply carries no card: {plain}"
    );
}

#[tokio::test]
async fn connections_reflect_manifest_intent_disconnected() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ connections { provider connected reason } } }"}"#,
    )
    .await;
    let conns = value["data"]["company"]["connections"].as_array().unwrap();
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0]["provider"], "slack");
    assert_eq!(conns[0]["connected"], false);
    assert_eq!(conns[0]["reason"], "Post updates.");
}

/// The two connection projections are one shape: whatever credential tier the
/// REST route reports for a provider, the GraphQL resolver must report the same
/// (issue #319). They share `connect_route_from_env`, and this pins that they
/// keep sharing it — a second copy of the resolution order is a second chance to
/// tell the console a hosted instance can run a local Connect.
#[tokio::test]
async fn rest_and_graphql_agree_on_the_connection_credential_source() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;

    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ connections { provider credentialSource } } }"}"#,
    )
    .await;
    let gql: Vec<(String, String)> = value["data"]["company"]["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .map(|row| {
            (
                row["provider"].as_str().unwrap().to_string(),
                row["credentialSource"]
                    .as_str()
                    .expect("every GraphQL row carries a credentialSource")
                    .to_string(),
            )
        })
        .collect();
    assert!(!gql.is_empty(), "expected at least one connection: {value}");

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/company/connections")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let rest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rest_rows: Vec<(String, String)> = rest
        .as_array()
        .expect("array")
        .iter()
        .map(|row| {
            (
                row["provider"].as_str().unwrap().to_string(),
                row["credentialSource"]
                    .as_str()
                    .expect("every REST row carries a credentialSource")
                    .to_string(),
            )
        })
        .collect();

    assert_eq!(
        rest_rows, gql,
        "REST and GraphQL disagree on the connection credential source"
    );
}

#[tokio::test]
async fn tasks_page_reflects_upserts_and_column_filter() {
    use crate::ports::tasks::TaskRecord;
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .tasks()
        .upsert(
            runtime.id(),
            &TaskRecord {
                id: "t1".into(),
                title: "Launch".into(),
                note: None,
                column: "todo".into(),
                priority: "high".into(),
                assignee: "maya".into(),
                updated_at_millis: 1_700_000_000_000,
                origin_chat_id: None,
                parent_task_id: None,
            },
        )
        .await
        .unwrap();
    let app = router(state);
    let value = query(
        app.clone(),
        r#"{"query":"{ company(id:\"acme\"){ tasks(column:\"todo\"){ total items { id title column } } } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["company"]["tasks"]["total"], 1);
    assert_eq!(value["data"]["company"]["tasks"]["items"][0]["id"], "t1");

    // A different column filters it out.
    let none = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ tasks(column:\"done\"){ total } } }"}"#,
    )
    .await;
    assert_eq!(none["data"]["company"]["tasks"]["total"], 0);
}

#[tokio::test]
async fn memory_page_reflects_upserts() {
    use crate::ports::facts::{FactKind, FactRecord};
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .facts()
        .upsert(
            runtime.id(),
            &FactRecord {
                id: "f1".into(),
                kind: FactKind::Preference,
                title: "Tone".into(),
                body: "Friendly.".into(),
                source: "general".into(),
                updated_at_millis: 1_700_000_000_000,
            },
        )
        .await
        .unwrap();
    let app = router(state);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ memory(kind: PREFERENCE){ total items { id kind title updatedAt } } } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["company"]["memory"]["total"], 1);
    assert_eq!(
        value["data"]["company"]["memory"]["items"][0]["kind"],
        "PREFERENCE"
    );
    assert!(
        value["data"]["company"]["memory"]["items"][0]["updatedAt"]
            .as_str()
            .unwrap()
            .starts_with("2023-")
    );
}

#[tokio::test]
async fn empty_surfaces_resolve_to_empty_lists() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ workspaceTree { id } inboxes { key } skills { id } workflows { id } } }"}"#,
    )
    .await;
    let company = &value["data"]["company"];
    assert_eq!(company["workspaceTree"].as_array().unwrap().len(), 0);
    assert_eq!(company["inboxes"].as_array().unwrap().len(), 0);
    assert_eq!(company["skills"].as_array().unwrap().len(), 0);
    assert_eq!(company["workflows"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn smtp_status_is_unconfigured_without_credentials() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ smtp { host port configured } domain { domain } } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["company"]["smtp"]["configured"], false);
    assert_eq!(value["data"]["company"]["smtp"]["host"], "");
    assert!(value["data"]["company"]["domain"].is_null());
}

#[tokio::test]
async fn usage_is_empty_without_samples() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ usage(range: D7){ totals { tokens costUsd connections } series { date } } } }"}"#,
    )
    .await;
    let usage = &value["data"]["company"]["usage"];
    assert_eq!(usage["totals"]["tokens"], 0.0);
    assert_eq!(usage["totals"]["connections"], 0);
    // D7 still yields a zero-filled 7-day series.
    assert_eq!(usage["series"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn usage_reflects_recorded_samples() {
    use crate::ports::usage::{SampleKind, UsageSample};
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let now = super::now_millis();
    runtime
        .usage()
        .record(
            runtime.id(),
            &UsageSample {
                at_millis: now,
                agent: "maya".into(),
                provider: "managed".into(),
                input_tokens: 100,
                output_tokens: 40,
                cached_input_tokens: 0,
                cost_usd: 0.5,
                kind: SampleKind::Inference,
                run_id: None,
            },
        )
        .await
        .unwrap();
    let app = router(state);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ usage(range: D30){ totals { inputTokens tokens costUsd } byAgent { name tokens } } } }"}"#,
    )
    .await;
    let usage = &value["data"]["company"]["usage"];
    assert_eq!(usage["totals"]["inputTokens"], 100.0);
    assert_eq!(usage["totals"]["tokens"], 140.0);
    assert_eq!(usage["totals"]["costUsd"], 0.5);
    assert_eq!(usage["byAgent"][0]["tokens"], 140.0);
}

#[tokio::test]
async fn finances_fold_the_ledger() {
    use crate::ports::types::LedgerEntry;
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let now = super::now_millis();
    runtime
        .store()
        .append_ledger(
            runtime.id(),
            LedgerEntry {
                at_millis: now,
                kind: "inference.spend".into(),
                amount_usd: -2.0,
                memo: "tokens".into(),
            },
        )
        .await
        .unwrap();
    runtime
        .store()
        .append_ledger(
            runtime.id(),
            LedgerEntry {
                at_millis: now,
                kind: "payment.received".into(),
                amount_usd: 10.0,
                memo: "invoice".into(),
            },
        )
        .await
        .unwrap();
    let app = router(state);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ finances { spentUsd revenueUsd netUsd transactions { id direction amountUsd } byCategory { category amount } } } }"}"#,
    )
    .await;
    let fin = &value["data"]["company"]["finances"];
    assert_eq!(fin["spentUsd"], 2.0);
    assert_eq!(fin["revenueUsd"], 10.0);
    assert_eq!(fin["netUsd"], 8.0);
    assert_eq!(fin["transactions"].as_array().unwrap().len(), 2);
}

/// On the serve path a company has an on-disk source dir; `Company.skills`,
/// `Company.workflow`, and the top-level `skillRegistry` resolve their content
/// from it (and the repo-level `skills/` root) rather than the empty bundle.
#[tokio::test]
async fn skills_and_workflows_resolve_from_source_dir() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // A company source directory with a committed skill and workflow.
    let source_dir = home.join("companies").join("acme");
    tokio::fs::create_dir_all(source_dir.join("skills/deal-memo"))
        .await
        .unwrap();
    tokio::fs::write(
        source_dir.join("skills/deal-memo/SKILL.md"),
        "---\nname: Deal Memo\ndescription: Write a deal memo.\ncategory: Research\n---\n# Deal Memo\n",
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(source_dir.join("workflows"))
        .await
        .unwrap();
    tokio::fs::write(
        source_dir.join("workflows/flow.toml"),
        "id = \"flow\"\nname = \"Test Flow\"\n[[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n",
    )
    .await
    .unwrap();

    // A separate repo-level shared skill library backing `skillRegistry`.
    let skills_root = home.join("skills");
    tokio::fs::create_dir_all(skills_root.join("web-research"))
        .await
        .unwrap();
    tokio::fs::write(
        skills_root.join("web-research/SKILL.md"),
        "---\nname: Web Research\ndescription: Research on the web.\ncategory: Research\n---\n# Web Research\n",
    )
    .await
    .unwrap();

    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[workflows]\nenabled = [\"flow\"]\n",
    )
    .unwrap();
    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .with_seed_dir(source_dir.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default())
        .with_home(home.to_path_buf())
        .with_skills_root(skills_root);
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    // Company.skills reads the committed source-dir skill.
    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ skills { id name source } workflow(id:\"flow\"){ id name nodes { id } } } skillRegistry { id name } }"}"#,
    )
    .await;
    let company = &value["data"]["company"];
    let skills = company["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1, "source-dir skill resolves");
    assert_eq!(skills[0]["id"], "deal-memo");
    assert_eq!(skills[0]["source"], "company");
    // Company.workflow reads the graph from the source dir.
    assert_eq!(company["workflow"]["name"], "Test Flow");
    assert_eq!(company["workflow"]["nodes"].as_array().unwrap().len(), 1);
    // skillRegistry reads the repo-level shared library.
    let registry = value["data"]["skillRegistry"].as_array().unwrap();
    assert!(registry.iter().any(|s| s["id"] == "web-research"));
}

/// Issue #239: `install` pins the library document into the delta, so a later
/// library edit must not rewrite an existing install. `Company.skills` has to
/// project that pinned snapshot rather than re-reading the live library —
/// otherwise GraphQL and REST report different `version`s for the same install,
/// and a slug that later leaves the library loses its persisted content.
#[tokio::test]
async fn company_skills_project_the_pinned_snapshot_of_a_registry_install() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // The shared library has moved on to a rewritten v2 of `web-research`, and
    // never had `retired-skill` at all.
    let skills_root = home.join("skills");
    tokio::fs::create_dir_all(skills_root.join("web-research"))
        .await
        .unwrap();
    tokio::fs::write(
        skills_root.join("web-research/SKILL.md"),
        "---\nname: Web Research v2\ndescription: Rewritten upstream.\ncategory: Ops\nversion: 2.0.0\n---\n# Web Research v2\n",
    )
    .await
    .unwrap();

    let store = FsCompanyStore::new(home.clone());
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(home.clone(), manifest())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();

    // Two registry installs, each holding the document pinned at install time.
    for (slug, doc) in [
        (
            "web-research",
            "---\nname: Web Research\ndescription: Research on the web.\ncategory: Research\nversion: 1.0.0\n---\n# Web Research\nStep one.\n",
        ),
        (
            "retired-skill",
            "---\nname: Retired Skill\ndescription: Withdrawn from the library.\ncategory: Ops\nversion: 1.0.0\n---\n# Retired Skill\nStill installed here.\n",
        ),
    ] {
        runtime
            .skills()
            .set(
                runtime.id(),
                &crate::ports::skills_state::SkillState {
                    slug: slug.to_string(),
                    enabled: true,
                    source: crate::ports::skills_state::SkillSource::Registry,
                    custom_doc: Some(doc.to_string()),
                },
            )
            .await
            .unwrap();
    }

    let state = AppState::new(AppConfig::default())
        .with_home(home.clone())
        .with_skills_root(skills_root);
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ skills { id name description category source version } } skillRegistry { id version } }"}"#,
    )
    .await;
    let skills = value["data"]["company"]["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 2, "both installs resolve: {value}");

    let pinned = skills
        .iter()
        .find(|s| s["id"] == "web-research")
        .expect("the installed library skill");
    assert_eq!(pinned["source"], "registry");
    assert_eq!(
        pinned["version"], "1.0.0",
        "the pinned revision survives a later library edit"
    );
    assert_eq!(pinned["name"], "Web Research");
    assert_eq!(pinned["category"], "Research");

    // A slug the library no longer serves keeps its real persisted content
    // instead of degrading to a titleized name and a blank description.
    let retired = skills
        .iter()
        .find(|s| s["id"] == "retired-skill")
        .expect("the install whose slug left the library");
    assert_eq!(retired["name"], "Retired Skill");
    assert_eq!(retired["description"], "Withdrawn from the library.");
    assert_eq!(retired["version"], "1.0.0");

    // The registry tab itself is *not* pinned — it browses the live library.
    let registry = value["data"]["skillRegistry"].as_array().unwrap();
    assert_eq!(registry.len(), 1);
    assert_eq!(registry[0]["id"], "web-research");
    assert_eq!(registry[0]["version"], "2.0.0");
}

/// Issue #168: a hosted tenant has no source directory, so its workflows live
/// only as runtime-authored bodies on the record. `Company.workflows` must
/// resolve their real display name (not the id fallback) and `Company.workflow`
/// must return the full graph.
#[tokio::test]
async fn workflows_resolve_from_the_record_overlay_with_no_source_dir() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[workflows]\nenabled = [\"hosted\"]\n",
    )
    .unwrap();
    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                id: "hosted".to_string(),
                toml: "id = \"hosted\"\nname = \"Hosted Flow\"\n\
                       [[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n\
                       [[node]]\nid = \"n2\"\nkind = \"output\"\nname = \"Done\"\n\
                       [[edge]]\nfrom = \"n1\"\nto = \"n2\"\n"
                    .to_string(),
            }],
            overlay_budgets: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();

    // Built WITHOUT `with_seed_dir` — the hosted shape.
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    assert!(
        runtime.source_dir().is_none(),
        "no source dir in hosted mode"
    );
    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state),
        r#"{"query":"{ company(id:\"acme\"){ workflows { id name enabled } workflow(id:\"hosted\"){ id name nodes { id } edges { from to } } } }"}"#,
    )
    .await;
    let company = &value["data"]["company"];
    let summaries = company["workflows"].as_array().expect("summaries");
    assert_eq!(summaries.len(), 1, "value: {value}");
    assert_eq!(summaries[0]["id"], "hosted");
    // The real name from the overlay body, not the id fallback.
    assert_eq!(summaries[0]["name"], "Hosted Flow");
    assert_eq!(company["workflow"]["name"], "Hosted Flow");
    assert_eq!(company["workflow"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(company["workflow"]["edges"].as_array().unwrap().len(), 1);
}

/// Issue #168: a runtime-authored workflow with an **empty** manifest
/// `[workflows].enabled` must still appear in `Company.workflows`. The resolver
/// used to drive its id set off the enabled list alone, so this returned `[]`
/// while `Company.workflow` happily returned the full graph.
///
/// This used to be the ordinary post-restart state on the fs backend, because a
/// boot rebuild overwrote the record's manifest from the seed. Issue #208 fixed
/// that — a rebuild now merges surviving overlay ids back into `enabled` — so
/// the state is written here *after* the build instead. The resolver's guarantee
/// is unchanged and still worth pinning: it enumerates graph bodies on their own
/// evidence, whatever put the record in this shape (a hand-edited record, a
/// store written by an older build, a future writer that adds a body first).
///
/// Also pins REST/GraphQL agreement: both surfaces must report the same id set.
#[tokio::test]
async fn workflows_summary_lists_an_overlay_workflow_with_no_enabled_entry() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");

    // Nothing enabled in the manifest — the graph body is the only evidence.
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();

    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest.clone())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    assert!(
        runtime.source_dir().is_none(),
        "no source dir in hosted mode"
    );

    // Write the enabled-less record AFTER the build. Since issue #208 a boot
    // rebuild merges surviving overlay ids back into `[workflows].enabled`, so
    // seeding this state before the build would be healed away — and this test
    // is about the *resolver*, which must enumerate overlay bodies on their own
    // evidence no matter how the record got into this shape.
    let store = FsCompanyStore::new(home.to_path_buf());
    store
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: vec![crate::ports::types::OverlayWorkflow {
                id: "orphan".to_string(),
                toml: "id = \"orphan\"\nname = \"Orphan Flow\"\n\
                       [[node]]\nid = \"n1\"\nkind = \"trigger\"\nname = \"Start\"\n"
                    .to_string(),
            }],
            overlay_budgets: Vec::new(),
            template_provenance: None,
        })
        .await
        .unwrap();

    let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;

    let value = query(
        router(state.clone()),
        r#"{"query":"{ company(id:\"acme\"){ workflows { id name enabled } } }"}"#,
    )
    .await;
    let summaries = value["data"]["company"]["workflows"]
        .as_array()
        .expect("summaries");
    assert_eq!(summaries.len(), 1, "value: {value}");
    assert_eq!(summaries[0]["id"], "orphan");
    assert_eq!(summaries[0]["name"], "Orphan Flow");
    // Honest flag: the graph exists and is runnable, but the manifest does not
    // declare it, so `enabled` reads false rather than being faked to true.
    assert_eq!(
        summaries[0]["enabled"], false,
        "`enabled` reports manifest membership, not existence"
    );

    // REST and GraphQL must report the same id set.
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/company/workflows")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let rest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rest_ids: Vec<&str> = rest
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    let gql_ids: Vec<&str> = summaries
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(rest_ids, gql_ids, "REST and GraphQL disagree on the id set");
}

/// The committed SDL snapshot freezes the read contract. Regenerate with
/// `cargo test -- --ignored regenerate_sdl_snapshot` after any schema change.
#[test]
fn sdl_snapshot_matches() {
    let expected = include_str!("schema.graphql");
    let actual = super::sdl();
    assert_eq!(
        actual, expected,
        "GraphQL SDL drifted from schema.graphql; regenerate with \
         `cargo test -- --ignored regenerate_sdl_snapshot`"
    );
}

#[test]
#[ignore = "writes the SDL snapshot; run explicitly after a schema change"]
fn regenerate_sdl_snapshot() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/graphql/schema.graphql");
    std::fs::write(&path, super::sdl()).unwrap();
}
