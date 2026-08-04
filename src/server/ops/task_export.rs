//! One task's record as a document a person can read (issue #352).
//!
//! `GET …/tasks/{task_id}/export` renders everything the Task Detail screen
//! shows — the header and the worked/waiting split, the ordered timeline with
//! its details expanded, the sign-offs, the artifact versions and their
//! human-edit diffs, and the neighbouring cards — as **one self-contained HTML
//! file**.
//!
//! ## Why HTML
//!
//! The bar in epic #184 is "a non-technical person can read it unaided", which
//! rules out the JSON the screen already fetches. Of the three candidates:
//!
//! * **HTML** opens by double-click in any browser on any machine, needs no
//!   reader to be installed and no tool to be explained, and can carry the
//!   *proportional waiting bands* the screen uses — a four-hour wait has to look
//!   different from a four-second one, which is the whole point of #305 and is
//!   exactly what a text format flattens away. It also prints: the reader's own
//!   browser turns it into the PDF a client asks for, with no PDF engine in this
//!   crate. And it costs **no new dependency** — the document is a `format!` and
//!   inlined CSS.
//! * **Markdown** loses the proportions and, opened by the non-technical reader
//!   this is *for*, shows raw `##` and `|` in a text editor rather than a
//!   document.
//! * **PDF** is what gets asked for by name, but generating one needs either a
//!   new rendering crate or a headless browser, and the browser's Print dialog
//!   already produces it from this file.
//!
//! ## Why the server renders it
//!
//! One implementation serves the console button *and* `curl`, so an audit
//! export can be scripted or scheduled without reimplementing the document in
//! whatever client wants it. A client-side renderer would exist only inside the
//! React view — the same place the record is stuck today.
//!
//! ## Redaction
//!
//! The document is rendered from [`assemble_detail`] and
//! [`artifacts_for_task`] — the *same values* the console's own reads return,
//! not a privileged path to the journal. `detail` text is scrubbed at source
//! before it ever reaches either caller, and nothing here re-reads the event
//! log. Everything interpolated is HTML-escaped, so a card titled
//! `<script>…</script>` renders as text in the exported file.
//!
//! Exporting is a pure read: no journal entry, no column change, no state.

use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::AppState;
use crate::ports::now_millis;
use crate::ports::tasks::column_label;
use crate::server::error::ApiError;
use crate::server::ops::artifacts::{ArtifactView, artifacts_for_task};
use crate::server::ops::tasks::{TaskDetail, TimelineEntry, assemble_detail};
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the export route fragment.
pub fn router() -> Router<AppState> {
    scoped("/tasks/{task_id}/export", get(export_task))
}

/// The `{task_id}` path segment.
#[derive(Debug, Deserialize)]
struct TaskPath {
    task_id: String,
}

/// `GET …/tasks/{task_id}/export` — the task's record as an HTML document.
///
/// Answers `text/html` with a `Content-Disposition: attachment` so a plain
/// `curl -OJ` (or a browser navigation) lands a named file rather than a page.
/// 404s when the id names no card, inherited from [`assemble_detail`].
async fn export_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<Response, ApiError> {
    let detail = assemble_detail(&company, &task_id).await?;
    let artifacts = artifacts_for_task(&company, &task_id).await?;
    // The company's display name, for a document that will be read far from the
    // console it came from. Falling back to the id keeps a bundle-less test or a
    // half-provisioned company exporting rather than failing.
    let company_name = company
        .runtime
        .store
        .load(company.id())
        .await?
        .map(|record| record.manifest.company.name)
        .unwrap_or_else(|| company.id().to_string());

    let html = render_document(&company_name, &detail, &artifacts, now_millis());
    let filename = format!("task-{}.html", slug(&detail.task.title, &detail.task.id));
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        html,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// The stylesheet, inlined so the file survives being emailed as one attachment.
///
/// Deliberately small and system-font based: no webfont to fetch, no colour that
/// fails to print, and a `@media print` block so Save-as-PDF drops the page
/// chrome rather than clipping it.
const STYLE: &str = r#"
:root { color-scheme: light; }
* { box-sizing: border-box; }
body { margin: 0; background: #f6f7f9; color: #14171a;
  font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }
main { max-width: 46rem; margin: 0 auto; padding: 2.5rem 1.25rem 4rem; }
header.doc { border-bottom: 2px solid #14171a; padding-bottom: 1rem; margin-bottom: 1.5rem; }
.eyebrow { margin: 0 0 .25rem; font-size: .75rem; letter-spacing: .08em; text-transform: uppercase; color: #5b6570; }
h1 { margin: 0 0 .4rem; font-size: 1.75rem; line-height: 1.25; }
.sub { margin: 0; color: #5b6570; font-size: .85rem; }
h2 { margin: 2.25rem 0 .35rem; font-size: 1.05rem; }
.lede { margin: 0 0 .85rem; color: #5b6570; font-size: .85rem; }
dl.facts { display: grid; grid-template-columns: repeat(auto-fit, minmax(11rem, 1fr));
  gap: .75rem 1.25rem; margin: 0; padding: 1rem 1.15rem; background: #fff;
  border: 1px solid #dfe3e8; border-radius: .6rem; }
dl.facts dt { font-size: .72rem; letter-spacing: .06em; text-transform: uppercase; color: #5b6570; }
dl.facts dd { margin: .15rem 0 0; font-size: .95rem; font-weight: 600; }
.card { background: #fff; border: 1px solid #dfe3e8; border-radius: .6rem; padding: .7rem .9rem; }
ol.entries { list-style: none; margin: 0; padding: 0; }
ol.entries > li { margin-bottom: .5rem; }
.entry { display: flex; gap: .75rem; align-items: baseline; }
.when { flex: none; width: 6.5rem; font-variant-numeric: tabular-nums; font-size: .78rem; color: #5b6570; }
.what { flex: 1; min-width: 0; font-weight: 600; }
.detail { margin: .45rem 0 0 7.25rem; padding: .55rem .7rem; background: #f2f4f7;
  border-left: 3px solid #c9d0d8; border-radius: .25rem; white-space: pre-wrap;
  overflow-wrap: anywhere; font-size: .85rem; color: #2b3138; }
.wait { display: flex; align-items: center; justify-content: center; margin-bottom: .5rem;
  border: 1px dashed #d9a441; background: #fdf6e6; border-radius: .5rem;
  color: #8a5b00; font-size: .78rem; font-weight: 600; }
.tone-failed .what { color: #a11b2b; }
.tone-done .what { color: #1a6c3c; }
.muted { color: #5b6570; font-size: .88rem; }
table.versions { width: 100%; border-collapse: collapse; font-size: .85rem; }
table.versions th, table.versions td { text-align: left; padding: .35rem .5rem; border-bottom: 1px solid #e6e9ed; }
table.versions th { font-size: .72rem; letter-spacing: .06em; text-transform: uppercase; color: #5b6570; }
.body { margin: .6rem 0 0; padding: .6rem .75rem; background: #f2f4f7; border-radius: .35rem;
  white-space: pre-wrap; overflow-wrap: anywhere; font-size: .85rem; }
.diff { margin: .6rem 0 0; border: 1px solid #dfe3e8; border-radius: .35rem; overflow: hidden; font-size: .8rem; }
.diff div { padding: .12rem .6rem; white-space: pre-wrap; overflow-wrap: anywhere; }
.diff .ins { background: #e7f6ec; }
.diff .del { background: #fdeaea; text-decoration: line-through; }
.diff .eq { color: #5b6570; }
footer.doc { margin-top: 3rem; padding-top: 1rem; border-top: 1px solid #dfe3e8;
  color: #5b6570; font-size: .78rem; }
@media print {
  body { background: #fff; }
  main { max-width: none; padding: 0; }
  .card, dl.facts, .diff { border-color: #bbb; }
  h2 { break-after: avoid; }
  ol.entries > li, .artifact { break-inside: avoid; }
}
"#;

/// Renders the whole document.
///
/// Pure: everything it needs is in its arguments, including `now`, so the output
/// is deterministic and the tests assert against a fixed clock.
fn render_document(
    company_name: &str,
    detail: &TaskDetail,
    artifacts: &[ArtifactView],
    now: u64,
) -> String {
    let task = &detail.task;
    let worked = worked_millis(&detail.timeline, now);
    let waiting = waiting_millis(&detail.timeline, detail.waiting_since, now);

    let mut out = String::with_capacity(8 * 1024);
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str(&format!(
        "<title>Task record: {}</title>\n",
        escape(&task.title)
    ));
    out.push_str("<style>");
    out.push_str(STYLE);
    out.push_str("</style>\n</head>\n<body>\n<main>\n");

    // --- header ------------------------------------------------------------
    out.push_str("<header class=\"doc\">\n<p class=\"eyebrow\">Task record</p>\n");
    out.push_str(&format!("<h1>{}</h1>\n", escape(&task.title)));
    out.push_str(&format!(
        "<p class=\"sub\">{} &middot; exported {}</p>\n</header>\n",
        escape(company_name),
        escape(&format_utc(now))
    ));

    // --- the facts anyone reading this needs first --------------------------
    out.push_str("<dl class=\"facts\">\n");
    fact(&mut out, "Status", column_label(&task.column));
    fact(&mut out, "Priority", &sentence_case(&task.priority));
    fact(
        &mut out,
        "Worked on by",
        if task.assignee.trim().is_empty() {
            "Nobody yet"
        } else {
            task.assignee.as_str()
        },
    );
    fact(&mut out, "Last updated", &format_utc(task.updated_at));
    fact(&mut out, "Time worked", &format_duration(worked.millis));
    fact(
        &mut out,
        "Waiting on a person",
        &format_duration(waiting.millis),
    );
    out.push_str("</dl>\n");
    if waiting.live {
        out.push_str(
            "<p class=\"lede\">This task is waiting on a person right now, so the two \
             durations above are still counting.</p>\n",
        );
    }

    if let Some(note) = task
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        out.push_str("<h2>What was asked for</h2>\n");
        out.push_str(&format!(
            "<div class=\"card body\">{}</div>\n",
            escape(note)
        ));
    }

    render_timeline(&mut out, detail, now);
    render_approvals(&mut out, &detail.timeline);
    render_artifacts(&mut out, artifacts);
    render_lineage(&mut out, detail);

    // --- footer -------------------------------------------------------------
    out.push_str("<footer class=\"doc\">\n");
    out.push_str(&format!(
        "<p>Exported from {} on {}. Times are UTC. This is a copy of the record as it \
         stood at that moment; exporting it changed nothing about the task.</p>\n",
        escape(company_name),
        escape(&format_utc(now))
    ));
    out.push_str(&format!("<p>Reference: {}</p>\n", escape(&task.id)));
    out.push_str("</footer>\n</main>\n</body>\n</html>\n");
    out
}

/// One `<dt>`/`<dd>` pair in the facts grid.
fn fact(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!(
        "<dt>{}</dt><dd>{}</dd>\n",
        escape(label),
        escape(value)
    ));
}

/// The ordered history, with every detail expanded and waiting rendered as space.
///
/// The console collapses repeated failures into one row with a count and hides
/// details behind a disclosure. A document has no disclosure to click, and a
/// reader who was handed this file cannot go and expand anything — so everything
/// is printed, in order, once.
fn render_timeline(out: &mut String, detail: &TaskDetail, now: u64) {
    out.push_str("<h2>What happened, in order</h2>\n");
    out.push_str(
        "<p class=\"lede\">Every step this task went through. Shaded blocks are periods \
         when the company was waiting for a person, drawn taller the longer the wait.</p>\n",
    );
    if detail.timeline.is_empty() && detail.waiting_since.is_none() {
        out.push_str(
            "<p class=\"card muted\">Nothing has happened on this task yet. It has not \
             been started.</p>\n",
        );
        return;
    }

    out.push_str("<ol class=\"entries\">\n");
    for entry in &detail.timeline {
        // A resolved sign-off is rendered as the wait it caused, then the
        // decision itself — the same order the screen shows them in.
        if let Some(waited) = entry.waited_millis.filter(|w| *w > 0) {
            wait_band(
                out,
                &format!("Waited {} for a person", format_duration(waited)),
                waited,
            );
        }
        out.push_str(&format!(
            "<li class=\"card {}\">\n",
            tone_class(&entry.kind)
        ));
        out.push_str(&format!(
            "<div class=\"entry\"><span class=\"when\">{}</span><span class=\"what\">{}</span></div>\n",
            escape(&clock_utc(entry.at_millis)),
            escape(&entry.label)
        ));
        if let Some(text) = entry
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            out.push_str(&format!("<div class=\"detail\">{}</div>\n", escape(text)));
        }
        out.push_str("</li>\n");
    }
    if let Some(since) = detail.waiting_since {
        let live = now.saturating_sub(since);
        wait_band(
            out,
            &format!(
                "Still waiting on a person, {} so far",
                format_duration(live)
            ),
            live,
        );
    }
    out.push_str("</ol>\n");
}

/// A waiting period, drawn at the same proportional height the screen uses.
fn wait_band(out: &mut String, label: &str, millis: u64) {
    out.push_str(&format!(
        "<li class=\"wait\" style=\"min-height:{}px\">{}</li>\n",
        waiting_band_height(millis),
        escape(label)
    ));
}

/// The sign-offs section.
///
/// **Until #333 lands** (PR #349) an approval carries no task id, so the only
/// approvals that can be attributed to a card are the resolutions that fell
/// inside its run window — which is what the timeline's `approval` entries
/// already are, and what this section prints, with the correlation stated
/// plainly rather than implied. When #333 merges, `TaskDetail` gains an
/// `approvals[]` array keyed by a real id, and this section should read that
/// instead: the honesty note goes away with it, and pending sign-offs (which
/// have no resolution event and therefore cannot appear here at all today)
/// start showing up.
fn render_approvals(out: &mut String, timeline: &[TimelineEntry]) {
    let approvals: Vec<&TimelineEntry> = timeline.iter().filter(|e| e.kind == "approval").collect();
    out.push_str("<h2>Sign-offs</h2>\n");
    if approvals.is_empty() {
        out.push_str(
            "<p class=\"card muted\">No decision needed a person's approval while this \
             task ran.</p>\n",
        );
        return;
    }
    out.push_str(
        "<p class=\"lede\">Decisions a person was asked to approve while this task was \
         being worked on. These are matched to the period the task ran in, so a sign-off \
         given for other work in the same period can appear here too.</p>\n",
    );
    out.push_str("<ol class=\"entries\">\n");
    for entry in approvals {
        out.push_str("<li class=\"card\">\n<div class=\"entry\">");
        out.push_str(&format!(
            "<span class=\"when\">{}</span><span class=\"what\">{}</span>",
            escape(&clock_utc(entry.at_millis)),
            escape(&entry.label)
        ));
        if let Some(waited) = entry.waited_millis {
            out.push_str(&format!(
                "<span class=\"when\">after {}</span>",
                escape(&format_duration(waited))
            ));
        }
        out.push_str("</div>\n</li>\n");
    }
    out.push_str("</ol>\n");
}

/// The outputs section: every artifact, its revisions, its final text, and the
/// agent-to-human diff when somebody edited it.
fn render_artifacts(out: &mut String, artifacts: &[ArtifactView]) {
    out.push_str("<h2>What this task produced</h2>\n");
    if artifacts.is_empty() {
        out.push_str("<p class=\"card muted\">This task recorded no output.</p>\n");
        return;
    }
    out.push_str(
        "<p class=\"lede\">Each output, with every revision it went through. Where a \
         person edited what the company wrote, the change is shown line by line.</p>\n",
    );
    for view in artifacts {
        let a = &view.artifact;
        out.push_str("<div class=\"card artifact\" style=\"margin-bottom:.75rem\">\n");
        out.push_str(&format!(
            "<div class=\"entry\"><span class=\"what\">{}</span><span class=\"when\">{}</span></div>\n",
            escape(&a.title),
            escape(&format_utc(a.updated_at_millis))
        ));

        out.push_str("<table class=\"versions\">\n<tr><th>Revision</th><th>Written by</th><th>When</th><th>Why</th></tr>\n");
        for v in &a.versions {
            let who = match v.author {
                crate::ports::artifacts::ArtifactAuthor::Agent => {
                    format!("{} (company)", v.author_id)
                }
                crate::ports::artifacts::ArtifactAuthor::Operator => {
                    format!("{} (person)", v.author_id)
                }
            };
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                v.version,
                escape(&who),
                escape(&format_utc(v.created_at_millis)),
                escape(v.note.as_deref().unwrap_or("")),
            ));
        }
        out.push_str("</table>\n");

        if let Some(latest) = a.versions.last() {
            out.push_str(&format!(
                "<div class=\"body\">{}</div>\n",
                escape(&latest.body)
            ));
        }

        if let Some(diff) = &view.human_edit_diff {
            out.push_str(&format!(
                "<p class=\"lede\" style=\"margin-top:.7rem\">A person changed {}% of what \
                 the company wrote, between revision {} and revision {}: {} lines added, {} \
                 removed.</p>\n",
                (diff.churn * 100.0).round() as u64,
                diff.from_version,
                diff.to_version,
                diff.added,
                diff.removed
            ));
            out.push_str("<div class=\"diff\">\n");
            for line in &diff.lines {
                let (class, prefix) = match line.op {
                    crate::ports::artifacts::DiffOp::Insert => ("ins", "+ "),
                    crate::ports::artifacts::DiffOp::Delete => ("del", "- "),
                    crate::ports::artifacts::DiffOp::Equal => ("eq", "  "),
                };
                out.push_str(&format!(
                    "<div class=\"{class}\">{}{}</div>\n",
                    prefix,
                    escape(&line.text)
                ));
            }
            out.push_str("</div>\n");
        }
        out.push_str("</div>\n");
    }
}

/// Parent and children, by title — never by id, which would mean nothing to the
/// reader this document is for.
fn render_lineage(out: &mut String, detail: &TaskDetail) {
    let lineage = &detail.lineage;
    if lineage.parent.is_none() && lineage.children.is_empty() {
        return;
    }
    out.push_str("<h2>Related work</h2>\n");
    out.push_str("<ul class=\"card\" style=\"margin:0;padding-left:1.2rem\">\n");
    if let Some(parent) = &lineage.parent {
        out.push_str(&format!(
            "<li>Came out of: {} <span class=\"muted\">({})</span></li>\n",
            escape(&parent.title),
            escape(column_label(&parent.column))
        ));
    }
    for child in &lineage.children {
        out.push_str(&format!(
            "<li>Led to: {} <span class=\"muted\">({})</span></li>\n",
            escape(&child.title),
            escape(column_label(&child.column))
        ));
    }
    out.push_str("</ul>\n");
}

/// Colours the entry by what it was, so a failure does not read like a reply.
fn tone_class(kind: &str) -> &'static str {
    match kind {
        "tool_failed" => "tone-failed",
        "completed" => "tone-done",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Arithmetic mirrored from the console (#305)
// ---------------------------------------------------------------------------

/// A duration and whether it is still running.
struct Span {
    millis: u64,
    live: bool,
}

/// The task's worked time: each `dispatched` opens a window its `completed`
/// closes, re-dispatch opens another, and an open window runs to `now`.
///
/// Mirrors `workedMillis` in `frontend/src/views/TaskDetailView.tsx`. The two
/// are a hand-maintained pair: the screen must not be able to say "worked 4m"
/// while the exported record of the same task says something else, and a Rust
/// test cannot see the TS function.
fn worked_millis(timeline: &[TimelineEntry], now: u64) -> Span {
    let mut total = 0u64;
    let mut open_at: Option<u64> = None;
    for e in timeline {
        match e.kind.as_str() {
            "dispatched" => open_at = Some(e.at_millis),
            // A `completed` with no open window is a card journaled before
            // dispatch anchors existed: skipped, never counted from zero.
            "completed" => {
                if let Some(opened) = open_at.take() {
                    total += e.at_millis.saturating_sub(opened);
                }
            }
            _ => {}
        }
    }
    let live = open_at.is_some();
    if let Some(opened) = open_at {
        total += now.saturating_sub(opened);
    }
    Span {
        millis: total,
        live,
    }
}

/// The task's waiting-on-a-person time, interval-merged then summed.
///
/// Mirrors `waitingMillis` in `frontend/src/views/TaskDetailView.tsx`, including
/// the merge: two approvals parked at once mean the company waited *once* over
/// the overlap, and double-counting could make waiting exceed the elapsed time
/// it is compared against.
fn waiting_millis(timeline: &[TimelineEntry], waiting_since: Option<u64>, now: u64) -> Span {
    let mut spans: Vec<(u64, u64)> = timeline
        .iter()
        .filter(|e| e.kind == "approval")
        // `None` means the host could not recover the park instant and `0` is a
        // real instant sign-off. Neither is a span.
        .filter_map(|e| {
            e.waited_millis
                .filter(|w| *w > 0)
                .map(|w| (e.at_millis.saturating_sub(w), e.at_millis))
        })
        .collect();
    let live = waiting_since.is_some();
    if let Some(since) = waiting_since {
        spans.push((since, now.max(since)));
    }
    spans.sort_unstable();

    let mut total = 0u64;
    let mut cursor: Option<(u64, u64)> = None;
    for span in spans {
        match cursor {
            Some((start, end)) if span.0 <= end => cursor = Some((start, end.max(span.1))),
            Some((start, end)) => {
                total += end - start;
                cursor = Some(span);
            }
            None => cursor = Some(span),
        }
    }
    if let Some((start, end)) = cursor {
        total += end - start;
    }
    Span {
        millis: total,
        live,
    }
}

/// The pixel height of a waiting band, mirroring `waitingBandHeight` in the
/// console: a log curve with a 12px floor and a 112px cap, so four seconds and
/// four hours are both on the page and visibly different.
fn waiting_band_height(millis: u64) -> u64 {
    let minutes = millis as f64 / 60_000.0;
    let raw = 12.0 + 26.0 * (1.0 + minutes).log2();
    raw.round().clamp(12.0, 112.0) as u64
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `4 Aug 2026, 14:32 UTC` — a date a person reads, not an epoch.
///
/// UTC and stated as such. A server-rendered document has no access to the
/// reader's zone, and a bare local-looking time on a record that may cross
/// borders is worse than an explicit one.
fn format_utc(millis: u64) -> String {
    let t = crate::runtime::cron::CivilTime::from_unix_millis(millis);
    let month = MONTHS
        .get((t.month.max(1) - 1) as usize)
        .copied()
        .unwrap_or("Jan");
    format!(
        "{} {} {}, {:02}:{:02} UTC",
        t.day, month, t.year, t.hour, t.minute
    )
}

/// `14:32:09` — the time of day, for a timeline row where the date is context.
fn clock_utc(millis: u64) -> String {
    let t = crate::runtime::cron::CivilTime::from_unix_millis(millis);
    let seconds = (millis % 60_000) / 1000;
    format!("{:02}:{:02}:{:02}", t.hour, t.minute, seconds)
}

/// `1h 04m 09s` / `4m 09s` / `9s`, matching the console's `formatDuration`.
fn format_duration(millis: u64) -> String {
    let s = millis / 1000;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// `high` → `High`. The board stores wire words; a document prints words.
fn sentence_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Escapes text for interpolation into the document.
///
/// Every value that reaches the template goes through this — a card title, a
/// timeline label, an agent's reply, an artifact body. Without it a task called
/// `<img onerror=…>` would become live markup in a file that gets opened in a
/// browser and forwarded to a client.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A filesystem-safe stem for the downloaded file, from the card's title.
///
/// Falls back to the id when a title reduces to nothing (emoji, punctuation),
/// so the attachment always has a name.
fn slug(title: &str, id: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars().take(60) {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { sanitize_id(id) } else { out }
}

/// The id reduced to safe filename characters, for the slug fallback.
fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    if cleaned.is_empty() {
        "record".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod test;
