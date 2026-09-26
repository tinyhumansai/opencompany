use super::tests_owner_setup::{Harness, graph, graph_without_destination, reached_output, record};
use super::*;

use crate::company::parse_workflow;
use crate::runtime::channel::DeskChannel;

/// A channel the deployment never wired cannot be conjured by a graph. The
/// failure names what IS wired, so the fix is obvious from the run result.
#[tokio::test]
async fn channel_that_is_not_wired_fails_with_the_wired_list() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&["*"]),
        &graph("channel", Some("telegram")),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Failed);
    // The unwired failure speaks in the same sentence the console's picker
    // pre-flight shows (issue #981), naming what IS wired — nothing, here.
    assert!(
        reports[0]
            .detail
            .contains("is not an automation delivery channel"),
        "{reports:?}"
    );
    assert!(
        reports[0].detail.contains("no durable channels"),
        "{reports:?}"
    );
    // The two channel failures are classified apart: "you named a channel
    // that does not exist" and "the channel said no" want different fixes,
    // and the log line only ever sees this half.
    assert_eq!(reports[0].reason, DeliveryReason::ChannelNotWired);
    // The channel id — which for this arm IS the target — stays off the
    // loggable half, same rule as a recipient address (issue #248).
    assert!(
        !reports[0].reason.to_string().contains("telegram"),
        "{reports:?}"
    );
    assert!(h.channel.sent().is_empty());
}

// --- reachability & wiring ----------------------------------------------

/// An `output` node on a branch the run never took gets no attempt and NO
/// ROW. An absent row means "not reached", never "silently dropped".
#[tokio::test]
async fn an_unreached_output_node_produces_no_row() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.add_admin("u1", "ada@acme.test").await;
    // The engine reached `start` but never `done`.
    let output = serde_json::json!({
        "nodes": { "start": { "items": [{ "json": { "seed": 1 } }] } }
    });

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&["*"]),
        &graph("owner", None),
        "run-1",
        &output,
        &[],
    )
    .await;

    assert!(reports.is_empty(), "{reports:?}");
    assert!(h.mail.sent().is_empty());
    assert!(h.channel.sent().is_empty());
}

/// An `output` node with no `destination` is the pre-#170 shape. It still
/// shows in the run drawer, still sends nothing, and — since #925 — says so
/// with a `Skipped` row instead of contributing nothing at all.
///
/// **This assertion is inverted from what it was.** It previously read
/// `reports.is_empty()`, which is the behaviour #925 was filed against:
/// silence made "the author routed nothing on purpose" and "the author never
/// configured a destination" the same observation. The transport assertions
/// below are the part that must not change — nothing is sent either way.
#[tokio::test]
async fn an_output_node_without_a_destination_reports_the_gap_and_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    let plain = parse_workflow(
        r#"
id = "plain"
name = "Plain"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#,
    )
    .expect("parses");

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&["*"]),
        &plain,
        "run-1",
        &reached_output(),
        &[],
    )
    .await;
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Skipped);
    assert_eq!(reports[0].reason, DeliveryReason::NoDestinationConfigured);
    assert!(
        h.mail.sent().is_empty() && h.channel.sent().is_empty(),
        "the row is a statement about configuration; nothing may leave the process"
    );
}

/// The #169 lesson: an unwired delivery bundle must be LOUD. It writes a
/// `failed` row onto the run result — where an operator actually looks —
/// rather than skipping in a debug log.
#[tokio::test]
async fn unwired_delivery_reports_loudly_instead_of_skipping() {
    let reports = deliver_outputs(
        None,
        &record(&["*"]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Failed);
    assert_eq!(reports[0].node, "done");
    assert_eq!(reports[0].kind, "owner");
    assert!(reports[0].detail.contains("not wired"), "{reports:?}");
    assert!(
        reports[0].detail.contains("nothing was sent"),
        "{reports:?}"
    );
}

// --- issue #438: one delivery per approval lineage ------------------------

/// One ledger row naming `node`, as a continuation's trigger input carries.
fn already(node: &str, kind: &str) -> Vec<DeliveredReport> {
    vec![DeliveredReport {
        node: node.to_string(),
        kind: kind.to_string(),
    }]
}

/// **The regression.** A continuation reaches the same `output` node again,
/// and must not mail the report a second time. The row says so, and the
/// transport is never touched.
#[tokio::test]
async fn a_report_this_lineage_already_sent_is_not_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.add_admin("u1", "ada@acme.test").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &already("done", "owner"),
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Skipped);
    assert_eq!(reports[0].reason, DeliveryReason::AlreadyDelivered);
    assert_eq!(reports[0].node, "done");
    assert!(
        h.mail.sent().is_empty(),
        "the transport must never be reached: {:?}",
        h.mail.sent()
    );
    assert!(h.channel.sent().is_empty());
}

/// A report the first run **parked** counts as delivered too. Otherwise
/// every continuation stacks a second identical cold-send card, and
/// approving both mails the stranger twice — `park_cold_recipient` has no
/// dedupe of its own.
#[tokio::test]
async fn a_report_this_lineage_already_parked_is_not_parked_again() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, false).with_parking(dir.path(), "full");
    // Cold: the company has never heard from this address.
    let cold = graph("email", Some("stranger@example.test"));

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&["email"]),
        &cold,
        "run-1",
        &reached_output(),
        &already("done", "email"),
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Skipped);
    assert_eq!(reports[0].reason, DeliveryReason::AlreadyDelivered);
    assert!(
        h.journal
            .as_ref()
            .expect("parking wired")
            .pending()
            .is_empty(),
        "a continuation must not stack a second card for one send"
    );
    assert!(h.mail.sent().is_empty());
}

/// The ledger suppresses the node it names and nothing else. A second
/// output node in the same graph still delivers — otherwise one earlier
/// send would silence the whole graph.
#[tokio::test]
async fn a_node_the_ledger_does_not_name_still_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.add_admin("u1", "ada@acme.test").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &already("some_other_node", "owner"),
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent);
    assert_eq!(h.mail.sent().len(), 1);
}

/// A run nobody resumed carries an empty ledger, and behaves exactly as it
/// did before #438. Every other test in this module passes `&[]`, so this
/// states the invariant they all rely on.
#[tokio::test]
async fn a_run_with_no_ledger_delivers_normally() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.add_admin("u1", "ada@acme.test").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent);
    assert_eq!(h.mail.sent().len(), 1);
}

/// An unreached node on the ledger produces no row at all: "not reached"
/// outranks "already delivered", because there was nothing to deliver this
/// time either way.
#[tokio::test]
async fn an_unreached_node_on_the_ledger_still_produces_no_row() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    let unreached = serde_json::json!({ "nodes": { "start": { "items": [] } } });

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &unreached,
        &already("done", "owner"),
    )
    .await;

    assert!(reports.is_empty(), "{reports:?}");
}

// --- report extraction ---------------------------------------------------

/// Several items concatenate in order; the doubly-wrapped `json.json.text`
/// the engine sometimes emits is read too, with the outer value winning.
#[test]
fn report_text_reads_plain_and_doubly_wrapped_items() {
    let output = serde_json::json!({
        "nodes": { "done": { "items": [
            { "json": { "text": "first" } },
            { "json": { "json": { "text": "second" } } },
            { "json": { "text": "outer", "json": { "text": "inner" } } },
        ] } }
    });
    assert_eq!(report_text(&output, "done"), "first\n\nsecond\n\nouter");
}

/// A data-shaped item with no `text` is delivered as JSON rather than
/// dropped — an empty report would be worse than an ugly one.
#[test]
fn report_text_falls_back_to_json_for_a_textless_item() {
    let output = serde_json::json!({
        "nodes": { "done": { "items": [{ "json": { "revenue": 12 } }] } }
    });
    assert!(report_text(&output, "done").contains("revenue"));
}

#[test]
fn report_text_of_a_node_with_no_items_says_so() {
    let output = serde_json::json!({ "nodes": { "done": { "items": [] } } });
    assert!(report_text(&output, "done").contains("no output"));
}

/// Truncation is character-indexed: a byte slice here would panic
/// mid-codepoint on any multi-byte report.
#[test]
fn truncation_never_splits_a_codepoint() {
    let text = "é".repeat(50);
    let cut = truncate_chars(&text, 10);
    assert!(cut.starts_with(&"é".repeat(10)));
    assert!(cut.ends_with(TRUNCATION_MARKER));
    // Untouched when it fits.
    assert_eq!(truncate_chars("short", 10), "short");
}

// --- issue #529: the write-behind delivery record ------------------------

/// A `Sent` dispatch journals exactly one `WorkflowReportDelivered`, shaped
/// from the row — the durable record a crashed run leaves so a re-run can
/// skip it.
#[tokio::test]
async fn a_sent_delivery_journals_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path(), false, true);
    h.deps.channels = vec![Arc::new(DeskChannel::new(
        h.company.clone(),
        "engineering".to_string(),
        h.events.clone(),
    ))];

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("channel", Some("engineering")),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");

    let journaled = h.journaled_deliveries().await;
    assert_eq!(
        journaled.len(),
        1,
        "exactly one record per dispatch: {journaled:?}"
    );
    let CompanyEvent::WorkflowReportDelivered {
        workflow_id,
        run_id,
        node,
        kind,
        target,
    } = &journaled[0]
    else {
        panic!("expected a WorkflowReportDelivered, got {:?}", journaled[0]);
    };
    assert_eq!(workflow_id, "report_flow");
    assert_eq!(run_id, "run-1");
    assert_eq!(node, "done");
    assert_eq!(kind, "channel");
    assert_eq!(target.as_deref(), Some("engineering"));
    let events = h
        .events
        .read_from(&h.company, crate::ports::types::EventSeq::new(0), 20)
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        CompanyEvent::AgentReply { chat_id, text, .. }
            if chat_id == "engineering" && text.contains("Q3 is up 12%.")
    )));
}

/// A `Pending` park journals a record too: the card is durable and approving
/// it sends, so a re-run must treat the report as already delivered —
/// exactly as issue #438's in-lineage ledger counts a `Pending` row.
#[tokio::test]
async fn a_pending_park_journals_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true).with_parking(dir.path(), "full");
    h.receive_from("someone-else@example.com").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&["*"]),
        &graph("email", Some("stranger@example.com")),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;
    assert_eq!(reports[0].status, DeliveryStatus::Pending, "{reports:?}");

    let journaled = h.journaled_deliveries().await;
    assert_eq!(
        journaled.len(),
        1,
        "a park is a delivery for the ledger: {journaled:?}"
    );
    let CompanyEvent::WorkflowReportDelivered { node, kind, .. } = &journaled[0] else {
        panic!("expected a WorkflowReportDelivered");
    };
    assert_eq!(node, "done");
    assert_eq!(kind, "email");
}

/// A row that did NOT leave the process journals nothing. A `Failed` channel
/// (unwired target) leaves no record, so a re-run is free to retry it.
#[tokio::test]
async fn a_failed_delivery_journals_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // No channel wired, so a `channel` destination fails.
    let h = Harness::new(dir.path(), false, false);

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("channel", Some("telegram")),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;
    assert_eq!(reports[0].status, DeliveryStatus::Failed, "{reports:?}");
    assert!(
        h.journaled_deliveries().await.is_empty(),
        "a failed dispatch left the process nothing to record"
    );
}

/// An `AlreadyDelivered` skip journals nothing — the report is on the ledger
/// precisely because it already went out, so recording it again would double
/// the very thing the ledger exists to prevent.
#[tokio::test]
async fn an_already_delivered_skip_journals_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), false, true);

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("channel", Some("operator")),
        "run-1",
        &reached_output(),
        &[DeliveredReport {
            node: "done".to_string(),
            kind: "channel".to_string(),
        }],
    )
    .await;
    assert_eq!(reports[0].status, DeliveryStatus::Skipped, "{reports:?}");
    assert_eq!(reports[0].reason, DeliveryReason::AlreadyDelivered);
    assert!(
        h.journaled_deliveries().await.is_empty(),
        "a report skipped because it already went out is not re-recorded"
    );
}

/// A journal that cannot be written does not fail the delivery: the report
/// still sends and its row is still `Sent`. Losing the record risks one
/// duplicate on a later re-run — the accepted write-behind cost, never a
/// failed send.
#[tokio::test]
async fn a_journal_failure_does_not_fail_a_delivery() {
    let dir = tempfile::tempdir().unwrap();
    // A channel that accepts the send, so the journal write is what this
    // case actually reaches. Pointed at `operator` it would fail on the
    // refusal instead and pass for the wrong reason.
    let h = Harness::new(dir.path(), false, true)
        .with_recording_channel("engineering")
        .with_failing_events();

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("channel", Some("engineering")),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(
        h.recording().sent().len(),
        1,
        "the report reached the channel despite the journal"
    );
}

/// Issue #542: the dry router runs the routing half only. A reached output
/// node yields one `Skipped`/`DryRun` row naming where the report WOULD have
/// gone — no deps, no transport, no journal.
#[test]
fn deliver_outputs_dry_routes_a_reached_node_without_sending() {
    let workflow = graph("email", Some("ada@example.com"));
    let reports = deliver_outputs_dry(&record(&["email"]), &workflow, &reached_output());
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].node, "done");
    assert_eq!(reports[0].status, DeliveryStatus::Skipped);
    assert_eq!(reports[0].reason, DeliveryReason::DryRun);
    assert_eq!(reports[0].target.as_deref(), Some("ada@example.com"));
    assert!(
        reports[0].detail.contains("email ada@example.com"),
        "the row should name where it would have gone: {}",
        reports[0].detail
    );
}

// --- issue #925: an unconfigured destination is not the same as no report --

/// **The regression.** A run that reaches an output node naming no
/// destination used to return an empty `deliveries` list, which the console
/// renders as `Finished — this run routed no reports.` — the same sentence
/// it shows a workflow that deliberately routed nothing. The row is what
/// tells the two apart, and it carries the reason as a closed token so a
/// reader does not have to parse prose.
///
/// Deps are `None` here on purpose: the check must land *before* anything
/// touches a transport, so this passes on a runtime with no delivery ports
/// and would fail with a `NotWired` row if the arms were ever reordered.
#[tokio::test]
async fn a_reached_output_node_with_no_destination_says_so() {
    let reports = deliver_outputs(
        None,
        &record(&["*"]),
        &graph_without_destination(),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].node, "done");
    assert_eq!(reports[0].status, DeliveryStatus::Skipped);
    assert_eq!(reports[0].reason, DeliveryReason::NoDestinationConfigured);
    assert_eq!(
        reports[0].target, None,
        "there is no destination, so there is no target to name"
    );
    assert!(
        reports[0].detail.contains("no destination"),
        "the row has to say what is missing: {}",
        reports[0].detail
    );
}

/// The other half of the same rule: an output node with no destination that
/// the run never reached still contributes nothing. "Never configured" is
/// only worth reporting about a node the run actually arrived at — otherwise
/// every untaken branch would file a complaint.
#[tokio::test]
async fn an_unreached_output_node_with_no_destination_stays_silent() {
    let output = serde_json::json!({ "nodes": { "start": { "items": [] } } });
    let reports = deliver_outputs(
        None,
        &record(&["*"]),
        &graph_without_destination(),
        "run-1",
        &output,
        &[],
    )
    .await;

    assert!(
        reports.is_empty(),
        "an unreached node owes no report either way: {reports:?}"
    );
}
