use super::tests_core::*;

/// A manifest upgrade that widens the allow-list into a BYO namespace must
/// not hand billing to persisted teammates whose grant was left unstated
/// (#788). Their pre-upgrade scope is frozen into an explicit line instead.
#[test]
fn an_upgrade_into_chargebee_preserves_the_pre_upgrade_scope_of_empty_lines() {
    let old: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\"]\n",
    )
    .expect("old manifest");
    let new: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\", \"chargebee\"]\n",
    )
    .expect("new manifest");
    let overlay_agents = vec![
        OverlayAgent {
            provider: None,
            id: "clerk".to_string(),
            name: "Clerk".to_string(),
            role: "Data Entry".to_string(),
            description: None,
            // `None` = inherit (tracks the allow-list) — the state that freezes.
            tools: None,
            skills: None,
            model: None,
            harness: None,
        },
        OverlayAgent {
            provider: None,
            id: "finance_help".to_string(),
            name: "Finance Help".to_string(),
            role: "Assistant".to_string(),
            description: None,
            tools: Some(vec!["docs.*".to_string()]),
            skills: None,
            model: None,
            harness: None,
        },
    ];

    let (migrated, _) = RuntimeBuilder::preserve_pre_upgrade_grant_scope(
        overlay_agents,
        Vec::new(),
        Some(&old),
        &new,
    );

    assert_eq!(
        migrated[0].tools,
        Some(old.tools.allow.clone()),
        "an absent (inherit) line is frozen to its pre-upgrade scope rather than silently inheriting chargebee"
    );
    assert_eq!(
        migrated[1].tools,
        Some(vec!["docs.*".to_string()]),
        "a stated grant is untouched"
    );
}

/// The console's per-agent edit half carries the same inherit-freeze rule.
/// Since #1804 `AgentOverride.tools` is a double-option: `Some(None)` is the
/// "reset this teammate to the company's standard grant" spelling (the
/// inherit state), so an upgrade into a BYO namespace must freeze it to the
/// previous allow-list as well — otherwise the override, copied across the
/// rebuild verbatim, replaces the new manifest's explicit non-billing
/// `tools` line with the widened list. `Some(Some([]))` (deny-all) and
/// `Some(Some(globs))` (narrow) state their own scope and are left untouched.
#[test]
fn an_upgrade_into_chargebee_freezes_an_empty_agent_override_scope() {
    let old: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\"]\n",
    )
    .expect("old manifest");
    let new: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\", \"chargebee\"]\n",
    )
    .expect("new manifest");
    let edits = vec![
        AgentOverride {
            agent_id: "tax_preparer".to_string(),
            // The stored spelling of "reset to the company's standard grant"
            // since #1804 — `Some(None)`, the inherit state. (An empty list
            // is `Some(Some(vec![]))`, a deny-all, and is NOT frozen.)
            tools: Some(None),
            ..Default::default()
        },
        AgentOverride {
            agent_id: "brand_strategist".to_string(),
            tools: Some(Some(vec!["docs.*".to_string()])),
            ..Default::default()
        },
    ];

    let (_, migrated) =
        RuntimeBuilder::preserve_pre_upgrade_grant_scope(Vec::new(), edits, Some(&old), &new);

    assert_eq!(
        migrated[0].tools,
        Some(Some(old.tools.allow.clone())),
        "an inherit override (Some(None)) is frozen to its pre-upgrade scope rather than silently inheriting chargebee"
    );
    assert_eq!(
        migrated[1].tools,
        Some(Some(vec!["docs.*".to_string()])),
        "a stated override grant is untouched"
    );
}

/// When the upgrade does not newly confer a BYO namespace, empty lines keep
/// tracking the allow-list as they always have.
#[test]
fn an_upgrade_without_a_new_billing_namespace_leaves_empty_lines_tracking() {
    let old: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"media\"]\n",
    )
    .expect("old manifest");
    let new: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"media\", \"search\"]\n",
    )
    .expect("new manifest");
    let overlay_agents = vec![OverlayAgent {
        provider: None,
        id: "clerk".to_string(),
        name: "Clerk".to_string(),
        role: "Data Entry".to_string(),
        description: None,
        // `None` = inherit / tracks the allow-list.
        tools: None,
        skills: None,
        model: None,
        harness: None,
    }];

    let (migrated, _) = RuntimeBuilder::preserve_pre_upgrade_grant_scope(
        overlay_agents,
        Vec::new(),
        Some(&old),
        &new,
    );

    assert!(
        migrated[0].tools.is_none(),
        "no BYO namespace was newly conferred, so the inherit line keeps tracking (stays None)"
    );
}

/// Automatic Git checkpoints are opt-in and stay off unless the operator
/// flips the switch. The default is asserted here so a silent change to the
/// host default — which would start shelling out to `git` in every agent
/// workspace — cannot slip past.
#[test]
fn workspace_git_checkpoints_default_off_and_switchable() {
    let home = tmp_home("opencompany-workspace-git-");
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n")
            .expect("manifest");
    let builder = RuntimeBuilder::new(home.path().to_path_buf(), manifest);
    assert!(
        !builder.workspace_git_enabled,
        "workspace Git checkpoints must default to off"
    );
    let enabled = builder.with_workspace_git_enabled(true);
    assert!(enabled.workspace_git_enabled);
    assert!(
        !enabled
            .with_workspace_git_enabled(false)
            .workspace_git_enabled,
        "the switch must also be able to turn checkpoints back off"
    );
}

/// The provider decorator only has value if the runtime keeps all of its
/// safe handles. This pins the overlay path specifically: direct builder
/// injection could otherwise pass while `with_memory_overlay` still drops
/// scratch, scoped facades, or the archive reader.
#[tokio::test]
async fn memory_overlay_carries_scratch_scopes_and_archive_access_to_runtime() {
    use crate::store::{FsContextStore, FsMemoryStore, MemoryOverlay};

    let home = tmp_home("opencompany-memory-overlay-");
    let memory = tempfile::tempdir().unwrap();
    let context = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let scoped = tempfile::tempdir().unwrap();
    let plain: Arc<dyn ContextStore> = Arc::new(FsContextStore::new(context.path().to_path_buf()));
    let scratch: Arc<dyn ContextStore> =
        Arc::new(FsContextStore::new(scratch.path().to_path_buf()));
    let scopes: Arc<dyn crate::store::MemoryScopes> = Arc::new(TestMemoryScopes {
        context: Arc::new(FsContextStore::new(scoped.path().to_path_buf())),
    });
    let mut overlay = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(memory.path().to_path_buf())),
        plain,
        None,
    );
    overlay.scratch = Some(scratch);
    overlay.scopes = Some(scopes);

    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let runtime = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_memory_overlay(&overlay)
        .build()
        .await
        .unwrap();

    assert!(runtime.scratch_context().is_some());
    assert!(runtime.agent_context("cto").is_some());
    assert!(runtime.desk_context("engineering").is_some());
    assert_eq!(runtime.archived_traces().await.unwrap(), Some(Vec::new()));
}

/// A live engine swap must replace the outgoing engine's memory-family
/// ports, never inherit them. `with_handover` carries the outgoing
/// runtime's ports, and `build()` used to resolve those handover-first —
/// so a rebuild that re-applied the new selection kept the old engine's
/// scratch and scope partitions (issue #1113): provider→provider, the
/// successor read the engine the swap was replacing. The overlay-applied
/// marker makes the builder's own (new) handles authoritative whenever the
/// selection was re-applied.
#[tokio::test]
async fn a_rebuild_reapplying_the_engine_replaces_the_handover_ports() {
    use crate::store::{FsContextStore, FsMemoryStore, MemoryOverlay};

    // Engine A: plain ports, no decorator (the pre-swap engine).
    let home = tmp_home("opencompany-engine-swap-");
    let mem_a = tempfile::tempdir().unwrap();
    let ctx_a = tempfile::tempdir().unwrap();
    let overlay_a = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem_a.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx_a.path().to_path_buf())),
        None,
    );
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let first = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
        .with_memory_overlay(&overlay_a)
        .build()
        .await
        .unwrap();
    assert!(
        first.scratch_context().is_none(),
        "engine A has no decorator"
    );

    // Engine B: adds scratch and scope partitions, so the swap is
    // observable — the successor must carry B's, not A's (none).
    let mem_b = tempfile::tempdir().unwrap();
    let ctx_b = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let scoped = tempfile::tempdir().unwrap();
    let mut overlay_b = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem_b.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx_b.path().to_path_buf())),
        None,
    );
    overlay_b.scratch = Some(Arc::new(FsContextStore::new(scratch.path().to_path_buf())));
    overlay_b.scopes = Some(Arc::new(TestMemoryScopes {
        context: Arc::new(FsContextStore::new(scoped.path().to_path_buf())),
    }));

    let swapped = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_memory_overlay(&overlay_b)
        .with_handover(first.handover())
        .build()
        .await
        .unwrap();
    assert!(
        swapped.scratch_context().is_some(),
        "the swapped engine's scratch partition must win over the handover's none"
    );
    assert!(
        swapped.agent_context("cto").is_some(),
        "the swapped engine's scope partition must win over the handover's none"
    );
}

/// Issue #1781 review (Codex P1): `register_company`'s `serve` boot loop
/// always loads through `CompanyManifest::from_path_for_reload`, which
/// grandfathers a `RESERVED_AGENT_IDS` collision so an already-running
/// company survives a reservation rule that tightened after its
/// `company.toml` was written (`b80c45e2c`, `76c6cacdf`). That relaxation
/// was applied unconditionally, so a manifest hand-authored *after*
/// `operator` became reserved — one this store has never seen — booted
/// exactly as quietly as a genuine legacy one. `build()` now refuses this
/// case: `existing.is_none()` (no persisted record for this id) plus a
/// manifest that only clears the relaxed loader, never the strict one, is
/// not a restart to grandfather — it is a fresh authoring mistake.
#[tokio::test]
async fn first_boot_refuses_a_fresh_manifest_claiming_the_reserved_operator_agent_id() {
    let home = tmp_home("opencompany-first-boot-reserved-id-");
    let manifest: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n",
    )
    .expect("manifest");

    let err = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .build()
        .await
        .expect_err("a first-ever boot must not grandfather a brand new `operator` agent");
    match err {
        crate::OpenCompanyError::ManifestInvalid { problems, .. } => {
            assert!(
                problems.iter().any(|p| p.contains("operator")),
                "expected a reserved-id problem, got: {problems:?}"
            );
        }
        other => panic!("expected ManifestInvalid, got {other}"),
    }
}

/// The twin of the test above, proving the fix does not regress
/// `b80c45e2c`'s grandfather case: a company whose **stored** record
/// already carries the reserved `operator` agent id — a collision that
/// predates the reservation rule, not one an operator just introduced —
/// must still boot.
///
/// Seeded by writing the `CompanyRecord` straight to the store rather
/// than via a first `build()`, because `build()` itself enforces the
/// strict, unrelaxed `validate()` whenever `existing.is_none()`
/// (`861a8fbad`) — a bare first boot with this manifest would already be
/// refused by `first_boot_refuses_a_fresh_manifest_claiming_the_reserved_operator_agent_id`
/// above, never reaching the grandfather case this test means to prove.
/// Writing the record directly is exactly how a real grandfathered
/// company got here in production: its `company.toml` was accepted, and
/// its record written, before the rule existed at all.
#[tokio::test]
async fn a_reboot_still_grandfathers_an_already_registered_operator_agent_id() {
    let home = tmp_home("opencompany-reboot-reserved-id-");
    let reserved: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\n\
         [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n",
    )
    .expect("manifest");
    let id = company_id_from_name("Acme");
    FsCompanyStore::new(home.path())
        .save(&CompanyRecord {
            id: id.clone(),
            manifest: reserved.clone(),
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
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_desk_hive: Vec::new(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();

    RuntimeBuilder::new(home.path().to_path_buf(), reserved)
        .with_id(id)
        .build()
        .await
        .expect(
            "a company whose STORED record already carries this collision must still \
             reboot, even though the manifest being loaded only clears the relaxed loader",
        );
}

/// The twin of the test above from the other direction: a reboot whose
/// manifest *newly* adds a reserved-id agent — one the stored record does
/// not carry — must be refused exactly as a first boot would be. This is
/// the vulnerability Codex flagged on #1781: `existing.is_some()` used to
/// be the entire test, so an operator could edit `company.toml` between
/// two restarts to mint `operator` (or `system`/`main`/`general`) and the
/// very next `serve` boot excused it as if it had always been there.
#[tokio::test]
async fn a_reboot_refuses_a_newly_introduced_reserved_agent_id() {
    let home = tmp_home("opencompany-reboot-new-reserved-id-");
    let safe: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n")
            .expect("manifest");
    RuntimeBuilder::new(home.path().to_path_buf(), safe)
        .build()
        .await
        .expect("the first boot with a safe manifest must succeed and persist a record");

    let reserved: CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\n\
         [[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n",
    )
    .expect("manifest");
    let err = RuntimeBuilder::new(home.path().to_path_buf(), reserved)
        .build()
        .await
        .expect_err(
            "editing company.toml to add a reserved id between two restarts must not be \
             excused just because a record already existed",
        );
    match err {
        crate::OpenCompanyError::ManifestInvalid { problems, .. } => {
            assert!(
                problems.iter().any(|p| p.contains("operator")),
                "expected a reserved-id problem, got: {problems:?}"
            );
        }
        other => panic!("expected ManifestInvalid, got {other}"),
    }
}

/// The mirror: switching to the base backend must drop the outgoing
/// provider's decorator, not inherit it. The handover carries the
/// provider's scratch and scope partitions, and a rebuild that applied no
/// overlay used to inherit them anyway — so a company switched to `store`
/// kept reading the provider it just deselected. The overlay-cleared marker
/// resolves the builder's own (absent) ports instead, which is the base
/// backend's honest answer.
#[tokio::test]
async fn a_rebuild_clearing_the_engine_drops_the_handover_decorator() {
    use crate::store::{FsContextStore, FsMemoryStore, MemoryOverlay};

    let home = tmp_home("opencompany-engine-clear-");
    let mem = tempfile::tempdir().unwrap();
    let ctx = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let scoped = tempfile::tempdir().unwrap();
    let mut overlay = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx.path().to_path_buf())),
        None,
    );
    overlay.scratch = Some(Arc::new(FsContextStore::new(scratch.path().to_path_buf())));
    overlay.scopes = Some(Arc::new(TestMemoryScopes {
        context: Arc::new(FsContextStore::new(scoped.path().to_path_buf())),
    }));
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let first = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
        .with_memory_overlay(&overlay)
        .build()
        .await
        .unwrap();
    assert!(
        first.scratch_context().is_some(),
        "engine A has a decorator"
    );

    let cleared = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_memory_overlay_cleared()
        .with_handover(first.handover())
        .build()
        .await
        .unwrap();
    assert!(
        cleared.scratch_context().is_none(),
        "the base backend has no scratch partition; the provider's must not be inherited"
    );
    assert!(
        cleared.agent_context("cto").is_none(),
        "the base backend has no scope partitions; the provider's must not be inherited"
    );
}

/// The scratch/scopes swaps above leave `ops.facts` untouched: the ops
/// struct is inherited wholesale on a rebuild, so the fact store stayed on
/// the outgoing engine while memory and context moved to the new one — a
/// fact created after a live engine swap was written to the deselected
/// engine while its recall mirror went to the new context store. This pins
/// the override that keeps `facts` on the selected engine's port family.
#[tokio::test]
async fn a_rebuild_reapplying_the_engine_replaces_the_fact_store() {
    use crate::store::{FsContextStore, FsMemoryStore, FsOps, MemoryOverlay};

    // Engine A serves facts; the swap to B must re-point `ops.facts` at B's
    // store, not keep A's.
    let home = tmp_home("opencompany-engine-fact-swap-");
    let mem_a = tempfile::tempdir().unwrap();
    let ctx_a = tempfile::tempdir().unwrap();
    let facts_dir_a = tempfile::tempdir().unwrap();
    let facts_a: Arc<dyn FactStore> = Arc::new(FsOps::new(facts_dir_a.path().to_path_buf()));
    let mut overlay_a = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem_a.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx_a.path().to_path_buf())),
        None,
    );
    overlay_a.facts = Some(facts_a.clone());
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let first = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
        .with_memory_overlay(&overlay_a)
        .build()
        .await
        .unwrap();
    assert!(
        Arc::ptr_eq(first.facts(), &facts_a),
        "engine A's facts are the runtime's before the swap"
    );

    let mem_b = tempfile::tempdir().unwrap();
    let ctx_b = tempfile::tempdir().unwrap();
    let facts_dir_b = tempfile::tempdir().unwrap();
    let facts_b: Arc<dyn FactStore> = Arc::new(FsOps::new(facts_dir_b.path().to_path_buf()));
    let mut overlay_b = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem_b.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx_b.path().to_path_buf())),
        None,
    );
    overlay_b.facts = Some(facts_b.clone());

    let swapped = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
        .with_memory_overlay(&overlay_b)
        .with_handover(first.handover())
        .build()
        .await
        .unwrap();
    assert!(
        Arc::ptr_eq(swapped.facts(), &facts_b),
        "the swapped engine's facts must win over the handover's engine A store"
    );

    // The mirror: switching to the base backend drops the provider's fact
    // store back onto the base backend, exactly as the first-construction
    // branch does for an engine that serves no facts.
    let cleared = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_memory_overlay_cleared()
        .with_handover(first.handover())
        .build()
        .await
        .unwrap();
    assert!(
        !Arc::ptr_eq(cleared.facts(), &facts_a),
        "switching to `store` must drop the outgoing provider's fact store"
    );
}

/// Issue #1113 wiring: a live engine swap must move the selection marker on
/// the inherited harness pool, so the pool can drop the cached roster on
/// the next rebuild and `ensure` can fold the replacement ports into new
/// agents.
///
/// The pool-level contract (roster dropped, replacement store read) is
/// covered in `harness::built_in`; this pins the builder half — every build
/// re-records the selection, an unchanged one is a no-op, and a swap moves
/// the marker.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_rebuild_over_a_swapped_engine_rebinds_the_harness_pool() {
    use crate::store::{FsContextStore, FsMemoryStore, MemoryOverlay};

    let home = tmp_home("opencompany-engine-pool-");
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let pool = Arc::new(crate::harness::HarnessPool::new());

    let mem_a = tempfile::tempdir().unwrap();
    let ctx_a = tempfile::tempdir().unwrap();
    let overlay_a = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem_a.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx_a.path().to_path_buf())),
        None,
    );
    let first = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
        .with_memory_overlay(&overlay_a)
        .with_harness(pool.clone())
        .build()
        .await
        .unwrap();
    let id = first.id().clone();
    let fp_a = pool.memory_engine(&id).await;
    assert!(
        fp_a.is_some(),
        "boot records the engine selection on the pool"
    );

    // A rebuild that re-applies the same engine is a no-op: same marker,
    // so the pool keeps the roster (conversation history intact).
    let again = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
        .with_memory_overlay(&overlay_a)
        .with_harness(pool.clone())
        .with_handover(first.handover())
        .build()
        .await
        .unwrap();
    assert_eq!(
        pool.memory_engine(&id).await,
        fp_a,
        "re-applying the same engine keeps the same marker"
    );
    drop(again);

    // A live swap to engine B moves the marker, so the pool can tell the
    // cached roster is stale and drop it on the next build.
    let mem_b = tempfile::tempdir().unwrap();
    let ctx_b = tempfile::tempdir().unwrap();
    let overlay_b = MemoryOverlay::test_with_ports(
        Arc::new(FsMemoryStore::new(mem_b.path().to_path_buf())),
        Arc::new(FsContextStore::new(ctx_b.path().to_path_buf())),
        None,
    );
    let swapped = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_memory_overlay(&overlay_b)
        .with_harness(pool.clone())
        .with_handover(first.handover())
        .build()
        .await
        .unwrap();
    let fp_b = pool.memory_engine(&id).await;
    assert!(
        fp_b.is_some(),
        "the swap re-records the engine selection on the pool"
    );
    assert_ne!(
        fp_b, fp_a,
        "a different engine must move the marker, or the pool cannot tell a swap from a no-op"
    );
    drop(swapped);
}
