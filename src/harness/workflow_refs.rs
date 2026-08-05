//! The workflow half of a card's output link (issue #339, epic #183 §6).
//!
//! # Why a queue and not a return value
//!
//! Exactly the [`PendingPublishQueue`](crate::harness::publish::PendingPublishQueue)
//! problem, with exactly the same answer. `run_workflow` and `create_workflow`
//! are built **once per agent**, while the card they are working varies **per
//! dispatch** — so neither tool can hold a task id, and neither can reach the
//! task store. They stage a [`TaskOutputWorkflow`] here; the brain drains it
//! inside the settle, where it already holds the card.
//!
//! # Correlation exists only for the orchestrator desk
//!
//! Both tools are orchestrator-only
//! ([`orchestrator_tools`](crate::harness::orchestrator::orchestrator_tools)),
//! so a card worked by any other teammate can never stage one. That is a real
//! limit of the correlation and it is named rather than papered over: workflow
//! links appear on orchestrator cards and nowhere else. Workflow *runs*
//! themselves have deliberately zero task correlation
//! ([`CompanyEvent::WorkflowRunFinished`](crate::ports::types::CompanyEvent)
//! carries no task id), which is precisely why the correlation has to be
//! captured here, at the moment the tool runs inside a dispatch, rather than
//! joined afterwards.
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::{Arc, Mutex};

use crate::ports::tasks::{TaskOutputAction, TaskOutputWorkflow};

/// The most workflow references one card's output stamp keeps.
///
/// A bound, not a design constraint. The stamp rides every board poll, so an
/// agent that ran forty workflows in one turn must not turn a card into a
/// payload. Far above any plausible real dispatch — and the collapse in
/// [`WorkflowRefQueue::drain`] already folds repeats of one workflow into a
/// single entry, so reaching this means forty *distinct* workflows.
pub const MAX_WORKFLOW_REFS: usize = 20;

/// A shared, in-memory queue of workflow references staged during a dispatch.
///
/// Cheap to [`Clone`] (a shared handle); the tools built into the agent and the
/// brain that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
#[derive(Clone, Default)]
pub struct WorkflowRefQueue {
    inner: Arc<Mutex<Vec<TaskOutputWorkflow>>>,
}

impl WorkflowRefQueue {
    /// Stages a reference to a workflow this turn ran or authored.
    pub fn push(&self, reference: TaskOutputWorkflow) {
        self.inner
            .lock()
            .expect("workflow ref queue")
            .push(reference);
    }

    /// Empties the queue. Called before each turn so nothing a prior turn — an
    /// operator chat turn earlier in the same cycle, or an abandoned redirect
    /// re-run — staged can be attributed to this card.
    pub fn clear(&self) {
        self.inner.lock().expect("workflow ref queue").clear();
    }

    /// How many references are staged, before collapsing.
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("workflow ref queue").len()
    }

    /// Drains every staged reference, emptying the queue and collapsing the raw
    /// list into what the card should carry.
    pub fn drain(&self) -> Vec<TaskOutputWorkflow> {
        let raw = std::mem::take(&mut *self.inner.lock().expect("workflow ref queue"));
        collapse(raw)
    }
}

/// Folds the raw staged list into one entry per workflow, newest-first-wins on
/// having actually run, capped at [`MAX_WORKFLOW_REFS`].
///
/// # Why collapse at all
///
/// The common shape is an agent that authors a graph and then runs it in the
/// same turn — two entries naming one workflow. The card shows *one* link, and
/// "created" is the weaker of the two answers: a run has a run id, so it can
/// open the overlay showing what actually executed. Keeping both would also
/// inflate the `+N more` count with a duplicate the operator cannot tell apart.
///
/// So per workflow id: the **last** entry that ran wins; failing that, the last
/// entry at all. Order otherwise follows first appearance, so the list reads in
/// the order the turn did things rather than in hash order.
fn collapse(raw: Vec<TaskOutputWorkflow>) -> Vec<TaskOutputWorkflow> {
    let mut out: Vec<TaskOutputWorkflow> = Vec::new();
    for reference in raw {
        match out
            .iter_mut()
            .find(|held| held.workflow_id == reference.workflow_id)
        {
            // A later *run* supersedes whatever we held; a later *create* does
            // not overwrite a run we already saw, because the run is the
            // stronger link.
            Some(held) if reference.action == TaskOutputAction::Ran => *held = reference,
            Some(_) => {}
            None => out.push(reference),
        }
    }
    out.truncate(MAX_WORKFLOW_REFS);
    out
}

#[cfg(test)]
mod test {
    use super::*;

    fn wf(id: &str, run: Option<&str>, action: TaskOutputAction) -> TaskOutputWorkflow {
        TaskOutputWorkflow {
            workflow_id: id.to_string(),
            run_id: run.map(str::to_string),
            action,
        }
    }

    /// The shared-handle property the whole design rests on: the tool pushes
    /// through one clone, the brain drains through another, and the drain
    /// empties the queue so a second dispatch starts clean.
    #[test]
    fn a_clone_sees_the_same_queue_and_a_drain_empties_it() {
        let queue = WorkflowRefQueue::default();
        let tool_side = queue.clone();
        tool_side.push(wf("digest", Some("wf-1"), TaskOutputAction::Ran));
        assert_eq!(queue.queued(), 1);

        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].workflow_id, "digest");
        assert_eq!(queue.queued(), 0, "a drain must not leak into the next run");
        assert!(queue.drain().is_empty());
    }

    /// A redirect abandons the turn that staged, so the queue is cleared per
    /// turn — a workflow an abandoned re-run started must not be credited to
    /// the card that eventually settles.
    #[test]
    fn clear_discards_an_abandoned_turns_refs() {
        let queue = WorkflowRefQueue::default();
        queue.push(wf("stale", None, TaskOutputAction::Created));
        queue.clear();
        assert!(queue.drain().is_empty());
    }

    /// The common shape: author a graph, then run it. One workflow, one link,
    /// and the link is the one that can show what actually executed.
    #[test]
    fn creating_then_running_one_workflow_collapses_to_the_run() {
        let queue = WorkflowRefQueue::default();
        queue.push(wf("digest", None, TaskOutputAction::Created));
        queue.push(wf("digest", Some("wf-1"), TaskOutputAction::Ran));

        let drained = queue.drain();
        assert_eq!(drained.len(), 1, "got {drained:?}");
        assert_eq!(drained[0].action, TaskOutputAction::Ran);
        assert_eq!(drained[0].run_id.as_deref(), Some("wf-1"));
    }

    /// Run twice: the later run wins, because the card's link should open the
    /// run that most recently happened rather than an earlier one.
    #[test]
    fn a_later_run_supersedes_an_earlier_one() {
        let queue = WorkflowRefQueue::default();
        queue.push(wf("digest", Some("wf-1"), TaskOutputAction::Ran));
        queue.push(wf("digest", Some("wf-2"), TaskOutputAction::Ran));
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].run_id.as_deref(), Some("wf-2"));
    }

    /// The reverse order must NOT downgrade: authoring a second graph after
    /// running the first is two workflows, and re-saving one already run keeps
    /// the run link.
    #[test]
    fn a_later_create_does_not_downgrade_a_run() {
        let queue = WorkflowRefQueue::default();
        queue.push(wf("digest", Some("wf-1"), TaskOutputAction::Ran));
        queue.push(wf("digest", None, TaskOutputAction::Created));
        queue.push(wf("weekly", None, TaskOutputAction::Created));

        let drained = queue.drain();
        assert_eq!(drained.len(), 2, "got {drained:?}");
        assert_eq!(drained[0].workflow_id, "digest");
        assert_eq!(
            drained[0].run_id.as_deref(),
            Some("wf-1"),
            "a re-save must not erase the link to the run"
        );
        // First-appearance order, so the list reads as the turn happened.
        assert_eq!(drained[1].workflow_id, "weekly");
    }

    /// The bound, so a pathological turn cannot turn a board poll into a
    /// payload. Distinct workflows past the cap are dropped, not merged.
    #[test]
    fn the_staged_list_is_capped() {
        let queue = WorkflowRefQueue::default();
        for i in 0..MAX_WORKFLOW_REFS + 5 {
            queue.push(wf(&format!("wf-{i}"), None, TaskOutputAction::Created));
        }
        assert_eq!(queue.drain().len(), MAX_WORKFLOW_REFS);
    }
}
