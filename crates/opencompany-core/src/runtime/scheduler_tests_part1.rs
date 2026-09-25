use super::tests_core::*;

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
