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

/// How far back a restart catch-up (issue #241) will reach to make up a single
/// missed fire: seven days, in minutes.
///
/// Bounded on purpose. Buzz's `scheduled_workflow_fires` never replays a missed
/// fire at all; unbounded replay would stampede token-burning cycles after a
/// long outage. One catch-up per schedule per boot, within this window, is the
/// middle path — it fixes the headline "a weekly run fell during the deploy and
/// vanished" defect without turning a fortnight of downtime into a fortnight of
/// backlog.
pub const CATCHUP_WINDOW_MINUTES: u64 = 7 * 24 * 60;

/// How old a claim row must be before the maintenance tick prunes it: fourteen
/// days, in minutes.
///
/// Deliberately **larger** than [`CATCHUP_WINDOW_MINUTES`] and documented right
/// beside it: the catch-up anchor is the newest claimed minute, and it may be up
/// to a catch-up window old. Pruning at a cutoff inside that window could delete
/// the anchor out from under a booting replica, re-arming a fire that already
/// happened. Keeping the cutoff strictly past the window makes that impossible
/// by construction. A `* * * * *` schedule writes 1440 rows a day, so the prune
/// is what bounds growth.
pub const PRUNE_CUTOFF_MINUTES: u64 = 14 * 24 * 60;

// Compile-time guarantee of the retention invariant: the prune cutoff must sit
// strictly outside the catch-up window, or a prune could delete the anchor a
// booting replica still needs. Enforced here so a future edit to either constant
// that violated it would fail the build, not a test.
const _: () = assert!(PRUNE_CUTOFF_MINUTES > CATCHUP_WINDOW_MINUTES);

/// The single catch-up instant to fire for a schedule at boot, or `None`.
///
/// The one place the "fire at most one missed occurrence" rule lives, shared by
/// both schedulers so they cannot drift. Given the schedule's matcher, the
/// `anchor` (the newest minute it is known to have fired, from
/// [`latest_fire`](crate::ports::ScheduleFireStore::latest_fire)), the current
/// `now_minute`, and a `window`:
///
/// * **No anchor** (`None`, a fresh install) yields `None`: a schedule that has
///   never fired has nothing to make up.
/// * Otherwise the answer is the most recent occurrence strictly between the
///   anchor and now — `anchor < missed < now_minute` — within `window` minutes,
///   or `None` when the last occurrence was already the anchor (nothing was
///   missed) or falls outside the window.
///
/// It returns the *original* scheduled minute, so a caller claims the fire at
/// that minute and simultaneously-booting replicas still race one row.
pub fn missed_instant(
    expr: &CronExpr,
    anchor: Option<u64>,
    now_minute: u64,
    window: u64,
) -> Option<u64> {
    let anchor = anchor?;
    // The most recent match strictly before now, bounded by the window.
    let missed = expr.prev_match_before(now_minute, window)?;
    // Only a match *after* the anchor was actually missed; one at or before it
    // was already fired (the anchor is the last claimed minute).
    (missed > anchor).then_some(missed)
}

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
    /// Restart-stable, content-derived identity for the durable fire claim
    /// (issue #241). See [`manifest_schedule_id`].
    id: String,
}

/// The durable [`ScheduleFireStore`](crate::ports::ScheduleFireStore) identity
/// for a manifest `[[schedule]]`: `"manifest-"` plus the first 16 hex chars of
/// `sha256(cron + "\n" + prompt)`.
///
/// Content-derived rather than the schedule's Vec index, which is what the
/// in-memory dedup keyed on before #241 — reordering the manifest silently
/// reassigned a schedule's identity and could double-fire or drop one. Two
/// schedules with the same `(cron, prompt)` now collapse to one id, so they
/// share one fire per minute; that is a deliberate, documented behaviour change
/// (a manifest that wants two truly distinct fires must differ in cron or
/// prompt). The `\n` separator keeps `("a", "bc")` and `("ab", "c")` distinct.
/// The `manifest-` prefix (a hyphen, never a colon) stays readable in a log line
/// and path-safe for the fs backend.
pub(crate) fn manifest_schedule_id(cron: &str, prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cron.as_bytes());
    hasher.update(b"\n");
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("manifest-{hex}")
}

/// Drives the cron schedules of a single [`CompanyRuntime`].
pub struct CompanyScheduler {
    runtime: Arc<CompanyRuntime>,
    /// When set, the runtime to drive is looked up here every tick so a runtime
    /// swap (issue #290) reaches cron instead of leaving it on the replaced
    /// instance. `None` keeps the boot snapshot.
    registry: Option<crate::runtime::CompanyRegistry>,
    schedules: Vec<ParsedSchedule>,
    clock: Arc<dyn Clock>,
    /// Per-schedule last-fired epoch minute, so a schedule fires at most once per
    /// minute no matter how often [`tick`](Self::tick) is called.
    last_fired: HashMap<usize, u64>,
    /// Whether the one restart catch-up (issue #241) has **completed** — a pass
    /// that got past the `ensure_running` guard AND touched the claim store
    /// without error (issue #661 F1). Starts `false` and, crucially, is NOT set
    /// by a pass that early-returned because the company was paused/archived: a
    /// company paused across boot would otherwise silently forfeit its make-up
    /// forever. [`spawn`](Self::spawn) calls [`catch_up`](Self::catch_up) once
    /// before the loop and again on every tick while this stays `false`, so the
    /// missed fire lands on the first minute the company is actually running.
    /// Re-running a partial pass is idempotent: a fired schedule advanced its
    /// durable anchor and a durable claim already made is not re-made.
    caught_up: bool,
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
                // Computed once, here, so identity is stable for the life of the
                // scheduler and independent of the schedule's position in the
                // manifest.
                id: manifest_schedule_id(&schedule.cron, &schedule.prompt),
            });
        }
        Ok(Self {
            runtime,
            registry: None,
            schedules: parsed,
            clock,
            last_fired: HashMap::new(),
            caught_up: false,
        })
    }

    /// Issue #290: re-read `registry` for this company on every tick, instead of
    /// driving the `Arc<CompanyRuntime>` snapshotted at boot.
    ///
    /// Without this, a runtime swap never reaches this loop: it keeps driving the
    /// replaced runtime — which, after a rebuild, is the offline echo brain the
    /// rebuild existed to get rid of, and is quiesced besides, so every tick
    /// fails. [`WorkflowScheduler`](crate::runtime::WorkflowScheduler) already
    /// reads the registry per tick; this is that pattern, opted into by the boot
    /// path so existing callers and tests keep the snapshot behaviour.
    ///
    /// The boot snapshot stays as the fallback: a company removed from the
    /// registry (archive) is not a reason to start driving nothing at all
    /// silently — `ensure_running` already rejects an archived company, and that
    /// is the check with the right error.
    pub fn following(mut self, registry: crate::runtime::CompanyRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The runtime to drive this tick: whatever is registered now, else the
    /// snapshot taken at construction.
    fn runtime(&self) -> Arc<CompanyRuntime> {
        self.registry
            .as_ref()
            .and_then(|registry| registry.get(self.runtime.id()))
            .unwrap_or_else(|| self.runtime.clone())
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
        let runtime = self.runtime();
        // Skip firing for a company that is not accepting work.
        if runtime.ensure_running().await.is_err() {
            return Ok(0);
        }

        let now = self.clock.now_millis();
        let minute = now / MINUTE_MS;
        let civil = CivilTime::from_unix_millis(now);
        let store = runtime.schedule_fires().clone();

        let mut fired = 0;
        for (idx, schedule) in self.schedules.iter().enumerate() {
            if !schedule.expr.matches(&civil) {
                continue;
            }
            if self.last_fired.get(&idx) == Some(&minute) {
                continue; // already fired this minute (cheap in-process first pass)
            }
            // The in-process map is only a first-pass filter; the durable claim
            // below is the authority. Set it before the claim so a transiently
            // failing store is not re-hit on every tick within this minute.
            self.last_fired.insert(idx, minute);
            match store.claim_fire(runtime.id(), &schedule.id, minute).await {
                // Won the claim: this is the one process/replica that fires.
                // One schedule's cycle failing must not end the tick: every
                // schedule after it in this minute would go unfired, and a
                // permanently broken one would hold them there.
                //
                // `Quiescing` is the exception, and it is not a special case
                // so much as the same rule: the runtime has stopped accepting
                // cycles altogether, so every schedule behind this one would
                // be refused for that reason too. Carrying on would turn one
                // retryable refusal into a whole minute of them, each logged
                // as a failure of its own schedule.
                Ok(true) => match runtime
                    .run_cycle(vec![CompanyEvent::ScheduleFired {
                        cron: schedule.cron.clone(),
                        prompt: schedule.prompt.clone(),
                    }])
                    .await
                {
                    Ok(_) => fired += 1,
                    Err(err @ crate::error::OpenCompanyError::Quiescing(_)) => return Err(err),
                    Err(err) => {
                        tracing::warn!(
                            company = %runtime.id(),
                            schedule = %schedule.id,
                            %err,
                            "scheduler: a fired schedule's cycle failed; the rest of this minute still fires"
                        );
                    }
                },
                // A peer — another replica, or this process before a restart —
                // already claimed this minute. Skip with ZERO side effects.
                Ok(false) => {}
                // Fail closed: a claim store that cannot answer must not fire
                // unclaimed, or it reintroduces the cross-replica double-fire
                // this claim exists to prevent. Skip this minute and warn.
                Err(err) => {
                    tracing::warn!(
                        company = %runtime.id(),
                        schedule = %schedule.id,
                        %err,
                        "scheduler: could not claim a fire; skipping this minute (fail closed)"
                    );
                }
            }
        }
        Ok(fired)
    }

    /// Fires at most one missed occurrence per schedule at boot (issue #241).
    ///
    /// The restart half of the durability fix: a schedule whose fire instant fell
    /// while the process was down would otherwise be dropped with nothing to say
    /// so. For each schedule this reads the [`latest_fire`] anchor, asks
    /// [`missed_instant`] for the single most-recent missed occurrence inside the
    /// catch-up window, and claims it **at its original minute** — so two replicas
    /// booting at once still race one claim and only one fires it. A fresh install
    /// (no anchor) makes up nothing. A claim-store read failure fails closed
    /// (skip), for the same reason [`tick`](Self::tick) does.
    ///
    /// Run from [`spawn`](Self::spawn) before the steady-state loop, and again on
    /// every tick until it latches (issue #661 F1) — a no-op once it has, and one
    /// `ensure_running` probe per minute while it has not.
    ///
    /// # Re-arming until one successful pass (issue #661 F1)
    ///
    /// The pre-loop-only call this replaced early-returned `Ok(0)` whenever
    /// `ensure_running` rejected — so a company **paused across boot** and resumed
    /// later never made up its missed fire, because the one attempt it ever got
    /// happened while it was paused. This now latches only after a pass that got
    /// *past* the `ensure_running` guard AND touched the store without error;
    /// [`spawn`](Self::spawn) re-drives it every minute until then, so the make-up
    /// lands on the first running minute. A pause never latches; a *transient*
    /// store error on the anchor read or the claim clears `complete`, so that pass
    /// does not latch either and a later tick retries — the same "defer, never
    /// forfeit" doctrine the workflow scheduler's first-sight catch-up follows.
    /// Re-running a partial pass is safe: a fired schedule advanced its durable
    /// anchor, and a durable claim already made is not re-made.
    ///
    /// [`latest_fire`]: crate::ports::ScheduleFireStore::latest_fire
    pub async fn catch_up(&mut self) -> Result<usize> {
        // Already made up — nothing more this process needs to do.
        if self.caught_up {
            return Ok(0);
        }
        if self.schedules.is_empty() {
            return Ok(0);
        }
        let runtime = self.runtime();
        if runtime.ensure_running().await.is_err() {
            // Paused/archived: do NOT latch, so a later resume still catches up.
            return Ok(0);
        }
        let now_minute = self.clock.now_millis() / MINUTE_MS;
        let store = runtime.schedule_fires().clone();
        let mut fired = 0;
        // Whether this pass reached a verdict for every schedule without a
        // transient store error. A single failed read/claim leaves it `false`, so
        // the latch stays clear and a later tick retries rather than forfeiting.
        let mut complete = true;
        for schedule in &self.schedules {
            let anchor = match store.latest_fire(runtime.id(), &schedule.id).await {
                Ok(anchor) => anchor,
                Err(err) => {
                    complete = false;
                    tracing::warn!(
                        company = %runtime.id(),
                        schedule = %schedule.id,
                        %err,
                        "scheduler: could not read catch-up anchor; skipping catch-up (fail closed)"
                    );
                    continue;
                }
            };
            let Some(missed) =
                missed_instant(&schedule.expr, anchor, now_minute, CATCHUP_WINDOW_MINUTES)
            else {
                continue;
            };
            match store.claim_fire(runtime.id(), &schedule.id, missed).await {
                Ok(true) => {
                    runtime
                        .run_cycle(vec![CompanyEvent::ScheduleFired {
                            cron: schedule.cron.clone(),
                            prompt: schedule.prompt.clone(),
                        }])
                        .await?;
                    fired += 1;
                    tracing::info!(
                        company = %runtime.id(),
                        schedule = %schedule.id,
                        missed_minute = missed,
                        "scheduler: fired one catch-up for a schedule missed during downtime"
                    );
                }
                // A simultaneously-booting replica claimed the catch-up first.
                Ok(false) => {}
                Err(err) => {
                    complete = false;
                    tracing::warn!(
                        company = %runtime.id(),
                        schedule = %schedule.id,
                        %err,
                        "scheduler: could not claim catch-up fire; skipping (fail closed)"
                    );
                }
            }
        }
        // Latch only a clean pass; a partial one stays re-armed for a later tick.
        if complete {
            self.caught_up = true;
        }
        Ok(fired)
    }

    /// Spawns a background task that ticks on every minute boundary until
    /// `shutdown` is notified, then returns. Boot holds the join handle and the
    /// shared `shutdown` so the scheduler stops cleanly when the server does.
    pub fn spawn(mut self, shutdown: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Issue #241: make up at most one missed fire per schedule that fell
            // during downtime, BEFORE entering the steady-state loop, so a weekly
            // run whose instant landed inside the last deploy still happens.
            if let Err(err) = self.catch_up().await {
                tracing::warn!(company = %self.runtime.id(), %err, "scheduled catch-up failed");
            }
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
                        // Issue #661 F1: re-attempt the boot catch-up until it
                        // latches. A no-op once made up; while the company is
                        // paused this is one cheap `ensure_running` probe a minute,
                        // so a company resumed after boot still makes up its missed
                        // fire on its first running minute rather than never.
                        if let Err(err) = self.catch_up().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "scheduled catch-up failed");
                        }
                        if let Err(err) = self.tick().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "scheduled cycle failed");
                        }
                        // Issue #971: maintenance is NOT driven from here any
                        // more. It rides the process-wide
                        // `MaintenanceTicker`, which reaches every registered
                        // company rather than only the ones with a manifest
                        // cron — and a company with a cron would otherwise be
                        // swept twice a minute for no gain.
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
#[path = "scheduler_tests_core.rs"]
mod tests_core;
#[cfg(test)]
#[path = "scheduler_tests_part1.rs"]
mod tests_part1;
#[cfg(test)]
#[path = "scheduler_tests_part2.rs"]
mod tests_part2;
