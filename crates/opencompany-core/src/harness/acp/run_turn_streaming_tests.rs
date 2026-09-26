use super::run_turn_test_fixtures::*;
use super::tests_fold::turn;
use super::*;

/// Drains the live frames a turn published, giving up once the bus goes
/// quiet — a turn that published nothing must be provable, not merely
/// unobserved, so this returns an empty vec rather than hanging.
async fn drain_live(
    stream: &mut futures::stream::BoxStream<'static, crate::turn_stream::LiveFrame>,
) -> Vec<crate::turn_stream::TurnStreamEvent> {
    use futures::StreamExt;
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await
    {
        if let Some(event) = frame.as_turn() {
            frames.push(event.clone());
        }
    }
    frames
}
#[tokio::test]
async fn a_chat_turn_streams_its_execution_state_onto_the_watching_thread() {
    // The gap this closes: an ACP turn used to be observable only once it
    // was over. On a five-minute coding turn that is indistinguishable
    // from a hang, while a `built_in` teammate beside it shows every tool
    // call as it starts.
    let company = CompanyId::new("acme-live-chat");
    let mut bus = crate::turn_stream::subscribe(&company);

    let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(a_working_turn())));
    let outcome = run_turn
        .run(&company, "ceo", "go", ChatTarget::channel(Some("design")))
        .await
        .expect("the turn answers");

    let frames = drain_live(&mut bus).await;
    let kinds: Vec<&str> = frames.iter().map(|f| f.kind).collect();
    assert_eq!(
        kinds,
        vec!["thinking", "tool_call", "tool_result"],
        "one coalesced thinking row, the call, and its completion"
    );

    // Routed to the thread that asked, and labelled with the desk that
    // answered — a frame on the wrong thread is worse than no frame.
    assert!(frames.iter().all(
        |f| f.chat_id.as_deref() == Some("design") && f.agent_id.as_deref() == Some("ceo")
    ));
    // Ordered and dedupable by the console.
    assert_eq!(
        frames.iter().map(|f| f.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    let call = &frames[1];
    assert_eq!(call.tool_call_id.as_deref(), Some("c1"));
    assert_eq!(call.label.as_deref(), Some("Read src/main.rs"));
    assert_eq!(call.status, Some("running"));

    let result = &frames[2];
    assert_eq!(
        result.tool_call_id.as_deref(),
        Some("c1"),
        "the completion pairs back to its row"
    );
    assert_eq!(result.status, Some("ok"));
    assert_eq!(result.result.as_deref(), Some("8 characters"));

    // And the live view did not replace the durable one.
    assert_eq!(outcome.reply, "done");
    assert_eq!(
        outcome
            .steps
            .iter()
            .filter(|s| s.kind == TurnStepKind::ToolCall)
            .count(),
        1,
        "the same updates still fold into the timeline that rides the reply"
    );
}

#[tokio::test]
async fn an_unaddressed_chat_turn_streams_onto_the_answering_agents_dm() {
    let company = CompanyId::new("acme-live-default");
    let mut bus = crate::turn_stream::subscribe(&company);

    let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(vec![AcpUpdate::ToolCall {
        id: "c1".into(),
        title: "Search".into(),
    }])));
    run_turn
        .run(&company, "ceo", "go", ChatTarget::default())
        .await
        .expect("the turn answers");

    let frames = drain_live(&mut bus).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].chat_id.as_deref(), Some("dm:ceo"));
}

#[tokio::test]
async fn a_dispatched_card_turn_streams_nothing_onto_the_console() {
    // A dispatched card shows no chat bubble and its steps are folded into
    // the card's own note. Streaming them would put rows on whatever
    // thread most recently sent — the misattribution `LiveStream::Off`
    // exists to prevent.
    let company = CompanyId::new("acme-live-card");
    let mut bus = crate::turn_stream::subscribe(&company);

    let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(a_working_turn())));
    let control = crate::company::steer::SteerControl::new();
    run_turn
        .run_steered_background(&company, "ceo", "go", &control, ChatTarget::default(), None)
        .await
        .expect("the turn answers");

    assert!(
        drain_live(&mut bus).await.is_empty(),
        "a background turn publishes nothing"
    );
}

#[tokio::test]
async fn a_workflow_node_streams_onto_its_run_rather_than_a_desk() {
    // The trait default for `run_background_workflow` forwards to `run`
    // with no chat id — which, now that `run` streams, would publish a
    // node's tool calls onto the DEFAULT DESK. This asserts the override
    // that keeps them on the run-trace sheet instead.
    let company = CompanyId::new("acme-live-workflow");
    let mut bus = crate::turn_stream::subscribe(&company);

    let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(vec![AcpUpdate::ToolCall {
        id: "c1".into(),
        title: "Fetch".into(),
    }])));
    run_turn
        .run_background_workflow(&company, "ceo", "go", None, "run-7", "node-2")
        .await
        .expect("the turn answers");

    let frames = drain_live(&mut bus).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].workflow_run_id.as_deref(), Some("run-7"));
    assert_eq!(frames[0].node_id.as_deref(), Some("node-2"));
    assert!(
        frames[0].chat_id.is_none(),
        "a node has no chat thread to attribute to"
    );
}

/// How many rows a console holding these frames ends up showing.
///
/// A tool call is two frames and one row: `tool_call` opens it and
/// `tool_result` flips that same row in place, paired by `toolCallId`
/// (`app-shell.tsx`'s `onTurnEvent`). Counting frames instead of rows
/// would make the live view look like it shows twice the work.
fn rendered_rows(frames: &[TurnStreamEvent]) -> usize {
    let paired: std::collections::HashSet<&str> = frames
        .iter()
        .filter_map(|f| f.tool_call_id.as_deref())
        .collect();
    paired.len() + frames.iter().filter(|f| f.tool_call_id.is_none()).count()
}

#[test]
fn the_live_rows_and_the_folded_steps_stay_in_step() {
    // The two views are the same updates read twice, and the property that
    // matters is that neither invents or drops a row the other has. A
    // non-terminal `tool_call_update` is the one that could: it leaves the
    // folded step `Running` and must publish no second row.
    let updates = a_working_turn();
    let mut state = LiveState::default();
    let frames: Vec<_> = updates
        .iter()
        .filter_map(|u| live_frame_from(u, &mut state))
        .collect();

    let outcome = fold(turn(updates));
    assert_eq!(
        rendered_rows(&frames),
        outcome.steps.len(),
        "one live row per folded step: {frames:?} vs {:?}",
        outcome.steps
    );
    assert_eq!(
        frames.iter().filter(|f| f.kind == "tool_call").count(),
        outcome
            .steps
            .iter()
            .filter(|s| s.kind == TurnStepKind::ToolCall)
            .count()
    );
}

#[tokio::test]
async fn a_completion_for_a_call_nobody_saw_start_publishes_no_row() {
    // `fold` drops these ("a step with no label is worse on a timeline
    // than no step"), so the live view must too — a row that appears while
    // the turn runs and is missing from the finished timeline reads as
    // work that was undone.
    let company = CompanyId::new("acme-live-ghost");
    let mut bus = crate::turn_stream::subscribe(&company);

    let run_turn = AcpRunTurn::new(Arc::new(Scripted::answering(vec![
        AcpUpdate::ToolCallUpdate {
            id: "ghost".into(),
            status: "completed".into(),
            result: Some("x".into()),
        },
    ])));
    let outcome = run_turn
        .run(&company, "ceo", "go", ChatTarget::channel(Some("design")))
        .await
        .expect("the turn answers");

    assert!(drain_live(&mut bus).await.is_empty());
    assert!(
        outcome
            .steps
            .iter()
            .all(|s| s.kind != TurnStepKind::ToolCall)
    );
}

#[test]
fn thinking_around_assistant_text_folds_and_streams_the_same_way() {
    // The divergence PR #1904's review caught: the live mapper closed a
    // thinking run on assistant text and `fold` did not, so this sequence
    // streamed two `Thinking` rows and folded one — the second row
    // vanishing the moment the reply replaced the live timeline.
    let updates = vec![
        AcpUpdate::ThoughtChunk,
        AcpUpdate::MessageChunk("partly there. ".into()),
        AcpUpdate::ThoughtChunk,
        AcpUpdate::MessageChunk("done".into()),
    ];

    let mut state = LiveState::default();
    let live = updates
        .iter()
        .filter_map(|u| live_frame_from(u, &mut state))
        .count();

    let outcome = fold(turn(updates));
    let folded = outcome
        .steps
        .iter()
        .filter(|s| s.kind == TurnStepKind::Thinking)
        .count();

    assert_eq!(folded, 2, "text closes a thinking run, so this is two");
    assert_eq!(live, folded, "and the live view says the same");
    assert_eq!(outcome.reply, "partly there. done");
}

#[test]
fn a_burst_of_thoughts_is_one_row_until_something_else_happens() {
    // A model emits these by the hundred; a timeline of them is noise.
    // Mirrors `fold`'s own coalescing so the live view does not show a
    // different number of thinking rows than the finished one.
    let mut state = LiveState::default();
    let thoughts = vec![AcpUpdate::ThoughtChunk; 5];
    let frames: Vec<_> = thoughts
        .iter()
        .filter_map(|u| live_frame_from(u, &mut state))
        .collect();
    assert_eq!(frames.len(), 1);

    // Text closes the run, so the next thought opens a new row — exactly
    // what `fold` does with its own `thinking` flag.
    assert!(live_frame_from(&AcpUpdate::MessageChunk("hi".into()), &mut state).is_none());
    assert!(live_frame_from(&AcpUpdate::ThoughtChunk, &mut state).is_some());
}
