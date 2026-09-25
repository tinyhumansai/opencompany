use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::usage::{SampleKind, UsageSample};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oc-team-")
        .tempdir()
        .expect("tempdir")
}

async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
    state_with(home, toml::from_str(manifest_toml).unwrap()).await
}

async fn state_with(home: &std::path::Path, manifest: CompanyManifest) -> AppState {
    let store = FsCompanyStore::new(home.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
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
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
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

async fn get_team(state: &AppState) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/company/team")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
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

/// Two teammates on one manifest: `analyst` is capped, `writer` is not.
const ROSTER: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
     [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\nbudget_usd_daily = 5.0\n\
     [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n";

/// Drives any team route with an explicit cookie, so the auth boundary can
/// be exercised with an admin session, a member session, or none at all.
async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let request = match &body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(value).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
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

fn admin_cookie() -> String {
    crate::server::test_support::fixed_cookie("acme")
}

/// `PUT …/team/{id}/budget` as the seeded admin.
async fn put_budget(state: &AppState, agent: &str, body: Value) -> (StatusCode, Value) {
    send(
        state,
        "PUT",
        &format!("/api/v1/company/team/{agent}/budget"),
        Some(body),
        Some(&admin_cookie()),
    )
    .await
}

/// One roster row from `GET …/team`.
async fn team_row(state: &AppState, agent: &str) -> Value {
    let (status, body) = get_team(state).await;
    assert_eq!(status, StatusCode::OK);
    body.as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == agent)
        .unwrap_or_else(|| panic!("no {agent} row in {body}"))
        .clone()
}

// --- Console budget writes (issue #343) ---------------------------------

/// Issue #788/#789: naming a BYO billing namespace for a NEW teammate is a
/// billing decision, and a member must not be able to make it. A budget
/// that spends money is already admin-only; an explicit `chargebee` grant
/// without a budget must be too — otherwise any member could mint a
/// billing-capable teammate the day the company ceiling includes one, while
/// editing an existing teammate's `tools` is already admin-only.
#[tokio::test]
async fn a_member_may_not_create_a_teammate_with_a_billing_grant() {
    use crate::ports::UserRole;
    let home_dir = home();
    let state = state_with_manifest(
        home_dir.path(),
        "[company]\nname = \"Acme\"\n[tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\", \"chargebee\"]\n",
    )
    .await;
    let member = crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Jamie", "role": "Billing", "tools": ["chargebee"]})),
        Some(&member),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The refused add must not have persisted a teammate.
    let (list_status, team) = get_team(&state).await;
    assert_eq!(list_status, StatusCode::OK, "{team}");
    let rows = team.as_array().unwrap();
    assert!(
        rows.iter().all(|r| r["name"] != "Jamie"),
        "a refused add must not persist a teammate: {team}"
    );

    // An admin can still mint the billing-capable teammate.
    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana", "role": "Billing", "tools": ["chargebee"]})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["id"], "dana", "{created}");
}

/// An **overlay** teammate can be capped after the fact too — the case the
/// pre-#343 read path hardcoded to `None` ("uncapped in v1").
///
/// Capping it also has to start the spend read for the whole roster, which
/// the old `any_capped` scan (manifest agents only) would have missed on a
/// company whose only capped teammate came from the console.
#[tokio::test]
async fn an_overlay_teammate_can_be_capped_and_reports_its_spend() {
    let home_dir = home();
    let state = state_with_manifest(
        home_dir.path(),
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n",
    )
    .await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Jamie", "role": "Growth"})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let jamie = created["id"].as_str().unwrap().to_string();

    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    runtime
        .usage()
        .record(
            &id,
            &UsageSample {
                at_millis: crate::ports::now_millis(),
                agent: jamie.clone(),
                provider: "managed".into(),
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
                cost_usd: 0.75,
                kind: SampleKind::Inference,
                run_id: None,
                model: None,
            },
        )
        .await
        .unwrap();

    let (status, _) = put_budget(&state, &jamie, json!({"budgetUsdDaily": 2.0})).await;
    assert_eq!(status, StatusCode::OK);

    let row = team_row(&state, &jamie).await;
    assert_eq!(row["budgetUsdDaily"], 2.0, "{row}");
    assert!(
        (row["spentTodayUsd"].as_f64().unwrap() - 0.75).abs() < 1e-9,
        "capping the only console-added teammate must start the meter read \
         for the roster: {row}"
    );
}

/// A budget write answers with the teammate as it **effectively** stands,
/// not as the blueprint declared it.
///
/// `updated_row` is a second place the roster is rendered, and it used to
/// read the raw manifest row. Once a manifest teammate became editable that
/// made one card change identity depending on which route last touched it:
/// a console rename showed on the Team page and then vanished the moment an
/// admin set a cap, because the budget response overwrote the row with the
/// name and role from `company.toml`.
#[tokio::test]
async fn a_budget_write_answers_with_the_edited_identity() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    // Rename a blueprint teammate through the console.
    let (status, _) = send(
        &state,
        "PATCH",
        "/api/v1/company/team/analyst",
        Some(json!({"role": "Managing Director", "description": "Runs the place."})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Then set a cap on the same teammate. The response is a roster row.
    let (status, row) = put_budget(&state, "analyst", json!({"budgetUsdDaily": 4.0})).await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(
        row["role"], "Managing Director",
        "the budget write answered with the blueprint's role, undoing the rename on the \
         card the console re-renders from: {row}"
    );
    assert_eq!(row["description"], "Runs the place.", "{row}");

    // And clearing the cap answers the same way — same helper, same defect.
    let (status, cleared) = send(
        &state,
        "DELETE",
        "/api/v1/company/team/analyst/budget",
        None,
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["role"], "Managing Director", "{cleared}");
}

/// Removing a teammate takes its override with it, so the record does not
/// accumulate rows for teammates that no longer exist.
#[tokio::test]
async fn removing_a_teammate_drops_its_budget_override() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (_, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Jamie", "role": "Growth", "budgetUsdDaily": 2.0})),
        Some(&admin_cookie()),
    )
    .await;
    let jamie = created["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/team/{jamie}"),
        None,
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let record = state
        .registry()
        .get(&CompanyId::new("acme"))
        .unwrap()
        .store()
        .load(&CompanyId::new("acme"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        record.overlay_budgets.is_empty(),
        "the removed teammate's override went with it: {:?}",
        record.overlay_budgets
    );
}

/// Issue #686 — a console-added teammate gets a readable snake_case id
/// derived from its name, so its workspace folder reads
/// `agents/dana_designer/` rather than `agents/019fad5ada20-…/`.
///
/// A second teammate with the same name suffixes rather than being refused:
/// duplicate display names were always accepted here, and taking that away
/// would be a capability regression dressed as a bug fix.
#[tokio::test]
async fn a_console_added_teammate_gets_a_readable_id() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let add = async |name: &str| {
        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": name, "role": "Designer"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        created["id"].as_str().unwrap().to_string()
    };

    assert_eq!(add("Dana Designer").await, "dana_designer");
    assert_eq!(add("Dana Designer").await, "dana_designer_2");
    // A name colliding with a manifest agent's id steps past it — an
    // unsuffixed `writer` would be dropped by `build_roster` and the
    // teammate would save without ever materialising.
    assert_eq!(add("Writer").await, "writer_2");
    // A name with no legal stem in it takes the shared fallback.
    assert_eq!(add("24/7").await, "teammate");
}

/// The slug is a seat name, not a chain of custody: removing a teammate
/// frees its id, and re-adding the same name takes it back — which is what
/// makes remove-plus-re-add the remedy for a typo'd name, since the new
/// teammate adopts the old `Agents/<slug>/` folder.
///
/// Pinned rather than left implicit because it is the one consequence of
/// name-derived ids that a generated id did not have.
#[tokio::test]
async fn removing_a_teammate_frees_its_slug_for_reuse() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (_, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana Designer", "role": "Designer"})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(created["id"], "dana_designer");

    let (status, _) = send(
        &state,
        "DELETE",
        "/api/v1/company/team/dana_designer",
        None,
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, again) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana Designer", "role": "Designer"})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(
        again["id"], "dana_designer",
        "the freed slug comes back rather than suffixing past a ghost: {again}"
    );
}

/// Issue #1781 review, Codex P2 follow-up: the sibling of the desk-side
/// fix — a legacy **overlay** teammate at the literal id `operator`
/// (grandfathered; `POST .../team` reserves this id going forward, the
/// same way `create_desk` reserves it for desks) diverts
/// `operator_feed_channel()` to the fallback address via `is_roster_agent`.
/// Unlike a manifest teammate, `remove_member`'s overlay branch deletes
/// outright with no `retire_agent` tombstone — so without this fix the
/// live `is_roster_agent`/`is_retired` checks would both go false the
/// moment the delete lands, reverting the feed to `OPERATOR_CHANNEL` and
/// orphaning every report already journaled under the fallback.
///
/// Seeded directly on the stored record rather than through `POST
/// .../team`, for the same reason the desk-side test seeds its collision
/// directly: the creation route has refused this id since before this fix
/// existed, so this shape can only be reached by data that predates it.
#[tokio::test]
async fn removing_an_overlay_teammate_at_the_operator_id_keeps_the_feed_diverted() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();

    let mut record = runtime.store().load(&id).await.unwrap().unwrap();
    record
        .overlay_agents
        .push(crate::ports::types::OverlayAgent {
            provider: None,
            id: "operator".to_string(),
            name: "Legacy Operator".to_string(),
            role: "Chief of Staff".to_string(),
            description: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        });
    runtime.store().save(&record).await.unwrap();

    let reloaded = runtime.store().load(&id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.operator_feed_channel(),
        crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "fixture must start in the collision state this test exercises"
    );

    let (status, _) = send(
        &state,
        "DELETE",
        "/api/v1/company/team/operator",
        None,
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let after = runtime.store().load(&id).await.unwrap().unwrap();
    assert!(
        !after.is_roster_agent(crate::runtime::channel::OPERATOR_CHANNEL),
        "the colliding teammate must actually be gone, or this is not \
         exercising the live-check-flips-back failure mode at all"
    );
    assert_eq!(
        after.operator_feed_channel(),
        crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "the feed address must stay on the fallback once the overlay \
         teammate that caused the collision is deleted — flipping back to \
         OPERATOR_CHANNEL would orphan every report already journaled \
         under the fallback and let the deleted teammate's own historical \
         DM history (chat_id == \"operator\") resurface as system-feed \
         content"
    );
}

/// The id is minted once. `PATCH …/team/{id}` renames the teammate and
/// leaves the id alone — a name-keyed id would orphan the teammate's
/// workspace folder, its budget row and its desk memberships on every
/// correction (the trap name-keyed DM ids sprang in issue #364).
#[tokio::test]
async fn renaming_a_teammate_does_not_remint_its_id() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (_, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Dana Designer", "role": "Designer"})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(created["id"], "dana_designer");

    let (status, edited) = send(
        &state,
        "PATCH",
        "/api/v1/company/team/dana_designer",
        Some(json!({"name": "Dana Diaz"})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["name"], "Dana Diaz");
    assert_eq!(
        edited["id"], "dana_designer",
        "the id records what the teammate was called at creation: {edited}"
    );

    // And the per-teammate routes still answer on the original slug.
    let (status, _) = put_budget(&state, "dana_designer", json!({"budgetUsdDaily": 3.0})).await;
    assert_eq!(status, StatusCode::OK);
    let row = team_row(&state, "dana_designer").await;
    assert_eq!(row["budgetUsdDaily"], 3.0, "{row}");
    assert_eq!(row["name"], "Dana Diaz", "{row}");
}

/// Every teammate route keyed on `{agent_id}` keeps working when that id is
/// a slug — the inbox toggle alongside the budget pair, since the slug now
/// travels in a URL path where a generated id used to.
#[tokio::test]
async fn slug_ids_work_across_the_per_teammate_routes() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let (_, created) = send(
        &state,
        "POST",
        "/api/v1/company/team",
        Some(json!({"name": "Ana Maria (Growth)", "role": "Growth"})),
        Some(&admin_cookie()),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(id, "ana_maria_growth");

    let (status, _) = send(
        &state,
        "PUT",
        &format!("/api/v1/company/team/{id}/inbox"),
        Some(json!({"enabled": true})),
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(team_row(&state, &id).await["inboxEnabled"], true);

    let (status, _) = put_budget(&state, &id, json!({"budgetUsdDaily": 1.5})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &state,
        "DELETE",
        &format!("/api/v1/company/team/{id}/budget"),
        None,
        Some(&admin_cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        team_row(&state, &id).await.get("budgetUsdDaily").is_none(),
        "the reset came back through the slug-keyed route"
    );
}

/// Issue #304 — the cap was never on the wire at all, so the issue's "and
/// displayed in the console" was stale against main. A capped teammate now
/// carries both its cap and its spend since UTC midnight, summed from the
/// meter; an uncapped one carries neither key.
///
/// The omission is the contract, not an optimisation: the console tells
/// "spends freely" from "capped and has spent nothing" by presence alone.
#[tokio::test]
async fn a_capped_teammate_carries_its_cap_and_todays_spend() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let now = crate::ports::now_millis();
    for cost in [1.25f64, 0.50] {
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: now,
                    agent: "analyst".into(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: cost,
                    kind: SampleKind::Inference,
                    run_id: None,
                    model: None,
                },
            )
            .await
            .unwrap();
    }
    // The uncapped teammate's spend must not leak onto the capped one.
    runtime
        .usage()
        .record(
            &id,
            &UsageSample {
                at_millis: now,
                agent: "writer".into(),
                provider: "managed".into(),
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
                cost_usd: 9.00,
                kind: SampleKind::Inference,
                run_id: None,
                model: None,
            },
        )
        .await
        .unwrap();

    let (status, body) = get_team(&state).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap();

    let analyst = rows.iter().find(|m| m["id"] == "analyst").unwrap();
    assert_eq!(analyst["budgetUsdDaily"], 5.0, "{analyst}");
    assert!(
        (analyst["spentTodayUsd"].as_f64().unwrap() - 1.75).abs() < 1e-9,
        "spend is summed per agent since UTC midnight: {analyst}"
    );

    let writer = rows.iter().find(|m| m["id"] == "writer").unwrap();
    assert!(
        writer.get("budgetUsdDaily").is_none() && writer.get("spentTodayUsd").is_none(),
        "an uncapped teammate omits both keys: {writer}"
    );
}

/// Yesterday's spend is not today's: the read is anchored at 00:00 UTC, the
/// same boundary the harness gate and the policy arm enforce against.
#[tokio::test]
async fn spend_today_excludes_yesterday() {
    let home_dir = home();
    let state = state_with_manifest(home_dir.path(), ROSTER).await;

    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let yesterday =
        crate::metering::utc_day_start_millis(crate::ports::now_millis()).saturating_sub(1);
    runtime
        .usage()
        .record(
            &id,
            &UsageSample {
                at_millis: yesterday,
                agent: "analyst".into(),
                provider: "managed".into(),
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
                cost_usd: 9.00,
                kind: SampleKind::Inference,
                run_id: None,
                model: None,
            },
        )
        .await
        .unwrap();

    let (status, body) = get_team(&state).await;
    assert_eq!(status, StatusCode::OK);
    let analyst = body
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "analyst")
        .unwrap()
        .clone();
    assert_eq!(analyst["budgetUsdDaily"], 5.0, "{analyst}");
    assert_eq!(
        analyst["spentTodayUsd"], 0.0,
        "a capped teammate with no spend today reads $0, not yesterday's $9: {analyst}"
    );
}
