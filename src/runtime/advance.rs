//! The one guarded mover for the board's automatic edge (issue #337, epic
//! #183 §4).
//!
//! [`column_for_settled_run`] decides *where* a settled attempt's card belongs.
//! This module owns the far narrower question of *whether it is allowed to move
//! it*, and it is the only place outside `run_task`'s own settle that writes a
//! card's column off the back of a run.
//!
//! # Why the guard, and why it is code rather than intent
//!
//! Epic #337's acceptance criteria include one that reads as a prohibition
//! rather than a feature: *"a card in Paused or In Review is never moved
//! automatically — only the person, or the unblocking event, moves it."*
//!
//! Three system paths settle a run they did not dispatch — the cycle's
//! terminality backstop, [`CompanyRuntime::abandon_run`], and the boot reaper's
//! card sweep. Each of them can fire long after the card has moved on: an
//! operator can drag a card out of In Progress, an approval can park it in
//! Paused, a *later* attempt can land it in In Review, all while the row this
//! path is settling is still claiming to be live. A mover that trusted its
//! caller's idea of where the card was would happily yank a parked card back to
//! To-do and destroy real pending work.
//!
//! So [`advance_settled_card`] **re-reads the card** and refuses unless it is
//! still in [`COLUMN_IN_PROGRESS`]. The guard is a structural property of the
//! only function that can do the move, not a rule each of the three callers has
//! to remember.
//!
//! # Why it cannot re-fire dispatch
//!
//! The write goes through the plain [`TaskStore::upsert`] port, never
//! [`CompanyRuntime::upsert_task`]. Only the latter carries the
//! `task_enters_in_progress` edge, and every column this module writes is a
//! *departure* from In Progress in any case — but routing through the port is
//! what makes "settling a run cannot start another one" true by construction
//! rather than by inspection of the mapping.
//!
//! [`column_for_settled_run`]: crate::ports::tasks::column_for_settled_run
//! [`CompanyRuntime::abandon_run`]: crate::company::runtime::CompanyRuntime
//! [`CompanyRuntime::upsert_task`]: crate::company::runtime::CompanyRuntime

use crate::Result;
use crate::ports::TaskStore;
use crate::ports::now_millis;
use crate::ports::runs::RunStatus;
use crate::ports::tasks::{
    COLUMN_IN_PROGRESS, COLUMN_PLANNING, COLUMN_TODO, column_for_settled_run,
};
use crate::ports::types::CompanyId;

/// The note attribution used when the *runtime* settles a card, as opposed to
/// an agent that produced a result or an operator who stopped one.
///
/// Its own word rather than reusing the assignee's or `"operator"`: a card that
/// came back to To-do because the host died must not read as though a teammate
/// gave up or a person cancelled it.
pub const SYSTEM_ATTRIBUTION: &str = "system";

/// Why a card moved, as one note block: `[<who>] <what>`.
///
/// The card has no first-class `result` field, so every outcome — an agent's
/// reply, an operator redirect, a dispatch failure, and now a system settle —
/// lands as an attributed block appended below whatever the note already said.
/// Nothing is ever overwritten: the note is the card's history.
///
/// Ungated and shared, so the harness settle and the three system paths append
/// in one shape. A second copy of this two-line function is exactly how a card
/// ends up with two different-looking note formats depending on which path
/// touched it last.
pub fn append_result(prev: Option<&str>, attribution: &str, body: &str) -> String {
    let block = format!("[{attribution}] {body}");
    match prev.filter(|p| !p.is_empty()) {
        Some(p) => format!("{p}\n\n{block}"),
        None => block,
    }
}

/// Moves `task_id`'s card to wherever `status` lands it, carrying `reason` onto
/// the note — but **only** if the card is still in [`COLUMN_IN_PROGRESS`].
///
/// Returns the column it wrote, or `None` when nothing moved. `None` covers
/// four distinct no-ops, all of them correct and none of them an error:
///
/// * `status` is not settled ([`RunStatus::Pending`] / [`RunStatus::Running`]),
///   so there is no landing to write;
/// * the card is gone (deleted between dispatch and settle);
/// * the card has already left In Progress under its own steam — an operator
///   dragged it, a later attempt landed it, an approval parked it. **This is
///   the guard**, and it is what keeps a Paused or In Review card untouched by
///   a late settle;
/// * the card was never in In Progress to begin with.
///
/// Errors only on a store fault. Every caller treats that as best-effort and
/// logs it: the attempt row is already settled by the time this runs, so a
/// board write that cannot land must not undo it.
pub async fn advance_settled_card(
    tasks: &dyn TaskStore,
    company: &CompanyId,
    task_id: &str,
    status: RunStatus,
    reason: &str,
) -> Result<Option<&'static str>> {
    let Some(column) = column_for_settled_run(status) else {
        return Ok(None);
    };
    // Re-read rather than trusting a card the caller is holding: the whole
    // point of the guard is that the board may have moved since.
    let Some(mut card) = tasks
        .list(company)
        .await?
        .into_iter()
        .find(|t| t.id == task_id)
    else {
        return Ok(None);
    };
    if card.column != COLUMN_IN_PROGRESS {
        return Ok(None);
    }
    card.note = Some(append_result(
        card.note.as_deref(),
        SYSTEM_ATTRIBUTION,
        reason,
    ));
    card.column = column.to_string();
    card.updated_at_millis = now_millis();
    tasks.upsert(company, &card).await?;
    Ok(Some(column))
}

/// The note a card gets when a planning pass was interrupted by the host going
/// away underneath it (issue #337).
///
/// Its own wording rather than the orphan-run one: nothing *ran*, so "an
/// attempt was abandoned" would be false. What happened is smaller and the
/// operator's recovery is a single drag.
pub const PLANNING_INTERRUPTED: &str = "the host restarted during planning, so the pass never finished — drag the card back into \
     Planning to try again";

/// Returns every card found sitting in [`COLUMN_PLANNING`] to
/// [`COLUMN_TODO`], carrying [`PLANNING_INTERRUPTED`] on its note. Returns the
/// ids it moved.
///
/// # Why a planning pass needs its own sweep
///
/// The orphan-run reaper cannot see this. A pass mints **no**
/// [`RunRecord`](crate::ports::runs::RunRecord) — deliberately: there is no
/// agent turn, no tool loop and nothing to steer, so an attempt row would be a
/// fiction (see `docs/spec/runtime/planning.md`). But that is exactly what
/// makes the crash case invisible: a host that dies mid-pass leaves a card in
/// Planning with nothing anywhere claiming to be working it, and because the
/// trigger is the *transition* into the column — which already happened —
/// nothing will ever re-drive it. The card would sit there forever looking
/// busy.
///
/// So the boot sweep reads the board directly. It is safe for the same reason
/// the run reaper is and for one more of its own:
///
///  * **Boot-only.** Nothing from this process can be in flight at boot, so
///    every Planning card provably belongs to a dead process. Like the run
///    reaper, this must NOT run on a rebuild ([`RuntimeRebuilder`]), where that
///    premise is false and a live pass would be yanked out from under itself.
///  * **Planning is transient by construction.** Every terminating path of a
///    pass leaves the column — to In Progress on success, to To-do otherwise.
///    A card resting in Planning is therefore never a state an operator chose
///    and never a state a healthy pass leaves behind, which is what makes
///    "found here at boot ⇒ interrupted" a sound inference rather than a guess.
///
/// It writes through the plain [`TaskStore::upsert`] port, never
/// [`CompanyRuntime::upsert_task`], so returning a card cannot fire the
/// planning edge again and put the company straight back into the pass that was
/// just interrupted.
///
/// Best-effort per card: one card that will not move must not stop the rest and
/// must not fail boot.
///
/// [`RuntimeRebuilder`]: crate::runtime::rebuild::RuntimeRebuilder
/// [`CompanyRuntime::upsert_task`]: crate::company::runtime::CompanyRuntime
pub async fn sweep_stranded_planning(
    tasks: &dyn TaskStore,
    company: &CompanyId,
) -> Result<Vec<String>> {
    let stranded: Vec<_> = tasks
        .list(company)
        .await?
        .into_iter()
        .filter(|t| t.column == COLUMN_PLANNING)
        .collect();
    let mut returned = Vec::with_capacity(stranded.len());
    for mut card in stranded {
        card.note = Some(append_result(
            card.note.as_deref(),
            SYSTEM_ATTRIBUTION,
            PLANNING_INTERRUPTED,
        ));
        card.column = COLUMN_TODO.to_string();
        card.updated_at_millis = now_millis();
        match tasks.upsert(company, &card).await {
            Ok(()) => returned.push(card.id),
            Err(err) => tracing::warn!(
                company = %company,
                task = %card.id,
                error = %err,
                "[planning] could not return a card stranded in Planning by a previous host \
                 process; it stays there until the next boot"
            ),
        }
    }
    Ok(returned)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::tasks::{COLUMN_IN_REVIEW, COLUMN_PAUSED, TaskRecord};
    use crate::store::FsOps;
    use std::sync::Arc;

    fn card(id: &str, column: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: "Draft the spec".to_string(),
            note: None,
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee: "maya".to_string(),
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
        }
    }

    async fn store() -> (tempfile::TempDir, Arc<dyn TaskStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(FsOps::new(dir.path()));
        (dir, store)
    }

    async fn column_of(tasks: &Arc<dyn TaskStore>, company: &CompanyId, id: &str) -> String {
        tasks
            .list(company)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .expect("card exists")
            .column
    }

    /// The ordinary move: a card being worked, whose attempt failed, comes back
    /// to To-do carrying the reason a person can read.
    #[tokio::test]
    async fn a_card_in_progress_moves_and_carries_the_reason() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", COLUMN_IN_PROGRESS))
            .await
            .unwrap();

        let moved = advance_settled_card(
            tasks.as_ref(),
            &company,
            "t-1",
            RunStatus::Failed,
            "the host restarted",
        )
        .await
        .unwrap();

        assert_eq!(moved, Some(COLUMN_TODO));
        let after = tasks
            .list(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .unwrap();
        assert_eq!(after.column, COLUMN_TODO);
        assert_eq!(
            after.note.as_deref(),
            Some("[system] the host restarted"),
            "the reason must be readable on the card"
        );
    }

    /// The guard, which is the whole reason this function exists: a card an
    /// operator (or a later attempt, or an approval) has already moved on is
    /// **never** yanked back by a late settle.
    #[tokio::test]
    async fn a_card_outside_in_progress_is_never_moved() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        for (id, column) in [
            ("t-paused", COLUMN_PAUSED),
            ("t-review", COLUMN_IN_REVIEW),
            ("t-todo", COLUMN_TODO),
        ] {
            tasks.upsert(&company, &card(id, column)).await.unwrap();
        }

        // Try every settled status against every parked column — none may move.
        for status in [
            RunStatus::Succeeded,
            RunStatus::WaitingApproval,
            RunStatus::Paused,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            for (id, column) in [
                ("t-paused", COLUMN_PAUSED),
                ("t-review", COLUMN_IN_REVIEW),
                ("t-todo", COLUMN_TODO),
            ] {
                let moved =
                    advance_settled_card(tasks.as_ref(), &company, id, status, "late settle")
                        .await
                        .unwrap();
                assert_eq!(moved, None, "{status} moved {id} out of {column}");
                assert_eq!(column_of(&tasks, &company, id).await, column);
            }
        }

        // And nothing scribbled on their notes either.
        for id in ["t-paused", "t-review", "t-todo"] {
            let note = tasks
                .list(&company)
                .await
                .unwrap()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
                .note;
            assert_eq!(note, None, "{id} was annotated by a refused move");
        }
    }

    /// An unsettled status is not a landing. A run still claiming to be live
    /// must leave its card exactly where it is.
    #[tokio::test]
    async fn an_unsettled_status_moves_nothing() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", COLUMN_IN_PROGRESS))
            .await
            .unwrap();

        for status in [RunStatus::Pending, RunStatus::Running] {
            assert_eq!(
                advance_settled_card(tasks.as_ref(), &company, "t-1", status, "still going")
                    .await
                    .unwrap(),
                None
            );
        }
        assert_eq!(column_of(&tasks, &company, "t-1").await, COLUMN_IN_PROGRESS);
    }

    /// A card deleted between dispatch and settle is a no-op, not an error —
    /// the attempt row is already settled and there is nothing left to annotate.
    #[tokio::test]
    async fn a_vanished_card_is_a_quiet_no_op() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        assert_eq!(
            advance_settled_card(
                tasks.as_ref(),
                &company,
                "t-gone",
                RunStatus::Failed,
                "orphaned",
            )
            .await
            .unwrap(),
            None
        );
    }

    /// A success does not skip the review stop, even on the system paths. The
    /// board's automatic edge has no route to Done at all.
    #[tokio::test]
    async fn a_succeeded_settle_stops_in_review() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", COLUMN_IN_PROGRESS))
            .await
            .unwrap();

        let moved = advance_settled_card(
            tasks.as_ref(),
            &company,
            "t-1",
            RunStatus::Succeeded,
            "settled elsewhere",
        )
        .await
        .unwrap();
        assert_eq!(moved, Some(COLUMN_IN_REVIEW));
    }

    // --- The planning boot sweep (issue #337) -------------------------------

    /// The crash case. A card left in Planning by a dead process comes back to
    /// To-do saying what happened — because nothing else can recover it: a pass
    /// mints no run row, so the orphan reaper never sees it, and the trigger is
    /// the transition into the column, which already happened.
    #[tokio::test]
    async fn a_card_stranded_in_planning_comes_back_to_todo() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-1", COLUMN_PLANNING))
            .await
            .unwrap();

        let returned = sweep_stranded_planning(tasks.as_ref(), &company)
            .await
            .unwrap();
        assert_eq!(returned, vec!["t-1".to_string()]);

        let after = tasks
            .list(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-1")
            .unwrap();
        assert_eq!(after.column, COLUMN_TODO);
        let note = after.note.expect("the reason is on the card");
        assert!(note.starts_with("[system] "), "{note}");
        assert!(
            note.contains("host restarted during planning"),
            "an operator must be able to tell this from a plan that failed: {note}"
        );
        // Idempotent: a second boot finds nothing left to move.
        assert!(
            sweep_stranded_planning(tasks.as_ref(), &company)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The sweep is scoped to Planning and nothing else. A board full of cards
    /// in every other column is untouched — this must never become a
    /// general-purpose "reset the board" pass.
    #[tokio::test]
    async fn the_planning_sweep_touches_no_other_column() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        let others = [
            ("t-todo", COLUMN_TODO),
            ("t-progress", COLUMN_IN_PROGRESS),
            ("t-paused", COLUMN_PAUSED),
            ("t-review", COLUMN_IN_REVIEW),
            ("t-done", crate::ports::tasks::COLUMN_DONE),
        ];
        for (id, column) in others {
            tasks.upsert(&company, &card(id, column)).await.unwrap();
        }
        tasks
            .upsert(&company, &card("t-planning", COLUMN_PLANNING))
            .await
            .unwrap();

        let returned = sweep_stranded_planning(tasks.as_ref(), &company)
            .await
            .unwrap();
        assert_eq!(returned, vec!["t-planning".to_string()]);

        for (id, column) in others {
            assert_eq!(column_of(&tasks, &company, id).await, column, "{id} moved");
            let note = tasks
                .list(&company)
                .await
                .unwrap()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
                .note;
            assert_eq!(note, None, "{id} was annotated by a sweep that skipped it");
        }
    }

    /// Per-company, like every other sweep. One tenant's interrupted pass must
    /// not move another tenant's card.
    #[tokio::test]
    async fn the_planning_sweep_is_scoped_to_one_company() {
        let (_dir, tasks) = store().await;
        let alpha = CompanyId::new("alpha");
        let beta = CompanyId::new("beta");
        tasks
            .upsert(&alpha, &card("a-1", COLUMN_PLANNING))
            .await
            .unwrap();
        tasks
            .upsert(&beta, &card("b-1", COLUMN_PLANNING))
            .await
            .unwrap();

        assert_eq!(
            sweep_stranded_planning(tasks.as_ref(), &alpha)
                .await
                .unwrap(),
            vec!["a-1".to_string()]
        );
        assert_eq!(column_of(&tasks, &beta, "b-1").await, COLUMN_PLANNING);
    }

    /// The note is append-only: a second block never eats the first.
    #[test]
    fn append_result_keeps_what_the_note_already_said() {
        assert_eq!(append_result(None, "system", "gone"), "[system] gone");
        assert_eq!(
            append_result(Some("[maya] draft"), "system", "gone"),
            "[maya] draft\n\n[system] gone"
        );
        // An empty prior note must not leave a leading blank block.
        assert_eq!(append_result(Some(""), "system", "gone"), "[system] gone");
    }
}
