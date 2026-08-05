//! Issue #383: the live set of workflow runs an operator can still stop.
//!
//! A run used to be reachable only as an in-flight HTTP request. Nothing held a
//! handle to it, so there was nowhere to send "stop" — which is why the issue is
//! *both* halves at once: a run has to be addressable while it is running before
//! it can be cancellable at all.
//!
//! [`RunSupervisor`] is that address book. [`begin`](RunSupervisor::begin) mints
//! the run's [`WorkflowRunContext`] — the same context, carrying the same
//! `run_id` issue #371 already correlates progress events on, so **no second
//! identifier is introduced** — and registers its [`RunCancel`] handle;
//! [`cancel`](RunSupervisor::cancel) fires that handle;
//! [`RunGuard`]'s `Drop` deregisters the entry on every exit path.
//!
//! ## What it holds, and what it deliberately does not
//!
//! It holds **cancel handles, not [`JoinHandle`](tokio::task::JoinHandle)s**. A
//! run settles itself and its guard reaps the entry; the supervisor never needs
//! to abort a task, and holding join handles would mean deciding who reaps a run
//! nobody is waiting on. The shape is the one
//! [`InflightRegistry`](crate::company::steer::InflightRegistry) already uses
//! for steerable turns, for the same RAII reason.
//!
//! It is also **not** how a host that dies mid-run is cleaned up. That is
//! already solved: a dead host's runs are settled at the next boot by
//! [`sweep_interrupted_runs`](super::sweep_interrupted_runs), which needs no
//! help from an in-memory map (and could not get any — the map dies with the
//! process). This module adds no sweep of its own.
//!
//! ## Two known gaps, both inherited rather than introduced
//!
//! * A **panicking** run task unwinds, so its guard drops and the entry goes —
//!   but nothing journals a `WorkflowRunFinished`, so the run reads
//!   `running: true` in `GET …/workflows/runs` until the next restart sweeps it.
//!   That is the same exposure #371 accepted for a run whose journal append
//!   failed, and the same remedy settles both.
//! * A live runtime swap ([`rebuild_company`](super::rebuild_company)) gives the
//!   successor runtime a fresh supervisor, so a run registered on the old one
//!   can no longer be cancelled (it still finishes and still journals). This
//!   matches how the steer registry behaves across a rebuild — see
//!   [`RuntimeHandover`](super::RuntimeHandover) — and cancelling is a
//!   best-effort operator convenience, not a correctness guarantee.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::ports::{RunCancel, WorkflowRunContext};

/// One registered run: its stop signal, plus the graph it belongs to for the log
/// line the cancel route emits.
struct Slot {
    workflow_id: String,
    cancel: RunCancel,
}

/// The live set of cancellable workflow runs, keyed by run id.
///
/// One per [`CompanyRuntime`](crate::company::runtime::CompanyRuntime), so the
/// key needs no company component — every reader already resolved a company
/// before it got here. Cheap to [`Clone`] (a shared handle): the same map is
/// seen by the HTTP run route that registers runs, the cancel route that fires
/// them, the cron scheduler, and the orchestrator's `run_workflow` tool.
#[derive(Clone, Default)]
pub struct RunSupervisor {
    inner: Arc<Mutex<HashMap<String, Slot>>>,
}

impl RunSupervisor {
    /// An empty supervisor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a run context and registers its stop signal.
    ///
    /// `workflow_id` is the graph about to run and `scheduled` says whether a
    /// cron started it — both ride the context exactly as they did before, so
    /// this is a drop-in for [`WorkflowRunContext::new`] that additionally makes
    /// the run reachable.
    ///
    /// The returned [`RunGuard`] MUST be held for the duration of the run.
    /// Dropping it deregisters the entry, which is what keeps a settled run from
    /// lingering as a cancellable one — and because it is a `Drop`, that holds
    /// on the error and panic paths too.
    pub fn begin(&self, workflow_id: &str, scheduled: bool) -> (WorkflowRunContext, RunGuard) {
        let ctx = WorkflowRunContext::new(scheduled);
        self.inner.lock().expect("run supervisor poisoned").insert(
            ctx.run_id.clone(),
            Slot {
                workflow_id: workflow_id.to_string(),
                cancel: ctx.cancel.clone(),
            },
        );
        let guard = RunGuard {
            supervisor: self.clone(),
            run_id: ctx.run_id.clone(),
        };
        (ctx, guard)
    }

    /// Fires a registered run's stop signal.
    ///
    /// Returns `false` when no run with `run_id` is registered — which covers
    /// both "never existed" and "already settled", and the route answers `404`
    /// for both. They are genuinely the same answer to the operator: there is
    /// nothing here to stop.
    pub fn cancel(&self, run_id: &str) -> bool {
        let guard = self.inner.lock().expect("run supervisor poisoned");
        let Some(slot) = guard.get(run_id) else {
            return false;
        };
        tracing::info!(
            workflow = %slot.workflow_id,
            %run_id,
            "workflow run: an operator asked to stop this run"
        );
        slot.cancel.cancel();
        true
    }

    /// Removes a run's slot (called by [`RunGuard`]'s `Drop`).
    fn deregister(&self, run_id: &str) {
        self.inner
            .lock()
            .expect("run supervisor poisoned")
            .remove(run_id);
    }

    /// How many runs are currently registered. For tests and diagnostics.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("run supervisor poisoned").len()
    }

    /// Whether no run is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An RAII guard that deregisters a run when dropped.
///
/// Held for the whole run — through the runner call *and* the
/// [`record_run_finished`](super::record_run_finished) that follows it — so a
/// run stays cancellable right up to the moment it settles, and stops being
/// cancellable the moment it does.
pub struct RunGuard {
    supervisor: RunSupervisor,
    run_id: String,
}

impl RunGuard {
    /// The run this guard keeps registered.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.supervisor.deregister(&self.run_id);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// The core loop: a begun run is registered under the id its context
    /// carries, cancelling fires the signal that context holds, and the guard
    /// takes the entry away.
    #[test]
    fn begin_registers_cancel_fires_and_the_guard_deregisters() {
        let supervisor = RunSupervisor::new();
        assert!(supervisor.is_empty());

        let (ctx, guard) = supervisor.begin("digest", false);
        assert_eq!(supervisor.len(), 1);
        assert_eq!(guard.run_id(), ctx.run_id);
        assert!(!ctx.cancel.is_cancelled());

        assert!(supervisor.cancel(&ctx.run_id), "a live run cancels");
        assert!(
            ctx.cancel.is_cancelled(),
            "the signal the runner is selecting on is the one the supervisor fired"
        );

        drop(guard);
        assert!(supervisor.is_empty());
    }

    /// The two 404 cases, which are one case: nothing to stop. A settled run is
    /// indistinguishable from one that never existed, and deliberately so —
    /// keeping a tombstone would mean deciding when to expire it.
    #[test]
    fn cancelling_an_unknown_or_settled_run_reports_false() {
        let supervisor = RunSupervisor::new();
        assert!(!supervisor.cancel("never-existed"));

        let (ctx, guard) = supervisor.begin("digest", false);
        drop(guard);
        assert!(
            !supervisor.cancel(&ctx.run_id),
            "a settled run is no longer cancellable"
        );
    }

    /// The guard deregisters on an **unwind**, not just on a clean return. This
    /// is the case a manual `deregister()` call at the end of the run body would
    /// get wrong, and it is why this is a `Drop` type: a panicking run task must
    /// not leave a permanently-cancellable ghost behind.
    #[test]
    fn the_guard_deregisters_when_the_run_panics() {
        let supervisor = RunSupervisor::new();
        let outer = supervisor.clone();
        let result = std::panic::catch_unwind(move || {
            let (_ctx, _guard) = outer.begin("digest", false);
            assert_eq!(outer.len(), 1);
            panic!("the run blew up");
        });
        assert!(result.is_err(), "the panic really happened");
        assert!(
            supervisor.is_empty(),
            "the guard unwound and took the entry with it"
        );
    }

    /// Two runs of the same graph coexist and are cancelled independently. The
    /// host places no cap on concurrent runs of one workflow (the console keeps
    /// its own per-workflow guard), so the map must key on the run rather than
    /// on the graph.
    #[test]
    fn concurrent_runs_of_one_workflow_cancel_independently() {
        let supervisor = RunSupervisor::new();
        let (first, _first_guard) = supervisor.begin("digest", false);
        let (second, _second_guard) = supervisor.begin("digest", true);
        assert_eq!(supervisor.len(), 2);
        assert_ne!(first.run_id, second.run_id);

        assert!(supervisor.cancel(&second.run_id));
        assert!(second.cancel.is_cancelled());
        assert!(
            !first.cancel.is_cancelled(),
            "cancelling one run leaves the other alone"
        );
    }

    /// A cancel that lands *before* anything awaits the signal is still
    /// observed. This is the property the watch channel buys over a `Notify`,
    /// and losing it would make a cancel racing a slow node hang until the run
    /// finished on its own.
    #[tokio::test]
    async fn a_cancel_before_the_await_is_still_seen() {
        let supervisor = RunSupervisor::new();
        let (ctx, _guard) = supervisor.begin("digest", false);
        supervisor.cancel(&ctx.run_id);

        tokio::time::timeout(std::time::Duration::from_secs(1), ctx.cancel.cancelled())
            .await
            .expect("an already-fired signal resolves immediately");
    }

    /// And a cancel that lands *after* the await started wakes it.
    #[tokio::test]
    async fn a_cancel_after_the_await_wakes_it() {
        let supervisor = RunSupervisor::new();
        let (ctx, _guard) = supervisor.begin("digest", false);
        let waiter = tokio::spawn({
            let cancel = ctx.cancel.clone();
            async move { cancel.cancelled().await }
        });
        // Yield so the waiter is definitely parked before the signal fires.
        tokio::task::yield_now().await;
        supervisor.cancel(&ctx.run_id);

        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("the waiter woke")
            .expect("the waiter did not panic");
    }
}
