//! Task board writes: `POST /tasks`, `PATCH /tasks/{task_id}`,
//! `DELETE /tasks/{task_id}` under both scope forms.
//!
//! Bodies mirror the console's `TaskCard` (`frontend/src/lib/tasks-sample.ts`)
//! in camelCase; the `assignee` is a plain desk/teammate label. Writes land in
//! the [`TaskStore`](crate::ports::TaskStore).

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::steer::{InflightEntry, SteerAction, SteerError, cap_redirect};
use crate::error::OpenCompanyError;
use crate::ports::tasks::TaskRecord;
use crate::ports::types::CompanyEvent;
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the task route fragment.
pub fn router() -> Router<AppState> {
    scoped("/tasks", post(create_task).get(list_tasks))
        // The static `/tasks/inflight` GET is registered before the dynamic
        // `/tasks/{task_id}` so the operator strip's read never collides with a
        // card id (`{task_id}` carries no GET anyway).
        .merge(scoped("/tasks/inflight", get(list_inflight)))
        .merge(scoped(
            "/tasks/{task_id}",
            patch(patch_task).delete(delete_task),
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

async fn create_task(
    company: ScopedCompany,
    Json(body): Json<CreateTask>,
) -> Result<Json<TaskCard>, ApiError> {
    let record = TaskRecord {
        id: generate_id(),
        title: body.title,
        note: body.note,
        column: body.column.unwrap_or_else(|| "backlog".to_string()),
        priority: body.priority.unwrap_or_else(|| "medium".to_string()),
        assignee: body.assignee.unwrap_or_default(),
        updated_at_millis: now_millis(),
    };
    company.runtime.upsert_task(&record).await?;
    Ok(Json(record.into()))
}

async fn patch_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
    Json(body): Json<PatchTask>,
) -> Result<Json<TaskCard>, ApiError> {
    let mut record = company
        .runtime
        .tasks()
        .list(company.id())
        .await?
        .into_iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("task {task_id}")))?;
    if let Some(title) = body.title {
        record.title = title;
    }
    if let Some(note) = body.note {
        record.note = Some(note);
    }
    if let Some(column) = body.column {
        record.column = column;
    }
    if let Some(priority) = body.priority {
        record.priority = priority;
    }
    if let Some(assignee) = body.assignee {
        record.assignee = assignee;
    }
    record.updated_at_millis = now_millis();
    company.runtime.upsert_task(&record).await?;
    Ok(Json(record.into()))
}

async fn delete_task(
    company: ScopedCompany,
    Path(TaskPath { task_id }): Path<TaskPath>,
) -> Result<StatusCode, ApiError> {
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
