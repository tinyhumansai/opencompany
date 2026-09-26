use super::*;
use crate::ports::tasks::TaskTitle;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;
use super::operator_test_support_4::*;

/// Issue #65: the console's default thread addresses sends with
/// `chat: "main"`, but pre-threading history and the synthetic operator
/// desk are keyed on `"General"`. A transcript spanning both ids — one
/// operator turn journaled under each — must read back as one history via
/// the REST route with no `?desk=` selector (the console's default read).
#[tokio::test]
async fn chat_history_route_reunifies_general_and_main_transcripts() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                episode: None,
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "General".to_string(),
                agent_id: "ceo".to_string(),
                text: "reply under General".to_string(),
                steps: Vec::new(),
            },
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                episode: None,
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "main".to_string(),
                agent_id: "ceo".to_string(),
                text: "reply under main".to_string(),
                steps: Vec::new(),
            },
        )
        .await
        .unwrap();

    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/chat/history")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let messages = value.as_array().unwrap();
    let texts: Vec<&str> = messages
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert!(
        texts.contains(&"reply under General"),
        "missing General-id reply: {texts:?}"
    );
    assert!(
        texts.contains(&"reply under main"),
        "missing main-id reply: {texts:?}"
    );
}

/// The console must name the session the runtime actually uses.
///
/// Pinned to [`openhuman_session_key`] itself rather than to the literal
/// `"acme:ceo"`, because the property worth keeping is not the current
/// spelling — it is that there is only ever **one** spelling. A route that
/// built its own `format!` would pass a literal assertion on the day it was
/// written and go on passing it the day the minting function changed.
#[tokio::test]
async fn the_session_route_reports_the_key_openhuman_session_key_mints() {
    let expected = crate::session_key::openhuman_session_key(&CompanyId::new("acme"), "ceo");
    let rows = session_rows("/api/v1/companies/acme/agents/ceo/session").await;
    for row in &rows {
        assert_eq!(
            row.get("openhumanSessionKey").and_then(|k| k.as_str()),
            Some(expected.as_str()),
            "every row of one agent's session belongs to that one session: {row}"
        );
    }
}

/// The single-company alias resolves the same company, so it must report
/// the same session — an operator reading the same agent through the other
/// scope form is not looking at a second session.
#[tokio::test]
async fn both_scope_forms_of_the_session_route_report_the_same_key() {
    let scoped = session_rows("/api/v1/companies/acme/agents/ceo/session").await;
    let alias = session_rows("/api/v1/company/agents/ceo/session").await;
    assert_eq!(
        scoped[0].get("openhumanSessionKey"),
        alias[0].get("openhumanSessionKey"),
    );
}

/// The field reaches the browser under the name the console binds to.
/// `tsc` cannot check a hand-written interface against a Rust DTO; this is
/// that check, in the idiom the folded-aside test above set.
#[tokio::test]
async fn the_session_key_reaches_the_wire_as_camel_case() {
    let rows = session_rows("/api/v1/companies/acme/agents/ceo/session").await;
    assert!(
        rows[0].get("openhuman_session_key").is_none(),
        "snake_case would silently read as undefined in the console: {}",
        rows[0]
    );
    assert!(rows[0].get("openhumanSessionKey").is_some(), "{}", rows[0]);
}

/// Regression: a reply's tool-call timeline must survive a history reload —
/// switching threads and coming back reloads `chat/history`, which used to
/// return text only, so the steps vanished. They are now persisted on the
/// `AgentReply` and projected back through the DTO.
#[tokio::test]
async fn chat_history_route_rehydrates_reply_steps() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                episode: None,
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "main".to_string(),
                agent_id: "ceo".to_string(),
                text: "done".to_string(),
                steps: vec![TurnStep {
                    kind: crate::ports::types::TurnStepKind::ToolCall,
                    status: crate::ports::types::TurnStepStatus::Ok,
                    label: "Reading messages".to_string(),
                    detail: None,
                    elapsed_ms: Some(9),
                    ..TurnStep::default()
                }],
            },
        )
        .await
        .unwrap();

    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/chat/history")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let reply = value
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["text"] == "done")
        .expect("the reply is in history");
    assert_eq!(
        reply["steps"][0]["label"], "Reading messages",
        "the persisted timeline must ride back on the history DTO"
    );
    assert_eq!(reply["steps"][0]["status"], "ok");
    assert_eq!(reply["steps"][0]["elapsedMs"], 9);
}

/// A reply's produced-file buttons are durable transcript data, and the
/// projection must stop returning either kind once its target is gone.
#[tokio::test]
async fn chat_history_route_rehydrates_outputs_and_drops_deleted_targets() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let company = runtime.id().clone();

    runtime
        .workspace()
        .create(
            &company,
            &attachment_note_node("node-1", "launch-note.md"),
            Some("Launch notes"),
        )
        .await
        .unwrap();
    runtime
        .workspace()
        .create(
            &company,
            &attachment_note_node("node-2", "surviving-note.md"),
            Some("Keep this note"),
        )
        .await
        .unwrap();
    runtime
        .artifacts()
        .upsert(
            &company,
            &crate::ports::artifacts::ArtifactRecord::new(
                "artifact-1",
                "task-1",
                "Launch brief",
                crate::ports::artifacts::ArtifactKind::Markdown,
                "# Launch",
                "ceo",
                1,
            ),
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                episode: None,
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: vec![
                    crate::ports::types::ChatOutput {
                        kind: crate::ports::types::ChatOutputKind::WorkspaceNode,
                        target_id: "node-1".to_string(),
                        title: "launch-note.md".to_string(),
                        task_id: None,
                        version: None,
                    },
                    crate::ports::types::ChatOutput {
                        kind: crate::ports::types::ChatOutputKind::WorkspaceNode,
                        target_id: "node-2".to_string(),
                        title: "surviving-note.md".to_string(),
                        task_id: None,
                        version: None,
                    },
                    crate::ports::types::ChatOutput {
                        kind: crate::ports::types::ChatOutputKind::Artifact,
                        target_id: "artifact-1".to_string(),
                        title: "Launch brief".to_string(),
                        task_id: Some("task-1".to_string()),
                        version: Some(1),
                    },
                ],
                chat_id: "main".to_string(),
                agent_id: "ceo".to_string(),
                text: "I wrote both files.".to_string(),
                steps: Vec::new(),
            },
        )
        .await
        .unwrap();

    let history = |app: axum::Router| async move {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/company/chat/history")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    };

    let app = router(state);
    let first = history(app.clone()).await;
    let reply = first
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["text"] == "I wrote both files.")
        .unwrap();
    assert_eq!(reply["outputs"].as_array().unwrap().len(), 3);
    assert_eq!(reply["outputs"][0]["targetId"], "node-1");
    assert_eq!(reply["outputs"][1]["targetId"], "node-2");
    assert_eq!(reply["outputs"][2]["kind"], "artifact");
    assert_eq!(reply["outputs"][2]["taskId"], "task-1");
    assert_eq!(reply["outputs"][2]["version"], 1);

    runtime
        .workspace()
        .delete(&company, "node-1")
        .await
        .unwrap();
    runtime
        .artifacts()
        .delete(&company, "artifact-1")
        .await
        .unwrap();

    let reloaded = history(app).await;
    let reply = reloaded
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["text"] == "I wrote both files.")
        .unwrap();
    let outputs = reply["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 1, "only live targets may rehydrate: {reply}");
    assert_eq!(outputs[0]["targetId"], "node-2");
}

/// Issue #246: a reply that opened a board card must still say so after a
/// transcript reload. The "card opened" chip is rendered from `taskId`, and
/// a chip that exists only on the live POST response vanishes the moment
/// the operator switches threads and comes back — which is exactly when
/// they would go looking for it.
#[tokio::test]
async fn chat_history_route_rehydrates_the_card_a_reply_opened() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    // The card has to actually be on the board: the history projection
    // reports `taskId` only for a card that still exists, so that a chip
    // cannot come back pointing at a card someone deleted (issue #984).
    runtime
        .tasks()
        .upsert(
            runtime.id(),
            &crate::ports::tasks::TaskRecord {
                id: "t-77".to_string(),
                title: TaskTitle::authored("Draft the launch note"),
                note: None,
                column: crate::ports::tasks::COLUMN_TODO.to_string(),
                priority: "medium".to_string(),
                assignee: String::new(),
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

    for (text, task_id) in [
        ("opened one", Some("t-77".to_string())),
        ("just talking", None),
    ] {
        runtime
            .events()
            .append(
                runtime.id(),
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    episode: None,
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id,
                    outputs: Vec::new(),
                    chat_id: "main".to_string(),
                    agent_id: "ceo".to_string(),
                    text: text.to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .unwrap();
    }

    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/chat/history")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let messages = value.as_array().unwrap();

    let opened = messages
        .iter()
        .find(|m| m["text"] == "opened one")
        .expect("the card-opening reply is in history");
    assert_eq!(
        opened["taskId"], "t-77",
        "the chip's correlation key must ride back on the history DTO"
    );

    // A reply that opened nothing omits the key rather than sending null,
    // so no bubble grows a chip it should not have — and every message
    // journaled before this field existed reads back unchanged.
    let chatter = messages
        .iter()
        .find(|m| m["text"] == "just talking")
        .expect("the ordinary reply is in history");
    assert!(
        chatter.get("taskId").is_none(),
        "an ordinary chat reply must not carry a card: {chatter}"
    );
}

/// A desk id with no `?desk=` selector defaults to the operator/General
/// thread; an unaddressed thread id that neither matches a manifest desk
/// nor the General desk reads back empty rather than erroring.
#[tokio::test]
async fn chat_history_route_unknown_desk_is_empty_not_an_error() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/chat/history?desk=strategy")
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
}

/// Issue #862: REST history carries the same cursor/window contract as the
/// paginated GraphQL surface. A copilot replay can therefore ask for the
/// tail it needs without the route reading past its cursor.
#[tokio::test]
async fn chat_history_route_honors_before_and_limit() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let mut seqs = Vec::new();
    for text in ["oldest", "kept", "newest"] {
        seqs.push(
            runtime
                .events()
                .append(
                    runtime.id(),
                    CompanyEvent::AgentReply {
                        audience: Vec::new(),
                        episode: None,
                        mentions: Vec::new(),
                        mention_depth: 0,
                        parent: None,
                        task_id: None,
                        outputs: Vec::new(),
                        chat_id: "workflow-copilot:weekly_report".to_string(),
                        agent_id: "ceo".to_string(),
                        text: text.to_string(),
                        steps: Vec::new(),
                    },
                )
                .await
                .unwrap(),
        );
    }

    let uri = format!(
        "/api/v1/company/chat/history?desk=workflow-copilot:weekly_report&before={}&limit=1",
        seqs[2].value()
    );

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value[0]["text"], "kept");
    assert_eq!(value.as_array().unwrap().len(), 1);
}

/// A history cursor pages messages, not the current reaction state. A
/// toggle can be journaled after the cursor for a message still selected by
/// that cursor, and must therefore remain visible on the paged result.
#[tokio::test]
async fn chat_history_cursor_keeps_later_reactions_on_displayed_messages() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let message = runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::AgentReply {
                audience: Vec::new(),
                episode: None,
                mentions: Vec::new(),
                mention_depth: 0,
                parent: None,
                task_id: None,
                outputs: Vec::new(),
                chat_id: "General".to_string(),
                agent_id: "ceo".to_string(),
                text: "kept".to_string(),
                steps: Vec::new(),
            },
        )
        .await
        .unwrap();
    let cursor = runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::FeedbackFiled {
                note: "cursor marker".to_string(),
            },
        )
        .await
        .unwrap();
    runtime
        .events()
        .append(
            runtime.id(),
            CompanyEvent::ReactionToggled {
                message_seq: message,
                emoji: "👍".to_string(),
                on: true,
                by: None,
            },
        )
        .await
        .unwrap();

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/company/chat/history?before={}&limit=1",
                    cursor.value()
                ))
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value[0]["id"], message.value().to_string());
    assert_eq!(value[0]["reactions"][0]["emoji"], "👍");
}

/// The enabler for everything else in #364: a sent message comes back with
/// the durable id it was journaled under, on both halves of the exchange —
/// the operator's own line and each reply — and those ids are the same ones
/// `chat/history` returns on the next read.
#[tokio::test]
async fn chat_response_carries_durable_message_ids() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let dm = default_dm(&state).await;
    let app = router(state);
    let cookie = crate::server::test_support::fixed_cookie("acme");

    let sent = post_chat(&app, &cookie, r#"{"text":"hi"}"#).await;
    let mine = sent["messageId"].as_str().expect("own message id");
    let reply = sent["responses"][0]["messageId"]
        .as_str()
        .expect("reply message id");
    assert_ne!(mine, reply, "the two halves are separate journal lines");

    let history = get_history(&app, &cookie, &dm).await;
    let ids: Vec<&str> = history.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&mine), "own id absent from history: {ids:?}");
    assert!(
        ids.contains(&reply),
        "reply id absent from history: {ids:?}"
    );
}
