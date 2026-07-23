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

pub(crate) fn home() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("opencompany-gql-{}", crate::ports::generate_id()))
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
    let home = home();
    let app = router(state_with_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ companies { id name lifecycle pendingApprovals } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["companies"][0]["id"], "acme");
    assert_eq!(value["data"]["companies"][0]["name"], "Acme");
    assert_eq!(value["data"]["companies"][0]["lifecycle"], "running");
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn company_query_by_id_resolves() {
    let home = home();
    let app = router(state_with_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id: \"acme\") { id pendingApprovals } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["company"]["id"], "acme");
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn company_query_without_id_resolves_the_sole_company() {
    let home = home();
    let app = router(state_with_company(&home).await);
    let value = query(app, r#"{"query":"{ company { id } }"}"#).await;
    assert_eq!(value["data"]["company"]["id"], "acme");
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn unknown_company_query_is_null() {
    let home = home();
    let app = router(state_with_company(&home).await);
    let value = query(app, r#"{"query":"{ company(id: \"ghost\") { id } }"}"#).await;
    assert!(value["data"]["company"].is_null());
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn approvals_field_is_empty_before_any_park() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
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
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn chats_list_the_manifest_desks() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn connections_reflect_manifest_intent_disconnected() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn tasks_page_reflects_upserts_and_column_filter() {
    use crate::ports::tasks::TaskRecord;
    let home = home();
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
                column: "backlog".into(),
                priority: "high".into(),
                assignee: "maya".into(),
                updated_at_millis: 1_700_000_000_000,
            },
        )
        .await
        .unwrap();
    let app = router(state);
    let value = query(
        app.clone(),
        r#"{"query":"{ company(id:\"acme\"){ tasks(column:\"backlog\"){ total items { id title column } } } }"}"#,
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn memory_page_reflects_upserts() {
    use crate::ports::facts::{FactKind, FactRecord};
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn empty_surfaces_resolve_to_empty_lists() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn smtp_status_is_unconfigured_without_credentials() {
    let home = home();
    let app = router(state_with_rich_company(&home).await);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ smtp { host port configured } domain { domain } } }"}"#,
    )
    .await;
    assert_eq!(value["data"]["company"]["smtp"]["configured"], false);
    assert_eq!(value["data"]["company"]["smtp"]["host"], "");
    assert!(value["data"]["company"]["domain"].is_null());
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn usage_is_empty_without_samples() {
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn usage_reflects_recorded_samples() {
    use crate::ports::usage::{SampleKind, UsageSample};
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

#[tokio::test]
async fn finances_fold_the_ledger() {
    use crate::ports::types::LedgerEntry;
    let home = home();
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
    tokio::fs::remove_dir_all(&home).await.ok();
}

/// On the serve path a company has an on-disk source dir; `Company.skills`,
/// `Company.workflow`, and the top-level `skillRegistry` resolve their content
/// from it (and the repo-level `skills/` root) rather than the empty bundle.
#[tokio::test]
async fn skills_and_workflows_resolve_from_source_dir() {
    let home = home();
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

    tokio::fs::remove_dir_all(&home).await.ok();
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
