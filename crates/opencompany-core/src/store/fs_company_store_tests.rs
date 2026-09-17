use super::tests::tmp_root;
use super::*;

pub(super) fn sample_manifest() -> crate::company::CompanyManifest {
    let toml_src = r#"
            [company]
            name = "Acme"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
        "#;
    toml::from_str(toml_src).expect("parse manifest")
}

#[tokio::test]
async fn company_store_saves_and_loads() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let record = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
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
    store.save(&record).await.unwrap();

    let loaded = store.load(&id).await.unwrap().expect("record exists");
    assert_eq!(loaded.manifest.company.name, "Acme");
    assert_eq!(loaded.lifecycle, "running");
    // One authored teammate, plus the global baseline the load path merges
    // into every stored manifest.
    assert_eq!(
        loaded
            .manifest
            .agents
            .iter()
            .filter(|agent| !agent.global)
            .count(),
        1
    );
    assert_eq!(
        loaded.manifest.agents.len(),
        1 + crate::globals::agents().len()
    );

    let summaries = store.list().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].name, "Acme");

    assert!(
        store
            .load(&CompanyId::new("ghost"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn save_publishes_the_gate_marker_before_the_manifest() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    store
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_desk_hive: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
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
        })
        .await
        .unwrap();

    let order = append_probe::write_order_for(&[&bundle.meta_json(), &bundle.company_toml()]);
    assert_eq!(
        order,
        vec![bundle.meta_json(), bundle.company_toml()],
        "meta.json (the gate marker) must land before company.toml (what \
             `load` treats as the bundle's existence) — reversed, an \
             interrupted save can auto-activate a fresh company as a legacy \
             one on its next boot"
    );
}

/// **PR #1875 review finding**: the opposite ordering rule from the test
/// above, for the opposite case — updating a bundle that is already live
/// must publish `company.toml` before `meta.json`.
///
/// Reversed (the first-publish order applied to an update too), a crash
/// between the two writes during `PATCH {scope}`'s name-confirm save
/// (`company_profile::patch_company`) can durably land `name_confirmed:
/// true` while `company.toml` still carries the pre-rename placeholder
/// name — and since `name_confirmed` is what hides the console's only
/// rename control, that mismatched pair has no way back through the UI.
/// Same reasoning as the sibling test: only the *order* the publishes
/// land in distinguishes the two outcomes, which is what
/// `append_probe::write_order_for` records.
#[tokio::test]
async fn updating_an_existing_bundle_publishes_the_manifest_before_the_gate_marker() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let first_save = CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
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
    // Publish the bundle for the first time — the create-path branch,
    // unaffected by this test's assertion.
    store.save(&first_save).await.unwrap();

    // The exact write `patch_company` performs: an existing bundle,
    // `name_confirmed` flips to `true` alongside the manifest name.
    let mut second_save = first_save;
    second_save.manifest.company.name = "Operator Chosen Name".to_string();
    second_save.name_confirmed = true;
    store.save(&second_save).await.unwrap();

    let order = append_probe::write_order_for(&[&bundle.meta_json(), &bundle.company_toml()]);
    // The log is global across both saves; the update's pair is the last
    // two entries.
    let update_order = &order[order.len() - 2..];
    assert_eq!(
        update_order,
        vec![bundle.company_toml(), bundle.meta_json()],
        "an update to an existing bundle must publish company.toml (the \
             name) before meta.json (name_confirmed) — reversed, an \
             interrupted rename save can durably confirm the wrong name with \
             no way back through the console"
    );
}

#[tokio::test]
async fn a_save_interrupted_after_the_first_write_still_reads_back_as_absent() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    fault_probe::fail_next_write(&bundle.meta_json());

    let record = || CompanyRecord {
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: sample_manifest(),
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

    let err = store.save(&record()).await;
    assert!(
        err.is_err(),
        "the injected failure must propagate out of save"
    );

    // The one combination that would make `load` lie about the company
    // existing: `company.toml` must not have landed.
    assert!(
        !tokio::fs::try_exists(&bundle.company_toml()).await.unwrap(),
        "company.toml must not exist after the first write of save failed"
    );

    let loaded = store.load(&id).await.unwrap();
    assert!(
        loaded.is_none(),
        "a save interrupted before company.toml landed must read back as \
             absent, not as an existing company — retrying create/reset for \
             this id must be possible"
    );

    // `fail_next_write` is one-shot, so the retry hits the real write path
    // and must succeed.
    store
        .save(&record())
        .await
        .expect("retry succeeds once the fault is no longer armed");
    assert!(store.load(&id).await.unwrap().is_some());
}

/// **Issue #1828 review, second round**: the sibling window the test
/// above never exercised. `meta.json` carries the lifecycle and every
/// overlay; the original fix (this file's prior revision) wrote it
/// *first*, unconditionally, for every save — first publish and update
/// alike. That order is safe for a first publish (see the test above),
/// but for an update it means a fault on `company.toml`'s write — the
/// *second* write under that old, unconditional order — still leaves the
/// lifecycle change durably on disk in `meta.json` even though `save`
/// returns `Err` to the caller: a resume that reports a 500 while
/// `lifecycle` already reads back `"running"`, with the audit event the
/// caller only appends after a successful `save` never written.
///
/// The fix makes the order conditional: an update to an already-existing
/// bundle writes `company.toml` *first*, so the same fault — on
/// `company.toml`'s write — now fails the update's *first* write instead,
/// before `meta.json` is touched at all. This proves that directly:
/// publish a bundle, then arm the fault on `company.toml` for an update
/// save, and assert the lifecycle read back afterward is still the *old*
/// value, never the new one — the update fails toward "nothing changed,"
/// not "changed anyway, but the caller was told it didn't." It then
/// retries without the fault and asserts the new lifecycle *does* land.
#[tokio::test]
async fn an_update_interrupted_on_the_second_write_does_not_persist_the_lifecycle_change() {
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

    // Publish the bundle for the first time — the create-path branch,
    // unaffected by this test's assertion.
    store
        .save(&record("provisioning"))
        .await
        .expect("first publish succeeds");

    // The update save this test exercises: a resume, flipping lifecycle
    // to "running". Under the old, unconditional order this fault landed
    // on the *second* write, after `meta.json` (the new lifecycle) had
    // already committed — the untested window the review flagged. Under
    // the fix, `save` writes `company.toml` first for an update, so this
    // same fault now fails the update's *first* write, before
    // `meta.json` is ever touched.
    fault_probe::fail_next_write(&bundle.company_toml());

    let err = store.save(&record("running")).await;
    assert!(
        err.is_err(),
        "the injected failure must propagate out of save"
    );

    let loaded = store
        .load(&id)
        .await
        .unwrap()
        .expect("the bundle still exists — only the update failed");
    assert_eq!(
        loaded.lifecycle, "provisioning",
        "a failed company.toml write during an update must leave \
             meta.json — and the lifecycle it carries — exactly as it was \
             before the update; the caller was told the resume failed, so \
             nothing about it may have taken effect"
    );

    // `fail_next_write` is one-shot, so the retry hits the real write
    // path and the lifecycle change lands normally.
    store
        .save(&record("running"))
        .await
        .expect("retry succeeds once the fault is no longer armed");
    assert_eq!(store.load(&id).await.unwrap().unwrap().lifecycle, "running");
}

/// **Issue #1828 review, fourth round**: the mirror image of the
/// second-round hazard above, on the *other* file. For an update, `save`
/// commits `company.toml` (the manifest — name, output, logo, …)
/// *before* `meta.json`, so that a failure on `meta.json` never lands a
/// lifecycle/overlay change the caller was told failed. But ordering the
/// two *writes* that way means a fault on the *second* write —
/// `meta.json` — used to let the *first* write land durably first:
/// exactly the same shape of bug as the second-round hazard, just with
/// which file survives and which is protected swapped. A real-world
/// instance: `PUT …/logo` changes `record.manifest.company.logo_url`
/// and calls `save`; a transient failure writing `meta.json` right after
/// would report the request failed while the new logo was already on
/// disk and would reappear on reload.
///
/// The fix is not a fifth reorder — reordering again would just swap the
/// hazard back. It durably stages both files (write + fsync to a temp
/// name) *before* committing (renaming) either one, so a write failure —
/// the likely failure mode a transient I/O error or a full disk actually
/// produces — never touches a live file no matter which of the two temp
/// writes fails or in what order they're attempted. This proves it:
/// publish a bundle, change the manifest, arm the fault on `meta.json`'s
/// write for the update `save`, and assert the manifest read back
/// afterward is still the *old* value — never the new one — even though
/// `meta.json` is committed second and untouched-on-disk is normally
/// where a change would be expected to survive a same-call failure.
#[tokio::test]
async fn an_update_interrupted_on_the_second_write_does_not_persist_the_manifest_change() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let store = FsCompanyStore::new(&root);
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.clone(), &id);

    let record = |company_name: &str| {
        let mut manifest = sample_manifest();
        manifest.company.name = company_name.to_string();
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_desk_hive: Vec::new(),
            id: id.clone(),
            manifest,
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
        }
    };

    // Publish the bundle for the first time — the create-path branch,
    // unaffected by this test's assertion.
    store
        .save(&record("Acme"))
        .await
        .expect("first publish succeeds");

    // The update save this test exercises: a manifest-only change (the
    // shape of the logo endpoint's `PUT …/logo`), landing via a
    // `company.toml`-first commit order. Under the pre-fix code the
    // fault below fires on the *second* write, after `company.toml`
    // (the new name) had already been written and committed — the
    // untested window the review flagged. Under the fix, both files are
    // staged before either is committed, so this same fault fails
    // before `company.toml` is ever published.
    fault_probe::fail_next_write(&bundle.meta_json());

    let err = store.save(&record("Acme Renamed")).await;
    assert!(
        err.is_err(),
        "the injected failure must propagate out of save"
    );

    let loaded = store
        .load(&id)
        .await
        .unwrap()
        .expect("the bundle still exists — only the update failed");
    assert_eq!(
        loaded.manifest.company.name, "Acme",
        "a failed meta.json write during an update must leave \
             company.toml — and the manifest fields it carries — exactly as \
             it was before the update; the caller was told the save \
             failed, so nothing about it may have taken effect"
    );

    // `fail_next_write` is one-shot, so the retry hits the real write
    // path and the manifest change lands normally.
    store
        .save(&record("Acme Renamed"))
        .await
        .expect("retry succeeds once the fault is no longer armed");
    assert_eq!(
        store
            .load(&id)
            .await
            .unwrap()
            .unwrap()
            .manifest
            .company
            .name,
        "Acme Renamed"
    );
}

/// **Issue #1828 review, fifth round**: the structural fix above (fourth
/// round) stages both files — write + fsync to a temp name — before
/// committing either, so a write failure never touches a live file. It
/// says nothing about the temp file that staging *did* manage to write
/// before the failure. If staging `meta.json` succeeds and staging
/// `company.toml` then fails, the old code let `?` return straight out
/// of `save`, leaving the fully written, fsynced `meta.tmp-*` file
/// behind — nothing ever renames it into place and nothing ever deletes
/// it. On the disk-full failure this matters most for, that is doubly
/// bad: the leaked temp file's bytes are exactly what the disk is
/// already short of, so every retry into a persistent fault stages
/// another orphan and shrinks the room available to recover in, rather
/// than the retry loop converging on either success or a clean failure.
///
/// This proves it: arm the write fault on `company.toml` for a
/// first-time publish — the same fault as the very first round's test
/// above, which already proves the *live* files stay safe — and, in
/// addition to that, assert no `*.tmp-*` file is left anywhere under the
/// **Issue #1828 review, eleventh round**: `commit_staged`'s
/// `spawn_blocking` job keeps running after the caller's future is
/// dropped, exactly like `stage_atomic_bytes`. So a save cancelled while
/// the *second* commit was in flight had `StagedGuard::drop` delete
/// `meta_tmp` out from under a rename that was still going to happen:
/// the rename then failed `NotFound`, the manifest was already
/// published, and cancellation skipped the rollback branch — leaving a
/// new manifest paired with old metadata.
///
/// Ownership of each temp now passes to `commit_staged` before the call,
/// so the guard no longer races it. Drives the case with `stall_probe`
/// on the metadata staging write and an abort while it is parked.
#[tokio::test]
async fn cancelling_a_save_does_not_delete_a_temp_a_commit_still_owns() {
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

    // Park the metadata staging write, then abort the save while it is
    // held there — the update path stages meta.json first.
    let gate = stall_probe::arm(&bundle.meta_json());
    let after = record_named("After");
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

    // Whatever the cancellation left behind, the bundle must never be a
    // new manifest paired with stale metadata, and must not accumulate
    // orphaned temps.
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

    // The record must still load — the whole hazard is a manifest whose
    // paired metadata never landed.
    let loaded = reader.load(&id).await.expect("load must not error");
    assert!(
        loaded.is_some(),
        "the bundle must remain loadable after a cancelled update"
    );
}
