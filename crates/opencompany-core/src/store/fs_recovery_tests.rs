use super::tests::tmp_root;
use super::tests_company_store::sample_manifest;
use super::*;

/// **Issue #1828 review, tenth round**: `commit_staged` returned a plain
/// `Err` for two very different states — the rename never happened, or
/// the rename landed and only `sync_parent_dir` failed. The rollback
/// added in the ninth round compensated identically for both, so a
/// post-rename sync failure on `meta.json` restored the *old* manifest
/// while readers already saw the *new* metadata: precisely the mixed
/// record the rollback exists to prevent.
///
/// `fault_probe::fail_next_dir_sync` reaches that state, which
/// `fail_next_commit` cannot — it fires before the rename. The manifest
/// must stay as the save left it, not be rolled back.
#[tokio::test]
async fn a_post_rename_sync_failure_does_not_roll_the_manifest_back() {
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

    store
        .save(&record_named("Before"))
        .await
        .expect("first publish");

    // Fail only the durability step of the *second* commit, after its
    // rename has already replaced meta.json.
    fault_probe::fail_next_dir_sync(&bundle.meta_json());
    let err = store.save(&record_named("After")).await;
    assert!(
        err.is_err(),
        "a post-rename sync failure must still be reported to the caller"
    );

    let manifest = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
    assert!(
        manifest.contains("After") && !manifest.contains("Before"),
        "meta.json was already published, so the manifest must NOT be \
             rolled back onto it — found {manifest}"
    );
}

/// **Issue #1828 review, twelfth round follow-up** (finding on
/// 3878400724): a post-rename directory-sync failure on the *first*
/// commit (`company.toml`, update path) used to be treated exactly like
/// a total failure — the staged `meta.json` was discarded and never
/// even attempted, even though the manifest rename had already landed.
/// Readers then saw the new manifest paired with the old metadata: the
/// same mixed record the *second* commit's `published` branch (tenth
/// round, the test above) already guards against, just on the other
/// commit.
///
/// `fault_probe::fail_next_dir_sync` reaches the "rename landed, sync
/// alone failed" state on the *first* commit specifically. The metadata
/// must still land.
#[tokio::test]
async fn a_first_commit_sync_failure_still_lands_the_paired_metadata() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = |name: &str, lifecycle: &str| CompanyRecord {
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

    store
        .save(&record("Before", "running"))
        .await
        .expect("first publish");

    // Fail only the durability step of the *first* commit — the rename
    // that replaces company.toml has already landed by the time this
    // fires.
    fault_probe::fail_next_dir_sync(&bundle.company_toml());
    let err = store.save(&record("After", "paused")).await;
    assert!(
        err.is_err(),
        "a post-rename sync failure must still be reported to the caller"
    );

    let manifest = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
    assert!(
        manifest.contains("After") && !manifest.contains("Before"),
        "the manifest rename already landed and must not be rolled back — found {manifest}"
    );

    let meta = std::fs::read_to_string(bundle.meta_json()).expect("meta.json on disk");
    assert!(
        meta.contains("paused"),
        "the metadata write must not be abandoned just because the manifest's own \
             directory sync failed after its rename already landed — found {meta}"
    );
}

/// **Issue #1828 review, twelfth round follow-up** (finding on
/// 3878400729): before this round, `guard` still watched `meta_tmp`
/// while the *first* commit (`company.toml`, update path) was in
/// flight. `commit_staged`'s rename is deliberately uncancellable
/// (sixth/eleventh rounds), so cancelling `save` there dropped `guard`
/// synchronously — reclaiming `meta_tmp` — while the detached rename
/// could still land moments later, publishing the new manifest against
/// metadata that never got a chance to commit.
///
/// `stall_probe::arm_commit` parks `commit_staged`'s blocking closure
/// just before the rename — so it is genuinely about to run, not merely
/// staged — giving the test a deterministic window to abort while it is
/// parked, then release it. The manifest and metadata must land
/// together or not at all.
#[tokio::test]
async fn cancelling_a_save_during_the_first_commit_does_not_orphan_the_second() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = |name: &str, lifecycle: &str| CompanyRecord {
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

    store
        .save(&record("Before", "running"))
        .await
        .expect("first publish");

    // The update path commits company.toml first — park that rename in
    // flight, then abort the save while it is held there.
    let gate = stall_probe::arm_commit(&bundle.company_toml());
    let after = record("After", "paused");
    let reader = FsCompanyStore::new(&root);
    let handle = tokio::spawn(async move { store.save(&after).await });
    gate.wait().await;
    handle.abort();
    let joined = handle.await;
    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "the save task must actually have been cancelled for this test \
             to mean anything, got {joined:?}"
    );
    gate.release().expect("stall gate still open");

    // The detached commit unit keeps running after cancellation — give
    // it a moment to finish landing (or fully bailing on) both files.
    let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
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
        settled.is_ok(),
        "a cancelled save must not strand a staged temp in {}",
        bundle_dir.display()
    );

    let loaded = reader
        .load(&id)
        .await
        .expect("load must not error")
        .expect("the bundle must remain loadable after a cancelled update");
    let manifest_updated = loaded.manifest.company.name == "After";
    let meta_updated = loaded.lifecycle == "paused";
    assert_eq!(
        manifest_updated, meta_updated,
        "the manifest and metadata must land together or not at all after a \
             cancelled save (manifest updated = {manifest_updated}, metadata updated \
             = {meta_updated})"
    );
}

/// **Issue #1828 review, finding on 3878896036**: `company_write_lock` —
/// the per-company serialization every load-mutate-save caller relies on
/// — lives on the *caller's* frame, not on the detached commit task the
/// twelfth round introduced. Cancel a caller while it awaits `commit_rx`
/// and that guard drops immediately even though the commit it was
/// guarding is still renaming files in the background; nothing used to
/// stop a fresh caller from acquiring the now-free lock and starting its
/// own save for the same bundle while the orphaned commit was still in
/// flight.
///
/// Reuses `stall_probe::arm_commit` to park the cancelled call's first
/// rename in flight, aborts its caller, then proves a concurrent second
/// save for the same bundle cannot complete while that orphaned commit
/// is still parked — and that once it is released, the second save's
/// content is what survives, not a mix with (or an overwrite by) the
/// cancelled one.
#[tokio::test]
async fn abort_then_concurrent_update_does_not_race_the_orphaned_commit() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = |name: &str, lifecycle: &str| CompanyRecord {
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

    store
        .save(&record("Before", "running"))
        .await
        .expect("first publish");

    // Park the stale call's first commit (company.toml, update path) in
    // flight, then abort its caller while it is held there — the same
    // setup as the sibling test above, but this time a second, live
    // caller shows up while the first is still orphaned mid-commit.
    let gate = stall_probe::arm_commit(&bundle.company_toml());
    let stale = record("Stale", "paused");
    let stale_store = FsCompanyStore::new(&root);
    let stale_handle = tokio::spawn(async move { stale_store.save(&stale).await });
    gate.wait().await;
    stale_handle.abort();
    let joined = stale_handle.await;
    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "the stale save's caller must actually have been cancelled for this \
             test to mean anything, got {joined:?}"
    );

    // The stale call's detached commit is still parked on the rename it
    // acquired `commit_lock` for. A fresh caller's own save must not be
    // able to start committing until that lock is released, even though
    // `company_write_lock` (held by whatever calls `save` in production)
    // was already dropped when the stale caller was aborted above.
    let fresh_store = FsCompanyStore::new(&root);
    let fresh = record("Fresh", "archived");
    let mut fresh_handle = tokio::spawn(async move { fresh_store.save(&fresh).await });
    let raced_ahead =
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut fresh_handle).await;
    assert!(
        raced_ahead.is_err(),
        "a concurrent save must not complete while an orphaned commit for \
             the same bundle is still in flight — got {raced_ahead:?}"
    );

    // Release the stale commit and let it settle, then the fresh save
    // must be free to finish.
    gate.release().expect("stall gate still open");
    let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
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
        settled.is_ok(),
        "the stale, orphaned commit must not strand a staged temp in {}",
        bundle_dir.display()
    );

    let joined_fresh = tokio::time::timeout(std::time::Duration::from_secs(5), fresh_handle)
        .await
        .expect("the fresh save must complete once the orphaned commit clears")
        .expect("the fresh save's task must not panic");
    joined_fresh.expect("the fresh save must succeed");

    let reader = FsCompanyStore::new(&root);
    let loaded = reader
        .load(&id)
        .await
        .expect("load must not error")
        .expect("the bundle must remain loadable");
    assert_eq!(
        loaded.manifest.company.name, "Fresh",
        "the live, awaited save must win — not the cancelled one whose \
             detached commit was merely still in flight"
    );
    assert_eq!(
        loaded.lifecycle, "archived",
        "the live, awaited save must win — not the cancelled one whose \
             detached commit was merely still in flight"
    );
}

/// **Issue #1828 review, finding on comment 3879048530**: `commit_lock`
/// (above) only serializes the *commit* phase — it says nothing about
/// `load`, which reads `company.toml`/`meta.json` straight off disk with
/// no lock at all. A fresh caller's `company_write_lock` is freed the
/// instant a cancelled caller's frame drops, so a fresh load-mutate-save
/// cycle (every real call site — `policy.rs`, `team.rs`, …) can call
/// `load` immediately, while the previous caller's orphaned commit is
/// still parked mid-rename, and read the pre-commit record. That fresh
/// caller's own `save` then blocks on `commit_lock` until the orphaned
/// commit finishes, so the two commits never interleave — but the fresh
/// caller already merged its change onto stale data, so its save durably
/// *reverts* the orphaned commit's already-landed change the instant it
/// finally lands. This is a lost update, not a file-corruption race, so
/// `commit_lock` alone cannot see it: both commits still succeed, in the
/// correct order, on well-formed files.
///
/// Reuses `stall_probe::arm_commit` exactly as the sibling test above to
/// park an aborted caller's commit mid-rename, but this time drives a
/// real `load()` → mutate one field → `save()` cycle for the "fresh"
/// caller — the production shape the prior round's test skipped by
/// constructing its second `CompanyRecord` directly — so a `load` that
/// races ahead of the still-parked commit is actually exercised.
#[tokio::test]
async fn a_racing_load_does_not_lose_an_orphaned_commits_update() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = |name: &str, lifecycle: &str| CompanyRecord {
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

    store
        .save(&record("Before", "running"))
        .await
        .expect("first publish");

    // Park the stale call's first commit (company.toml, update path) in
    // flight, exactly as the sibling test above, then abort its caller
    // while it is held there.
    let gate = stall_probe::arm_commit(&bundle.company_toml());
    let stale = record("Before", "paused");
    let stale_store = FsCompanyStore::new(&root);
    let stale_handle = tokio::spawn(async move { stale_store.save(&stale).await });
    gate.wait().await;
    stale_handle.abort();
    let joined = stale_handle.await;
    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "the stale save's caller must actually have been cancelled for this \
             test to mean anything, got {joined:?}"
    );

    // The stale call's detached commit is still parked mid-rename,
    // holding `commit_lock`, with `lifecycle: "paused"` not yet on disk.
    // A fresh caller now runs the real production sequence: `load`,
    // touch one unrelated field, `save` back the merged record — the
    // same shape `policy.rs`/`team.rs` use. Spawned rather than awaited
    // inline so the still-parked commit above cannot block this task
    // from being scheduled at all.
    let fresh_store = FsCompanyStore::new(&root);
    let fresh_id = id.clone();
    let mut fresh_handle = tokio::spawn(async move {
        let mut loaded = fresh_store
            .load(&fresh_id)
            .await
            .expect("load must not error")
            .expect("bundle must exist");
        loaded.manifest.company.name = "Fresh".to_string();
        // Deliberately NOT touching `lifecycle` — mirrors a real caller
        // that only mutates the field it owns and carries the rest of
        // the loaded record through untouched.
        fresh_store.save(&loaded).await
    });

    // Give the fresh task a real window to run while the orphaned commit
    // is still parked. If `load` is unguarded, it finishes almost
    // immediately (well inside this window) and reads the pre-commit
    // "running" lifecycle; if `load` waits on the same lock the commit
    // holds, this whole task is still blocked when the window ends.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Release the stale commit and let it land.
    gate.release().expect("stall gate still open");
    let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
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
        settled.is_ok(),
        "the stale, orphaned commit must not strand a staged temp in {}",
        bundle_dir.display()
    );

    let joined_fresh = tokio::time::timeout(std::time::Duration::from_secs(5), &mut fresh_handle)
        .await
        .expect("the fresh save must complete once the orphaned commit clears")
        .expect("the fresh save's task must not panic");
    joined_fresh.expect("the fresh save must succeed");

    let reader = FsCompanyStore::new(&root);
    let loaded = reader
        .load(&id)
        .await
        .expect("load must not error")
        .expect("the bundle must remain loadable");
    assert_eq!(
        loaded.manifest.company.name, "Fresh",
        "the fresh caller's own change must land"
    );
    assert_eq!(
        loaded.lifecycle, "paused",
        "the orphaned commit's `lifecycle: \"paused\"` update landed on disk \
             before the fresh save committed, so a fresh save that carries \
             forward whatever it loaded must not silently revert it back to \
             \"running\" — that is a lost update, even though both commits \
             individually succeeded on well-formed files"
    );
}
