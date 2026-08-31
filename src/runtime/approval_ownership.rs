//! Which card owns a parked approval, on the **queue** read (#1891).
//!
//! # The two keys, and why the queue used to answer with the wrong one
//!
//! A park carries two correlation keys, and [`approval_owner`] on the task
//! detail read resolves them in a fixed order: the attempt (`Effect::run_id`,
//! #242) is authoritative wherever it is present, and the parked card link
//! (#333) is the fallback for a park with no attempt behind it. Neither is a
//! superset of the other — a `RunRecord` names its card so a run id resolves to
//! a task, but a task id can never say which *attempt* parked something, and
//! `run_id` is `None` by design for a chat turn, a workflow delivery or a gate.
//!
//! `GET …/approvals` projected the **raw park link** and nothing else. On a
//! read-only surface that was a wrong label: `the_attempt_id_outranks_the_card_link_when_both_are_present`
//! (`src/server/ops/write_test.rs`) parks an approval under one card's attempt
//! while stamping another card's link, and the task detail read correctly
//! refuses to show it — but the queue happily handed it out under the stamped
//! link, so a console joining on that link put it on the wrong card.
//!
//! #1891 turned that console-side join into a **decision surface**: the board
//! card now carries Approve and Decline. A wrong label became an operator
//! resolving a different card's request, which is why this resolution moved
//! host-side rather than staying a console approximation. The console cannot
//! do it — `ApprovalSummary` carries no attempt id for a task-linked park, and
//! `approval_owner` needs the card's own attempt ids, which the board has no
//! way to hold for every card on it.
//!
//! # What this does, and what it deliberately does not
//!
//! It **rewrites `ApprovalSummary::task` to the resolved owner** so every
//! consumer — the board's blocked row, the Approvals page's per-card filter,
//! the run drawer — reads one answer instead of each re-deriving a different
//! one. It does not touch the journal: the park's own record keeps both keys
//! exactly as it stamped them, and this is a read-side projection of the same
//! rule the detail route already applies.
//!
//! A park with **no** attempt keeps its stamped link untouched. That is the
//! fallback arm of the same rule, and leaving it alone is what keeps a chat
//! turn, a scheduler tick and a pre-#242 park reading exactly as they did.
//!
//! # Which parks the caller may hand over
//!
//! Only ones whose `run_id` is genuinely a **task attempt**. `Effect::run_id`
//! holds two id spaces — an attempt and, on the workflow path, a workflow run —
//! and `generate_id` is only process-locally unique, so the value cannot say
//! which it is; `workflow_run_of` discriminates on the recorded park site
//! instead. Handing a workflow run id to `owners` would let a collision with a
//! persisted attempt id resolve to that attempt's card, relabelling a workflow
//! approval onto a card no owner claims — on the surface that now decides
//! (#1895 review). `CompanyRuntime::pending_approvals_resolved` filters with
//! that same predicate before building either map.
//!
//! Which makes the guarantee a **subset**, not an equality: the queue never
//! claims an approval the card would disown, and abstains where the id space is
//! ambiguous. `approval_owner` can be exact there because it asks whether a run
//! is among *one card's* attempts; the queue has no per-card state to ask with.

use std::collections::HashMap;

use crate::runtime::types::{ApprovalSummary, TaskLink};

/// What the run store **answered** for each attempt it was asked about.
///
/// Presence is the load-bearing part: an entry means the store answered, and
/// `None` inside it is a definite "this attempt belongs to no card" — a chat
/// run, a workflow node's run, or a run the store says does not exist. An
/// attempt **absent from the map** was never successfully asked about, and is a
/// different thing entirely.
///
/// Collapsing those two was a defect (#1895 review). A transient store failure
/// became "no owner", which unlinked a still-parked approval; the console's
/// per-card join then dropped the row and the board card re-enabled Resume over
/// an approval nobody had decided — a store blip handing the operator exactly
/// the re-dispatch this work exists to keep out of their hand. A stale link is
/// a label that might be wrong. A dropped blocker is a card claiming to be free
/// when it is not.
///
/// [`approval_owner`]: crate::server::ops::tasks
pub type AttemptOwner<'a> = &'a HashMap<String, Option<String>>;

/// Rewrites each summary's `task` to the card the host would put it on.
///
/// `attempts` maps an **approval id** to the attempt that parked it, for the
/// parks that name one; `owners` maps an **attempt id** to the card that
/// attempt belongs to. Both are passed in rather than read here so this stays a
/// pure function of its inputs and can be tested without a runtime or a store —
/// the same shape `fold_page` and `approval_owner` are written in, and for the
/// same reason: an ownership rule that can only be exercised through an HTTP
/// round trip is one whose edge cases go untested.
///
/// Four outcomes, and the two that leave the stamp alone are as deliberate as
/// the two that overwrite it:
///
/// * the park names **no attempt** — its stamped link stands, untouched;
/// * the store was **never successfully asked** about the attempt — the stamp
///   stands too, because the only alternative is asserting something no read
///   supports (see [`AttemptOwner`]);
/// * the attempt resolves to a **card** — that card owns it, whatever the park
///   stamped, which is the correction;
/// * the store answered **no card** — [`TaskLink::Unlinked`], because a
///   card-level key can never override an attempt-level one. This is the arm
///   that matters: the stamped link is what the queue used to hand out, and
///   keeping it here would leave exactly the misattribution this exists to fix.
pub fn resolve_owners(
    summaries: &mut [ApprovalSummary],
    attempts: &HashMap<String, String>,
    owners: AttemptOwner<'_>,
) {
    for summary in summaries.iter_mut() {
        let Some(run_id) = attempts.get(summary.id.as_ref()) else {
            continue;
        };
        // `get` before the inner `Option`: an absent key is "not asked" and
        // must fall through, not read as "no card".
        let Some(answer) = owners.get(run_id) else {
            continue;
        };
        summary.task = Some(match answer.as_deref() {
            Some(task_id) => TaskLink::Task {
                id: task_id.to_string(),
            },
            None => TaskLink::Unlinked,
        });
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::runtime::types::ApprovalSummary;

    fn summary(id: &str, task: Option<TaskLink>) -> ApprovalSummary {
        ApprovalSummary {
            id: crate::ports::types::ApprovalId::new(id),
            kind: "web_fetch".to_string(),
            group: crate::ports::types::EffectGroup::Other,
            amount_usd: None,
            at_millis: 0,
            expires_at_millis: None,
            task,
            agent: None,
            payload: None,
            thread: None,
            workflow_run_id: None,
            workflow_id: None,
            broadly_grantable: false,
            broadly_deniable: false,
            contents_hidden: false,
            batch: None,
        }
    }

    fn attempts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(a, r)| (a.to_string(), r.to_string()))
            .collect()
    }

    fn owners(pairs: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(r, t)| (r.to_string(), t.map(str::to_string)))
            .collect()
    }

    /// The fallback arm. Nothing to outrank the stamp, so the stamp stands —
    /// a chat turn, a scheduler tick, a park from before #242.
    #[test]
    fn a_park_with_no_attempt_keeps_the_link_it_was_stamped_with() {
        let mut rows = vec![
            summary("a", Some(TaskLink::Task { id: "t-1".into() })),
            summary("b", Some(TaskLink::Unlinked)),
            summary("c", None),
        ];
        resolve_owners(&mut rows, &attempts(&[]), &owners(&[]));
        assert_eq!(rows[0].task, Some(TaskLink::Task { id: "t-1".into() }));
        assert_eq!(rows[1].task, Some(TaskLink::Unlinked));
        assert_eq!(rows[2].task, None);
    }

    /// The correction. The park stamped `t-1`; the attempt behind it belongs to
    /// `t-other`, so `t-other` owns it — which is what the task detail read has
    /// always said, and what the queue used to contradict.
    #[test]
    fn the_attempt_outranks_the_stamped_link() {
        let mut rows = vec![summary(
            "appr-elsewhere",
            Some(TaskLink::Task { id: "t-1".into() }),
        )];
        resolve_owners(
            &mut rows,
            &attempts(&[("appr-elsewhere", "run-c")]),
            &owners(&[("run-c", Some("t-other"))]),
        );
        assert_eq!(
            rows[0].task,
            Some(TaskLink::Task {
                id: "t-other".into()
            })
        );
    }

    /// The same rule pointing the other way: stamped `Unlinked`, but parked
    /// under this card's second attempt, so it lands on the card.
    #[test]
    fn an_attempt_claims_a_park_that_was_stamped_unlinked() {
        let mut rows = vec![summary("appr-attempt-2", Some(TaskLink::Unlinked))];
        resolve_owners(
            &mut rows,
            &attempts(&[("appr-attempt-2", "run-b")]),
            &owners(&[("run-b", Some("t-1"))]),
        );
        assert_eq!(rows[0].task, Some(TaskLink::Task { id: "t-1".into() }));
    }

    /// An attempt that belongs to no card takes the approval off every card,
    /// rather than letting the stale stamp put it on one. A card-level key
    /// never overrides an attempt-level one.
    #[test]
    fn an_attempt_owned_by_no_card_unlinks_the_approval() {
        let mut rows = vec![summary("a", Some(TaskLink::Task { id: "t-1".into() }))];
        resolve_owners(
            &mut rows,
            &attempts(&[("a", "run-chat")]),
            &owners(&[("run-chat", None)]),
        );
        assert_eq!(rows[0].task, Some(TaskLink::Unlinked));
    }

    /// An attempt the store *says* does not exist is a definite answer — no
    /// card claims it — and falling back to the stamp there would restore the
    /// misattribution on exactly the rows least able to prove otherwise.
    #[test]
    fn an_attempt_the_store_denies_unlinks_rather_than_trusting_the_stamp() {
        let mut rows = vec![summary("a", Some(TaskLink::Task { id: "t-1".into() }))];
        resolve_owners(
            &mut rows,
            &attempts(&[("a", "run-gone")]),
            &owners(&[("run-gone", None)]),
        );
        assert_eq!(rows[0].task, Some(TaskLink::Unlinked));
    }

    /// A read that never succeeded is **not** that answer (#1895 review).
    ///
    /// This is the one with teeth. Unlinking here drops the approval out of the
    /// console's per-card join, and the board card then re-enables Resume over
    /// something nobody decided — a transient store failure handing the
    /// operator the re-dispatch. The stamp is kept instead: possibly a wrong
    /// label, never a card that claims to be free while it is blocked.
    #[test]
    fn an_attempt_the_store_could_not_be_asked_about_keeps_its_stamp() {
        let mut rows = vec![
            summary("a", Some(TaskLink::Task { id: "t-1".into() })),
            summary("b", Some(TaskLink::Unlinked)),
        ];
        // Neither run id is in `owners` — the reads failed rather than answered.
        resolve_owners(
            &mut rows,
            &attempts(&[("a", "run-a"), ("b", "run-b")]),
            &owners(&[]),
        );
        assert_eq!(rows[0].task, Some(TaskLink::Task { id: "t-1".into() }));
        assert_eq!(rows[1].task, Some(TaskLink::Unlinked));
    }

    /// One failed read must not take its neighbours' answers with it.
    #[test]
    fn a_failed_read_does_not_disturb_the_attempts_that_answered() {
        let mut rows = vec![
            summary("a", Some(TaskLink::Task { id: "t-1".into() })),
            summary("b", Some(TaskLink::Task { id: "t-1".into() })),
        ];
        resolve_owners(
            &mut rows,
            &attempts(&[("a", "run-ok"), ("b", "run-failed")]),
            &owners(&[("run-ok", Some("t-other"))]),
        );
        assert_eq!(
            rows[0].task,
            Some(TaskLink::Task {
                id: "t-other".into()
            })
        );
        assert_eq!(rows[1].task, Some(TaskLink::Task { id: "t-1".into() }));
    }

    /// Two attempts at one card both land on it — #183 settled that repeat
    /// trips through review are normal, so this must not read as two owners.
    #[test]
    fn two_attempts_at_one_card_both_resolve_to_it() {
        let mut rows = vec![
            summary("a", Some(TaskLink::Unlinked)),
            summary("b", Some(TaskLink::Unlinked)),
        ];
        resolve_owners(
            &mut rows,
            &attempts(&[("a", "run-a"), ("b", "run-b")]),
            &owners(&[("run-a", Some("t-1")), ("run-b", Some("t-1"))]),
        );
        assert_eq!(rows[0].task, Some(TaskLink::Task { id: "t-1".into() }));
        assert_eq!(rows[1].task, Some(TaskLink::Task { id: "t-1".into() }));
    }
}
