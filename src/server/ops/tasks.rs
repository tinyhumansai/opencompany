//! Task board reads and writes: `POST /tasks`, `GET /tasks`,
//! `GET /tasks/{task_id}`, `PATCH /tasks/{task_id}`, `DELETE /tasks/{task_id}`
//! under both scope forms.
//!
//! Bodies mirror the console's `TaskCard` (`frontend/src/lib/tasks-sample.ts`)
//! in camelCase; the `assignee` is a plain desk/teammate label. Writes land in
//! the [`TaskStore`](crate::ports::TaskStore).
//!
//! `GET /tasks/{task_id}` (issue #185) is the Task Detail screen's read
//! foundation: it assembles the card header, the per-task timeline, the
//! lineage, and the approvals trail into one response so the console makes a
//! single call. See [`task_detail`] for the assembly and its scrub discipline.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::steer::{InflightEntry, SteerAction, SteerError, cap_redirect};
use crate::error::OpenCompanyError;
use crate::ports::tasks::{BOARD_COLUMNS, COLUMN_TODO, TaskRecord, is_board_column};
use crate::ports::types::CompanyEvent;
use crate::ports::{generate_id, now_millis};
use crate::runtime::assignee;
use crate::server::error::ApiError;
use crate::server::ops::runs::{RunSummary, runs_for_task};
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the task route fragment.
pub fn router() -> Router<AppState> {
    scoped("/tasks", post(create_task).get(list_tasks))
        // The static `/tasks/inflight` GET is registered before the dynamic
        // `/tasks/{task_id}`, so the operator strip's read never collides with
        // a card id.
        //
        // That ordering used to be belt-and-braces — `{task_id}` carried no GET
        // at all. Issue #185 gave it one (`task_detail`), so the two now
        // genuinely overlap on `GET /tasks/inflight` and the static segment
        // winning is load-bearing, not incidental. Axum's router prefers a
        // static segment over a parameter, and
        // `inflight_read_is_not_shadowed_by_task_detail` pins it — a card can
        // never be named `inflight`, so a regression here would silently turn
        // the operator strip into a 404.
        .merge(scoped("/tasks/inflight", get(list_inflight)))
        .merge(scoped(
            "/tasks/{task_id}",
            get(task_detail).patch(patch_task).delete(delete_task),
        ))
        .merge(scoped("/tasks/{task_id}/steer", post(steer_task)))
}

/// A task card as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskCard {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    column: String,
    priority: String,
    assignee: String,
    updated_at: u64,
    /// The card this one was spawned from (#185). Omitted on a lineage root so
    /// the board's existing wire shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_task_id: Option<String>,
    /// The chat thread this card was opened from (issue #246).
    ///
    /// `TaskRecord::origin_chat_id` has existed since #151 (it is what lets a
    /// completed run answer in the conversation that asked), and the tool-spawn
    /// path stamps it — but it was never *readable*: no DTO projected it, so
    /// task detail could not show where a card came from. Omitted when absent,
    /// which is every card the board created before this, so the existing wire
    /// shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_chat_id: Option<String>,
}

impl From<TaskRecord> for TaskCard {
    fn from(t: TaskRecord) -> Self {
        Self {
            id: t.id,
            title: t.title,
            note: t.note,
            column: t.column,
            priority: t.priority,
            assignee: t.assignee,
            updated_at: t.updated_at_millis,
            parent_task_id: t.parent_task_id,
            origin_chat_id: t.origin_chat_id,
        }
    }
}

/// The create-task body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTask {
    title: String,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    column: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    /// Opens this card as a child of an existing one (#185). Absent for a
    /// lineage root, which is every card the board creates today.
    #[serde(default)]
    parent_task_id: Option<String>,
    /// The chat thread this card is being opened from (issue #246).
    ///
    /// Set by the transcript's "Add to board" action, which is the one creation
    /// entry point that *has* an originating conversation; the board's `+`
    /// button omits it. Absent is the previous behaviour and stays the default,
    /// so no existing caller changes.
    #[serde(default)]
    origin_chat_id: Option<String>,
}

/// The partial patch body (any subset; a drag sends `{column}`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchTask {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    column: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    /// Re-parents the card (#185). Omitting it leaves lineage untouched — the
    /// same partial-patch contract every other field here follows.
    #[serde(default)]
    parent_task_id: Option<String>,
}

/// The sub-resource path (`task_id`); the scope `id` is consumed by the extractor.
#[derive(Debug, Deserialize)]
struct TaskPath {
    task_id: String,
}

/// `GET …/tasks` — the whole board, newest-updated first. The console reads
/// this to render the Kanban columns and each card's detail (note, assignee).
async fn list_tasks(company: ScopedCompany) -> Result<Json<Vec<TaskCard>>, ApiError> {
    let mut rows = company.runtime.tasks().list(company.id()).await?;
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at_millis));
    Ok(Json(rows.into_iter().map(TaskCard::from).collect()))
}

/// Validates a proposed `parent_task_id` against the board.
///
/// Rejects three things, all at the write boundary — the cheap place to keep
/// the lineage a forest rather than discovering it is not one on read:
///
/// * **self-parenting**, which would make a card its own parent *and* its own
///   child in `task_detail`;
/// * a parent that **names no existing card**, which yields a dangling edge and
///   a lineage whose `parent` silently reads as `None`;
/// * a **cycle** (`t1 → t2 → t1`). Nothing hangs today because `task_detail`
///   walks a single level, but a persisted cycle is a latent trap for any
///   future consumer that does walk the chain — a rollup, a breadcrumb.
///
/// `child` is the id being parented — `None` on create, where the new card has
/// no id on the board yet and therefore cannot be part of a cycle.
fn validate_parent(
    parent_task_id: &str,
    child: Option<&str>,
    board: &[TaskRecord],
) -> Result<(), ApiError> {
    if Some(parent_task_id) == child {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "a task cannot be its own parent".to_string(),
        )));
    }
    if !board.iter().any(|t| t.id == parent_task_id) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "parent task {parent_task_id} does not exist"
        ))));
    }
    let Some(child) = child else {
        return Ok(());
    };
    // Walk up from the proposed parent. Reaching `child` means the new edge
    // would close a loop. The visited set bounds the walk even if the stored
    // board already contains a cycle from before this validation existed.
    let mut seen = std::collections::HashSet::new();
    let mut cursor = Some(parent_task_id.to_string());
    while let Some(id) = cursor {
        if id == child {
            return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
                "parent task {parent_task_id} would create a cycle through {child}"
            ))));
        }
        if !seen.insert(id.clone()) {
            break;
        }
        cursor = board
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.parent_task_id.clone());
    }
    Ok(())
}

/// Rejects a `column` the board does not render (issue #205).
///
/// `column` is a free string on the wire, and nothing checked it: a typo'd
/// `"in-progress"` was persisted verbatim, so the card disappeared from every
/// rendered column *and* — since only the exact literal `in_progress`
/// edge-fires a dispatch — silently never ran. Refusing at the write boundary
/// is the cheap place to keep the board's six columns the only six.
fn validate_column(column: &str) -> Result<(), ApiError> {
    if is_board_column(column) {
        return Ok(());
    }
    Err(ApiError(OpenCompanyError::InvalidRequest(format!(
        "\"{column}\" is not a board column — use one of: {}",
        BOARD_COLUMNS.join(", ")
    ))))
}

/// Resolves an `assignee` against the company roster, returning the canonical
/// id to store — and rejecting one that names nobody (issue #205).
///
/// The board's Assignee field is free text, so "Shane" — a name this company
/// has never had — used to be persisted verbatim and then silently dispatched
/// to the orchestrator. A teammate id, a desk (by id or name), and blank are
/// all accepted; a desk with no members yet is accepted too, because assigning
/// work to a desk you are about to staff is legitimate (dispatch is where that
/// one is refused, and it says why).
///
/// What is stored is the **canonical** key rather than what was typed, so the
/// board's assignee column is one namespace of real ids and every downstream
/// reader matches on the same string.
///
/// A company whose record has not been persisted yet has no roster to check
/// against, so the value passes through unvalidated rather than being guessed
/// at — the same permissive stance the rest of the write plane takes toward an
/// unsaved record.
async fn resolve_assignee(company: &ScopedCompany, assignee: String) -> Result<String, ApiError> {
    let Some(record) = company.runtime.store().load(company.id()).await? else {
        return Ok(assignee);
    };
    let resolution = assignee::resolve(&record, &assignee);
    match resolution.canonical() {
        Some(canonical) => Ok(canonical.to_string()),
        None => Err(ApiError(OpenCompanyError::InvalidRequest(
            resolution
                .rejection()
                .unwrap_or_else(|| format!("\"{assignee}\" is not on this company's roster")),
        ))),
    }
}

async fn create_task(
    company: ScopedCompany,
    Json(body): Json<CreateTask>,
) -> Result<Json<TaskCard>, ApiError> {
    // Read → validate → write is one critical section. Two concurrent requests
    // that each read the board before either has written would both validate
    // against a snapshot missing the other's edge, and could persist a lineage
    // neither request would have been allowed to create on its own.
    let _serialized = company.runtime.task_writes.lock().await;
    if let Some(parent) = body.parent_task_id.as_deref() {
        let board = company.runtime.tasks().list(company.id()).await?;
        // `None` child: the card does not exist yet, so it cannot be in a cycle.
        validate_parent(parent, None, &board)?;
    }
    // Issue #205: validated before anything is written, so a bad column or a
    // bad assignee is a `400` the operator sees rather than a card that lands
    // somewhere the board cannot render or hands work to a name nobody answers to.
    // Issue #206: a card created here is manual entry — the board's `+` button,
    // which lives on To-do alone — so that is the default. Issue #301 made it
    // the *only* not-started column: the separate `backlog` pool was collapsed
    // into To-do, and every lifecycle return now lands here too, carrying its
    // reason on the note. So this default is no longer a choice between two
    // columns; it is simply where not-started work lives.
    let column = body.column.unwrap_or_else(|| COLUMN_TODO.to_string());
    validate_column(&column)?;
    let assignee = resolve_assignee(&company, body.assignee.unwrap_or_default()).await?;
    let record = TaskRecord {
        id: generate_id(),
        title: body.title,
        note: body.note,
        column,
        priority: body.priority.unwrap_or_else(|| "medium".to_string()),
        assignee,
        updated_at_millis: now_millis(),
        // Issue #246: provenance is now carried, not dropped. This was
        // hardcoded `None` while the tool-spawn path stamped the same field
        // correctly, so a card opened from a conversation had no way back to it
        // — and #151's "answer where you were asked" post-back could never fire
        // for anything the REST surface created. A blank string is normalised
        // away so an empty form field cannot persist as a thread id that
        // matches nothing.
        origin_chat_id: body
            .origin_chat_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty()),
        parent_task_id: body.parent_task_id,
    };
    company.runtime.upsert_task(&record).await?;
    Ok(Json(record.into()))
}

async fn patch_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
    Json(body): Json<PatchTask>,
) -> Result<Json<TaskCard>, ApiError> {
    // As in `create_task`, the read → validate → write is one critical section:
    // a re-parent validated against a stale board is exactly how two requests
    // close a cycle neither of them could close alone. Held for the whole
    // handler, which also makes the surrounding read-modify-write of the card's
    // other fields lost-update free.
    let _serialized = company.runtime.task_writes.lock().await;
    // The whole board is kept (not consumed by `into_iter`) so a re-parent can
    // be checked for existence and cycles against it.
    let board = company.runtime.tasks().list(company.id()).await?;
    let mut record = board
        .iter()
        .find(|t| t.id == task_id)
        .cloned()
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("task {task_id}")))?;
    if let Some(title) = body.title {
        record.title = title;
    }
    if let Some(note) = body.note {
        record.note = Some(note);
    }
    // Issue #205: a patch is validated on the same terms as a create. `record`
    // is a local clone that is only upserted once every field has been applied,
    // so returning the `400` from here discards the partial edit rather than
    // persisting half of it.
    if let Some(column) = body.column {
        validate_column(&column)?;
        record.column = column;
    }
    if let Some(priority) = body.priority {
        record.priority = priority;
    }
    if let Some(assignee) = body.assignee {
        record.assignee = resolve_assignee(&company, assignee).await?;
    }
    if let Some(parent_task_id) = body.parent_task_id {
        validate_parent(&parent_task_id, Some(&task_id), &board)?;
        record.parent_task_id = Some(parent_task_id);
    }
    record.updated_at_millis = now_millis();
    company.runtime.upsert_task(&record).await?;
    Ok(Json(record.into()))
}

async fn delete_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<StatusCode, ApiError> {
    // Serialized with the other board writes so a delete cannot land between a
    // concurrent re-parent's existence check and its write, which would leave
    // the dangling edge `validate_parent` exists to prevent.
    let _serialized = company.runtime.task_writes.lock().await;
    if company
        .runtime
        .tasks()
        .delete(company.id(), &task_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "task {task_id}"
        ))))
    }
}

// ---------------------------------------------------------------------------
// Task detail (issue #185): the Task Detail screen's read foundation.
// ---------------------------------------------------------------------------

/// One entry on a task's timeline.
///
/// Deliberately the *same scrubbed vocabulary* as [`TurnStep`] rather than a
/// raw event dump: `kind` / `status` / `label` / `detail` are already the shape
/// the console renders for a chat bubble's steps, so the Task Detail screen
/// reuses that renderer instead of growing a second one.
///
/// Nothing here can carry raw tool arguments, tool output, or call ids. The
/// only free text that reaches `detail` is a value the producing event already
/// scrubbed at source (`McpCallFailed.message`) or the agent's own reply text
/// (`DeskTaskCompleted.output`), which is the same string already shown in the
/// card's note.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimelineEntry {
    /// The journal sequence this entry came from — the console's stable key,
    /// and what makes the timeline strictly ordered.
    seq: u64,
    /// Epoch-millis the event was journaled.
    at_millis: u64,
    /// A stable wire word for what happened: `dispatched`, `reply`,
    /// `tool_failed`, `approval`, or `completed`.
    kind: String,
    /// A short human label.
    label: String,
    /// Optional scrubbed detail (see the type docs for what may appear here).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// For an `approval` entry: how long the company sat waiting on the
    /// operator before this resolution landed (issue #305).
    ///
    /// Recovered by joining the resolution's `approval_id` against the runtime
    /// journal's park instants — the park time is journal-only, so the event log
    /// alone cannot answer it. Omitted, rather than zeroed, when the park
    /// instant is unknown (an approval parked by a build older than the index):
    /// the console then renders the row exactly as it did before, and a wait is
    /// never fabricated from a gap between timeline rows.
    ///
    /// Clamped to the run window's opening, so an approval that was already
    /// parked when this task was dispatched charges only the part of its wait
    /// that overlapped this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    waited_millis: Option<u64>,
}

/// A neighbouring card in the lineage, trimmed to what a link needs.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LineageRef {
    id: String,
    title: String,
    column: String,
}

impl From<&TaskRecord> for LineageRef {
    fn from(t: &TaskRecord) -> Self {
        Self {
            id: t.id.clone(),
            title: t.title.clone(),
            column: t.column.clone(),
        }
    }
}

/// The parent/children view of a task.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Lineage {
    /// The card this one was spawned from, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<LineageRef>,
    /// Cards spawned from this one, oldest-updated first for a stable render.
    children: Vec<LineageRef>,
}

/// The assembled Task Detail response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskDetail {
    /// The card header — the same shape `GET /tasks` returns per card.
    task: TaskCard,
    /// The per-task event stream, oldest first.
    timeline: Vec<TimelineEntry>,
    /// Parent and children.
    lineage: Lineage,
    /// The card's recorded attempts, newest first (issue #242).
    ///
    /// Additive: a card dispatched before run records existed legitimately
    /// carries an empty list, because synthesising attempts from old
    /// `AgentReply` events would fabricate identity. Bounded by
    /// [`TASK_DETAIL_RUN_LIMIT`](crate::server::ops::runs::TASK_DETAIL_RUN_LIMIT)
    /// — a card can be re-dispatched without limit, and this read stays one
    /// cheap call.
    runs: Vec<RunSummary>,
    /// Epoch-millis the company started waiting on an operator *right now*
    /// (issue #305), or `None` when nothing is currently parked for this run.
    ///
    /// This is the live half of the working-vs-waiting split: a still-open
    /// approval has no `ApprovalResolved` event yet, so it cannot appear as a
    /// timeline entry, yet it is precisely the state an operator opening this
    /// screen most needs to see. Set only while the run window is open, and
    /// only from approvals parked at or after the window opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    waiting_since: Option<u64>,
}

/// `GET …/tasks/{task_id}` — the Task Detail screen's single read (issue #185).
///
/// Assembles four things the console would otherwise have to stitch client-side
/// (and could not, for the journal-derived halves):
///
/// * **header** — the card itself;
/// * **timeline** — the company-scoped journal filtered down to this task. The
///   [`TaskDispatched`](CompanyEvent::TaskDispatched) and
///   [`DeskTaskCompleted`](CompanyEvent::DeskTaskCompleted) anchors match on
///   their own `task_id`; the events a dispatch *produces* (replies, failed
///   tool calls) match on the `task_id` threaded onto them by this same issue.
///   Without that threading these events are indistinguishable from every other
///   desk reply in the log, which is exactly why the filter could not be built
///   before;
/// * **approvals trail** — [`ApprovalResolved`](CompanyEvent::ApprovalResolved)
///   events that fall inside the task's run window (its dispatch anchor through
///   its completion anchor, or through the end of the log while it is still
///   running). Parked effects carry no task id, so a window is the honest
///   correlation here rather than a false-precision per-task link; entries are
///   labelled as such so a reader is not misled;
/// * **lineage** — parent and children, from `parent_task_id`.
///
/// 404s when the id names no card, matching `PATCH` / `DELETE`.
async fn task_detail(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<Json<TaskDetail>, ApiError> {
    let rows = company.runtime.tasks().list(company.id()).await?;
    let card = rows
        .iter()
        .find(|t| t.id == task_id)
        .cloned()
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("task {task_id}")))?;

    // Lineage is a pure board read — no journal needed.
    let parent = card
        .parent_task_id
        .as_ref()
        .and_then(|pid| rows.iter().find(|t| &t.id == pid))
        .map(LineageRef::from);
    let mut children: Vec<&TaskRecord> = rows
        .iter()
        .filter(|t| t.parent_task_id.as_deref() == Some(task_id.as_str()))
        .collect();
    children.sort_by_key(|t| t.updated_at_millis);
    let children = children.into_iter().map(LineageRef::from).collect();

    let (timeline, open_window_at) = task_timeline(&company, &task_id).await?;

    // The live wait (issue #305). Only meaningful while a run window is open —
    // a parked approval on a finished task belongs to whatever runs next, not to
    // this one — and only for approvals parked at or after the window opened, so
    // a pre-existing park is not re-attributed to this dispatch. Earliest wins:
    // waiting started when the first of them parked.
    let waiting_since = open_window_at.and_then(|opened_at| {
        company
            .runtime
            .pending_approvals()
            .into_iter()
            .map(|a| a.at_millis)
            .filter(|at| *at >= opened_at)
            .min()
    });

    // An indexed store read, not another journal pass (issue #242).
    let runs = runs_for_task(&company, &task_id).await?;

    Ok(Json(TaskDetail {
        task: card.into(),
        timeline,
        lineage: Lineage { parent, children },
        runs,
        waiting_since,
    }))
}

/// How many journal events one `read_from` page pulls.
///
/// The scan is bounded per page rather than per request: a task's events can sit
/// anywhere in a company's history, so the whole log must still be *traversed* —
/// but it is never all *resident* at once.
const TIMELINE_PAGE: usize = 512;

/// Folds the company journal down to one task's timeline.
///
/// Oldest-first, paged. `window` opens on this task's dispatch anchor and closes
/// on its completion anchor; untagged-but-windowed events (approvals) are only
/// admitted while it is open, so a resolution belonging to a different task's
/// run never leaks in.
///
/// **Why the scan does not stop at the first completion anchor.** A card can be
/// re-dispatched — moved back to `in_progress` after review — which opens a
/// second dispatch → completion cycle later in the same log. Stopping at the
/// first `DeskTaskCompleted` would silently truncate every run after the first,
/// which is worse than the cost it saves. Bounding the page size gives the
/// memory win without that correctness loss; a stored per-task dispatch offset
/// is the durable fix for the traversal cost and is left to the epic.
///
/// Returns the timeline alongside the instant the *still-open* window opened,
/// or `None` when the task is not mid-run — the caller needs that anchor to
/// scope the live wait (issue #305).
async fn task_timeline(
    company: &ScopedCompany,
    task_id: &str,
) -> Result<(Vec<TimelineEntry>, Option<u64>), ApiError> {
    use crate::ports::types::EventSeq;

    // One snapshot for the whole fold, not a lookup per event: the fold is a
    // pure function over it, and the journal lock is never held while paging.
    let park_instants = company.runtime.approval_park_instants();

    let mut timeline = Vec::new();
    let mut window_opened_at: Option<u64> = None;
    let mut next_seq = 0u64;
    loop {
        let page = company
            .runtime
            .events()
            .read_from(company.id(), EventSeq::new(next_seq), TIMELINE_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        // Advance past the last event read. `read_from` is inclusive of `seq`,
        // so without the `+ 1` the final event of each page would be re-read
        // forever.
        next_seq = page
            .last()
            .map(|ev| ev.seq.value() + 1)
            .unwrap_or(next_seq + 1);
        let exhausted = page.len() < TIMELINE_PAGE;
        fold_page(
            &page,
            task_id,
            &park_instants,
            &mut window_opened_at,
            &mut timeline,
        );
        if exhausted {
            break;
        }
    }
    Ok((timeline, window_opened_at))
}

/// Folds one page of journal events onto `timeline`, carrying the window state
/// across pages.
///
/// `window_opened_at` is both the window flag and its anchor: `Some(at)` while
/// a dispatch is open, `None` once it closes. `park_instants` is the journal
/// snapshot the approval arm joins against to recover waiting time (#305);
/// keeping it a parameter leaves this a pure function of its inputs.
fn fold_page(
    page: &[crate::ports::types::StoredEvent],
    task_id: &str,
    park_instants: &std::collections::HashMap<crate::ports::types::ApprovalId, u64>,
    window_opened_at: &mut Option<u64>,
    timeline: &mut Vec<TimelineEntry>,
) {
    use crate::ports::types::ActorKind;

    for ev in page {
        let entry = match &ev.event {
            CompanyEvent::TaskDispatched { task_id: id, .. } if id == task_id => {
                *window_opened_at = Some(ev.at_millis);
                Some(("dispatched", "Dispatched".to_string(), None, None))
            }
            CompanyEvent::AgentReply {
                agent_id,
                text,
                task_id: Some(id),
                ..
            } if id == task_id => Some((
                "reply",
                format!("Reply from {agent_id}"),
                Some(text.clone()),
                None,
            )),
            CompanyEvent::McpCallFailed {
                server,
                tool,
                message,
                task_id: Some(id),
                ..
            } if id == task_id => Some((
                "tool_failed",
                format!("{server} · {tool} failed"),
                Some(message.clone()),
                None,
            )),
            CompanyEvent::DeskTaskCompleted {
                task_id: id,
                desk,
                output,
                column,
            } if id == task_id => {
                *window_opened_at = None;
                Some((
                    "completed",
                    format!("Finished on {desk} → {column}"),
                    Some(output.clone()),
                    None,
                ))
            }
            // Window-correlated, not id-correlated — see `task_detail`'s docs.
            // The operator's identity is deliberately dropped: it can carry a
            // user id, matching the SSE projection's deny-by-default stance.
            // Only `by.kind` is read, which names a category and never a person.
            CompanyEvent::ApprovalResolved {
                approval_id,
                verdict,
                by,
            } if window_opened_at.is_some() => {
                // The approval id joins the resolution back to the journal's
                // park instant. Clamping to the window's opening keeps a wait
                // that began before this task was dispatched from charging its
                // pre-dispatch portion to this run.
                let waited = park_instants.get(approval_id).map(|parked_at| {
                    let from =
                        window_opened_at.map_or(*parked_at, |opened| (*parked_at).max(opened));
                    ev.at_millis.saturating_sub(from)
                });
                // A system actor here is the TTL sweep, not a person: expiry
                // resolves to a default-deny, and saying "Approval denied" for
                // it would read as though somebody looked at it and said no.
                let label = if by.kind == ActorKind::System {
                    "Approval expired (auto-denied)".to_string()
                } else {
                    format!(
                        "Approval {}",
                        crate::brain::medulla::effects::verdict_word(*verdict)
                    )
                };
                Some(("approval", label, None, waited))
            }
            _ => None,
        };
        if let Some((kind, label, detail, waited_millis)) = entry {
            timeline.push(TimelineEntry {
                seq: ev.seq.value(),
                at_millis: ev.at_millis,
                kind: kind.to_string(),
                label,
                detail,
                waited_millis,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Steer (issue #111): pause / cancel / redirect an in-flight run from chat.
//
// Both routes sit under the same operator-authenticated `ScopedCompany` guard as
// every other task write. There is NO agent tool for steering anywhere — the
// mechanism is reachable only from this operator control plane, so it is
// structurally non-agent-injectable.
// ---------------------------------------------------------------------------

/// One in-flight, steerable run as the operator strip renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InflightCard {
    /// The board task id, or `null` for a delegation (no card).
    task_id: Option<String>,
    /// The steer key the `POST …/tasks/{key}/steer` route addresses.
    key: String,
    /// `"task"` or `"delegation"`.
    kind: String,
    title: String,
    agent_id: String,
    started_at: u64,
    /// The last requested action word (`pause`/`cancel`/`redirect`), or `null`.
    pending_action: Option<String>,
}

impl From<InflightEntry> for InflightCard {
    fn from(e: InflightEntry) -> Self {
        Self {
            task_id: e.task_id,
            key: e.key,
            kind: e.kind.as_str().to_string(),
            title: e.title,
            agent_id: e.agent_id,
            started_at: e.started_at_millis,
            pending_action: e.pending_action,
        }
    }
}

/// The steer request body (`{action, instruction?, confirm?}`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SteerBody {
    /// `"pause"` | `"cancel"` | `"redirect"`.
    action: String,
    /// Required (non-empty) for `redirect`; ignored otherwise.
    #[serde(default)]
    instruction: Option<String>,
    /// Must be `true` for `cancel` (a guardrail against an accidental cancel).
    #[serde(default)]
    confirm: Option<bool>,
}

/// `GET …/tasks/inflight` — the company's live, steerable runs, oldest first so
/// the strip order is stable.
async fn list_inflight(company: ScopedCompany) -> Result<Json<Vec<InflightCard>>, ApiError> {
    let mut rows: Vec<InflightCard> = company
        .runtime
        .steer()
        .list(company.id())
        .into_iter()
        .map(InflightCard::from)
        .collect();
    rows.sort_by_key(|r| r.started_at);
    Ok(Json(rows))
}

/// `POST …/tasks/{task_id}/steer` — apply an operator steer to an in-flight run.
///
/// Validation (all `400`): unknown action; `cancel` without `confirm: true`;
/// `redirect` without a non-empty `instruction`. An unknown key is `404`; a card
/// that exists but is not in flight is `409`. On accept the run's control is set,
/// a best-effort [`CompanyEvent::TaskSteered`] is journaled, and the route
/// returns `202 Accepted` (the disposition lands on the card asynchronously).
async fn steer_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
    Json(body): Json<SteerBody>,
) -> Result<StatusCode, ApiError> {
    let bad = |msg: &str| ApiError(OpenCompanyError::InvalidRequest(msg.to_string()));

    let action = match body.action.as_str() {
        "pause" => SteerAction::Pause,
        "cancel" => {
            if body.confirm != Some(true) {
                return Err(bad("cancel requires confirm: true"));
            }
            SteerAction::Cancel
        }
        "redirect" => {
            let instruction = body
                .instruction
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("redirect requires a non-empty instruction"))?;
            SteerAction::Redirect {
                instruction: cap_redirect(instruction),
            }
        }
        other => return Err(bad(&format!("unknown steer action '{other}'"))),
    };

    // Capture the audit fields before the action is moved into the registry.
    let action_word = action.as_str().to_string();
    let instruction_for_event = match &action {
        SteerAction::Redirect { instruction } => Some(instruction.clone()),
        _ => None,
    };

    match company
        .runtime
        .steer()
        .steer(company.id(), &task_id, action)
    {
        Ok(()) => {
            // Best-effort audit: a journal failure never fails the accepted steer.
            let _ = company
                .runtime
                .events()
                .append(
                    company.id(),
                    CompanyEvent::TaskSteered {
                        task_id: task_id.clone(),
                        action: action_word,
                        instruction: instruction_for_event,
                        by: None,
                    },
                )
                .await;
            Ok(StatusCode::ACCEPTED)
        }
        // pause / redirect on a delegation (cancel-only in v1).
        Err(SteerError::Unsupported) => Err(bad("this run only supports cancel")),
        // No such run in flight: distinguish an idle card (409) from an unknown
        // key (404) by consulting the board.
        Err(SteerError::NotInFlight) => {
            let exists = company
                .runtime
                .tasks()
                .list(company.id())
                .await?
                .into_iter()
                .any(|t| t.id == task_id);
            if exists {
                Err(ApiError(OpenCompanyError::Conflict(format!(
                    "task {task_id} is not in flight"
                ))))
            } else {
                Err(ApiError(OpenCompanyError::NotFound(format!(
                    "task {task_id}"
                ))))
            }
        }
    }
}
