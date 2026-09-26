use super::*;
use crate::AppConfig;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;
use super::operator_test_support_3::*;

/// `detach: true` answers `202` with the ids the accept already established,
/// claims nothing about a turn that has not settled, and — the half that
/// removes the 504 from the operator's path — arrives while the turn is
/// demonstrably still going (issue #983).
#[tokio::test]
async fn a_detached_turn_answers_202_before_the_turn_finishes() {
    let home_dir = home();
    // The same blocking brain the queue tests use: it parks inside the cycle
    // until released, so the turn is provably unfinished when the response
    // below is read.
    let (brain, entered, release) = BlockingChatBrain::new();
    let state = build_state_with_brain(
        home_dir.path(),
        "running",
        AppConfig::default(),
        Some(brain),
    )
    .await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let app = router(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"do the long thing","detach":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(body["detached"], true, "{body}");
    assert!(
        body["turnId"].as_str().is_some_and(|s| !s.is_empty()),
        "the turn id is what the console polls: {body}"
    );
    assert!(
        body["messageId"].as_str().is_some_and(|s| !s.is_empty()),
        "the message is journaled at accept, so its id is knowable here: {body}"
    );
    // The whole point: this body is not allowed to look settled. A console
    // that found `responses` here would render an empty answer as the reply.
    assert!(
        body.get("responses").is_none(),
        "a detached response must not look settled: {body}"
    );
    assert!(
        body.get("stillAwaiting").is_none(),
        "a detached response must not look settled: {body}"
    );

    // And the turn really had not finished when that body was written — the
    // brain is still parked, holding the cycle open.
    entered.acquire().await.expect("the turn entered").forget();
    let statuses: Vec<String> = turn_rows(&runtime)
        .await
        .into_iter()
        .map(|(_, status)| status)
        .collect();
    assert!(
        statuses.iter().any(|s| s == "running" || s == "pending"),
        "the response beat the turn, which is the point: {statuses:?}"
    );

    // It settles on its own, with nobody waiting on it.
    release.add_permits(1);
    until("the detached turn never settled", async || {
        turn_rows(&runtime)
            .await
            .iter()
            .any(|(_, status)| status == "succeeded")
    })
    .await;
}

/// The wire-compat guarantee in the other direction: a caller that sends no
/// `detach` gets exactly the response it always got — a `200` carrying the
/// settled turn — plus the additive `turnId`. An older console is untouched.
#[tokio::test]
async fn a_body_without_detach_still_gets_the_synchronous_response() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    // `post_chat` asserts the 200 itself — the legacy status is part of what
    // this test is pinning.
    let body = post_chat(&app, &cookie, r#"{"text":"hi"}"#).await;

    assert!(
        body["responses"].as_array().is_some_and(|r| !r.is_empty()),
        "the settled shape carries the replies: {body}"
    );
    assert!(
        body["messageId"].as_str().is_some(),
        "the legacy durable id is unchanged: {body}"
    );
    assert!(
        body.get("detached").is_none(),
        "the synchronous response must not carry the detach discriminator: {body}"
    );
    assert!(
        body["turnId"].as_str().is_some(),
        "`turnId` is additive on the synchronous response too: {body}"
    );
}

/// The detached turn is not fire-and-forget: the message it journaled at
/// accept, and the answer the spawned task journals afterwards, both land in
/// the durable transcript. This is the backstop the console re-reads, and
/// the reason a dropped frame is not a lost answer.
#[tokio::test]
async fn a_detached_turn_still_journals_its_question_and_its_answer() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let dm = default_dm(&state).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"detached hello","detach":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mine = body["messageId"].as_str().unwrap().to_string();

    // The turn owns its own settle, so wait for the answer to appear rather
    // than for a handle this route deliberately does not hold.
    let mut history = Vec::new();
    for _ in 0..100 {
        history = get_history(&app, &cookie, &dm).await;
        if history
            .iter()
            .any(|m| m["text"].as_str() == Some("You said: detached hello"))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let ids: Vec<&str> = history.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&mine.as_str()),
        "the id handed back at 202 must resolve in history: {ids:?}"
    );
    assert!(
        history
            .iter()
            .any(|m| m["text"].as_str() == Some("You said: detached hello")),
        "the detached turn's answer never reached the transcript: {history:?}"
    );
}

/// **Issue #1000 — the floor of the `202` contract.** A detached response
/// is a promise the console can poll, and the poll starts from the body's
/// `turnId`; a `202` carrying no row is a promise a buffered-`/events`
/// tenant cannot collect, which strands the reply until reload. So when the
/// turn's row cannot be minted, the route must not answer `202` at all: it
/// settles the turn synchronously instead, handing the console the answer —
/// a state the console renders natively, being the same shape an older host
/// (one that ignored `detach`) has always returned.
#[tokio::test]
async fn a_detached_request_without_a_turn_row_settles_synchronously() {
    let home_dir = home();
    let state = state_with_failing_runs(home_dir.path()).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let body = post_chat(&app, &cookie, r#"{"text":"rowless detach","detach":true}"#).await;

    // The settled shape, not the empty 202: the console is handed the
    // answer, never a turn id it cannot act on.
    assert!(
        body.get("detached").is_none(),
        "a rowless turn must not claim it can be read back: {body}"
    );
    assert!(
        body["responses"].as_array().is_some_and(|r| !r.is_empty()),
        "the synchronous fallback still delivers the reply: {body}"
    );
}

/// A thread reply survives a reload: the parent id posted with the message
/// comes back on both the operator's line and the answer it drew, so a
/// rehydrating console folds the exchange under the same row it was typed
/// under instead of flattening it into the channel.
#[tokio::test]
async fn thread_replies_survive_a_history_reload() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let dm = default_dm(&state).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let root = post_chat(&app, &cookie, r#"{"text":"the plan"}"#).await;
    let root_id = root["messageId"].as_str().unwrap().to_string();

    let threaded = post_chat(
        &app,
        &cookie,
        &serde_json::json!({ "text": "a follow-up", "parent": root_id }).to_string(),
    )
    .await;
    assert!(
        threaded["responses"][0]["messageId"]
            .as_str()
            .is_some_and(|id| id != root_id),
        "the answer is its own journal line, not the root's"
    );

    let history = get_history(&app, &cookie, &dm).await;
    let parented: Vec<(&str, Option<&str>)> = history
        .iter()
        .map(|m| (m["text"].as_str().unwrap(), m["parentId"].as_str()))
        .collect();
    // The root sits in the channel; both halves of the threaded exchange
    // hang off it — the answer under the row the thread opened from, not
    // under the question, so a thread never nests inside a thread.
    assert!(
        parented.contains(&("the plan", None)),
        "root should be unparented: {parented:?}"
    );
    assert!(
        parented.contains(&("a follow-up", Some(root_id.as_str()))),
        "threaded message lost its parent: {parented:?}"
    );
    assert!(
        parented
            .iter()
            .any(|(text, parent)| *text == "You said: a follow-up"
                && *parent == Some(root_id.as_str())),
        "the reply to a threaded message left the thread: {parented:?}"
    );
}

/// A parent that is not a message id is a 400, not a silently-flattened
/// thread: a reply that quietly lands in the channel reads to the operator
/// as a reply that went missing.
#[tokio::test]
async fn chat_rejects_a_parent_that_is_not_a_message_id() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hi","parent":"m3"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A reaction records who reacted and survives a reload; clearing it removes
/// the row; and setting the same reaction twice leaves exactly one row —
/// the explicit `on` flag is what makes the write idempotent.
#[tokio::test]
async fn reactions_persist_are_attributed_and_are_idempotent() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let dm = default_dm(&state).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let sent = post_chat(&app, &cookie, r#"{"text":"ship it"}"#).await;
    let target = sent["messageId"].as_str().unwrap().to_string();

    assert_eq!(
        post_reaction(&app, &cookie, &target, "👍", true).await,
        StatusCode::NO_CONTENT
    );
    // Twice, deliberately: a retry or a double tap must not double the row.
    assert_eq!(
        post_reaction(&app, &cookie, &target, "👍", true).await,
        StatusCode::NO_CONTENT
    );

    let history = get_history(&app, &cookie, &dm).await;
    let reacted = history
        .iter()
        .find(|m| m["id"].as_str() == Some(target.as_str()))
        .expect("the reacted-to message is still in history");
    let rows = reacted["reactions"].as_array().expect("reactions present");
    assert_eq!(rows.len(), 1, "one row per person per emoji: {rows:?}");
    assert_eq!(rows[0]["emoji"], "👍");
    assert_eq!(rows[0]["mine"], true, "the reader is the one who reacted");
    assert!(
        rows[0]["by"].as_str().is_some_and(|by| !by.is_empty()),
        "a reaction names who made it: {rows:?}"
    );

    // Clearing drops the row entirely rather than leaving a zero behind.
    assert_eq!(
        post_reaction(&app, &cookie, &target, "👍", false).await,
        StatusCode::NO_CONTENT
    );
    let history = get_history(&app, &cookie, &dm).await;
    let cleared = history
        .iter()
        .find(|m| m["id"].as_str() == Some(target.as_str()))
        .unwrap();
    assert!(
        cleared.get("reactions").is_none(),
        "a cleared reaction leaves no row: {cleared:?}"
    );
}

/// A reaction may only name a chat message. A sequence position that holds
/// something else — or nothing at all — is a 404, so the log can never carry
/// a reaction no reader could render.
#[tokio::test]
async fn reactions_refuse_a_target_that_is_not_a_message() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let not_a_message = runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::FeedbackFiled {
                note: "unrelated".to_string(),
            },
        )
        .await
        .unwrap();
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    assert_eq!(
        post_reaction(
            &app,
            &cookie,
            &not_a_message.value().to_string(),
            "👍",
            true
        )
        .await,
        StatusCode::NOT_FOUND
    );
    // A sequence position nothing has ever occupied.
    assert_eq!(
        post_reaction(&app, &cookie, "99999", "👍", true).await,
        StatusCode::NOT_FOUND
    );
    // And a target that is not a sequence position at all.
    assert_eq!(
        post_reaction(&app, &cookie, "m3", "👍", true).await,
        StatusCode::BAD_REQUEST
    );
}

/// A reaction is a journal line read by the operator projection, so it takes
/// an emoji and not a payload: empty, oversized, and control-character
/// bodies are all refused.
#[tokio::test]
async fn reactions_refuse_a_body_that_is_not_an_emoji() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let sent = post_chat(&app, &cookie, r#"{"text":"hi"}"#).await;
    let target = sent["messageId"].as_str().unwrap().to_string();

    for bad in ["", "   ", "yes\nno", &"x".repeat(REACTION_MAX_BYTES + 1)] {
        assert_eq!(
            post_reaction(&app, &cookie, &target, bad, true).await,
            StatusCode::BAD_REQUEST,
            "accepted a non-emoji reaction: {bad:?}"
        );
    }
    // A multi-code-point emoji is still one reaction, and is accepted.
    assert_eq!(
        post_reaction(&app, &cookie, &target, "👩‍💻", true).await,
        StatusCode::NO_CONTENT
    );
}

/// PR #1781 review: `history_for_desk` (reload) and `project_event_for_viewer`
/// (live SSE) both already hide an owner-fallback report from a non-admin —
/// this proves the reaction route agrees, rather than letting a Member
/// react to (and thereby confirm the existence and sequence position of) a
/// report they cannot read. Answered with the same 404 an unknown sequence
/// gets, not a 403, so probing this endpoint cannot distinguish "hidden"
/// from "never existed".
#[tokio::test]
async fn reactions_refuse_a_target_that_is_an_admin_only_report() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let report = runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "operator".into(),
                agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
                text: "no admin has a mailbox".into(),
                steps: Vec::new(),
                episode: None,
            },
        )
        .await
        .unwrap();
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let app = router(state);
    let member_cookie = crate::server::test_support::member_cookie("acme");
    let admin_cookie = crate::server::test_support::fixed_cookie("acme");

    // A Member gets the same 404 an unknown message would.
    assert_eq!(
        post_reaction(
            &app,
            &member_cookie,
            &report.value().to_string(),
            "👍",
            true
        )
        .await,
        StatusCode::NOT_FOUND
    );
    // An admin may react to it normally.
    assert_eq!(
        post_reaction(&app, &admin_cookie, &report.value().to_string(), "👍", true).await,
        StatusCode::NO_CONTENT
    );
}

/// Regression for the third acceptance item of #364, which the console's
/// own scoping already satisfied but nothing pinned: a message posted in one
/// channel must be absent from another, end to end through the route — not
/// only in the `owns` predicate. Reactions ride the same boundary.
#[tokio::test]
async fn a_message_in_one_channel_is_absent_from_another() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_manifest(&home, desk_manifest()).await;
    let dm = default_dm(&state).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let in_studio = post_chat(&app, &cookie, r#"{"text":"studio only","chat":"studio"}"#).await;
    let studio_id = in_studio["messageId"].as_str().unwrap().to_string();
    post_chat(&app, &cookie, r#"{"text":"general only"}"#).await;
    assert_eq!(
        post_reaction(&app, &cookie, &studio_id, "👀", true).await,
        StatusCode::NO_CONTENT
    );

    let studio: Vec<String> = get_history(&app, &cookie, "studio")
        .await
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect();
    let general = get_history(&app, &cookie, &dm).await;
    let general_texts: Vec<&str> = general
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();

    assert!(
        studio.iter().any(|t| t == "studio only"),
        "the desk lost its own message: {studio:?}"
    );
    assert!(
        !studio.iter().any(|t| t == "general only"),
        "a General message leaked into the desk: {studio:?}"
    );
    assert!(
        general_texts.contains(&"general only"),
        "General lost its own message: {general_texts:?}"
    );
    assert!(
        !general_texts.contains(&"studio only"),
        "a desk message leaked into General: {general_texts:?}"
    );
    // The reaction is on the desk's message, so it is not visible from a
    // channel that cannot see the message it is about.
    assert!(
        general.iter().all(|m| m.get("reactions").is_none()),
        "a reaction crossed a channel boundary: {general:?}"
    );
}

/// **Issue #2028 (finding 2, deadlock regression).** Answering a
/// task-backed blocker in a DM runs the whole path end to end: the route
/// reads and classifies the reply, settles the verdict, and waits on the
/// follow-up that re-dispatches the card — and that follow-up runs on a
/// spawned task which takes `task_writes` for its board edit.
///
/// So the route must not still hold `task_writes` when it waits. It did,
/// having mirrored the guard from the review branch above it, and the two
/// together are a deadlock: the handler waits for a task that is waiting for
/// the handler's lock. Explicitly bounded rather than left to hang, so a
/// regression fails in seconds instead of taking a runner down for an hour.
#[cfg(feature = "openhuman")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dm_answer_to_a_task_backed_blocker_completes() {
    use crate::company::blocker_sender::BlockerSenderSignals;
    use crate::ports::blockers::{BlockerKind, BlockerPayload, BlockerSource, BlockerStep};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = build_state_with_brain_and_manifest(
        &home,
        "running",
        AppConfig::default(),
        None,
        roster_manifest(),
    )
    .await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();
    let app = router(state);

    let mut card = crate::ports::tasks::TaskRecord {
        id: "t-9".to_string(),
        title: crate::ports::tasks::TaskTitle::authored("Draft the launch note"),
        note: None,
        column: crate::ports::tasks::COLUMN_PAUSED.to_string(),
        priority: "medium".to_string(),
        assignee: "backend_engineer".to_string(),
        updated_at_millis: 1,
        origin: None,
        origin_message_seq: None,
        parent_task_id: None,
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        bounced: None,
    };
    card.origin =
        crate::ports::tasks::TaskOrigin::new(Some("dm:backend_engineer".to_string()), None);
    runtime.tasks().upsert(runtime.id(), &card).await.unwrap();

    runtime
        .park_blocker(
            &BlockerPayload {
                kind: BlockerKind::Infrastructure,
                source: BlockerSource::Provider,
                step: Some(BlockerStep::Task {
                    task_id: "t-9".to_string(),
                }),
                reason: "the model id was rejected".to_string(),
                needed: "a model id this provider serves".to_string(),
                group_key: None,
            },
            "t-9",
            BlockerSenderSignals {
                started_by: None,
                owner_desk: None,
                assignee: Some("backend_engineer".to_string()),
            },
        )
        .await
        .expect("parks the blocker into the teammate's DM");

    let response = tokio::time::timeout(
        Duration::from_secs(30),
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/companies/acme/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"chat":"dm:backend_engineer","text":"yes, go ahead and retry it"}"#,
                ))
                .unwrap(),
        ),
    )
    .await
    .expect(
        "answering a task-backed blocker in a DM deadlocked: the route held the board \
         lock while waiting on the follow-up that needs it",
    )
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        runtime.pending_approvals().is_empty(),
        "the answered blocker is retired"
    );
    let moved = runtime
        .tasks()
        .list(runtime.id())
        .await
        .expect("list")
        .into_iter()
        .find(|t| t.id == "t-9")
        .expect("the card is still on the board");
    assert_eq!(
        moved.column,
        crate::ports::tasks::COLUMN_IN_PROGRESS,
        "the DM answer re-dispatched the paused card"
    );
}
