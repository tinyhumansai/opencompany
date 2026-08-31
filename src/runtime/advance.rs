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
use crate::ports::notifications::{Notification, NotificationStore, Subject, SubjectKind};
use crate::ports::now_millis;
use crate::ports::runs::RunStatus;
use crate::ports::tasks::{
    COLUMN_IN_PROGRESS, COLUMN_PAUSED, COLUMN_PLANNING, COLUMN_TODO, column_for_settled_run,
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
    // Issue #1865: the board's bounce chip, set on the exact same landing this
    // function already computed and cleared on any other one. `column` is
    // `column_for_settled_run`'s answer, so this cannot drift from the write
    // above into a second, independent reading of "did this bounce".
    card.bounced = bounced_reason(column, status, reason);
    card.updated_at_millis = now_millis();
    tasks.upsert(company, &card).await?;
    Ok(Some(column))
}

/// Returns a card whose blocker expired unanswered to [`COLUMN_TODO`],
/// carrying the question nobody answered (issue #1861). `true` when the card
/// moved.
///
/// The approval TTL's default-deny reaching the board. A blocker parks the card
/// in `paused` and asks; if nothing answers before the deadline the question is
/// retired, and this is what stops the card sitting in `paused` forever waiting
/// on a decision that has already been made against it. Epic #183's rule
/// applies again at that point: a card that cannot proceed goes back to To-do
/// carrying its reason, never into a stuck column of its own.
///
/// # Why the question is preserved rather than dropped
///
/// The unanswered question is the single most useful thing on the card. It is
/// what a person needs in order to unblock the work whenever they next look —
/// the TTL expiring does not make the work possible, it only stops pretending
/// somebody is about to answer.
///
/// # The guard, and why it differs from [`advance_settled_card`]'s
///
/// That function guards on [`COLUMN_IN_PROGRESS`] because it settles a run that
/// was running. This one guards on [`COLUMN_PAUSED`], the column the blocker
/// itself put the card in. Same principle either way: a card an operator has
/// since dragged somewhere is theirs, and an expiry must not drag it back.
///
/// # Why the chip is set here rather than through [`bounced_reason`]
///
/// `bounced_reason` answers "did this *settle* bounce", from a
/// [`RunStatus`] — and there is no run settling here. The attempt this blocker
/// came from ended long ago; what expired is a park. The chip is still exactly
/// right for the board's purpose (#1865): this card is not fresh, and an
/// operator scanning To-do must be able to see that without opening it.
pub async fn return_expired_blocker_card(
    tasks: &dyn TaskStore,
    company: &CompanyId,
    task_id: &str,
    question: &str,
) -> Result<bool> {
    let Some(mut card) = tasks
        .list(company)
        .await?
        .into_iter()
        .find(|t| t.id == task_id)
    else {
        return Ok(false);
    };
    if card.column != COLUMN_PAUSED {
        return Ok(false);
    }
    let reason = format!("{EXPIRED_BLOCKER}: {question}");
    card.note = Some(append_result(
        card.note.as_deref(),
        SYSTEM_ATTRIBUTION,
        &reason,
    ));
    card.column = COLUMN_TODO.to_string();
    card.bounced = Some(reason);
    card.updated_at_millis = now_millis();
    tasks.upsert(company, &card).await?;
    Ok(true)
}

/// The lead-in on a card returned by an unanswered blocker (issue #1861).
///
/// Its own wording rather than the failure one: nothing failed. The work is
/// exactly as possible as it was, and the only thing that changed is that
/// nobody answered in time.
pub const EXPIRED_BLOCKER: &str =
    "nobody answered this in time, so it is back in To-do — it still needs";

/// Whether a settle lands a card back on [`COLUMN_TODO`] because the attempt
/// **failed or was cancelled**, as opposed to any other landing this function
/// writes (issue #1865).
///
/// The single rule both card-write sites (this module's system mover, and
/// `run_task`'s own rich settle in `harness::built_in::brain`) apply, so
/// "which card gets the bounce chip" cannot answer differently depending on
/// which of the two paths happened to settle a given run.
///
/// `WaitingApproval`/`Paused` also land on a column other than
/// [`COLUMN_IN_PROGRESS`] but never on `COLUMN_TODO` — see
/// [`column_for_settled_run`] — so checking the column alone already excludes
/// them; the status check on top is what tells a genuine failure apart from
/// the one other status [`column_for_settled_run`] maps to `COLUMN_TODO`... in
/// practice there is none today, but the explicit check keeps this correct by
/// construction rather than by the current shape of that mapping.
pub fn bounced_reason(column: &str, status: RunStatus, reason: &str) -> Option<String> {
    (column == COLUMN_TODO && matches!(status, RunStatus::Failed | RunStatus::Cancelled))
        .then(|| reason.to_string())
}

/// Files the durable "a board card's dispatch failed and bounced back to
/// To-do" notification (issue #1865).
///
/// Shared by every system path that settles a run its own turn did not —
/// [`CompanyRuntime::abandon_run`](crate::company::runtime::CompanyRuntime::abandon_run),
/// the cycle's terminality backstop, and the boot reaper's card sweep in
/// [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build) — so a
/// crash-recovered dispatch failure is announced exactly like a live one
/// instead of only picking up the bounce chip silently. Call this only after
/// [`advance_settled_card`] actually reports the card landed on
/// [`COLUMN_TODO`]; a run that settled without moving the card raises nothing.
///
/// Whole-company audience: a bounced card has no single decider the way a
/// mention does, and its assignee is exactly who the card's own `assignee`
/// field already names for anyone who opens it.
///
/// Best-effort and logged, never propagated: the dispatch has already failed,
/// and a bookkeeping write cannot make that better or worse.
pub async fn notify_dispatch_failed(
    notifications: &dyn NotificationStore,
    company: &CompanyId,
    task_id: &str,
    reason: &str,
) {
    // `Notification.title` is documented as one line. `reason` is a free-form
    // failure text (an error's `Display`, in practice) and is not guaranteed
    // not to carry `\r`/`\n`, so normalize before interpolating — otherwise a
    // multiline reason persists a multiline title.
    let one_line_reason = reason.replace(['\r', '\n'], " ");
    let note = Notification {
        id: crate::ports::generate_id(),
        kind: "dispatch_failed".to_string(),
        subject: Subject {
            kind: SubjectKind::Task,
            id: task_id.to_string(),
        },
        created_at: now_millis(),
        title: format!("A card's dispatch failed and returned to To-do: {one_line_reason}"),
        audience: None,
        context: None,
    };
    if let Err(err) = notifications.append(company, &note).await {
        tracing::warn!(
            company = %company,
            task = %task_id,
            error = %err,
            "[runs] a dispatch-failure notification could not be recorded; the card still \
             bounced, but nobody is badged for it"
        );
    }
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
            bounced: None,
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
        // Issue #1865: the board's bounce chip, set on the same landing.
        assert_eq!(
            after.bounced.as_deref(),
            Some("the host restarted"),
            "a card failed back to To-do must carry the bounce reason"
        );
    }

    /// A landing other than To-do — the far more common case — must never set
    /// the bounce chip. `WaitingApproval` lands on Paused, which this test
    /// exercises as the representative non-bounce settle.
    #[tokio::test]
    async fn a_non_todo_landing_never_sets_bounced() {
        let (_dir, tasks) = store().await;
        let company = CompanyId::new("acme");
        tasks
            .upsert(&company, &card("t-2", COLUMN_IN_PROGRESS))
            .await
            .unwrap();

        advance_settled_card(
            tasks.as_ref(),
            &company,
            "t-2",
            RunStatus::WaitingApproval,
            "parked a gate",
        )
        .await
        .unwrap();

        let after = tasks
            .list(&company)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == "t-2")
            .unwrap();
        assert_eq!(after.column, COLUMN_PAUSED);
        assert_eq!(
            after.bounced, None,
            "a card parked for approval did not bounce"
        );
    }

    // ── Issue #1865: `bounced_reason`, the pure rule both card-write sites share ──

    #[test]
    fn bounced_reason_fires_on_failed_landing_on_todo() {
        assert_eq!(
            bounced_reason(COLUMN_TODO, RunStatus::Failed, "boom"),
            Some("boom".to_string())
        );
    }

    #[test]
    fn bounced_reason_fires_on_cancelled_landing_on_todo() {
        assert_eq!(
            bounced_reason(COLUMN_TODO, RunStatus::Cancelled, "stopped"),
            Some("stopped".to_string())
        );
    }

    #[test]
    fn bounced_reason_is_none_off_todo_even_for_a_failure() {
        // Structurally `column_for_settled_run` never actually pairs `Failed`
        // with anything but `COLUMN_TODO` — this pins the function's OWN
        // contract regardless, so it stays correct if that mapping ever grows
        // a second failure landing.
        assert_eq!(
            bounced_reason(COLUMN_IN_REVIEW, RunStatus::Failed, "boom"),
            None
        );
    }

    #[test]
    fn bounced_reason_is_none_on_todo_for_a_non_failure_status() {
        assert_eq!(
            bounced_reason(COLUMN_TODO, RunStatus::Succeeded, "n/a"),
            None
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

    /// CodeRabbit review (PR #1883): `Notification.title` is documented as
    /// one line, but `reason` is interpolated unnormalized. A failure reason
    /// carrying `\r`/`\n` — plausible, since it is an error's `Display` in
    /// practice — used to persist a multiline title.
    #[tokio::test]
    async fn notify_dispatch_failed_keeps_the_title_single_line() {
        let dir = tempfile::tempdir().unwrap();
        let notifications: Arc<dyn NotificationStore> = Arc::new(FsOps::new(dir.path()));
        let company = CompanyId::new("acme");

        notify_dispatch_failed(
            notifications.as_ref(),
            &company,
            "t-1",
            "boom\nsecond line\r\nthird line",
        )
        .await;

        let notes = notifications.list(&company, "anyone").await.unwrap();
        let filed = notes
            .iter()
            .find(|n| n.notification.subject.id == "t-1")
            .expect("the notification was filed");
        assert!(
            !filed.notification.title.contains('\n') && !filed.notification.title.contains('\r'),
            "the title must stay one line: {:?}",
            filed.notification.title
        );
        assert!(
            filed
                .notification
                .title
                .contains("boom second line  third line"),
            "the reason's content must survive, just flattened: {:?}",
            filed.notification.title
        );
    }

    /// CodeRabbit review (PR #1883): the boot-reaper conformance test
    /// (`runtime::builder::tests::boot_reaper_notifies_a_bounced_card_same_as_the_live_paths`)
    /// checks only `kind` and `subject.id`. This unit-tests the two things
    /// that shared assertion never exercised: the title actually names the
    /// task and carries the reason, and the row is company-wide
    /// (`audience: None`) — the whole reason this notification exists (issue
    /// #1865's doc comment) rather than a targeted one only the assignee
    /// would see.
    #[tokio::test]
    async fn notify_dispatch_failed_files_a_company_wide_row_naming_task_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let notifications: Arc<dyn NotificationStore> = Arc::new(FsOps::new(dir.path()));
        let company = CompanyId::new("acme");

        notify_dispatch_failed(
            notifications.as_ref(),
            &company,
            "t-42",
            "the host vanished",
        )
        .await;

        let notes = notifications.list(&company, "anyone-at-all").await.unwrap();
        let filed = notes
            .iter()
            .find(|n| n.notification.subject.id == "t-42")
            .expect("the notification was filed");
        assert_eq!(filed.notification.kind, "dispatch_failed");
        assert_eq!(filed.notification.subject.kind, SubjectKind::Task);
        assert!(
            filed.notification.title.contains("the host vanished"),
            "the title must carry the reason: {:?}",
            filed.notification.title
        );
        assert_eq!(
            filed.notification.audience, None,
            "a bounced card has no single decider the way a mention does — this must be \
             company-wide, not targeted at the assignee alone"
        );
    }

    /// A [`NotificationStore`] whose `append` always fails — the "the durable
    /// row itself could not be recorded" case `notify_dispatch_failed`'s own
    /// doc comment says is best-effort and logged, never propagated.
    struct FailingNotifications;

    #[async_trait::async_trait]
    impl NotificationStore for FailingNotifications {
        async fn append(&self, _company: &CompanyId, _notification: &Notification) -> Result<()> {
            Err(crate::error::OpenCompanyError::Store(
                "notification append always fails in this test".to_string(),
            ))
        }

        async fn list(
            &self,
            _company: &CompanyId,
            _user: &str,
        ) -> Result<Vec<crate::ports::notifications::NotificationView>> {
            Ok(Vec::new())
        }

        async fn mark_read(
            &self,
            _company: &CompanyId,
            _user: &str,
            _ids: Option<&[String]>,
        ) -> Result<u64> {
            Ok(0)
        }
    }

    /// CodeRabbit review (PR #1883): the boot-reaper test never exercises a
    /// failing store, so nothing proved the append failure stays best-effort.
    /// This is the whole point of the doc comment on `notify_dispatch_failed`
    /// — a card that bounced must not un-bounce because the notification
    /// bookkeeping write happened to fail.
    #[tokio::test]
    async fn notify_dispatch_failed_does_not_panic_or_propagate_when_append_fails() {
        let company = CompanyId::new("acme");
        // The whole assertion: this returns `()`, not a `Result`, and the call
        // completes without panicking even though `append` always errors.
        notify_dispatch_failed(&FailingNotifications, &company, "t-1", "boom").await;
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
