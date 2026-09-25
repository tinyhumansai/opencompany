use super::steps_fixtures_tests::*;
use super::*;

// #411: what came back
// -----------------------------------------------------------------------

/// "How far have we come" was unanswerable when a success was a name and a
/// duration. A collection answers it with a count; anything else with a
/// size. Neither carries content.
#[test]
fn a_success_summarises_what_came_back_without_its_content() {
    let list = one(
        "mcp_call_tool",
        true,
        r#"[{"id":1},{"id":2},{"id":3}]"#,
        Some(serde_json::json!({ "server": "github", "tool": "list_issues" })),
    );
    assert_eq!(list.result.as_deref(), Some("3 items"));

    let object = one("mcp_call_tool", true, r#"{"a":1,"b":2}"#, None);
    assert_eq!(object.result.as_deref(), Some("2 fields"));

    let prose = one("mcp_call_tool", true, "a plain sentence came back", None);
    assert_eq!(prose.result.as_deref(), Some("26 characters"));

    let big = one("mcp_call_tool", true, &"x".repeat(4_200), None);
    assert_eq!(big.result.as_deref(), Some("4.2k characters"));

    let empty = one("mcp_call_tool", true, "   ", None);
    assert_eq!(empty.result, None, "nothing came back, so nothing is said");
}

/// An intrinsic OpenCompany tool's output is OC-authored operator copy, so a
/// success shows it — the same argument that already let its *failures*
/// through verbatim.
#[test]
fn an_intrinsic_tools_success_shows_its_own_message() {
    let step = one("query_company", true, "3 desks, 2 open cards", None);
    assert_eq!(step.result.as_deref(), Some("3 desks, 2 open cards"));
}

// -----------------------------------------------------------------------
// #410 seen from here: a cut result is legible
// -----------------------------------------------------------------------

/// Issue #410's failure was invisible from the trace: the call succeeded,
/// the answer was incomplete, and no status word can say both. The flag
/// can.
#[test]
fn a_cut_result_is_flagged_as_truncated() {
    for output in [
        "…the first action\n\n[truncated by tool cap: 8123 more chars not shown]",
        "…\n\n[… 4096 bytes truncated by tool_result_budget — re-run with a narrower query \
         to see the rest …]",
        "[tool_result_preview]\ntool: composio_list_tools\noriginal_bytes: 90000\n",
    ] {
        let step = one("composio_list_tools", true, output, None);
        assert_eq!(
            step.status,
            TurnStepStatus::Ok,
            "a cut result still succeeded"
        );
        assert!(step.truncated, "not flagged as cut: {output}");
    }
}

#[test]
fn a_complete_result_is_not_flagged() {
    assert!(!one("composio_list_tools", true, "[]", None).truncated);
}

/// COUPLING: all three markers are strings lifted from the vendored tool
/// pipeline. If any is reworded, this fails rather than letting truncation
/// go quiet again — which is precisely how #410 stayed hidden.
#[test]
fn truncation_markers_still_appear_in_the_vendored_tool_pipeline() {
    let sources = [
        vendored(
            "vendor/openhuman/crates/openhuman-core/src/agent/tinyagents/middleware/tool_output.rs",
        ),
        vendored(
            "vendor/openhuman/crates/openhuman-core/src/agent/harness/tool_result_artifacts/mod.rs",
        ),
    ]
    .concat();
    // Only the markers upstream still writes. `truncated by tool cap:` was
    // dropped from the vendored source by `6865c81eb` (uncapped tool
    // summaries) and appears there no longer — but it stays in
    // `TRUNCATION_MARKERS`, because that classifies results rather than
    // source and an older trace still carries the phrase. Asserting it here
    // would fail on a string the pipeline is right to have stopped writing.
    for marker in TRUNCATION_MARKERS
        .iter()
        .filter(|marker| **marker != "truncated by tool cap:")
    {
        assert!(
            sources.contains(marker),
            "'{marker}' no longer appears in the vendored tool pipeline — \
             re-derive `output_was_truncated` against the new wording"
        );
    }
}

// -----------------------------------------------------------------------
// Security
// -----------------------------------------------------------------------

/// SECURITY, and the acceptance criterion the issue spells out: a planted
/// credential in **arguments** and in a **result** reaches neither the
/// folded steps nor anything serialized from them.
///
/// Arguments now ride this surface (bounded, and only through #372's
/// redactor), so this covers the top level, a nested object, and an array —
/// the three shapes `approval_display` itself is tested on — plus the two
/// non-argument channels that were already refused: raw output, and the
/// server-supplied `display_detail`.
#[test]
fn planted_secret_never_reaches_serialized_steps() {
    let events = vec![
        // `display_detail` carries the secret; we never read it.
        AgentProgress::ToolCallStarted {
            call_id: "c1".to_string(),
            tool_name: "mcp_call_tool".to_string(),
            arguments: Value::Null,
            iteration: 1,
            display_label: Some("Calling a remote tool".to_string()),
            display_detail: Some(format!("auth={FAKE_SECRET}")),
        },
        // Success: the secret is in the nested remote arguments (top level,
        // nested, and inside an array) AND in the output body.
        completed(
            "c1",
            "mcp_call_tool",
            true,
            &format!("remote said: {FAKE_SECRET}"),
            Some(serde_json::json!({
                "server": "brave",
                "tool": "search",
                "arguments": {
                    "api_key": FAKE_SECRET,
                    "env": { "GITHUB_TOKEN": FAKE_SECRET },
                    "headers": [{ "Authorization": format!("Bearer {FAKE_SECRET}") }],
                }
            })),
            None,
        ),
        // A failing call whose raw output also carries the secret.
        completed(
            "c2",
            "mcp_call_tool",
            false,
            &format!("401 unauthorized token={FAKE_SECRET}"),
            Some(serde_json::json!({
                "server": "brave",
                "tool": "search",
                "arguments": { "password": FAKE_SECRET }
            })),
            None,
        ),
        // A parked call: the refusal text quotes the arguments back.
        completed(
            "c3",
            "send_email",
            false,
            &format!("{} body={FAKE_SECRET}", approval_refusal("send_email")),
            Some(serde_json::json!({ "to": "a@b.test", "client_secret": FAKE_SECRET })),
            None,
        ),
    ];
    let steps = fold_steps(events);
    let json = serde_json::to_string(&steps).expect("steps serialize");
    assert!(
        !json.contains(FAKE_SECRET),
        "a planted secret leaked into the serialized steps: {json}"
    );
    // ...and the redactor really ran, rather than the arguments simply
    // being dropped: the non-sensitive sibling survives.
    assert!(
        json.contains("a@b.test"),
        "this is a redactor, not a mute: {json}"
    );
}

/// The invariant the widened argument rendering must not weaken: a remote
/// tool's **output** is a body we do not control, and none of it — not even
/// bounded — becomes a result.
#[test]
fn a_remote_results_content_never_becomes_the_result_summary() {
    let step = one(
        "mcp_call_tool",
        true,
        "the remote said something quite specific and private",
        None,
    );
    let result = step.result.as_deref().unwrap();
    assert!(
        !result.contains("private") && !result.contains("remote said"),
        "a remote body's content leaked into the summary: {result}"
    );
    assert_eq!(result, "52 characters");
}

#[test]
fn remote_tool_failure_stays_scrubbed() {
    let steps = fold_steps(vec![
        started("c1", "mcp_call_tool", Some("Calling a remote tool")),
        completed(
            "c1",
            "mcp_call_tool",
            false,
            &format!("401 unauthorized token={FAKE_SECRET}"),
            Some(serde_json::json!({ "server": "brave", "tool": "search" })),
            None,
        ),
    ]);
    let rendered = serde_json::to_string(&steps).unwrap();
    assert!(
        !rendered.contains(FAKE_SECRET),
        "remote output must never surface: {rendered}"
    );
}
