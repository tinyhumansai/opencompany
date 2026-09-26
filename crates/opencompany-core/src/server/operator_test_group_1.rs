use super::*;
use crate::ports::types::CompanyRecord;
use crate::ports::types::EventSeq;
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;

#[tokio::test]
async fn chat_returns_echoed_response() {
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
                // Issue #1725: not "hi". A bare pleasantry is answered by
                // the runtime without a turn, so the echo brain — which is
                // what this asserts is wired up — never sees it.
                .body(Body::from(r#"{"text":"ship the landing page"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["responses"][0]["text"],
        "You said: ship the landing page"
    );
    assert_eq!(value["responses"][0]["channel"], "operator");
}

/// An actionable operator chat opens **no** card on its own — the board is a
/// tool call. The REST handler used to card any message that led with an
/// action verb, so "build the landing page" became a work item before any
/// agent had read it; it is now tracked only when an agent calls `spawn_task`
/// (this test runs on the echo brain, which never does).
///
/// The one signal the route still cards on is the composer's "Build me the
/// workflow" control, and issue #576 still holds for that card: it lands in
/// **Planning**, not To-do, because a signed-in person (`fixed_cookie`) is
/// behind the request. `tasks.len() == 1` is doing real work there: the card
/// is created *directly* in `planning` by a single `upsert_task`, so a second
/// card, or one that arrived via To-do and was promoted, would both show.
#[tokio::test]
async fn a_plain_chat_opens_no_card_and_a_requested_workflow_lands_in_planning() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    // An instruction the lexical triage reads as work — and no card.
    assert!(
        matches!(
            crate::company::task_intent::triage_message("build the landing page"),
            crate::company::task_intent::MessageTriage::Track(_)
        ),
        "fixture must be a message the triage calls work, or this proves nothing"
    );
    let r = app
        .clone()
        .oneshot(chat_to("build the landing page", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        runtime.tasks().list(&id).await.unwrap().is_empty(),
        "an actionable ask opens no card by itself: tracking is an agent's tool call"
    );

    // Greeting → still nothing.
    let r = app.clone().oneshot(chat_to("thanks!", None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(runtime.tasks().list(&id).await.unwrap().is_empty());

    // The explicit control → one Planning card, titled from the ask.
    let r = app
        .oneshot(workflow_chat_to("build the landing page", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1, "a requested workflow opens one card");
    assert_eq!(
        tasks[0].column,
        crate::ports::tasks::COLUMN_PLANNING,
        "issue #576: the prompt box promotes its own card, with no drag"
    );
    assert_eq!(tasks[0].priority, "medium");
    assert_eq!(tasks[0].title, "Build the landing page");
}

/// Issue #1725, through the route an operator actually hits: "hi" comes
/// back answered, with no card, no steps, and no turn behind it.
///
/// The unit-level proof that the brain is not called lives in
/// `runtime::cycle`'s `a_bare_greeting_answers_without_calling_the_brain`,
/// where a counting brain can be injected. This one pins that the chat
/// handler reaches that path at all — the two are separate failures, and a
/// correct fast path nothing routes to leaves the bug where it was.
///
/// The echo brain answers `"You said: <text>"`, so the assertion below is
/// also the evidence: a canned greeting means the brain never ran.
#[tokio::test]
async fn a_bare_greeting_is_answered_without_a_turn() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["responses"][0]["text"],
        crate::company::task_intent::SmallTalk::Hello.reply(),
        "a greeting is answered by the runtime, not by a turn"
    );
    // The console showed "1 step" for a greeting on staging. There is no
    // step to show, so the field is omitted entirely.
    assert!(
        value["responses"][0]["steps"].is_null(),
        "no tool ran: {}",
        value["responses"][0]
    );
    assert!(
        runtime.tasks().list(&id).await.unwrap().is_empty(),
        "a greeting opens no card"
    );
}

/// The card a chat opens is handed to the teammate the operator addressed.
///
/// Every test in this family sends the composer's "Build me the workflow"
/// control, because that is the one signal on which the route still opens a
/// card on its own — a plain message is tracked only when an agent calls
/// `spawn_task`. What is pinned here is what a route-opened card *records*.
///
/// The fixture is the whole test. `CROSSED` names *backend* work and is
/// addressed to the **product manager**, so the two candidate answers are
/// distinguishable: pre-fix this card was born blank and the planning pass
/// filled it from a content match of the title against teammate roles —
/// which is exactly the wrong answer here. A message whose text and
/// addressee agree would pass on pre-fix code and prove nothing; that is
/// what the two "right" rows of the issue's table were.
#[tokio::test]
async fn chat_addressed_to_a_teammate_assigns_that_teammate() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app
        .oneshot(workflow_chat_to(CROSSED, Some("product_manager")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1, "an actionable ask opens one card");
    assert_eq!(
        tasks[0].assignee, "product_manager",
        "the card belongs to the teammate the operator addressed"
    );
    assert_ne!(
        tasks[0].assignee, "backend_engineer",
        "…and not to whoever the message text happens to name"
    );
}

/// A desk-addressed chat is assigned to the **desk**, not to its lead.
///
/// Writing the lead would erase the desk from the board the moment the card
/// was created — the invariant `AssigneeResolution::canonical` holds for
/// every other write site (issue #214), now held here too.
#[tokio::test]
async fn chat_addressed_to_a_desk_assigns_the_desk() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app
        .oneshot(workflow_chat_to(CROSSED, Some("engineering")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].assignee, "engineering",
        "picking a desk IS the operator's routing decision"
    );
}

/// A thread key that names nobody in particular opens a blank card when a
/// workflow is asked for: the empty string, the console's legacy fallback
/// desk id, and the default "General" desk this company does not have.
///
/// A blank assignee is what hands a card to the orchestrator's own queue.
#[tokio::test]
async fn a_chat_to_no_one_in_particular_leaves_the_card_unassigned() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    for thread in [Some(""), Some("main"), Some(DEFAULT_DESK)] {
        let r = app
            .clone()
            .oneshot(workflow_chat_to(CROSSED, thread))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "thread {thread:?}");
    }

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 3, "one card per message: {tasks:?}");
    for card in &tasks {
        assert_eq!(
            card.assignee, "",
            "a message to no one in particular leaves the card for the orchestrator"
        );
    }
}

/// A message with no `chat` at all is still accepted, but it is addressed to
/// the default agent's DM: the card is the orchestrator's and answers there.
#[tokio::test]
async fn an_unaddressed_chat_lands_in_the_default_agents_dm() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app.oneshot(workflow_chat_to(CROSSED, None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1, "one card: {tasks:?}");
    assert_eq!(tasks[0].assignee, "product_manager");
    assert_eq!(tasks[0].origin_chat_id(), Some("dm:product_manager"));

    let events = runtime
        .events()
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let asked = events
        .iter()
        .find_map(|stored| match &stored.event {
            CompanyEvent::OperatorMessage { chat, .. } => Some(chat.clone()),
            _ => None,
        })
        .expect("the operator's message is journaled");
    assert_eq!(asked.as_deref(), Some("dm:product_manager"));
}

/// A transient roster-read failure while resolving the default agent's DM
/// must not fail the send: it falls back to `DEFAULT_DESK`, exactly as an
/// unaddressed message did before that DM resolution existed.
#[tokio::test]
async fn an_unaddressed_chat_still_sends_when_the_roster_read_fails() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let (state, store) = state_with_roster_and_failing_store(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    store.fail_next_loads(1);

    let r = app.oneshot(workflow_chat_to(CROSSED, None)).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "a roster read failure must not fail the whole chat send"
    );

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1, "one card: {tasks:?}");
    assert_eq!(
        tasks[0].origin_chat_id(),
        Some(crate::server::ops::language::DEFAULT_DESK),
        "falls back to the default desk when the roster cannot be read"
    );
}

/// A thread key that names nothing on the roster is not an error: the card
/// is opened, unassigned, exactly as it was before this route resolved
/// anything. A chat must never 400 — and must never lose its card — over who
/// it was addressed to.
#[tokio::test]
async fn an_unknown_addressee_leaves_the_card_unassigned() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app
        .oneshot(workflow_chat_to(CROSSED, Some("nobody_by_that_name")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "an unknown thread is not a 400");

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1, "…and the card is still opened");
    assert_eq!(tasks[0].assignee, "", "…with nobody guessed onto it");
}

/// The card remembers the thread it was opened from, so the marker that says
/// it settled lands back in the conversation that asked for the work.
///
/// `origin_chat_id` is the field issue #151 added for exactly this, and the
/// console already renders the marker in whatever channel it names — the
/// route was simply never filling it in. An unaddressed message opens a card
/// with no origin.
#[tokio::test]
async fn a_chat_card_remembers_the_thread_it_was_opened_from() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app
        .clone()
        .oneshot(workflow_chat_to(CROSSED, Some("dm:designer")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(
        tasks[0].origin_chat_id(),
        Some("dm:designer"),
        "the thread as the console addressed it"
    );

    let r = app
        .oneshot(workflow_chat_to("draft the investor update", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let tasks = runtime.tasks().list(&id).await.unwrap();
    let unaddressed = tasks
        .iter()
        .find(|c| c.title == "Draft the investor update")
        .expect("the second card");
    assert_eq!(
        unaddressed.origin_chat_id(),
        Some("dm:product_manager"),
        "an unaddressed message answers in the default agent's DM"
    );

    // The addressed card, found by title rather than by index: the two are
    // listed together from here on, and this assertion is about the one
    // that has a desk.
    let addressed = tasks
        .iter()
        .find(|c| c.origin_chat_id() == Some("dm:designer"))
        .expect("the addressed card");
    // Reversed once #1890 D landed alongside B, and the reversal is the
    // point. B alone read the message's own `parent`, so a card raised from
    // a channel-level question recorded no thread — right while a thread
    // was only ever something an operator opened by hand.
    //
    // D changed what a thread is: an answer parents to the message that
    // opened the exchange, so that question is a root. A card raised from
    // it belongs to the thread it just started, and recording `None` here
    // would put the settle marker in the channel while the answer to the
    // same message sat in a thread — the split B exists to prevent.
    assert!(
        addressed.origin_parent().is_some(),
        "a channel-level question is itself the thread its card was raised in",
    );
}

/// Issue #1890 D part 1: every answer threads under the message that
/// opened the exchange.
///
/// The two arms are the whole rule, and the second is the change: before
/// it, an answer to an unthreaded question was journaled unparented, so the
/// only threads that existed were ones an operator opened by hand.
#[test]
fn an_answer_threads_under_the_message_that_opened_the_exchange() {
    let message = EventSeq::new(41);
    // Not in a thread: the exchange becomes one, rooted at the question.
    assert_eq!(reply_thread(None, message), Some(message));
    // Already in one: the same root, so a follow-up does not open a thread
    // of its own — N messages in a thread is one topic, not N.
    let root = EventSeq::new(7);
    assert_eq!(reply_thread(Some(root), message), Some(root));
    // Never `None`: uniform is what keeps `parent` out of the hands of race
    // timing, since `parent` is permanent and presentation is not.
    assert!(reply_thread(None, message).is_some());
    assert!(reply_thread(Some(root), message).is_some());
}

/// Issue #1890 B: the card remembers **which thread** inside that channel.
///
/// The channel alone was never enough — a channel holds any number of live
/// threads, and a settle filed against the channel surfaces in none of
/// them. A message's own `parent` IS its root (a reply is parented to its
/// question's parent, never to the question), so the route reads it
/// straight off the send with no walk.
#[tokio::test]
async fn a_chat_card_remembers_the_thread_inside_the_channel() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app
        .oneshot(workflow_chat_in_thread(
            CROSSED,
            Some("dm:designer"),
            Some(41),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks[0].origin_chat_id(), Some("dm:designer"));
    assert_eq!(
        tasks[0].origin_parent(),
        Some(crate::ports::types::EventSeq::new(41)),
        "the root the operator was answering in",
    );
}

/// The console mints a DM channel id as `dm:<teammate-id>`, and that form is
/// documented as a valid channel key — so it has to address the teammate
/// here as well as in the responder lookup.
#[tokio::test]
async fn a_console_dm_channel_id_addresses_the_teammate() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_roster(&home).await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    let r = app
        .oneshot(workflow_chat_to(CROSSED, Some("dm:designer")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].assignee, "designer");
}

/// Issue #845: an explicit "Build me the workflow" opens a card even when
/// the triage would have opened nothing.
///
/// The composer's toggle was consulted *only* on the card-opening branch,
/// and that branch is gated on the triage. So a `workflow` request the
/// classifier read as a question or as chatter dropped the choice on the
/// floor: no card, therefore no builder pass, therefore nothing built — and
/// no error either, because a conversational reply came back as though the
/// message had been handled.
///
/// Both halves are pinned here: the same text opens nothing as a `once`
/// message and opens a `workflow` card when the operator asked for one.
#[tokio::test]
async fn an_explicit_workflow_request_opens_a_card_the_triage_declined() {
    use crate::ports::tasks::TaskDeliverable;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home, "running").await;
    let id = CompanyId::new("acme");
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    // A question by construction — `is_question` fires on the wh-opener, so
    // the triage answers `Answer` and the card branch declines.
    let text = "what would a weekly AEO audit of the blog even look like?";
    assert!(
        matches!(
            crate::company::task_intent::triage_message(text),
            crate::company::task_intent::MessageTriage::Answer
        ),
        "fixture must be one the triage declines to card, or this proves nothing"
    );

    let chat = |deliverable: Option<&str>| {
        let body = match deliverable {
            Some(d) => format!(
                r#"{{"text":{},"deliverable":"{d}"}}"#,
                serde_json::json!(text)
            ),
            None => format!(r#"{{"text":{}}}"#, serde_json::json!(text)),
        };
        Request::builder()
            .method("POST")
            .uri("/api/v1/company/chat")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // `once` (and no choice at all): unchanged — the triage still decides.
    let r = app.clone().oneshot(chat(None)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app.clone().oneshot(chat(Some("once"))).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        runtime.tasks().list(&id).await.unwrap().is_empty(),
        "a `once` question must still open nothing"
    );

    // `workflow`: the operator's explicit choice outranks the classifier.
    let r = app.oneshot(chat(Some("workflow"))).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let tasks = runtime.tasks().list(&id).await.unwrap();
    assert_eq!(tasks.len(), 1, "the workflow choice must open its card");
    assert_eq!(
        tasks[0].deliverable,
        TaskDeliverable::Workflow,
        "and it must be the deliverable that routes it to the builder pass"
    );
    // Titled through `to_title`, exactly as a `Track` card would have been.
    assert_eq!(
        tasks[0].title,
        crate::company::task_intent::to_title(text),
        "a bypassed card must be titled byte-for-byte as a tracked one"
    );
}

/// CHAT-021: a card-open failure must not vanish into a server log while
/// the operator sees an ordinary success. The chat turn itself still
/// returns 200 — it did nothing wrong — but a durable system note in the
/// same desk must say the card did not open, exactly as a turn that
/// aborts mid-answer already leaves a visible notice rather than silence.
#[tokio::test]
async fn a_card_open_failure_is_reported_in_the_channel_not_swallowed() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let store = FsCompanyStore::new(home.clone());
    let id = CompanyId::new("acme");
    use crate::ports::CompanyStore;
    store
        .save(&CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: roster_manifest(),
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
    let runtime = RuntimeBuilder::new(home, roster_manifest())
        .with_id(id.clone())
        .with_tasks(Arc::new(FailingTaskUpsert))
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default());
    state.registry().insert(id.clone(), Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    let runtime = state.registry().get(&id).unwrap();
    let app = router(state);

    // `deliverable: "workflow"` opens a card deterministically, whatever
    // the lexical triage would have made of the words (see the test
    // above) — the fixture does not need to be a message the classifier
    // happens to card.
    let body = format!(
        r#"{{"text":{},"deliverable":"workflow"}}"#,
        serde_json::json!("automate the weekly report")
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/company/chat")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the chat turn itself did nothing wrong and must still succeed"
    );

    let events = runtime
        .events()
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    let notice = events.into_iter().find_map(|stored| match stored.event {
        CompanyEvent::AgentReply {
            agent_id,
            text,
            chat_id,
            ..
        } if agent_id == crate::ports::SYSTEM_AUTHOR => Some((chat_id, text)),
        _ => None,
    });
    let (chat_id, text) = notice.expect(
        "a card-open failure must leave a visible system note in the channel, not just a \
         server-side log line",
    );
    assert!(text.to_lowercase().contains("card"));
    assert_eq!(
        chat_id, "dm:product_manager",
        "an unaddressed message's notice lands in the default agent's DM"
    );
}

#[tokio::test]
async fn a_notice_with_no_addressed_chat_goes_to_the_default_agents_dm() {
    let home_dir = home();
    let state = state_with_roster(home_dir.path()).await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();

    assert_eq!(
        addressed_or_default_dm(&runtime, Some("engineering"))
            .await
            .as_deref(),
        Some("engineering")
    );
    assert_eq!(
        addressed_or_default_dm(&runtime, None).await.as_deref(),
        Some("dm:product_manager")
    );
}
