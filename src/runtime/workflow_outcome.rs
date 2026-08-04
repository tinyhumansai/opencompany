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

use std::sync::Arc;

use crate::ports::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId};
use crate::ports::workflow_runner::{DeliveryReport, WorkflowRun};

/// Journals a finished workflow run, best-effort.
///
/// `scheduled` says whether a cron started the run rather than an operator —
/// the distinction is the point, since a scheduled run is the
/// nobody-was-watching case this record exists for.
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
    outcome: Result<&WorkflowRun, &str>,
) {
    let (deliveries, pending_approvals, error): (Vec<DeliveryReport>, Vec<String>, Option<String>) =
        match outcome {
            Ok(run) => (run.deliveries.clone(), run.pending_approvals.clone(), None),
            Err(err) => (Vec::new(), Vec::new(), Some(err.to_string())),
        };

    let event = CompanyEvent::WorkflowRunFinished {
        workflow_id: workflow_id.to_string(),
        scheduled,
        // Neither entry point mints a run id today. Kept on the event so #242's
        // first-class run — or any future correlated entry point — needs no
        // migration to start populating it.
        run_id: None,
        deliveries,
        pending_approvals,
        error,
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

        record_run_finished(&events, &company, "digest", true, Ok(&run)).await;

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

        record_run_finished(&events, &company, "digest", false, Ok(&run)).await;

        let events = journaled(&events, &company).await;
        let CompanyEvent::WorkflowRunFinished { scheduled, .. } = &events[0] else {
            panic!("expected a WorkflowRunFinished");
        };
        assert!(!*scheduled);
    }
}
