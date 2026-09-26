use super::*;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;

/// Removing an overlay member drops it from the merged view; a manifest
/// member cannot be removed (409), and an unknown overlay member is a 404.
#[tokio::test]
async fn remove_desk_member_drops_overlay_and_guards_manifest() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // Seed an overlay member.
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/desks/studio/members")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"eng"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // Removing a manifest member is a 409.
    let manifest_remove = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/desks/studio/members/ceo")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manifest_remove.status(), StatusCode::CONFLICT);

    // Removing the overlay member succeeds and drops it from the list.
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

    let desks = get_desks(&app, &cookie).await;
    assert_eq!(desks[0]["members"].as_array().unwrap().len(), 1);
    assert!(desks[0].get("overlayMembers").is_none());

    // Removing it again is a 404 (no such overlay member).
    let gone = app
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
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

/// A desk that exists only in the operator overlay can be staffed and
/// unstaffed like a manifest desk. Both membership handlers used to test the
/// manifest alone, so a console-created desk could be reordered and deleted
/// but never gain or lose a member (#833). Every other desk test seeds its
/// desk from the manifest, so only an overlay-created desk exercises this.
#[tokio::test]
async fn desk_member_writes_reach_an_overlay_created_desk() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let desk =
        seed_overlay_desk(&app, &cookie, r#"{"name":"Growth desk","members":["ceo"]}"#).await;
    assert_eq!(desk, "growth_desk");
    assert_eq!(desk_members(&app, &cookie, &desk).await, ["ceo"]);

    let added = post_desk_member(&app, &cookie, &desk, r#"{"agent_id":"eng"}"#).await;
    assert_eq!(added.status(), StatusCode::NO_CONTENT);
    assert_eq!(desk_members(&app, &cookie, &desk).await, ["ceo", "eng"]);

    let removed = delete_desk_member(&app, &cookie, &desk, "eng").await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(desk_members(&app, &cookie, &desk).await, ["ceo"]);
}

/// An unknown desk id is refused as a missing desk, not a missing company.
/// The refusal used to be raised as `CompanyNotFound("desk ghost")`, which
/// rendered as `company not found: desk ghost` — the wrong resource, and the
/// desk id stuffed into a company id slot (#833). The status stays `404`
/// because both variants map there.
#[tokio::test]
async fn unknown_desk_member_writes_refuse_as_a_missing_desk() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let added = post_desk_member(&app, &cookie, "ghost", r#"{"agent_id":"eng"}"#).await;
    assert_eq!(added.status(), StatusCode::NOT_FOUND);
    let message = error_message(added).await;
    assert!(
        !message.contains("company not found"),
        "add refusal blames the company: {message:?}"
    );
    assert!(message.contains("ghost"), "add refusal drops the desk id");

    let removed = delete_desk_member(&app, &cookie, "ghost", "eng").await;
    assert_eq!(removed.status(), StatusCode::NOT_FOUND);
    let message = error_message(removed).await;
    assert!(
        !message.contains("company not found"),
        "remove refusal blames the company: {message:?}"
    );
    assert!(
        message.contains("ghost"),
        "remove refusal drops the desk id"
    );
}

/// Creating a desk persists it as an overlay and surfaces it in `list_desks`
/// alongside the manifest desks, flagged `overlayCreated` with its lead
/// first. The manifest is never rewritten.
#[tokio::test]
async fn create_desk_persists_and_appears_in_list() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/desks")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"Growth desk","description":"Acquisition.","members":["eng","ceo"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let bytes = to_bytes(created.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // Id derived from the name; the first member is the lead.
    assert_eq!(body["id"], "growth_desk");
    assert_eq!(body["name"], "Growth desk");
    assert_eq!(body["overlayCreated"], true);
    assert_eq!(body["members"][0], "eng");
    assert_eq!(body["members"][1], "ceo");

    // The list now carries the manifest desk and the created overlay desk.
    let desks = get_desks(&app, &cookie).await;
    let arr = desks.as_array().unwrap();
    assert_eq!(arr.len(), 2, "{arr:?}");
    assert_eq!(arr[0]["id"], "studio"); // manifest desk first
    assert_eq!(arr[1]["id"], "growth_desk");
    assert_eq!(arr[1]["overlayCreated"], true);
}

/// Issue #1835, both wire directions. A create that never mentions
/// `responder` — every existing caller, and the org chart today — answers
/// and lists with **no** `responder` key at all, so old consoles see the
/// pre-#1835 shape byte-for-byte. A create with `responder: "auto"`
/// answers and lists `"auto"`, and the mode survives the store round-trip
/// rather than collapsing back to a lead desk.
#[tokio::test]
async fn create_desk_carries_the_responder_mode_and_omits_the_default() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let post = |body: &'static str| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/company/desks")
                        .header("cookie", &cookie)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        }
    };

    let lead = post(r#"{"name":"Growth desk","members":["eng"]}"#).await;
    assert!(
        lead.get("responder").is_none(),
        "a mode never stated must not appear on the wire: {lead}"
    );
    let auto = post(r#"{"name":"Launch week","members":["eng","ceo"],"responder":"auto"}"#).await;
    assert_eq!(auto["responder"], "auto", "{auto}");

    // The list re-reads the store, so this is the round-trip half: the
    // manifest desk and the defaulted create stay keyless, the channel
    // keeps its mode.
    let desks = get_desks(&app, &cookie).await;
    let arr = desks.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert!(arr[0].get("responder").is_none(), "manifest desk: {desks}");
    assert!(
        arr[1].get("responder").is_none(),
        "defaulted create: {desks}"
    );
    assert_eq!(arr[2]["responder"], "auto", "{desks}");
}

/// Issue #1835, codex review: an `auto` channel cannot be created empty —
/// the selector would have no candidates and the first-member fallback no
/// first member, so its unmentioned messages would silently fall to the
/// orchestrator, contradicting the channel's own model. A **lead** desk
/// keeps its right to start empty and be staffed from the org chart.
/// Revert the guard in `create_desk` and the first assertion answers 201.
#[tokio::test]
async fn an_auto_channel_cannot_be_created_empty_but_a_lead_desk_still_can() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let post = |body: &'static str| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    let refused = post(r#"{"name":"Launch week","responder":"auto"}"#).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(refused.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        body.contains("at least one member"),
        "the refusal names the reason, not a generic 400: {body}"
    );

    let empty_lead = post(r#"{"name":"Someday desk"}"#).await;
    assert_eq!(
        empty_lead.status(),
        StatusCode::CREATED,
        "an empty lead desk is still legal — it gains members from the org chart"
    );
}

/// Create-desk validation: an empty name is 400, an id colliding with a
/// manifest desk is 409, an unknown member is 400, and — issue #1757 — an
/// id (explicit or name-derived) colliding with the reserved `operator`
/// system channel is 409 even though it is not a manifest or overlay desk
/// `desk_exists` would otherwise catch.
#[tokio::test]
async fn create_desk_validates_name_id_and_members() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let cases = [
        (r#"{"name":"   "}"#, StatusCode::BAD_REQUEST),
        (r#"{"name":"Studio","id":"studio"}"#, StatusCode::CONFLICT),
        (
            r#"{"name":"Ghost desk","members":["ghost"]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            r#"{"name":"Operator","id":"operator"}"#,
            StatusCode::CONFLICT,
        ),
        (r#"{"name":"operator"}"#, StatusCode::CONFLICT),
        // PR #1781 review (CodeRabbit P2 follow-up to `316bc9229`): the id
        // guard alone lets a display-name collision through — `{"id":
        // "ops", "name": "Operator"}` never touches the reserved id, but
        // `resolve_desk_id` would still fold a `?desk=Operator` selector
        // onto this desk exactly as it would onto one literally named
        // `operator`. Same shape for the collision-fallback display name.
        (r#"{"name":"Operator","id":"ops"}"#, StatusCode::CONFLICT),
        (
            r#"{"name":"operator-feed","id":"ops2"}"#,
            StatusCode::CONFLICT,
        ),
        // Issue #1743 / PR #1781 review: a desk claiming a General
        // spelling — by id or by display name — would shadow the
        // built-in `#general` channel exactly as an `operator`-id desk
        // shadows the Operator feed.
        (r#"{"name":"Ops","id":"general"}"#, StatusCode::CONFLICT),
        (r#"{"name":"Ops","id":"main"}"#, StatusCode::CONFLICT),
        (r#"{"name":"General"}"#, StatusCode::CONFLICT),
        (r#"{"name":"Main"}"#, StatusCode::CONFLICT),
    ];
    for (body, want) in cases {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/desks")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), want, "body {body}");
    }
}

/// Deleting an operator-created desk drops it (and any of its overlay
/// members); a manifest desk cannot be deleted (409); an unknown id is 404.
#[tokio::test]
async fn delete_desk_removes_overlay_and_guards_manifest() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // Create an overlay desk to delete.
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/desks")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Growth","members":["eng"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // A manifest desk cannot be deleted.
    let manifest_delete = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/desks/studio")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(manifest_delete.status(), StatusCode::CONFLICT);

    // The overlay desk deletes and drops out of the list.
    let delete = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/desks/growth")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);

    let desks = get_desks(&app, &cookie).await;
    // Only the manifest desk remains — the Operator feed is its own
    // surface now (issue #1757 rework), not injected into this list.
    let arr = desks.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{arr:?}");
    assert_eq!(arr[0]["id"], "studio");

    // Deleting it again is a 404.
    let gone = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/desks/growth")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

/// Issue #1781 review (Codex P2): deleting a legacy overlay desk that was
/// holding `operator_feed_channel()` on the fallback address must not let
/// it revert to `OPERATOR_CHANNEL`.
///
/// `desk_exists`/`resolve_desk_id` are live checks — with no tombstone,
/// removing the colliding desk makes them stop matching, so the divert
/// would silently flip back the moment `delete_desk` succeeds. Seeded
/// directly on the stored record rather than through `POST .../desks`
/// (as `list_desks_hides_an_overlay_desk_shadowing_general` does for its
/// own General case): `create_desk`'s own guard has refused the id and
/// name `operator` since `316bc9229`, so this shape can only be reached
/// by an overlay desk that predates it — exactly what this proves stays
/// safe to delete.
#[tokio::test]
async fn delete_desk_keeps_the_operator_feed_diverted_after_the_collision_is_gone() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();

    let mut record = runtime.store().load(&id).await.unwrap().unwrap();
    record.overlay_desks.push(OverlayDesk {
        id: "operator".to_string(),
        name: "Legacy Ops".to_string(),
        description: None,
        members: vec![],
        responder: ResponderMode::Lead,
        hive: Default::default(),
    });
    runtime.store().save(&record).await.unwrap();

    let reloaded = runtime.store().load(&id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.operator_feed_channel(),
        crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "fixture must start in the collision state this test exercises"
    );

    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");
    let delete = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/desks/operator")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);

    let after = runtime.store().load(&id).await.unwrap().unwrap();
    assert!(
        !after.desk_exists(crate::runtime::channel::OPERATOR_CHANNEL),
        "the colliding desk must actually be gone, or this is not \
         exercising the live-check-flips-back failure mode at all"
    );
    assert_eq!(
        after.operator_feed_channel(),
        crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
        "the feed address must stay on the fallback once the desk that \
         caused the collision is deleted — flipping back to \
         OPERATOR_CHANNEL would orphan every report already journaled \
         under the fallback and let the deleted desk's own historical \
         transcript (chat_id == \"operator\") resurface as system-feed \
         content"
    );
}

/// Add-member validation: an unknown desk is 404, an unknown teammate is
/// 400, and a teammate already on the desk is 409.
#[tokio::test]
async fn add_desk_member_validates_desk_agent_and_duplicates() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let cases = [
        (
            "/api/v1/company/desks/ghost/members",
            r#"{"agent_id":"eng"}"#,
            StatusCode::NOT_FOUND,
        ),
        (
            "/api/v1/company/desks/studio/members",
            r#"{"agent_id":"ghost"}"#,
            StatusCode::BAD_REQUEST,
        ),
        // `ceo` is already a manifest member of `studio`.
        (
            "/api/v1/company/desks/studio/members",
            r#"{"agent_id":"ceo"}"#,
            StatusCode::CONFLICT,
        ),
    ];
    for (uri, body, want) in cases {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), want, "{uri} {body}");
    }
}

/// A `PUT .../order` reorders the desk; the change surfaces in `list_desks`
/// as the new `members` order (the hierarchy), and an empty body resets it.
#[tokio::test]
async fn set_desk_order_reorders_and_resets() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");
    seed_overlay_eng(&app, &cookie).await;

    // Base order is manifest-first: ceo, then the overlay eng.
    let desks = get_desks(&app, &cookie).await;
    assert_eq!(desks[0]["members"][0], "ceo");
    assert_eq!(desks[0]["members"][1], "eng");

    // Promote the overlay member to the lead slot.
    let status = put_desk_order(
        &app,
        &cookie,
        "studio",
        r#"{"ordered_member_ids":["eng","ceo"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let desks = get_desks(&app, &cookie).await;
    assert_eq!(desks[0]["members"][0], "eng");
    assert_eq!(desks[0]["members"][1], "ceo");

    // An empty body clears the override, restoring the blueprint order.
    let status = put_desk_order(&app, &cookie, "studio", r#"{"ordered_member_ids":[]}"#).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let desks = get_desks(&app, &cookie).await;
    assert_eq!(desks[0]["members"][0], "ceo");
    assert_eq!(desks[0]["members"][1], "eng");
}

/// An operator-created (overlay) desk can be reordered too — the set-order
/// handler validates existence with `desk_exists`, which covers overlay desks,
/// not just manifest group chats. A manifest-only check used to 404 here (#133).
#[tokio::test]
async fn set_desk_order_reorders_an_overlay_created_desk() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // Create an overlay desk with two members (lead is `ceo` by declaration).
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/desks")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"Growth desk","members":["ceo","eng"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // Reordering the overlay desk succeeds (not 404) and promotes `eng`.
    let status = put_desk_order(
        &app,
        &cookie,
        "growth_desk",
        r#"{"ordered_member_ids":["eng","ceo"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The new hierarchy surfaces in the list for the overlay desk.
    let desks = get_desks(&app, &cookie).await;
    let growth = desks
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "growth_desk")
        .expect("overlay desk present");
    assert_eq!(growth["members"][0], "eng");
    assert_eq!(growth["members"][1], "ceo");
}

/// Set-order validation: an unknown desk is 404, an unknown member id is 400,
/// and a duplicate id is 400.
#[tokio::test]
async fn set_desk_order_validates_desk_members_and_duplicates() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");
    seed_overlay_eng(&app, &cookie).await;

    // Unknown desk → 404.
    assert_eq!(
        put_desk_order(&app, &cookie, "ghost", r#"{"ordered_member_ids":["ceo"]}"#).await,
        StatusCode::NOT_FOUND
    );
    // A non-member id → 400.
    assert_eq!(
        put_desk_order(
            &app,
            &cookie,
            "studio",
            r#"{"ordered_member_ids":["ceo","ghost"]}"#
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    // Duplicate id → 400.
    assert_eq!(
        put_desk_order(
            &app,
            &cookie,
            "studio",
            r#"{"ordered_member_ids":["ceo","ceo"]}"#
        )
        .await,
        StatusCode::BAD_REQUEST
    );
}
