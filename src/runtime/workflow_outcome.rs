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
use crate::ports::workflow_runner::{DeliveryReport, WorkflowRun};

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
pub async fn record_run_finished(
    events: &Arc<dyn EventLog>,
    company: &CompanyId,
    workflow_id: &str,
    scheduled: bool,
    run_id: &str,
    outcome: Result<&WorkflowRun, &str>,
) {
    // Issue #383: `cancelled` rides the Ok arm only, and that is not an
    // oversight. A run the runner never returned from cannot have been stopped
    // by an operator — the stop signal resolves *into* an `Ok(cancelled)`, never
    // into an `Err` — so the error arm is unambiguously a failure or the boot
    // sweep's synthetic one.
    let (deliveries, pending_approvals, error, cancelled): (
        Vec<DeliveryReport>,
        Vec<String>,
        Option<String>,
        bool,
    ) = match outcome {
        Ok(run) => (
            run.deliveries.clone(),
            run.pending_approvals.clone(),
            None,
            run.cancelled,
        ),
        Err(err) => (Vec::new(), Vec::new(), Some(err.to_string()), false),
    };

    let event = CompanyEvent::WorkflowRunFinished {
        workflow_id: workflow_id.to_string(),
        scheduled,
        // Issue #371 started populating this reserved field. The event's own
        // wire shape is unchanged — it has always carried an optional `run_id` —
        // so a reader predating #371 still decodes every line it could before.
        run_id: Some(run_id.to_string()),
        deliveries,
        pending_approvals,
        error,
        cancelled,
    };

    if let Err(err) = events.append(company, event).await {
        // Swallowed on purpose: the run already happened and its work is valid.
        // Losing the record is worth a loud line, never a failed run.
        tracing::warn!(
            %company,
            workflow = %workflow_id,
            scheduled,
            %err,
            "workflow run outcome could not be journaled; the run itself was unaffected"
        );
    }
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
            Err(INTERRUPTED_BY_RESTART),
        )
        .await;
    }
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
        }
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
            Err("agent node `worker` had no inference source"),
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
            Err("it broke"),
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
}
