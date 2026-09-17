use super::tests::tmp_root;
use super::tests_company_store::sample_manifest;
use super::*;

/// **Issue #1828 review, ninth round**: on the update path the two files
/// are published by two independent renames, `company.toml` first. If the
/// `meta.json` commit then fails, the manifest edit is already durable
/// while `save` returns `Err` — so a logo or name change persists even
/// though the caller was told the save failed, and a call that changed
/// both files leaves a mixed record.
///
/// Drives it with `fault_probe::fail_next_commit`, which fails the
/// rename step rather than the staging write, and asserts the manifest
/// on disk is the one from before the failed save.
#[tokio::test]
async fn a_failed_meta_commit_rolls_the_published_manifest_back() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record_named = |name: &str| CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: {
            let mut m = sample_manifest();
            m.company.name = name.to_string();
            m
        },
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    };

    // First publish, so the next save takes the update path.
    store
        .save(&record_named("Before"))
        .await
        .expect("first publish");
    let published = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
    assert!(
        published.contains("Before"),
        "precondition: the first save must have published the original name"
    );

    // Now fail only the *second* commit of the update.
    fault_probe::fail_next_commit(&bundle.meta_json());
    let err = store.save(&record_named("After")).await;
    assert!(
        err.is_err(),
        "the injected meta.json commit failure must propagate out of save"
    );

    let after = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
    assert!(
        after.contains("Before") && !after.contains("After"),
        "a save that reported failure must not leave its manifest edit \
             published — found {after}"
    );

    let orphans = std::fs::read_dir(bundle.company_toml().parent().unwrap())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .count();
    assert_eq!(
        orphans, 0,
        "the rollback must not leave its own temp behind"
    );
}

/// **Issue #1828 review, eighth round**: the guard closed the "dropped
/// mid-stage" hole, but its first version disarmed *before* awaiting the
/// error path's `remove_staged`. Aborting the task inside that await
/// therefore reopened the very hole the guard exists to close: the
/// cleanup never finished, and `Drop` no longer had `meta_tmp` to
/// reclaim.
///
/// Fault plus cancellation, as the review asked for: fail the second
/// stage so the error path runs, park `remove_staged` with
/// `cleanup_probe`, then abort. Pre-fix the `meta.tmp-<id>` survives;
/// post-fix the still-armed guard reclaims it as the frame unwinds.
#[tokio::test]
async fn aborting_during_the_error_path_cleanup_still_reclaims_the_staged_temp_file() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
        ledger: Vec::new(),
        lifecycle: "provisioning".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    };

    let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&bundle_dir).expect("bundle dir");

    // Fail the *second* stage so `save` takes its error path, and park
    // the cleanup that path awaits.
    fault_probe::fail_next_write(&bundle.company_toml());
    let cleanup_gate = cleanup_probe::arm(&bundle_dir);

    let handle = tokio::spawn(async move { store.save(&record).await });

    cleanup_gate.wait().await;
    handle.abort();
    let joined = handle.await;
    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "the save task must actually have been cancelled inside the \
             cleanup await for this test to mean anything, got {joined:?}"
    );

    let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let orphans = std::fs::read_dir(&bundle_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                .count();
            if orphans == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;

    assert!(
        cleaned_up.is_ok(),
        "aborting inside the error path's remove_staged must still \
             reclaim the staged temp — an orphan sat in {} for the whole \
             timeout",
        bundle_dir.display()
    );
}

/// **Issue #1828 review, seventh round**: the sibling test below covers a
/// second-stage write that *fails*. This covers the second-stage write
/// that never returns at all because the caller went away — an aborted
/// task, or an axum handler cancelled by a client disconnect, which is a
/// reachable path since `save` is called from the operator routes.
///
/// `save` stages `meta.json`, then awaits the staging of `company.toml`.
/// Dropped in that window, none of its explicit `remove_staged` branches
/// runs, and the already-fsynced `meta.tmp-<id>` loses its only handle.
/// The second stage cleans up after itself (sixth round), so pre-fix the
/// bundle is left with exactly one orphan; post-fix `StagedGuard`'s
/// `Drop` reclaims it as the frame unwinds.
#[tokio::test]
async fn dropping_save_between_its_two_stages_does_not_strand_the_first_temp_file() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
        ledger: Vec::new(),
        lifecycle: "provisioning".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    };

    // Park the *second* stage, so the abort below lands squarely in the
    // window where `meta.json` is staged and `company.toml` is not.
    let gate = stall_probe::arm(&bundle.company_toml());

    let handle = tokio::spawn(async move { store.save(&record).await });

    gate.wait().await;
    handle.abort();
    let joined = handle.await;
    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "the task running save must actually have been cancelled for \
             this test to mean anything, got {joined:?}"
    );

    // Let the parked second stage finish; it reclaims its own temp.
    gate.release().expect("stall gate still open");

    let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
    let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let orphans = std::fs::read_dir(&bundle_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                .count();
            if orphans == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;

    assert!(
        cleaned_up.is_ok(),
        "dropping save between its two stages must not strand the \
             meta.json temp it had already staged — an orphan sat in {} for \
             the whole timeout",
        bundle_dir.display()
    );
}

/// bundle directory afterward. Pre-fix, `meta.tmp-<id>` survives the
/// failed `save` call; post-fix, `save`'s error path removes it before
/// returning.
#[tokio::test]
async fn a_failed_second_stage_write_does_not_strand_the_first_staged_temp_file() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = || CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
        ledger: Vec::new(),
        lifecycle: "provisioning".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    };

    fn staged_tmp_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tmp-"))
            })
            .collect()
    }

    // Staging order is fixed regardless of the commit-order branch:
    // `save` stages `meta.json` first, then `company.toml`. Arming the
    // fault on `company.toml`'s write fails the *second* stage call,
    // after `meta.json`'s temp file has already landed on disk.
    fault_probe::fail_next_write(&bundle.company_toml());

    let err = store.save(&record()).await;
    assert!(
        err.is_err(),
        "the injected staging failure must propagate out of save"
    );

    assert!(
        !tokio::fs::try_exists(&bundle.company_toml()).await.unwrap(),
        "company.toml must not exist after a staging failure"
    );
    assert!(
        !tokio::fs::try_exists(&bundle.meta_json()).await.unwrap(),
        "meta.json must not exist after a staging failure"
    );
    assert_eq!(
        staged_tmp_files(bundle.dir()),
        Vec::<std::path::PathBuf>::new(),
        "a failed second stage write must not strand the temp file the \
             first stage write already committed to disk — every retry into \
             a persistent fault (e.g. a full disk) would otherwise leak \
             another one"
    );

    // `fail_next_write` is one-shot, so the retry hits the real write
    // path and publishes normally, with no temp files left behind
    // either.
    store
        .save(&record())
        .await
        .expect("retry succeeds once the fault is no longer armed");
    assert!(store.load(&id).await.unwrap().is_some());
    assert_eq!(
        staged_tmp_files(bundle.dir()),
        Vec::<std::path::PathBuf>::new(),
        "a successful save must not leave any staged temp file behind \
             either"
    );
}

/// **Issue #1828 review, sixth round**: the fifth round's `remove_staged`
/// calls only run on `save`'s own error path — code that never executes
/// if `save`'s caller is cancelled before that path is reached.
/// `spawn_blocking` cannot be cancelled: dropping its `JoinHandle` future
/// stops nothing, it only discards the result. So if the future calling
/// `stage_atomic_bytes` is itself dropped while parked on that await —
/// exactly what happens when an axum handler is cancelled by a client
/// disconnect mid-`save`, or a task is `abort()`ed — the write finishes
/// on the blocking pool regardless, but the only reference to the temp
/// path it wrote is gone with the dropped future. Neither `commit_staged`
/// nor `remove_staged` ever runs, and the fully written, fsynced temp
/// file is orphaned for good.
///
/// This proves it with `stall_probe`: park the write mid-flight, `abort`
/// the task awaiting `stage_atomic_bytes` (simulating the cancellation),
/// then release the write and confirm it still lands on disk (proving
/// cancellation didn't stop it — that's the hazard, not the fix). Pre-fix
/// the temp file then sits there forever; post-fix the detached task
/// notices nobody claimed the result and removes it itself.
#[tokio::test]
async fn cancelling_the_caller_does_not_strand_the_staged_temp_file() {
    let root_dir = tmp_root();
    let target = root_dir.path().join("bundle").join("company.toml");

    let gate = stall_probe::arm(&target);

    let awaited_target = target.clone();
    let handle = tokio::spawn(async move { stage_atomic_bytes(&awaited_target, b"hello").await });

    // Deterministic rendezvous: the write closure has reached the gate
    // and parked, so aborting now is guaranteed to land on the await
    // this test is exercising, not before or after it.
    gate.wait().await;

    handle.abort();
    let joined = handle.await;
    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "the task awaiting stage_atomic_bytes must actually have been \
             cancelled for this test to mean anything, got {joined:?}"
    );

    // Let the parked write proceed. Nothing above the blocking pool is
    // watching it anymore — this is the crux of the hazard: the write
    // was never cancellable, only the caller's ability to hear about it.
    gate.release().expect("stall gate still open");

    // Poll for the temp file's fate instead of a fixed sleep: the
    // detached cleanup task needs a moment to resume after the blocking
    // write returns. Pre-fix there is nothing to wait for — the file
    // sits there for the lifetime of the test (and the process) — so
    // this loop only terminates via the timeout, which is the failing
    // signal on unpatched code.
    let bundle_dir = target.parent().unwrap().to_path_buf();
    let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let has_orphan = std::fs::read_dir(&bundle_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
            if !has_orphan {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;

    assert!(
        cleaned_up.is_ok(),
        "cancelling the caller must not strand the temp file \
             stage_atomic_bytes already wrote and fsynced — it sat in {} \
             for the whole timeout",
        bundle_dir.display()
    );
}

/// **Issue #1828 review, seventeenth round** (finding on comment
/// 3878696002): the sixth round's fix reclaims `tmp` when `tx.send`
/// *fails* — i.e. when `rx` was already dropped before the send was
/// attempted. It does nothing for the other order: `send` succeeds
/// (the `Receiver` was still alive at that instant) but the future
/// awaiting it is dropped before it is ever polled again to actually
/// consume the value. `send` completing and the awaiting task resuming
/// are two independently-scheduled events, so that gap is real — a
/// successfully-sent, never-consumed oneshot value is silently dropped,
/// and with it the only other reference to `tmp`.
///
/// This can't be proven by aborting a spawned task and hoping the
/// timing lines up, the way the sixth round's test does — that races
/// two independently-scheduled tasks against wall-clock time. Instead,
/// `send_probe` fires the instant `tx.send` returns `Ok`, and this test
/// races that notification against the awaited future itself in a
/// `biased` `select!`: whichever branch is checked first and found
/// ready wins outright within a single poll, so once the notification
/// fires, the awaited future is provably *not yet re-polled* to
/// retrieve the value — dropping it right there reproduces "sent
/// successfully, never consumed" deterministically, no sleeps involved.
///
/// Pre-fix (before the local `StagedGuard` in `stage_atomic_bytes`) the
/// temp file is orphaned forever: the detached task saw `send` succeed
/// and did no cleanup, and the caller never got `tmp` back to hand to
/// `commit_staged` / `remove_staged` / `save`'s own guard. Post-fix the
/// local guard's `Drop` reclaims it as soon as this test's `drop(fut)`
/// runs.
#[tokio::test]
async fn dropping_the_caller_after_a_successful_send_does_not_strand_the_temp_file() {
    let root_dir = tmp_root();
    let target = root_dir.path().join("bundle").join("company.toml");

    let notify = send_probe::arm(&target);

    // `Box::pin`, not `tokio::pin!`: the latter shadows `fut` with a
    // `Pin<&mut F>` into a hidden stack slot that outlives this
    // function's scope, so a later `drop(fut)` would only drop that
    // reference — the real future (and the `StagedGuard` living inside
    // it) would stay alive, silently defeating this whole test. `Box`
    // makes `fut` the actual owner, so dropping it drops the future for
    // real.
    let mut fut = Box::pin(stage_atomic_bytes(&target, b"hello"));

    tokio::select! {
        biased;
        _ = notify.notified() => {
            // The detached task's `tx.send` has already returned `Ok`
            // — `rx` was alive at that instant, and the value is now
            // buffered inside it. `biased` guarantees `&mut fut` was
            // NOT polled again in the same event that delivered this
            // notification (this arm is checked first and wins
            // outright), so `fut` is still parked on `rx.await`,
            // having never retrieved that buffered value. Falling
            // through to `drop(fut)` below discards it unconsumed —
            // exactly the race this test targets.
        }
        _ = &mut fut => {
            panic!(
                "stage_atomic_bytes resolved before send_probe's \
                 notification fired — the race window this test \
                 targets (send succeeds, caller never consumes it) \
                 was never actually exercised"
            );
        }
    }
    drop(fut);

    let bundle_dir = target.parent().unwrap().to_path_buf();
    let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let has_orphan = std::fs::read_dir(&bundle_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
            if !has_orphan {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;

    assert!(
        cleaned_up.is_ok(),
        "dropping the awaiting future after a successful but \
             never-consumed send must not strand the temp file \
             stage_atomic_bytes already wrote and fsynced — it sat in {} \
             for the whole timeout",
        bundle_dir.display()
    );
}

/// **Issue #1828 review, seventh round**: the sixth round's cleanup only
/// fires when `tx.send` fails, i.e. when the caller is gone. It does
/// nothing for the other way `result` can be `Err`: `File::create`
/// succeeds (the temp file now exists on disk) and `write_all` or
/// `sync_data` then fails. The caller is still there and gets the
/// `Err`, but never gets `tmp` back, so nothing downstream can call
/// `remove_staged` for it either — the fully-created, partially-written
/// temp file is orphaned even though nothing was cancelled.
///
/// `fault_probe::fail_next_mid_write` proves it: unlike
/// `fail_next_write`, which fails before any filesystem call, this lets
/// `File::create` succeed and fails right after, so a `.tmp-*` file is
/// on disk when the injected failure hits. Pre-fix that file survives
/// the failed call; post-fix `stage_atomic_bytes` removes it itself
/// before returning `Err`.
#[tokio::test]
async fn a_write_failure_after_create_does_not_strand_the_temp_file() {
    let root_dir = tmp_root();
    let target = root_dir.path().join("bundle").join("company.toml");

    fault_probe::fail_next_mid_write(&target);

    let err = stage_atomic_bytes(&target, b"hello").await;
    assert!(
        err.is_err(),
        "the injected mid-write failure must propagate out of stage_atomic_bytes"
    );

    let bundle_dir = target.parent().unwrap();
    let has_orphan = std::fs::read_dir(bundle_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
    assert!(
        !has_orphan,
        "a write failure after File::create succeeded must not strand \
             the temp file it already created in {}",
        bundle_dir.display()
    );
}

/// **Issue #1828 review, third round**: `save` picks its write order by
/// probing `try_exists(company.toml)` and treating a failed probe the
/// same as `Ok(false)`. For an *update* — the bundle already exists —
/// that misclassification steers the save onto the first-publish branch,
/// which writes `meta.json` (the new lifecycle) *first*. If the
/// subsequent `company.toml` write then also fails, or even if it
/// doesn't, the probe failure alone means the safety property the
/// second-round fix established (an update's first write is
/// `company.toml`, so a failure never lands the lifecycle change) no
/// longer holds — the wrong branch was taken before either file write
/// was attempted.
///
/// This proves the probe failure itself is propagated as an error from
/// `save`, rather than silently steering the branch choice: publish a
/// bundle, arm a fault on the *existence check* (not a write) for the
/// next update save, and assert `save` returns `Err` — never that it
/// silently took the first-publish branch and left `meta.json`
/// rewritten with the new lifecycle. A retry without the fault must
/// still land the update normally.
#[tokio::test]
async fn a_failed_existence_probe_during_an_update_does_not_misfire_the_first_publish_order() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = |lifecycle: &str| CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
        ledger: Vec::new(),
        lifecycle: lifecycle.to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    };

    // Publish the bundle for the first time — unaffected by the fault
    // this test arms below.
    store
        .save(&record("provisioning"))
        .await
        .expect("first publish succeeds");

    // Simulate the existence probe itself failing (a transient I/O error
    // or an ACL denial), not either write. Under the pre-fix
    // `unwrap_or(false)`, this reads as "does not exist" and `save`
    // proceeds to write `meta.json` first — the first-publish order —
    // even though the bundle is live.
    fault_probe::fail_next_exists_check(&bundle.company_toml());

    let err = store.save(&record("running")).await;
    assert!(
        err.is_err(),
        "a failed existence probe must propagate out of save, not be \
             silently read as \"bundle does not exist\""
    );

    let loaded = store
        .load(&id)
        .await
        .unwrap()
        .expect("the bundle still exists — only the probe failed");
    assert_eq!(
        loaded.lifecycle, "provisioning",
        "a failed existence probe must not let save fall through to the \
             first-publish write order and rewrite meta.json's lifecycle \
             before company.toml is even considered"
    );

    // The fault is one-shot, so the retry hits the real probe and lands
    // the update normally.
    store
        .save(&record("running"))
        .await
        .expect("retry succeeds once the fault is no longer armed");
    assert_eq!(store.load(&id).await.unwrap().unwrap().lifecycle, "running");
}
