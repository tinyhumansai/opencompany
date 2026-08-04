//! Tests for the exported task record (issue #352).
//!
//! The renderer is a pure function, so this file asserts the document itself
//! against a fixture: that it reads as prose rather than as a data dump, that
//! the arithmetic matches the screen's, and that nothing a user typed can become
//! markup.
//!
//! The wire contract — the content type, the attachment, the 404, and the one
//! property a pure test cannot see (exporting a task changes nothing about it)
//! — is exercised over the real router in `server::ops::write_test`, where the
//! request harness lives.

use super::*;
use crate::server::ops::tasks::{Lineage, LineageRef, TaskCard, TaskDetail, TimelineEntry};

/// 2026-08-05 09:00:00 UTC — a fixed clock, so the document is deterministic.
const T0: u64 = 1_785_920_400_000;

fn entry(seq: u64, at: u64, kind: &str, label: &str, detail: Option<&str>) -> TimelineEntry {
    TimelineEntry {
        seq,
        at_millis: at,
        kind: kind.to_string(),
        label: label.to_string(),
        detail: detail.map(str::to_string),
        waited_millis: None,
    }
}

fn card(title: &str) -> TaskCard {
    TaskCard {
        id: "t-1".to_string(),
        title: title.to_string(),
        note: Some("Write the launch post and get it signed off.".to_string()),
        column: "in_review".to_string(),
        priority: "high".to_string(),
        assignee: "writer".to_string(),
        updated_at: T0 + 600_000,
        parent_task_id: None,
        origin_chat_id: None,
    }
}

/// A worked-then-finished task with a reply, a failed tool call and a sign-off.
fn detail() -> TaskDetail {
    let mut approval = entry(4, T0 + 400_000, "approval", "Approval approved", None);
    approval.waited_millis = Some(120_000);
    TaskDetail {
        task: card("Launch post"),
        timeline: vec![
            entry(1, T0, "dispatched", "Dispatched", None),
            entry(
                2,
                T0 + 60_000,
                "reply",
                "Reply from writer",
                Some("First draft is up."),
            ),
            entry(
                3,
                T0 + 120_000,
                "tool_failed",
                "mailer · send failed",
                Some("server rejected the call"),
            ),
            approval,
            entry(
                5,
                T0 + 600_000,
                "completed",
                "Finished on writer → in_review",
                Some("Posted."),
            ),
        ],
        lineage: Lineage {
            parent: Some(LineageRef {
                id: "t-parent".to_string(),
                title: "Launch week".to_string(),
                column: "in_progress".to_string(),
            }),
            children: vec![LineageRef {
                id: "t-child".to_string(),
                title: "Social cutdowns".to_string(),
                column: "todo".to_string(),
            }],
        },
        waiting_since: None,
    }
}

/// The acceptance bar: the document is readable prose, not a data dump.
///
/// Asserts the two things a non-technical reader needs — plain-English section
/// headings and human labels — and the two things they must never be shown: the
/// board's wire words, and JSON.
#[test]
fn the_document_reads_as_prose_not_as_data() {
    let html = render_document("Acme Co", &detail(), &[], T0 + 900_000);

    for heading in [
        "Task record",
        "What was asked for",
        "What happened, in order",
        "Sign-offs",
        "What this task produced",
        "Related work",
    ] {
        assert!(html.contains(heading), "missing section: {heading}");
    }
    // Human labels, not board ids. (The `completed` entry's own label carries
    // the landing column verbatim, because the document must say what the
    // screen says — the facts grid is what a reader takes the status from.)
    assert!(
        html.contains("<dd>In review</dd>"),
        "status is not humanised: {html}"
    );
    assert!(!html.contains("<dd>in_review</dd>"));
    assert!(html.contains("<dd>High</dd>"));
    // A date a person reads, and no epochs.
    assert!(html.contains("5 Aug 2026"), "no readable date: {html}");
    assert!(!html.contains("1785920400000"));
    // Not a JSON dump: no braces-and-quotes payload, no camelCase field names.
    for token in ["\"seq\"", "\"atMillis\"", "\"kind\"", "waitedMillis"] {
        assert!(
            !html.contains(token),
            "raw field name in the document: {token}"
        );
    }
    // Opens on its own: a complete document, styles inlined, nothing fetched.
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<style>"));
    assert!(
        !html.contains("<script"),
        "the document must not need scripting"
    );
    assert!(
        !html.contains("http://") && !html.contains("https://"),
        "the document must not fetch anything: {html}"
    );
}

/// Every detail is expanded. The screen hides them behind a disclosure and
/// collapses repeats; a reader holding a file has nothing to click.
#[test]
fn every_timeline_entry_and_detail_is_printed() {
    let html = render_document("Acme Co", &detail(), &[], T0 + 900_000);
    for text in [
        "Dispatched",
        "Reply from writer",
        "First draft is up.",
        "mailer · send failed",
        "server rejected the call",
        "Posted.",
    ] {
        assert!(html.contains(text), "timeline lost: {text}");
    }
}

/// The worked/waiting split, and the proportional band the split exists for.
#[test]
fn the_worked_and_waiting_split_is_carried() {
    let html = render_document("Acme Co", &detail(), &[], T0 + 900_000);
    // Dispatched at T0, completed at T0+10m.
    assert!(html.contains("10m 00s"), "worked total missing: {html}");
    // One 2-minute approval wait.
    assert!(html.contains("2m 00s"), "waiting total missing: {html}");
    assert!(html.contains("Waited 2m 00s for a person"));
    assert!(
        html.contains("min-height:"),
        "the waiting band has no drawn height"
    );
}

/// A still-parked task says so, and its live wait runs to now.
#[test]
fn a_task_still_waiting_says_so() {
    let mut d = detail();
    d.timeline.truncate(1); // dispatched only, still running
    d.waiting_since = Some(T0 + 60_000);
    let html = render_document("Acme Co", &d, &[], T0 + 300_000);
    assert!(html.contains("waiting on a person right now"));
    assert!(
        html.contains("Still waiting on a person, 4m 00s so far"),
        "{html}"
    );
}

/// An untouched task exports an honest, empty-but-explained document rather
/// than blank sections.
#[test]
fn an_untouched_task_exports_an_honest_document() {
    let mut d = detail();
    d.timeline.clear();
    d.task.note = None;
    d.lineage = Lineage {
        parent: None,
        children: Vec::new(),
    };
    let html = render_document("Acme Co", &d, &[], T0);

    assert!(html.contains("Nothing has happened on this task yet"));
    assert!(html.contains("No decision needed a person's approval"));
    assert!(html.contains("This task recorded no output."));
    // Related work is omitted entirely when there is none — an empty heading is
    // noise in a document somebody reads top to bottom.
    assert!(!html.contains("Related work"));
}

/// Nothing a user typed can become markup.
///
/// The document is opened in a browser and forwarded to clients, so a card
/// title or an agent reply carrying a tag is the one failure that turns an
/// audit record into an attack. Every interpolation goes through `escape`;
/// this pins the ones an attacker controls.
#[test]
fn user_text_cannot_become_markup() {
    let mut d = detail();
    d.task.title = "<script>alert('x')</script>".to_string();
    d.task.note = Some("<img src=x onerror=alert(1)>".to_string());
    d.timeline = vec![entry(
        1,
        T0,
        "reply",
        "Reply from \"writer\"",
        Some("</style><script>steal()</script>"),
    )];
    let html = render_document("<b>Acme</b>", &d, &[], T0);

    assert!(!html.contains("<script>"), "script tag survived: {html}");
    assert!(!html.contains("<img src=x"));
    assert!(!html.contains("<b>Acme</b>"));
    assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
    assert!(html.contains("&quot;writer&quot;"));
}

/// The waiting band's curve is the console's: a floor, a cap, and a visible
/// difference between a short wait and a long one. Drift here is the export
/// quietly telling a different story than the screen.
#[test]
fn the_waiting_band_curve_mirrors_the_console() {
    assert_eq!(waiting_band_height(0), 12);
    assert_eq!(waiting_band_height(4_000), 14); // four seconds
    assert_eq!(waiting_band_height(4 * 60 * 60 * 1000), 112); // four hours, capped
    assert!(waiting_band_height(60_000) > waiting_band_height(4_000));
    assert!(waiting_band_height(u64::MAX) <= 112);
}

/// Overlapping waits are merged, not summed twice — the company waited once.
#[test]
fn overlapping_waits_are_merged() {
    let mut first = entry(1, T0 + 300_000, "approval", "Approval approved", None);
    first.waited_millis = Some(300_000); // [T0, T0+5m]
    let mut second = entry(2, T0 + 420_000, "approval", "Approval approved", None);
    second.waited_millis = Some(300_000); // [T0+2m, T0+7m]
    let waiting = waiting_millis(&[first, second], None, T0);
    assert_eq!(waiting.millis, 420_000, "the overlap was counted twice");

    // A re-dispatched card accumulates both of its worked windows.
    let worked = worked_millis(
        &[
            entry(1, T0, "dispatched", "Dispatched", None),
            entry(2, T0 + 60_000, "completed", "Finished", None),
            entry(3, T0 + 600_000, "dispatched", "Dispatched", None),
            entry(4, T0 + 720_000, "completed", "Finished", None),
        ],
        T0,
    );
    assert_eq!(worked.millis, 180_000);
    assert!(!worked.live);
}

/// The filename is derived from the title, and always exists.
#[test]
fn the_attachment_is_named_after_the_task() {
    assert_eq!(slug("Launch post", "t-1"), "launch-post");
    assert_eq!(slug("  Ship it!!  ", "t-1"), "ship-it");
    // A title with nothing filename-safe in it falls back to the id.
    assert_eq!(slug("🚀🚀", "t-1"), "t-1");
    assert_eq!(slug("", "../../etc/passwd"), "etcpasswd");
}

/// Artifacts: every revision, who wrote it, the final text, and the human edit
/// rendered as a diff a non-technical reader can follow.
#[test]
fn artifacts_carry_their_versions_and_the_human_edit() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord};
    use crate::server::ops::artifacts::ArtifactView;

    let mut record = ArtifactRecord::new(
        "a-1",
        "t-1",
        "Launch post",
        ArtifactKind::Markdown,
        "Hello world\nsecond line",
        "writer",
        T0,
    );
    record.push_version(
        "Hello, world!\nsecond line",
        crate::ports::artifacts::ArtifactAuthor::Operator,
        "alex",
        T0 + 60_000,
        Some("operator edit before approval".to_string()),
    );
    let view = ArtifactView::from(record);
    let html = render_document(
        "Acme Co",
        &detail(),
        std::slice::from_ref(&view),
        T0 + 900_000,
    );

    assert!(html.contains("Launch post"));
    assert!(html.contains("writer (company)"));
    assert!(html.contains("alex (person)"));
    assert!(html.contains("operator edit before approval"));
    assert!(html.contains("Hello, world!"), "the final text is missing");
    assert!(
        html.contains("A person changed"),
        "the human edit is not explained"
    );
    assert!(html.contains("class=\"ins\"") && html.contains("class=\"del\""));
}
