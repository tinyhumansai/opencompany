use super::*;
use crate::AppConfig;
use crate::ports::types::EventSeq;
use crate::server::router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::operator_test_support_1::*;
use super::operator_test_support_2::*;
use super::operator_test_support_3::*;

/// POL-011: `extend_approval` is one handler mounted under both scope
/// forms (`scoped("/approvals/{aid}/extend", ...)`), so the platform
/// `/companies/{id}/...` form must carry the exact same admin gate the
/// `/company/...` alias does — and must not become a side channel that
/// resolves against the wrong company merely because its id rode in the
/// path instead of the alias.
#[tokio::test]
async fn extend_on_the_scoped_route_form_enforces_admin_and_the_right_company() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let approval = park_for_extend(&runtime, "appr-scoped", crate::ports::now_millis()).await;
    crate::server::test_support::seed_fixed_member(&state, "acme").await;
    let member_cookie = crate::server::test_support::member_cookie("acme");
    let admin_cookie = crate::server::test_support::fixed_cookie("acme");
    let app = router(state);

    // AUTH: a member is refused on the scoped form exactly as on the alias.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/companies/acme/approvals/{approval}/extend"
                ))
                .header("cookie", &member_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // FAIL: addressing a *different* company id on the scoped form must
    // 404 rather than reach into `acme`'s gate — the path segment is the
    // only thing naming the company here, unlike the alias.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/companies/globex/approvals/{approval}/extend"
                ))
                .header("cookie", &admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a company id that does not exist must not extend acme's approval"
    );
    assert!(
        runtime.pending_approvals().iter().any(|a| a.id == approval),
        "the approval must still be sitting under its real company, untouched"
    );

    // And the scoped form works for the right admin and the right company.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/companies/acme/approvals/{approval}/extend"
                ))
                .header("cookie", &admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// **Issue #383 / #380 defect 3 — the keystone.** A client that walks away
/// mid-turn must not take the agent's continuation with it.
///
/// The host is plain `axum::serve(listener, router(state))` and nothing on
/// the resolve path was spawned, so the follow-up agent turn lived *inside*
/// the request future. Hyper drops that future the moment the peer closes,
/// and nginx closes its upstream connection when it gives up on a slow
/// response. So on a hosted tenant the sequence was: verdict recorded,
/// journaled, single-use grant minted — and then the re-dispatch the grant
/// existed for cancelled mid-flight. The operator's approval was spent and
/// the conversation never resumed, which is precisely what #380 reported.
///
/// `Router::oneshot` reproduces that cancellation faithfully rather than by
/// analogy: the mechanism is the same one hyper uses — the handler future is
/// owned by the future the caller is polling, and dropping the latter drops
/// the former.
#[tokio::test]
async fn a_dropped_connection_does_not_cancel_the_follow_up_cycle() {
    let home_dir = home();
    let c = stalled_company(home_dir.path()).await;

    // Approve it, then let the connection die once the turn is under way.
    let mut resolving = Box::pin(c.app.clone().oneshot(resolve_request(
        &c.approval_id,
        serde_json::json!({"verdict":"approve"}),
    )));
    tokio::select! {
        _ = &mut resolving => panic!("the resolve answered before the follow-up turn began"),
        _ = c.entered.notified() => {}
    }
    drop(resolving);

    // The verdict is already durable and the grant already spent — this is
    // the state the operator is left in when the proxy gives up.
    assert!(
        !c.runtime
            .pending_approvals()
            .iter()
            .any(|a| a.id == c.approval_id),
        "the verdict was journaled before the connection dropped"
    );
    assert!(
        c.runtime.grants.peek(&c.approval_id).is_some(),
        "the single-use grant was minted before the connection dropped"
    );

    // So the continuation the grant exists for must still complete.
    c.release.notify_one();
    assert!(
        await_continuation(&c.runtime).await,
        "the follow-up cycle died with the dropped connection: the grant is spent \
         and the agent never continued"
    );
    assert_eq!(
        c.runtime.grants.live_count(),
        1,
        "the continuation minted no second grant"
    );
}

/// **Issue #882.** A chat turn whose caller walks away mid-flight must still
/// finish and still journal its answer.
///
/// This is the chat-lane twin of
/// `a_dropped_connection_does_not_cancel_the_follow_up_cycle`. Both the
/// cycle and the `AgentReply` append used to live inside the request future,
/// so a turn slower than nginx's read timeout was cancelled mid-flight and
/// the answer was never written. The operator's DM history then held their
/// question and nothing else — the turn could not be read back on reload and
/// could not be resumed, which is what #882 reported. Workflow runs survived
/// the identical 504 precisely because they are spawned.
///
/// `Router::oneshot` reproduces the cancellation by the same mechanism hyper
/// uses: the handler future is owned by the future the caller polls, so
/// dropping the latter drops the former.
#[tokio::test]
async fn a_dropped_connection_does_not_lose_the_chat_turns_work() {
    let home_dir = home();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let state = build_state_with_brain(
        home_dir.path(),
        "running",
        AppConfig::default(),
        Some(Arc::new(StalledChatBrain {
            entered: entered.clone(),
            release: release.clone(),
        })),
    )
    .await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let app = router(state);

    // Send the turn, then let the connection die once it is under way —
    // exactly what the proxy does when it decides the upstream is too slow.
    let mut chatting = Box::pin(app.clone().oneshot(chat_request("run the seo audit")));
    tokio::select! {
        _ = &mut chatting => panic!("the chat answered before the turn began"),
        _ = entered.notified() => {}
    }
    drop(chatting);

    // Nothing is journaled yet: the turn is still stalled inside the brain.
    assert!(
        !reply_journaled(&runtime).await,
        "the reply was journaled before the turn was released"
    );

    // Issue #983: the turn was recorded the instant it was accepted, and
    // the record is what a re-read resolves — so at this point the operator
    // has walked away and the turn is still `Running` rather than absent.
    let row = turn_rows(&runtime)
        .await
        .pop()
        .expect("accepting the turn minted a row");
    assert_eq!(
        row.1, "running",
        "a turn whose caller is gone must still read as under way"
    );

    // The work must survive the caller giving up.
    release.notify_one();
    assert!(
        await_reply_journaled(&runtime).await,
        "the chat turn died with the dropped connection: the operator's \
         message is journaled, the answer is not, and the turn can neither \
         be read back nor resumed (issue #882)"
    );

    // Issue #983: and so must the settle. The row is written by the spawned
    // task, not by the handler, so a dropped connection leaving it
    // `Running` forever would be the #882 bug one layer down — the turn
    // finishes, the answer lands, and the status surface still claims work
    // is in flight until the next boot reaps it.
    until("the settle died with the dropped connection", async || {
        turn_rows(&runtime)
            .await
            .iter()
            .all(|(_, status)| status == "succeeded")
    })
    .await;
}

/// **Issue #983 — the direct regression for the observed empty history.**
///
/// The operator's message used to be appended *inside* the per-company
/// serial lock, so a message sent while another turn held that lock did not
/// exist anywhere until the turn ahead of it finished. Reloading during a
/// long turn showed an empty conversation: the operator could not see their
/// own question, could not tell whether it had been received, and re-sent it.
///
/// The blocking first turn is what makes this a real test. With a single
/// turn the lock is free and the cycle appends immediately, so the bug is
/// invisible — which is exactly why it survived. Two turns reproduce the
/// serial train the field report saw with five.
#[tokio::test]
async fn a_queued_message_is_in_the_transcript_before_its_turn_runs() {
    let home_dir = home();
    let (brain, entered, release) = BlockingChatBrain::new();
    let state = build_state_with_brain(
        home_dir.path(),
        "running",
        AppConfig::default(),
        Some(brain),
    )
    .await;
    let dm = default_dm(&state).await;
    let app = router(state);

    // Turn one takes the lock and stops inside the brain.
    let first = tokio::spawn({
        let app = app.clone();
        async move { app.oneshot(chat_request("the first question")).await }
    });
    entered.acquire().await.expect("turn one entered").forget();

    // Turn two is accepted while turn one still owns the lock.
    let second = tokio::spawn({
        let app = app.clone();
        async move { app.oneshot(chat_request("the second question")).await }
    });

    until(
        "the queued message never reached the transcript",
        async || {
            history_texts(&app, &dm)
                .await
                .iter()
                .any(|t| t == "the second question")
        },
    )
    .await;

    // …and it is there while its turn is provably not finished: no answer
    // has been journaled for either message.
    let texts = history_texts(&app, &dm).await;
    assert!(
        texts.contains(&"the first question".to_string()),
        "the running turn's own message is missing: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.starts_with("answered:")),
        "a turn finished before the assertion could run: {texts:?}"
    );

    release.add_permits(2);
    for turn in [first, second] {
        let response = turn.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

/// One POST journals **exactly one** `OperatorMessage`, and the response's
/// `messageId` is that message's own sequence.
///
/// The pin for the pre-journaled cycle path. The route now appends the
/// message itself and hands the cycle the seq; a cycle that appended again
/// would double every operator message in every transcript, and one that
/// reported a seq of its own would hand the console an id that resolves to
/// the wrong line — both silent, both only visible here.
#[tokio::test]
async fn one_post_journals_one_message_and_reports_its_seq() {
    let home_dir = home();
    let state = state_with_company(home_dir.path(), "running").await;
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let app = router(state);

    let response = app
        .clone()
        .oneshot(chat_request("just the one"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let journaled: Vec<(EventSeq, String)> = runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|s| match s.event {
            CompanyEvent::OperatorMessage { text, .. } => Some((s.seq, text)),
            _ => None,
        })
        .collect();
    assert_eq!(
        journaled.len(),
        1,
        "one POST must journal one message, got {journaled:?}"
    );
    assert_eq!(journaled[0].1, "just the one");
    assert_eq!(
        body["messageId"].as_str(),
        Some(journaled[0].0.value().to_string().as_str()),
        "messageId must resolve to the message's own line"
    );
}

/// **Issue #983 — the direct regression for the serial train.**
///
/// The per-company cycle lock is held for a whole turn with unbounded
/// waiters, so five concurrent messages became a queue and the fifth
/// inherited the whole queue's latency. Nothing recorded that, so an
/// operator watching a slow company could not tell "my turn is queued" from
/// "my turn is wedged" from "nothing was received".
///
/// The two statuses are what makes the wait legible, which is why the row
/// is created at accept and started only once the cycle holds the lock.
/// Collapsing them — starting the row where it is created — would make both
/// turns read `Running`, and this assertion is what stops that.
#[tokio::test]
async fn a_queued_turn_is_pending_while_the_running_one_holds_the_lock() {
    let home_dir = home();
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

    let first = tokio::spawn({
        let app = app.clone();
        async move { app.oneshot(chat_request("first")).await }
    });
    entered.acquire().await.expect("turn one entered").forget();
    let second = tokio::spawn({
        let app = app.clone();
        async move { app.oneshot(chat_request("second")).await }
    });

    // Compared as a sorted pair rather than in row order: two POSTs a
    // millisecond apart can tie on `created_at_millis`, and what is being
    // asserted is that the two turns hold *different* statuses at once, not
    // which row the store lists first.
    until(
        "the second turn never queued behind the first",
        async || {
            let mut statuses: Vec<String> = turn_rows(&runtime)
                .await
                .into_iter()
                .map(|(_, status)| status)
                .collect();
            statuses.sort();
            statuses == ["pending", "running"]
        },
    )
    .await;

    release.add_permits(2);
    for turn in [first, second] {
        assert_eq!(turn.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    until("both turns must reach a terminal status", async || {
        turn_rows(&runtime)
            .await
            .iter()
            .all(|(_, status)| status == "succeeded")
    })
    .await;
    assert_eq!(turn_rows(&runtime).await.len(), 2, "one row per POST");
}

/// The same proof over a **real socket**, so the keystone rests on hyper's
/// actual behaviour rather than on `oneshot` being a good model of it.
///
/// This boots the production server — `axum::serve` over a bound
/// `TcpListener` — writes the resolve by hand, and then hangs up mid-turn
/// the way a proxy does when it gives up on a slow upstream. Hyper reads the
/// peer's close while the handler is still pending and drops the request,
/// which is precisely the cancellation #380's hosted tenant hit. A graceful
/// `FIN` is enough; it does not take a reset.
///
/// **The pause after the close is load-bearing.** Hyper does not learn the
/// peer is gone the instant the client calls `close` — it learns when its
/// connection task next polls the socket and reads EOF. Release the barrier
/// before that happens and the turn finishes on its own merits, so the test
/// passes whether or not the cycle is drop-safe and proves nothing. Measured
/// while building this: without the pause, the pre-fix inline code passed
/// this test; with it, the pre-fix code fails and the fix passes.
#[tokio::test]
async fn a_real_socket_close_does_not_cancel_the_follow_up_cycle() {
    use tokio::io::AsyncWriteExt;

    let home_dir = home();
    let c = stalled_company(home_dir.path()).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = c.app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let body = serde_json::json!({ "verdict": "approve" }).to_string();
    let request = format!(
        "POST /api/v1/company/approvals/{} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Cookie: {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        c.approval_id,
        crate::server::test_support::fixed_cookie("acme"),
        body.len(),
    );
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    socket.flush().await.unwrap();

    // The turn is under way — the verdict is journaled and the grant minted.
    // Now the client goes away without ever reading a response, and we wait
    // for hyper to actually notice (see the note above).
    c.entered.notified().await;
    socket.shutdown().await.unwrap();
    drop(socket);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    assert!(
        c.runtime.grants.peek(&c.approval_id).is_some(),
        "the grant was minted before the socket closed"
    );

    c.release.notify_one();
    assert!(
        await_continuation(&c.runtime).await,
        "a real peer close cancelled the follow-up cycle: the grant is spent \
         and the agent never continued"
    );
    assert_eq!(c.runtime.grants.live_count(), 1);
    server.abort();
}

/// `detach` answers on the verdict, not on the turn (issue #383).
///
/// This is the half that removes the *wait*, and with it #380's gateway
/// timeout: the response is already in the operator's hands while the agent
/// is demonstrably still mid-turn. The continuation then arrives on the
/// event stream, where the console is already subscribed.
#[tokio::test]
async fn a_detached_resolve_answers_before_the_turn_finishes() {
    let home_dir = home();
    let c = stalled_company(home_dir.path()).await;

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        c.app.clone().oneshot(resolve_request(
            &c.approval_id,
            serde_json::json!({"verdict":"approve","detach":true}),
        )),
    )
    .await
    .expect("a detached resolve must not wait on the agent turn")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        serde_json::json!({ "recorded": true, "alreadyResolved": false, "stillAwaiting": 0, "outcome": "settled" })
    );

    // The answer really did precede the work: the turn is only now under
    // way, and is still blocked.
    c.entered.notified().await;
    assert!(
        !continued(&c.runtime),
        "the turn had already finished, so this proved nothing about waiting"
    );
    assert!(
        c.runtime.grants.peek(&c.approval_id).is_some(),
        "the grant is minted before the response, not after the turn"
    );

    c.release.notify_one();
    assert!(
        await_continuation(&c.runtime).await,
        "a detached continuation must still land"
    );
    assert_eq!(c.runtime.grants.live_count(), 1);
}

/// **Issue #431.** A *detached* resolve — the verb the inline chat card
/// uses — mints a standing grant when one was asked for, and that grant is
/// visible on the one list both surfaces read.
///
/// This pairing had no coverage before this test, which is why it looked
/// covered: its neighbour above sends `detach` with no scope, so the grant
/// it asserts is the single-use one, while every standing-grant test
/// resolves *without* `detach`. Until #431 the console could not ask for
/// this combination at all, so nothing exercised it; now the chat card can,
/// it is the console's only way to mint a standing grant.
///
/// It holds because `run_resolve` computes the scope and hands it to
/// `resolve_approval_spawned` *before* it branches on `detach` — statement
/// ordering inside one function, which a refactor could reverse without a
/// single existing test going red. Hence asserting it rather than reading it.
#[tokio::test]
async fn a_detached_resolve_mints_a_standing_grant_and_lists_it() {
    let home_dir = home();
    let c = stalled_company_parking(home_dir.path(), grantable_tool_call()).await;

    // The same flag the inline card gates its scope control on: if this is
    // false the console offers no choice, and the rest of this is moot.
    assert!(
        c.runtime.pending_approvals()[0].broadly_grantable,
        "the fixture must be an approval a standing scope may be asked for"
    );

    let response = c
        .app
        .clone()
        .oneshot(resolve_request(
            &c.approval_id,
            serde_json::json!({
                "verdict": "approve",
                "detach": true,
                "scope": "tool",
                "expires_in_millis": 60 * 60 * 1000,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Minted by the time the detached answer is written, not merely promised
    // by it — the receipt says `recorded`, so a grant that appeared later
    // would make that a lie.
    assert_eq!(
        c.runtime.grants.standing_count(),
        1,
        "a detached resolve carrying a tool scope must mint a standing grant"
    );

    // And visible on the one list route both surfaces read, described the
    // same way — this is what "appears in the same list as one granted from
    // the page" means, there being only one list.
    let listed = c
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/company/grants")
                .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let bytes = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["tool"], "file_write");
    assert_eq!(rows[0]["agent"], "ceo");

    // The continuation still lands, so the scope did not cost the detach.
    c.entered.notified().await;
    c.release.notify_one();
    assert!(
        await_continuation(&c.runtime).await,
        "a detached continuation must still land when a scope was granted"
    );
}

/// The default is unchanged: no `detach` key means the response still
/// carries the follow-up cycle's messages, in the same `ChatResponse` shape
/// every existing caller parses. Only the drop-safety is new.
#[tokio::test]
async fn the_default_resolve_still_answers_with_the_cycle_response() {
    let home_dir = home();
    let c = stalled_company(home_dir.path()).await;

    // Let the turn through the moment it starts.
    c.release.notify_one();
    let response = c
        .app
        .clone()
        .oneshot(resolve_request(
            &c.approval_id,
            serde_json::json!({"verdict":"approve"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value.get("responses").is_some_and(|r| r.is_array()),
        "the un-detached body is still a ChatResponse, got {value}"
    );
    assert!(
        continued(&c.runtime),
        "the un-detached resolve waited for the turn, as it always did"
    );
    assert_eq!(c.runtime.grants.live_count(), 1);
}
