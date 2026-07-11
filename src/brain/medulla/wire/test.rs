//! Round-trip and mapping tests for the `/orchestration/v1` wire types.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::*;
use crate::error::OpenCompanyError;

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + DeserializeOwned,
{
    let json = serde_json::to_string(value).expect("serialize");
    serde_json::from_str(&json).expect("deserialize")
}

// ---------------------------------------------------------------------------
// Envelope + events
// ---------------------------------------------------------------------------

fn sample_events_request() -> EventsRequest {
    EventsRequest {
        counterpart_agent_id: "opencompany:acme".to_string(),
        session_id: "01SESSION".to_string(),
        event: WireEvent {
            seq: 7,
            role: Role::User,
            sender: "operator".to_string(),
            body: "ship it".to_string(),
            ts: 1_700_000_000,
            kind: "operator.message".to_string(),
        },
    }
}

#[test]
fn events_request_round_trips_with_protocol_one() {
    let req = sample_events_request();
    let envelope = Envelope::v1(req.clone());
    let json = serde_json::to_value(&envelope).unwrap();

    // protocol:1 is present and the camelCase fields are flattened alongside it.
    assert_eq!(json["protocol"], 1);
    assert_eq!(json["counterpartAgentId"], "opencompany:acme");
    assert_eq!(json["sessionId"], "01SESSION");
    assert_eq!(json["event"]["role"], "user");

    let back: Envelope<EventsRequest> = serde_json::from_value(json).unwrap();
    assert_eq!(back.protocol, PROTOCOL);
    assert_eq!(back.body, req);
}

#[test]
fn wire_event_role_serializes_lowercase() {
    for (role, wire) in [
        (Role::User, "user"),
        (Role::Assistant, "assistant"),
        (Role::System, "system"),
    ] {
        assert_eq!(serde_json::to_value(role).unwrap(), json!(wire));
        assert_eq!(round_trip(&role), role);
    }
}

#[test]
fn events_accepted_round_trips_camel_case() {
    let accepted = EventsAccepted {
        accepted: true,
        cycle_id: "cyc:opencompany:acme:01SESSION:7".to_string(),
    };
    let json = serde_json::to_value(&accepted).unwrap();
    assert_eq!(json["cycleId"], "cyc:opencompany:acme:01SESSION:7");
    assert_eq!(round_trip(&accepted), accepted);
}

#[test]
fn cycle_id_matches_spec_format_and_is_idempotent() {
    let id = cycle_id("opencompany:acme", "01SESSION", 7);
    assert_eq!(id, "cyc:opencompany:acme:01SESSION:7");
    // Same coordinates → same id (the idempotency key).
    assert_eq!(id, cycle_id("opencompany:acme", "01SESSION", 7));
    // A different seq yields a different id.
    assert_ne!(id, cycle_id("opencompany:acme", "01SESSION", 8));
}

// ---------------------------------------------------------------------------
// world-diff
// ---------------------------------------------------------------------------

#[test]
fn world_diff_round_trips_and_validates_bounds() {
    let req = WorldDiffRequest {
        session_id: "01SESSION".to_string(),
        entries: vec![WorldDiffEntry {
            seq: 1,
            note: "closed a deal".to_string(),
            ts: 42,
        }],
    };
    let json = serde_json::to_value(Envelope::v1(req.clone())).unwrap();
    assert_eq!(json["protocol"], 1);
    assert_eq!(json["sessionId"], "01SESSION");
    assert_eq!(round_trip(&req), req);
    assert!(req.validate().is_ok());

    let accepted = WorldDiffAccepted {
        accepted: true,
        duplicates: 2,
        tick_scheduled: true,
    };
    let acc_json = serde_json::to_value(&accepted).unwrap();
    assert_eq!(acc_json["tickScheduled"], true);
    assert_eq!(round_trip(&accepted), accepted);
}

#[test]
fn world_diff_rejects_empty_and_oversized() {
    let empty = WorldDiffRequest {
        session_id: "s".to_string(),
        entries: vec![],
    };
    assert_orch_code(&empty.validate().unwrap_err(), "ORCH_VALIDATION_ERROR");

    let too_many = WorldDiffRequest {
        session_id: "s".to_string(),
        entries: (0..(WORLD_DIFF_MAX_ENTRIES as u64 + 1))
            .map(|seq| WorldDiffEntry {
                seq,
                note: "n".to_string(),
                ts: 0,
            })
            .collect(),
    };
    assert_orch_code(&too_many.validate().unwrap_err(), "ORCH_VALIDATION_ERROR");

    let long_note = WorldDiffRequest {
        session_id: "s".to_string(),
        entries: vec![WorldDiffEntry {
            seq: 0,
            note: "x".repeat(WORLD_DIFF_MAX_NOTE + 1),
            ts: 0,
        }],
    };
    assert_orch_code(&long_note.validate().unwrap_err(), "ORCH_VALIDATION_ERROR");
}

// ---------------------------------------------------------------------------
// Read surface
// ---------------------------------------------------------------------------

#[test]
fn read_surface_views_round_trip() {
    let summary = SessionSummary {
        session_id: "s".to_string(),
        status: "running".to_string(),
        last_seq: 12,
    };
    assert_eq!(
        serde_json::to_value(&summary).unwrap()["lastSeq"],
        json!(12)
    );
    assert_eq!(round_trip(&summary), summary);

    let message = MessageView {
        seq: 3,
        role: Role::Assistant,
        sender: "acme".to_string(),
        body: "done".to_string(),
        ts: 1,
        kind: "chat".to_string(),
    };
    assert_eq!(round_trip(&message), message);

    let state = SessionState {
        session_id: "s".to_string(),
        status: "running".to_string(),
        last_seq: 9,
        last_cycle_id: Some("cyc:a:s:9".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&state).unwrap()["lastCycleId"],
        json!("cyc:a:s:9")
    );
    assert_eq!(round_trip(&state), state);

    let steering = SteeringView {
        active: Some(SteeringDirective {
            directive: "prioritize refunds".to_string(),
            ts: 5,
        }),
        history: vec![SteeringDirective {
            directive: "be terse".to_string(),
            ts: 1,
        }],
    };
    assert_eq!(round_trip(&steering), steering);

    let diff = WorldDiffView {
        seq: 2,
        note: "paid vendor".to_string(),
        ts: 3,
    };
    assert_eq!(round_trip(&diff), diff);
}

#[test]
fn session_state_omits_absent_last_cycle_id() {
    let state = SessionState {
        session_id: "s".to_string(),
        status: "idle".to_string(),
        last_seq: 0,
        last_cycle_id: None,
    };
    let json = serde_json::to_value(&state).unwrap();
    assert!(json.get("lastCycleId").is_none());
}

// ---------------------------------------------------------------------------
// Socket frames
// ---------------------------------------------------------------------------

#[test]
fn register_tools_frame_round_trips_with_input_schema() {
    let frame = RegisterToolsFrame {
        tools: vec![
            ToolManifestEntry {
                name: "context_search".to_string(),
                description: Some("search context".to_string()),
                input_schema: Some(json!({"type": "object"})),
            },
            ToolManifestEntry {
                name: "bare".to_string(),
                description: None,
                input_schema: None,
            },
        ],
    };
    let json = serde_json::to_value(&frame).unwrap();
    assert_eq!(json["tools"][0]["inputSchema"], json!({"type": "object"}));
    // Absent optionals are omitted, not null.
    assert!(json["tools"][1].get("description").is_none());
    assert!(json["tools"][1].get("inputSchema").is_none());
    assert_eq!(round_trip(&frame), frame);
}

#[test]
fn effect_frame_round_trips_through_spread_wire_body() {
    let frame = EffectFrame {
        kind: "send_dm".to_string(),
        cycle_id: "cyc:a:s:1".to_string(),
        call_id: call_id("cyc:a:s:1", "send_dm", 0),
        payload: json!({"channel": "operator", "text": "hi"}),
    };
    assert_eq!(frame.event_name(), "orch:effect:send_dm");

    // The wire body spreads the payload alongside cycleId/callId.
    let body = frame.to_wire_body();
    assert_eq!(body["cycleId"], "cyc:a:s:1");
    assert_eq!(body["callId"], "cyc:a:s:1:send_dm:0");
    assert_eq!(body["channel"], "operator");
    assert_eq!(body["text"], "hi");

    let back = EffectFrame::from_wire_body("send_dm", body).unwrap();
    assert_eq!(back, frame);
}

#[test]
fn effect_frame_preserves_non_object_payload() {
    let frame = EffectFrame {
        kind: "noop".to_string(),
        cycle_id: "cyc:a:s:2".to_string(),
        call_id: call_id("cyc:a:s:2", "noop", 0),
        payload: json!("scalar-payload"),
    };
    let back = EffectFrame::from_wire_body("noop", frame.to_wire_body()).unwrap();
    assert_eq!(back, frame);
}

#[test]
fn call_id_matches_spec_and_dedupes_by_index() {
    let id = call_id("cyc:a:s:1", "send_dm", 0);
    assert_eq!(id, "cyc:a:s:1:send_dm:0");
    // Same (cycle, kind, index) → identical id (dedupe key).
    assert_eq!(id, call_id("cyc:a:s:1", "send_dm", 0));
    // A different index → a different id.
    assert_ne!(id, call_id("cyc:a:s:1", "send_dm", 1));
    // A different kind → a different id.
    assert_ne!(id, call_id("cyc:a:s:1", "publish", 0));
}

#[test]
fn effect_and_tool_frames_round_trip() {
    let result = EffectResult {
        call_id: "cyc:a:s:1:send_dm:0".to_string(),
        ok: false,
        error: Some("pending approval".to_string()),
        result: None,
    };
    assert_eq!(
        serde_json::to_value(&result).unwrap()["callId"],
        json!("cyc:a:s:1:send_dm:0")
    );
    assert_eq!(round_trip(&result), result);

    let call = ToolCallFrame {
        cycle_id: "cyc:a:s:1".to_string(),
        call_id: "cyc:a:s:1:tool:0".to_string(),
        name: "context_search".to_string(),
        args: json!({"query": "invoices"}),
        timeout_ms: DEFAULT_TOOL_TIMEOUT_MS,
    };
    assert_eq!(
        serde_json::to_value(&call).unwrap()["timeoutMs"],
        json!(30_000)
    );
    assert_eq!(round_trip(&call), call);

    let answer = ToolResultFrame {
        call_id: "cyc:a:s:1:tool:0".to_string(),
        ok: true,
        result: Some(json!({"hits": []})),
        error: None,
    };
    assert_eq!(round_trip(&answer), answer);
}

#[test]
fn socket_event_name_constants_and_helper() {
    assert_eq!(REGISTER_TOOLS, "orch:register_tools");
    assert_eq!(EFFECT_RESULT, "orch:effect:result");
    assert_eq!(TOOL_CALL, "orch:tool_call");
    assert_eq!(TOOL_RESULT, "orch:tool_result");
    assert_eq!(effect_event_name("send_dm"), "orch:effect:send_dm");
    assert_eq!(effect_event_name("publish"), "orch:effect:publish");
}

// ---------------------------------------------------------------------------
// Response envelope + error codes
// ---------------------------------------------------------------------------

#[test]
fn success_envelope_decodes_to_data() {
    let body = json!({
        "success": true,
        "data": { "accepted": true, "cycleId": "cyc:a:s:1" }
    });
    let resp: ApiResponse<EventsAccepted> = serde_json::from_value(body).unwrap();
    let accepted = resp.into_result().unwrap();
    assert_eq!(accepted.cycle_id, "cyc:a:s:1");
}

#[test]
fn error_envelope_maps_all_nine_codes() {
    let codes = [
        ("ORCH_PROTOCOL_MISMATCH", OrchErrorCode::ProtocolMismatch),
        ("ORCH_MODEL_NOT_ALLOWED", OrchErrorCode::ModelNotAllowed),
        ("ORCH_VALIDATION_ERROR", OrchErrorCode::ValidationError),
        (
            "ORCH_INSUFFICIENT_BALANCE",
            OrchErrorCode::InsufficientBalance,
        ),
        ("ORCH_RATE_LIMITED", OrchErrorCode::RateLimited),
        (
            "ORCH_UPSTREAM_MODEL_ERROR",
            OrchErrorCode::UpstreamModelError,
        ),
        ("ORCH_INVALID_STATE", OrchErrorCode::InvalidState),
        ("ORCH_DEVICE_OFFLINE", OrchErrorCode::DeviceOffline),
        ("ORCH_EXECUTE_TIMEOUT", OrchErrorCode::ExecuteTimeout),
    ];
    for (wire, expected) in codes {
        assert_eq!(OrchErrorCode::from_wire(wire), expected);
        assert_eq!(expected.as_str(), wire);

        let body = json!({ "success": false, "error": "boom", "errorCode": wire });
        let resp: ApiResponse<EventsAccepted> = serde_json::from_value(body).unwrap();
        let err = resp.into_result().unwrap_err();
        assert_orch_code(&err, wire);
    }
}

#[test]
fn unknown_error_code_is_preserved() {
    assert_eq!(
        OrchErrorCode::from_wire("ORCH_FUTURE"),
        OrchErrorCode::Unknown("ORCH_FUTURE".to_string())
    );
    let body = json!({ "success": false, "error": "boom", "errorCode": "ORCH_FUTURE" });
    let resp: ApiResponse<EventsAccepted> = serde_json::from_value(body).unwrap();
    assert_orch_code(&resp.into_result().unwrap_err(), "ORCH_FUTURE");
}

#[test]
fn protocol_mismatch_surfaces_min_max_details() {
    let body = json!({
        "success": false,
        "error": "protocol out of range",
        "errorCode": "ORCH_PROTOCOL_MISMATCH",
        "details": { "min": 1, "max": 1 }
    });
    let resp: ApiResponse<EventsAccepted> = serde_json::from_value(body).unwrap();

    // The structured detail is available on the decoded envelope.
    let details = resp.details.clone().unwrap();
    assert_eq!(details["min"], 1);
    assert_eq!(details["max"], 1);

    // And the mapped error carries the code with the details folded in.
    let err = resp.into_result().unwrap_err();
    assert_orch_code(&err, "ORCH_PROTOCOL_MISMATCH");
    let OpenCompanyError::Orchestration { message, .. } = &err else {
        panic!("expected orchestration error");
    };
    assert!(message.contains("\"min\":1"), "message was {message}");
    assert!(message.contains("\"max\":1"), "message was {message}");

    // Advertised protocol range constants line up with the spec.
    assert_eq!((PROTOCOL_MIN, PROTOCOL_MAX), (1, 1));
}

// ---------------------------------------------------------------------------
// model-field guard
// ---------------------------------------------------------------------------

#[test]
fn assert_no_model_flags_top_level_and_nested() {
    assert_orch_code(
        &assert_no_model(&json!({ "model": "gpt" })).unwrap_err(),
        "ORCH_MODEL_NOT_ALLOWED",
    );
    assert_orch_code(
        &assert_no_model(&json!({ "event": { "model": "claude" } })).unwrap_err(),
        "ORCH_MODEL_NOT_ALLOWED",
    );
    assert_orch_code(
        &assert_no_model(&json!({ "list": [ { "model": "x" } ] })).unwrap_err(),
        "ORCH_MODEL_NOT_ALLOWED",
    );
}

#[test]
fn assert_no_model_passes_clean_bodies() {
    let body = serde_json::to_value(Envelope::v1(sample_events_request())).unwrap();
    assert!(assert_no_model(&body).is_ok());
    // The typed events body never serializes a `model` key.
    assert!(!body.to_string().contains("\"model\""));
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn assert_orch_code(err: &OpenCompanyError, expected: &str) {
    match err {
        OpenCompanyError::Orchestration { code, .. } => {
            assert_eq!(code, expected, "unexpected orchestration code");
            // The HTTP envelope surfaces the verbatim ORCH_* code.
            assert_eq!(err.code(), expected);
        }
        other => panic!("expected orchestration error, got {other:?}"),
    }
}
