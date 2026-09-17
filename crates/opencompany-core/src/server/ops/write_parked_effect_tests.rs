//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::http::StatusCode;
use serde_json::Value;

use super::write_test_support::*;
use crate::ports::tasks::{TaskRecord, TaskTitle};
use crate::ports::types::CompanyId;
use crate::runtime::journal::{ApprovalConversation, TaskLink};

/// **The acceptance test** (#305): an approval that parked and later resolved
/// reports the wait it actually caused.
///
/// Before this, `ApprovalResolved` carried a verdict and an actor but no park
/// time, so the console could only show one undifferentiated elapsed figure — a
/// task idle all day on a human looked exactly like one busy all day. The
/// assertion is exact arithmetic against the observed event timestamps, not a
/// tolerance: the whole value of the number is that it is not an estimate.
#[tokio::test]
async fn task_timeline_reports_the_wait_an_approval_actually_caused() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // Parked 40ms into the run. The sleep only guarantees the resolution lands
    // strictly after the park; the assertion below derives the expected span
    // from the real timestamps rather than from the sleep's duration.
    let id = ApprovalId::new("appr-1");
    let parked_at = dispatched_at + 40;
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Both halves of a real resolution: the journal drops it from the parked
    // queue *and* the event log gains the resolution. Doing only the latter
    // would leave the task reading as still waiting — the assertion at the end
    // of this test is what pins the pair together.
    runtime.journal.record_resolved(&id).await.unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::ApprovalResolved {
                approval_id: id.clone(),
                verdict: Verdict::Approve,
                by: Actor {
                    kind: ActorKind::User,
                    id: "u-1".into(),
                },
            },
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let approval = only_approval(&body);
    let resolved_at = approval["atMillis"].as_u64().unwrap();
    assert!(
        resolved_at > parked_at,
        "the sleep did not outlast the park"
    );
    assert_eq!(
        approval["waitedMillis"].as_u64().unwrap(),
        resolved_at - parked_at,
        "the wait must be the real park→resolve span, not an inference",
    );
    assert_eq!(approval["label"], "Approval approved");

    // The join must not become a new identity leak: it reads `approval_id` and
    // `by.kind`, never `by.id`.
    let raw = serde_json::to_string(&body["timeline"]).unwrap();
    assert!(!raw.contains("u-1"), "operator identity leaked: {raw}");

    // The wait is over, so nothing is pending: no live figure.
    assert!(
        body.get("waitingSince").is_none(),
        "a resolved approval must not leave the task reading as still waiting",
    );
}

/// A wait that ended in a TTL sweep is still a wait, and must not read as a
/// human decision.
///
/// Expiry used to write *only* a journal record, so a default-deny-on-silence
/// produced no event at all — the single case where waiting is most costly was
/// the one case the timeline could not see. The sweep now also appends a
/// system-attributed `ApprovalResolved`, and the read side labels it as an
/// expiry: rendering "Approval denied" would claim somebody looked at it.
#[tokio::test]
async fn expired_approval_is_labelled_as_an_expiry_and_carries_its_wait() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let id = ApprovalId::new("appr-stale");
    let parked_at = dispatched_at + 40;
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    // Re-park into the gate at epoch 0 so it is unambiguously past any TTL.
    runtime
        .approval_gate
        .rehydrate(id.clone(), parked_effect(), 0);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let expired = runtime.sweep_expired_approvals().await.unwrap();
    assert_eq!(expired, vec![id]);

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let approval = only_approval(&body);
    assert_eq!(
        approval["label"], "Approval expired (auto-denied)",
        "an expiry must not read as though a human decided",
    );
    let resolved_at = approval["atMillis"].as_u64().unwrap();
    assert_eq!(
        approval["waitedMillis"].as_u64().unwrap(),
        resolved_at - parked_at,
        "an expired approval's wait is the span nobody answered in",
    );
}

/// An approval already parked when the task was dispatched charges this run only
/// for the part of its wait that overlapped the run.
///
/// Approvals carry no task id, so they are correlated to the dispatch window.
/// Without the clamp, an effect parked hours before this card was dispatched
/// would dump its whole backlog wait onto this task's header — a figure larger
/// than the task's own elapsed time, which is visibly wrong.
#[tokio::test]
async fn a_wait_that_began_before_dispatch_is_clamped_to_the_run_window() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // Parked a full hour before this task was ever dispatched.
    let id = ApprovalId::new("appr-old");
    runtime
        .journal
        .record_parked(
            &id,
            &parked_effect(),
            dispatched_at - 3_600_000,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    runtime.journal.record_resolved(&id).await.unwrap();
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::ApprovalResolved {
                approval_id: id,
                verdict: Verdict::Deny,
                by: Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                },
            },
        )
        .await
        .unwrap();

    let (_, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    let approval = only_approval(&body);
    let resolved_at = approval["atMillis"].as_u64().unwrap();
    let waited = approval["waitedMillis"].as_u64().unwrap();
    assert_eq!(
        waited,
        resolved_at - dispatched_at,
        "the pre-dispatch hour must not be charged to this run",
    );
    assert!(waited < 3_600_000, "the clamp did not apply: {waited}");
    assert_eq!(approval["label"], "Approval denied");
}

/// A task parked on an operator *right now* reports it, even though no
/// resolution event exists yet.
///
/// This is the state the screen most needs to surface — "your agent is stopped,
/// waiting on you" — and it is invisible in the event log by construction: the
/// approval has not been resolved, so nothing has been appended. It comes from
/// the still-pending queue instead, scoped to the open run window.
#[tokio::test]
async fn a_currently_parked_approval_surfaces_as_a_live_wait() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let parked_at = dispatched_at + 10;
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-live"),
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["waitingSince"].as_u64().unwrap(),
        parked_at,
        "the live wait must start at the park instant",
    );
    // Nothing resolved, so nothing reached the timeline.
    assert!(
        body["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "approval"),
        "an unresolved approval must not fake a timeline row",
    );
}

/// A task that never waited reports no waiting at all — not a zero.
///
/// Both fields are `skip_serializing_if = "Option::is_none"`, so their absence
/// is what lets the console omit the figure entirely. If either were serialized
/// as `0`, every task on the board would grow a permanent "Waiting 0s", which
/// the issue calls out by name.
#[tokio::test]
async fn a_task_that_never_waited_reports_no_waiting_fields() {
    use crate::ports::types::CompanyEvent;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, _) = dispatched_task(&state, &company).await;

    runtime
        .events()
        .append(
            &company,
            CompanyEvent::DeskTaskCompleted {
                task_id: "t-1".into(),
                desk: "ceo".into(),
                output: "shipped".into(),
                column: "in_review".into(),
                artifact_ids: Vec::new(),
                origin_chat_id: None,
                origin_parent: None,
            },
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("waitingSince").is_none(),
        "a task with nothing parked must not report a live wait",
    );
    for entry in body["timeline"].as_array().unwrap() {
        assert!(
            entry.get("waitedMillis").is_none(),
            "a non-approval row must never carry a wait: {entry:?}",
        );
    }
}

// ── Issue #333: a task's Approvals tab shows that task's approvals ──────────
//
// The tab used to filter the *timeline* for `kind == "approval"`, which meant
// it could only ever show a resolution that fell inside the run window — and
// showed nothing at all for the state that matters most, an approval parked
// right now with the card stopped behind it. These pin the real query: the
// task id the runtime journal records with every parked effect.

/// **The acceptance test**: an approval raised while working a task appears on
/// that task's Approvals tab while it is still parked.
///
/// This is the QA repro — a request sitting on the main Approvals page while
/// the originating card's own tab read "No approvals in this run" — and it is
/// unreachable through the timeline by construction: nothing is appended to the
/// event log until somebody decides.
#[tokio::test]
async fn a_parked_approval_appears_on_its_own_task() {
    use crate::ports::types::ApprovalId;

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    let parked_at = dispatched_at + 10;
    runtime
        .journal
        .record_parked(
            &ApprovalId::new("appr-mine"),
            &parked_effect(),
            parked_at,
            TaskLink::Task { id: "t-1".into() },
            ApprovalConversation::default(),
            None,
        )
        .await
        .unwrap();

    let (status, body) = send(&state, "GET", "/api/v1/company/tasks/t-1", None).await;
    assert_eq!(status, StatusCode::OK);

    let approvals = body["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(approvals[0]["id"], "appr-mine");
    assert_eq!(approvals[0]["status"], "pending");
    assert_eq!(approvals[0]["atMillis"].as_u64().unwrap(), parked_at);
    // #468 shrank this projection to what the card's one waiting line reads.
    // `kind`, `resolvedAtMillis` and `waitedMillis` left with the Approvals tab.
    for gone in ["kind", "resolvedAtMillis", "waitedMillis"] {
        assert!(
            approvals[0].get(gone).is_none(),
            "`{gone}` was dropped with the Approvals tab (#468)",
        );
    }
    // The timeline is untouched — a parked approval still has no event.
    assert!(
        body["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["kind"] != "approval"),
    );
}

/// **The acceptance test that the old window could not pass**: two cards worked
/// in the same window keep their own approvals.
///
/// Under the window correlation both rows landed on both tabs, because the only
/// question asked was "did this resolve while that card was running". The join
/// is an id now, so a card's tab shows its own sign-off and nothing else.
#[tokio::test]
async fn a_second_task_in_the_same_window_does_not_absorb_the_first_s_approvals() {
    use crate::ports::types::{Actor, ActorKind, ApprovalId, CompanyEvent, Verdict};

    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;
    let company = CompanyId::new("acme");
    let (runtime, dispatched_at) = dispatched_task(&state, &company).await;

    // A second card, dispatched into the same open window as `t-1`.
    runtime
        .tasks()
        .upsert(
            &company,
            &TaskRecord {
                id: "t-2".into(),
                title: TaskTitle::authored("Also ship it"),
                note: None,
                column: "in_progress".into(),
                priority: "medium".into(),
                assignee: "ceo".into(),
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
    runtime
        .events()
        .append(
            &company,
            CompanyEvent::TaskDispatched {
                task_id: "t-2".into(),
                run_id: None,
                origin_chat_id: None,
                origin_parent: None,
            },
        )
        .await
        .unwrap();

    // One approval each, both parked and resolved inside both windows.
    for (id, owner) in [("appr-one", "t-1"), ("appr-two", "t-2")] {
        let id = ApprovalId::new(id);
        runtime
            .journal
            .record_parked(
                &id,
                &parked_effect(),
                dispatched_at + 5,
                TaskLink::Task { id: owner.into() },
                ApprovalConversation::default(),
                None,
            )
            .await
            .unwrap();
        runtime.journal.record_resolved(&id).await.unwrap();
        runtime
            .events()
            .append(
                &company,
                CompanyEvent::ApprovalResolved {
                    approval_id: id,
                    verdict: Verdict::Approve,
                    by: Actor {
                        kind: ActorKind::User,
                        id: "u-1".into(),
                    },
                },
            )
            .await
            .unwrap();
    }

    for (task, own, other) in [
        ("t-1", "appr-one", "appr-two"),
        ("t-2", "appr-two", "appr-one"),
    ] {
        let (status, body) = send(
            &state,
            "GET",
            &format!("/api/v1/company/tasks/{task}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let ids: Vec<&str> = body["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![own], "{task} must own exactly its own approval");
        assert!(!ids.contains(&other));
        // And the timeline agrees — one surface, one correlation.
        let rows: Vec<&Value> = body["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "approval")
            .collect();
        assert_eq!(rows.len(), 1, "{task}: {rows:?}");
    }
}
