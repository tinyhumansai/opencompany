//! [`CompanyScheduler`]: drives a company's `[[schedule]]` crons into cycles.
//!
//! Boot lifecycle step 4 starts one scheduler per live company. On each tick it
//! asks an injectable [`Clock`] for the current minute, matches every parsed
//! [`CronExpr`](crate::runtime::cron::CronExpr) against it, and — for each
//! schedule that is due and has not already fired this minute — enqueues a
//! [`CompanyEvent::ScheduleFired`] into the company's serial cycle queue via
//! [`CompanyRuntime::run_cycle`]. Because the runtime holds a per-company serial
//! lock, scheduled cycles interleave safely with operator chat and webhooks.
//!
//! The clock is a trait so tests are fully deterministic: [`FakeClock`] lets a
//! test set or advance the current time and assert exactly which ticks fire,
//! with no wall-clock sleeps. In production [`SystemClock`] reads
//! [`now_millis`](crate::ports::now_millis) and [`CompanyScheduler::spawn`]
//! sleeps to each minute boundary until a shutdown signal fires.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::Result;
use crate::company::Schedule;
use crate::company::runtime::CompanyRuntime;
use crate::ports::now_millis;
use crate::ports::types::CompanyEvent;
use crate::runtime::cron::{CivilTime, CronExpr};

/// Milliseconds in one minute.
pub(crate) const MINUTE_MS: u64 = 60_000;

/// A source of the current wall-clock time, in unix epoch milliseconds.
///
/// Injected so the scheduler never reads a real clock in tests.
pub trait Clock: Send + Sync {
    /// The current time as unix epoch milliseconds.
    fn now_millis(&self) -> u64;
}

/// The production clock: reads the system wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        now_millis()
    }
}

/// A test clock whose time is set or advanced explicitly.
#[derive(Debug, Default)]
pub struct FakeClock(AtomicU64);

impl FakeClock {
    /// A fake clock parked at `ms`.
    pub fn new(ms: u64) -> Self {
        Self(AtomicU64::new(ms))
    }

    /// Jumps the clock to an absolute `ms`.
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    /// Advances the clock by `delta` milliseconds.
    pub fn advance(&self, delta: u64) {
        self.0.fetch_add(delta, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// One parsed schedule: its matcher plus the prompt to deliver when it fires.
struct ParsedSchedule {
    expr: CronExpr,
    cron: String,
    prompt: String,
}

/// Drives the cron schedules of a single [`CompanyRuntime`].
pub struct CompanyScheduler {
    runtime: Arc<CompanyRuntime>,
    schedules: Vec<ParsedSchedule>,
    clock: Arc<dyn Clock>,
    /// Per-schedule last-fired epoch minute, so a schedule fires at most once per
    /// minute no matter how often [`tick`](Self::tick) is called.
    last_fired: HashMap<usize, u64>,
}

impl CompanyScheduler {
    /// Parses `schedules` and binds them to `runtime`, driven by `clock`.
    ///
    /// Returns an error only when a cron expression fails to parse; callers at
    /// boot log the error and skip scheduling for that company rather than
    /// aborting the whole server.
    pub fn new(
        runtime: Arc<CompanyRuntime>,
        schedules: &[Schedule],
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let mut parsed = Vec::with_capacity(schedules.len());
        for schedule in schedules {
            parsed.push(ParsedSchedule {
                expr: CronExpr::parse(&schedule.cron)?,
                cron: schedule.cron.clone(),
                prompt: schedule.prompt.clone(),
            });
        }
        Ok(Self {
            runtime,
            schedules: parsed,
            clock,
            last_fired: HashMap::new(),
        })
    }

    /// Whether this scheduler has any schedules to drive.
    pub fn is_empty(&self) -> bool {
        self.schedules.is_empty()
    }

    /// Runs one tick: fires every schedule that is due this minute and has not
    /// already fired this minute, running a cycle per fire. Returns how many
    /// schedules fired.
    ///
    /// A paused or archived company fires nothing (its `ensure_running` guard
    /// rejects), so schedules resume cleanly when the company is unpaused.
    pub async fn tick(&mut self) -> Result<usize> {
        if self.schedules.is_empty() {
            return Ok(0);
        }
        // Skip firing for a company that is not accepting work.
        if self.runtime.ensure_running().await.is_err() {
            return Ok(0);
        }

        let now = self.clock.now_millis();
        let minute = now / MINUTE_MS;
        let civil = CivilTime::from_unix_millis(now);

        let mut fired = 0;
        for (idx, schedule) in self.schedules.iter().enumerate() {
            if !schedule.expr.matches(&civil) {
                continue;
            }
            if self.last_fired.get(&idx) == Some(&minute) {
                continue; // already fired this minute
            }
            self.last_fired.insert(idx, minute);
            self.runtime
                .run_cycle(vec![CompanyEvent::ScheduleFired {
                    cron: schedule.cron.clone(),
                    prompt: schedule.prompt.clone(),
                }])
                .await?;
            fired += 1;
        }
        Ok(fired)
    }

    /// Runs the per-tick maintenance that rides the same minute boundary as
    /// scheduled fires: sweep parked approvals past their TTL to a default-deny,
    /// then sweep single-use grants the agent never redeemed (issue #243).
    ///
    /// The grant sweep's ids are deliberately not folded into the return value —
    /// callers read it as "approvals that expired", and a grant expiry is a
    /// different event with a different meaning (the operator DID approve; the
    /// agent simply never acted). It announces itself on the operator channel
    /// instead.
    pub async fn tick_maintenance(&self) -> Result<Vec<crate::ports::types::ApprovalId>> {
        let expired = self.runtime.sweep_expired_approvals().await?;
        self.runtime.sweep_expired_grants().await?;
        Ok(expired)
    }

    /// Spawns a background task that ticks on every minute boundary until
    /// `shutdown` is notified, then returns. Boot holds the join handle and the
    /// shared `shutdown` so the scheduler stops cleanly when the server does.
    pub fn spawn(mut self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // The `Notified` future is built ONCE and pinned across iterations,
            // not rebuilt inside the `select!`. Boot signals with
            // `notify_waiters()`, which wakes only the waiters registered at
            // that instant — a future created fresh each iteration is not
            // registered while `tick` is running, so a shutdown arriving
            // mid-tick would be dropped and the scheduler would sleep another
            // full minute before noticing. Polled once here, this one stays
            // registered, and a notification delivered during `tick` is
            // latched: the next `select!` sees it immediately.
            let notified = shutdown.notified();
            tokio::pin!(notified);
            loop {
                let sleep_ms = millis_to_next_minute(self.clock.now_millis());
                tokio::select! {
                    _ = &mut notified => break,
                    _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {
                        if let Err(err) = self.tick().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "scheduled cycle failed");
                        }
                        if let Err(err) = self.tick_maintenance().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "approval sweep failed");
                        }
                    }
                }
            }
        })
    }
}

/// Milliseconds from `now` to the next whole-minute boundary (always `>= 1` so
/// the spawn loop never busy-spins on an exact boundary).
///
/// Shared with [`WorkflowScheduler`](super::workflow_scheduler::WorkflowScheduler)
/// so both minute-boundary loops wake on the same tick.
pub(crate) fn millis_to_next_minute(now: u64) -> u64 {
    let into_minute = now % MINUTE_MS;
    MINUTE_MS - into_minute
}

#[cfg(test)]
mod test {
    use super::*;
    use async_trait::async_trait;

    use crate::company::CompanyManifest;
    use crate::policy::ManifestApprovalGate;
    use crate::ports::brain::{Brain, CycleHost};
    use crate::ports::types::{
        CompressedTrace, CycleRequest, CycleResult, Effect, EffectGroup, EventSeq, OutboundMessage,
        TokenUsage,
    };
    use crate::runtime::RuntimeBuilder;
    use crate::runtime::cron::CivilTime;

    fn tmp_home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-sched-")
            .tempdir()
            .expect("tempdir")
    }

    fn manifest(policy_mode: &str) -> CompanyManifest {
        let toml_src = format!(
            r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "{policy_mode}"
            "#
        );
        toml::from_str(&toml_src).expect("parse manifest")
    }

    /// Unix millis for a UTC civil minute, reusing the cron module's math.
    fn millis_at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        // Search forward from a coarse lower bound is overkill; instead binary
        // via the known conversion: rebuild through CivilTime round-trip.
        // Simpler: brute a direct computation using days-from-civil is private,
        // so derive from a probe. We reconstruct by scanning day starts.
        let mut probe = 0u64;
        // Jump in ~day steps to the target date, then add hour/minute.
        loop {
            let c = CivilTime::from_unix_millis(probe);
            if (c.year, c.month, c.day) == (year, month, day) {
                break;
            }
            probe += 86_400_000;
            if probe > 4_102_444_800_000 {
                panic!("date out of probe range");
            }
        }
        probe + (hour as u64) * 3_600_000 + (minute as u64) * MINUTE_MS
    }

    /// A brain that echoes ScheduleFired events into an operator response, so a
    /// test can assert a scheduled cycle actually ran.
    struct ScheduleBrain;

    #[async_trait]
    impl Brain for ScheduleBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            let mut responses = Vec::new();
            for event in &req.events {
                if let CompanyEvent::ScheduleFired { prompt, .. } = event {
                    responses.push(OutboundMessage {
                        task_id: None,
                        channel: "operator".into(),
                        text: format!("scheduled: {prompt}"),
                        steps: Vec::new(),
                        reply_to: None,
                    });
                }
            }
            Ok(CycleResult {
                channel_responses: responses,
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "scheduled")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    fn scheduled_manifest() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [[schedule]]
            cron = "0 9 * * MON"
            prompt = "weekly standup"

            [policy]
            mode = "full"
        "#;
        toml::from_str(toml_src).expect("parse manifest")
    }

    #[tokio::test]
    async fn fires_once_per_matching_minute_and_dedupes() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );

        // Park the clock at Monday 2026-07-13 09:00 UTC — the schedule matches.
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock.clone()).unwrap();

        assert_eq!(scheduler.tick().await.unwrap(), 1);

        // A ScheduleFired event landed in the log and the brain answered.
        let events = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event,
            CompanyEvent::ScheduleFired { .. }
        ));

        // A second tick within the same minute does not re-fire (dedupe).
        clock.advance(30_000);
        assert_eq!(scheduler.tick().await.unwrap(), 0);

        // Advancing into a non-matching minute (09:01) fires nothing.
        clock.set(millis_at(2026, 7, 13, 9, 1));
        assert_eq!(scheduler.tick().await.unwrap(), 0);

        // The following Monday 09:00 fires again.
        clock.set(millis_at(2026, 7, 20, 9, 0));
        assert_eq!(scheduler.tick().await.unwrap(), 1);

        let events = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn non_matching_minute_never_fires() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        // A Tuesday 09:00 — the Monday schedule must not fire.
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 14, 9, 0)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();
        assert_eq!(scheduler.tick().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn empty_schedule_set_is_a_noop() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = Arc::new(
            RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
                .await
                .unwrap(),
        );
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = CompanyScheduler::new(rt, &[], clock).unwrap();
        assert!(scheduler.is_empty());
        assert_eq!(scheduler.tick().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn tick_maintenance_expires_parked_approval() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        // A brain that parks a Sign effect so there is something to expire.
        struct ParkBrain;
        #[async_trait]
        impl Brain for ParkBrain {
            async fn run_cycle(
                &self,
                req: CycleRequest,
                host: &dyn CycleHost,
            ) -> Result<CycleResult> {
                for event in &req.events {
                    if let CompanyEvent::ScheduleFired { .. } = event {
                        host.emit_effect(Effect {
                            kind: "filing.submit".into(),
                            group: EffectGroup::Sign,
                            amount_usd: None,
                            established_thread: false,
                            first_time_counterparty: false,
                            payload: serde_json::Value::Null,
                            agent: None,
                            run_id: None,
                        })
                        .await?;
                    }
                }
                Ok(CycleResult {
                    channel_responses: Vec::new(),
                    new_traces: vec![CompressedTrace::now(&req.cycle_id, "park")],
                    ledger_deltas: Vec::new(),
                    token_usage: TokenUsage::default(),
                })
            }
        }

        let manifest = scheduled_manifest_supervised();
        let schedules = manifest.schedules.clone();
        // Zero-TTL gate: anything parked is instantly past its deadline.
        let gate = Arc::new(ManifestApprovalGate::new(manifest.policy.clone()).with_ttl_millis(0));
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest)
                .with_brain(Arc::new(ParkBrain))
                .with_approvals(gate)
                .build()
                .await
                .unwrap(),
        );
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        // The scheduled cycle parks one approval.
        assert_eq!(scheduler.tick().await.unwrap(), 1);
        assert_eq!(rt.pending_approvals().len(), 1);

        // Maintenance sweeps it to a default-deny.
        let expired = scheduler.tick_maintenance().await.unwrap();
        assert_eq!(expired.len(), 1);
        assert!(rt.pending_approvals().is_empty());
    }

    fn scheduled_manifest_supervised() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [[schedule]]
            cron = "0 9 * * MON"
            prompt = "weekly standup"

            [policy]
            mode = "supervised"
        "#;
        toml::from_str(toml_src).expect("parse manifest")
    }

    /// A brain that parks inside its first cycle until released, so a test can
    /// deliver a shutdown while a tick is provably in flight.
    struct BlockingBrain {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl Brain for BlockingBrain {
        async fn run_cycle(&self, req: CycleRequest, _host: &dyn CycleHost) -> Result<CycleResult> {
            if let Some(tx) = self.started.lock().expect("started lock").take() {
                let _ = tx.send(());
            }
            // Taken out of the mutex first so no guard is held across the await.
            let release = self.release.lock().expect("release lock").take();
            if let Some(rx) = release {
                let _ = rx.await;
            }
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "blocking")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A shutdown delivered *while a tick is running* must still stop the loop.
    ///
    /// Boot signals with `notify_waiters()`, which wakes only the waiters
    /// registered at that instant. A `spawn` that rebuilds `shutdown.notified()`
    /// inside the `select!` has no waiter registered while `tick` runs — the
    /// signal is dropped and the loop sleeps to the next minute boundary before
    /// noticing it was asked to stop.
    ///
    /// The assertion is loop termination, not elapsed time: the clock is parked
    /// 1ms before a minute boundary so the loop's sleep is 1ms, meaning a
    /// scheduler that missed the signal keeps ticking (and never joins) while a
    /// correct one exits on its very next iteration with nothing left to await.
    /// The 5s bound only caps how long the failing case takes to report.
    #[tokio::test]
    async fn shutdown_during_a_tick_stops_the_loop() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let rt = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest)
                .with_brain(Arc::new(BlockingBrain {
                    started: std::sync::Mutex::new(Some(started_tx)),
                    release: std::sync::Mutex::new(Some(release_rx)),
                }))
                .build()
                .await
                .unwrap(),
        );

        // Monday 2026-07-13 09:00:59.999 UTC: the schedule matches this civil
        // minute, and `millis_to_next_minute` is therefore 1ms.
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0) + MINUTE_MS - 1));
        let scheduler = CompanyScheduler::new(rt, &schedules, clock).unwrap();
        let shutdown = Arc::new(Notify::new());
        let handle = scheduler.spawn(shutdown.clone());

        // Signal shutdown only once the cycle is provably in flight, then let
        // that cycle finish.
        started_rx.await.expect("tick started");
        shutdown.notify_waiters();
        release_tx.send(()).expect("release the in-flight cycle");

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("scheduler kept running after a shutdown delivered mid-tick")
            .expect("scheduler task panicked");
    }

    #[tokio::test]
    async fn bad_cron_fails_construction() {
        // A scheduler over an unparsable cron surfaces the error at construction.
        let bad = [Schedule {
            cron: "not a cron".into(),
            prompt: "x".into(),
        }];
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let rt = Arc::new(
            RuntimeBuilder::fs_defaults(home.clone(), manifest("full"))
                .await
                .unwrap(),
        );
        let clock = Arc::new(FakeClock::new(0));
        assert!(CompanyScheduler::new(rt, &bad, clock).is_err());
    }

    #[test]
    fn next_minute_sleep_is_bounded() {
        assert_eq!(millis_to_next_minute(0), MINUTE_MS);
        assert_eq!(millis_to_next_minute(1), MINUTE_MS - 1);
        assert_eq!(millis_to_next_minute(MINUTE_MS - 1), 1);
        assert_eq!(millis_to_next_minute(MINUTE_MS), MINUTE_MS);
    }
}
