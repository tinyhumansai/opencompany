//! The [`TaskStore`] port: the company's durable Kanban board.
//!
//! Tasks are the operator-visible work items the console's board renders (see
//! [`BOARD_COLUMNS`]). They are hand-curated state, not cycle working memory —
//! the brain's per-cycle task results live in
//! [`MemoryStore`](crate::ports::MemoryStore). Each record is keyed by a stable
//! id within the company.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

// ---------------------------------------------------------------------------
// The board's column vocabulary (issue #205)
// ---------------------------------------------------------------------------
//
// Transcribed once here — the port every writer already depends on — rather
// than in the harness lifecycle seam, which is `#[cfg(feature = "openhuman")]`
// and therefore invisible to the REST write boundary and to the default build.
// `crate::harness::lifecycle` re-exports these so its `COLUMN_*` paths are
// unchanged, and `CompanyRuntime`'s dispatch edge reads `COLUMN_IN_PROGRESS`
// from here, so each literal exists in exactly one place.

/// Everything not started (issue #301): work nobody has picked up yet **and**
/// work a failed, cancelled or revised run returned. This is the board's
/// manual-entry column — the `+` button lives here alone — and what
/// `POST …/tasks` defaults to.
///
/// Before #301 the returned half lived in a separate `backlog` pool (issue
/// #206). Epic #183 §3 collapsed the two: the split encoded *why* a card had
/// not started — never picked vs bounced back — but that is provenance, not
/// position, and every return path already stamps its reason onto the card's
/// note (`review_note`, the dispatch error, `[operator] cancelled while in
/// flight`), which the board renders. See [`LEGACY_COLUMN_BACKLOG`] for how
/// stored cards migrate.
pub const COLUMN_TODO: &str = "todo";
/// Between intake and dispatch: the card is being turned into a plan.
///
/// **Inert today, deliberately.** Nothing writes it automatically — epic #183
/// §4's auto-advance does, and that is blocked on #242/#243. The vocabulary
/// lands first so §4's code can write `planning` through a write boundary that
/// already accepts it; shipping the column later instead would leave
/// #242-dependent code writing a column the host rejects. An operator may drag
/// a card into it manually (any-column PATCH is today's contract) and nothing
/// happens, which is correct: planning does not dispatch.
pub const COLUMN_PLANNING: &str = "planning";
/// The dispatch column: entering it hands the card to its assignee.
pub const COLUMN_IN_PROGRESS: &str = "in_progress";
/// Where a steered-to-pause run parks. Resume is a `column → in_progress` PATCH.
pub const COLUMN_PAUSED: &str = "paused";
/// Where a finished board-created card waits for its operator reviewer.
pub const COLUMN_IN_REVIEW: &str = "in_review";
/// The terminal column — nothing dispatches out of it.
pub const COLUMN_DONE: &str = "done";

/// Every column the board renders, in board order — the host's half of the
/// console's `TASK_COLUMNS` (`frontend/src/lib/tasks-sample.ts`).
///
/// A card's `column` is a plain string on the wire, so before #205 a typo'd or
/// invented column was persisted verbatim and then simply never rendered: the
/// card vanished from the board with no error, and — since only the exact
/// literal `in_progress` edge-fires a dispatch — a typo'd `in-progress` also
/// silently never ran. This list is what the write boundary checks against.
///
/// Issue #301 reshaped this list to epic #183 §3: `backlog` was removed and
/// `planning` added. `todo` absorbed both of `backlog`'s jobs — the unqueued
/// pool and the lifecycle's return landing — so a card that cannot be planned
/// goes back to `todo` with the reason on its note rather than into a second
/// not-started column. `planning` is accepted but nothing writes it yet
/// (see [`COLUMN_PLANNING`]).
pub const BOARD_COLUMNS: [&str; 6] = [
    COLUMN_TODO,
    COLUMN_PLANNING,
    COLUMN_IN_PROGRESS,
    COLUMN_PAUSED,
    COLUMN_IN_REVIEW,
    COLUMN_DONE,
];

/// The column id issue #206 used for the unqueued pool, removed by #301.
///
/// Kept only as the migration's left-hand side. Cards persisted before #301
/// carry this literal, which [`is_board_column`] now rejects — so without a
/// migration they would fail to render and vanish from the board, the exact
/// silent disappearance #205 exists to prevent. [`TaskRecord::column`]
/// normalizes it to [`COLUMN_TODO`] on **read**, so every stored card heals
/// lazily at its next load and persists the new literal at its next upsert.
///
/// Reads heal; writes do not. A client still *sending* `"backlog"` gets a
/// `400` from the REST write boundary naming the valid set, because the REST
/// DTOs deserialize `column` as a plain `String` and validate it separately —
/// legacy data recovers silently, a dead-column write fails loudly.
pub const LEGACY_COLUMN_BACKLOG: &str = "backlog";

/// Whether `column` names a column the board actually renders.
pub fn is_board_column(column: &str) -> bool {
    BOARD_COLUMNS.contains(&column)
}

/// Rewrites a stored column literal that no longer names a board column.
///
/// Today that is exactly one mapping: [`LEGACY_COLUMN_BACKLOG`] →
/// [`COLUMN_TODO`] (issue #301). Applied on deserialization of
/// [`TaskRecord::column`], which is the single choke point every persistence
/// backend and the export/import path all funnel through — sqlite and mongodb
/// both store the record as a `task_json` string and the fs bundle as a JSON
/// array, so all three parse through this one `impl Deserialize`.
fn migrate_column(column: String) -> String {
    if column == LEGACY_COLUMN_BACKLOG {
        COLUMN_TODO.to_string()
    } else {
        column
    }
}

/// Serde shim applying [`migrate_column`] to a stored `column`.
fn deserialize_column<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(migrate_column(String::deserialize(deserializer)?))
}

/// One card on the company's task board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    /// Stable id for the task within the company.
    pub id: String,
    /// The task's title.
    pub title: String,
    /// An optional longer note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The board column — one of [`BOARD_COLUMNS`].
    ///
    /// Read through [`migrate_column`], so a card persisted before issue #301
    /// under the removed `backlog` id loads as [`COLUMN_TODO`] instead of as a
    /// column nothing renders.
    #[serde(deserialize_with = "deserialize_column")]
    pub column: String,
    /// The priority (`low`, `medium`, `high`).
    pub priority: String,
    /// Which desk/teammate owns it.
    pub assignee: String,
    /// Epoch-millis timestamp of the last update.
    pub updated_at_millis: u64,
    /// The chat/desk thread the task was created from, when it came from a
    /// delegation (issue #151 §3.2).
    ///
    /// A dispatched card runs asynchronously, long after the turn that spawned
    /// it has answered, so the completion reply has no ambient thread to post
    /// onto — without this it can only be written into `note`, where the
    /// operator has to go looking for it. Stamped by `spawn_task` from the
    /// delegating turn's chat id.
    ///
    /// `None` for a card created straight on the board (no originating
    /// conversation) and for every card written before this field existed —
    /// both simply get no post-back, exactly as today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_chat_id: Option<String>,
    /// The task whose dispatch turn spawned this card (issue #185) — the
    /// parent half of the Task Detail screen's lineage.
    ///
    /// An agent running a dispatched card can itself delegate, opening a new
    /// To-do card through the same `spawn_task` path. That makes the board a
    /// forest, but nothing recorded the edge: `origin_chat_id` names the
    /// *conversation* a card came from, which is shared by every sibling
    /// spawned in that thread and is `None` entirely for a board-native card.
    /// Lineage needs the task-to-task edge, so it gets its own field.
    ///
    /// Read back as: `parent` is the card with this id; `children` are the
    /// cards whose `parent_task_id` is this card's id.
    ///
    /// `None` for a card created straight on the board, for one delegated from
    /// an ordinary chat turn (no task in scope), and for every card written
    /// before this field existed — all three are lineage roots, exactly as
    /// today. Additive on the wire like [`Self::origin_chat_id`], so no stored
    /// board needs migrating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
}

/// Durable per-company task board. Company A's tasks MUST be invisible to
/// company B.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Lists every task, most-recently-updated first.
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>>;
    /// Inserts or replaces a task by id.
    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()>;
    /// Deletes a task by id; returns whether a task was removed.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

#[cfg(test)]
mod test {
    use super::*;

    /// Pins the **Rust** list's ids and their order against a literal, so a
    /// reorder or a rename is a deliberate two-place edit rather than a
    /// side effect.
    ///
    /// It does **not** protect against drift from the console's mirror in
    /// `frontend/src/lib/tasks-sample.ts` — a Rust test cannot see the TS list,
    /// so a column added on one side and not the other keeps this green.
    /// Closing that gap means generating one list from the other (a build step
    /// this crate does not have, across a separate npm build), so for now the
    /// mirror is maintained by hand and the two lists are reviewed together.
    #[test]
    fn columns_are_ordered_and_unique() {
        assert_eq!(
            BOARD_COLUMNS,
            [
                "todo",
                "planning",
                "in_progress",
                "paused",
                "in_review",
                "done"
            ]
        );
        let mut sorted = BOARD_COLUMNS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            BOARD_COLUMNS.len(),
            "column ids must be unique"
        );
    }

    #[test]
    fn is_board_column_accepts_only_board_columns() {
        for column in BOARD_COLUMNS {
            assert!(is_board_column(column), "{column} is a board column");
        }
        // Issue #301: To-do is the one not-started column, Planning is new, and
        // the old Backlog pool is gone — a client still writing it gets a 400
        // rather than a card the board cannot render.
        assert!(is_board_column(COLUMN_TODO));
        assert!(is_board_column(COLUMN_PLANNING));
        assert!(!is_board_column(LEGACY_COLUMN_BACKLOG));
        // Near-misses a typo'd client might send.
        assert!(!is_board_column("to_do"));
        assert!(!is_board_column("To-do"));
        assert!(!is_board_column("inprogress"));
        assert!(!is_board_column(""));
    }

    /// Issue #301's whole migration, at the seam every persistence backend
    /// shares. sqlite and mongodb store a card as a `task_json` string and the
    /// fs bundle as a JSON array, so all three — plus export/import — parse
    /// through this one `Deserialize`. A stored `backlog` card therefore heals
    /// on read instead of failing `is_board_column` and vanishing from the
    /// board.
    #[test]
    fn a_stored_backlog_card_reads_back_in_todo() {
        // A raw blob in exactly the shape a pre-#301 build persisted.
        let legacy = r#"{
            "id": "t-1",
            "title": "Bounced work",
            "note": "[operator] cancelled while in flight",
            "column": "backlog",
            "priority": "medium",
            "assignee": "maya",
            "updatedAtMillis": 7
        }"#;
        let migrated: TaskRecord = serde_json::from_str(legacy).expect("legacy card parses");
        assert_eq!(migrated.column, COLUMN_TODO);
        assert!(
            is_board_column(&migrated.column),
            "a migrated card must render on the board"
        );
        // The reason the collapse is lossless: the note survives untouched, so
        // "bounced back" is still readable on the card.
        assert_eq!(
            migrated.note.as_deref(),
            Some("[operator] cancelled while in flight")
        );

        // The next upsert persists the new literal — nothing re-writes it back.
        let round_tripped = serde_json::to_string(&migrated).unwrap();
        assert!(
            round_tripped.contains("\"column\":\"todo\""),
            "{round_tripped}"
        );

        // Migration is exactly one mapping; every live column is passed through
        // untouched, so a future column cannot be silently rewritten.
        for column in BOARD_COLUMNS {
            assert_eq!(migrate_column(column.to_string()), column);
        }
    }
}
