//! Tests driving [`MockTransport`] through the [`MedullaTransport`] seam.

use super::*;
use crate::brain::medulla::wire::{
    EffectFrame, Role, ToolCallFrame, WireEvent, WorldDiffEntry, call_id,
};

fn events_request(seq: u64) -> EventsRequest {
    EventsRequest {
        counterpart_agent_id: "opencompany:acme".to_string(),
        session_id: "01SESSION".to_string(),
        event: WireEvent {
            seq,
            role: Role::User,
            sender: "operator".to_string(),
            body: "hi".to_string(),
            ts: 0,
            kind: "operator.message".to_string(),
        },
    }
}

#[tokio::test]
async fn post_events_computes_cycle_id_and_records() {
    let mock = MockTransport::new();
    let accepted = mock.post_events(events_request(7)).await.unwrap();
    assert!(accepted.accepted);
    assert_eq!(accepted.cycle_id, "cyc:opencompany:acme:01SESSION:7");

    let posted = mock.posted_events();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].event.seq, 7);
}

#[tokio::test]
async fn post_events_error_injection_maps_to_orchestration() {
    let mock = MockTransport::new();
    mock.fail_post_events(OrchErrorCode::InsufficientBalance);
    let err = mock.post_events(events_request(1)).await.unwrap_err();
    assert_eq!(err.code(), "ORCH_INSUFFICIENT_BALANCE");
    // The failed post is not recorded as accepted.
    assert!(mock.posted_events().is_empty());
}

#[tokio::test]
async fn cycle_frames_replays_plan_then_completes() {
    let mock = MockTransport::new();
    let cycle = "cyc:opencompany:acme:01SESSION:1";
    let effect = EffectFrame {
        kind: "send_dm".to_string(),
        cycle_id: cycle.to_string(),
        call_id: call_id(cycle, "send_dm", 0),
        payload: serde_json::json!({"channel": "operator", "text": "hi"}),
    };
    let tool = ToolCallFrame {
        cycle_id: cycle.to_string(),
        call_id: call_id(cycle, "tool", 0),
        name: "context_search".to_string(),
        args: serde_json::json!({"query": "x"}),
        timeout_ms: 30_000,
    };
    // No trailing CycleComplete: the mock appends one.
    mock.script_cycle(
        cycle,
        vec![
            InboundFrame::Effect(effect.clone()),
            InboundFrame::ToolCall(tool.clone()),
        ],
    );

    let frames: Vec<_> = mock.cycle_frames(cycle).map(|f| f.unwrap()).collect().await;
    assert_eq!(
        frames,
        vec![
            InboundFrame::Effect(effect),
            InboundFrame::ToolCall(tool),
            InboundFrame::CycleComplete,
        ]
    );
}

#[tokio::test]
async fn unscripted_cycle_yields_only_completion() {
    let mock = MockTransport::new();
    let frames: Vec<_> = mock
        .cycle_frames("cyc:unknown")
        .map(|f| f.unwrap())
        .collect()
        .await;
    assert_eq!(frames, vec![InboundFrame::CycleComplete]);
}

#[tokio::test]
async fn records_acks_answers_and_registrations() {
    let mock = MockTransport::new();

    mock.register_tools(vec![ToolManifestEntry {
        name: "context_search".to_string(),
        description: None,
        input_schema: None,
    }])
    .await
    .unwrap();

    mock.ack_effect(EffectResult {
        call_id: "cyc:a:s:1:send_dm:0".to_string(),
        ok: true,
        error: None,
        result: None,
    })
    .await
    .unwrap();

    mock.answer_tool_call(ToolResultFrame {
        call_id: "cyc:a:s:1:tool:0".to_string(),
        ok: true,
        result: Some(serde_json::json!({"hits": []})),
        error: None,
    })
    .await
    .unwrap();

    assert_eq!(mock.registered_tools().len(), 1);
    assert_eq!(mock.registered_tools()[0][0].name, "context_search");
    assert_eq!(mock.acks().len(), 1);
    assert!(mock.acks()[0].ok);
    assert_eq!(mock.tool_answers().len(), 1);
    assert_eq!(mock.tool_answers()[0].call_id, "cyc:a:s:1:tool:0");
}

#[tokio::test]
async fn world_diff_is_validated_and_recorded() {
    let mock = MockTransport::new();
    let req = WorldDiffRequest {
        session_id: "01SESSION".to_string(),
        entries: vec![WorldDiffEntry {
            seq: 1,
            note: "shipped".to_string(),
            ts: 0,
        }],
    };
    let accepted = mock.post_world_diff(req).await.unwrap();
    assert!(accepted.accepted);
    assert!(accepted.tick_scheduled);
    assert_eq!(mock.posted_world_diffs().len(), 1);

    // An invalid batch is rejected before recording.
    let empty = WorldDiffRequest {
        session_id: "s".to_string(),
        entries: vec![],
    };
    assert_eq!(
        mock.post_world_diff(empty).await.unwrap_err().code(),
        "ORCH_VALIDATION_ERROR"
    );
    assert_eq!(mock.posted_world_diffs().len(), 1);
}
