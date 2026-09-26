use super::*;
use crate::company::CompanyManifest;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;

/// Removing an overlay member prunes it from the desk's order overlay, so the
/// remaining members keep the operator's relative order without a stale id.
#[tokio::test]
async fn remove_desk_member_prunes_the_order_entry() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");
    seed_overlay_eng(&app, &cookie).await;

    // Reorder to [eng, ceo], then remove eng.
    assert_eq!(
        put_desk_order(
            &app,
            &cookie,
            "studio",
            r#"{"ordered_member_ids":["eng","ceo"]}"#
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let remove = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/desks/studio/members/eng")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(remove.status(), StatusCode::NO_CONTENT);

    // Only the manifest member remains; the order entry is gone (no stale
    // eng lingering), so ceo is the lead.
    let desks = get_desks(&app, &cookie).await;
    assert_eq!(desks[0]["members"].as_array().unwrap().len(), 1);
    assert_eq!(desks[0]["members"][0], "ceo");
}

#[tokio::test]
async fn desks_route_returns_the_company_desks() {
    // The default test manifest defines no group chats, so the route
    // answers 200 with an empty list — the console falls back to its
    // static default threads.
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/desks")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let desks = value.as_array().unwrap();
    assert!(desks.is_empty(), "{desks:?}");
}

/// The Operator feed's identity route is gone, and `list_desks` carries
/// only real desks.
#[tokio::test]
async fn the_operator_feed_route_is_gone() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/operator-channel")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let desks = get_desks(&app, &cookie).await;
    assert!(
        desks
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["id"] != "operator"),
        "list_desks must carry zero operator logic: {desks:?}"
    );
}

/// `list_desks` never names the legacy Operator feed, and posting to its
/// chat id is still refused so its archived reports stay a read-only log.
#[tokio::test]
async fn the_legacy_operator_feed_stays_read_only() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let desks = get_desks(&app, &cookie).await;
    let desks = desks.as_array().unwrap();
    let ids: Vec<&str> = desks.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["studio"], "list_desks carries only real desks");

    // A send addressed to it is refused (read-only), never journaled.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hi","chat":"operator"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_client_error(),
        "posting to the operator channel must be refused, got {}",
        response.status()
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_lowercase();
    assert!(body.contains("read-only"), "{body}");
}

/// Issue #1781 review (CodeRabbit): `CompanyRuntime::ensure_desk_writable`
/// re-loads the record on every operator-channel send (to catch a
/// grandfathered desk/teammate colliding with the reserved id) and
/// propagates a real `store().load` failure with `?` rather than folding
/// it into "no real recipient". Collapsing it would misreport a store
/// outage as the ordinary read-only refusal — same 4xx, same message,
/// same "read-only" wording an operator would wrongly believe.
///
/// Corrupting `company.toml` on disk after the app is built (rather than
/// mocking `CompanyStore`) exercises the real `FsCompanyStore::load`
/// error path — `Err(OpenCompanyError::Store("invalid company.toml: …"))`
/// — which has no `Store` arm in `ApiError::status` and therefore falls
/// to the catch-all `INTERNAL_SERVER_ERROR`. A collapsed-to-`false` read
/// would instead surface as `InvalidRequest` (400) with the read-only
/// wording, so the status code and body together distinguish the two.
#[tokio::test]
async fn a_failing_store_load_is_not_collapsed_into_the_read_only_refusal() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // Corrupt the on-disk manifest so the next `store().load()` — the one
    // `ensure_desk_writable` runs fresh on every send — fails instead of
    // returning `Some(record)`.
    let toml_path = crate::store::Bundle::new(&home, &CompanyId::new("acme")).company_toml();
    tokio::fs::write(&toml_path, b"not valid toml [[[")
        .await
        .expect("corrupt company.toml");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hi","chat":"operator"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a store load failure must propagate as itself, not the read-only 4xx"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_lowercase();
    assert!(
        !body.contains("read-only"),
        "a store outage must not be misreported as the ordinary read-only refusal: {body}"
    );
}

/// Issue #1757 migration: `operator` was not a reserved id before this
/// issue, and a stored manifest is never re-validated on load
/// (`CompanyManifest::from_stored_toml` skips validation on purpose, so
/// tightening a rule never strands an already-running company) — so a
/// company provisioned earlier can already have a real `[[group_chat]]`
/// using that id. Built directly with `toml::from_str` (bypassing
/// `into_validated`, the same way a stored manifest reaches
/// `CompanyRuntime` without going through it) to stand in for exactly
/// that: data that predates the guard. Without the carve-outs in
/// `list_desks` and `chat_and_emit`, this desk would be shadowed by a
/// synthetic, read-only duplicate under the same id the moment this
/// feature shipped, and every send to it would be refused. This proves
/// it is grandfathered instead: listed once, not flagged `system`, and
/// still writable.
#[tokio::test]
async fn a_manifest_desk_predating_the_reserved_operator_id_stays_writable() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let legacy_manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"operator\"\nname = \"Ops Room\"\nmembers = [\"ceo\"]\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, legacy_manifest).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let desks = get_desks(&app, &cookie).await;
    let desks = desks.as_array().unwrap();
    assert_eq!(desks.len(), 1, "no duplicate synthetic entry: {desks:?}");
    assert_eq!(desks[0]["id"], "operator");
    assert_eq!(
        desks[0]["name"], "Ops Room",
        "the real desk's own name, not the synthetic channel's: {desks:?}"
    );
    assert!(
        desks[0].get("system").is_none(),
        "grandfathered desk is a real desk (system defaults false and is \
         omitted), not the system channel: {desks:?}"
    );

    // A send addressed to it must go through — this is the pre-existing
    // desk's own line, not the (absent) synthetic system channel.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"text":"ship the landing page","chat":"operator"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "a pre-existing desk that already owns the `operator` id must stay \
         writable, got {}",
        response.status()
    );
}

/// The name-collision sibling of the id-collision test above (issue #1781
/// review, Codex P1 follow-up): a manifest desk grandfathered onto the
/// **display name** `Operator` (`{ id = "legacy_ops", name = "Operator" }`)
/// rather than the literal id. `resolve_desk_id` — what every *read*
/// already resolves a `?desk=` selector through — matches this desk by
/// name just as thoroughly as the id-collision desk above is matched by
/// id, but `ensure_desk_writable` used to check the *raw* selector string
/// against `OPERATOR_CHANNEL` before any such resolution ran, so a send
/// addressed to the desk's own supported alias (`chat: "Operator"`,
/// case-insensitive) was refused as the read-only system feed — reachable
/// by name for reads, refused by name for writes, the exact mismatch
/// `create_desk`'s reservation comment (above) warns a desk can never be
/// addressed consistently under. A send addressed to the desk's real id
/// (`legacy_ops`) already sailed through either way, which this also
/// covers as the negative control.
#[tokio::test]
async fn a_manifest_desk_grandfathered_onto_the_operator_name_stays_writable() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let legacy_manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"legacy_ops\"\nname = \"Operator\"\nmembers = [\"ceo\"]\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, legacy_manifest).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // The desk's own real id still works — this was never broken.
    let by_id = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"by id","chat":"legacy_ops"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        by_id.status().is_success(),
        "a send addressed to the grandfathered desk's real id must stay writable, got {}",
        by_id.status()
    );

    // The desk's supported display-name alias must now work too.
    let by_name = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"by name","chat":"Operator"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        by_name.status().is_success(),
        "a send addressed to the grandfathered desk's own case-insensitive \
         `Operator` alias must resolve to the real desk, not the read-only \
         system feed, got {}",
        by_name.status()
    );
}

/// The fallback-address sibling of the test above (issue #1781 review,
/// Codex P2 follow-up): a manifest desk grandfathered onto the display
/// name `operator-feed` — `OPERATOR_CHANNEL_COLLISION_FALLBACK` itself —
/// rather than `Operator`. No desk or teammate here claims the *primary*
/// `operator` id or name, so `operator_feed_channel()` stays on the
/// literal address and never diverts; the fallback is purely this desk's
/// own pre-#1757 display name. `ensure_desk_writable` used to refuse the
/// fallback constant unconditionally, without resolving it through
/// `resolve_desk_id` first the way the primary branch does — so a send
/// addressed to this desk's own supported case-insensitive alias
/// (`chat: "operator-feed"`) was refused as if it named the synthetic
/// read-only system desk, even though nothing here is actually diverted.
/// A send to the desk's real id (`ops`) already sailed through either
/// way, which this also covers as the negative control.
#[tokio::test]
async fn a_manifest_desk_grandfathered_onto_the_fallback_name_stays_writable() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let legacy_manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"ops\"\nname = \"operator-feed\"\nmembers = [\"ceo\"]\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, legacy_manifest).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let record = runtime.store().load(&id).await.unwrap().unwrap();
    assert_eq!(
        record.operator_feed_channel(),
        crate::runtime::channel::OPERATOR_CHANNEL,
        "fixture must NOT be in the diverted state — this proves the \
         fallback name is refused even with no primary collision at all, \
         which the diverted case above does not exercise"
    );
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // The desk's own real id still works — this was never broken.
    let by_id = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"by id","chat":"ops"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        by_id.status().is_success(),
        "a send addressed to the grandfathered desk's real id must stay writable, got {}",
        by_id.status()
    );

    // The desk's supported display-name alias must now work too.
    let by_name = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"by name","chat":"operator-feed"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        by_name.status().is_success(),
        "a send addressed to the grandfathered desk's own case-insensitive \
         `operator-feed` alias must resolve to the real desk, not the \
         read-only system feed, got {}",
        by_name.status()
    );
}

/// Issue #1757 migration, the other namespace: a **teammate**, not a desk,
/// already named `operator`. `ChatView` addresses a DM by the teammate's
/// bare id (issue #364), so a message meant for this person also arrives
/// here as `chat == "operator"` — the same shape as a send meant for the
/// system feed. `desk_exists` alone cannot tell them apart: it only walks
/// `group_chats` and `overlay_desks`, never the roster, so a company that
/// named a manifest agent "Operator" before this feature shipped would
/// find that teammate's DM permanently refused, with the console giving no
/// way to rename or migrate out of the collision (`RESERVED_AGENT_IDS` and
/// `mint_agent_id` only stop a *future* mint). `is_roster_agent` closes the
/// same gap `desk_exists` closes for desks.
#[tokio::test]
async fn a_manifest_agent_predating_the_reserved_operator_id_stays_dm_able() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let legacy_manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, legacy_manifest).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // A DM addressed to the grandfathered teammate — by its bare id, the
    // same address `ChatView` sends — must go through rather than be
    // refused as a send to the read-only system channel.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"text":"status update please","chat":"operator"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "a pre-existing teammate that already owns the `operator` id must \
         stay DM-able, got {}",
        response.status()
    );
}

/// A company whose roster names a teammate `operator` keeps the legacy
/// feed's collision-fallback address refused as read-only.
#[tokio::test]
async fn a_grandfathered_operator_teammates_fallback_address_stays_read_only() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let legacy_manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, legacy_manifest).await;
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"text":"hello","chat":"{}"}}"#,
                    crate::runtime::OPERATOR_CHANNEL_COLLISION_FALLBACK
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the disjoint system-feed address must stay read-only"
    );
}

/// A grandfathered desk at the literal `operator` id keeps its own id.
#[tokio::test]
async fn a_grandfathered_operator_desk_keeps_its_own_id() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let legacy_manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"operator\"\nname = \"Ops Room\"\nmembers = [\"ceo\"]\n",
    )
    .unwrap();
    let state = state_with_manifest(&home, legacy_manifest).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let desks = get_desks(&app, &cookie).await;
    let desks = desks.as_array().unwrap();
    assert_eq!(desks.len(), 1);
    assert_eq!(
        desks[0]["id"], "operator",
        "the desk itself must keep its own literal id: {desks:?}"
    );
}
