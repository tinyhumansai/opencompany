//! Graceful shutdown for the host (issue #986).
//!
//! A hosted tenant is a pod. Every deploy, every `refresh_all_tenants` run and
//! every eviction sends it `SIGTERM`, and until this module existed nothing in
//! the process handled that signal — so the default disposition applied and the
//! host died, mid-turn. Turns are long (measured well past fifteen minutes on
//! staging), so that reliably meant "in the middle of one". In a container the
//! entrypoint `exec`s `opencompany`, making it PID 1 — where the kernel *drops*
//! a default-disposition `SIGTERM` rather than delivering it — so the pod sat
//! idle until the kubelet's `SIGKILL` at the end of its grace period, and even
//! a turn that finished in the remaining seconds of that window was cut off
//! before it could be saved. The journal was left holding a question with no
//! answer.
//!
//! The two halves here are the ones the signal needs and neither is sufficient
//! alone:
//!
//! 1. [`signal`] — a future that resolves on `SIGTERM` (and `SIGINT`, so a local
//!    `Ctrl-C` takes the same path a rollout does). Registering a handler at all
//!    is what stops the kernel's default kill, which is why an unset
//!    `terminationGracePeriodSeconds` bought nothing before: a grace period is a
//!    window to *handle* the signal in, and nothing was handling it.
//! 2. [`drain`] — quiesce every registered company, then wait for the cycle each
//!    one has in flight, bounded.
//!
//! ## Why the drain cannot be axum's connection drain
//!
//! `with_graceful_shutdown` waits for in-flight *connections*. That would have
//! been enough if a turn lived inside its request future — but issue #383
//! deliberately moved it off, precisely so a client walking away could not take
//! the agent's continuation with it. Dispatches, scheduled workflow runs and
//! approval follow-ups all run on detached `tokio::spawn`s. So at the moment
//! `SIGTERM` arrives the connection set can be empty while several turns are
//! very much running, and a connection drain would report "nothing in flight"
//! and exit.
//!
//! [`CompanyRuntime::quiesce`](crate::company::runtime::CompanyRuntime::quiesce)
//! is the primitive that does see them. It was built for the rebuild swap
//! (issue #290) and answers exactly the question shutdown asks: stop accepting
//! new cycles, then prove the one in flight has finished by acquiring the
//! per-company `serial` lock every cycle holds for its whole duration. Detached
//! work is covered because it takes that same lock — it either completed before
//! the flag was set or it is the turn being waited on.
//!
//! ## Why the bound is honest rather than generous
//!
//! A turn can outlast any grace period a rollout is willing to wait through, so
//! this reduces how often work is killed; it does not eliminate it. The
//! failed-run record from issue #983 stage 1 stays the backstop for the turns
//! that still get cut off.
//!
//! ## What this must not touch
//!
//! `/healthz`. The manager's wake-on-request proxy blocks on that endpoint and
//! gives up after its startup timeout, so nothing here may slow boot. Nothing
//! here runs before the signal, and the signal only ever arrives at the end of a
//! pod's life.

use std::time::Duration;

use crate::AppState;

/// Overrides how long [`drain`] waits for in-flight turns, in seconds.
///
/// `0` disables the wait entirely (quiesce, then exit immediately), which is a
/// deliberate escape hatch for an operator who wants the old behaviour back
/// without reverting.
pub const GRACE_ENV: &str = "OPENCOMPANY_SHUTDOWN_GRACE_SECONDS";

/// How long [`drain`] waits for in-flight turns when [`GRACE_ENV`] is unset.
///
/// Sized to fit inside Kubernetes' *default* 30s `terminationGracePeriodSeconds`
/// together with [`CONNECTION_GRACE`], so a tenant running this build gets a
/// real, complete drain even on a pod spec that has not been updated to name a
/// grace period of its own. Raising this past the pod's grace period does not
/// buy a longer drain — it buys a `SIGKILL` in the middle of one — so the two
/// are meant to move together.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(25);

/// The extra window the server gets, after the drain, to finish writing
/// responses on connections that are still open.
///
/// Small on purpose. By the time the drain returns, the work those connections
/// were waiting on is either done or already past its bound; this is for the
/// bytes, not for the turn. It is also what keeps a long-lived stream — the
/// console's event stream never ends on its own — from holding the process open
/// past the pod's grace period and turning a clean exit into a `SIGKILL`.
pub const CONNECTION_GRACE: Duration = Duration::from_secs(2);

/// The last window, after the server has stopped serving, for analytics to get
/// its final batch out (issue #1739).
///
/// **Bounded because the budget above is the whole point.** [`DEFAULT_GRACE`]
/// and [`CONNECTION_GRACE`] are sized to land at 27s, deliberately under
/// Kubernetes' default 30s `terminationGracePeriodSeconds`. The flush is a
/// network call to a collector this process does not control, and its client
/// timeout is 5s — so an unbounded flush during a rollout with a slow collector
/// took the worst case to 32s and invited the `SIGKILL` the 27s exists to
/// avoid, losing the drain rather than the telemetry.
///
/// 2s keeps the total at 29s. Telemetry is the right thing to give up here: a
/// dropped batch costs a boot line in a dashboard, while an overrun costs a
/// half-finished turn. An operator who raises [`GRACE_ENV`] past the pod's
/// grace period has already left this budget behind, and this bound does not
/// grow with it.
pub const FLUSH_BUDGET: Duration = Duration::from_secs(2);

/// Kubernetes' default `terminationGracePeriodSeconds`, which every budget in
/// this module is sized against. Named rather than left in prose, so the
/// arithmetic can be asserted.
pub const POD_DEFAULT_GRACE: Duration = Duration::from_secs(30);

/// How long the analytics flush actually gets, given the configured drain.
///
/// [`FLUSH_BUDGET`] is a **ceiling, not an allowance**. Added flat to a
/// configurable drain it re-created the problem it was added to fix: with
/// `OPENCOMPANY_SHUTDOWN_GRACE_SECONDS=28`, drain plus connection grace fit in
/// 30s exactly, and a flat two seconds on top took it to 32 — the same
/// mid-shutdown `SIGKILL`, for a value the operator had every reason to think
/// was safe.
///
/// So it is derived from what is left: whatever remains of the pod's default
/// grace after the drain and the connection window, capped at [`FLUSH_BUDGET`].
/// A drain that already fills the budget leaves zero, and the flush is skipped.
///
/// Telemetry is the right thing to give way — a dropped batch costs a line in a
/// dashboard, an overrun costs a half-finished turn — and that applies just as
/// much to an operator who raised the drain deliberately: this cannot know what
/// their pod's grace period actually is, so it declines to spend seconds it
/// cannot prove are there.
pub fn flush_budget(drain: Duration) -> Duration {
    POD_DEFAULT_GRACE
        .saturating_sub(drain.saturating_add(CONNECTION_GRACE))
        .min(FLUSH_BUDGET)
}

/// The drain bound, read from [`GRACE_ENV`] and falling back to
/// [`DEFAULT_GRACE`].
///
/// A malformed value falls back rather than failing: this is read while the
/// process is already on its way out, and refusing to shut down over a typo in
/// an environment variable would be a worse outcome than the wrong bound.
pub fn grace_from_env() -> Duration {
    parse_grace(std::env::var(GRACE_ENV).ok().as_deref())
}

/// The parse behind [`grace_from_env`], split out so it can be tested without
/// mutating process environment — which no test can do safely in a binary whose
/// other tests run on the same process.
fn parse_grace(raw: Option<&str>) -> Duration {
    let Some(raw) = raw else {
        return DEFAULT_GRACE;
    };
    match raw.trim().parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => {
            tracing::warn!(
                "{GRACE_ENV}=`{raw}` is not a whole number of seconds; using the {}s default",
                DEFAULT_GRACE.as_secs()
            );
            DEFAULT_GRACE
        }
    }
}

/// Resolves on the first termination signal.
///
/// `SIGTERM` is the one a rollout, a refresh and an eviction all send.
/// `SIGINT` is included so a developer's `Ctrl-C` exercises the same path in
/// development that production takes — a shutdown path that only ever runs in
/// production is a shutdown path nobody has watched work.
#[cfg(unix)]
pub async fn signal() {
    use tokio::signal::unix::{SignalKind, signal};

    // Registering both is what displaces the kernel's default kill. If either
    // registration fails we still want the other, and if both fail we fall back
    // to never resolving: an unhandled signal then kills the process exactly as
    // it did before this module, which is the honest degradation.
    let mut term = signal(SignalKind::terminate()).ok();
    let mut interrupt = signal(SignalKind::interrupt()).ok();
    if term.is_none() && interrupt.is_none() {
        tracing::error!("could not install signal handlers; shutdown will not be graceful");
        std::future::pending::<()>().await;
    }
    let terminated = async {
        match term.as_mut() {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    };
    let interrupted = async {
        match interrupt.as_mut() {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = terminated => tracing::info!("received SIGTERM"),
        () = interrupted => tracing::info!("received SIGINT"),
    }
}

/// Resolves on the first termination signal.
///
/// Off Unix there is no `SIGTERM`; `Ctrl-C` is the whole of it.
#[cfg(not(unix))]
pub async fn signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("received Ctrl-C");
    } else {
        tracing::error!("could not install a Ctrl-C handler; shutdown will not be graceful");
        std::future::pending::<()>().await;
    }
}

/// Arms a listener for a *second* termination signal and hard-exits on it.
///
/// The first signal replaces the kernel's default disposition and resolves
/// [`signal`], so a `SIGTERM`/`SIGINT` that arrives after that (while the drain
/// is running) is otherwise swallowed for the rest of the process's life. On an
/// idle host the process is gone a couple of seconds after the first signal and
/// this never fires; it exists for the other half — a long drain, where the
/// developer who pressed Ctrl-C once and watched the 25s ceiling start has no
/// way to change their mind. A second press is the escape hatch. `130` is the
/// conventional "terminated by Ctrl-C" exit code.
///
/// Deliberately a one-way, process-level action: there is no graceful recovery
/// from having been told to stop twice.
#[cfg(unix)]
pub fn arm_force_exit_on_second_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).ok();
        let mut interrupt = signal(SignalKind::interrupt()).ok();
        let terminated = async {
            match term.as_mut() {
                Some(s) => {
                    let _ = s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let interrupted = async {
            match interrupt.as_mut() {
                Some(s) => {
                    let _ = s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = terminated => {}
            () = interrupted => {}
        }
        tracing::warn!("received a second termination signal; exiting immediately");
        std::process::exit(130);
    });
}

/// Arms a listener for a second `Ctrl-C` and hard-exits on it.
#[cfg(not(unix))]
pub fn arm_force_exit_on_second_signal() {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::warn!("received a second Ctrl-C; exiting immediately");
        std::process::exit(130);
    });
}

/// Stops every registered company accepting new cycles, then waits for the ones
/// in flight to settle, for at most `grace`.
///
/// Returns whether the wait completed — `false` means at least one turn was
/// still running when the bound expired and is about to be cut off. That is a
/// real outcome, not an error: see the module docs on why the bound is shorter
/// than the longest turn.
///
/// The companies are drained concurrently. Serially, a single busy company would
/// spend the whole bound and leave every other company's turn to be killed for
/// no reason — and a tenant can hold more than one.
pub async fn drain(state: &AppState, grace: Duration) -> bool {
    let registry = state.registry();
    // Before the snapshot, deliberately. The host keeps serving through the
    // drain, so `POST /api/v1/companies` can register a company while this
    // runs; without this it would land after the snapshot, never be quiesced,
    // and be free to start a turn nothing is waiting for. Setting the flag
    // first makes every later registration born quiesced, so a company is
    // either in the snapshot below or unable to run a cycle at all.
    registry.begin_shutdown();
    let runtimes: Vec<_> = registry
        .list()
        .iter()
        .filter_map(|id| registry.get(id))
        .collect();
    if runtimes.is_empty() {
        return true;
    }

    let count = runtimes.len();
    tracing::info!(
        "draining {count} compan{}",
        if count == 1 { "y" } else { "ies" }
    );
    let drained = futures::future::join_all(runtimes.iter().map(|runtime| runtime.quiesce()));
    match tokio::time::timeout(grace, drained).await {
        Ok(_) => {
            tracing::info!("all in-flight turns settled; shutting down");
            true
        }
        Err(_) => {
            // Named so the operator reading the pod's last lines can tell this
            // apart from the silent kill it replaces, and can tell that raising
            // the bound is the knob that would have helped.
            tracing::warn!(
                "a turn was still running after {}s ({GRACE_ENV}); \
                 shutting down anyway — it will be reaped as failed on the next boot",
                grace.as_secs()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {

    /// The three shutdown budgets must still fit inside a pod's default grace.
    ///
    /// Kubernetes' default `terminationGracePeriodSeconds` is 30, and every one
    /// of these constants is sized against it — but the arithmetic lived only in
    /// prose, so issue #1739's flush was added on the end and took the worst
    /// case to 32s without anything complaining. A `SIGKILL` mid-drain is a
    /// half-finished turn, which is worse than anything the extra window buys.
    ///
    /// This fails on the next constant raised in isolation. Raise the pod's
    /// grace period deliberately, together, if the budget really has to grow.
    #[test]
    fn the_shutdown_budgets_fit_inside_a_pods_default_grace() {
        // Every drain an operator can configure, not just the default: a flat
        // flush on top of a configurable drain is exactly how 28s became 32s.
        for secs in 0..=40u64 {
            let drain = Duration::from_secs(secs);
            let total = drain + super::CONNECTION_GRACE + super::flush_budget(drain);
            if drain + super::CONNECTION_GRACE > super::POD_DEFAULT_GRACE {
                // Already past the budget on its own — the flush must not make
                // it worse, and cannot make it better.
                assert_eq!(
                    super::flush_budget(drain),
                    Duration::ZERO,
                    "a drain of {secs}s already fills the budget; the flush must take nothing"
                );
                continue;
            }
            assert!(
                total <= super::POD_DEFAULT_GRACE,
                "drain {secs}s + connections {}s + flush {}s = {}s, past the {}s default",
                super::CONNECTION_GRACE.as_secs(),
                super::flush_budget(drain).as_secs(),
                total.as_secs(),
                super::POD_DEFAULT_GRACE.as_secs(),
            );
        }
        // And the default drain still gets the full ceiling — a budget derived
        // down to nothing would be a silent retirement of the flush.
        assert_eq!(
            super::flush_budget(super::DEFAULT_GRACE),
            super::FLUSH_BUDGET,
            "the default drain must still afford the whole flush budget"
        );
    }
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{DEFAULT_GRACE, drain, parse_grace};
    use crate::company::CompanyManifest;
    use crate::ports::types::{
        Actor, ActorKind, CompanyEvent, CompanyId, CompressedTrace, CycleRequest, CycleResult,
        TokenUsage,
    };
    use crate::ports::{Brain, CycleHost};
    use crate::runtime::RuntimeBuilder;
    use crate::{AppConfig, AppState, Result};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-shutdown-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest(name: &str) -> CompanyManifest {
        toml::from_str(&format!(
            "[company]\nname = \"{name}\"\n[policy]\nmode = \"full\"\n"
        ))
        .expect("parse manifest")
    }

    fn operator_message(text: &str) -> CompanyEvent {
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            text: text.into(),
            by: Some(Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            }),
            chat: None,
            parent: None,
            deliverable: None,
            attachments: Vec::new(),
        }
    }

    /// A brain that parks inside its cycle until released, so a test can deliver
    /// a shutdown while a turn is provably in flight — the situation the whole
    /// module exists for.
    struct BlockingBrain {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Brain for BlockingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            self.entered.notify_waiters();
            self.release.notified().await;
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "blocking")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A company whose next turn parks until `release` is notified.
    struct StalledCompany {
        state: AppState,
        id: CompanyId,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn stalled_company(home: &std::path::Path) -> StalledCompany {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let manifest = manifest("Acme");
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(CompanyId::new("acme"))
            .with_brain(Arc::new(BlockingBrain {
                entered: entered.clone(),
                release: release.clone(),
                finished: finished.clone(),
            }))
            .build()
            .await
            .expect("build a runtime");
        let id = CompanyId::new("acme");
        let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
        state.registry().insert(id.clone(), Arc::new(runtime));
        StalledCompany {
            state,
            id,
            entered,
            release,
            finished,
        }
    }

    /// Starts a turn the way production does — on a detached task holding its own
    /// `Arc<CompanyRuntime>`, with nothing awaiting it — and returns once the
    /// brain is provably inside the cycle.
    async fn start_detached_turn(c: &StalledCompany) -> tokio::task::JoinHandle<()> {
        let runtime = c
            .state
            .registry()
            .get(&c.id)
            .expect("the company is registered");
        let entered = c.entered.notified();
        tokio::pin!(entered);
        // Arm the waiter before the spawn so the notification cannot be missed.
        let handle = tokio::spawn(async move {
            let _ = runtime.run_cycle(vec![operator_message("do it")]).await;
        });
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .expect("the turn started");
        handle
    }

    #[test]
    fn an_unset_grace_uses_the_default() {
        assert_eq!(parse_grace(None), DEFAULT_GRACE);
    }

    #[test]
    fn a_grace_of_zero_is_honoured_rather_than_treated_as_unset() {
        // The escape hatch back to pre-#986 behaviour. Folding `0` into the
        // default would make it impossible to ask for no wait at all.
        assert_eq!(parse_grace(Some("0")), Duration::ZERO);
        assert_eq!(parse_grace(Some(" 90 ")), Duration::from_secs(90));
    }

    #[test]
    fn a_malformed_grace_falls_back_instead_of_failing() {
        // Read while the process is already on its way out: refusing to shut
        // down over a typo would be worse than the wrong bound.
        assert_eq!(parse_grace(Some("thirty")), DEFAULT_GRACE);
        assert_eq!(parse_grace(Some("")), DEFAULT_GRACE);
        assert_eq!(parse_grace(Some("-1")), DEFAULT_GRACE);
    }

    #[tokio::test]
    async fn a_host_with_no_companies_drains_immediately() {
        let dir = home();
        let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
        assert!(drain(&state, Duration::from_secs(30)).await);
    }

    #[tokio::test]
    async fn a_drain_stops_every_company_accepting_new_work() {
        let dir = home();
        let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
        for name in ["Acme", "Globex"] {
            let id = CompanyId::new(name.to_lowercase());
            let runtime = RuntimeBuilder::new(dir.path().join(name), manifest(name))
                .with_id(id.clone())
                .build()
                .await
                .expect("build a runtime");
            state.registry().insert(id, Arc::new(runtime));
        }

        assert!(drain(&state, Duration::from_secs(30)).await);
        for id in state.registry().list() {
            assert!(
                state.registry().get(&id).expect("registered").is_quiesced(),
                "`{id}` was left accepting cycles after the drain"
            );
        }
    }

    /// **The keystone.** A turn that is not attached to any request must still
    /// be waited on.
    ///
    /// This is the whole of issue #986's root cause: issue #383 deliberately
    /// moved the agent turn off the request future so a client hanging up could
    /// not cancel it, which means at `SIGTERM` the connection set can be empty
    /// while several turns are running. A connection drain — all
    /// `with_graceful_shutdown` gives on its own — would look at that empty set,
    /// report "nothing in flight" and exit straight through the live turn.
    ///
    /// So the assertion is ordering, not elapsed time: the drain must still be
    /// pending while the detached turn is parked, and must only complete after
    /// the turn does.
    #[tokio::test]
    async fn a_drain_waits_for_a_turn_that_no_request_is_holding() {
        let dir = home();
        let c = stalled_company(dir.path()).await;
        let turn = start_detached_turn(&c).await;

        let mut draining = Box::pin(drain(&c.state, Duration::from_secs(30)));
        tokio::select! {
            _ = &mut draining => panic!(
                "the drain returned through a live turn: shutdown would have killed it"
            ),
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        assert!(
            !c.finished.load(std::sync::atomic::Ordering::SeqCst),
            "the turn is still parked, so the drain had nothing to have waited for"
        );

        c.release.notify_waiters();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), draining)
                .await
                .expect("the drain returned once the turn finished"),
            "a turn that finished inside the bound must report a complete drain"
        );
        assert!(
            c.finished.load(std::sync::atomic::Ordering::SeqCst),
            "the drain returned before the turn completed"
        );
        turn.await.expect("the turn task did not panic");
    }

    /// **The provisioning race.** A company registered *while the drain is
    /// running* must not be able to start a turn.
    ///
    /// The host keeps serving through the drain on purpose, so
    /// `POST /api/v1/companies` stays reachable for the whole window — up to 25
    /// seconds. A company registered after the drain took its snapshot is not in
    /// that snapshot, so nothing waits for it; if it could still accept a cycle
    /// it would start a turn seconds before the process exits and lose it.
    ///
    /// Closed at the registry rather than in the provisioning handler: boot, the
    /// provision route and a rebuild swap all register through `insert`, and a
    /// re-scan after the drain would only move the race to whatever lands after
    /// the last scan.
    #[tokio::test]
    async fn a_company_registered_during_the_drain_cannot_start_a_turn() {
        let dir = home();
        let c = stalled_company(dir.path()).await;
        let turn = start_detached_turn(&c).await;

        // The drain is now pending on the parked turn — the window a provision
        // request would land in.
        let mut draining = Box::pin(drain(&c.state, Duration::from_secs(30)));
        tokio::select! {
            _ = &mut draining => panic!("the drain returned through a live turn"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        let late = CompanyId::new("globex");
        let runtime = RuntimeBuilder::new(dir.path().join("globex"), manifest("Globex"))
            .with_id(late.clone())
            .build()
            .await
            .expect("build a runtime");
        c.state.registry().insert(late.clone(), Arc::new(runtime));

        let registered = c.state.registry().get(&late).expect("registered");
        assert!(
            registered.is_quiesced(),
            "a company registered mid-drain can accept cycles nothing will wait for"
        );
        assert!(
            registered
                .run_cycle(vec![operator_message("late work")])
                .await
                .is_err(),
            "the late company must refuse the cycle, not merely be flagged"
        );

        c.release.notify_waiters();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), draining)
                .await
                .expect("the drain finished")
        );
        turn.await.expect("the turn task did not panic");
    }

    /// The flag is one-way and scoped to shutdown: an ordinary registration is
    /// untouched, or every company would boot refusing work.
    #[tokio::test]
    async fn registering_a_company_before_shutdown_leaves_it_accepting_work() {
        let dir = home();
        let state = AppState::new(AppConfig::default()).with_home(dir.path().to_path_buf());
        assert!(!state.registry().is_shutting_down());
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(dir.path().to_path_buf(), manifest("Acme"))
            .with_id(id.clone())
            .build()
            .await
            .expect("build a runtime");
        state.registry().insert(id.clone(), Arc::new(runtime));
        assert!(!state.registry().get(&id).expect("registered").is_quiesced());
    }

    /// The same proof one level up, over the real server.
    ///
    /// Lives here rather than beside `serve_on_until` because what it asserts is
    /// this module's contract — that the wiring in `routes` actually reaches the
    /// drain — and the stalled-company fixture it needs is right above.
    ///
    /// Before issue #986 the host was a bare `axum::serve(listener, router)`
    /// with no shutdown path at all: `SIGTERM` was unhandled, so this sequence
    /// had no analogue — the process simply stopped existing, mid-turn.
    #[tokio::test]
    async fn the_host_does_not_exit_until_the_in_flight_turn_has_settled() {
        let dir = home();
        let c = stalled_company(dir.path()).await;
        let turn = start_detached_turn(&c).await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let (signal, wait_for_signal) = tokio::sync::oneshot::channel::<()>();
        let mut host = tokio::spawn(crate::server::serve_on_until(
            listener,
            c.state.clone(),
            async move {
                let _ = wait_for_signal.await;
            },
        ));

        signal.send(()).expect("deliver the termination signal");
        tokio::select! {
            _ = &mut host => panic!("the host exited through a live turn"),
            () = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
        assert!(
            !c.finished.load(std::sync::atomic::Ordering::SeqCst),
            "the turn is still parked, so the host had nothing to have waited for"
        );

        c.release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(10), &mut host)
            .await
            .expect("the host exited once the turn settled")
            .expect("the host task did not panic")
            .expect("a graceful shutdown is not an error");
        assert!(
            c.finished.load(std::sync::atomic::Ordering::SeqCst),
            "the host exited before the turn completed"
        );
        turn.await.expect("the turn task did not panic");
    }

    /// The ceiling. An open connection must not hold the pod past its grace
    /// period, because the reward for that is a `SIGKILL` — the exact abrupt
    /// stop this module exists to remove, arriving a few seconds later.
    ///
    /// The connection here is a real one: a chat `POST` runs its cycle *inside*
    /// the request future, so with the brain parked the request is genuinely
    /// in flight and hyper is genuinely waiting on it. The console's event
    /// stream is the case that actually motivates this — it never ends on its
    /// own — but a parked request reproduces the same hold with none of the
    /// streaming scaffolding.
    #[tokio::test]
    async fn an_open_connection_does_not_hold_the_host_past_the_bound() {
        use tokio::io::AsyncWriteExt;

        let dir = home();
        let c = stalled_company(dir.path()).await;
        crate::server::test_support::seed_fixed_admin(&c.state, "acme").await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("a bound address");
        let (signal, wait_for_signal) = tokio::sync::oneshot::channel::<()>();
        let host = tokio::spawn(crate::server::routes::serve_on_until_with_grace(
            listener,
            c.state.clone(),
            async move {
                let _ = wait_for_signal.await;
            },
            Duration::from_millis(200),
        ));

        // A chat POST whose cycle parks in the brain: the request future is
        // still pending, so the connection stays open with nothing on it to
        // finish.
        let body = serde_json::json!({ "text": "do it" }).to_string();
        let request = format!(
            "POST /api/v1/companies/acme/chat HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Cookie: {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            crate::server::test_support::fixed_cookie("acme"),
            body.len(),
        );
        let mut socket = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the host");
        socket
            .write_all(request.as_bytes())
            .await
            .expect("write the request");
        socket.flush().await.expect("flush the request");
        tokio::time::timeout(Duration::from_secs(5), c.entered.notified())
            .await
            .expect("the chat turn started");

        // The turn is never released, so nothing here finishes on its own.
        signal.send(()).expect("deliver the termination signal");
        tokio::time::timeout(super::CONNECTION_GRACE + Duration::from_secs(5), host)
            .await
            .expect("an open connection held the host past its own bound")
            .expect("the host task did not panic")
            .expect("giving up on a connection is not an error");

        c.release.notify_waiters();
        drop(socket);
    }

    /// An **idle** host must not sit out the whole drain bound just because a
    /// connection is open.
    ///
    /// This is the case the ceiling's clock placement decides, and the one the
    /// other ceiling test cannot see: with nothing in flight the drain returns
    /// at once, so timing the connection window from the *signal* would leave
    /// the host waiting out all of `grace` for a stream that is never going to
    /// end, while timing it from the drain lets it go in two seconds. Staging
    /// tenants are refreshed several times an afternoon and are idle most of the
    /// time, so this is the common path, not the exotic one.
    ///
    /// The connection is a real event stream — the thing that actually never
    /// ends — and, unlike a parked chat `POST`, it holds no `serial` lock, so
    /// the host is genuinely idle while it is open.
    #[tokio::test]
    async fn an_idle_host_does_not_wait_out_the_drain_bound_for_an_open_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = home();
        let c = stalled_company(dir.path()).await;
        crate::server::test_support::seed_fixed_admin(&c.state, "acme").await;
        // Deliberately no turn: `drain` has nothing to wait for and returns
        // immediately, which is what puts the two clock placements far apart.

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("a bound address");
        let (signal, wait_for_signal) = tokio::sync::oneshot::channel::<()>();
        // A bound far larger than the connection grace, so the two placements
        // are trivially distinguishable: 30s versus about 2s.
        let grace = Duration::from_secs(30);
        let host = tokio::spawn(crate::server::routes::serve_on_until_with_grace(
            listener,
            c.state.clone(),
            async move {
                let _ = wait_for_signal.await;
            },
            grace,
        ));

        let request = format!(
            "GET /api/v1/company/events HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Cookie: {}\r\n\
             Accept: text/event-stream\r\n\r\n",
            crate::server::test_support::fixed_cookie("acme"),
        );
        let mut socket = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the host");
        socket
            .write_all(request.as_bytes())
            .await
            .expect("write the request");
        socket.flush().await.expect("flush the request");

        // Read the response head so the stream is provably established — an
        // unestablished connection would be idle, which hyper closes at once and
        // which would prove nothing.
        let mut head = [0u8; 32];
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut head))
            .await
            .expect("the event stream answered")
            .expect("read the response head");
        assert!(read > 0, "the event stream sent nothing");

        signal.send(()).expect("deliver the termination signal");
        tokio::time::timeout(super::CONNECTION_GRACE + Duration::from_secs(5), host)
            .await
            .expect("an idle host waited out the whole drain bound for an open stream")
            .expect("the host task did not panic")
            .expect("giving up on a connection is not an error");

        drop(socket);
    }

    /// The honest half of the bound: a turn longer than the grace period is
    /// still cut off, and the drain says so rather than holding the process past
    /// the pod's grace period into a `SIGKILL`.
    #[tokio::test]
    async fn a_turn_that_outlasts_the_bound_does_not_hold_the_process() {
        let dir = home();
        let c = stalled_company(dir.path()).await;
        let turn = start_detached_turn(&c).await;

        assert!(
            !drain(&c.state, Duration::from_millis(200)).await,
            "an expired bound must report an incomplete drain, so the boot reaper's \
             failed-run record stays the backstop"
        );
        // And the company stopped accepting new cycles regardless, so nothing
        // starts a fresh turn in the window before the process exits.
        assert!(
            c.state
                .registry()
                .get(&c.id)
                .expect("registered")
                .is_quiesced()
        );

        c.release.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_secs(5), turn).await;
    }
}
