//! Task board writes: `POST /tasks`, `PATCH /tasks/{task_id}`,
//! `DELETE /tasks/{task_id}` under both scope forms.
//!
//! Bodies mirror the console's `TaskCard` (`frontend/src/lib/tasks-sample.ts`)
//! in camelCase; the `assignee` is a plain desk/teammate label. Writes land in
//! the [`TaskStore`](crate::ports::TaskStore).

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::tasks::TaskRecord;
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the task route fragment.
pub fn router() -> Router<AppState> {
    scoped("/tasks", post(create_task).get(list_tasks)).merge(scoped(
        "/tasks/{task_id}",
        patch(patch_task).delete(delete_task),
    ))
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
    rows.sort_by(|a, b| b.updated_at_millis.cmp(&a.updated_at_millis));
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
