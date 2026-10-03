use super::*;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;
use super::operator_test_support_3::*;

/// Issue #371 also starts projecting the run id on the settle-frame — the
/// key that lets the console clear the right canvas when two runs overlap.
/// Still omitted for a pre-#371 row, so no permanently-null key appears.
#[test]
fn projects_the_run_id_on_a_finished_run_only_when_there_is_one() {
    let with_id = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
        workflow_id: "digest".into(),
        scheduled: false,
        run_id: Some("run-9".into()),
        deliveries: Vec::new(),
        pending_approvals: Vec::new(),
        error: None,
        cancelled: false,
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }))
    .expect("projected");
    assert_eq!(with_id["runId"], "run-9");

    let legacy = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
        workflow_id: "digest".into(),
        scheduled: false,
        run_id: None,
        deliveries: Vec::new(),
        pending_approvals: Vec::new(),
        error: None,
        cancelled: false,
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }))
    .expect("projected");
    assert!(legacy.get("runId").is_none(), "{legacy}");
}

#[test]
fn drops_non_attention_and_raw_payload_events() {
    // The operator's own message, and every variant that carries a raw
    // third-party payload or is audit-only, is dropped so nothing unexpected
    // (or secret-bearing) ever reaches the console.
    //
    // This list is unchanged by #228: adding `workflow_run_finished` to the
    // projection widened the wire by exactly one listed variant, and this
    // test passing untouched is what proves the deny-by-default default
    // still drops everything it dropped before.
    let dropped = [
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        },
        CompanyEvent::WebhookReceived {
            channel: "email".into(),
            body: serde_json::json!({"authorization": "Bearer sk-secret"}),
        },
        CompanyEvent::Unknown,
        CompanyEvent::ScheduleFired {
            cron: "0 9 * * *".into(),
            prompt: "daily standup".into(),
        },
        CompanyEvent::FeedbackFiled {
            note: "too slow".into(),
        },
        CompanyEvent::MemoryFactDeleted {
            fact_id: "f-1".into(),
        },
    ];
    for event in dropped {
        assert!(
            super::project_event(&stored(event.clone())).is_none(),
            "event should be dropped from the SSE feed: {event:?}"
        );
    }
}

#[tokio::test]
async fn events_route_streams_text_event_stream() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/events")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The SSE head is returned immediately; the body streams indefinitely, so
    // we assert the status + content-type without draining it.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
}

#[tokio::test]
async fn events_route_requires_a_session() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The composer's own typing pings must not echo back to it — the bus has
/// no per-listener addressing, so this filter is the only thing standing
/// between "you typed" and a fresh "Alice is typing…" line under your own
/// cursor.
#[test]
fn a_typing_frame_from_the_viewer_is_dropped_and_from_anybody_else_is_kept() {
    let mine = crate::turn_stream::LiveFrame::Typing(crate::turn_stream::TypingFrame {
        kind: "typing",
        user_id: "u1".into(),
        chat_id: "engineering".into(),
        parent_id: None,
        at_millis: 0,
    });
    assert!(super::is_own_typing_frame(&mine, Some("u1")));
    assert!(!super::is_own_typing_frame(&mine, Some("u2")));
    assert!(
        !super::is_own_typing_frame(&mine, None),
        "a machine credential with nobody behind it authors nothing to echo"
    );

    let presence = crate::turn_stream::LiveFrame::Presence(crate::turn_stream::PresenceFrame {
        kind: "presence",
        user_id: "u1".into(),
        status: "online",
        at_millis: 0,
    });
    assert!(
        !super::is_own_typing_frame(&presence, Some("u1")),
        "presence is left alone — only typing echoes"
    );
}

// -----------------------------------------------------------------------
// Standing permissions (issue #374)
// -----------------------------------------------------------------------

/// Every contradictory or unbounded scope request is a 400, and none of them
/// reaches the runtime.
///
/// The approval id is deliberately one that does not exist: each of these
/// must be refused at the edge, so the fact that resolving a missing
/// approval would otherwise be a harmless no-op never gets a chance to mask
/// a body that should not have been accepted.
///
/// A deny may now ride the tool scope (issue #1458 — a standing refusal),
/// so that pairing is asserted as *accepted* at the bottom rather than
/// listed among the refusals.
#[tokio::test]
async fn a_contradictory_or_unbounded_scope_is_refused() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;

    let day: u64 = 24 * 60 * 60 * 1000;
    for (label, body) in [
        (
            "an argument edit and a standing grant contradict",
            format!(
                r#"{{"verdict":"approve","scope":"tool","expires_in_millis":{day},"amended_payload":{{"to":"x"}}}}"#
            ),
        ),
        (
            "the deadline is mandatory",
            r#"{"verdict":"approve","scope":"tool"}"#.to_string(),
        ),
        (
            "zero is not a duration",
            r#"{"verdict":"approve","scope":"tool","expires_in_millis":0}"#.to_string(),
        ),
        (
            "past the seven-day cap is refused, never clamped",
            format!(
                r#"{{"verdict":"approve","scope":"tool","expires_in_millis":{}}}"#,
                MAX_STANDING_GRANT_MILLIS + 1
            ),
        ),
        (
            "a duration is meaningless on the once scope",
            format!(r#"{{"verdict":"approve","scope":"once","expires_in_millis":{day}}}"#),
        ),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/company/approvals/appr-missing")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{label}: must be refused at the edge"
        );
    }

    // An unrecognised scope is refused too, one layer earlier: `ResolveScope`
    // is a closed enum, so axum's JSON extractor rejects it as 422 before
    // any handler runs. The status differs from the checks above; what
    // matters is that it is never silently downgraded to `once`, which would
    // hand an operator a single call when they asked for a standing one.
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/approvals/appr-missing")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"verdict":"approve","scope":"forever"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Exactly at the cap is fine — the boundary is inclusive.
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/approvals/appr-missing")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"verdict":"approve","scope":"tool","expires_in_millis":{MAX_STANDING_GRANT_MILLIS}}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::BAD_REQUEST);

    // A deny riding the tool scope is no longer a contradiction: it mints a
    // standing refusal (issue #1458). Same edge validation as an approve —
    // duration mandatory, bounded, and the missing approval resolves as a
    // no-op — so it is accepted exactly where a matching approve would be.
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/approvals/appr-missing")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"verdict":"deny","scope":"tool","expires_in_millis":{day}}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::BAD_REQUEST);
}

/// The default body — no `scope` key at all — is accepted exactly as before.
#[tokio::test]
async fn an_omitted_scope_is_the_pre_374_request() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/approvals/appr-missing")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"verdict":"approve"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// The grants list is empty on a fresh company, and revoking something that
/// is not there is a 404 rather than a cheerful no-op.
#[tokio::test]
async fn the_grants_list_starts_empty_and_revoking_nothing_is_a_404() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;

    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/grants")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value.as_array().unwrap().len(), 0);

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/grants/nope")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A standing grant is listed with the authenticated user's id, is revocable,
/// and revoking is idempotent-by-404. Both scope forms answer.
#[tokio::test]
async fn a_standing_grant_is_listed_under_its_granter_and_can_be_revoked() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    runtime
        .grants
        .grant_standing(crate::runtime::grants::StandingGrant {
            id: crate::runtime::grants::GrantId::new("g1"),
            agent: "ops".into(),
            workflow: None,
            tool: "workspace_write".into(),
            verdict: Verdict::Approve,
            granted_by: Actor {
                kind: ActorKind::User,
                id: "user-7".into(),
            },
            approval_id: ApprovalId::new("appr-1"),
            at_millis: 1_000,
            expires_at_millis: crate::ports::now_millis() + 60 * 60 * 1000,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
            scope: None,
        });

    // Both addressing forms list it.
    for uri in ["/api/v1/company/grants", "/api/v1/companies/acme/grants"] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value[0]["id"], "g1", "{uri}");
        assert_eq!(value[0]["tool"], "workspace_write");
        assert_eq!(value[0]["agent"], "ops");
        assert_eq!(
            value[0]["granted_by"]["id"], "user-7",
            "the list names who actually granted it"
        );
        assert!(
            value[0].get("payload").is_none() && value[0].get("args").is_none(),
            "a standing grant has no arguments, so the list opens no redaction surface"
        );
    }

    // Revoke, then it is gone and a second revoke is a 404.
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/grants/g1")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(runtime.standing_grants().len(), 0);

    let response = router(state)
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/grants/g1")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// GRANT-012 (AUTH): `GET {scope}/grants` stays readable by any member —
/// the same consistency `GET {scope}/tools/grants` holds — but revoking one
/// is an admin action (issue #2169). A Member must see the list and be
/// refused the delete.
#[tokio::test]
async fn a_member_may_list_standing_grants_but_not_revoke_one() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .grants
        .grant_standing(crate::runtime::grants::StandingGrant {
            id: crate::runtime::grants::GrantId::new("g-member"),
            agent: "ops".into(),
            workflow: None,
            tool: "workspace_write".into(),
            verdict: Verdict::Approve,
            granted_by: Actor {
                kind: ActorKind::User,
                id: "user-7".into(),
            },
            approval_id: ApprovalId::new("appr-1"),
            at_millis: 1_000,
            expires_at_millis: crate::ports::now_millis() + 60 * 60 * 1000,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
            scope: None,
        });
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let member_cookie = crate::server::test_support::member_cookie("acme");
    let app = router(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/grants")
                .header("cookie", &member_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a member may read the standing-grants list"
    );
    let body = body_json(response).await;
    assert_eq!(body[0]["id"], "g-member");

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/company/grants/g-member")
                .header("cookie", &member_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "revoking a standing grant is an admin action, matching the tools/grants plane"
    );
}

/// GRANT-012 (FAIL): `revoke_standing` is a plain map removal with no
/// expiry check of its own — a grant past its deadline that nothing has
/// *swept* yet is still found and revoked normally (204), exactly as
/// `/extend` can still rescue a not-yet-swept approval. Only once
/// `sweep_standing` has actually removed it does revoke correctly answer
/// the "nothing to revoke" 404 the route's own doc promises — the same
/// distinction as an already-revoked id, never a 500.
#[tokio::test]
async fn revoking_a_grant_is_404_only_once_it_is_actually_swept() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let stale_grant = |id: &str| crate::runtime::grants::StandingGrant {
        id: crate::runtime::grants::GrantId::new(id),
        agent: "ops".into(),
        workflow: None,
        tool: "workspace_write".into(),
        verdict: Verdict::Approve,
        granted_by: Actor {
            kind: ActorKind::User,
            id: "user-7".into(),
        },
        approval_id: ApprovalId::new("appr-1"),
        at_millis: 1_000,
        // Already in the past either way; only sweeping tells the two apart.
        expires_at_millis: 1_001,
        origin_thread: None,
        origin_parent: None,
        origin_task: None,
        scope: None,
    };
    runtime.grants.grant_standing(stale_grant("g-unswept"));
    runtime.grants.grant_standing(stale_grant("g-swept"));

    let app = router(state.clone());
    let delete = |id: &'static str| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/company/grants/{id}"))
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // Past-deadline but not yet swept: still a normal, successful revoke.
    let response = delete("g-unswept").await;
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "an expired-but-unswept grant is still physically present, so revoking it is an \
         ordinary success — exactly as extend can still rescue an unswept approval"
    );

    // Now actually sweep the other one out from under the route.
    let swept = runtime.grants.sweep_standing(crate::ports::now_millis());
    assert_eq!(swept.len(), 1, "premise: the grant was in fact swept");

    let response = delete("g-swept").await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "once actually swept, revoke must report the same 'nothing to revoke' answer an \
         already-revoked id does"
    );
}

/// GRANT-012 (CONC). Two browsers — or one double-click — racing a
/// `DELETE` on the same grant id must not both report success:
/// `revoke_standing` is a plain `HashMap::remove`, so exactly one caller
/// takes the grant and every other must see the ordinary "already gone"
/// 404 a second revoke gets, not a duplicate 204 or a panic on a double
/// free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_revokes_of_the_same_grant_settle_once() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    runtime
        .grants
        .grant_standing(racing_standing_grant("g-race"));

    let app = router(state);
    let racers: Vec<_> = (0..8)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/api/v1/company/grants/g-race")
                        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            })
        })
        .collect();

    let mut statuses = Vec::new();
    for racer in racers {
        statuses.push(
            racer
                .await
                .expect("the request task did not panic")
                .status(),
        );
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::NO_CONTENT)
            .count(),
        1,
        "exactly one simultaneous revoke may take the grant, got {statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::NOT_FOUND)
            .count(),
        7,
        "every loser must see the ordinary already-gone 404, got {statuses:?}"
    );
    assert_eq!(runtime.grants.standing().len(), 0);
}
