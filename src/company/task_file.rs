//! Seed board cards: `globals/tasks.toml` and `companies/<name>/tasks.toml`.
//!
//! A company already boots knowing what it tracks — `seed_ledgers` seeds its
//! axes and `seed_workspace` seeds its documents — and with nothing to do. The
//! To-do column is empty, so the setup work every company obviously has (write
//! the brief, wire the connections, set the flow this vertical runs on) exists
//! only if somebody types it in, and the agents that were supposed to do the
//! setting up have nothing to pick up.
//!
//! These files are that work, authored beside the company it belongs to and
//! seeded onto the board once, at first boot — see `runtime::builder`.
//!
//! # Why the board and not a ledger
//!
//! `tasks` is the one built-in that is [`LedgerSource::Native`]: its rows live
//! in the [`TaskStore`](crate::ports::TaskStore) and `ledgers::record` refuses
//! to write it, because entering a phase on the board fires real work. So a
//! seed card is a [`TaskRecord`], not a `LedgerEvent`.
//!
//! # Why a card cannot say which column it is in
//!
//! Moving a card into `in_progress` is the edge that **dispatches a run**, and
//! `planning` bills a planning pass (`company::runtime::upsert_task`). A seed
//! file that could name a column would be one paste away from every freshly
//! provisioned company spending inference on work nobody asked for. So there is
//! no `column` key: a seeded card is To-do by construction, and the seeder
//! writes through the plain store rather than the edge-firing path.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::error::{OpenCompanyError, Result};
use crate::ports::tasks::{COLUMN_TODO, TaskDeliverable, TaskRecord};

/// The bundle file holding one company's seed cards.
pub const TASKS_FILE: &str = "tasks.toml";

/// The priorities a card may be authored with, matching the board's own.
const PRIORITIES: [&str; 3] = ["low", "medium", "high"];

/// The longest a seed card's id may be, matching the ledger slug bound.
const MAX_ID_CHARS: usize = 48;

/// The on-disk shape of a `tasks.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskSeedFile {
    #[serde(default, rename = "task")]
    tasks: Vec<TaskSeed>,
}

/// One `[[task]]` entry: a card the company starts with.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSeed {
    /// Stable id, lowercase and dashed. Stable because it is what a bundle card
    /// overrides a baseline card by, and what `[globals].disable` names.
    pub id: String,
    /// What the card asks for, in one line.
    pub title: String,
    /// The longer note: what "done" looks like, and which ledger or document it
    /// should produce.
    #[serde(default)]
    pub note: Option<String>,
    /// `low` | `medium` | `high`. Defaults to `medium`.
    #[serde(default)]
    pub priority: Option<String>,
    /// The teammate or desk that owns it.
    ///
    /// Optional, and **empty by default** rather than resolved to a name here.
    /// Seeding writes straight to the store, below `resolve_assignee` — which
    /// lives at the REST boundary — so a typo'd id would persist unchecked and
    /// surface much later as a card that refuses to dispatch. Empty is a legal,
    /// meaningful value (unassigned, routed to the orchestrator), and it is the
    /// only honest default for a baseline card that ships to every vertical and
    /// cannot know any company's roster.
    #[serde(default)]
    pub assignee: Option<String>,
}

impl TaskSeed {
    /// Renders this seed as a board card, stamped `at_millis`.
    ///
    /// Always [`COLUMN_TODO`], and always a lineage root: a seeded card came
    /// from no conversation, no parent card and no workflow run, so every
    /// provenance field is `None` rather than invented.
    pub fn to_record(&self, at_millis: u64) -> TaskRecord {
        TaskRecord {
            id: self.id.clone(),
            title: self.title.clone(),
            note: self.note.clone(),
            column: COLUMN_TODO.to_string(),
            priority: self
                .priority
                .clone()
                .unwrap_or_else(|| "medium".to_string()),
            assignee: self.assignee.clone().unwrap_or_default(),
            updated_at_millis: at_millis,
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
}

/// Whether `dir` is a bundle carrying seed cards.
pub fn has_task_file(dir: &Path) -> bool {
    dir.join(TASKS_FILE).is_file()
}

/// Loads the cards a bundle seeds, from `<dir>/tasks.toml`.
///
/// A missing file is not a problem to report: most bundles seed no card of
/// their own and get the baseline, which is a complete answer.
///
/// # Errors
///
/// Returns [`OpenCompanyError::ManifestInvalid`] listing every problem in the
/// file. All-or-nothing, like [`super::ledger_file::load_dir_ledgers`]: a
/// bundle's cards are a short hand-authored list, and shipping a vertical
/// silently short the setup work it is about is the failure this exists to
/// prevent. The seeder downgrades this to a warning so a hand-edited bundle
/// still boots — see `runtime::builder::seed_tasks`.
pub fn load_dir_tasks(dir: &Path) -> Result<Vec<TaskSeed>> {
    let path = dir.join(TASKS_FILE);
    let src = match std::fs::read_to_string(&path) {
        Ok(src) => src,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(OpenCompanyError::DataRead { path, source }),
    };

    let (tasks, problems) = parse_tasks(TASKS_FILE, &src);
    if problems.is_empty() {
        Ok(tasks)
    } else {
        Err(OpenCompanyError::ManifestInvalid { path, problems })
    }
}

/// Parses one seed file, returning the cards that parsed alongside every
/// problem from the ones that did not.
///
/// [`load_dir_tasks`] turns this into an all-or-nothing [`Result`] for a
/// company's own bundle. The global baseline (`crate::globals`) wants the
/// opposite — a malformed baseline must not cost every company its cards — so
/// it calls this directly and keeps what parsed, exactly as it does for ledgers.
pub(crate) fn parse_tasks(file_name: &str, src: &str) -> (Vec<TaskSeed>, Vec<String>) {
    let file: TaskSeedFile = match toml::from_str(src) {
        Ok(file) => file,
        Err(err) => {
            return (
                Vec::new(),
                vec![format!(
                    "`{file_name}` is not valid TOML — {}",
                    err.message()
                )],
            );
        }
    };

    let mut kept: Vec<TaskSeed> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (index, mut task) in file.tasks.into_iter().enumerate() {
        let label = if task.id.trim().is_empty() {
            format!("`{file_name}` task #{}", index + 1)
        } else {
            format!("`{file_name}` task `{}`", task.id.trim())
        };

        match normalize_task_id(&task.id) {
            Ok(id) => task.id = id,
            Err(err) => {
                problems.push(format!("{label} {err}"));
                continue;
            }
        }

        if task.title.trim().is_empty() {
            problems.push(format!(
                "{label} has no `title` — a card nobody can read is a card nobody picks up."
            ));
            continue;
        }
        task.title = task.title.trim().to_string();

        if let Some(priority) = task.priority.as_deref() {
            let priority = priority.trim().to_ascii_lowercase();
            if !PRIORITIES.contains(&priority.as_str()) {
                problems.push(format!(
                    "{label} has `priority = \"{priority}\"`, which the board does not render — \
                     use one of {}.",
                    PRIORITIES.join(", ")
                ));
                continue;
            }
            task.priority = Some(priority);
        }

        // A duplicate id would put two cards on the board claiming one identity,
        // and which one survived would depend on write order rather than on this
        // file. It is also what a bundle card overrides a baseline card by, so an
        // id that is not unique makes that precedence unresolvable.
        if !seen.insert(task.id.clone()) {
            problems.push(format!(
                "{label} repeats an `id` used earlier in the file — ids must be unique."
            ));
            continue;
        }

        kept.push(task);
    }

    (kept, problems)
}

/// Holds a seed card's id to the same lowercase-and-dashed rule every other
/// name the runtime puts on a surface follows.
fn normalize_task_id(raw: &str) -> std::result::Result<String, String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Err(
            "has no `id` — a seed card needs one so a bundle can override it and \
                    `[globals].disable` can name it."
                .to_string(),
        );
    }
    if value.chars().count() > MAX_ID_CHARS
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "has `id = \"{value}\"`, which is not a usable id — use lowercase letters, digits and \
             hyphens, up to {MAX_ID_CHARS} characters, not starting or ending with a hyphen."
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod test {
    use super::*;

    fn parse(src: &str) -> (Vec<TaskSeed>, Vec<String>) {
        parse_tasks(TASKS_FILE, src)
    }

    #[test]
    fn reads_a_card_and_defaults_what_an_author_left_out() {
        let (tasks, problems) = parse(
            r#"
            [[task]]
            id = "set-up-the-book-flow"
            title = "Set up the book flow"
            "#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(tasks.len(), 1);

        let card = tasks[0].to_record(1_000);
        assert_eq!(card.id, "set-up-the-book-flow");
        assert_eq!(card.priority, "medium");
        // Unassigned, not a guessed name — the seeder is below the resolver.
        assert_eq!(card.assignee, "");
        assert_eq!(card.updated_at_millis, 1_000);
    }

    #[test]
    fn a_seeded_card_is_always_todo_and_carries_no_invented_provenance() {
        let (tasks, _) = parse(
            r#"
            [[task]]
            id = "a"
            title = "A"
            "#,
        );
        let card = tasks[0].to_record(0);
        // The whole safety property: nothing seeded can enter the dispatching
        // or the billing column.
        assert_eq!(card.column, COLUMN_TODO);
        assert!(card.origin_chat_id.is_none());
        assert!(card.parent_task_id.is_none());
        assert!(card.origin_run_id.is_none());
        assert!(card.origin_workflow_id.is_none());
        assert!(card.plan.is_none());
        assert!(card.output.is_none());
    }

    #[test]
    fn a_column_key_is_refused_rather_than_honoured() {
        // `deny_unknown_fields` is what enforces "a card cannot name a column".
        let (tasks, problems) = parse(
            r#"
            [[task]]
            id = "a"
            title = "A"
            column = "in_progress"
            "#,
        );
        assert!(tasks.is_empty());
        assert!(!problems.is_empty(), "a column key must be refused");
    }

    #[test]
    fn refuses_a_duplicate_id() {
        let (tasks, problems) = parse(
            r#"
            [[task]]
            id = "a"
            title = "First"

            [[task]]
            id = "a"
            title = "Second"
            "#,
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "First");
        assert!(
            problems.iter().any(|p| p.contains("repeats an `id`")),
            "{problems:?}"
        );
    }

    #[test]
    fn refuses_an_unrenderable_priority() {
        let (tasks, problems) = parse(
            r#"
            [[task]]
            id = "a"
            title = "A"
            priority = "urgent"
            "#,
        );
        assert!(tasks.is_empty());
        assert!(
            problems.iter().any(|p| p.contains("priority")),
            "{problems:?}"
        );
    }

    #[test]
    fn refuses_a_title_that_says_nothing() {
        let (tasks, problems) = parse(
            r#"
            [[task]]
            id = "a"
            title = "   "
            "#,
        );
        assert!(tasks.is_empty());
        assert!(
            problems.iter().any(|p| p.contains("`title`")),
            "{problems:?}"
        );
    }

    #[test]
    fn refuses_an_id_that_is_not_lowercase_and_dashed() {
        for bad in ["Set_Up", "-leading", "trailing-", "has space"] {
            let (tasks, problems) = parse(&format!("[[task]]\nid = \"{bad}\"\ntitle = \"A\"\n"));
            assert!(tasks.is_empty(), "`{bad}` should be refused");
            assert!(!problems.is_empty(), "`{bad}` should report a problem");
        }
    }

    #[test]
    fn one_bad_card_does_not_cost_the_others() {
        let (tasks, problems) = parse(
            r#"
            [[task]]
            id = "good-one"
            title = "Good"

            [[task]]
            id = "bad one"
            title = "Bad"

            [[task]]
            id = "good-two"
            title = "Also good"
            "#,
        );
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["good-one", "good-two"]);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn a_malformed_file_is_one_problem_and_no_cards() {
        let (tasks, problems) = parse("[[task]\nid = ");
        assert!(tasks.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("not valid TOML"), "{problems:?}");
    }

    #[test]
    fn a_missing_file_is_not_a_problem() {
        let dir = std::env::temp_dir().join("oc-task-file-absent");
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(!has_task_file(&dir));
        assert!(load_dir_tasks(&dir).expect("absent is fine").is_empty());
    }
}
