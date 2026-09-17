//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::http::StatusCode;
use serde_json::json;

use super::write_test_support::*;
use crate::ports::tasks::{TaskRecord, TaskTitle};
use crate::ports::types::CompanyId;

/// Under the `openhuman` feature, adding a server probes it — and a probe that
/// fails (dead endpoint) is **never** rolled back: the server stays added, and
/// its scrubbed health is returned as `test` and persisted onto the GET shape.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn mcp_add_probes_without_rollback_and_persists_health() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // A syntactically valid but unreachable endpoint (nothing listening).
    let (status, added) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers",
        Some(json!({ "name": "dead", "endpoint": "http://127.0.0.1:1/mcp" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // No rollback — the server is present despite the failed probe.
    assert_eq!(added["server"]["name"], "dead");
    // The probe result is echoed and the status is a non-ok tier.
    assert!(added["test"].is_object(), "probe result echoed: {added}");
    assert_ne!(added["test"]["status"], "ok");

    // The health is persisted onto the GET shape too.
    let (_, list) = send(&state, "GET", "/api/v1/company/mcp/servers", None).await;
    let server = list
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "dead")
        .expect("server present");
    assert!(server["health"].is_object(), "health persisted: {server}");

    // On-demand Test re-probes and returns health.
    let (status, health) = send(
        &state,
        "POST",
        "/api/v1/company/mcp/servers/dead/test",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        health["status"].is_string(),
        "test returns health: {health}"
    );
}

/// #187: the Artifacts tab's full loop — an agent draft, a human edit appended
/// as a new version, and the diff between them.
///
/// The point of the port is that the operator's edit does **not** overwrite the
/// agent's text, so this asserts v1 survives verbatim after the edit. A store
/// that mutated in place would still serve a plausible-looking artifact while
/// having destroyed the one datum the epic wants.
#[tokio::test]
async fn artifact_versions_capture_the_human_edit_and_diff() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The agent's draft.
    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts",
        Some(json!({
            "taskId": "t-1",
            "title": "Launch post",
            "kind": "markdown",
            "body": "alpha\nbeta\ngamma",
            "authorId": "ceo"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["versions"].as_array().unwrap().len(), 1);
    // No human has touched it, so no diff is offered.
    assert!(created.get("humanEditDiff").is_none());

    // The operator edits one line before approving.
    let (status, edited) = send(
        &state,
        "POST",
        &format!("/api/v1/company/artifacts/{id}/versions"),
        Some(json!({ "body": "alpha\nBETA\ngamma" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let versions = edited["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    // v1 is untouched — the whole reason versions are append-only.
    assert_eq!(versions[0]["body"], "alpha\nbeta\ngamma");
    assert_eq!(versions[0]["author"], "agent");
    assert_eq!(versions[1]["author"], "operator");
    assert_eq!(versions[1]["note"], "operator edit before approval");

    // The derived diff rides along, so the tab needs one call.
    let diff = &edited["humanEditDiff"];
    assert_eq!(diff["fromVersion"], 1);
    assert_eq!(diff["toVersion"], 2);
    assert_eq!(diff["added"], 1);
    assert_eq!(diff["removed"], 1);

    // …and is also addressable on its own.
    let (status, standalone) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(standalone["toVersion"], 2);

    // Listing by task returns it; an unrelated task sees nothing.
    let (status, listed) = send(&state, "GET", "/api/v1/company/tasks/t-1/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let (_, empty) = send(
        &state,
        "GET",
        "/api/v1/company/tasks/t-other/artifacts",
        None,
    )
    .await;
    assert_eq!(empty.as_array().unwrap().len(), 0);

    // Issue #244: this record was created through the REST route, not by a
    // publish, so it carries no `source` — and the response omits the key
    // entirely rather than sending a null the console would have to special-case.
    assert!(
        listed[0].get("source").is_none(),
        "a record with no source must not carry an empty one: {}",
        listed[0]
    );
}

/// Issue #244: a published artifact's `source` reaches the console.
///
/// The record is flattened into [`ArtifactView`], so this is really asserting
/// that the projection stays a flatten and never becomes a hand-written field
/// list — the moment it does, a new record field is silently invisible to the
/// tab that exists to render it.
#[tokio::test]
async fn a_published_artifacts_source_reaches_the_console() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let company = state.registry().list()[0].clone();
    let runtime = state.registry().get(&company).expect("company");
    let published = ArtifactRecord::new(
        "art-1",
        "t-1",
        "Launch spec",
        ArtifactKind::Markdown,
        "# Spec",
        "ceo",
        1,
    )
    .with_source("specs/launch.md");
    ArtifactStore::upsert(runtime.artifacts().as_ref(), &company, &published)
        .await
        .expect("seed");

    let (status, listed) = send(&state, "GET", "/api/v1/company/tasks/t-1/artifacts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["source"], "specs/launch.md");

    let (status, one) = send(&state, "GET", "/api/v1/company/artifacts/art-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["source"], "specs/launch.md");
}

/// #185: `GET …/tasks/{id}` assembles the header, the per-task timeline, and
/// the lineage in one read.
///
/// The timeline half is the point: the journal is company-scoped, so this
/// asserts that a reply tagged with *this* task is admitted while an untagged
/// chat reply and a reply tagged to a *different* task are both excluded. Those
/// three cases are exactly what the `task_id` threading exists to separate.
#[tokio::test]
async fn task_detail_assembles_timeline_and_lineage() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let runtime = state.registry().get(&company).unwrap();

    let card = |id: &str, title: &str, parent: Option<&str>| TaskRecord {
        id: id.into(),
        title: TaskTitle::authored(title),
        note: None,
        column: "in_review".into(),
        priority: "medium".into(),
        assignee: "ceo".into(),
        updated_at_millis: 1,
        origin: None,
        parent_task_id: parent.map(str::to_string),
        output: None,
        plan: None,
        planning_attempts: Vec::new(),
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        origin_message_seq: None,
        bounced: None,
    };
    for t in [
        card("t-parent", "Parent", None),
        card("t-1", "Ship it", Some("t-parent")),
        card("t-child", "Subtask", Some("t-1")),
        card("t-other", "Unrelated", None),
    ] {
        runtime.tasks().upsert(&company, &t).await.unwrap();
    }

    for event in [
        CompanyEvent::TaskDispatched {
            task_id: "t-1".into(),
            run_id: None,
            origin_chat_id: None,
            origin_parent: None,
        },
        // Tagged to this task — admitted.
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "t-1".into(),
            agent_id: "ceo".into(),
            text: "on it".into(),
            steps: Vec::new(),
            task_id: Some("t-1".into()),
            outputs: Vec::new(),
        },
        // An ordinary chat reply — excluded.
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "General".into(),
            agent_id: "ceo".into(),
            text: "unrelated chatter".into(),
            steps: Vec::new(),
            task_id: None,
            outputs: Vec::new(),
        },
        // Tagged to a different task — excluded.
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            chat_id: "t-other".into(),
            agent_id: "ceo".into(),
            text: "someone else's work".into(),
            steps: Vec::new(),
            task_id: Some("t-other".into()),
            outputs: Vec::new(),
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
    ] {
        runtime.events().append(&company, event).await.unwrap();
    }

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["task"]["id"], "t-1");
    assert_eq!(body["task"]["parentTaskId"], "t-parent");

    let kinds: Vec<&str> = body["timeline"]
        .as_array()
        .expect("timeline array")
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["dispatched", "reply", "completed"]);

    let raw = serde_json::to_string(&body["timeline"]).unwrap();
    assert!(
        !raw.contains("unrelated chatter") && !raw.contains("someone else's work"),
        "another task's / an untagged chat reply leaked onto this timeline: {raw}"
    );

    assert_eq!(body["lineage"]["parent"]["id"], "t-parent");
    let children = body["lineage"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], "t-child");

    // An unknown id 404s, matching PATCH/DELETE.
    let (status, _) = send(&state, "GET", "/api/v1/company/tasks/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #187: the diff route's argument contract, and the 404s.
#[tokio::test]
async fn artifact_diff_rejects_a_half_specified_range() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (_, created) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts",
        Some(json!({ "taskId": "t-1", "title": "Draft", "body": "one" })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // Neither bound, and no operator edit yet → nothing to diff, stated plainly
    // rather than silently returning an empty diff.
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Half a range is a 400, not a guess about the other end.
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff?from=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A version that does not exist names itself.
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/v1/company/artifacts/{id}/diff?from=1&to=9"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown artifact ids 404 on every handler that takes one.
    let (status, _) = send(&state, "GET", "/api/v1/company/artifacts/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/artifacts/nope/versions",
        Some(json!({ "body": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&state, "DELETE", "/api/v1/company/artifacts/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// #185 gave `GET …/tasks/{task_id}` a handler, which now overlaps the static
/// `GET …/tasks/inflight` the operator strip reads.
///
/// Before #185 the dynamic segment carried no GET, so nothing could shadow the
/// strip. Now something can: if the routes were ever reordered (or the static
/// one dropped), `inflight` would be parsed as a *card id*, `task_detail` would
/// find no such card, and the strip would 404 — with no test failing anywhere
/// else, because no card can be named `inflight` for the collision to show up
/// in ordinary use.
#[tokio::test]
async fn inflight_read_is_not_shadowed_by_task_detail() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    // The strip's read still resolves to the inflight handler: an array, not
    // the object `task_detail` would return, and not a 404.
    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/inflight", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.is_array(),
        "GET /tasks/inflight must hit list_inflight, not task_detail: {body}"
    );
}
