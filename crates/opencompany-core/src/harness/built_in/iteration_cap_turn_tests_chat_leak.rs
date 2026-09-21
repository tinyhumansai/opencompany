use super::*;

// ---------------------------------------------------------------------------
// Issue #1725 — the greeting fast-path + context-isolation regression.
//
// The direct reproduction of the screenshot bug, end to end through the real
// `CompanyAgent::run_with_steer` turn path against the scripted model: a task
// that fetched content (the "sport story ranking" HTML) leaves the agent's
// in-memory history full, and a bare "hi" on a NEW chat used to run the whole
// agentic loop AND reply against the prior task's replayed content. The four
// unit tests cover the mechanisms individually (per-turn tool/memory/goal
// suppression, the greeting classifier, the chat-only hint, per-conversation
// history); this is the one test that exercises them together and asserts the
// observable symptoms the screenshot showed.
// ---------------------------------------------------------------------------

/// A live turn-stream routing context for `chat`, so the pool binds this
/// agent's history to that thread (issue #1725).
fn stream_for(chat: &str) -> crate::turn_stream::TurnStreamCtx {
    crate::turn_stream::TurnStreamCtx {
        company: CompanyId::new("acme"),
        agent_id: "ceo".to_string(),
        route: crate::turn_stream::LiveRoute::Chat {
            chat_id: chat.to_string(),
        },
        message_seq: None,
    }
}

/// A task fetches content and sets the agent's context; then a bare "hi" on a
/// different chat must run **no** tools, open no loop, and carry **nothing**
/// from the prior task. Reverting the fix (see the module note) makes the "hi"
/// turn offer its tools and replay the fetched content — the screenshot bug.
#[tokio::test]
async fn a_greeting_after_a_task_runs_no_tools_and_leaks_no_prior_context() {
    // A body distinctive enough that its presence in a later turn's model
    // request is unambiguous — this stands in for the replayed ranking HTML.
    const FETCHED: &str = "SPORTBALL_RANKING_HTML_MARKER_9F3A";

    let (model_url, script) = spawn_script(
        vec![
            // Task A: read the ranking note (a tool round), then answer.
            Turn::Call {
                tool: "file_read".to_string(),
                args: json!({ "path": "note-00.md" }),
            },
            Turn::Say("Ranked the sport stories."),
            // The bare greeting on a fresh chat: one plain reply, no tool call
            // scripted — so if the turn tries to loop it runs off the script.
            Turn::Say("Hi! How can I help you today?"),
        ],
        12,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, 1).await;
    // Overwrite the seeded note with the distinctive fetched body.
    let workspace = agent_workspace(dir.path(), &CompanyId::new("acme"), "ceo");
    std::fs::write(
        workspace.join("note-00.md"),
        format!("{FETCHED}\n<html>ranked sport stories</html>\n"),
    )
    .expect("seed the fetched note");

    // ── Task A on chat "sports": a real work turn — tools attach and run. ──
    let (outcome_a, _usage_a) = agent
        .run_with_steer(
            "rank the sport stories and read the ranking html",
            None,
            Some(stream_for("sports")),
            None,
            None,
            ChatTarget::channel(Some("sports")),
        )
        .await;
    let outcome_a = outcome_a.expect("task A runs");
    assert!(
        !outcome_a.steps.is_empty(),
        "task A must actually run a tool step (the fetch) — otherwise the \
         isolation below proves nothing"
    );
    {
        let seen = script.seen.lock().unwrap();
        // The fetched content really entered the model's context on task A.
        assert!(
            seen.iter().any(|r| r.to_string().contains(FETCHED)),
            "task A's fetched content must reach the model on its own turn"
        );
        // And task A was offered its tools (the contrast the greeting breaks).
        let a_tools = seen
            .first()
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        assert!(a_tools > 0, "a real work turn must be offered its tools");
    }

    let calls_before = model_calls(&script);

    // ── A bare "hi" on a DIFFERENT chat — the greeting fast path. ──
    let (outcome_b, _usage_b) = with_chat_only_hint(
        true,
        agent.run_with_steer(
            "hi",
            None,
            Some(stream_for("smalltalk")),
            None,
            None,
            ChatTarget::channel(Some("smalltalk")),
        ),
    )
    .await;
    let outcome_b = outcome_b.expect("the greeting runs");

    // 1) Zero tool steps ran — the greeting never entered the agentic loop.
    assert!(
        outcome_b.steps.is_empty(),
        "a greeting must run no tool steps, got {:?}",
        outcome_b.steps
    );
    // 2) Exactly one model call — no tool-loop iterations.
    assert_eq!(
        model_calls(&script) - calls_before,
        1,
        "the greeting must be a single model call, not a loop"
    );

    let greeting_req = script
        .seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the greeting produced a model request");

    // 3) The greeting turn was offered NO tools (suppress_tools).
    let greeting_tools = greeting_req
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        greeting_tools, 0,
        "a chat-only turn must be sent an empty tool schema"
    );

    // 4) NOTHING from task A leaked into the greeting's context — no replayed
    //    fetched HTML, no prior-task active-goal block. This is the screenshot
    //    bug, asserted directly.
    let greeting_str = greeting_req.to_string();
    assert!(
        !greeting_str.contains(FETCHED),
        "task A's fetched content must NOT replay into an unrelated greeting"
    );
    assert!(
        !greeting_str.contains("[active_goal]"),
        "no prior task's goal may steer the greeting"
    );

    // 5) It still answered — abstain-or-reduce, never a silent non-answer.
    assert!(
        !outcome_b.reply.trim().is_empty(),
        "the greeting still gets a reply"
    );
}

/// A background task (`stream: None` — the same shape `run_background` and
/// `run_steered_background` hand `run_with_steer`, since neither carries a
/// chat thread) must not leave the agent bound to whichever chat happened to
/// stream last. Reverting the unthreaded-turn branch in `run_with_steer`
/// leaves `bound_chat` pointed at "sports" after the background turn runs, so
/// the operator's SECOND turn on that same chat reads `switched == false`,
/// skips the clear-and-reseed, and inherits the background task's fetched
/// content — the cross-context leak review found on #1725.
#[tokio::test]
async fn a_background_turn_does_not_leak_into_the_next_turn_on_its_bound_chat() {
    const FETCHED: &str = "BACKGROUND_TASK_MARKER_71B2";

    let (model_url, script) = spawn_script(
        vec![
            // Chat "sports", turn 1: a plain reply — binds bound_chat to "sports".
            Turn::Say("Sure, tracking the sports desk."),
            // The background task: a tool round, then an answer.
            Turn::Call {
                tool: "file_read".to_string(),
                args: json!({ "path": "note-00.md" }),
            },
            Turn::Say("Background task done."),
            // Chat "sports", turn 2 — the SAME chat id as turn 1.
            Turn::Say("Sounds good."),
        ],
        12,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let agent = company_agent(model_url, dir.path(), None, 1).await;
    // Overwrite the seeded note with the distinctive background-task body.
    let workspace = agent_workspace(dir.path(), &CompanyId::new("acme"), "ceo");
    std::fs::write(
        workspace.join("note-00.md"),
        format!("{FETCHED}\n<html>background task content</html>\n"),
    )
    .expect("seed the fetched note");

    // ── Chat "sports", turn 1: binds bound_chat to "sports". ──
    agent
        .run_with_steer(
            "hello from sports",
            None,
            Some(stream_for("sports")),
            None,
            None,
            ChatTarget::channel(Some("sports")),
        )
        .await
        .0
        .expect("chat turn 1 runs");

    let background_request_index = model_calls(&script);

    // ── The background task: unthreaded — `stream: None`, same shared Agent. ──
    let (outcome_bg, _usage_bg) = agent
        .run_with_steer(
            "run the background task",
            None,
            None,
            None,
            None,
            // A background task names no conversation — the point of the case.
            ChatTarget::default(),
        )
        .await;
    let outcome_bg = outcome_bg.expect("background task runs");
    assert!(
        !outcome_bg.steps.is_empty(),
        "the background task must actually run a tool step (the fetch) — \
         otherwise the isolation below proves nothing"
    );
    {
        let seen = script.seen.lock().unwrap();
        let request = seen[background_request_index].to_string();
        assert!(!request.contains("hello from sports"));
        assert!(!request.contains("Sure, tracking the sports desk."));
        assert!(
            seen.iter().any(|r| r.to_string().contains(FETCHED)),
            "the background task's fetched content must reach the model on \
             its own turn"
        );
    }

    let calls_before = model_calls(&script);

    // ── Chat "sports", turn 2 — same chat id the background task ran under
    //    no binding for, so this must be treated as a switch and re-seed. ──
    agent
        .run_with_steer(
            "still there?",
            None,
            Some(stream_for("sports")),
            None,
            None,
            ChatTarget::channel(Some("sports")),
        )
        .await
        .0
        .expect("chat turn 2 runs");

    assert_eq!(
        model_calls(&script) - calls_before,
        1,
        "chat turn 2 must be a single model call, not a loop"
    );

    let turn2_req = script
        .seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("chat turn 2 produced a model request");

    assert!(
        !turn2_req.to_string().contains(FETCHED),
        "the background task's fetched content must NOT leak into the next \
         turn on the chat it happened to be bound to before the background \
         task ran"
    );
}

#[tokio::test]
async fn self_contained_delegation_skips_retrieved_task_outcomes() {
    let (model_url, script) = spawn_script(vec![Turn::Say("Assigned slice completed.")], 4).await;
    let dir = tempfile::tempdir().unwrap();
    let dependencies = deps(model_url.clone(), dir.path());
    let agent = company_agent(model_url, dir.path(), None, 1).await;
    let company = CompanyId::new("acme");
    dependencies
        .context
        .put(
            &company,
            crate::ports::types::ContextChunk {
                label: "task-outcome/ceo".into(),
                body: "UNRELATED_PRIOR_TASK_MARKER Read the budget regression note".into(),
            },
        )
        .await
        .unwrap();
    let hits = dependencies
        .context
        .search(&company, "Read the budget regression note", 5)
        .await
        .unwrap();
    assert!(
        !hits.is_empty(),
        "the test must have retrievable prior context"
    );
    let pool = super::super::HarnessPool::new();
    pool.agents
        .write()
        .await
        .insert(company.clone(), vec![Arc::new(agent)]);
    let result = pool
        .run(
            &company,
            "ceo",
            "Read the budget regression note",
            &dependencies,
            ChatTarget::deliberating(Some("delegation"), None),
        )
        .await
        .unwrap();
    assert_eq!(result.reply, "Assigned slice completed.");
    let seen = script.seen.lock().unwrap();
    assert!(!seen[0].to_string().contains("UNRELATED_PRIOR_TASK_MARKER"));
}
