//! The task-board read: `Company.tasks` over the [`TaskStore`] port.

use std::sync::Arc;

use async_graphql::{ID, SimpleObject};

use super::pagination::Page;
use crate::company::runtime::CompanyRuntime;
use crate::ports::tasks::TaskRecord;

/// One card on the company's task board. Mirrors [`TaskRecord`] — **partially,
/// and on purpose**.
///
/// This projection is deliberately narrower than the REST `TaskCard`: it has
/// never carried `parentTaskId`, `originChatId`, or the note-adjacent detail
/// the console's screens read, because the GraphQL surface answers *"what is on
/// the board"* rather than *"what happened to this card"*.
///
/// The output link (issue #339) is omitted for the same reason and is a real
/// gap, named rather than hidden: it is a **console link** — it resolves to
/// artifact-viewer and run-trace routes that only the operator console has —
/// so projecting it here would publish addresses no GraphQL consumer can
/// follow. A consumer that wants a card's deliverable should read the artifact
/// and run surfaces directly. Widen this the day a GraphQL client needs the
/// correlation itself rather than the link.
#[derive(SimpleObject)]
#[graphql(name = "Task")]
pub struct TaskGql {
    /// The task id.
    pub id: ID,
    /// The task title.
    pub title: String,
    /// An optional longer note.
    pub note: Option<String>,
    /// The board column: `todo` | `planning` | `in_progress` | `paused` |
    /// `in_review` | `done`.
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
