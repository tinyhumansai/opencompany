//! The task-board read: `Company.tasks` over the [`TaskStore`] port.

use std::sync::Arc;

use async_graphql::{ID, SimpleObject};

use super::pagination::Page;
use crate::company::runtime::CompanyRuntime;
use crate::ports::tasks::TaskRecord;

/// One card on the company's task board. Mirrors [`TaskRecord`].
#[derive(SimpleObject)]
#[graphql(name = "Task")]
pub struct TaskGql {
    /// The task id.
    pub id: ID,
    /// The task title.
    pub title: String,
    /// An optional longer note.
    pub note: Option<String>,
    /// The board column: `backlog` | `in_progress` | `in_review` | `done`.
    pub column: String,
    /// The priority: `low` | `medium` | `high`.
    pub priority: String,
    /// The assigned teammate id.
    pub assignee: String,
}

impl From<TaskRecord> for TaskGql {
    fn from(record: TaskRecord) -> Self {
        Self {
            id: ID(record.id),
            title: record.title,
            note: record.note,
            column: record.column,
            priority: record.priority,
            assignee: record.assignee,
        }
    }
}

/// Resolves `Company.tasks(column, first, offset)`.
pub(crate) async fn resolve(
    runtime: &Arc<CompanyRuntime>,
    column: Option<String>,
    first: i32,
    offset: i32,
) -> async_graphql::Result<Page<TaskGql>> {
    let mut rows = runtime.tasks().list(runtime.id()).await?;
    if let Some(column) = column {
        rows.retain(|row| row.column == column);
    }
    let items: Vec<TaskGql> = rows.into_iter().map(TaskGql::from).collect();
    Ok(Page::slice(
        items,
        offset.max(0) as usize,
        first.max(0) as usize,
    ))
}
