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
//! lineage, the task's own approvals (issue #333), and — since issue #335 — the
//! card's discussion thread into one response so the console makes a single call. See
//! [`task_detail`] for the assembly and its scrub discipline, and
//! [`post_discussion`] for the thread's one write.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::steer::{InflightEntry, MAX_REDIRECT_CHARS, SteerAction, SteerError};
use crate::company::{WorkflowGraphSpec, create_company_workflow, raw_workflow_from_spec};
use crate::error::OpenCompanyError;
use crate::ports::tasks::{
    COLUMN_DONE, COLUMN_TODO, TaskDeliverable, TaskOutput, TaskOutputAction, TaskOutputSource,
    TaskOutputWorkflow, TaskRecord, TaskWorkflowProposal, cap_discussion, is_board_column,
};
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
        // Issue #580: apply or reject the workflow the builder pass proposed for a
        // `workflow`-deliverable card sitting In Review. Apply is the ONE place a
        // proposal becomes a real workflow (through `create_company_workflow`);
        // reject clears it and returns the card to To-do. Both under the same
        // `ScopedCompany` guard as every other task write.
        .merge(scoped(
            "/tasks/{task_id}/workflow-proposal/apply",
            post(apply_workflow_proposal),
        ))
        .merge(scoped(
            "/tasks/{task_id}/workflow-proposal/reject",
            post(reject_workflow_proposal),
        ))
        .merge(scoped("/tasks/{task_id}/discussion", post(post_discussion)))
        // Issue #358. `DELETE` on one message, under the same `ScopedCompany`
        // guard as every other task write — see `redact_discussion` for why
        // that is the right authority rather than an admin-only one.
        .merge(scoped(
            "/tasks/{task_id}/discussion/{seq}",
            delete(redact_discussion),
        ))
}

/// A task card as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCard {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
    /// Which of the board's three phases this card is in — `pending`,
    /// `working` or `done` (issue #1512).
    ///
    /// The name is unchanged and the meaning is narrower: it used to carry the
    /// stored stage, so a client saw six words here and had to know which of
    /// four meant "started". It is the board's column, and the board now has
    /// three of them.
    pub(crate) column: String,
    /// Which kind of working, when the card is working: `planning`,
    /// `in_progress`, `paused` or `in_review` (issue #1512).
    ///
    /// Omitted for a pending or done card, because there is only one way to be
    /// either. This is what the console reads for the affordances that are
    /// genuinely stage-specific — Resume on a paused card, the review link on
    /// one waiting for a verdict — which used to be read off `column` and
    /// therefore forced `column` to stay six-valued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<String>,
    pub(crate) priority: String,
    pub(crate) assignee: String,
    pub(crate) updated_at: u64,
    /// Lifetime task cost, including descendants. Omitted for a true zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost: Option<CostDisplay>,
    /// The card this one was spawned from (#185). Omitted on a lineage root so
    /// the board's existing wire shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_task_id: Option<String>,
    /// The chat thread this card was opened from (issue #246).
    ///
    /// `TaskRecord::origin_chat_id` has existed since #151 (it is what lets a
    /// completed run answer in the conversation that asked), and the tool-spawn
    /// path stamps it — but it was never *readable*: no DTO projected it, so
    /// task detail could not show where a card came from. Omitted when absent,
    /// which is every card the board created before this, so the existing wire
    /// shape is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) origin_chat_id: Option<String>,
    /// What the card's latest successful attempt produced (issue #339) — the
    /// link that turns a finished card into something the operator can open.
    ///
    /// Rides the **board** read, not just task detail, because the link is
    /// rendered on the card itself: a board that had to open every card to find
    /// out what it produced would be N reads per poll. It is bounded — one
    /// entry per published file, one per workflow — and omitted entirely for a
    /// card that has never succeeded, so the existing wire shape is unchanged
    /// for every card that carried no output before this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<crate::ports::tasks::TaskOutput>,
    /// The brief the last planning pass wrote (issue #337).
    ///
    /// Projected verbatim rather than reshaped: the host already decided every
    /// prerequisite verdict, and a second transcription here is how the badge
    /// the console renders drifts from the verdict the dispatch gate used.
    /// Omitted for a card nobody has planned, which is every card until it is
    /// dragged into Planning — so the board's existing wire shape is unchanged.
    ///
    /// Nothing secret can ride here: the pass never puts a credential *value*
    /// in front of the model, and a prerequisite carries only a name and a
    /// verdict. See `docs/spec/runtime/planning.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<crate::ports::tasks::TaskPlan>,
    /// Whether the card produces a one-off result or a reusable workflow
    /// (issue #580). Omitted when `once` — the console's default — so every card
    /// the board rendered before #580 keeps its exact wire shape; a `workflow`
    /// card sends `"workflow"` so the composer toggle and the review panel know
    /// to render.
    #[serde(skip_serializing_if = "TaskDeliverable::is_once")]
    pub(crate) deliverable: TaskDeliverable,
    /// The workflow the builder pass proposed, awaiting approval (issue #580).
    /// Present only while a `workflow` card sits In Review with a built proposal;
    /// the review panel reads its `summary` and `ops` graph. Omitted otherwise,
    /// so the existing wire shape is unchanged for every card without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workflow_proposal: Option<TaskWorkflowProposal>,
    /// The workflow run whose agent node opened this card (issue #661 / M5).
    ///
    /// Projected because a card with no parent and no origin chat is otherwise
    /// unexplained on the board: an operator finding a card they did not open, and
    /// that no conversation asked for, has nothing to look at. With this the console
    /// can link straight to the run in the workflow history panel.
    ///
    /// Omitted when absent — which is every card not opened by a run, i.e. every
    /// card that existed before this — so the board's existing wire shape is
    /// unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) origin_run_id: Option<String>,
    /// The workflow graph that run is of (issue #661 / M5).
    ///
    /// Carried beside the run id rather than resolved from it, for the reason
    /// [`TaskRecord::origin_workflow_id`](crate::ports::TaskRecord::origin_workflow_id)
    /// gives: the journal is trimmable and the board is not, so a card must be able
    /// to name its workflow after its run's rows are gone. Omitted when absent, in
    /// lockstep with `originRunId`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) origin_workflow_id: Option<String>,
    /// Why a failed or cancelled run returned this card to `todo` (issue
    /// #1865) — the chip that tells a bounced card apart from a fresh one
    /// without opening it. Omitted for every card that has never bounced,
    /// which is every card the board rendered before this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bounced: Option<String>,
}

impl From<TaskRecord> for TaskCard {
    fn from(t: TaskRecord) -> Self {
        Self {
            id: t.id,
            title: t.title,
            note: t.note,
            column: crate::ledger::board::phase_of(&t.column).to_string(),
            stage: stage_of(&t.column),
            priority: t.priority,
            assignee: t.assignee,
            updated_at: t.updated_at_millis,
            cost: None,
            parent_task_id: t.parent_task_id,
            origin_chat_id: t.origin_chat_id,
            output: t.output,
            plan: t.plan,
            deliverable: t.deliverable,
            workflow_proposal: t.workflow_proposal,
            origin_run_id: t.origin_run_id,
            origin_workflow_id: t.origin_workflow_id,
            bounced: t.bounced,
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
    /// Whether this card produces a one-off result or a reusable workflow
    /// (issue #580) — the operator's explicit choice (D2a). Absent means `once`,
    /// the historical behaviour, so no existing caller changes.
    #[serde(default)]
    deliverable: Option<TaskDeliverable>,
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
    /// Switches the card between a one-off result and a reusable workflow
    /// (issue #580). Omitting it leaves the choice untouched — so an operator can
    /// flip a To-do card to `workflow` before dragging it into In Progress, where
    /// the builder pass fires. The same partial-patch contract every field here
    /// follows.
    #[serde(default)]
    deliverable: Option<TaskDeliverable>,
}

/// The sub-resource path (`task_id`); the scope `id` is consumed by the extractor.
#[derive(Debug, Deserialize)]
struct TaskPath {
    task_id: String,
}

/// The sub-resource path for one discussion message (issue #358): the card and
/// the journal sequence the post was written at.
#[derive(Debug, Deserialize)]
struct DiscussionPath {
    task_id: String,
    seq: u64,
}

/// `GET …/tasks` — the whole board, newest-updated first. The console reads
/// this to render the Kanban columns and each card's detail (note, assignee).
async fn list_tasks(company: ScopedCompany) -> Result<Json<Vec<TaskCard>>, ApiError> {
    let mut rows = company.runtime.tasks().list(company.id()).await?;
    let costs = costs_for_board(&company, &rows).await?;
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at_millis));
    Ok(Json(
        rows.into_iter()
            .map(|row| {
                let cost = costs
                    .totals
                    .get(&row.id)
                    .and_then(|total| CostDisplay::new(total.total_usd, company.may_read_contents));
                let mut card = TaskCard::from(row);
                card.cost = cost;
                card
            })
            .collect(),
    ))
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

/// Resolves a written `column` to the stage that is actually stored, rejecting
/// a word the board does not know (issue #205, issue #1512).
///
/// `column` is a free string on the wire, and nothing checked it: a typo'd
/// `"in-progress"` was persisted verbatim, so the card disappeared from every
/// rendered column *and* — since only the exact literal `in_progress`
/// edge-fires a dispatch — silently never ran. Refusing at the write boundary
/// is the cheap place to keep the board's vocabulary the only vocabulary.
///
/// # Two words in, one word stored
///
/// Since #1512 the board *reads* as three phases and *stores* six stages, so a
/// drop sends `working` where it used to send `in_progress`. Both are accepted
/// and they are not equivalent:
///
/// * a **phase** resolves to that phase's [`entry_stage`](crate::ledger::board::BoardPhase::entry_stage)
///   — `working` becomes `in_progress`, which dispatches. This is what the
///   console, the tools and any ordinary client send.
/// * a **stage** is stored verbatim. Nothing in the product needs this any
///   more, but the runtime's own paths and every stored card speak it, and a
///   boundary that refused `in_review` would refuse to describe a state the
///   board can be in.
///
/// The error names the phases only. A caller who guessed wrong is a caller who
/// should be sending one of three words, and listing the six would teach them
/// the vocabulary this issue exists to stop teaching.
fn resolve_column(column: &str) -> Result<String, ApiError> {
    if let Some(stage) = crate::ledger::board::entry_stage(column) {
        return Ok(stage.to_string());
    }
    if is_board_column(column) {
        return Ok(column.to_string());
    }
    Err(ApiError(OpenCompanyError::InvalidRequest(format!(
        "\"{column}\" is not a board column — use one of: {}",
        crate::ledger::board::phase_ids().join(", ")
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
    let column = match body.column {
        Some(column) => resolve_column(&column)?,
        None => COLUMN_TODO.to_string(),
    };
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
        // Nothing has run yet, so there is no deliverable to point at
        // (issue #339). The first successful settle stamps it.
        output: None,
        // Issue #337: a plan is something the host *produces*, never something
        // intake accepts. Nothing on the create body can set it, so a client
        // cannot post a card that already claims to be planned — and cannot
        // forge the prerequisite verdicts that decide whether it dispatches.
        plan: None,
        // Issue #580: the operator's explicit once-vs-workflow choice (D2a),
        // defaulting to the historical one-off. A `workflow` card created here
        // lands in To-do like any other; the builder pass fires only when it is
        // dragged into In Progress. There is no proposal yet — the builder mints
        // one.
        planning_attempts: Vec::new(),
        deliverable: body.deliverable.unwrap_or_default(),
        workflow_proposal: None,
        origin_run_id: None,
        origin_workflow_id: None,
        bounced: None,
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
        record.column = resolve_column(&column)?;
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
    // Issue #580: flip the once-vs-workflow choice. Applied before the upsert, so
    // a patch that both sets `deliverable: "workflow"` and drags the card into
    // In Progress dispatches through the builder pass rather than an ordinary
    // dispatch — the edge in `upsert_task` reads the record this write persists.
    if let Some(deliverable) = body.deliverable {
        record.deliverable = deliverable;
    }
    record.updated_at_millis = now_millis();
    // `upsert_task` can persist a record that differs from `record`: leaving
    // `todo` for any other column clears a stale `bounced` chip (issue #1865),
    // and that clear happens on the clone it writes, not on this local
    // `record`. Serializing its return value rather than `record` itself is
    // what keeps this response in sync with the row that was actually stored
    // (Codex review, PR #1883) — a client such as `TaskEditDialog` reconciles
    // its board state straight from this body.
    let stored = company.runtime.upsert_task(&record).await?;
    Ok(Json(stored.into()))
}

/// `DELETE …/tasks/{task_id}` — remove a card from the board.
///
/// # An in-flight card is refused rather than deleted (issue #984)
///
/// A running turn holds its card in memory and writes it back when it settles
/// (`tasks.upsert` on the harness settle path). Deleting the row underneath it
/// therefore does not remove the card: the settle re-creates it, in
/// `in_review`/`done`, *after* every console surface has already dropped the
/// chip naming it — a card nothing can reach, which is precisely the
/// "board fills with conversation" failure this is meant to fix.
///
/// So an in-flight card is a `409` with the steer route named, not a delete.
///
/// **Refusing rather than cancel-then-delete is the deliberate choice**, and not
/// merely the smaller one. A cancel is cooperative: it sets the run's
/// [`SteerControl`](crate::company::steer::SteerControl) and the turn stops at
/// its *next iteration boundary*, which may be after the settle write has
/// already gone out. Cancel-then-delete would therefore reintroduce the same
/// race it was meant to close, only less often — the worst kind of fix, because
/// it would pass a test and fail in production. Refusing is the state the
/// operator can act on: cancel, watch it stop, then delete.
async fn delete_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<StatusCode, ApiError> {
    // Serialized with the other board writes so a delete cannot land between a
    // concurrent re-parent's existence check and its write, which would leave
    // the dangling edge `validate_parent` exists to prevent.
    let _serialized = company.runtime.task_writes.lock().await;

    // Checked under the write lock, so a run that registers after this point
    // cannot slip between the check and the delete.
    if company
        .runtime
        .steer()
        .list(company.id())
        .iter()
        .any(|run| run.task_id.as_deref() == Some(task_id.as_str()))
    {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "task {task_id} is running — cancel it first (POST …/tasks/{task_id}/steer \
             with `action: \"cancel\", confirm: true`), then delete it. Deleting it now \
             would not remove it: the turn writes the card back when it settles."
        ))));
    }

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
// The plan → workflow bridge: apply / reject a proposal (issue #580)
// ---------------------------------------------------------------------------

/// `POST …/tasks/{task_id}/workflow-proposal/apply` — approve and create the
/// workflow a builder pass proposed (issue #580).
///
/// This is the **one** path a proposal becomes a real workflow. It rebuilds the
/// [`RawWorkflow`](crate::company::RawWorkflow) from the **stored** `ops` — the
/// host is the authority, the browser's copy is never trusted — and runs it
/// through [`create_company_workflow`], which takes the company write lock,
/// re-validates shape + roster + destinations + id/name uniqueness, and (issue
/// #276) lands any schedule-carrying graph switched off until a person arms it.
///
/// "The same validation an editor save runs" is the whole contract here, and
/// until issue #1191 it was not true: the channel-destination rule lived on the
/// two write routes rather than in the shared core, so this path — the ONE path
/// where the operator did not author the graph, the path #836 exists because of
/// — was the path with no check. A proposal naming a channel nobody wired was
/// persisted and the card marked Done.
///
/// On success the card is stamped with a [`TaskOutput`] linking the created
/// workflow to the build attempt (issue #339) and moved to **Done**, and the
/// proposal is cleared. If the create is refused — the roster drifted since the
/// proposal was generated, a name has since been taken — the reason is appended
/// to the card's note and the card **stays In Review** with its proposal intact,
/// and the refusal is returned to the caller.
async fn apply_workflow_proposal(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<Json<TaskCard>, ApiError> {
    let _serialized = company.runtime.task_writes.lock().await;
    let mut record = company
        .runtime
        .tasks()
        .list(company.id())
        .await?
        .into_iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("task {task_id}")))?;

    let proposal = record.workflow_proposal.clone().ok_or_else(|| {
        OpenCompanyError::InvalidRequest("this card has no workflow proposal to apply".to_string())
    })?;

    // Host authority: rebuild and re-validate a graph from the STORED ops rather
    // than trusting any client-supplied body.
    let spec: WorkflowGraphSpec = serde_json::from_value(proposal.ops.clone()).map_err(|err| {
        OpenCompanyError::InvalidRequest(format!(
            "the stored workflow proposal could not be read as a graph: {err}"
        ))
    })?;
    let mut draft = raw_workflow_from_spec(&spec)?;

    // Issue #1862 prerequisite: a proposal that names no owning desk defaults
    // to the proposing card's assignee's desk — the same "somebody is
    // responsible for this" fallback the sender-resolution chain leans on
    // when a run has no triggering agent. Best-effort: an assignee with no
    // desk (or a company record that fails to load) leaves `owner_desk`
    // `None` rather than blocking the apply — the same permissive stance
    // `resolve_assignee` above takes toward an unloadable record.
    if draft
        .owner_desk
        .as_deref()
        .is_none_or(|desk| desk.trim().is_empty())
        && let Ok(Some(company_record)) = company.runtime.store().load(company.id()).await
    {
        // Issue #1882 review: a card assigned directly to a desk stores the
        // canonical desk id as its `assignee` (`runtime::assignee`,
        // `AssigneeResolution::canonical` — including an `EmptyDesk` with no
        // roster member yet), not a teammate id. `desk_of_member` searches
        // desk MEMBERSHIP, so a desk-id assignee — nobody's member — resolved
        // to nothing even though the card already names its owning desk.
        // Resolve as a desk first; only fall back to the teammate's desk when
        // the assignee is not itself a desk.
        //
        // Issue #1882 review: the teammate fallback uses `sole_desk_of_member`,
        // not `desk_of_member` — a teammate who sits on two or more desks gives
        // no basis for picking either one, and `desk_of_member` would silently
        // persist whichever desk happens to be declared first in the manifest.
        draft.owner_desk = company_record
            .resolve_desk_id(&record.assignee)
            .or_else(|| {
                crate::runtime::delegation_tools::sole_desk_of_member(
                    &company_record,
                    &record.assignee,
                )
            });
    }

    // Issue #1191: the deliverable channel set, read off the SAME runtime the
    // console's destination picker is served from. Apply is a save, and it used
    // to be the one save that skipped the channel rule — so a proposal naming a
    // channel nobody wired was persisted, the card was marked Done, and the
    // resulting workflow could not be saved again from the editor without first
    // fixing a destination the operator never chose.
    let file = match create_company_workflow(
        company.id(),
        company.runtime.source_dir(),
        company.runtime.store(),
        Some(company.runtime.events()),
        draft,
        Some(&company.runtime.deliverable_channel_ids()),
        // Issue #1843: the operator applying the proposal, when there is
        // one — same attribution rule as the direct REST create path.
        company.actor.clone(),
    )
    .await
    {
        Ok(file) => file,
        Err(err) => {
            // Roster drift, or a name taken since the proposal was generated.
            // Keep the card In Review with its proposal, name the reason on the
            // note, and surface the error — best-effort, so a note write that also
            // fails cannot mask the real create failure.
            let reason = format!(
                "could not create the proposed workflow, so it is still waiting for review: {err}"
            );
            record.note = Some(crate::runtime::advance::append_result(
                record.note.as_deref(),
                crate::runtime::advance::SYSTEM_ATTRIBUTION,
                &reason,
            ));
            record.updated_at_millis = now_millis();
            let _ = company.runtime.tasks().upsert(company.id(), &record).await;
            return Err(ApiError(err));
        }
    };

    // Link the created workflow to the build attempt (issue #339) and finish the
    // card. #276 already left a scheduled graph disarmed inside the create;
    // nothing here arms it. The attempt ordinal is a nicety — a failed run read
    // costs the label, never the link.
    let attempt = company
        .runtime
        .runs()
        .get_run(company.id(), &proposal.run_id)
        .await
        .ok()
        .flatten()
        .map(|run| run.attempt);
    record.output = Some(TaskOutput {
        source: TaskOutputSource::Run {
            run_id: proposal.run_id.clone(),
            attempt,
        },
        at_millis: now_millis(),
        artifacts: Vec::new(),
        workflows: vec![TaskOutputWorkflow {
            workflow_id: file.id.clone(),
            run_id: None,
            action: TaskOutputAction::Created,
        }],
    });
    record.workflow_proposal = None;
    let note = format!("approved — created the `{}` workflow", file.name);
    record.note = Some(crate::runtime::advance::append_result(
        record.note.as_deref(),
        crate::runtime::advance::SYSTEM_ATTRIBUTION,
        &note,
    ));
    record.column = COLUMN_DONE.to_string();
    record.updated_at_millis = now_millis();
    company
        .runtime
        .tasks()
        .upsert(company.id(), &record)
        .await?;
    Ok(Json(record.into()))
}

/// `POST …/tasks/{task_id}/workflow-proposal/reject` — discard the proposed
/// workflow and return the card to To-do (issue #580, decision D2c).
///
/// The card keeps its `workflow` deliverable, so dragging it back into In
/// Progress runs the builder pass again; an operator who wanted a one-off instead
/// flips `deliverable` with a patch. Nothing about the company's workflow list
/// changes — the proposal never was a workflow.
async fn reject_workflow_proposal(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<Json<TaskCard>, ApiError> {
    let _serialized = company.runtime.task_writes.lock().await;
    let mut record = company
        .runtime
        .tasks()
        .list(company.id())
        .await?
        .into_iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("task {task_id}")))?;
    if record.workflow_proposal.is_none() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this card has no workflow proposal to reject".to_string(),
        )));
    }
    record.workflow_proposal = None;
    record.note = Some(crate::runtime::advance::append_result(
        record.note.as_deref(),
        crate::runtime::advance::SYSTEM_ATTRIBUTION,
        "the proposed workflow was rejected — the card is back in Pending",
    ));
    record.column = COLUMN_TODO.to_string();
    record.updated_at_millis = now_millis();
    company
        .runtime
        .tasks()
        .upsert(company.id(), &record)
        .await?;
    Ok(Json(record.into()))
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
pub(crate) struct TimelineEntry {
    /// The journal sequence this entry came from — the console's stable key,
    /// and what makes the timeline strictly ordered.
    pub(crate) seq: u64,
    /// Epoch-millis the event was journaled.
    pub(crate) at_millis: u64,
    /// A stable wire word for what happened: `dispatched`, `reply`,
    /// `tool_failed`, `approval`, or `completed`.
    pub(crate) kind: String,
    /// A short human label.
    pub(crate) label: String,
    /// Optional scrubbed detail (see the type docs for what may appear here).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    /// Stable key for a cost row that did not originate in the journal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost_key: Option<String>,
    /// Source-currency USD for this line, or an explicit hidden state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost: Option<CostDisplay>,
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
    pub(crate) waited_millis: Option<u64>,
}

/// One approval that belongs to this task (issue #333).
///
/// Distinct from the `approval` [`TimelineEntry`], which can only ever describe
/// a *resolution* — an approval still parked has no `ApprovalResolved` event,
/// so it cannot reach the timeline at all, and that is exactly the one the card
/// needs to report: the sign-off a task is stalled on right now.
///
/// **Deliberately three fields (issue #468).** This used to back an Approvals
/// tab on the task card that listed every sign-off with its kind and its
/// park→resolve span. That tab is gone: approvals are decided in one place, and
/// a second half-surface beside it was a maintenance cost with no payoff. What
/// replaced it is a single "waiting on approval" line linking to the Approvals
/// page, so this projection now carries what that line needs and stops there —
/// whether anything is pending, and since when. `id` stays because it is the
/// discriminator that makes "which approvals belong to which card" testable,
/// which is #333's actual behaviour and outlives the tab.
///
/// Dropped with the tab: `kind`, `resolvedAtMillis`, `waitedMillis`. The
/// park→resolve arithmetic is unchanged and still observable on the `approval`
/// [`TimelineEntry`], which carries its own `waitedMillis`.
///
/// Still carries no payload, and the reason has changed. It used to be that a
/// task read was the wrong place to widen exposure. That argument does not hold
/// — `GET …/approvals` already returns the payload to any company member — so
/// the honest reason is simply that this line does not need one. Whether
/// membership is the right boundary for that sibling route is asked separately
/// in issue #618 and is not decided here.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskApproval {
    /// The approval's id — the same one the Approvals page resolves against.
    pub(crate) id: String,
    /// Epoch-millis the effect parked. The card measures "waiting for N" from
    /// here against its own clock, which is why a pending row needs no
    /// server-side span.
    pub(crate) at_millis: u64,
    /// `pending`, `approved`, `denied`, or `expired`.
    pub(crate) status: String,
}

/// One irreversible effect this task already executed (issue #351).
///
/// Drawn from the runtime journal's executed record — the same append-only set
/// that makes an effect at-most-once — and **not** from the timeline. A
/// timeline row says what an agent reported; this says what the runtime
/// committed to run, and only the two together make a retry warning
/// trustworthy.
///
/// "Committed", not "completed": the journal writes the record before the effect
/// is performed, which is what makes it at-most-once, and the runtime never
/// re-attempts it afterwards. An entry is therefore something the operator must
/// assume happened, not something proven to have finished — the console's
/// wording is qualified to match.
///
/// `kind` is the dotted effect kind, the same vocabulary the Approvals page
/// already receives and maps to plain language client-side (`effectAction` in
/// `frontend/src/lib/language.ts`). Sending the key and rendering the sentence
/// keeps operator-facing wording in the one layer the glossary rule puts it in,
/// rather than growing a second copy on the host.
///
/// Deliberately no payload. The journal does not retain one for an executed
/// effect, so a recipient or a message body cannot reach this response even by
/// accident — the same scrub discipline [`TimelineEntry`] documents.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IrreversibleEffect {
    /// The dotted effect kind, e.g. `payment.send`.
    kind: String,
    /// Epoch-millis the effect was committed.
    at_millis: u64,
    /// The USD amount involved, if any — what makes "sent a payment" into
    /// "sent a payment of $2,400".
    ///
    /// **Role-restricted (issue #705).** This is money, and it is the same
    /// class of field [`ApprovalSummary::amount_usd`](crate::runtime::types::ApprovalSummary)
    /// is restricted to admins by issue #618. It is redacted through the *same*
    /// predicate, at the edge, by
    /// [`effects_for_principal`](crate::server::approval_visibility::effects_for_principal)
    /// — never here, because the projection does not know who is asking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) amount_usd: Option<f64>,
    /// Whether an amount was withheld from this reader.
    ///
    /// The same reasoning as
    /// [`ApprovalSummary::contents_hidden`](crate::runtime::types::ApprovalSummary):
    /// `amount_usd: None` already means "this effect involved no money", so
    /// blanking alone would make a withheld payment and a free tool call the
    /// same bytes on the wire. A console has to be able to say *hidden by your
    /// role* rather than render a payment that looks like it cost nothing.
    ///
    /// Skipped when `false`, so an admin's response — and every response
    /// produced before this field existed — serializes byte-identically.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) amount_hidden: bool,
}

impl From<crate::runtime::journal::ExecutedEffect> for IrreversibleEffect {
    fn from(e: crate::runtime::journal::ExecutedEffect) -> Self {
        Self {
            kind: e.kind,
            at_millis: e.at_millis,
            amount_usd: e.amount_usd,
            // Never redacted at construction — the journal does not know who is
            // asking. `effects_for_principal` is the only thing that sets this.
            amount_hidden: false,
        }
    }
}

/// A neighbouring card in the lineage, trimmed to what a link needs.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LineageRef {
    pub(crate) id: String,
    pub(crate) title: String,
    /// The card's phase, on the same terms as [`TaskCard::column`].
    pub(crate) column: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost: Option<CostDisplay>,
}

impl LineageRef {
    fn from_task(t: &TaskRecord, cost: Option<CostDisplay>) -> Self {
        Self {
            id: t.id.clone(),
            title: t.title.clone(),
            column: crate::ledger::board::phase_of(&t.column).to_string(),
            cost,
        }
    }
}

/// The stage word a card carries, for the cards where it says something.
///
/// `None` for pending and done: there is exactly one way to be either, so a
/// field naming which would be a field naming nothing — and an omitted field
/// is what tells a client "do not offer a stage-specific control here" without
/// it needing the phase table to work that out.
fn stage_of(stored: &str) -> Option<String> {
    crate::ledger::board::column(stored)
        .filter(|column| column.phase == crate::ledger::board::PHASE_WORKING)
        .map(|column| column.id.to_string())
}

/// A positive USD amount or an explicit role-redacted state. A true zero is
/// represented by omitting the whole object, never by rendering `$0.00`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CostDisplay {
    #[serde(skip_serializing_if = "Option::is_none")]
    amount_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
}

impl CostDisplay {
    pub(super) fn new(amount_usd: f64, may_read: bool) -> Option<Self> {
        (amount_usd > 0.0).then(|| Self {
            amount_usd: may_read.then_some(amount_usd),
            hidden: !may_read,
        })
    }
}

async fn costs_for_board(
    company: &ScopedCompany,
    tasks: &[TaskRecord],
) -> Result<super::task_cost::TaskCosts, ApiError> {
    use std::collections::{HashMap, HashSet};

    let runs = company
        .runtime
        .runs()
        .list_runs(company.id(), &crate::ports::runs::RunFilter::default())
        .await?;
    let active: HashSet<&str> = runs
        .iter()
        .filter(|run| !run.status.is_terminal())
        .map(|run| run.id.as_str())
        .collect();
    let mut live = HashMap::new();
    if !active.is_empty() {
        let since = now_millis().saturating_sub(crate::ports::usage::RETENTION_MILLIS);
        match company.runtime.usage().query(company.id(), since).await {
            Ok(samples) => {
                for sample in samples {
                    if let Some(run_id) = sample.run_id
                        && active.contains(run_id.as_str())
                    {
                        *live.entry(run_id).or_insert(0.0) += sample.cost_usd;
                    }
                }
            }
            Err(err) => tracing::warn!(
                company = %company.id(),
                error = %err,
                "[usage] live task cost reconciliation fell back to run snapshots"
            ),
        }
    }
    Ok(super::task_cost::reconcile(tasks, &runs, &live))
}

/// The parent/children view of a task.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Lineage {
    /// The card this one was spawned from, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<LineageRef>,
    /// Cards spawned from this one, oldest-updated first for a stable render.
    pub(crate) children: Vec<LineageRef>,
}

/// The worked/waiting split (issue #305), computed once by the host.
///
/// Both halves are derived from the same timeline the screen and the exported
/// record are handed, and they used to be derived *twice* — once in
/// `frontend/src/views/TaskDetailView.tsx`, once in the exporter — so the two
/// could disagree about how long a person was waited on with nothing failing.
/// The host does the arithmetic and both callers read the result, which is the
/// same reason [`assemble_detail`] is shared rather than re-read.
///
/// **Live runs are the one thing a snapshot cannot carry.** A dispatch window
/// that is still open, or an approval still parked, keeps growing after these
/// totals are taken. `worked_live` / `waiting_live` mark those, and
/// `as_of_millis` is the instant they were taken: a caller that wants a ticking
/// figure adds `now - as_of_millis` to the live half and does nothing else.
///
/// That extension is *exact*, not an approximation, and it is why the merge
/// does not have to be repeated client-side: every closed span ends in the past,
/// so past `as_of_millis` the only interval still growing is the open one, and
/// it grows second for second.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskDurations {
    /// Milliseconds this task was actively worked, as of `as_of_millis`.
    pub(crate) worked_millis: u64,
    /// A dispatch window is still open — extend `worked_millis` from `as_of_millis`.
    pub(crate) worked_live: bool,
    /// Milliseconds the company spent waiting on a person, interval-merged.
    pub(crate) waiting_millis: u64,
    /// An approval is still parked — extend `waiting_millis` from `as_of_millis`.
    pub(crate) waiting_live: bool,
    /// The instant both totals were taken.
    pub(crate) as_of_millis: u64,
}

impl TaskDurations {
    /// Both totals, over one timeline, as of `as_of_millis`.
    ///
    /// The single construction site, so a caller cannot assemble a `TaskDetail`
    /// whose durations disagree with its timeline.
    pub(crate) fn compute(
        timeline: &[TimelineEntry],
        waiting_since: Option<u64>,
        as_of_millis: u64,
    ) -> Self {
        let (worked_millis, worked_live) = worked_span(timeline, as_of_millis);
        let (waiting_millis, waiting_live) = waiting_span(timeline, waiting_since, as_of_millis);
        Self {
            worked_millis,
            worked_live,
            waiting_millis,
            waiting_live,
            as_of_millis,
        }
    }

    /// The worked total extended to `now` when a window is still open.
    pub(crate) fn worked_at(&self, now: u64) -> u64 {
        self.extend(self.worked_millis, self.worked_live, now)
    }

    /// The waiting total extended to `now` when an approval is still parked.
    pub(crate) fn waiting_at(&self, now: u64) -> u64 {
        self.extend(self.waiting_millis, self.waiting_live, now)
    }

    fn extend(&self, total: u64, live: bool, now: u64) -> u64 {
        if live {
            total + now.saturating_sub(self.as_of_millis)
        } else {
            total
        }
    }
}

/// The task's worked time: each `dispatched` opens a window its `completed`
/// closes, re-dispatch opens another, and an open window runs to `now`.
///
/// A `completed` with no open window is a card journaled before dispatch
/// anchors existed: skipped, never counted from zero.
fn worked_span(timeline: &[TimelineEntry], now: u64) -> (u64, bool) {
    let mut total = 0u64;
    let mut open_at: Option<u64> = None;
    for e in timeline {
        match e.kind.as_str() {
            "dispatched" => open_at = Some(e.at_millis),
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
    (total, live)
}

/// The task's waiting-on-a-person time, interval-merged then summed.
///
/// Each resolved approval carries `waited_millis` — the exact park→resolve span,
/// already clamped to the run window — so a span is reconstructed as
/// `[at_millis - waited_millis, at_millis]` rather than inferred from gaps. A
/// still-parked approval has no resolution event yet and arrives as
/// `waiting_since`, running to `now`.
///
/// The merge matters: two approvals parked at once mean the company waited
/// *once* over the overlap, and double-counting could make waiting exceed the
/// elapsed time it is compared against.
fn waiting_span(timeline: &[TimelineEntry], waiting_since: Option<u64>, now: u64) -> (u64, bool) {
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
    (total, live)
}

/// One message in a task's discussion thread (issue #335).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscussionMessage {
    /// The journal sequence the post came from — the console's stable key, and
    /// what makes the thread strictly ordered. Shares its numbering with
    /// [`TimelineEntry::seq`]: both project out of the same journal.
    seq: u64,
    /// Epoch-millis the message was journaled.
    at_millis: u64,
    /// Who posted, as a label a reader can recognize: a roster display name (or
    /// the local part of their email), `someone` for a user id no longer on the
    /// roster, and `operator` for a post made with a machine credential. Never
    /// an email address and never a user id — a thread is read by every member
    /// of the company.
    author: String,
    /// The message text, exactly as posted (codepoint-capped on write) — or
    /// [`REDACTED_DISCUSSION_TEXT`](crate::ports::tasks::REDACTED_DISCUSSION_TEXT)
    /// once withdrawn (issue #358). The original text is never served after a
    /// withdrawal, on this route or any other.
    text: String,
    /// Whether this message was withdrawn (issue #358).
    ///
    /// Sent only when true, so every row a pre-#358 console ever rendered keeps
    /// exactly the shape it had. A console that does not know the field still
    /// shows the placeholder text rather than the withdrawn message, because
    /// the substitution happens server-side — the flag only lets a console that
    /// *does* know style the row as withdrawn instead of as something a person
    /// typed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    redacted: bool,
    /// Who withdrew it, as the same kind of label as `author`. Present only on
    /// a withdrawn row.
    ///
    /// A removal nobody's name is on is one a member can make quietly from a
    /// thread other people were reading, which is a different product than
    /// "anyone may tidy up a mistake in the open".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redacted_by: Option<String>,
}

/// The post-a-message body (`{text}`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostDiscussion {
    /// The message. Must be non-empty after trimming; truncated to
    /// [`MAX_DISCUSSION_CHARS`](crate::ports::tasks::MAX_DISCUSSION_CHARS)
    /// codepoints.
    text: String,
}

/// The assembled Task Detail response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskDetail {
    /// The card header — the same shape `GET /tasks` returns per card.
    pub(crate) task: TaskCard,
    /// The per-task event stream, oldest first.
    pub(crate) timeline: Vec<TimelineEntry>,
    /// This task's own approvals, oldest first — still-parked ones included
    /// (issue #333).
    ///
    /// The Approvals tab used to filter `timeline` for `kind == "approval"`,
    /// which could only ever show *resolutions* that happened to fall in the
    /// run window, and showed nothing at all for the one state that matters:
    /// an approval parked right now, blocking this card. This is the real
    /// query, joined on the task id the approval was parked with.
    pub(crate) approvals: Vec<TaskApproval>,
    /// The worked/waiting split, so the screen and the exported record cannot
    /// disagree about it.
    pub(crate) durations: TaskDurations,
    /// What this task has already done that cannot be undone (issue #351),
    /// oldest first. Empty for a task that only read, thought, and replied.
    ///
    /// Re-running a task re-runs its effects. The console gates Retry behind a
    /// confirmation that names these, and shows no confirmation at all when the
    /// list is empty — so this array is the whole difference between one click
    /// and a stop-and-read.
    pub(crate) irreversible_effects: Vec<IrreversibleEffect>,
    /// Whether the company's journal holds executed history it cannot describe
    /// (issue #351) — records written before descriptions existed.
    ///
    /// Company-wide, not per-task, because an undescribed record carries no card
    /// either; there is nothing to attribute it to. It is the qualifier on the
    /// field above: an empty `irreversibleEffects` means "this card did nothing
    /// irreversible" only while this is `false`. When it is `true` the console
    /// confirms a retry regardless and says earlier activity cannot be
    /// described, rather than presenting a gap as an all-clear.
    pub(crate) history_incomplete: bool,
    /// The card's discussion thread, oldest first (issue #335).
    ///
    /// Served here rather than from a route of its own so the Discussion tab
    /// costs the screen no extra read and rides the same 4s poll the timeline
    /// does — which is what makes another operator's post appear in this
    /// browser without a refresh. Empty for a card nobody has posted on, which
    /// is what the tab's honest empty state renders.
    ///
    /// Capped at the newest [`DISCUSSION_PAGE`] posts; older ones are fetched
    /// on demand with `?discussionBefore=`.
    pub(crate) discussion: Vec<DiscussionMessage>,
    /// Whether the thread has posts older than the ones in `discussion`.
    ///
    /// The console's "load earlier" affordance, and the honest half of the cap:
    /// a truncated thread that did not say it was truncated would read as the
    /// whole conversation.
    pub(crate) discussion_has_more: bool,
    /// Parent and children.
    pub(crate) lineage: Lineage,
    /// The card's recorded attempts, newest first (issue #242).
    ///
    /// Additive: a card dispatched before run records existed legitimately
    /// carries an empty list, because synthesising attempts from old
    /// `AgentReply` events would fabricate identity. Bounded by
    /// [`TASK_DETAIL_RUN_LIMIT`](crate::server::ops::runs::TASK_DETAIL_RUN_LIMIT)
    /// — a card can be re-dispatched without limit, and this read stays one
    /// cheap call.
    pub(crate) runs: Vec<RunSummary>,
    /// Epoch-millis the company started waiting on an operator *right now*
    /// (issue #305), or `None` when nothing is currently parked for this run.
    ///
    /// This is the live half of the working-vs-waiting split: a still-open
    /// approval has no `ApprovalResolved` event yet, so it cannot appear as a
    /// timeline entry, yet it is precisely the state an operator opening this
    /// screen most needs to see. Set only while the run window is open, and
    /// only from approvals parked at or after the window opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_since: Option<u64>,
}

///
/// Assembles six things the console would otherwise have to stitch client-side
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
/// * **approvals** — every approval this task parked, resolved or still
///   waiting, joined on the task id the runtime journal recorded with it
///   (issue #333). An approval parked by a build older than that carries no id,
///   and only those fall back to the old run-window correlation — so a second
///   card worked in the same window can no longer absorb this one's sign-offs;
/// * **lineage** — parent and children, from `parent_task_id`;
/// * **irreversible effects** — what the task already did that a retry would
///   do again (issue #351), read straight off the journal's executed record,
///   plus the `historyIncomplete` qualifier saying whether that record can
///   describe everything it holds;
/// * **runs** — the card's recorded attempts (issue #242), read from the run
///   store rather than the journal.
///
/// Issue #335 added the seventh, **discussion** — the card's own message thread,
/// folded out of the same journal traversal from
/// [`TaskDiscussionPosted`](CompanyEvent::TaskDiscussionPosted). It is a
/// separate array rather than another timeline `kind` because the two answer
/// different questions: the timeline is the record of what the *company* did on
/// this card, and the discussion is what *people* said about it. Folding them
/// into one list would put an operator's aside between a dispatch and its
/// completion and call it part of the run.
///
/// The thread is **paged** (`?discussionBefore=<seq>`, newest
/// [`DISCUSSION_PAGE`] by default) for the reason the timeline is not: this
/// screen polls every 4s, and a discussion is the one part of the response a
/// human grows without bound. See [`DISCUSSION_PAGE`].
///
/// 404s when the id names no card, matching `PATCH` / `DELETE`.
async fn task_detail(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
    Query(query): Query<TaskDetailQuery>,
) -> Result<Json<TaskDetail>, ApiError> {
    Ok(Json(
        assemble_detail_with_cursor(&company, &task_id, query.discussion_before).await?,
    ))
}

/// Assembles the Task Detail read. [`task_detail`] serves it as JSON; the
/// export document (issue #352) renders the *same value* to HTML.
///
/// That sharing is the export's redaction guarantee, and the reason this is a
/// function rather than a body inlined into the handler. An exporter that read
/// the journal itself would be a second, unreviewed path to the same events —
/// one scrub away from disagreeing with the console about what an operator is
/// allowed to see. Here there is nothing to keep in step: the document renders
/// what the screen renders because it is handed the identical value.
pub(crate) async fn assemble_detail(
    company: &ScopedCompany,
    task_id: &str,
) -> Result<TaskDetail, ApiError> {
    assemble_detail_with_cursor(company, task_id, None).await
}

/// Assembles task detail with an optional discussion cursor for the paged JSON
/// read. Export always calls [`assemble_detail`] and therefore receives the
/// newest discussion page while rendering the same core projection.
async fn assemble_detail_with_cursor(
    company: &ScopedCompany,
    task_id: &str,
    discussion_before: Option<u64>,
) -> Result<TaskDetail, ApiError> {
    let task_id = task_id.to_string();
    let rows = company.runtime.tasks().list(company.id()).await?;
    let card = rows
        .iter()
        .find(|t| t.id == task_id)
        .cloned()
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("task {task_id}")))?;

    // Lineage is a pure board read — no journal needed.
    let costs = costs_for_board(company, &rows).await?;
    let display_cost = |id: &str| {
        costs
            .totals
            .get(id)
            .and_then(|total| CostDisplay::new(total.total_usd, company.may_read_contents))
    };
    let parent = card
        .parent_task_id
        .as_ref()
        .and_then(|pid| rows.iter().find(|t| &t.id == pid))
        .map(|task| LineageRef::from_task(task, display_cost(&task.id)));
    let mut children: Vec<&TaskRecord> = rows
        .iter()
        .filter(|t| t.parent_task_id.as_deref() == Some(task_id.as_str()))
        .collect();
    children.sort_by_key(|t| t.updated_at_millis);
    let children = children
        .into_iter()
        .map(|task| LineageRef::from_task(task, display_cost(&task.id)))
        .collect();

    // This card's attempts (issue #242), as a set of run ids. One query, bounded
    // by the card's own attempt count — and the authoritative half of the
    // correlation: a run id resolves to exactly one card, so "was this approval
    // parked under one of *my* attempts" is answerable without opening a run.
    let task_runs = costs.run_ids.get(&task_id).cloned().unwrap_or_default();

    let TaskFold {
        mut timeline,
        mut approvals,
        discussion,
        discussion_has_more,
        window_opened_at: open_window_at,
    } = fold_task_journal(
        company,
        &task_id,
        DiscussionWindow {
            before_seq: discussion_before,
            first: DISCUSSION_PAGE,
        },
        &task_runs,
    )
    .await?;

    // Resolve the posters' labels — one roster read per detail, and only when
    // the card actually has a thread, so the 4s poll on a card nobody has
    // posted on costs exactly what it did before #335.
    let discussion = if discussion.is_empty() {
        Vec::new()
    } else {
        let authors = crate::server::chat_history::author_labels(&company.runtime).await?;
        discussion
            .into_iter()
            .map(|row| row.into_message(&authors))
            .collect()
    };

    // The still-parked half (issue #333), on exactly the resolution order
    // `approval_owner` documents below — run id first, then the parked
    // card link, and the run window only for a park that recorded neither.
    let pending: Vec<crate::runtime::types::ApprovalSummary> = company
        .runtime
        .pending_approvals()
        .into_iter()
        .filter(|a| {
            let origin = company.runtime.approval_origin(&a.id);
            match approval_owner(origin.as_ref(), &task_id, &task_runs) {
                ApprovalOwner::Mine => true,
                ApprovalOwner::NotMine => false,
                // The legacy rule, kept only for a park that recorded neither key:
                // inside an open window and parked at or after it opened, so a
                // backlog item is not re-attributed to this dispatch.
                ApprovalOwner::Unrecorded => {
                    open_window_at.is_some_and(|opened_at| a.at_millis >= opened_at)
                }
            }
        })
        .collect();

    // The live wait (issue #305): waiting started when the first of them parked.
    //
    // Still gated on an open run window, unchanged by #333. The header subtracts
    // this from the task's elapsed run time, and a finished card whose sign-off
    // was never answered would otherwise report a wait that keeps growing after
    // the work stopped — a "Worked 0s, waiting 3 days" that reads as a bug. The
    // Approvals tab below lists that row regardless, which is where it belongs.
    //
    // Clamped to the window's opening for the same reason a resolved wait is: a
    // card re-dispatched with one of its own approvals still parked charges this
    // run only for the part of the wait that this run has actually sat through.
    let waiting_since = open_window_at
        .and_then(|opened_at| pending.iter().map(|a| a.at_millis.max(opened_at)).min());

    approvals.extend(pending.into_iter().map(|a| TaskApproval {
        id: a.id.as_ref().to_string(),
        at_millis: a.at_millis,
        status: "pending".to_string(),
    }));
    approvals.sort_by(|a, b| a.at_millis.cmp(&b.at_millis).then_with(|| a.id.cmp(&b.id)));

    // The split is computed here, once, so the console and the exported record
    // read the same numbers rather than deriving them separately (#352 review).
    let durations = TaskDurations::compute(&timeline, waiting_since, now_millis());

    if let Some(entries) = costs.entries.get(&task_id) {
        timeline.extend(entries.iter().filter_map(|entry| {
            CostDisplay::new(entry.amount_usd, company.may_read_contents).map(|cost| {
                TimelineEntry {
                    seq: 0,
                    at_millis: entry.at_millis,
                    kind: "note".to_string(),
                    label: entry.label.clone(),
                    detail: None,
                    cost_key: Some(entry.key.clone()),
                    cost: Some(cost),
                    waited_millis: None,
                }
            })
        }));
        timeline.sort_by(|a, b| {
            a.at_millis.cmp(&b.at_millis).then_with(|| {
                a.cost_key
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.cost_key.as_deref().unwrap_or(""))
            })
        });
    }

    // What a retry would re-do (issue #351). A pure journal read — one indexed
    // lookup, no event scan — so a task that did nothing irreversible costs
    // nothing extra however long the company has been running.
    //
    // Redacted at the edge (issue #705): the amount is money, and a Member gets
    // the row without it. Applied here rather than in the handler because this
    // function is deliberately shared with the export document — see
    // [`assemble_detail`] — so the guarantee holds for both readers by
    // construction instead of by two callers remembering.
    let irreversible_effects = crate::server::approval_visibility::effects_for_principal(
        company.may_read_contents,
        company
            .runtime
            .irreversible_effects(&task_id)
            .into_iter()
            .map(IrreversibleEffect::from)
            .collect(),
    );

    // An indexed store read, not another journal pass (issue #242).
    let runs = runs_for_task(company, &task_id).await?;

    Ok(TaskDetail {
        task: {
            let cost = display_cost(&card.id);
            let mut task = TaskCard::from(card);
            task.cost = cost;
            task
        },
        timeline,
        approvals,
        durations,
        irreversible_effects,
        history_incomplete: company.runtime.has_undescribed_history(),
        discussion,
        discussion_has_more,
        lineage: Lineage { parent, children },
        runs,
        waiting_since,
    })
}

/// How many journal events one `read_from` page pulls.
///
/// The scan is bounded per page rather than per request: a task's events can sit
/// anywhere in a company's history, so the whole log must still be *traversed* —
/// but it is never all *resident* at once.
const TIMELINE_PAGE: usize = 512;

/// How many discussion posts one detail read answers with.
///
/// The timeline is bounded by what the *company* did on one card; a discussion
/// is bounded by nothing — people keep typing. Without a cap the whole thread
/// comes back on every 4s poll of an open detail screen, per browser, forever:
/// a card with a few hundred posts at
/// [`MAX_DISCUSSION_CHARS`](crate::ports::tasks::MAX_DISCUSSION_CHARS) each is hundreds
/// of kilobytes re-serialized fifteen times a minute, and it only grows.
///
/// So the read answers with the tail — what somebody opening the card actually
/// reads first — and everything older is fetched on demand behind
/// `?discussionBefore=<seq>`. That is the `first` + `before_seq` shape
/// [`chat_history::history_for_desk`](crate::server::chat_history::history_for_desk)
/// already uses for a desk transcript, which has the same unbounded-writer
/// problem and answered it the same way.
const DISCUSSION_PAGE: usize = 50;

/// The detail read's query string.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskDetailQuery {
    /// An opaque journal cursor: only posts *before* this `seq` are considered,
    /// so the console walks backwards through the thread by passing the `seq` of
    /// the oldest message it holds. Absent means the newest page.
    ///
    /// Only the discussion is paged — the timeline and the lineage are bounded
    /// by the card's own run history and come back whole.
    #[serde(default)]
    discussion_before: Option<u64>,
}

/// Which slice of a thread one fold should keep.
#[derive(Debug, Clone, Copy)]
struct DiscussionWindow {
    /// Exclusive upper cursor: keep posts with `seq <` this. `None` is "up to
    /// the newest".
    before_seq: Option<u64>,
    /// How many of the most recent remaining posts to keep.
    first: usize,
}

/// A discussion post as the fold reads it, before its author is resolved.
///
/// The journal carries an [`Actor`](crate::ports::types::Actor); the wire
/// carries a label. Keeping the two apart means the roster is read once per
/// request rather than once per message.
#[derive(Debug)]
struct DiscussionRow {
    seq: u64,
    at_millis: u64,
    text: String,
    by: Option<crate::ports::types::Actor>,
    /// Set when a later [`CompanyEvent::TaskDiscussionRedacted`] supersedes this
    /// post (issue #358): the withdrawer, or `None` for a machine credential.
    ///
    /// `Option<Option<Actor>>` reads awkwardly and means exactly what it says:
    /// the outer layer is "was it withdrawn", the inner is "by a named person".
    /// Collapsing them would make an anonymous withdrawal indistinguishable
    /// from no withdrawal at all, which is the one confusion this row cannot
    /// afford.
    redacted_by: Option<Option<crate::ports::types::Actor>>,
}

impl DiscussionRow {
    /// Resolves the poster against a `user id → label` map (see
    /// [`crate::server::chat_history::author_labels`]).
    fn into_message(
        self,
        authors: &std::collections::HashMap<String, String>,
    ) -> DiscussionMessage {
        use crate::ports::types::ActorKind;
        let author = match &self.by {
            // A signed-in human: name them from the roster. A user who has
            // since been removed reads as "someone" rather than as a raw id.
            Some(actor) if actor.kind == ActorKind::User => authors
                .get(&actor.id)
                .cloned()
                .unwrap_or_else(|| "someone".to_string()),
            // A machine credential, or a post journaled before attribution
            // existed: there is nobody to name. Same fallback the desk
            // transcript takes for an unattributed operator message.
            _ => "operator".to_string(),
        };
        // Issue #358. The substitution happens HERE, on the way out, rather
        // than being left to the caller: this is the one place a journalled
        // post becomes a wire row, so a withdrawn message cannot reach a reader
        // through a surface that forgot to check the flag.
        let (text, redacted, redacted_by) = match self.redacted_by {
            Some(by) => (
                crate::ports::tasks::REDACTED_DISCUSSION_TEXT.to_string(),
                true,
                Some(label_for(&by, authors)),
            ),
            None => (self.text, false, None),
        };
        DiscussionMessage {
            seq: self.seq,
            at_millis: self.at_millis,
            author,
            text,
            redacted,
            redacted_by,
        }
    }
}

/// An [`Actor`](crate::ports::types::Actor) as a label a reader recognizes.
///
/// The same three-way resolution the author label above uses — a roster display
/// name, `someone` for a user no longer on the roster, `operator` for a machine
/// credential — factored out because #358 gave the row a second person to name.
fn label_for(
    actor: &Option<crate::ports::types::Actor>,
    authors: &std::collections::HashMap<String, String>,
) -> String {
    use crate::ports::types::ActorKind;
    match actor {
        Some(actor) if actor.kind == ActorKind::User => authors
            .get(&actor.id)
            .cloned()
            .unwrap_or_else(|| "someone".to_string()),
        _ => "operator".to_string(),
    }
}

/// What one traversal of the company journal yields for a single task.
#[derive(Debug, Default)]
struct TaskFold {
    /// The run record, oldest first.
    timeline: Vec<TimelineEntry>,
    /// This task's resolved approvals, oldest first.
    approvals: Vec<TaskApproval>,
    /// The discussion thread, oldest first (issue #335) — at most
    /// [`DiscussionWindow::first`] posts, the newest inside the window.
    discussion: Vec<DiscussionRow>,
    /// Whether the window dropped an older post, i.e. the thread continues
    /// behind the page.
    discussion_has_more: bool,
    /// The instant a *still-open* dispatch window opened, or `None` when the
    /// task is not mid-run — the anchor the live wait (#305) is scoped to.
    window_opened_at: Option<u64>,
}

/// Folds the company journal down to one task's timeline and discussion.
///
/// Oldest-first, paged. `window` opens on this task's dispatch anchor and closes
/// on its completion anchor; untagged-but-windowed events (approvals) are only
/// admitted while it is open, so a resolution belonging to a different task's
/// run never leaks in.
///
/// The discussion rides this same traversal rather than a second one: both
/// projections read the same log, and the journal is long enough that scanning
/// it twice per detail poll would be the whole cost of the tab. `discussion`
/// says which slice of the thread to keep; posts outside it are dropped as they
/// are read, so a ten-thousand-post thread is traversed but never resident.
///
/// **Why the scan does not stop at the first completion anchor.** A card can be
/// re-dispatched — moved back to `in_progress` after review — which opens a
/// second dispatch → completion cycle later in the same log. Stopping at the
/// first `DeskTaskCompleted` would silently truncate every run after the first,
/// which is worse than the cost it saves. Bounding the page size gives the
/// memory win without that correctness loss; a stored per-task dispatch offset
/// is the durable fix for the traversal cost and is left to the epic.
///
/// Returns the timeline, discussion, and this task's resolved approvals
/// alongside the instant the still-open window opened, or `None` when the task
/// is not mid-run.
async fn fold_task_journal(
    company: &ScopedCompany,
    task_id: &str,
    discussion: DiscussionWindow,
    task_runs: &std::collections::HashSet<String>,
) -> Result<TaskFold, ApiError> {
    use crate::ports::types::{ApprovalId, EventSeq};

    // Per-id, not a snapshot. The origin index is unbounded and never pruned
    // (see `ApprovalOrigin`), while a fold resolves only the approval events on
    // its own pages — and this route is polled, so a snapshot would copy the
    // company's whole approval history every few seconds.
    let approval_origin = |id: &ApprovalId| company.runtime.approval_origin(id);

    let mut fold = TaskFold::default();
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
            discussion,
            approval_origin,
            task_runs,
            &mut fold,
        );
        if exhausted {
            break;
        }
    }
    Ok(fold)
}

/// What an approval's recorded keys say about who owns it.
///
/// Three states, deliberately — the whole correction this makes to #333's first
/// cut. That version asked `origins.get(id).and_then(|o| o.task_id)`, which is
/// `None` both for *no origin recorded* and for *origin recorded, belonging to
/// no card*, and sent both to the run window. Since #333 every unlinked park
/// records itself as such, so the second case is not rare — it is every workflow
/// delivery, every chat turn, every scheduler tick, and the hosted brain's own
/// gate — and each one landed on whatever card happened to be running, dragging
/// that card's `waitingSince` with it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ApprovalOwner {
    /// Recorded as belonging to the card being read.
    Mine,
    /// Recorded as belonging to a different card, or to no card at all.
    NotMine,
    /// Neither key recorded — a park written before #333. The **only** state
    /// that may fall back to the run-window heuristic, and the caller decides
    /// how, because the resolved and still-parked halves scope it differently.
    Unrecorded,
}

/// Which card owns an approval, resolved attempt-first (issues #333 + #242).
///
/// Both keys are kept, and the order between them is load-bearing, because
/// neither is a superset of the other:
///
/// 1. **`run_id` — attempt-level, authoritative.** A
///    [`RunRecord`](crate::ports::runs::RunRecord) names its card, so a run id
///    resolves to a task; a task id can never resolve to a run. #183 settled
///    that repeat trips through review are normal, so two attempts on one card
///    is the expected case, and only this key separates them. Checked against
///    `task_runs`, this card's own attempt ids. Any present id takes precedence:
///    an id missing from this card's run set returns `NotMine` without consulting
///    the card-level link, including when the run record is unavailable.
/// 2. **the parked [`TaskLink`](crate::runtime::journal::TaskLink) —
///    card-level, the fallback.** `run_id` is
///    `None` by design wherever no attempt is behind the park — a chat turn, a
///    workflow delivery, a scheduler tick, the hosted brain's gate — and those
///    parks are stamped here instead, in `CycleHostImpl::park`, which every
///    park path passes through. So the run id cannot be the only key without
///    losing all of them.
/// 3. **neither recorded** — and only then is the answer "unknown" rather than
///    "unlinked". A park that recorded a link saying `Unlinked` is a *resolved*
///    answer: it belongs to no card, so it does not belong to this one either.
fn approval_owner(
    origin: Option<&crate::runtime::journal::ApprovalOrigin>,
    task_id: &str,
    task_runs: &std::collections::HashSet<String>,
) -> ApprovalOwner {
    let Some(origin) = origin else {
        return ApprovalOwner::Unrecorded;
    };
    // 1. The attempt wins wherever there is one.
    if let Some(run_id) = origin.run_id.as_deref() {
        return if task_runs.contains(run_id) {
            ApprovalOwner::Mine
        } else {
            ApprovalOwner::NotMine
        };
    }
    // 2. Otherwise the card the parking cycle stamped. 3. Or nothing at all.
    match &origin.task {
        Some(link) if link.task_id() == Some(task_id) => ApprovalOwner::Mine,
        Some(_) => ApprovalOwner::NotMine,
        None => ApprovalOwner::Unrecorded,
    }
}

/// Folds one page of journal events onto `fold`, carrying the window state
/// across pages.
///
/// `fold.window_opened_at` is both the window flag and its anchor: `Some(at)`
/// while a dispatch is open, `None` once it closes. `approval_origin` resolves what an
/// approval was when it parked, to recover waiting time (#305) and the owning
/// task (#333), and `task_runs` this card's attempt ids (#242), the
/// authoritative half of that ownership test — see [`approval_owner`]. The
/// origin arrives as a per-id lookup rather than a snapshot: that index is
/// unbounded and never pruned, and this route is polled. Both are parameters so
/// this stays a pure function of its inputs.
/// Resolved approvals land on `approvals` as well as on the timeline — same
/// row, two surfaces.
fn fold_page(
    page: &[crate::ports::types::StoredEvent],
    task_id: &str,
    discussion: DiscussionWindow,
    approval_origin: impl Fn(
        &crate::ports::types::ApprovalId,
    ) -> Option<crate::runtime::journal::ApprovalOrigin>,
    task_runs: &std::collections::HashSet<String>,
    fold: &mut TaskFold,
) {
    use crate::ports::types::ActorKind;

    // Carried by value across the page and written back at the end, so the
    // window state and the two output lists are never borrowed from `fold` at
    // the same time.
    let mut window_opened_at = fold.window_opened_at;
    for ev in page {
        // The discussion arm short-circuits before the timeline match: a post is
        // the other projection of this task, never a run event, so it must not
        // reach `timeline` even as an unlabelled row.
        if let CompanyEvent::TaskDiscussionPosted {
            task_id: id,
            text,
            by,
        } = &ev.event
            && id == task_id
        {
            // Outside the cursor: the caller is walking backwards through the
            // thread and already holds this post. Skipped before the cap so the
            // page ends where the caller asked it to, not one post short.
            if discussion
                .before_seq
                .is_some_and(|before| ev.seq.value() >= before)
            {
                continue;
            }
            fold.discussion.push(DiscussionRow {
                seq: ev.seq.value(),
                at_millis: ev.at_millis,
                text: text.clone(),
                by: by.clone(),
                // Filled in below if a later tombstone names this seq. The log
                // is walked forward, so the post is always seen before the
                // event that withdraws it.
                redacted_by: None,
            });
            // Keep the newest `first`, dropping from the front as the traversal
            // moves forward. The thread is still read oldest-first (the window
            // state and the timeline demand one pass), but only a page of it is
            // ever held — an unbounded thread costs the fold a constant.
            if fold.discussion.len() > discussion.first {
                fold.discussion.remove(0);
                fold.discussion_has_more = true;
            }
            continue;
        }
        // Issue #358: a withdrawal supersedes a post this fold may already be
        // holding. Deliberately NOT subject to the `before_seq` cursor above:
        // the cursor pages backwards through the thread, so a tombstone written
        // after the cursor is exactly the one that withdraws a message the
        // caller is about to read — skipping it would serve the original text
        // to anybody who scrolled far enough back.
        if let CompanyEvent::TaskDiscussionRedacted {
            task_id: id,
            seq,
            by,
        } = &ev.event
            && id == task_id
        {
            if let Some(row) = fold.discussion.iter_mut().find(|row| row.seq == *seq) {
                row.redacted_by = Some(by.clone());
            }
            // A tombstone naming a post already dropped from the window needs
            // nothing done: the row it withdraws is not in this page, and the
            // page that does hold it folds this same event on its own pass.
            continue;
        }
        let entry = match &ev.event {
            // `..` since #357: the variant gained `run_id`, which this
            // projection has no use for — the anchor is the instant, not the
            // attempt.
            CompanyEvent::TaskDispatched { task_id: id, .. } if id == task_id => {
                window_opened_at = Some(ev.at_millis);
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
                ..
            } if id == task_id => {
                window_opened_at = None;
                Some((
                    "completed",
                    format!("Finished on {desk} → {column}"),
                    Some(output.clone()),
                    None,
                ))
            }
            // Id-correlated (#333), falling back to the window only for an
            // park that recorded neither key — see `approval_owner`.
            // The operator's identity is deliberately dropped: it can carry a
            // user id, matching the SSE projection's deny-by-default stance.
            // Only `by.kind` is read, which names a category and never a person.
            CompanyEvent::ApprovalResolved {
                approval_id,
                verdict,
                by,
            } if match approval_owner(approval_origin(approval_id).as_ref(), task_id, task_runs) {
                ApprovalOwner::Mine => true,
                ApprovalOwner::NotMine => false,
                // Pre-#333: neither key recorded, so the old window heuristic
                // is all there is. Scoped by the window being open at this
                // point in the fold, which is where a resolution sits.
                ApprovalOwner::Unrecorded => window_opened_at.is_some(),
            } =>
            {
                let origin = approval_origin(approval_id);
                let origin = origin.as_ref();
                // The approval id joins the resolution back to the journal's
                // park instant. Clamping to the window's opening keeps a wait
                // that began before this task was dispatched from charging its
                // pre-dispatch portion to this run.
                let waited = origin.map(|origin| {
                    let parked_at = origin.at_millis;
                    let from = window_opened_at.map_or(parked_at, |opened| parked_at.max(opened));
                    ev.at_millis.saturating_sub(from)
                });
                // A system actor here is the TTL sweep, not a person: expiry
                // resolves to a default-deny, and saying "Approval denied" for
                // it would read as though somebody looked at it and said no.
                let expired = by.kind == ActorKind::System;
                let label = if expired {
                    "Approval expired (auto-denied)".to_string()
                } else {
                    format!(
                        "Approval {}",
                        crate::brain::medulla::effects::verdict_word(*verdict)
                    )
                };
                fold.approvals.push(TaskApproval {
                    id: approval_id.as_ref().to_string(),
                    // An origin is missing only for a park this journal never
                    // saw; fall back to the resolution instant so the row still
                    // sorts sanely among its siblings.
                    at_millis: origin.map_or(ev.at_millis, |origin| origin.at_millis),
                    status: if expired {
                        "expired".to_string()
                    } else {
                        crate::brain::medulla::effects::verdict_word(*verdict).to_string()
                    },
                });
                Some(("approval", label, None, waited))
            }
            _ => None,
        };
        if let Some((kind, label, detail, waited_millis)) = entry {
            fold.timeline.push(TimelineEntry {
                seq: ev.seq.value(),
                at_millis: ev.at_millis,
                kind: kind.to_string(),
                label,
                detail,
                cost_key: None,
                cost: None,
                waited_millis,
            });
        }
    }
    fold.window_opened_at = window_opened_at;
}

/// `POST …/tasks/{task_id}/discussion` — post a message to a card's thread
/// (issue #335).
///
/// The write half of the Discussion tab. A post is journaled as
/// [`CompanyEvent::TaskDiscussionPosted`] and read back by
/// [`task_detail`], so it survives a reload and is visible to every operator of
/// the company on their next poll — one store, no per-browser state.
///
/// Validation: a `text` that is empty or whitespace-only is a `400` (an empty
/// row in a thread is noise nobody can remove — there is no delete in v1), and
/// an unknown card is a `404`, matching `PATCH` / `DELETE`. Over-long text is
/// truncated rather than rejected (see
/// [`MAX_DISCUSSION_CHARS`](crate::ports::tasks::MAX_DISCUSSION_CHARS)).
///
/// Unlike a chat message this runs **no cycle**: posting is a note on the card,
/// not a way to ask an agent for something. Dispatching work is the board's job
/// and spending money stays behind the column drag.
///
/// Answers `201` with the stored message — the journaled row, read back at its
/// own `seq` rather than re-stamped — so the console renders the post at once
/// instead of waiting out the 4s poll, and the row it renders is byte-for-byte
/// the one the next poll returns under the same key.
async fn post_discussion(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
    Json(body): Json<PostDiscussion>,
) -> Result<(StatusCode, Json<DiscussionMessage>), ApiError> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "a discussion message cannot be empty".to_string(),
        )));
    }
    let text = cap_discussion(text);

    // The card must exist: a thread on a deleted (or mistyped) id would be
    // written into the journal and then be unreachable from every read surface.
    let exists = company
        .runtime
        .tasks()
        .list(company.id())
        .await?
        .into_iter()
        .any(|t| t.id == task_id);
    if !exists {
        return Err(ApiError(OpenCompanyError::NotFound(format!(
            "task {task_id}"
        ))));
    }

    let by = company.actor.clone();
    // Not best-effort, unlike the steer audit: here the journal append IS the
    // write. A swallowed failure would show the operator a posted message that
    // vanishes on the next poll.
    let seq = company
        .runtime
        .events()
        .append(
            company.id(),
            CompanyEvent::TaskDiscussionPosted {
                task_id,
                text: text.clone(),
                by: by.clone(),
            },
        )
        .await?;

    // The echo is *the journaled row*, not a re-stamp of it: reading the event
    // back at its own `seq` is one bounded read, and it means the message the
    // console renders now carries the same `atMillis` the next poll returns
    // under the same key — a locally re-stamped copy would silently shift the
    // time by however long the append took. The local clock is the fallback if
    // that read comes back empty; the append itself already succeeded, so the
    // message is not lost either way.
    let at_millis = company
        .runtime
        .events()
        .read_from(company.id(), seq, 1)
        .await
        .ok()
        .and_then(|page| page.into_iter().next())
        .filter(|stored| stored.seq == seq)
        .map(|stored| stored.at_millis)
        .unwrap_or_else(now_millis);
    let authors = crate::server::chat_history::author_labels(&company.runtime).await?;
    let message = DiscussionRow {
        seq: seq.value(),
        at_millis,
        text,
        by,
        // A message cannot be withdrawn before it exists: the tombstone (#358)
        // names this `seq`, and this is the request that mints it.
        redacted_by: None,
    }
    .into_message(&authors);
    Ok((StatusCode::CREATED, Json(message)))
}

/// `DELETE …/tasks/{task_id}/discussion/{seq}` — withdraw a posted message
/// (issue #358).
///
/// ## What it does, and what it deliberately does not
///
/// It appends [`CompanyEvent::TaskDiscussionRedacted`], which supersedes the
/// post at `seq`. It does **not** rewrite or remove the original event: the log
/// is append-only, sequence numbers are stable ids other events name, and
/// import replays a bundle from zero to reproduce them — three properties a
/// mutation here would break for the sake of one tab.
///
/// What the withdrawal buys is the two things that actually matter to somebody
/// who has just pasted a credential into a card:
///
/// * **every read surface stops serving the text.** The substitution is in the
///   fold, so the console thread and the task-detail export document (#352,
///   which renders the same assembled value) change together and cannot drift;
/// * **the bundle stops carrying it.** [`store::export`](crate::store::export)
///   applies the same substitution to `events.jsonl`, so a round trip cannot
///   resurrect the message — the half that turns a permanent record into a
///   portable one.
///
/// It is not an at-rest erasure on the instance that already holds the bytes,
/// and the route does not pretend to be: rotating a leaked secret is still the
/// remedy. See `docs/modules/server/README.md`.
///
/// ## Who may
///
/// [`ScopedCompany`] — any member of the company, the same authority every
/// other task write on this router carries. Not admin-only, and that is a
/// deliberate match rather than an oversight: `DELETE …/tasks/{task_id}` lets
/// the same member remove the entire card, thread and all, so gating one
/// message above the card that contains it would be an incoherent boundary. The
/// withdrawal is attributed instead — `by` names whoever did it, and the thread
/// says so beside the row.
///
/// ## Answers
///
/// `200` with the withdrawn row as every reader now sees it. `404` when the
/// card is unknown, or when `seq` names anything other than a discussion post
/// on *this* card — a tombstone that pointed at another task's post, or at a
/// dispatch, would be a way to write nonsense into the journal. Withdrawing an
/// already-withdrawn message is a no-op success: a removal asked for twice has
/// still happened, and a `409` there would only make a retry look like a
/// failure.
async fn redact_discussion(
    company: ScopedCompany,
    Path(DiscussionPath { task_id, seq }): Path<DiscussionPath>,
) -> Result<Json<DiscussionMessage>, ApiError> {
    use crate::ports::types::EventSeq;

    let target = EventSeq::new(seq);
    let stored = company
        .runtime
        .events()
        .read_from(company.id(), target, 1)
        .await?
        .into_iter()
        .next()
        .filter(|stored| stored.seq == target)
        .ok_or_else(|| {
            ApiError(OpenCompanyError::NotFound(format!(
                "no discussion message {seq} on task {task_id}"
            )))
        })?;

    // The event at that position must be a post on THIS card. Anything else —
    // another task's post, a dispatch, a chat reply — is a 404 rather than a
    // silently written tombstone nothing will ever fold.
    let (posted_at, text, posted_by) = match &stored.event {
        CompanyEvent::TaskDiscussionPosted {
            task_id: id,
            text,
            by,
        } if *id == task_id => (stored.at_millis, text.clone(), by.clone()),
        _ => {
            return Err(ApiError(OpenCompanyError::NotFound(format!(
                "no discussion message {seq} on task {task_id}"
            ))));
        }
    };

    // Already withdrawn: answer the row as it stands rather than appending a
    // second tombstone. Two tombstones for one post would fold identically, so
    // this is about not growing the journal on a retry.
    let existing = find_redaction(&company, &task_id, seq).await?;
    let redacted_by = match existing {
        Some(by) => by,
        None => {
            let by = company.actor.clone();
            company
                .runtime
                .events()
                .append(
                    company.id(),
                    CompanyEvent::TaskDiscussionRedacted {
                        task_id: task_id.clone(),
                        seq,
                        by: by.clone(),
                    },
                )
                .await?;
            by
        }
    };

    let authors = crate::server::chat_history::author_labels(&company.runtime).await?;
    let message = DiscussionRow {
        seq,
        at_millis: posted_at,
        // Carried and then dropped by `into_message`, which substitutes the
        // placeholder for a withdrawn row. Never sent.
        text,
        by: posted_by,
        redacted_by: Some(redacted_by),
    }
    .into_message(&authors);
    Ok(Json(message))
}

/// The existing withdrawal of `seq` on `task_id`, if the thread already carries
/// one.
///
/// Walks the journal rather than the folded page because the fold is windowed:
/// a post far enough back to have been dropped from the page is exactly the one
/// a retry is most likely to name.
async fn find_redaction(
    company: &ScopedCompany,
    task_id: &str,
    seq: u64,
) -> Result<Option<Option<crate::ports::types::Actor>>, ApiError> {
    use crate::ports::types::EventSeq;

    let mut next = seq + 1;
    loop {
        let page = company
            .runtime
            .events()
            .read_from(company.id(), EventSeq::new(next), TIMELINE_PAGE)
            .await?;
        if page.is_empty() {
            return Ok(None);
        }
        for stored in &page {
            if let CompanyEvent::TaskDiscussionRedacted {
                task_id: id,
                seq: target,
                by,
            } = &stored.event
                && id == task_id
                && *target == seq
            {
                return Ok(Some(by.clone()));
            }
        }
        next = page
            .last()
            .map(|stored| stored.seq.value() + 1)
            .unwrap_or(next);
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
/// `redirect` without a non-empty `instruction`; `redirect` with an instruction
/// longer than [`MAX_REDIRECT_CHARS`]. An unknown key is `404`; a card that
/// exists but is not in flight is `409`. On accept the run's control is set,
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
            // Refuse rather than silently cap. The operator is synchronously
            // present and the console surfaces this 400, so telling them the
            // instruction is too long beats handing the agent a halved one that
            // reads — in the turn and in the audit trail — exactly like the
            // whole of what they typed. Count characters, never bytes.
            let chars = instruction.chars().count();
            if chars > MAX_REDIRECT_CHARS {
                return Err(bad(&format!(
                    "redirect instruction is {chars} characters; the limit is {MAX_REDIRECT_CHARS}"
                )));
            }
            SteerAction::Redirect {
                instruction: instruction.to_string(),
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

#[cfg(test)]
mod durations_test {
    use super::*;

    /// 2026-08-05 09:00:00 UTC.
    const T0: u64 = 1_785_920_400_000;

    fn entry(at: u64, kind: &str, waited: Option<u64>) -> TimelineEntry {
        TimelineEntry {
            seq: at,
            at_millis: at,
            kind: kind.to_string(),
            label: kind.to_string(),
            detail: None,
            cost_key: None,
            cost: None,
            waited_millis: waited,
        }
    }

    /// A re-dispatched card accumulates every worked window, not just the first.
    #[test]
    fn worked_accumulates_every_dispatch_window() {
        let d = TaskDurations::compute(
            &[
                entry(T0, "dispatched", None),
                entry(T0 + 60_000, "completed", None),
                entry(T0 + 600_000, "dispatched", None),
                entry(T0 + 720_000, "completed", None),
            ],
            None,
            T0,
        );
        assert_eq!(d.worked_millis, 180_000);
        assert!(!d.worked_live);
    }

    /// A `completed` with no open window is legacy data: skipped, not counted
    /// from zero, which would report the whole epoch as worked time.
    #[test]
    fn a_completion_without_a_dispatch_is_not_counted_from_zero() {
        let d = TaskDurations::compute(&[entry(T0 + 60_000, "completed", None)], None, T0);
        assert_eq!(d.worked_millis, 0);
    }

    /// Overlapping waits are merged, not summed twice — the company waited once.
    #[test]
    fn overlapping_waits_are_merged() {
        let d = TaskDurations::compute(
            &[
                entry(T0 + 300_000, "approval", Some(300_000)), // [T0,     T0+5m]
                entry(T0 + 420_000, "approval", Some(300_000)), // [T0+2m,  T0+7m]
            ],
            None,
            T0,
        );
        assert_eq!(d.waiting_millis, 420_000, "the overlap was counted twice");
        assert!(!d.waiting_live);
    }

    /// A sign-off resolved instantly (`0`) or with no recoverable park instant
    /// (`None`) is not a span. Counting either would invent waiting time.
    #[test]
    fn an_instant_or_unknown_sign_off_is_not_a_wait() {
        let d = TaskDurations::compute(
            &[
                entry(T0 + 60_000, "approval", Some(0)),
                entry(T0 + 120_000, "approval", None),
            ],
            None,
            T0,
        );
        assert_eq!(d.waiting_millis, 0);
    }

    /// The live halves run to `as_of`, and `*_at` extends them past it.
    ///
    /// This is the property the console's 1s tick and the exporter both rely on:
    /// a reader adds elapsed time to the live half and nothing else, because
    /// every closed span already ended before `as_of_millis`.
    #[test]
    fn live_spans_extend_exactly_and_sealed_ones_do_not() {
        let live = TaskDurations::compute(
            &[entry(T0, "dispatched", None)],
            Some(T0 + 60_000),
            T0 + 300_000,
        );
        assert!(live.worked_live && live.waiting_live);
        assert_eq!(live.worked_millis, 300_000);
        assert_eq!(live.waiting_millis, 240_000);
        // One more minute of wall clock adds one minute to each live half.
        assert_eq!(live.worked_at(T0 + 360_000), 360_000);
        assert_eq!(live.waiting_at(T0 + 360_000), 300_000);

        let sealed = TaskDurations::compute(
            &[
                entry(T0, "dispatched", None),
                entry(T0 + 600_000, "completed", None),
            ],
            None,
            T0 + 600_000,
        );
        assert!(!sealed.worked_live && !sealed.waiting_live);
        // A finished task's totals do not move, however late it is read.
        assert_eq!(sealed.worked_at(T0 + 99_000_000), 600_000);
        assert_eq!(sealed.waiting_at(T0 + 99_000_000), 0);
    }

    /// A client clock behind the host's cannot subtract from a total.
    #[test]
    fn a_reader_clock_behind_the_host_does_not_go_backwards() {
        let d = TaskDurations::compute(&[entry(T0, "dispatched", None)], None, T0 + 300_000);
        assert_eq!(d.worked_at(T0), 300_000);
    }
}

/// The redirect bound at the route boundary: an operator who typed too much is
/// told so, and one who typed exactly the limit gets every character through.
#[cfg(test)]
mod steer_redirect_test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::company::CompanyManifest;
    use crate::company::steer::InflightKind;
    use crate::ports::types::{CompanyId, CompanyRecord, EventSeq};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn manifest() -> CompanyManifest {
        toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .unwrap()
    }

    /// A company with one run already in flight under the key `active`, so the
    /// steer route reaches its accept path. The caller must hold the returned
    /// registration guard: dropping it deregisters the run.
    async fn state_with_inflight_run(
        home: &std::path::Path,
    ) -> (AppState, crate::company::steer::RegistrationGuard) {
        use crate::ports::CompanyStore;
        let id = CompanyId::new("acme");
        FsCompanyStore::new(home.to_path_buf())
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let guard = runtime.steer().register(
            &id,
            InflightEntry {
                key: "active".into(),
                task_id: Some("active".into()),
                kind: InflightKind::Task,
                title: "Active".into(),
                agent_id: "ceo".into(),
                started_at_millis: 1,
                pending_action: None,
            },
        );
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        (state, guard)
    }

    async fn steer(state: &AppState, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/company/tasks/active/steer")
            .header("content-type", "application/json")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    // ── Deleting a card that is running (issue #984) ─────────────────────────

    /// Puts a card on the board so a delete has something to remove.
    async fn seed_card(state: &AppState, id: &str) {
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).unwrap();
        runtime
            .tasks()
            .upsert(
                &company,
                &crate::ports::tasks::TaskRecord {
                    id: id.to_string(),
                    title: "Draft the launch note".to_string(),
                    note: None,
                    column: crate::ports::tasks::COLUMN_IN_PROGRESS.to_string(),
                    priority: "medium".to_string(),
                    assignee: String::new(),
                    updated_at_millis: 1,
                    origin_chat_id: None,
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    bounced: None,
                },
            )
            .await
            .unwrap();
    }

    async fn delete_card(state: &AppState, id: &str) -> StatusCode {
        let request = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/company/tasks/{id}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    async fn board_has(state: &AppState, id: &str) -> bool {
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).unwrap();
        runtime
            .tasks()
            .list(&company)
            .await
            .unwrap()
            .iter()
            .any(|task| task.id == id)
    }

    /// **A running card cannot be deleted out from under its turn.**
    ///
    /// The settle path writes the card back from the harness's in-memory clone,
    /// so a delete that lands mid-turn does not remove the card — it removes it
    /// until the turn finishes and then gets it back, in `in_review`/`done`,
    /// after every chat chip naming it has already gone. That is a card on the
    /// board that nothing can reach, which is the failure #984 is about.
    ///
    /// The card staying on the board is asserted as well as the status: a `409`
    /// that had already deleted the row would be worse than no check at all.
    #[tokio::test]
    async fn deleting_a_running_card_is_refused_and_leaves_it_on_the_board() {
        let home = tempfile::tempdir().unwrap();
        let (state, _guard) = state_with_inflight_run(home.path()).await;
        seed_card(&state, "active").await;

        assert_eq!(delete_card(&state, "active").await, StatusCode::CONFLICT);
        assert!(
            board_has(&state, "active").await,
            "the refusal must not have deleted it anyway"
        );
    }

    /// And the refusal is aimed at the running card, not at deletes in general.
    ///
    /// Without this, a guard that refused *every* delete would satisfy the test
    /// above — the board's own delete would be broken and the suite would still
    /// be green.
    #[tokio::test]
    async fn deleting_a_card_that_is_not_running_still_works() {
        let home = tempfile::tempdir().unwrap();
        let (state, _guard) = state_with_inflight_run(home.path()).await;
        seed_card(&state, "idle").await;

        assert_eq!(delete_card(&state, "idle").await, StatusCode::NO_CONTENT);
        assert!(!board_has(&state, "idle").await);
    }

    /// The instruction the run's audit event recorded, if any.
    async fn journaled_redirect(state: &AppState) -> Option<String> {
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).unwrap();
        runtime
            .events()
            .read_from(&company, EventSeq::new(0), usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .find_map(|stored| match stored.event {
                CompanyEvent::TaskSteered { instruction, .. } => instruction,
                _ => None,
            })
    }

    #[tokio::test]
    async fn an_over_length_redirect_is_refused_naming_the_bound() {
        let home = tempfile::Builder::new()
            .prefix("opencompany-steer-")
            .tempdir()
            .unwrap();
        let (state, _inflight) = state_with_inflight_run(home.path()).await;

        let too_long = "a".repeat(MAX_REDIRECT_CHARS + 1);
        let (status, body) = steer(
            &state,
            json!({"action": "redirect", "instruction": too_long}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"].as_str().unwrap_or_default();
        assert!(
            message.contains(&MAX_REDIRECT_CHARS.to_string()),
            "the refusal names the limit: {message}"
        );
        assert!(
            message.contains(&(MAX_REDIRECT_CHARS + 1).to_string()),
            "the refusal names the actual length: {message}"
        );
        assert!(
            journaled_redirect(&state).await.is_none(),
            "a refused redirect never reaches the run or the audit trail"
        );
    }

    #[tokio::test]
    async fn an_over_length_multibyte_redirect_is_refused_not_split() {
        let home = tempfile::Builder::new()
            .prefix("opencompany-steer-")
            .tempdir()
            .unwrap();
        let (state, _inflight) = state_with_inflight_run(home.path()).await;

        // Every character is 2 bytes, so a byte-indexed bound would land
        // mid-codepoint and panic. The route counts characters.
        let too_long = "é".repeat(MAX_REDIRECT_CHARS + 1);
        let (status, body) = steer(
            &state,
            json!({"action": "redirect", "instruction": too_long}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&(MAX_REDIRECT_CHARS + 1).to_string()),
            "length is counted in characters, not bytes: {body}"
        );
    }

    #[tokio::test]
    async fn an_at_limit_redirect_is_accepted_verbatim() {
        let home = tempfile::Builder::new()
            .prefix("opencompany-steer-")
            .tempdir()
            .unwrap();
        let (state, _inflight) = state_with_inflight_run(home.path()).await;

        let exact = "a".repeat(MAX_REDIRECT_CHARS);
        let (status, _) = steer(&state, json!({"action": "redirect", "instruction": exact})).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(
            journaled_redirect(&state).await.as_deref(),
            Some(exact.as_str()),
            "an at-limit instruction reaches the run whole — no cut, no marker"
        );
    }
}

/// `PATCH …/tasks/{task_id}` must hand back the row it actually persisted.
///
/// `upsert_task` clears a stale `bounced` chip on a clone when a card leaves
/// `todo` any way other than a re-dispatch or a re-plan (issue #1865), and
/// that clear used to be invisible to `patch_task`'s own response: the
/// handler serialized its local `record` — built before the upsert — instead
/// of what `upsert_task` returned. A client such as `TaskEditDialog` that
/// reconciles its board state straight from the PATCH body would keep
/// showing a bounce reason for a card whose stored row had already cleared
/// it (Codex review, PR #1883).
#[cfg(test)]
mod patch_clears_bounced_test {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn manifest() -> CompanyManifest {
        toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .unwrap()
    }

    async fn state(home: &std::path::Path) -> AppState {
        use crate::ports::CompanyStore;
        let id = CompanyId::new("acme");
        FsCompanyStore::new(home.to_path_buf())
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    /// Writes a bounced To-do card straight through the store, bypassing
    /// `upsert_task`'s dispatch/plan edges — the seed only needs the row to
    /// exist with a `bounced` chip already on it, not to fire either trigger.
    async fn seed_bounced_card(state: &AppState, id: &str) {
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).unwrap();
        runtime
            .tasks()
            .upsert(
                &company,
                &crate::ports::tasks::TaskRecord {
                    id: id.to_string(),
                    title: "Draft the launch note".to_string(),
                    note: None,
                    column: crate::ports::tasks::COLUMN_TODO.to_string(),
                    priority: "medium".to_string(),
                    assignee: String::new(),
                    updated_at_millis: 1,
                    origin_chat_id: None,
                    parent_task_id: None,
                    output: None,
                    plan: None,
                    planning_attempts: Vec::new(),
                    deliverable: crate::ports::tasks::TaskDeliverable::Once,
                    workflow_proposal: None,
                    origin_run_id: None,
                    origin_workflow_id: None,
                    bounced: Some("a previous run's dispatch failed".to_string()),
                },
            )
            .await
            .unwrap();
    }

    async fn patch_column(state: &AppState, id: &str, column: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/company/tasks/{id}"))
            .header("content-type", "application/json")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::from(json!({"column": column}).to_string()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        (status, value)
    }

    #[tokio::test]
    async fn patching_a_bounced_card_straight_to_done_returns_the_cleared_state() {
        let home = tempfile::tempdir().unwrap();
        let state = state(home.path()).await;
        seed_bounced_card(&state, "card-1").await;

        let (status, body) = patch_column(&state, "card-1", "done").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.get("bounced").is_none(),
            "the PATCH response still carries the stale bounce chip: {body}"
        );

        // The response is not just accidentally right while the persisted row
        // stays wrong — the store must agree too.
        let company = CompanyId::new("acme");
        let runtime = state.registry().get(&company).unwrap();
        let stored = runtime
            .tasks()
            .list(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "card-1")
            .unwrap();
        assert!(
            stored.bounced.is_none(),
            "the stored row should also have cleared the chip"
        );
    }
}
