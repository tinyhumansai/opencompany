use crate::ports::CompanyStore;
use crate::ports::types::CompanyId;
use crate::server::router;
use crate::store::FsCompanyStore;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::graphql_test_support_1::*;

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

/// The tier is readable, and it is the one the manifest declares — together
/// with the always-ask list, which is the half of the policy that actually
/// decides what parks.
///
/// `Company` already resolved `approvals` and `pendingApprovals` and not the
/// setting that decides whether anything ever enters that queue — so an empty
/// queue read the same under `supervised` (nothing pending) and `full` (nothing
/// will ever park). `full` **with** an always-ask list is a third reading
/// again, which is why the list is asserted here and not left to the override
/// case below: a resolver that answered `[]` unconditionally would report this
/// company as holding nothing back when it holds back two things.
#[tokio::test]
async fn policy_field_reports_the_tier_in_force() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_manifest(&home, policy_manifest()).await);
    let value = query(
        app,
        r#"{"query":"{ company(id: \"acme\") { policy { mode alwaysApprove manifestMode manifestAlwaysApprove overridden } } }"}"#,
    )
    .await;
    let policy = &value["data"]["company"]["policy"];
    assert_eq!(policy["mode"], "full", "{value}");
    assert_eq!(policy["manifestMode"], "full", "{value}");
    assert_eq!(policy["overridden"], false, "{value}");
    assert_eq!(
        policy["alwaysApprove"],
        serde_json::json!(["payment.send", "filing.submit"]),
        "with nothing overriding it the list in force is the manifest's, and it \
         is reported in the manifest's order: {value}"
    );
    assert_eq!(
        policy["manifestAlwaysApprove"],
        serde_json::json!(["payment.send", "filing.submit"]),
        "{value}"
    );
}

/// **The anti-drift assertion, and the reason this field maps `PolicyDto`
/// rather than reading the record itself.**
///
/// With a console override in force, `mode` and `manifestMode` diverge — and a
/// resolver that recomputed the tier from `manifest.policy.mode` would answer
/// `full` here while `GET {scope}/policy` answered `readonly`, with no way for
/// a caller to know which surface it had reached. That is the
/// two-sources-of-truth defect issue #1027 nearly shipped by putting this value
/// on `/capabilities` instead.
///
/// `overridden` is asserted separately from the two words on purpose: an
/// override that happened to match the manifest would still be an override, and
/// comparing `mode` with `manifestMode` cannot see one.
#[tokio::test]
async fn policy_field_reports_the_override_not_the_manifest() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let store = FsCompanyStore::new(home.clone());
    let state = state_with_manifest(&home, policy_manifest()).await;

    // Tighten the tier the way `PUT {scope}/policy` does — an overlay, never a
    // manifest edit. Both fields are overridden, and the list is deliberately
    // *not* the manifest's: two equal lists cannot show which one each field
    // was read from.
    let mut record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
    record.overlay_policy = Some(crate::ports::types::PolicyOverride {
        mode: Some("readonly".to_string()),
        always_approve: Some(vec!["deploy.production".to_string()]),
        auto_approve_under_usd: Some(Some(4.0)),
        approval_ttl_hours: Some(48),
        set_by: crate::ports::types::Actor {
            kind: crate::ports::types::ActorKind::Operator,
            id: "ada@example.com".to_string(),
        },
        at_millis: 1_700_000_000_000,
    });
    store.save(&record).await.unwrap();

    let value = query(
        router(state),
        r#"{"query":"{ company(id: \"acme\") { policy { mode alwaysApprove autoApproveUnderUsd approvalTtlHours manifestMode manifestAlwaysApprove manifestAutoApproveUnderUsd manifestApprovalTtlHours overridden setBy setAtMillis } } }"}"#,
    )
    .await;
    let policy = &value["data"]["company"]["policy"];
    assert_eq!(
        policy["mode"], "readonly",
        "the tier IN FORCE is the override, not the manifest: {value}"
    );
    assert_eq!(
        policy["manifestMode"], "full",
        "and the manifest's tier is still reported, so a client can see what a reset restores: {value}"
    );
    assert_eq!(
        policy["alwaysApprove"],
        serde_json::json!(["deploy.production"]),
        "the list IN FORCE is the override's too — and it is the lever that wins \
         over every tier, `full` included, so reading it from the manifest here \
         would report a company as holding back two things it does not: {value}"
    );
    assert_eq!(
        policy["manifestAlwaysApprove"],
        serde_json::json!(["payment.send", "filing.submit"]),
        "while the manifest's list is what a reset restores. The two fields \
         disagree on purpose: equal lists could not tell them apart, nor tell \
         either from the other being returned twice: {value}"
    );
    assert_eq!(policy["overridden"], true, "{value}");
    assert_eq!(policy["autoApproveUnderUsd"], 4.0, "{value}");
    assert_eq!(policy["approvalTtlHours"], 48.0, "{value}");
    assert!(policy["manifestAutoApproveUnderUsd"].is_null(), "{value}");
    assert!(policy["manifestApprovalTtlHours"].is_null(), "{value}");
    assert_eq!(policy["setBy"], "ada@example.com", "{value}");
    assert_eq!(
        policy["setAtMillis"], 1_700_000_000_000_f64,
        "epoch millis survive the f64 widening exactly: {value}"
    );
}

/// The two fields a client renders a policy *control* from: the tiers it may
/// offer, and what it has to tell an operator about when a change bites.
///
/// Neither varies with the company, which is why they are asserted once here
/// rather than in both cases above. `tiers` is not the text table — it is that
/// table narrowed to `POLICY_MODES` and ordered by it, so the console offers
/// exactly what this runtime accepts; offering a tier the gate would silently
/// downgrade is the failure it exists to prevent. `takesEffect` is the single
/// constant `GET`, `PUT` and `DELETE` all answer with, so an operator cannot be
/// quoted two different timings by two surfaces — the same one-derivation rule
/// the rest of this field follows.
#[tokio::test]
async fn policy_field_reports_the_selectable_tiers_and_when_a_change_takes_effect() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_manifest(&home, policy_manifest()).await);
    let value = query(
        app,
        r#"{"query":"{ company(id: \"acme\") { policy { tiers { value label description } takesEffect } } }"}"#,
    )
    .await;
    let policy = &value["data"]["company"]["policy"];

    let tiers = policy["tiers"].as_array().expect("tiers");
    let offered: Vec<&str> = tiers
        .iter()
        .map(|tier| tier["value"].as_str().expect("tier value"))
        .collect();
    assert_eq!(
        offered,
        crate::company::POLICY_MODES.to_vec(),
        "the tiers offered are the ones the runtime accepts, in its order: {value}"
    );
    for tier in tiers {
        assert!(
            !tier["label"].as_str().unwrap_or_default().is_empty()
                && !tier["description"].as_str().unwrap_or_default().is_empty(),
            "tier `{}` came back without operator-facing text, which is the whole \
             point of sending tiers rather than mode names: {value}",
            tier["value"]
        );
    }

    // One tier pinned exactly. `value`, `label` and `description` are three
    // adjacent `String`s: a mapping that swapped them would leave every
    // assertion above green while showing an operator a tier name where the
    // consequences should be.
    let readonly = tiers
        .iter()
        .find(|tier| tier["value"] == "readonly")
        .expect("the readonly tier");
    assert_eq!(readonly["label"], "Read-only", "{value}");
    assert_eq!(
        readonly["description"],
        "The agents can look at things but change nothing, contact nobody, and use no connected account. Billed tool calls are refused too — but the agents still think, and the company is billed for that.",
        "{value}"
    );

    assert_eq!(
        policy["takesEffect"],
        crate::server::ops::policy::TAKES_EFFECT,
        "asserted against the constant itself, not a copy of its wording: the \
         property is that GraphQL quotes the *same* timing REST does, and a \
         copy here would let the two drift apart while staying green: {value}"
    );
    assert!(
        !policy["takesEffect"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "and it is actually said, rather than the field existing and being blank: {value}"
    );
}

/// **Auth parity with REST.** `GET {scope}/policy` answers 401 to an
/// unauthenticated caller; this must not be the softer way in.
///
/// Tested rather than assumed. The gate lives in `graphql_handler` *before*
/// `schema().execute()`, so no resolver runs at all — but "the framework covers
/// it" is exactly the reasoning that ships a hole, and nothing in this suite
/// exercised the refusal before now. The same query minus the session cookie
/// every other test sends must return the error and no policy data.
#[tokio::test]
async fn policy_field_is_refused_without_a_session() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let app = router(state_with_company(&home).await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                // Deliberately no `cookie` header — the one difference from
                // `query()`.
                .body(Body::from(
                    r#"{"query":"{ company(id: \"acme\") { policy { mode } } }"}"#.to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value["data"]["company"].is_null(),
        "an unauthenticated caller must get no policy data: {value}"
    );
    assert_eq!(
        value["errors"][0]["message"], "unauthorized",
        "and must be told why, the same as the REST 401: {value}"
    );
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
    // The global baseline is appended to every roster; this test is about the
    // company's own teammate, so it reads the row rather than the whole list.
    let team = value["data"]["company"]["team"].as_array().unwrap();
    let maya = team
        .iter()
        .find(|row| row["id"] == "maya")
        .expect("maya is on the roster");
    assert_eq!(maya["role"], "Marketing Lead");
    assert!(maya["name"].is_null());
    for global in crate::globals::agents() {
        assert!(
            team.iter().any(|row| row["id"] == global.id.as_str()),
            "the baseline teammate `{}` is missing from the roster",
            global.id
        );
    }
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
        provider: None,
        id: "jamie".to_string(),
        name: "Jamie".to_string(),
        role: "Growth".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
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
        provider: None,
        id: "zeroed".to_string(),
        name: "Zeroed".to_string(),
        role: "Growth".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "uncapped".to_string(),
        name: "Uncapped".to_string(),
        role: "Ops".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
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
                audience: Vec::new(),
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "General".to_string(),
                agent_id: "maya".to_string(),
                text: "canonical id".to_string(),
                steps: Vec::new(),
                episode: None,
            },
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            runtime.id(),
            crate::ports::types::CompanyEvent::AgentReply {
                audience: Vec::new(),
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "main".to_string(),
                agent_id: "maya".to_string(),
                text: "console default-thread id".to_string(),
                steps: Vec::new(),
                episode: None,
            },
        )
        .await
        .unwrap();

    let app = router(state);
    let value = query(
        app,
        r#"{"query":"{ company(id:\"acme\"){ chat(id:\"general\"){ history(first: 1) { total items { text } } } } }"}"#,
    )
    .await;
    let texts: Vec<&str> = value["data"]["company"]["chat"]["history"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        vec!["console default-thread id"],
        "the page must contain only the newest matching message"
    );
    assert_eq!(
        value["data"]["company"]["chat"]["history"]["total"], 2,
        "the GraphQL page keeps its unpaginated total without making the REST reader scan it"
    );
}

/// A GraphQL caller controls `first`, but a history page must have the same
/// hard ceiling as REST so a huge integer cannot become a huge Vec reservation.
#[tokio::test]
async fn chat_history_clamps_an_oversized_page_request() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_rich_company(&home).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    for i in 0..201 {
        runtime
            .events()
            .append(
                runtime.id(),
                crate::ports::types::CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    outputs: Vec::new(),
                    chat_id: "General".to_string(),
                    agent_id: "maya".to_string(),
                    text: format!("message {i}"),
                    steps: Vec::new(),
                    episode: None,
                },
            )
            .await
            .unwrap();
    }

    let value = query(
        router(state),
        r#"{"query":"{ company(id:\"acme\"){ chat(id:\"general\"){ history(first: 2147483647) { total items { text } } } } }"}"#,
    )
    .await;
    let items = value["data"]["company"]["chat"]["history"]["items"]
        .as_array()
        .unwrap();
    assert_eq!(items.len(), 200, "the requested page is capped");
    assert_eq!(items[0]["text"], "message 1");
    assert_eq!(items[199]["text"], "message 200");
    assert_eq!(value["data"]["company"]["chat"]["history"]["total"], 201);
}
