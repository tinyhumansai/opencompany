//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use super::write_test_support::*;
use crate::ports::tasks::{TaskRecord, TaskTitle};
use crate::ports::types::CompanyId;
use crate::server::router;

/// #348 review: the thread is served on a screen that polls every 4s, so it
/// comes back as a **page** — the newest slice — with the rest reachable behind
/// a cursor. Without the cap, one busy card re-sends its whole history fifteen
/// times a minute per open browser, forever.
///
/// Asserted as a reader experiences it: the newest messages are the ones on the
/// first read, the response admits there are older ones, and passing the oldest
/// held `seq` back walks to the page before it without dropping or repeating a
/// message. The cursor's page is the *end* of the thread, which is what makes
/// `discussionHasMore` false there.
#[tokio::test]
async fn task_discussion_is_paged_newest_first_and_walks_back_with_a_cursor() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    // A thread longer than one page. Journaled directly: this test is about the
    // read's shape, and the write path is pinned by the tests above.
    const POSTS: usize = 62;
    for n in 0..POSTS {
        runtime
            .events()
            .append(
                &company,
                CompanyEvent::TaskDiscussionPosted {
                    task_id: "t-1".into(),
                    text: format!("message {n}"),
                    by: None,
                },
            )
            .await
            .unwrap();
    }

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    let page = body["discussion"].as_array().unwrap();
    assert!(
        page.len() < POSTS,
        "an unbounded thread came back whole: {} posts",
        page.len()
    );
    assert_eq!(
        body["discussionHasMore"], true,
        "a truncated thread that does not say so reads as the whole conversation"
    );
    // The tail, not the head: what somebody opening the card needs first.
    assert_eq!(
        page.last().unwrap()["text"],
        format!("message {}", POSTS - 1)
    );
    let first_seq = page[0]["seq"].as_u64().unwrap();
    let oldest_on_page = page[0]["text"].as_str().unwrap().to_string();

    // Walk back: the page *before* the oldest message held.
    let (status, older) = send(
        &state,
        "GET",
        &format!("/api/v1/company/tasks/t-1?discussionBefore={first_seq}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let older_page = older["discussion"].as_array().unwrap();
    assert_eq!(
        older_page.len(),
        POSTS - page.len(),
        "the cursor page plus the first page must be the whole thread"
    );
    assert_eq!(
        older["discussionHasMore"], false,
        "nothing precedes the start of the thread"
    );
    assert_eq!(older_page[0]["text"], "message 0");
    assert!(
        older_page
            .iter()
            .all(|m| m["seq"].as_u64().unwrap() < first_seq),
        "the cursor is exclusive — a message must not be served twice: {older_page:?}"
    );
    assert!(
        !older_page
            .iter()
            .any(|m| m["text"].as_str() == Some(oldest_on_page.as_str())),
        "the cursor message repeated on its own older page"
    );

    // A short thread is not paged at all — the flag stays honest downward.
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-2", "Quiet"))
        .await
        .unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t-2".into(),
                text: "just the one".into(),
                by: None,
            },
        )
        .await
        .unwrap();
    let (_, quiet) = send(&state, "GET", "/api/v1/company/tasks/t-2", None).await;
    assert_eq!(quiet["discussion"].as_array().unwrap().len(), 1);
    assert_eq!(quiet["discussionHasMore"], false);
}

/// #348 review: every post in the tests above is the harness admin, which
/// exercises one of `into_message`'s three branches. The other two are the ones
/// that matter for what reaches a reader's screen:
///
/// * a **departed** user — off the roster, so there is no name to resolve —
///   must read as `someone`, never as the raw user id the journal holds;
/// * a **machine credential** — the platform scope, which names no person —
///   must read as `operator`, and must journal no actor at all.
///
/// The machine half goes through the real write path with a tenant token, so it
/// pins `ScopedCompany`'s "keep the person, drop the credential" rule too: a
/// platform post that started attributing itself to *something* would show up
/// here as a label that is not `operator`.
#[tokio::test]
async fn task_discussion_names_a_departed_user_someone_and_a_machine_credential_operator() {
    use crate::ports::types::{Actor, ActorKind, CompanyEvent};
    use crate::server::platform_auth::{
        PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
    };
    use std::collections::HashSet;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new("plat-secret"));
    let state = state_with_company(&home)
        .await
        .with_platform_auth(PlatformAuthConfig::new(verifier));
    let company = CompanyId::new("acme");
    state.set_owner(company.clone(), "tenant:acme".to_string());
    let runtime = state.registry().get(&company).unwrap();
    runtime
        .tasks()
        .upsert(&company, &discussion_card("t-1", "Ship it"))
        .await
        .unwrap();

    // A user who has since left: journaled with an id the roster can no longer
    // resolve. Only the journal can hold this state, so the fixture is written
    // there rather than posted.
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t-1".into(),
                text: "I looked at this before I left".into(),
                by: Some(Actor {
                    kind: ActorKind::User,
                    id: "u-departed".into(),
                }),
            },
        )
        .await
        .unwrap();

    let token = UnsignedTenantVerifier::tenant_token(&PlatformClaims {
        tenant: "tenant:acme".to_string(),
        scopes: HashSet::from(["operator".to_string()]),
        companies: None,
    });
    let (status, posted) = send_auth(
        &state,
        "POST",
        "/api/v1/companies/acme/tasks/t-1/discussion",
        Some(json!({ "text": "posted by the platform" })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(posted["author"], "operator");

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let thread = body["discussion"].as_array().unwrap();
    let authors: Vec<&str> = thread
        .iter()
        .map(|m| m["author"].as_str().unwrap())
        .collect();
    assert_eq!(authors, vec!["someone", "operator"]);
    // The id the journal holds must not reach a reader — a thread is read by
    // every member of the company.
    let wire = serde_json::to_string(&body["discussion"]).unwrap();
    assert!(
        !wire.contains("u-departed"),
        "a user id reached the wire: {wire}"
    );
}

/// #352: `GET …/tasks/{id}/export` answers a downloadable HTML document, built
/// from the same read the console consumes, and changes nothing.
///
/// The last clause is an acceptance criterion in its own right and the one thing
/// the renderer's own tests cannot see: a document that quietly journalled an
/// "exported" event, or touched the card's column or `updatedAt`, would make an
/// audit export a modification of the thing being audited. So the board row and
/// the journal length are both compared across the call.
#[tokio::test]
async fn task_export_serves_a_readable_document_and_alters_nothing() {
    use crate::ports::types::{CompanyEvent, EventSeq};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "t-1".into(),
                title: TaskTitle::authored("Launch post"),
                note: Some("Write the launch post.".into()),
                column: "in_review".into(),
                priority: "high".into(),
                assignee: "writer".into(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();
    for event in [
        CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            // `None` is the honest value here, not a placeholder: this fixture
            // journals a dispatch directly rather than going through the choke
            // point that mints a run row (#242), and the export renders the
            // timeline, which does not read `run_id`.
            run_id: None,
            origin_chat_id: None,
            origin_parent: None,
        },
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "t-1".into(),
            agent_id: "writer".into(),
            text: "First draft is up.".into(),
            steps: Vec::new(),
            task_id: Some("t-1".into()),
            outputs: Vec::new(),
        },
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let before_board = runtime.tasks().list(&company).await.unwrap();
    let before_events = runtime
        .events()
        .read_from(&company, EventSeq::new(0), usize::MAX)
        .await
        .unwrap()
        .len();

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/company/tasks/t-1/export")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let disposition = response
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(content_type, "text/html; charset=utf-8");
    assert_eq!(
        disposition, "attachment; filename=\"task-launch-post.html\"",
        "the export must download as a named file"
    );

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(bytes.to_vec()).expect("the document is utf-8");
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("Launch post"));
    assert!(html.contains("<dd>Working — In review</dd>"));
    assert!(html.contains("First draft is up."));

    let after_board = runtime.tasks().list(&company).await.unwrap();
    assert_eq!(after_board, before_board, "exporting altered the board");
    let after_events = runtime
        .events()
        .read_from(&company, EventSeq::new(0), usize::MAX)
        .await
        .unwrap()
        .len();
    assert_eq!(after_events, before_events, "exporting journalled an event");

    let (status, _) = send(&state, "GET", "/api/v1/company/tasks/nope/export", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #185 review follow-up: pin the two timeline branches the first test skipped —
/// `tool_failed`, and the window-correlated `approval` arm.
///
/// The approval arm is the only branch in `fold_task_journal` whose correlation is
/// heuristic (parked effects carry no task id, so it is scoped by the run
/// window). That makes it the one most likely to regress into leaking another
/// run's resolution, so it is asserted from both sides: a resolution *before*
/// the dispatch anchor must be excluded, one *inside* the window admitted.
#[tokio::test]
async fn task_timeline_scopes_approvals_to_the_run_window() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "t-1".into(),
                title: TaskTitle::authored("Ship it"),
                note: None,
                column: "in_review".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
                updated_at_millis: 1,
                origin: None,
                parent_task_id: None,
                output: None,
                plan: None,
                planning_attempts: Vec::new(),
                deliverable: crate::ports::tasks::TaskDeliverable::Once,
                workflow_proposal: None,
                origin_run_id: None,
                origin_workflow_id: None,
                origin_message_seq: None,
                bounced: None,
            },
        )
        .await
        .unwrap();

    let approval = |id: &str| CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new(id),
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::User,
            id: "u-1".into(),
        },
    };

    for event in [
        // Before the dispatch anchor — belongs to some other run, must not leak.
        approval("before"),
        CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
            origin_chat_id: None,
            origin_parent: None,
        },
        // Inside the window — admitted.
        approval("during"),
        CompanyEvent::McpCallFailed {
            task_id: Some("t-1".into()),
            server: "gh".into(),
            tool: "issues".into(),
            status: "credential_required".into(),
            message: "needs auth".into(),
        },
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".into(),
            desk: "ceo".into(),
            output: "shipped".into(),
            column: "in_review".into(),
            artifact_ids: Vec::new(),
            origin_chat_id: None,
            origin_parent: None,
        },
        // After the window closed — must not leak either.
        approval("after"),
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let kinds: Vec<&str> = body["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["dispatched", "approval", "tool_failed", "completed"],
        "exactly one approval — the one inside the run window"
    );

    // The failure carries its scrubbed message; the operator's identity on the
    // approval is dropped, matching the SSE projection's deny-by-default stance.
    let raw = serde_json::to_string(&body["timeline"]).unwrap();
    assert!(raw.contains("needs auth"));
    assert!(!raw.contains("u-1"), "operator identity leaked: {raw}");
}

// ── Issue #305: working time vs waiting-on-a-human time ──────────────────────
//
// The split is a *read-time join*: the park instant lives only in the runtime
// journal (`ApprovalParked`), the resolution only in the event log
// (`ApprovalResolved`), and `approval_id` is the single key shared by both.
// These tests pin that join, its window clamp, and the two ways a wait can end
// (an operator decided, or the TTL swept it) — plus the negative case, which is
// an acceptance criterion in its own right: a task that never waited must
// report no waiting figure at all rather than a zero.
