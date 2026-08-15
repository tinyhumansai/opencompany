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

/// Issue #751: the parked-approval digest holds new notifications until parks go
/// quiet for this long, then flushes what accumulated as one email — so twenty
/// raised over an evening arrive as one message, not twenty.
const DIGEST_QUIET_MS: u64 = 30 * MINUTE_MS; // 30 minutes of quiet
/// …or until the oldest held notification has waited this long, whichever comes
/// first, so a company whose parks never fall quiet still gets its digest.
const DIGEST_MAX_MS: u64 = 12 * 60 * MINUTE_MS; // 12 hours
/// The fire-claim id that serialises the digest flush to one replica per minute,
/// reusing the same at-most-once store the cron schedules claim on.
const DIGEST_SCHEDULE_ID: &str = "notification-digest";

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
                Ok(true) => {
                    runtime
                        .run_cycle(vec![CompanyEvent::ScheduleFired {
                            cron: schedule.cron.clone(),
                            prompt: schedule.prompt.clone(),
                        }])
                        .await?;
                    fired += 1;
                }
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
        let runtime = self.runtime();
        let expired = runtime.sweep_expired_approvals().await?;
        runtime.sweep_expired_grants().await?;
        // Issue #241: bound the fire-claim log's growth on the same tick. The
        // cutoff sits a full week past the catch-up window (PRUNE_CUTOFF_MINUTES
        // > CATCHUP_WINDOW_MINUTES), so an anchor a booting replica still needs
        // is never eligible. Best-effort — a prune failure must not abort the
        // approval sweeps it shares a tick with.
        let cutoff = (self.clock.now_millis() / MINUTE_MS).saturating_sub(PRUNE_CUTOFF_MINUTES);
        if let Err(err) = runtime
            .schedule_fires()
            .prune_fires_before(runtime.id(), cutoff)
            .await
        {
            tracing::warn!(
                company = %runtime.id(),
                %err,
                "scheduler: pruning old fire claims failed"
            );
        }
        Ok(expired)
    }

    /// Issue #751: batch the parked-approval notifications a company accumulates
    /// into ONE digest email rather than one per park.
    ///
    /// **Quiet-then-flush**, derived from the undelivered notifications' own
    /// timestamps so it needs no extra state: hold while parks keep arriving, and
    /// flush once things go quiet for [`DIGEST_QUIET_MS`] or the oldest held one
    /// has waited out [`DIGEST_MAX_MS`]. An approval settled during the window is
    /// dropped from the digest (and marked delivered so it never reappears), so
    /// the digest never reports work already actioned. Best-effort, and
    /// at-most-once per company per minute across replicas via the fire claim.
    pub async fn tick_digest(&self) -> Result<usize> {
        let runtime = self.runtime();
        if runtime.ensure_running().await.is_err() {
            return Ok(0);
        }
        let company = runtime.id();
        let undelivered = runtime.notifications().undelivered(company).await?;
        if undelivered.is_empty() {
            return Ok(0);
        }
        // An approval settled during the window is noise the digest exists to
        // remove — partition the queue on "still parked".
        let pending: std::collections::HashSet<String> = runtime
            .pending_approvals()
            .into_iter()
            .map(|a| a.id.to_string())
            .collect();
        // `pending_approvals()` is authoritative for "still parked": boot replay
        // rehydrates every parked approval into the gate *before* the scheduler
        // is spawned (`RuntimeBuilder::build` performs the replay), so an id
        // absent here is resolved/expired, never merely "not loaded yet" — a
        // still-pending approval is therefore never dropped as settled.
        let mut settled = Vec::new();
        let mut live = Vec::new();
        for n in undelivered {
            let still_relevant = match n.subject.kind {
                crate::ports::notifications::SubjectKind::Approval => {
                    pending.contains(&n.subject.id)
                }
                // This digest speaks only about parked approvals — its subject and
                // body say so. Another notification kind belongs to whatever path
                // owns it, so drain it here rather than mail it under the wrong
                // heading; draining it also keeps the queue bounded, since a
                // non-approval subject has no "settled" notion to remove it later.
                _ => false,
            };
            if still_relevant {
                live.push(n);
            } else {
                settled.push(n.id);
            }
        }
        // Drop settled ones from the queue for good: marked delivered without
        // ever being emailed, so a later flush never reconsiders them. This runs
        // *before* the mailbox check below, so resolved approvals drain even on a
        // company with no mail — which is what keeps the queue bounded (it can
        // then only hold still-pending approvals, itself a bounded set).
        if !settled.is_empty() {
            runtime
                .notifications()
                .mark_delivered(company, &settled)
                .await?;
        }
        if live.is_empty() {
            return Ok(0);
        }
        // Quiet-then-flush, read off the live set's own timestamps.
        let now = self.clock.now_millis();
        let newest = live.iter().map(|n| n.created_at).max().unwrap_or(now);
        let oldest = live.iter().map(|n| n.created_at).min().unwrap_or(now);
        let quiet = now.saturating_sub(newest) >= DIGEST_QUIET_MS;
        let capped = now.saturating_sub(oldest) >= DIGEST_MAX_MS;
        if !quiet && !capped {
            return Ok(0); // still accumulating — wait for a lull or the cap
        }
        // Resolve the channel server-side. No mailbox or no owner → leave the
        // live set undelivered for a later flush when one exists (the console
        // still shows them); never drop what was never emailed. The queue stays
        // bounded across this branch: only still-pending approvals remain here
        // (settled ones drained above), so it tracks the pending set, not time —
        // and dropping them instead would silently suppress real approvals.
        let Some(mail) = runtime.mail().cloned() else {
            return Ok(0);
        };
        let record = match runtime.store().load(company).await {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(0),
            Err(err) => {
                tracing::warn!(company = %company, %err, "digest: could not load the company record");
                return Ok(0);
            }
        };
        // Recipients resolve from the roster, never from a notification.
        let recipients = crate::company::owners::owner_recipients(
            runtime.users().as_ref(),
            company,
            &record,
            runtime.bootstrap_admin(),
        )
        .await;
        if recipients.is_empty() {
            return Ok(0);
        }
        // At-most-once per company per minute across replicas.
        let minute = now / MINUTE_MS;
        if !runtime
            .schedule_fires()
            .claim_fire(company, DIGEST_SCHEDULE_ID, minute)
            .await?
        {
            return Ok(0);
        }
        // Mark delivered BEFORE the send: a crash mid-send loses at most one
        // email (the console still holds the notifications), whereas re-digesting
        // would resurrect the very firehose #751 exists to remove.
        let ids: Vec<String> = live.iter().map(|n| n.id.clone()).collect();
        runtime
            .notifications()
            .mark_delivered(company, &ids)
            .await?;
        let count = live.len();
        let subject = format!(
            "{} — {count} approval{} awaiting review",
            record.manifest.company.name,
            if count == 1 { "" } else { "s" },
        );
        let body = build_digest_body(&record.manifest.company.name, &live);
        // Spawn the SMTP send so a slow mail server never stalls the tick.
        tokio::spawn(crate::company::owners::deliver_to_owners(
            mail,
            recipients,
            subject,
            body,
            company.clone(),
        ));
        Ok(count)
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
                        if let Err(err) = self.tick_maintenance().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "approval sweep failed");
                        }
                        if let Err(err) = self.tick_digest().await {
                            tracing::warn!(company = %self.runtime.id(), %err, "notification digest failed");
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

/// The plain-text body of a parked-approval digest (issue #751): the list of what
/// is waiting, then where to act. Carries only each notification's title (the
/// effect kind + agent, never the payload), so the digest opens no redaction
/// surface the per-notification path did not.
fn build_digest_body(
    company_name: &str,
    notifications: &[crate::ports::notifications::Notification],
) -> String {
    let count = notifications.len();
    let mut body = format!(
        "{count} approval{} in {company_name} {} waiting for your review:\n\n",
        if count == 1 { "" } else { "s" },
        if count == 1 { "is" } else { "are" },
    );
    for n in notifications {
        body.push_str(&format!("  • {}\n", n.title));
    }
    body.push_str("\nOpen your OpenCompany console to approve or deny.\n");
    body
}

#[cfg(test)]
mod test {
    use super::*;
    use async_trait::async_trait;

    use crate::company::CompanyManifest;
    use crate::company::runtime::CompanyMail;
    use crate::policy::ManifestApprovalGate;
    use crate::ports::brain::{Brain, CycleHost};
    use crate::ports::types::{Actor, ActorKind, Verdict};
    use crate::ports::types::{
        CompressedTrace, CycleRequest, CycleResult, Effect, EffectGroup, EventSeq, OutboundMessage,
        TokenUsage,
    };
    use crate::runtime::RuntimeBuilder;
    use crate::runtime::cron::CivilTime;
    use crate::server::ops::mailer::RecordingMailSender;
    use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};

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
                        message_id: None,
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
        //
        // Filtered rather than counted: since issue #327 boot's workspace
        // scaffold journals a `WorkspaceChanged` per reserved root, so the log
        // is not empty before the tick. This test is about the cron firing
        // once, which is a question about `ScheduleFired` entries specifically.
        let events = rt
            .events
            .read_from(rt.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        let fired: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.event, CompanyEvent::ScheduleFired { .. }))
            .collect();
        assert_eq!(fired.len(), 1);

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
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, CompanyEvent::ScheduleFired { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn following_the_registry_reaches_a_rebuilt_runtime() {
        // Issue #290. The cron scheduler snapshots an `Arc<CompanyRuntime>` at
        // boot, so before this it kept driving a runtime that had been replaced
        // — and a replaced runtime is quiesced, so every fire failed. "Scheduled
        // workflows never fire" is one of the two surfaces #266 was reported
        // against, which makes this the half of a rebuild that is easiest to
        // ship broken.
        use crate::runtime::CompanyRegistry;

        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let outgoing = Arc::new(
            RuntimeBuilder::new(home.clone(), manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        let id = outgoing.id().clone();
        let registry = CompanyRegistry::new();
        registry.insert(id.clone(), outgoing.clone());

        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = CompanyScheduler::new(outgoing.clone(), &schedules, clock.clone())
            .unwrap()
            .following(registry.clone());

        // Stand in for a rebuild: quiesce the snapshotted runtime and register a
        // successor over the same home.
        outgoing.quiesce().await;
        let successor = Arc::new(
            RuntimeBuilder::new(home.clone(), scheduled_manifest())
                .with_id(id.clone())
                .with_brain(Arc::new(ScheduleBrain))
                .with_handover(outgoing.handover())
                .build()
                .await
                .unwrap(),
        );
        registry.insert(id.clone(), successor.clone());

        // The fire lands, which it could not have done against the quiesced
        // runtime the scheduler was built with.
        assert_eq!(scheduler.tick().await.unwrap(), 1);
        let events = successor
            .events()
            .read_from(&id, EventSeq::new(0), 10)
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e.event, CompanyEvent::ScheduleFired { .. })),
            "the successor ran the scheduled cycle",
        );
    }

    #[tokio::test]
    async fn without_following_a_scheduler_drives_its_snapshot() {
        // The opt-in is real: an un-followed scheduler still drives the runtime
        // it was handed, which is what every existing caller and test relies on.
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
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        rt.quiesce().await;

        // No registry to consult, so the quiesced snapshot is what it tries, and
        // the refusal surfaces rather than being silently swallowed.
        let err = scheduler
            .tick()
            .await
            .expect_err("a quiesced snapshot refuses the cycle");
        assert!(
            matches!(err, crate::error::OpenCompanyError::Quiescing(_)),
            "{err}"
        );
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

    // --- issue #241: durable claims + restart catch-up ---------------------

    use crate::error::OpenCompanyError;
    use crate::ports::schedule_fires::ScheduleFireStore;
    use crate::ports::types::CompanyId;

    /// ScheduleFired events actually written to a runtime's log. A claim loser,
    /// and a fail-closed skip, must leave this at whatever it was — the "zero
    /// side effects" invariant, read from the durable trail.
    async fn fired_count(rt: &CompanyRuntime) -> usize {
        rt.events
            .read_from(rt.id(), EventSeq::new(0), 1024)
            .await
            .unwrap()
            .iter()
            .filter(|e| matches!(e.event, CompanyEvent::ScheduleFired { .. }))
            .count()
    }

    /// A claim store whose every method errors, for the fail-closed path.
    struct ErroringFires;

    #[async_trait]
    impl ScheduleFireStore for ErroringFires {
        async fn claim_fire(&self, _c: &CompanyId, _s: &str, _m: u64) -> Result<bool> {
            Err(OpenCompanyError::Store("claim store is down".into()))
        }
        async fn latest_fire(&self, _c: &CompanyId, _s: &str) -> Result<Option<u64>> {
            Err(OpenCompanyError::Store("claim store is down".into()))
        }
        async fn prune_fires_before(&self, _c: &CompanyId, _m: u64) -> Result<usize> {
            Err(OpenCompanyError::Store("claim store is down".into()))
        }
        async fn delete_schedule_fires(&self, _c: &CompanyId, _s: &str) -> Result<usize> {
            Err(OpenCompanyError::Store("claim store is down".into()))
        }
    }

    #[test]
    fn manifest_schedule_id_is_stable_and_order_independent() {
        let a = manifest_schedule_id("0 9 * * MON", "standup");
        let b = manifest_schedule_id("0 9 * * MON", "standup");
        assert_eq!(a, b, "same (cron, prompt) → same id, whatever the position");
        assert!(a.starts_with("manifest-"), "{a}");
        // A different prompt or cron is a different schedule.
        assert_ne!(a, manifest_schedule_id("0 9 * * MON", "other"));
        assert_ne!(a, manifest_schedule_id("0 10 * * MON", "standup"));
        // The `\n` separator keeps ("a","bc") and ("ab","c") distinct.
        assert_ne!(
            manifest_schedule_id("a", "bc"),
            manifest_schedule_id("ab", "c")
        );
    }

    #[test]
    fn missed_instant_rules() {
        let expr = CronExpr::parse("0 9 * * MON").unwrap();
        let minute = |ms: u64| ms / MINUTE_MS;
        let m0 = minute(millis_at(2026, 7, 6, 9, 0)); // a Monday 09:00
        let m2 = minute(millis_at(2026, 7, 20, 9, 0)); // two Mondays later
        let now = minute(millis_at(2026, 7, 20, 9, 5)); // just after m2

        // Fresh install (no anchor) makes up nothing.
        assert_eq!(
            missed_instant(&expr, None, now, CATCHUP_WINDOW_MINUTES),
            None
        );
        // Downtime spanning the two intervening Mondays, anchor at m0 → exactly
        // the MOST RECENT missed one (m2), never the older intermediates.
        assert_eq!(
            missed_instant(&expr, Some(m0), now, CATCHUP_WINDOW_MINUTES),
            Some(m2)
        );
        // Anchor already at the most recent match → nothing was missed.
        assert_eq!(
            missed_instant(&expr, Some(m2), now, CATCHUP_WINDOW_MINUTES),
            None
        );
        // Beyond the window: too small a window cannot reach even m2.
        assert_eq!(missed_instant(&expr, Some(m0), now, 1), None);
    }

    /// Two independent schedulers over one durable store — a second replica, or a
    /// restarted process — fire exactly once between them, and the loser writes
    /// nothing.
    #[tokio::test]
    async fn two_schedulers_over_one_store_fire_once() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut a = CompanyScheduler::new(rt.clone(), &schedules, clock.clone()).unwrap();
        let mut b = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        let first = a.tick().await.unwrap();
        let second = b.tick().await.unwrap();
        assert_eq!(first + second, 1, "exactly one replica fires the minute");
        assert_eq!(
            fired_count(&rt).await,
            1,
            "the loser leaves no ScheduleFired behind"
        );
    }

    /// A claim store that errors fails **closed**: the fire is skipped, not fired
    /// unclaimed, so the double-fire the claim prevents cannot sneak back in.
    #[tokio::test]
    async fn a_failing_claim_store_fires_nothing() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .with_schedule_fires(Arc::new(ErroringFires))
                .build()
                .await
                .unwrap(),
        );
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 13, 9, 0)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        assert_eq!(scheduler.tick().await.unwrap(), 0, "fail-closed: no fire");
        assert_eq!(fired_count(&rt).await, 0);
    }

    /// Downtime spanning several occurrences produces exactly one catch-up (the
    /// most recent), and a second boot finds it already claimed.
    #[tokio::test]
    async fn catch_up_fires_one_missed_instant_then_none_left() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        // Anchor at the oldest of three Mondays; "now" just after the newest.
        let sid = manifest_schedule_id("0 9 * * MON", "weekly standup");
        let anchor = millis_at(2026, 7, 6, 9, 0) / MINUTE_MS;
        assert!(
            rt.schedule_fires()
                .claim_fire(rt.id(), &sid, anchor)
                .await
                .unwrap()
        );
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 20, 9, 5)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        assert_eq!(
            scheduler.catch_up().await.unwrap(),
            1,
            "one catch-up for the most recent missed Monday"
        );
        assert_eq!(fired_count(&rt).await, 1);
        // The catch-up claimed the ORIGINAL minute, so the anchor advanced to it.
        assert_eq!(
            rt.schedule_fires()
                .latest_fire(rt.id(), &sid)
                .await
                .unwrap(),
            Some(millis_at(2026, 7, 20, 9, 0) / MINUTE_MS)
        );
        // Idempotent: a second boot finds the catch-up already made.
        assert_eq!(scheduler.catch_up().await.unwrap(), 0);
        assert_eq!(fired_count(&rt).await, 1);
    }

    /// A fresh install — no anchor row anywhere — makes up nothing at boot.
    #[tokio::test]
    async fn catch_up_on_a_fresh_install_fires_nothing() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 20, 9, 5)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();
        assert_eq!(scheduler.catch_up().await.unwrap(), 0);
        assert_eq!(fired_count(&rt).await, 0);
    }

    /// Two replicas booting at once race the catch-up claim: one fires it, the
    /// other loses.
    #[tokio::test]
    async fn racing_catch_up_fires_once() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        let sid = manifest_schedule_id("0 9 * * MON", "weekly standup");
        let anchor = millis_at(2026, 7, 6, 9, 0) / MINUTE_MS;
        rt.schedule_fires()
            .claim_fire(rt.id(), &sid, anchor)
            .await
            .unwrap();
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 20, 9, 5)));
        let mut a = CompanyScheduler::new(rt.clone(), &schedules, clock.clone()).unwrap();
        let mut b = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        let fa = a.catch_up().await.unwrap();
        let fb = b.catch_up().await.unwrap();
        assert_eq!(fa + fb, 1, "only one booting replica fires the catch-up");
        assert_eq!(fired_count(&rt).await, 1);
    }

    // --- issue #661 (F1): re-arm the boot catch-up until one successful pass ----

    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// Moves the company's durable lifecycle, so a test can pause the company
    /// across a boot catch-up and later resume it.
    async fn set_lifecycle(rt: &CompanyRuntime, lifecycle: &str) {
        let store = rt.store().clone();
        let mut record = store
            .load(rt.id())
            .await
            .expect("loads")
            .expect("the builder materialized a record");
        record.lifecycle = lifecycle.to_string();
        store.save(&record).await.expect("saves");
    }

    /// An in-memory claim store whose first N `latest_fire` reads error, then
    /// works — the flaky-once double for "a transient store error does NOT latch
    /// the boot catch-up, so a later pass retries" (issue #661 F1). `seed` presets
    /// an anchor without consuming the fail budget.
    struct FlakyOnceFires {
        claims: Mutex<HashMap<(String, String), HashSet<u64>>>,
        fail_latest: AtomicUsize,
    }

    impl FlakyOnceFires {
        fn new(fail_latest: usize) -> Self {
            Self {
                claims: Mutex::new(HashMap::new()),
                fail_latest: AtomicUsize::new(fail_latest),
            }
        }
        fn seed(&self, c: &CompanyId, s: &str, m: u64) {
            self.claims
                .lock()
                .unwrap()
                .entry((c.as_ref().to_string(), s.to_string()))
                .or_default()
                .insert(m);
        }
    }

    #[async_trait]
    impl ScheduleFireStore for FlakyOnceFires {
        async fn claim_fire(&self, c: &CompanyId, s: &str, m: u64) -> Result<bool> {
            Ok(self
                .claims
                .lock()
                .unwrap()
                .entry((c.as_ref().to_string(), s.to_string()))
                .or_default()
                .insert(m))
        }
        async fn latest_fire(&self, c: &CompanyId, s: &str) -> Result<Option<u64>> {
            if self.fail_latest.load(Ordering::SeqCst) > 0 {
                self.fail_latest.fetch_sub(1, Ordering::SeqCst);
                return Err(OpenCompanyError::Store("flaky once claim store".into()));
            }
            Ok(self
                .claims
                .lock()
                .unwrap()
                .get(&(c.as_ref().to_string(), s.to_string()))
                .and_then(|set| set.iter().max().copied()))
        }
        async fn prune_fires_before(&self, _c: &CompanyId, _m: u64) -> Result<usize> {
            Ok(0)
        }
        async fn delete_schedule_fires(&self, c: &CompanyId, s: &str) -> Result<usize> {
            Ok(self
                .claims
                .lock()
                .unwrap()
                .remove(&(c.as_ref().to_string(), s.to_string()))
                .map_or(0, |set| set.len()))
        }
    }

    /// A company paused across boot gets its catch-up on RESUME, not never. The
    /// pre-loop-only catch-up used to early-return on `ensure_running` and latch
    /// nothing — but with no re-arm the missed fire was gone. Now the pause does
    /// not latch, so the first running pass makes it up.
    #[tokio::test]
    async fn catch_up_skipped_while_paused_runs_on_resume() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        // Anchor two Mondays back so the most recent Monday (2026-07-13) is missed.
        let sid = manifest_schedule_id("0 9 * * MON", "weekly standup");
        rt.schedule_fires()
            .claim_fire(rt.id(), &sid, millis_at(2026, 7, 6, 9, 0) / MINUTE_MS)
            .await
            .unwrap();
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 20, 9, 5)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        // Paused across boot: the guard rejects and nothing is made up — but the
        // latch stays clear.
        set_lifecycle(&rt, "paused").await;
        assert_eq!(
            scheduler.catch_up().await.unwrap(),
            0,
            "a paused company makes up nothing"
        );
        assert_eq!(fired_count(&rt).await, 0);

        // Resumed: the very next catch-up pass makes up the missed fire.
        set_lifecycle(&rt, "running").await;
        assert_eq!(
            scheduler.catch_up().await.unwrap(),
            1,
            "the resumed company gets its deferred catch-up"
        );
        assert_eq!(fired_count(&rt).await, 1);
    }

    /// One clean pass latches: a second pass is a no-op even when a genuinely new
    /// occurrence has since been missed. Proven by contrast with a fresh
    /// (non-latched) scheduler over the same store, which DOES make up the new one.
    #[tokio::test]
    async fn catch_up_latches_after_one_successful_pass() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .build()
                .await
                .unwrap(),
        );
        let sid = manifest_schedule_id("0 9 * * MON", "weekly standup");
        rt.schedule_fires()
            .claim_fire(rt.id(), &sid, millis_at(2026, 7, 6, 9, 0) / MINUTE_MS)
            .await
            .unwrap();
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 20, 9, 5)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock.clone()).unwrap();

        // First pass fires the most recent missed Monday and LATCHES.
        assert_eq!(scheduler.catch_up().await.unwrap(), 1);
        assert_eq!(fired_count(&rt).await, 1);

        // Advance a week: 2026-07-27 09:00 is now a genuinely new missed fire.
        clock.set(millis_at(2026, 7, 27, 9, 5));
        // The latched scheduler ignores it — that is the whole point of the latch.
        assert_eq!(
            scheduler.catch_up().await.unwrap(),
            0,
            "a latched scheduler runs no second pass"
        );
        assert_eq!(
            fired_count(&rt).await,
            1,
            "no new fire from the latched one"
        );

        // A fresh scheduler (latch clear) over the SAME store proves the new miss
        // was real and would have been caught but for the latch.
        let mut fresh = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();
        assert_eq!(
            fresh.catch_up().await.unwrap(),
            1,
            "a non-latched scheduler still makes up the newly missed fire"
        );
        assert_eq!(fired_count(&rt).await, 2);
    }

    /// A pass that hit a transient store error does NOT latch, so a later pass
    /// retries and fires — 0 then 1 across a flaky-once store.
    #[tokio::test]
    async fn a_failed_pass_does_not_latch() {
        let home_dir = tmp_home();
        let home = home_dir.path().to_path_buf();
        let manifest = scheduled_manifest();
        let schedules = manifest.schedules.clone();
        let flaky = Arc::new(FlakyOnceFires::new(1));
        let rt = Arc::new(
            RuntimeBuilder::new(home, manifest)
                .with_brain(Arc::new(ScheduleBrain))
                .with_schedule_fires(flaky.clone())
                .build()
                .await
                .unwrap(),
        );
        let sid = manifest_schedule_id("0 9 * * MON", "weekly standup");
        // Seed the anchor directly (bypassing the fail budget) so the miss is real.
        flaky.seed(rt.id(), &sid, millis_at(2026, 7, 6, 9, 0) / MINUTE_MS);
        let clock = Arc::new(FakeClock::new(millis_at(2026, 7, 20, 9, 5)));
        let mut scheduler = CompanyScheduler::new(rt.clone(), &schedules, clock).unwrap();

        // First pass: the anchor read errors, so the pass does not complete and
        // must NOT latch.
        assert_eq!(
            scheduler.catch_up().await.unwrap(),
            0,
            "a store error fires nothing and does not latch"
        );
        assert_eq!(fired_count(&rt).await, 0);

        // Second pass: the store now works, so the deferred catch-up fires.
        assert_eq!(
            scheduler.catch_up().await.unwrap(),
            1,
            "the retry makes up the missed fire"
        );
        assert_eq!(fired_count(&rt).await, 1);
    }

    // --- Notification digest (issue #751) -----------------------------------

    /// A brain that parks one effect on each operator message, so a test can
    /// accumulate parked approvals for the digest to batch.
    struct ParkBrain {
        effect: Effect,
    }

    #[async_trait]
    impl Brain for ParkBrain {
        async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult> {
            for event in &req.events {
                if let CompanyEvent::OperatorMessage { .. } = event {
                    host.emit_effect(self.effect.clone()).await?;
                }
            }
            Ok(CycleResult {
                channel_responses: Vec::new(),
                new_traces: vec![CompressedTrace::now(&req.cycle_id, "parked")],
                ledger_deltas: Vec::new(),
                token_usage: TokenUsage::default(),
            })
        }
    }

    /// A consequence effect that parks under `supervised`.
    fn park_effect(kind: &str) -> Effect {
        Effect {
            kind: kind.into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::json!({}),
            agent: Some("engineer".into()),
            run_id: None,
        }
    }

    fn test_smtp(from_email: &str) -> SmtpCredentials {
        SmtpCredentials {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "user".into(),
            password: "hunter2".into(),
            from_name: "Acme".into(),
            from_email: from_email.into(),
        }
    }

    fn park_message() -> CompanyEvent {
        CompanyEvent::OperatorMessage {
            parent: None,
            text: "do it".into(),
            by: None,
            chat: None,
        }
    }

    /// **The acceptance bar for issue #751.** Twenty (here three) approvals
    /// parked over an evening arrive as ONE digest, held until parks fall quiet,
    /// and are not re-sent after delivery. The clock is injectable, so the window
    /// is exercised with no sleeps.
    #[tokio::test]
    async fn digest_batches_parked_approvals_into_one_email_after_quiet() {
        let home_dir = tmp_home();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(Arc::new(ParkBrain {
                    effect: park_effect("shell"),
                }))
                .with_mail(CompanyMail {
                    sender: sender.clone(),
                    smtp: test_smtp("ceo@acme.test"),
                })
                .with_bootstrap_admin(Some("boss@acme.test".into()))
                .build()
                .await
                .unwrap(),
        );

        // A base instant, then three parks — each records one undelivered
        // notification stamped at ~base (real `now_millis()`).
        let base = crate::ports::now_millis();
        for _ in 0..3 {
            rt.run_cycle(vec![park_message()]).await.unwrap();
        }
        assert_eq!(
            rt.notifications().undelivered(rt.id()).await.unwrap().len(),
            3
        );
        assert_eq!(rt.pending_approvals().len(), 3);

        // Within the quiet window → hold, send nothing.
        let clock = Arc::new(FakeClock::new(base + MINUTE_MS));
        let scheduler = CompanyScheduler::new(rt.clone(), &[], clock.clone()).unwrap();
        assert_eq!(
            scheduler.tick_digest().await.unwrap(),
            0,
            "still accumulating"
        );
        assert!(sender.sent().is_empty());

        // Past the quiet threshold → one digest of all three; queue drains.
        clock.set(base + DIGEST_QUIET_MS + 2 * MINUTE_MS);
        assert_eq!(scheduler.tick_digest().await.unwrap(), 3);
        assert!(
            rt.notifications()
                .undelivered(rt.id())
                .await
                .unwrap()
                .is_empty()
        );

        // Drain the spawned send: exactly one email to the server-resolved owner,
        // its subject naming the batch of three.
        let mut sent = sender.sent();
        for _ in 0..200 {
            if !sent.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
            sent = sender.sent();
        }
        assert_eq!(sent.len(), 1, "three parks become one digest");
        assert_eq!(sent[0].1.to, "boss@acme.test");
        assert_eq!(sent[0].0, "ceo@acme.test");
        assert!(
            sent[0].1.subject.contains("3 approvals"),
            "subject: {}",
            sent[0].1.subject
        );

        // A second flush sends nothing more — everything was delivered.
        assert_eq!(scheduler.tick_digest().await.unwrap(), 0);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sender.sent().len(), 1, "no re-send after delivery");
    }

    /// An approval settled during the window is dropped from the digest and never
    /// emailed — the digest reports no already-actioned work.
    #[tokio::test]
    async fn digest_drops_an_approval_settled_during_the_window() {
        let home_dir = tmp_home();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(Arc::new(ParkBrain {
                    effect: park_effect("shell"),
                }))
                .with_mail(CompanyMail {
                    sender: sender.clone(),
                    smtp: test_smtp("ceo@acme.test"),
                })
                .with_bootstrap_admin(Some("boss@acme.test".into()))
                .build()
                .await
                .unwrap(),
        );

        let base = crate::ports::now_millis();
        for _ in 0..2 {
            rt.run_cycle(vec![park_message()]).await.unwrap();
        }
        let pending = rt.pending_approvals();
        assert_eq!(pending.len(), 2);

        // Settle one before the flush — it becomes noise the digest must skip.
        rt.resolve_approval(
            &pending[0].id,
            Verdict::Deny,
            Actor {
                kind: ActorKind::Operator,
                id: "boss".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(rt.pending_approvals().len(), 1);

        // Flush past the quiet threshold.
        let clock = Arc::new(FakeClock::new(base + DIGEST_QUIET_MS + 2 * MINUTE_MS));
        let scheduler = CompanyScheduler::new(rt.clone(), &[], clock).unwrap();
        // Only the still-pending one is digested; the settled one is dropped.
        assert_eq!(scheduler.tick_digest().await.unwrap(), 1);
        // Both leave the queue: the settled one marked delivered without an
        // email, the live one delivered by the digest.
        assert!(
            rt.notifications()
                .undelivered(rt.id())
                .await
                .unwrap()
                .is_empty()
        );

        let mut sent = sender.sent();
        for _ in 0..200 {
            if !sent.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
            sent = sender.sent();
        }
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].1.subject.contains("1 approval "),
            "digest reports only the live one: {}",
            sent[0].1.subject
        );
    }

    /// The `DIGEST_MAX_MS` cap flushes even when parks never fall quiet: the
    /// newest record is recent (inside the quiet window) while the oldest has
    /// waited out the cap. Both belong to one **still-pending approval** — a real
    /// park plus a backdated companion record for that same approval id — so the
    /// approval-only digest keeps them, and the fake clock alone drives the
    /// cap-vs-quiet edge.
    #[tokio::test]
    async fn digest_flushes_on_the_max_cap_even_when_not_quiet() {
        let home_dir = tmp_home();
        let sender = Arc::new(RecordingMailSender::new());
        let rt = Arc::new(
            RuntimeBuilder::new(home_dir.path().to_path_buf(), manifest("supervised"))
                .with_brain(Arc::new(ParkBrain {
                    effect: park_effect("shell"),
                }))
                .with_mail(CompanyMail {
                    sender: sender.clone(),
                    smtp: test_smtp("ceo@acme.test"),
                })
                .with_bootstrap_admin(Some("boss@acme.test".into()))
                .build()
                .await
                .unwrap(),
        );

        // Park one real approval → it is pending, and its auto-notification is the
        // recent (not-quiet) end of the window.
        let base = crate::ports::now_millis();
        rt.run_cycle(vec![park_message()]).await.unwrap();
        let approval_id = rt.pending_approvals()[0].id.to_string();

        // A second record for that SAME approval, backdated past the cap. Both are
        // live (the approval is still pending), so the old one trips the cap while
        // the fresh one keeps the window from being quiet.
        rt.notifications()
            .append(
                rt.id(),
                &crate::ports::notifications::Notification {
                    id: "backdated".into(),
                    kind: "approval_blocked".into(),
                    subject: crate::ports::notifications::Subject {
                        kind: crate::ports::notifications::SubjectKind::Approval,
                        id: approval_id,
                    },
                    created_at: base - DIGEST_MAX_MS - 1,
                    title: "an old parked approval".into(),
                },
            )
            .await
            .unwrap();

        // Clock just after the parks: newest is seconds old (not quiet), oldest is
        // past the cap → the flush is the cap's doing alone.
        let clock = Arc::new(FakeClock::new(base + 5_000));
        let scheduler = CompanyScheduler::new(rt.clone(), &[], clock).unwrap();
        assert_eq!(scheduler.tick_digest().await.unwrap(), 2);
        assert!(
            rt.notifications()
                .undelivered(rt.id())
                .await
                .unwrap()
                .is_empty()
        );

        let mut sent = sender.sent();
        for _ in 0..200 {
            if !sent.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
            sent = sender.sent();
        }
        assert_eq!(sent.len(), 1, "the cap flushes one digest");
        assert_eq!(sent[0].1.to, "boss@acme.test");
    }

    /// The digest body lists every notification's title and carries no payload
    /// (issue #372's redaction line), and it agrees in number with the count.
    #[test]
    fn digest_body_lists_titles_and_never_payload() {
        let note = |id: &str, title: &str| crate::ports::notifications::Notification {
            id: id.into(),
            kind: "approval_blocked".into(),
            subject: crate::ports::notifications::Subject {
                kind: crate::ports::notifications::SubjectKind::Approval,
                id: format!("appr-{id}"),
            },
            created_at: 0,
            title: title.into(),
        };

        let body = build_digest_body(
            "Acme",
            &[
                note("1", "send the $5000 invoice"),
                note("2", "publish the post"),
            ],
        );
        assert!(body.contains("2 approvals in Acme are waiting"), "{body}");
        assert!(body.contains("send the $5000 invoice"));
        assert!(body.contains("publish the post"));
        assert!(body.contains("approve or deny"));

        // Singular grammar for one, and the subject id (a would-be payload leak)
        // never appears — only the title does.
        let one = build_digest_body("Acme", &[note("9", "one thing")]);
        assert!(one.contains("1 approval in Acme is waiting"), "{one}");
        assert!(one.contains("one thing"));
        assert!(
            !one.contains("appr-9"),
            "the digest must not leak subject ids"
        );
    }
}
