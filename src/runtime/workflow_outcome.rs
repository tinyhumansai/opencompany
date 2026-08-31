//! Issue #228: journal what a workflow run actually did, from every entry point.
//!
//! A workflow run's outcome used to exist only in the moment. A **manual** run's
//! [`DeliveryReport`] rows lived in the console's run drawer until it was
//! dismissed; a **scheduled** run's reached only host stdout, which on a hosted
//! tenant is the platform team rather than the tenant's operator. Nothing wrote
//! a run outcome anywhere the console could read back afterwards — so the exact
//! thing an operator most needs to find later ("did last night's owner summary
//! actually go out?") was unfindable an hour after the run.
//!
//! This module is the one place that writes
//! [`CompanyEvent::WorkflowRunFinished`]. Both entry points — the console's
//! `POST …/workflows/{wid}/run` route and the cron
//! [`WorkflowScheduler`](super::WorkflowScheduler) — call
//! [`record_run_finished`], so a run's history is uniform no matter what started
//! it and the two call sites cannot drift apart in what they record.
//!
//! **Best-effort by construction.** The append happens *after* the run returns,
//! so it always records a finished run, and a failure to append is logged and
//! swallowed: journalling an outcome must never disturb the run path or fail a
//! run whose work already happened.
//!
//! It deliberately does **not** replace the scheduler's log lines. Those remain
//! the platform team's diagnostic on host stdout; this event is the *operator's*
//! surface, read back through `GET …/workflows/runs`.
//!
//! **Issue #371** added the other end of the same record. A run now also
//! journals a [`CompanyEvent::WorkflowRunStarted`] before the engine call and a
//! [`CompanyEvent::WorkflowNodeFinished`] per node as the graph is walked (both
//! written by the workflow runner), all sharing the `run_id` the entry point
//! mints and hands to [`record_run_finished`]. That correlation is what lets the
//! read side group a run's nodes with its outcome — and it is why a start with
//! no finish is meaningful, which [`sweep_interrupted_runs`] settles at boot.
//!
//! **Issue #383** added a third terminal reading. A run can now be *stopped by
//! an operator*, which is neither a failure nor a host restart, so it lands as
//! `cancelled: true` with **no error at all** rather than as an error string. A
//! cancelled run journals a real finish through this same helper, which is what
//! keeps [`sweep_interrupted_runs`] out of it: there is nothing left open to
//! sweep.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ports::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};
use crate::ports::workflow_runner::{DeliveryReport, WorkflowRun, WorkflowRunBoardRow};
use crate::runtime::workflow_resume::DeliveredReport;

/// The error stamped on a run the host never got to finish (issue #371).
///
/// Phrased as a host fact rather than a workflow fault: nothing about the graph
/// went wrong, the process holding it went away. An operator reading this in the
/// history should go looking at the deployment, not at their nodes.
pub const INTERRUPTED_BY_RESTART: &str = concat!(
    "this run was interrupted by a host restart and never finished; ",
    "the nodes recorded against it are the ones that completed before it stopped"
);

/// Journals a finished workflow run, best-effort.
///
/// `scheduled` says whether a cron started the run rather than an operator —
/// the distinction is the point, since a scheduled run is the
/// nobody-was-watching case this record exists for.
///
/// `run_id` correlates this outcome with the run's
/// [`WorkflowRunStarted`](CompanyEvent::WorkflowRunStarted) and every
/// [`WorkflowNodeFinished`](CompanyEvent::WorkflowNodeFinished) between them
/// (issue #371). The caller mints it, because on the error arm the runner
/// returns nothing that could carry one — and a failed run's per-node trail is
/// exactly the one worth correlating.
///
/// `outcome` is what the [`WorkflowRunner`](crate::ports::WorkflowRunner)
/// returned, error included: a run that failed outright is recorded too, and is
/// in fact the most important thing here — today's `Err` arm on the scheduled
/// path only warns to host stdout, so **the worst outcome is currently the
/// quietest**.
/// The six fields a settled run contributes to its journal row, split out of
/// [`record_run_finished`] so the Ok/Err fold is one named thing rather than a
/// six-wide tuple.
///
/// Which fields ride which arm is the whole content of this type — see
/// [`Settled::from`].
struct Settled {
    deliveries: Vec<DeliveryReport>,
    pending_approvals: Vec<String>,
    error: Option<String>,
    cancelled: bool,
    notices: Vec<String>,
    board: Vec<WorkflowRunBoardRow>,
    blocked_nodes: Vec<crate::ports::WorkflowBlockedNode>,
    approvals: Vec<crate::ports::WorkflowRunApprovalRow>,
}

/// A run that ended in an error, as [`record_run_finished`] takes it (issue
/// #1008).
///
/// # Why this is not just a `&str`
///
/// It was, and that was the bug. A failed run's journal row listed no
/// deliveries, no board rows, no blocked nodes and no approvals — not because
/// the run had none, but because the *error arm had nowhere to read them from*.
/// A run that mailed two owner summaries and then broke at a later node showed
/// zero delivery rows, which reads as "it sent nothing" and is the opposite of
/// true; a run that opened a card and then failed left the card on the board
/// with no run admitting to it.
///
/// `partial` is what the run had done by the time it broke, threaded up from
/// the runner on
/// [`OpenCompanyError::WorkflowRunFailed`](crate::OpenCompanyError::WorkflowRunFailed).
/// `None` is still correct and still common: the boot sweep's synthetic finish
/// describes a run this process never saw, and a run refused before the engine
/// started has genuinely done nothing.
///
/// The `From<&str>` conversion keeps every call site that has only a message
/// reading as it did.
pub struct FailedRun<'a> {
    /// The failure, as the journal's `error` string.
    pub error: &'a str,
    /// What the run had already done, when the caller can say.
    pub partial: Option<&'a WorkflowRun>,
}

impl<'a> From<&'a str> for FailedRun<'a> {
    /// A failure with nothing known about what the run had done.
    fn from(error: &'a str) -> Self {
        Self {
            error,
            partial: None,
        }
    }
}

impl From<Result<&WorkflowRun, FailedRun<'_>>> for Settled {
    /// # Which fields ride which arm
    ///
    /// Issue #383: `cancelled` rides the `Ok` arm only, and that is not an
    /// oversight. A run the runner never returned from cannot have been *stopped
    /// by an operator* — the stop signal resolves into an `Ok(cancelled)`, never
    /// into an `Err` — so the error arm is unambiguously a failure or the boot
    /// sweep's synthetic one.
    ///
    /// # The other five ride both arms now (issue #1008)
    ///
    /// `deliveries`, `notices`, `board`, `blocked_nodes` and `approvals` used to
    /// be hard-coded empty on the `Err` arm, on the argument that a failure
    /// "returns no `WorkflowRun` to read rows off". That was true of the
    /// signature, not of the world: every one of those rows records something
    /// the run **already did** — a report that was mailed, a card that is on the
    /// board, an approval sitting on the operator's Approvals page — and all of
    /// them are durable by the time the run breaks. Zeroing them made the
    /// journal disagree with what the operator could see in front of them.
    ///
    /// So the failure arm now carries a [`FailedRun::partial`] when the caller
    /// has one, and reads exactly the same fields off it that the `Ok` arm
    /// reads. `pending_approvals` is the one deliberate exception: it describes
    /// what the run is *still waiting on*, and a run that failed is waiting on
    /// nothing.
    fn from(outcome: Result<&WorkflowRun, FailedRun<'_>>) -> Self {
        match outcome {
            Ok(run) => Self {
                deliveries: run.deliveries.clone(),
                pending_approvals: run.pending_approvals.clone(),
                error: None,
                cancelled: run.cancelled,
                notices: run.notices.clone(),
                board: run.board.clone(),
                blocked_nodes: run.blocked_nodes.clone(),
                approvals: run.approvals.clone(),
            },
            Err(failed) => {
                let partial = failed.partial;
                Self {
                    deliveries: partial.map(|p| p.deliveries.clone()).unwrap_or_default(),
                    // The exception, and it is a claim: a failed run is not
                    // waiting on anybody. Any gate it parked before it broke is
                    // still decidable from the Approvals page — `approvals`
                    // below is the receipt for that — but listing it here would
                    // say this run intends to continue, which it does not.
                    pending_approvals: Vec::new(),
                    error: Some(failed.error.to_string()),
                    cancelled: false,
                    notices: partial.map(|p| p.notices.clone()).unwrap_or_default(),
                    board: partial.map(|p| p.board.clone()).unwrap_or_default(),
                    blocked_nodes: partial.map(|p| p.blocked_nodes.clone()).unwrap_or_default(),
                    approvals: partial.map(|p| p.approvals.clone()).unwrap_or_default(),
                }
            }
        }
    }
}

/// Returns whether the finish was durably appended. `false` means the append
/// failed and was swallowed (the run itself is unaffected, exactly as before) —
/// so a caller that knows the run is otherwise about to disappear from the live
/// set can escalate the lost record from this helper's `warn` to an `error`
/// naming the run. Issue #1009 added the return; callers that do not care about
/// the outcome (the boot sweep, the read-side cross-check) simply ignore it.
pub async fn record_run_finished(
    events: &Arc<dyn EventLog>,
    company: &CompanyId,
    workflow_id: &str,
    scheduled: bool,
    run_id: &str,
    outcome: Result<&WorkflowRun, FailedRun<'_>>,
) -> bool {
    // Which fields ride which arm, and why, is documented on `Settled::from`.
    let settled = Settled::from(outcome);

    let event = CompanyEvent::WorkflowRunFinished {
        workflow_id: workflow_id.to_string(),
        scheduled,
        // Issue #371 started populating this reserved field. The event's own
        // wire shape is unchanged — it has always carried an optional `run_id` —
        // so a reader predating #371 still decodes every line it could before.
        run_id: Some(run_id.to_string()),
        deliveries: settled.deliveries,
        pending_approvals: settled.pending_approvals,
        error: settled.error,
        cancelled: settled.cancelled,
        notices: settled.notices,
        board: settled.board,
        blocked_nodes: settled.blocked_nodes,
        approvals: settled.approvals,
    };

    if let Err(err) = events.append(company, event).await {
        // Swallowed on purpose: the run already happened and its work is valid.
        // Losing the record is worth a loud line, never a failed run. The caller
        // learns of the loss through the `false` return (issue #1009) and may
        // escalate it — this helper stays best-effort and never fails the run.
        tracing::warn!(
            %company,
            workflow = %workflow_id,
            scheduled,
            %err,
            "workflow run outcome could not be journaled; the run itself was unaffected"
        );
        return false;
    }
    true
}

/// Terminates workflow runs a previous host process left open (issue #371).
///
/// # Why an unterminated start is provably dead
///
/// Issue #371 made a run journal a
/// [`WorkflowRunStarted`](CompanyEvent::WorkflowRunStarted) *before* the engine
/// call, so a host that dies mid-run leaves a start with no matching finish.
/// Every entry point — the console's run route, the cron scheduler, the
/// orchestrator's `run_workflow` tool — drives the run future **inside this
/// process**, and exactly one process owns a company's journal (it is a
/// single-writer log). So at boot, before any of those entry points can have
/// started anything, an unmatched start cannot belong to a live run: there are
/// no live runs. No timeout heuristic is needed, for the same reason
/// [`reap_orphaned_runs`](crate::ports::runs::reap_orphaned_runs) needs none.
///
/// This is what keeps the read side honest. `GET …/workflows/runs` folds a
/// start without a finish as `running: true`, and that claim is only true
/// because this sweep settles the ones that will never finish — otherwise a run
/// killed last week would show a spinner forever.
///
/// # It must NOT run on a rebuild
///
/// The argument above holds at boot and is false the moment a company has been
/// serving. A scheduler-spawned run survives a live runtime swap
/// ([`rebuild_company`](crate::runtime::rebuild_company)), so sweeping mid-life
/// would stamp "interrupted by a host restart" on a run that is still walking
/// its graph — and then its real finish would land afterwards, leaving two
/// contradictory outcomes for one run id. The caller gates on the handover being
/// absent; see the call site in the runtime builder. Same lesson as #290.
///
/// Best-effort throughout: a read or append failure is logged and swallowed,
/// because record-keeping must never stop a company from booting.
pub async fn sweep_interrupted_runs(events: &Arc<dyn EventLog>, company: &CompanyId) {
    let stored = match events
        .read_from(company, EventSeq::new(0), usize::MAX)
        .await
    {
        Ok(stored) => stored,
        Err(err) => {
            tracing::warn!(
                %company,
                %err,
                "could not read the journal to sweep interrupted workflow runs"
            );
            return;
        }
    };

    // One pass, keyed on run id: a start inserts, a finish removes. Whatever is
    // left started and never settled. `HashMap` rather than two sets because the
    // synthetic finish needs the start's `workflow_id` and `scheduled` flag, and
    // those live only on the start.
    let mut open: HashMap<String, (String, bool)> = HashMap::new();
    for stored in stored {
        match stored.event {
            CompanyEvent::WorkflowRunStarted {
                workflow_id,
                run_id,
                scheduled,
                // This fold groups a run's nodes with its outcome; who started
                // it (issue #1862 prerequisite) is not part of that grouping.
                started_by: _,
            } => {
                open.insert(run_id, (workflow_id, scheduled));
            }
            CompanyEvent::WorkflowRunFinished {
                run_id: Some(run_id),
                ..
            } => {
                open.remove(&run_id);
            }
            // A pre-#371 finished row carries no run id and therefore closes
            // nothing. That is correct rather than a gap: it also had no start
            // to be matched against, so no such run can be sitting in `open`.
            _ => {}
        }
    }

    if open.is_empty() {
        return;
    }

    // Sorted so the appended order is deterministic — a `HashMap` iteration
    // order would make the journal's tail differ run to run for no reason, and
    // tests would have to sort around it.
    let mut interrupted: Vec<(String, (String, bool))> = open.into_iter().collect();
    interrupted.sort_by(|a, b| a.0.cmp(&b.0));

    for (run_id, (workflow_id, scheduled)) in interrupted {
        tracing::info!(
            %company,
            workflow = %workflow_id,
            %run_id,
            scheduled,
            "settling a workflow run left open by a previous host process"
        );
        record_run_finished(
            events,
            company,
            &workflow_id,
            scheduled,
            &run_id,
            Err(INTERRUPTED_BY_RESTART.into()),
        )
        .await;
    }
}

/// What the trailing suffix of **unclean** runs of one workflow already
/// delivered, folded from the journal (issue #529).
///
/// # The hole this closes
///
/// A run's deliveries live only on
/// [`WorkflowRunFinished::deliveries`](CompanyEvent::WorkflowRunFinished), which
/// is journaled *after* the run returns. A crash, a mid-graph failure, or a
/// panic therefore orphans the side effect: the report left the process, but the
/// boot sweep settles the run `FAILED` (or the run simply has no finish at all),
/// so no delivery record exists — and an operator's re-run re-mails every
/// already-sent report to real people. Issue #438's ledger does not cover this:
/// it rides one approval lineage's trigger input and is never persisted, so an
/// *independently* re-run or re-triggered workflow re-delivers.
///
/// This fold is the durable half. It replays the journal and returns the reports
/// dispatched by the **uncleanly-finished trailing runs** — the ones that owe
/// the next run nothing, because their work is stranded.
///
/// # Semantics: a clean finish absorbs and clears
///
/// One pass in journal order, holding a set of delivered nodes for `workflow_id`:
///
/// * a [`WorkflowReportDelivered`](CompanyEvent::WorkflowReportDelivered) for
///   this workflow **inserts** its node (write-behind at dispatch, so a
///   crashed run's sends are here even though its finish is not);
/// * a [`WorkflowRunFinished`](CompanyEvent::WorkflowRunFinished) for this
///   workflow with **no error clears** the set.
///
/// The clear is what keeps a daily scheduled digest delivering every day: a run
/// that finishes cleanly (a normal completion, or a
/// [`cancelled`](CompanyEvent::WorkflowRunFinished) one — which returns *before*
/// `deliver_outputs` and so dispatched nothing here, and carries no error)
/// absorbs everything delivered up to it, so the next run starts owed nothing.
/// Only deliveries by the *unclean* trailing suffix — a crash whose boot-sweep
/// finish carries the synthetic `error: Some(..)`, a mid-graph failure, or a
/// panic that never journaled a finish at all — survive the fold, and those are
/// exactly the ones a re-run must not repeat.
///
/// Identity is the **node**, so this unions cleanly with issue #438's
/// [`DeliveredReport`](crate::runtime::workflow_resume::DeliveredReport): the
/// caller concatenates the two and dedupes. An `owner` fan-out that wrote one
/// line per admin collapses to a single node entry here.
///
/// Best-effort read, the same posture as
/// [`sweep_interrupted_runs`] (which is left untouched): a journal read failure
/// yields an empty ledger and a warn, because a delivery guard that cannot read
/// the journal must fail *open* to the pre-#529 behaviour (deliver) rather than
/// silently suppress a report.
pub async fn delivered_by_unsettled_runs(
    events: &Arc<dyn EventLog>,
    company: &CompanyId,
    workflow_id: &str,
) -> Vec<DeliveredReport> {
    let stored = match events
        .read_from(company, EventSeq::new(0), usize::MAX)
        .await
    {
        Ok(stored) => stored,
        Err(err) => {
            tracing::warn!(
                %company,
                workflow = %workflow_id,
                %err,
                "could not read the journal to fold this workflow's stranded deliveries; a re-run \
                 will not know what a crashed run already sent"
            );
            return Vec::new();
        }
    };

    // Insertion order preserved, deduped by node — an output node has exactly one
    // destination, so its id is the identity and the first-seen kind describes
    // it. A `Vec` rather than a set keeps the returned order deterministic for
    // the caller's union.
    let mut delivered: Vec<DeliveredReport> = Vec::new();
    for stored in stored {
        match stored.event {
            // The dedupe (`not already present`) rides the guard: a repeat of a
            // node already in the ledger simply does not match, and falls through
            // to the no-op arm below — an `owner` fan-out's one-line-per-admin
            // collapses to a single entry this way.
            CompanyEvent::WorkflowReportDelivered {
                workflow_id: wid,
                node,
                kind,
                ..
            } if wid == workflow_id && !delivered.iter().any(|prior| prior.node == node) => {
                delivered.push(DeliveredReport { node, kind });
            }
            // A clean finish (no error) absorbs everything delivered up to it and
            // resets the ledger — the cadence guarantee. A failed / interrupted
            // finish carries an error and does NOT clear, so its run's stranded
            // deliveries stay owed-nothing to the next run.
            CompanyEvent::WorkflowRunFinished {
                workflow_id: wid,
                error,
                ..
            } if wid == workflow_id && error.is_none() => {
                delivered.clear();
            }
            _ => {}
        }
    }
    delivered
}

#[cfg(test)]
mod test {
    use serde_json::Value;

    use super::*;
    use crate::ports::types::EventSeq;
    use crate::ports::workflow_runner::DeliveryStatus;
    use crate::store::FsEventLog;

    /// The real filesystem journal over a temp home, not a test double: the
    /// claim being made is that a run outcome survives to disk and reads back,
    /// so the JSONL round trip is the thing under test.
    fn log() -> (tempfile::TempDir, Arc<dyn EventLog>) {
        let dir = tempfile::Builder::new()
            .prefix("oc-run-outcome-")
            .tempdir()
            .expect("tempdir");
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(dir.path()));
        (dir, events)
    }

    fn run_with(deliveries: Vec<DeliveryReport>, pending: Vec<String>) -> WorkflowRun {
        WorkflowRun {
            output: Value::Null,
            pending_approvals: pending,
            deliveries,
            cancelled: false,
            nodes: Vec::new(),
            notices: Vec::new(),
            board: Vec::new(),
            blocked_nodes: Vec::new(),
            approvals: Vec::new(),
        }
    }

    /// Issue #638: a run's notices survive onto the journaled outcome, which is
    /// the row the console's history panel reads.
    ///
    /// The point of the assertion is the **round trip**, not the assignment: the
    /// event is written to a real JSONL log and read back, so a field that
    /// serialized but did not deserialize — or one `skip_serializing_if` dropped
    /// on the way out — fails here rather than in front of an operator.
    #[tokio::test]
    async fn a_runs_notices_reach_the_journaled_outcome() {
        let (_dir, events) = log();
        let company = CompanyId::new("acme");
        let notice = "Heads up: 3 further gated tool calls were not raised for approval.";
        let run = WorkflowRun {
            notices: vec![notice.to_string()],
            ..run_with(Vec::new(), Vec::new())
        };

        record_run_finished(&events, &company, "wf", false, "run-1", Ok(&run)).await;

        let journaled = journaled(&events, &company).await;
        let CompanyEvent::WorkflowRunFinished { notices, error, .. } = journaled
            .iter()
            .find(|e| matches!(e, CompanyEvent::WorkflowRunFinished { .. }))
            .expect("the outcome was journaled")
        else {
            unreachable!("matched above")
        };
        assert_eq!(notices, &vec![notice.to_string()]);
        assert!(
            error.is_none(),
            "a run that raised a notice did not fail — putting this in `error` \
             would inflate the failure count and hide a real failure among them",
        );
    }

    /// The other direction, and the one that keeps the field honest: a run with
    /// nothing to say carries an empty list, and a failure the caller knows
    /// nothing else about carries none either — rather than inheriting whatever
    /// the Ok arm would have had.
    ///
    /// Since issue #1008 that second clause is conditional on the *caller*: a
    /// failure that arrives with a partial run does carry its notices, which
    /// `a_run_that_delivered_and_then_failed_keeps_its_rows` pins. This one pins
    /// the arm where there is genuinely nothing to read.
    #[tokio::test]
    async fn an_ordinary_run_and_a_failure_with_nothing_known_carry_no_notices() {
        let (_dir, events) = log();
        let company = CompanyId::new("acme");

        let ok = run_with(Vec::new(), Vec::new());
        record_run_finished(&events, &company, "wf", false, "run-ok", Ok(&ok)).await;
        record_run_finished(
            &events,
            &company,
            "wf",
            false,
            "run-bad",
            Err("boom".into()),
        )
        .await;

        for event in journaled(&events, &company).await {
            let CompanyEvent::WorkflowRunFinished { notices, .. } = event else {
                continue;
            };
            assert!(notices.is_empty(), "nothing to say means an empty list");
        }
    }

    /// Issue #1008's second half: a run that **delivered two reports and then
    /// failed** still lists those deliveries.
    ///
    /// This is the arm that was hard-coded empty. The reports were already in
    /// the recipients' inboxes by the time the later node broke, so journaling
    /// zero delivery rows did not describe a run that sent nothing — it
    /// described a run whose record disagreed with the world. The same holds for
    /// the board card it opened, the approval it parked and the notice it
    /// raised, so all four ride the failure arm together.
    #[tokio::test]
    async fn a_run_that_delivered_and_then_failed_keeps_its_rows() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");

        let partial = WorkflowRun {
            notices: vec!["a call was refused".to_string()],
            board: vec![crate::ports::WorkflowRunBoardRow {
                action: crate::ports::WorkflowBoardAction::Spawned,
                task_id: Some("task-7".to_string()),
                title: Some("Draft the summary".to_string()),
                assignee: None,
            }],
            approvals: vec![crate::ports::WorkflowRunApprovalRow {
                node_id: Some("work".to_string()),
                tool: Some("shell".to_string()),
                outcome: crate::ports::WorkflowApprovalOutcome::Parked,
                approval_id: Some("appr-1".to_string()),
            }],
            ..run_with(
                vec![
                    report("owner-summary", DeliveryStatus::Sent),
                    report("board-summary", DeliveryStatus::Sent),
                ],
                Vec::new(),
            )
        };

        record_run_finished(
            &events,
            &company,
            "digest",
            false,
            "run-bad",
            Err(FailedRun {
                error: "the third node died",
                partial: Some(&partial),
            }),
        )
        .await;

        let CompanyEvent::WorkflowRunFinished {
            deliveries,
            error,
            board,
            approvals,
            notices,
            pending_approvals,
            ..
        } = journaled(&events, &company)
            .await
            .into_iter()
            .find(|e| matches!(e, CompanyEvent::WorkflowRunFinished { .. }))
            .expect("the failure was journaled")
        else {
            unreachable!("matched above")
        };

        assert_eq!(
            deliveries.len(),
            2,
            "two reports were sent before the run broke; zeroing them tells the operator \
             nothing went out: {deliveries:?}"
        );
        assert_eq!(
            error.as_deref(),
            Some("the third node died"),
            "carrying the rows must not stop this reading as a failure"
        );
        assert_eq!(board.len(), 1, "the card is on the board: {board:?}");
        assert_eq!(
            approvals.len(),
            1,
            "the card is on the Approvals page: {approvals:?}"
        );
        assert_eq!(notices, vec!["a call was refused".to_string()]);
        assert!(
            pending_approvals.is_empty(),
            "the one deliberate exception: a failed run is not waiting on anybody, however \
             many gates it parked on the way down"
        );
    }

    /// The other half of the same claim: a failure with **nothing known** about
    /// what the run did still journals honestly empty rows rather than inventing
    /// any. The boot sweep's synthetic finish is exactly this shape.
    #[tokio::test]
    async fn a_failure_with_no_partial_run_journals_empty_rows() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");

        record_run_finished(
            &events,
            &company,
            "digest",
            false,
            "run-bad",
            Err("boom".into()),
        )
        .await;

        let CompanyEvent::WorkflowRunFinished {
            deliveries,
            board,
            approvals,
            blocked_nodes,
            ..
        } = journaled(&events, &company)
            .await
            .into_iter()
            .find(|e| matches!(e, CompanyEvent::WorkflowRunFinished { .. }))
            .expect("the failure was journaled")
        else {
            unreachable!("matched above")
        };
        assert!(deliveries.is_empty());
        assert!(board.is_empty());
        assert!(approvals.is_empty());
        assert!(blocked_nodes.is_empty());
    }

    fn report(node: &str, status: DeliveryStatus) -> DeliveryReport {
        DeliveryReport {
            node: node.to_string(),
            kind: "owner".to_string(),
            target: Some("ada@example.com".to_string()),
            status,
            detail: "this recipient has never written to the company".to_string(),
            reason: crate::ports::DeliveryReason::RecipientNotEstablished,
        }
    }

    async fn journaled(events: &Arc<dyn EventLog>, company: &CompanyId) -> Vec<CompanyEvent> {
        events
            .read_from(company, EventSeq::new(0), usize::MAX)
            .await
            .expect("read")
            .into_iter()
            .map(|s| s.event)
            .collect()
    }

    /// A completed run records its delivery rows and pending approvals verbatim
    /// — the rows are the whole reason the record exists.
    #[tokio::test]
    async fn a_completed_run_records_its_rows_and_approvals() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        let run = run_with(
            vec![
                report("owner_summary", DeliveryStatus::Skipped),
                report("also_sent", DeliveryStatus::Sent),
            ],
            vec!["review".to_string()],
        );

        record_run_finished(&events, &company, "digest", true, "run-1", Ok(&run)).await;

        let events = journaled(&events, &company).await;
        assert_eq!(events.len(), 1);
        let CompanyEvent::WorkflowRunFinished {
            workflow_id,
            scheduled,
            deliveries,
            pending_approvals,
            error,
            ..
        } = &events[0]
        else {
            panic!("expected a WorkflowRunFinished, got {:?}", events[0]);
        };
        assert_eq!(workflow_id, "digest");
        assert!(*scheduled);
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].node, "owner_summary");
        assert_eq!(deliveries[0].status, DeliveryStatus::Skipped);
        // The `detail` is the part that says what to fix, so it must survive.
        assert!(deliveries[0].detail.contains("never written"));
        assert_eq!(pending_approvals, &vec!["review".to_string()]);
        assert!(error.is_none(), "a completed run carries no error");
    }

    /// The arm that matters most: a run that failed outright is recorded, with
    /// the reason. Before this it only warned to host stdout.
    #[tokio::test]
    async fn a_failed_run_records_the_error() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");

        record_run_finished(
            &events,
            &company,
            "digest",
            true,
            "run-1",
            Err("agent node `worker` had no inference source".into()),
        )
        .await;

        let events = journaled(&events, &company).await;
        let CompanyEvent::WorkflowRunFinished {
            deliveries,
            pending_approvals,
            error,
            ..
        } = &events[0]
        else {
            panic!("expected a WorkflowRunFinished");
        };
        assert!(deliveries.is_empty());
        assert!(pending_approvals.is_empty());
        assert_eq!(
            error.as_deref(),
            Some("agent node `worker` had no inference source")
        );
    }

    /// A manual run is recorded the same way, flagged as not scheduled — that
    /// flag is what lets the console tell a cron run from a Run-button one.
    #[tokio::test]
    async fn a_manual_run_is_recorded_as_unscheduled() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        let run = run_with(Vec::new(), Vec::new());

        record_run_finished(&events, &company, "digest", false, "run-1", Ok(&run)).await;

        let events = journaled(&events, &company).await;
        let CompanyEvent::WorkflowRunFinished { scheduled, .. } = &events[0] else {
            panic!("expected a WorkflowRunFinished");
        };
        assert!(!*scheduled);
    }

    /// Issue #371: the outcome now carries the caller's run id, which is the
    /// only thing tying it to the run's start and per-node events.
    #[tokio::test]
    async fn the_outcome_carries_the_callers_run_id() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        let run = run_with(Vec::new(), Vec::new());

        record_run_finished(&events, &company, "digest", false, "run-42", Ok(&run)).await;

        let events = journaled(&events, &company).await;
        let CompanyEvent::WorkflowRunFinished { run_id, .. } = &events[0] else {
            panic!("expected a WorkflowRunFinished");
        };
        assert_eq!(run_id.as_deref(), Some("run-42"));
    }

    /// Issue #383: a run an operator stopped records `cancelled` and **no
    /// error**, and the boot sweep leaves it alone because it settled properly.
    ///
    /// The three terminal readings are asserted against each other on purpose:
    /// a cancelled run must not be confusable with a failed one (which carries
    /// an error) or with an interrupted one (which carries the sweep's
    /// synthetic error). Collapsing any pair would put a deliberate stop in the
    /// failure count.
    #[tokio::test]
    async fn a_cancelled_run_records_cancelled_with_no_error() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        let mut run = run_with(Vec::new(), Vec::new());
        run.cancelled = true;
        start(&events, &company, "run-stopped", false).await;

        record_run_finished(&events, &company, "digest", false, "run-stopped", Ok(&run)).await;
        // The sweep must find nothing: the run is settled, not open.
        sweep_interrupted_runs(&events, &company).await;

        let journal = journaled(&events, &company).await;
        assert_eq!(
            journal.len(),
            2,
            "the sweep appended nothing to an already-settled run"
        );
        let CompanyEvent::WorkflowRunFinished {
            cancelled, error, ..
        } = &journal[1]
        else {
            panic!("expected a WorkflowRunFinished, got {:?}", journal[1]);
        };
        assert!(cancelled);
        assert!(
            error.is_none(),
            "a stop is not a failure, so it carries no error: {error:?}"
        );
    }

    /// The other two readings, for contrast: a failure carries an error and is
    /// not cancelled, and the sweep's interrupted row is likewise not cancelled
    /// — nobody stopped it, the host went away.
    #[tokio::test]
    async fn failed_and_interrupted_runs_are_never_flagged_cancelled() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");

        record_run_finished(
            &events,
            &company,
            "digest",
            false,
            "run-bad",
            Err("it broke".into()),
        )
        .await;
        start(&events, &company, "run-dead", false).await;
        sweep_interrupted_runs(&events, &company).await;

        let settled: Vec<(Option<String>, bool)> = journaled(&events, &company)
            .await
            .into_iter()
            .filter_map(|e| match e {
                CompanyEvent::WorkflowRunFinished {
                    error, cancelled, ..
                } => Some((error, cancelled)),
                _ => None,
            })
            .collect();
        assert_eq!(
            settled,
            vec![
                (Some("it broke".to_string()), false),
                (Some(INTERRUPTED_BY_RESTART.to_string()), false),
            ],
            "neither a failure nor a host restart may read as an operator stop"
        );
    }

    /// Appends a `WorkflowRunStarted` for `run_id`.
    async fn start(events: &Arc<dyn EventLog>, company: &CompanyId, run_id: &str, scheduled: bool) {
        events
            .append(
                company,
                CompanyEvent::WorkflowRunStarted {
                    workflow_id: "digest".to_string(),
                    run_id: run_id.to_string(),
                    scheduled,
                    started_by: None,
                },
            )
            .await
            .expect("append");
    }

    /// The case the sweep exists for: a host died mid-run, leaving a start with
    /// no finish. Boot settles it — with the *start's* own workflow id and
    /// scheduled flag, and the same run id, so the console can still group the
    /// nodes that did complete under it.
    #[tokio::test]
    async fn an_interrupted_run_is_settled_at_boot() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-dead", true).await;
        // One node got through before the host went away — the whole point of
        // the record is that this survives.
        events
            .append(
                &company,
                CompanyEvent::WorkflowNodeFinished {
                    workflow_id: "digest".to_string(),
                    run_id: "run-dead".to_string(),
                    node_id: "ceo".to_string(),
                    status: crate::ports::types::WorkflowNodeStatus::Ok,
                    elapsed_ms: 12,
                    diagnostics: Vec::new(),
                    agent_run_id: None,
                },
            )
            .await
            .expect("append");

        sweep_interrupted_runs(&events, &company).await;

        let events = journaled(&events, &company).await;
        assert_eq!(events.len(), 3, "the sweep appends exactly one row");
        let CompanyEvent::WorkflowRunFinished {
            workflow_id,
            scheduled,
            run_id,
            error,
            ..
        } = &events[2]
        else {
            panic!("expected a WorkflowRunFinished, got {:?}", events[2]);
        };
        assert_eq!(workflow_id, "digest");
        assert!(*scheduled, "the flag is carried from the start event");
        assert_eq!(run_id.as_deref(), Some("run-dead"));
        assert_eq!(error.as_deref(), Some(INTERRUPTED_BY_RESTART));
    }

    /// A run that started and finished normally is left alone — otherwise every
    /// boot would append a duplicate, contradictory outcome to healthy history.
    #[tokio::test]
    async fn a_completed_run_is_left_alone() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        let run = run_with(Vec::new(), Vec::new());
        start(&events, &company, "run-ok", false).await;
        record_run_finished(&events, &company, "digest", false, "run-ok", Ok(&run)).await;

        sweep_interrupted_runs(&events, &company).await;

        assert_eq!(
            journaled(&events, &company).await.len(),
            2,
            "the sweep appended nothing"
        );
    }

    /// A journal written before #371 carries finished rows with no run id and no
    /// starts at all. It must sweep to a no-op: those runs are history, not
    /// in-flight work, and stamping them "interrupted" would rewrite the past.
    #[tokio::test]
    async fn a_pre_371_journal_sweeps_to_nothing() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        events
            .append(
                &company,
                CompanyEvent::WorkflowRunFinished {
                    workflow_id: "digest".to_string(),
                    scheduled: true,
                    run_id: None,
                    deliveries: Vec::new(),
                    pending_approvals: Vec::new(),
                    error: None,
                    cancelled: false,
                    notices: Vec::new(),
                    board: Vec::new(),
                    blocked_nodes: Vec::new(),
                    approvals: Vec::new(),
                },
            )
            .await
            .expect("append");

        sweep_interrupted_runs(&events, &company).await;

        assert_eq!(journaled(&events, &company).await.len(), 1);
    }

    /// Several open runs are all settled, and each keeps its own identity — a
    /// scheduled one stays scheduled, a manual one stays manual.
    #[tokio::test]
    async fn every_open_run_is_settled_with_its_own_flags() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-a", true).await;
        start(&events, &company, "run-b", false).await;

        sweep_interrupted_runs(&events, &company).await;

        let settled: Vec<(String, bool)> = journaled(&events, &company)
            .await
            .into_iter()
            .filter_map(|e| match e {
                CompanyEvent::WorkflowRunFinished {
                    run_id, scheduled, ..
                } => Some((run_id.unwrap_or_default(), scheduled)),
                _ => None,
            })
            .collect();
        assert_eq!(
            settled,
            vec![("run-a".to_string(), true), ("run-b".to_string(), false)]
        );
    }

    // --- issue #529: the durable delivery fold --------------------------------

    /// Appends a `WorkflowReportDelivered` — the write-behind record a dispatch
    /// leaves at run time.
    async fn deliver(
        events: &Arc<dyn EventLog>,
        company: &CompanyId,
        workflow_id: &str,
        run_id: &str,
        node: &str,
        kind: &str,
    ) {
        events
            .append(
                company,
                CompanyEvent::WorkflowReportDelivered {
                    workflow_id: workflow_id.to_string(),
                    run_id: run_id.to_string(),
                    node: node.to_string(),
                    kind: kind.to_string(),
                    target: Some("ada@example.com".to_string()),
                },
            )
            .await
            .expect("append");
    }

    /// The nodes the fold returns for `workflow_id`, in order.
    async fn stranded(
        events: &Arc<dyn EventLog>,
        company: &CompanyId,
        workflow_id: &str,
    ) -> Vec<String> {
        delivered_by_unsettled_runs(events, company, workflow_id)
            .await
            .into_iter()
            .map(|d| d.node)
            .collect()
    }

    /// A report delivered by a run that never journaled a finish (a crash) is
    /// owed-nothing to the next run — it is in the ledger.
    #[tokio::test]
    async fn a_delivery_without_a_finish_is_stranded() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;

        let ledger = delivered_by_unsettled_runs(&events, &company, "digest").await;
        assert_eq!(
            ledger,
            vec![DeliveredReport {
                node: "owner_summary".to_string(),
                kind: "owner".to_string(),
            }],
            "a crashed run's delivery must be owed-nothing to the next run"
        );
    }

    /// The cadence guarantee: a run that delivered AND finished cleanly clears
    /// the ledger, so the next scheduled run of the same workflow delivers
    /// again rather than being suppressed forever.
    #[tokio::test]
    async fn a_clean_finish_clears_the_ledger() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;
        record_run_finished(
            &events,
            &company,
            "digest",
            true,
            "run-1",
            Ok(&run_with(Vec::new(), Vec::new())),
        )
        .await;

        assert!(
            stranded(&events, &company, "digest").await.is_empty(),
            "a clean finish absorbs what the run delivered"
        );
    }

    /// A failed finish carries an error and does NOT clear — its run's stranded
    /// delivery stays owed-nothing to the next run.
    #[tokio::test]
    async fn a_failed_finish_keeps_the_ledger() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;
        record_run_finished(
            &events,
            &company,
            "digest",
            true,
            "run-1",
            Err("it broke".into()),
        )
        .await;

        assert_eq!(
            stranded(&events, &company, "digest").await,
            vec!["owner_summary".to_string()],
            "a finish that failed must not clear the delivered ledger"
        );
    }

    /// The boot sweep's synthetic INTERRUPTED finish is a failed finish
    /// (`error: Some`), so it likewise keeps the ledger — a host that died
    /// mid-run still owes the next run nothing for what it managed to send.
    #[tokio::test]
    async fn a_swept_interrupted_finish_keeps_the_ledger() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;
        // The host went away before finishing; boot settles it synthetically.
        sweep_interrupted_runs(&events, &company).await;

        assert_eq!(
            stranded(&events, &company, "digest").await,
            vec!["owner_summary".to_string()],
            "the sweep's synthetic error must not read as a clean finish"
        );
    }

    /// A cancelled run carries `error: None`, so it clears the ledger — and that
    /// is correct: a cancelled run returns before `deliver_outputs`, so it
    /// dispatched nothing here, and clearing simply resets to owed-nothing.
    #[tokio::test]
    async fn a_cancelled_finish_clears_the_ledger() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        // A prior crashed run stranded a delivery…
        start(&events, &company, "run-1", true).await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;
        // …then a later run of the same workflow was stopped by an operator.
        let mut cancelled = run_with(Vec::new(), Vec::new());
        cancelled.cancelled = true;
        record_run_finished(&events, &company, "digest", true, "run-2", Ok(&cancelled)).await;

        assert!(
            stranded(&events, &company, "digest").await.is_empty(),
            "a cancelled run has no error, so it clears like any clean finish"
        );
    }

    /// The multi-crash union: run 1 sends A then crashes, run 2 skips A, sends B,
    /// then crashes. The ledger accumulates {A, B} across both unclean runs.
    #[tokio::test]
    async fn deliveries_union_across_multiple_crashes() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        deliver(&events, &company, "digest", "run-1", "a", "owner").await;
        record_run_finished(
            &events,
            &company,
            "digest",
            true,
            "run-1",
            Err("crash 1".into()),
        )
        .await;

        start(&events, &company, "run-2", true).await;
        // run 2 skipped A (its own already_delivered saw it) and sent B.
        deliver(&events, &company, "digest", "run-2", "b", "owner").await;
        record_run_finished(
            &events,
            &company,
            "digest",
            true,
            "run-2",
            Err("crash 2".into()),
        )
        .await;

        assert_eq!(
            stranded(&events, &company, "digest").await,
            vec!["a".to_string(), "b".to_string()],
            "two unclean runs accumulate rather than the second forgetting the first"
        );
    }

    /// The same node delivered twice (an `owner` fan-out writes one line per
    /// admin) collapses to a single ledger entry, keyed on the node.
    #[tokio::test]
    async fn a_fanned_out_node_collapses_to_one_entry() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;

        assert_eq!(
            stranded(&events, &company, "digest").await,
            vec!["owner_summary".to_string()],
            "per-recipient lines dedupe to one node in the ledger"
        );
    }

    /// The fold is per-workflow: a crash in workflow A does not strand a
    /// delivery against workflow B, and a clean finish of A does not clear B.
    #[tokio::test]
    async fn the_fold_is_isolated_per_workflow() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        deliver(&events, &company, "digest", "run-1", "a", "owner").await;
        deliver(&events, &company, "weekly", "run-2", "b", "owner").await;
        // A clean finish of `digest` clears only `digest`.
        record_run_finished(
            &events,
            &company,
            "digest",
            true,
            "run-1",
            Ok(&run_with(Vec::new(), Vec::new())),
        )
        .await;

        assert!(
            stranded(&events, &company, "digest").await.is_empty(),
            "digest's own clean finish cleared it"
        );
        assert_eq!(
            stranded(&events, &company, "weekly").await,
            vec!["b".to_string()],
            "weekly's stranded delivery is untouched by digest's finish"
        );
    }

    /// Events the fold does not care about — starts, node finishes, unrelated
    /// records — are ignored rather than disturbing the ledger.
    #[tokio::test]
    async fn unrelated_events_are_ignored() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        start(&events, &company, "run-1", true).await;
        events
            .append(
                &company,
                CompanyEvent::WorkflowNodeFinished {
                    workflow_id: "digest".to_string(),
                    run_id: "run-1".to_string(),
                    node_id: "ceo".to_string(),
                    status: crate::ports::types::WorkflowNodeStatus::Ok,
                    elapsed_ms: 3,
                    diagnostics: Vec::new(),
                    agent_run_id: None,
                },
            )
            .await
            .expect("append");
        deliver(
            &events,
            &company,
            "digest",
            "run-1",
            "owner_summary",
            "owner",
        )
        .await;

        assert_eq!(
            stranded(&events, &company, "digest").await,
            vec!["owner_summary".to_string()],
            "only delivered/finished events for this workflow move the ledger"
        );
    }

    /// An empty or delivery-free journal folds to nothing rather than erroring.
    #[tokio::test]
    async fn an_empty_journal_folds_to_nothing() {
        let (_home, events) = log();
        let company = CompanyId::new("acme");
        assert!(stranded(&events, &company, "digest").await.is_empty());
    }

    /// An [`EventLog`] whose `append` always fails — the store went away
    /// mid-run. Issue #1009 uses it to prove the swallowed-append path reports
    /// its loss instead of only warning.
    struct FailingAppendLog;

    #[async_trait::async_trait]
    impl EventLog for FailingAppendLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            Err(crate::error::OpenCompanyError::Store(
                "append failed (test double)".to_string(),
            ))
        }

        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<crate::ports::types::StoredEvent>> {
            Ok(Vec::new())
        }

        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> futures::stream::BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    /// Issue #1009 (path B): a finish whose append is swallowed reports
    /// `journaled == false` rather than the silent `()` it used to. The run is
    /// otherwise unaffected — `record_run_finished` must not panic and must not
    /// propagate the append error — and a working log reports `true`, so the
    /// signal the caller escalates on is real both ways.
    #[tokio::test]
    async fn a_swallowed_append_reports_it_was_not_journaled() {
        let company = CompanyId::new("acme");
        let run = run_with(Vec::new(), Vec::new());

        let failing: Arc<dyn EventLog> = Arc::new(FailingAppendLog);
        let journaled =
            record_run_finished(&failing, &company, "digest", false, "run-1", Ok(&run)).await;
        assert!(
            !journaled,
            "a swallowed append reports the finish did not land"
        );

        let (_home, working) = log();
        let journaled =
            record_run_finished(&working, &company, "digest", false, "run-1", Ok(&run)).await;
        assert!(journaled, "a working log reports the finish landed");
    }
}
