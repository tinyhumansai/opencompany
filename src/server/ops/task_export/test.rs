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
use crate::server::ops::tasks::{
    Lineage, LineageRef, TaskCard, TaskDetail, TaskDurations, TimelineEntry,
};

/// 2026-08-05 09:00:00 UTC — a fixed clock, so the document is deterministic.
const T0: u64 = 1_785_920_400_000;

fn entry(seq: u64, at: u64, kind: &str, label: &str, detail: Option<&str>) -> TimelineEntry {
    TimelineEntry {
        seq,
        at_millis: at,
        kind: kind.to_string(),
        label: label.to_string(),
        detail: detail.map(str::to_string),
        cost_key: None,
        cost: None,
        waited_millis: None,
    }
}

fn card(title: &str) -> TaskCard {
    TaskCard {
        id: "t-1".to_string(),
        title: title.to_string(),
        note: Some("Write the launch post and get it signed off.".to_string()),
        column: "working".to_string(),
        stage: Some("in_review".to_string()),
        priority: "high".to_string(),
        assignee: "writer".to_string(),
        updated_at: T0 + 600_000,
        cost: None,
        parent_task_id: None,
        origin_chat_id: None,
        origin_run_id: None,
        origin_workflow_id: None,
        bounced: None,
        // The export document is deliberately link-free (issue #339): it is
        // read offline, by people who never saw the board, so a console hash
        // route would be a dead address in it. The deliverable itself is what
        // the export would have to carry, and that is a different change.
        output: None,
        plan: None,
        deliverable: crate::ports::tasks::TaskDeliverable::Once,
        workflow_proposal: None,
    }
}

/// A worked-then-finished task with a reply, a failed tool call and a sign-off.
///
/// Durations go through [`TaskDurations::compute`] — the same constructor
/// `assemble_detail` uses — so the fixture cannot drift from the host read it
/// stands in for.
fn detail() -> TaskDetail {
    detail_at(None)
}

/// [`detail`], optionally still parked on a person since `waiting_since`.
fn detail_at(waiting_since: Option<u64>) -> TaskDetail {
    let mut approval = entry(4, T0 + 400_000, "approval", "Approval approved", None);
    approval.waited_millis = Some(120_000);
    let timeline = vec![
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
    ];
    TaskDetail {
        task: card("Launch post"),
        durations: TaskDurations::compute(&timeline, waiting_since, T0 + 900_000),
        timeline,
        approvals: Vec::new(),
        irreversible_effects: Vec::new(),
        history_incomplete: false,
        discussion: Vec::new(),
        discussion_has_more: false,
        lineage: Lineage {
            parent: Some(LineageRef {
                id: "t-parent".to_string(),
                title: "Launch week".to_string(),
                column: "in_progress".to_string(),
                cost: None,
            }),
            children: vec![LineageRef {
                id: "t-child".to_string(),
                title: "Social cutdowns".to_string(),
                column: "todo".to_string(),
                cost: None,
            }],
        },
        runs: Vec::new(),
        waiting_since,
    }
}

/// Rebuilds a fixture's durations after its timeline was edited, so a test that
/// reshapes the fixture still exports numbers that match what it shows.
fn recompute(d: &mut TaskDetail, as_of: u64) {
    d.durations = TaskDurations::compute(&d.timeline, d.waiting_since, as_of);
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
        "What this task produced",
        "Related work",
    ] {
        assert!(html.contains(heading), "missing section: {heading}");
    }
    // Human labels, not board ids — and both halves of the card's state since
    // issue #1512: the phase a reader of the board sees, then the stage that
    // says what it is actually waiting on. (The `completed` entry's own label
    // carries the landing column verbatim, because the document must say what
    // the screen says — the facts grid is what a reader takes the status from.)
    assert!(
        html.contains("<dd>Working — In review</dd>"),
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
    let mut d = detail_at(Some(T0 + 60_000));
    d.timeline.truncate(1); // dispatched only, still running
    recompute(&mut d, T0 + 300_000);
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
    recompute(&mut d, T0);
    d.task.note = None;
    d.lineage = Lineage {
        parent: None,
        children: Vec::new(),
    };
    let html = render_document("Acme Co", &d, &[], T0);

    assert!(html.contains("Nothing has happened on this task yet"));
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

/// The document prints the host's figures, not its own.
///
/// The worked/waiting arithmetic moved onto `TaskDurations` so the console and
/// this renderer cannot disagree (#355 review); the merge and the window pairing
/// are tested there. What is left to pin here is that the renderer *reads* those
/// fields — a renderer that recomputed, or that ignored a live span, would still
/// pass every other test in this file.
#[test]
fn the_document_prints_the_hosts_figures() {
    let mut d = detail();
    // Numbers no correct derivation from this timeline would produce.
    d.durations = TaskDurations {
        worked_millis: 3_600_000,
        worked_live: false,
        waiting_millis: 900_000,
        waiting_live: false,
        as_of_millis: T0,
    };
    let html = render_document("Acme Co", &d, &[], T0 + 900_000);
    assert!(html.contains("1h 00m 00s"), "worked total not read: {html}");
    assert!(html.contains("15m 00s"), "waiting total not read: {html}");

    // A live span is extended from the host's instant, not recomputed.
    d.durations = TaskDurations {
        worked_millis: 60_000,
        worked_live: true,
        waiting_millis: 0,
        waiting_live: false,
        as_of_millis: T0,
    };
    let html = render_document("Acme Co", &d, &[], T0 + 60_000);
    assert!(html.contains("2m 00s"), "live worked not extended: {html}");
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
    // Every revision's *text*, not only the latest. The acceptance criterion is
    // "artifact versions and lineage"; printing a row per revision but the body
    // of one told a reader that revision 1 existed without ever saying what it
    // said (#355 review).
    assert!(
        html.contains("Hello world"),
        "the first revision's text is missing: {html}"
    );
    assert!(
        html.contains("Hello, world!"),
        "the final revision's text is missing"
    );
    assert!(html.contains("Revision 1") && html.contains("Revision 2"));
    // What the output is, so a reference-only artifact is not a bare path.
    assert!(html.contains("Markdown source"), "the kind is not named");
    assert!(
        html.contains("A person changed"),
        "the human edit is not explained"
    );
    assert!(html.contains("class=\"ins\"") && html.contains("class=\"del\""));
}

/// An `image` / `file` artifact says what it is rather than showing a bare path.
#[test]
fn a_reference_artifact_says_what_it_is() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord};
    use crate::server::ops::artifacts::ArtifactView;

    let view = ArtifactView::from(ArtifactRecord::new(
        "a-2",
        "t-1",
        "Launch banner",
        ArtifactKind::Image,
        "workspace://renders/banner.png",
        "designer",
        T0,
    ));
    let html = render_document(
        "Acme Co",
        &detail(),
        std::slice::from_ref(&view),
        T0 + 900_000,
    );
    assert!(
        html.contains("An image. The record carries where it is kept"),
        "an image artifact renders as a bare path: {html}"
    );
}

/// The artifact path escapes too — it is the largest agent-written surface in
/// the document, and the one this file did not cover (#355 review).
///
/// `author_id`, the version note, the body and each diff line are all
/// interpolated; any of them carrying a tag would turn an audit record into an
/// attack the moment somebody opens it in a browser.
#[test]
fn artifact_text_cannot_become_markup() {
    use crate::ports::artifacts::{ArtifactAuthor, ArtifactKind, ArtifactRecord};
    use crate::server::ops::artifacts::ArtifactView;

    let mut record = ArtifactRecord::new(
        "a-1",
        "t-1",
        "<title>Draft</title>",
        ArtifactKind::Markdown,
        "<script>first()</script>",
        "<b>writer</b>",
        T0,
    );
    record.push_version(
        "</div><script>second()</script>",
        ArtifactAuthor::Operator,
        "<i>alex</i>",
        T0 + 60_000,
        Some("<img src=x onerror=alert(1)>".to_string()),
    );
    let html = render_document(
        "Acme Co",
        &detail(),
        std::slice::from_ref(&ArtifactView::from(record)),
        T0 + 900_000,
    );

    for raw in [
        "<script>first()</script>",
        "<script>second()</script>",
        "<b>writer</b>",
        "<i>alex</i>",
        "<img src=x",
        "<title>Draft</title>",
    ] {
        assert!(!html.contains(raw), "artifact markup survived: {raw}");
    }
    // Escaped, not dropped — the reader still sees what was written.
    assert!(html.contains("&lt;script&gt;first()&lt;/script&gt;"));
    assert!(html.contains("&lt;b&gt;writer&lt;/b&gt; (company)"));
    assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
}

/// A pathological revision is cut, and the document says it was cut.
///
/// Every revision is printed now, so the body is the one input that scales with
/// the task's editing history. The reader must never be left believing they hold
/// the whole text when they do not.
#[test]
fn an_oversized_revision_is_bounded_and_says_so() {
    use crate::ports::artifacts::{ArtifactKind, ArtifactRecord};
    use crate::server::ops::artifacts::ArtifactView;

    let body = "x".repeat(MAX_BODY_CHARS + 500);
    let view = ArtifactView::from(ArtifactRecord::new(
        "a-3",
        "t-1",
        "Long draft",
        ArtifactKind::Text,
        body.as_str(),
        "writer",
        T0,
    ));
    let html = render_document(
        "Acme Co",
        &detail(),
        std::slice::from_ref(&view),
        T0 + 900_000,
    );
    assert!(html.len() < body.len() + 60_000, "the body was not bounded");
    assert!(
        html.contains("500 more characters were left out"),
        "the cut is silent: a reader cannot tell the text is incomplete"
    );
}

/// The header names where the card came from (#352 lists the origin link).
///
/// The card carries a chat id, not a URL, and this document is read far from any
/// console — so the record names the conversation rather than linking to it.
/// Absent on a board-created card, which has no origin at all.
#[test]
fn the_header_names_the_conversation_the_task_came_from() {
    let mut d = detail();
    d.task.origin_chat_id = Some("strategy".to_string());
    let html = render_document("Acme Co", &d, &[], T0 + 900_000);
    assert!(
        html.contains("<dt>Opened from</dt><dd>the strategy conversation</dd>"),
        "the origin is carried but never printed: {html}"
    );

    // A card the board created has no origin, and gets no empty row for one.
    let plain = render_document("Acme Co", &detail(), &[], T0 + 900_000);
    assert!(!plain.contains("Opened from"));

    // Whitespace is not an origin.
    let mut blank = detail();
    blank.task.origin_chat_id = Some("   ".to_string());
    let blank_html = render_document("Acme Co", &blank, &[], T0);
    assert!(!blank_html.contains("Opened from"));
}
