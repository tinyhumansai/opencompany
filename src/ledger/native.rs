//! Reading a runtime-owned ledger through the ledger surface.
//!
//! The task board keeps its own store, its own routes and its own dispatch
//! edge — see [`registry::builtins`](super::registry::builtins) for why. What
//! it does not get to keep is a second way of being read: a discovery surface
//! that names every ledger and then cannot open the one the company has had
//! since day one is a surface an agent stops trusting.
//!
//! So this projects [`TaskRecord`]s into the same [`Entry`] the engine folds an
//! event log into. One renderer, one index, one read shape, whichever side a
//! ledger's rows actually live on.

use std::collections::BTreeMap;

use super::engine::{Entries, Entry};
use super::types::{AuthorKind, LedgerAuthor};
use crate::ports::tasks::TaskRecord;

/// Projects the board into ledger entries, in the order the store returned
/// them.
///
/// `touched` is the **reverse** index rather than the position, because the
/// board's list is most-recently-updated first while the engine's `Recent`
/// order sorts descending on `touched`. Getting that backwards would render the
/// stalest card at the top of a section whose blurb says most recent first,
/// which is precisely the failure the ordering exists to fix.
pub fn entries_from_tasks(tasks: &[TaskRecord]) -> Entries {
    let total = tasks.len();
    let entries = tasks
        .iter()
        .enumerate()
        .map(|(position, task)| {
            let mut fields: BTreeMap<String, String> = BTreeMap::new();
            fields.insert("title".to_string(), task.title.clone());
            // The row's **status** is the phase, because that is the ledger's
            // declared vocabulary and the console's column list. The stage
            // rides alongside as prose, so a reader still learns that a working
            // card is specifically waiting on their verdict — without the board
            // growing a fourth pile to put it in. See `super::board`.
            fields.insert(
                "column".to_string(),
                super::board::phase_of(&task.column).to_string(),
            );
            if let Some(column) = super::board::column(&task.column)
                && column.phase == super::board::PHASE_WORKING
            {
                fields.insert("stage".to_string(), column.label.to_string());
            }
            fields.insert("priority".to_string(), task.priority.clone());
            if !task.assignee.trim().is_empty() {
                fields.insert("assignee".to_string(), task.assignee.clone());
            }
            if let Some(note) = task.note.as_ref().filter(|note| !note.trim().is_empty()) {
                fields.insert("note".to_string(), note.clone());
            }
            // Lineage is provenance a reader asks about often enough to be
            // worth a line, and it costs nothing when absent.
            if let Some(parent) = &task.parent_task_id {
                fields.insert("parent".to_string(), parent.clone());
            }
            let author = if task.assignee.trim().is_empty() {
                LedgerAuthor {
                    kind: AuthorKind::System,
                    id: "board".to_string(),
                    label: "the board".to_string(),
                }
            } else {
                LedgerAuthor::agent(task.assignee.clone())
            };
            Entry {
                id: task.id.clone(),
                fields,
                opened_by: author.clone(),
                updated_by: author,
                opened_at_millis: task.updated_at_millis,
                updated_at_millis: task.updated_at_millis,
                // A card's history is on the card, not in an event count. One
                // is the honest answer: this projection saw it once.
                events: 1,
                touched: total.saturating_sub(position + 1),
            }
        })
        .collect();
    Entries {
        entries,
        faults: Vec::new(),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ledger::registry::Registry;
    use crate::ports::tasks::{
        COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_TODO,
        TaskDeliverable,
    };

    fn card(id: &str, column: &str, updated: u64) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: format!("card {id}"),
            note: None,
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee: "ops".to_string(),
            updated_at_millis: updated,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    #[test]
    fn a_card_projects_onto_the_declared_field_names() {
        let registry = Registry::build([]);
        let spec = registry.find("tasks").expect("tasks is built in");
        let entries = entries_from_tasks(&[card("t1", COLUMN_IN_PROGRESS, 10)]);
        let entry = entries.find("t1").expect("the card projects");
        assert_eq!(entry.title(spec), "card t1");
        assert_eq!(entry.status(spec), crate::ledger::board::PHASE_WORKING);
        assert_eq!(entry.get("assignee"), "ops");
    }

    /// The four working stages project onto one status, and each says which
    /// one it is on the row rather than in the pile it landed in (issue #1512).
    #[test]
    fn every_working_stage_reads_as_one_status_and_keeps_its_own_name() {
        use crate::ledger::board::PHASE_WORKING;
        use crate::ports::tasks::COLUMN_PLANNING;

        let registry = Registry::build([]);
        let spec = registry.find("tasks").expect("tasks is built in");
        let entries = entries_from_tasks(&[
            card("planning", COLUMN_PLANNING, 40),
            card("running", COLUMN_IN_PROGRESS, 30),
            card("parked", crate::ports::tasks::COLUMN_PAUSED, 20),
            card("waiting", crate::ports::tasks::COLUMN_IN_REVIEW, 10),
        ]);
        for id in ["planning", "running", "parked", "waiting"] {
            let entry = entries.find(id).expect("the card projects");
            assert_eq!(entry.status(spec), PHASE_WORKING, "{id}");
        }
        assert_eq!(entries.find("waiting").unwrap().get("stage"), "In review");
        assert_eq!(entries.find("planning").unwrap().get("stage"), "Planning");
    }

    /// Pending and done carry no stage: there is only one way to be either, so
    /// a line saying which would be a line saying nothing.
    #[test]
    fn a_pending_or_done_card_carries_no_stage_line() {
        let entries = entries_from_tasks(&[
            card("waiting", COLUMN_TODO, 20),
            card("shipped", COLUMN_DONE, 10),
        ]);
        assert_eq!(entries.find("waiting").unwrap().get("stage"), "");
        assert_eq!(entries.find("shipped").unwrap().get("stage"), "");
    }

    /// The board lists most-recently-updated first; `Recent` sorts descending
    /// on `touched`. The two must agree, or the section's blurb lies.
    #[test]
    fn the_boards_order_survives_the_projection() {
        let entries = entries_from_tasks(&[
            card("newest", COLUMN_TODO, 30),
            card("middle", COLUMN_TODO, 20),
            card("oldest", COLUMN_TODO, 10),
        ]);
        let ordered =
            super::super::engine::ordered(&entries.entries, super::super::spec::Order::Recent);
        let ids: Vec<&str> = ordered.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, ["newest", "middle", "oldest"]);
    }

    /// Done is the board's one closed phase, so it is the one that lands in an
    /// index's archive rather than in its open list.
    #[test]
    fn only_done_reads_as_closed() {
        let registry = Registry::build([]);
        let spec = registry.find("tasks").expect("tasks is built in");
        let entries = entries_from_tasks(&[
            card("a", COLUMN_DONE, 30),
            card("b", COLUMN_IN_PROGRESS, 20),
            card("c", COLUMN_TODO, 10),
        ]);
        assert_eq!(entries.closed_count(spec), 1);
        assert_eq!(entries.open_count(spec), 2);
    }

    /// The file an agent actually reads: three headings, and a working card that
    /// still says what it is waiting on (issue #1512).
    #[test]
    fn the_rendered_board_has_three_headings_and_keeps_the_stage_on_the_row() {
        let registry = Registry::build([]);
        let spec = registry.find("tasks").expect("tasks is built in");
        let entries = entries_from_tasks(&[
            card("a", COLUMN_TODO, 40),
            card("b", COLUMN_PAUSED, 30),
            card("c", COLUMN_IN_REVIEW, 20),
            card("d", COLUMN_DONE, 10),
        ]);
        let rendered = crate::ledger::engine::render(spec, &entries);
        let headings: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();
        assert_eq!(
            headings,
            ["## Pending", "## Working", "## Done"],
            "{rendered}"
        );
        assert!(rendered.contains("In review"), "{rendered}");
        assert!(rendered.contains("Paused"), "{rendered}");
    }
}
