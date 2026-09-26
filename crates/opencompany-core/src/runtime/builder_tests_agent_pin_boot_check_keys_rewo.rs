use super::tests_core::*;
use super::*;

/// **The invariant that would have caught #981, restored by #1757.** The
/// picker's set (`deliverable_channel_ids`) and the delivery layer's set
/// (`WorkflowDeliveryDeps.channels`) are produced by the same `build()`, and
/// must have the **same membership** — an author must never be offered a
/// target the runner refuses, nor be refused a target the picker offers.
///
/// `operator` is offered by the picker and accepted by delivery without an
/// adapter (`workflows::delivery` reports it to the operator itself), so the
/// delivery side is its wired adapters plus `operator`. Compared as sets.
///
/// Needs the harness arm, because that is the only site that wires
/// `WorkflowDeliveryDeps` at all.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn the_picker_set_and_the_delivery_deps_have_the_same_membership() {
    use crate::harness::HarnessPool;

    let home_dir = tmp_home("oc-981-invariant-");
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("invariant-co");
    let manifest = parse(
        r#"
        [company]
        name = "Invariant Co"

        [policy]
        mode = "full"

        [[agent]]
        id = "eng1"
        role = "Engineer One"

        [[group_chat]]
        id = "engineering"
        name = "Engineering"
        members = ["eng1"]
        "#,
    );

    let stub = spawn_stub("ack").await;
    let runtime = RuntimeBuilder::new(home.clone(), manifest)
        .with_id(id.clone())
        .with_harness(Arc::new(HarnessPool::new()))
        .with_harness_inference(
            HostedProviderConfig {
                base_url: stub,
                credential: crate::company::Credential::from_value("k"),
                extra_headers: Vec::new(),
            },
            None,
        )
        .build()
        .await
        .unwrap();

    let delivery = runtime
        .workflow_harness_deps
        .as_ref()
        .expect("the harness arm wires workflow deps")
        .delivery
        .as_ref()
        .expect("the harness arm wires delivery deps");
    let deps_channels: Vec<String> = delivery
        .channels
        .iter()
        .map(|channel| channel.channel_id().to_string())
        .chain(std::iter::once(OPERATOR_CHANNEL.to_string()))
        .collect();
    assert!(
        delivery
            .channels
            .iter()
            .all(|channel| channel.channel_id() != OPERATOR_CHANNEL),
        "no adapter answers for `operator`; delivery reports it itself"
    );

    // Same membership on both sides, order-independent.
    let picker = runtime.deliverable_channel_ids();
    let picker_set: std::collections::BTreeSet<&String> = picker.iter().collect();
    let deps_set: std::collections::BTreeSet<&String> = deps_channels.iter().collect();
    assert_eq!(
        picker_set, deps_set,
        "the picker and the delivery layer must offer/accept the same channels: \
         picker={picker:?} delivery={deps_channels:?}"
    );
    // Not vacuous: both really carry the desk AND `operator`, so the equality
    // is proving inclusion of both, not an empty match.
    assert!(picker.contains(&"engineering".to_string()), "{picker:?}");
    assert!(
        picker.contains(&OPERATOR_CHANNEL.to_string()),
        "operator is a first-class deliverable channel now: {picker:?}"
    );
    assert!(
        runtime
            .channels
            .iter()
            .any(|channel| channel.channel_id() == OPERATOR_CHANNEL),
        "the interactive operator adapter must be wired, or the equality proves nothing"
    );
}

// ---- agent pin boot check (keys rework, issue #2306, slice 3a) ----

/// The pure helper: a manifest pair, an overlay pair, an edit that clears
/// a manifest agent's pair (`Some("")`), and an `acp`-bound agent that
/// must never contribute one however its record reads.
#[test]
fn agent_pairs_apply_edits_and_skip_acp_agents() {
    let manifest = parse(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "researcher"
        role = "Researcher"
        provider = "acme"
        model = "test-model-large"

        [[agent]]
        id = "writer"
        role = "Writer"
        provider = "acme"
        model = "test-model-small"

        [[harness]]
        id = "main"
        kind = "built_in"
        default = true

        [[harness]]
        id = "laptop"
        kind = "acp"

        [harness.acp]
        transport = "local"
        agent = "claude"
        "#,
    );
    let edits = vec![AgentOverride {
        agent_id: "writer".to_string(),
        provider: Some(String::new()),
        model: Some(String::new()),
        ..Default::default()
    }];
    let overlays = vec![OverlayAgent {
        provider: Some("other-co".to_string()),
        id: "sam".to_string(),
        name: "Sam".to_string(),
        role: "Web search".to_string(),
        description: None,
        tools: None,
        model: Some("test-model-small".to_string()),
        harness: None,
    }];

    let pairs = agent_pairs(&manifest, &edits, &overlays);
    let by_id: std::collections::BTreeMap<_, _> = pairs.into_iter().collect();
    assert_eq!(
        by_id.get("researcher").map(|c| c.provider.as_str()),
        Some("acme"),
        "the manifest pair, untouched by any edit"
    );
    assert!(
        !by_id.contains_key("writer"),
        "an edit clearing both halves must drop the pair, not leave a stale one"
    );
    assert_eq!(
        by_id.get("sam").map(|c| c.provider.as_str()),
        Some("other-co"),
        "an overlay teammate's own pair"
    );

    // An agent bound to an acp harness must never contribute a pair, even
    // if its record somehow carries one (an unvalidated manifest) — only
    // `researcher` moves to `laptop` here, so its absence below is the
    // whole assertion; `writer`'s own (untouched) pair still resolves.
    let mut acp_pair = manifest.clone();
    acp_pair.agents[0].harness = Some("laptop".to_string());
    let pairs = agent_pairs(&acp_pair, &[], &[]);
    assert!(
        !pairs.iter().any(|(id, _)| id == "researcher"),
        "an acp-bound agent's pair must never resolve: {pairs:?}"
    );
}

/// A company whose only inference source is a single agent's pin still
/// boots the harness brain, not the offline echo — `resolve_effective`
/// alone would answer "unconfigured" here, since there is no company or
/// harness default at all.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_company_whose_only_inference_is_a_pin_boots_the_harness_brain() {
    use crate::harness::HarnessPool;

    let home_dir = tmp_home("oc-3a-pin-boots-");
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let manifest = parse(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "researcher"
        role = "Researcher"
        provider = "acme"
        model = "test-model-large"
        "#,
    );

    // Seeded on the same on-disk secret store `RuntimeBuilder::build`
    // opens for this `home`/id when none is passed explicitly.
    let secrets = FsSecretStore::new(home.clone());
    inference::store::put_provider(
        &id,
        &secrets,
        inference::store::ProviderDraft {
            slug: "acme".to_string(),
            label: "Acme".to_string(),
            kind: "openai_compatible".to_string(),
            base_url: "http://127.0.0.1:9/v1".to_string(),
            models: std::collections::BTreeMap::new(),
            enabled: true,
        },
    )
    .await
    .unwrap();

    let runtime = RuntimeBuilder::new(home, manifest)
        .with_id(id)
        .with_harness(Arc::new(HarnessPool::new()))
        .build()
        .await
        .unwrap();
    assert_ne!(
        runtime.cognition().path,
        "echo",
        "an agent pin is a real inference source, not \"unconfigured\""
    );
}

/// The mirror, inverted (round-3a review P1-5, X13): a pin naming a
/// provider this company never added is a *declared* inference source —
/// broken, but declared — and X13 says a company that has said anything
/// about inference never boots the echo brain merely because that
/// something does not currently work. Before this fix, a pin resolving
/// to nothing made every boot-time check answer `false` exactly like an
/// unconfigured company, and the affected agent silently got an echo
/// reply instead of the fail-closed sentence naming its broken pin. The
/// harness brain now boots regardless; `TenantProvider::resolve`'s own
/// pin check (P1-1) is what then fails that agent's turns closed, naming
/// it.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_pin_naming_a_missing_provider_boots_the_harness_brain_not_echo() {
    use crate::harness::HarnessPool;

    let home_dir = tmp_home("oc-3a-pin-missing-");
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let manifest = parse(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "researcher"
        role = "Researcher"
        provider = "acme"
        model = "test-model-large"
        "#,
    );

    // No provider seeded this time.
    let runtime = RuntimeBuilder::new(home, manifest)
        .with_id(id)
        .with_harness(Arc::new(HarnessPool::new()))
        .build()
        .await
        .unwrap();
    assert_ne!(
        runtime.cognition().path,
        "echo",
        "a declared (even if broken) pair is still a declared inference \
         source, not \"unconfigured\" — X13"
    );
}

/// The exact reported sequence (round-3a review P1-5, X13): a company
/// with one provider, set as the full company default, has it switched
/// off (X14 keeps the default marker unchanged). The **next boot** —
/// hosted tenants restart on wake, so this is the normal path — must not
/// land on the echo brain just because that default cannot currently
/// resolve.
#[cfg(feature = "openhuman")]
#[tokio::test]
async fn a_disabled_full_default_boots_the_harness_brain_not_echo() {
    use crate::harness::HarnessPool;

    let home_dir = tmp_home("oc-3a-default-off-");
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("acme");
    let manifest = parse(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "researcher"
        role = "Researcher"
        "#,
    );

    let secrets = FsSecretStore::new(home.clone());
    inference::store::put_provider(
        &id,
        &secrets,
        inference::store::ProviderDraft {
            slug: "acme".to_string(),
            label: "Acme".to_string(),
            kind: "openai_compatible".to_string(),
            base_url: "http://127.0.0.1:9/v1".to_string(),
            models: std::collections::BTreeMap::new(),
            // Confirmed off, as X14's guard requires before a disable —
            // the marker below stays untouched by that confirm, exactly
            // as it would on the real route.
            enabled: false,
        },
    )
    .await
    .unwrap();
    inference::store::set_default_choice(
        &id,
        &secrets,
        &inference::store::ModelChoice {
            provider: "acme".to_string(),
            model: "test-model-large".to_string(),
        },
    )
    .await
    .unwrap();

    let runtime = RuntimeBuilder::new(home, manifest)
        .with_id(id)
        .with_harness(Arc::new(HarnessPool::new()))
        .build()
        .await
        .unwrap();
    assert_ne!(
        runtime.cognition().path,
        "echo",
        "a stored default naming a switched-off provider is a declared \
         inference source, not \"unconfigured\" — X13"
    );
}

/// A desk added to `company.toml` since the last boot is wired on this one.
///
/// The persisted record carries the manifest of a PREVIOUS boot, so reusing
/// it for desk resolution silently wires yesterday's `[[group_chat]]` list:
/// a desk the operator has just added would never become a delivery
/// destination, and the only symptom would be a workflow refusing a
/// destination the manifest plainly declares. The overlay halves still come
/// from the persisted record, which the `research` assertion pins.
#[tokio::test]
async fn a_desk_added_to_the_manifest_since_the_last_boot_is_wired() {
    use crate::ports::CompanyStore;
    use crate::store::FsCompanyStore;

    let dir = tempfile::tempdir().unwrap();
    let common = r#"
        [company]
        name = "Acme"
        [[agent]]
        id = "ceo"
        role = "Chief"
        [[group_chat]]
        id = "engineering"
        name = "Engineering"
        members = ["ceo"]
    "#;
    let persisted = parse(common);
    let booting = parse(&format!(
        "{common}\n[[group_chat]]\nid = \"growth\"\nname = \"Growth\"\nmembers = [\"ceo\"]\n"
    ));
    let id = CompanyId::new("acme");
    FsCompanyStore::new(dir.path())
        .save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_desk_hive: Vec::new(),
            id: id.clone(),
            manifest: persisted,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: vec![crate::ports::types::OverlayDesk {
                id: "research".to_string(),
                name: "Research".to_string(),
                description: None,
                members: vec!["ceo".to_string()],
                responder: crate::ports::types::ResponderMode::default(),
                hive: Default::default(),
            }],
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

    let runtime = RuntimeBuilder::new(dir.path(), booting)
        .with_id(id)
        .build()
        .await
        .unwrap();

    let ids: Vec<_> = runtime
        .channels
        .iter()
        .map(|channel| channel.channel_id())
        .collect();
    assert!(ids.contains(&"growth"), "{ids:?}");
    assert!(ids.contains(&"engineering"), "{ids:?}");
    assert!(ids.contains(&"research"), "{ids:?}");
}

#[cfg(feature = "tinyplace")]
#[tokio::test]
async fn discoverable_company_registers_and_publishes_without_blocking() {
    use crate::economy::signer::LocalSigner;
    use crate::economy::{MockTinyplaceClient, TinyplaceEconomy};
    use crate::ports::AgentEconomy;
    use crate::ports::CompanyStore;
    use crate::store::FsCompanyStore;

    let dir = tempfile::tempdir().unwrap();
    let manifest = parse(
        r#"
        [company]
        name = "Acme"
        handle = "acme"
        [place]
        discoverable = true
        skills = [{ id = "seo.audit", price_usd = "25.00" }]
        "#,
    );
    let id = CompanyId::new("acme");
    let store: Arc<dyn CompanyStore> = Arc::new(FsCompanyStore::new(dir.path().to_path_buf()));
    let signer = Arc::new(LocalSigner::generate());
    let mock = Arc::new(MockTinyplaceClient::new());
    let economy: Arc<dyn AgentEconomy> = Arc::new(
        TinyplaceEconomy::new(mock.clone(), signer, store, id.clone(), None).going_public(true),
    );

    let runtime = RuntimeBuilder::new(dir.path().to_path_buf(), manifest)
        .with_id(id)
        .with_economy(economy)
        .with_discoverable(true)
        .build()
        .await
        .unwrap();

    // The economy is wired, and boot registered + published the card.
    assert!(runtime.has_economy());
    assert_eq!(mock.count("register_name"), 1, "boot claimed the handle");
    assert_eq!(mock.count("put_agent"), 1, "boot published the card");
}

/// Issue #454, at the construction path that actually runs in production.
///
/// The economy above is *injected*, so it proves nothing about how a real
/// company's economy is assembled. This one goes through
/// [`maybe_build_economy`] — the only production builder of a
/// [`TinyplaceEconomy`] — and asserts the property that only holds when the
/// outbox replayer was attached before the type erasure: an offline
/// `publish_card` returns `Ok`, because there is now something that will send
/// the card it queued.
///
/// **This test is the guard on that one line.** Delete
/// `spawn_outbox_replayer(&economy, …)` from `maybe_build_economy` and the
/// publish below returns `tinyplace_unreachable` instead — verified by doing
/// exactly that.
#[cfg(feature = "tinyplace")]
#[tokio::test]
async fn discoverable_path_builds_an_economy_that_can_degrade_offline() {
    use crate::economy::build_agent_card;
    use crate::ports::CompanyStore;
    use crate::ports::types::CompanyIdentity;
    use crate::store::FsCompanyStore;

    let dir = tempfile::tempdir().unwrap();
    let manifest = parse(
        r#"
        [company]
        name = "Acme"
        handle = "acme"
        [place]
        discoverable = true
        "#,
    );
    let id = CompanyId::new("acme");
    let store: Arc<dyn CompanyStore> = Arc::new(FsCompanyStore::new(dir.path().to_path_buf()));

    // A port nothing listens on: every call is refused, which is exactly the
    // `unreachable` condition the outbox exists for. Bound and released so
    // the OS confirms it is free, rather than guessing a number.
    let dead = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };

    let economy = maybe_build_economy(
        &manifest,
        dir.path(),
        &id,
        store,
        Some(format!("http://{dead}")),
        true,
    )
    .await
    .expect("a discoverable company with a handle gets an economy");

    let identity = CompanyIdentity {
        company: id.clone(),
        handle: "acme".to_string(),
    };
    let card = build_agent_card(&manifest, "http://127.0.0.1:8080");
    economy
        .publish_card(&identity, &card)
        .await
        .expect("the built economy degrades offline, which it may only do with a replayer");
}

/// **Issue #276's durability claim, at the path that actually threatens it.**
///
/// `merge_enabled_workflows` re-derives `[workflows].enabled` from seed ∪
/// overlay ids on every build — it re-arms that list by design. The pause
/// switch is a separate field precisely so a rebuild cannot undo it, and
/// this is the test that says so: disable a workflow, build again over the
/// same store, and it must still be disabled.
///
/// The store round-trip tests cover save→load; this covers build→save, which
/// is a different write and the one that would silently re-arm every paused
/// schedule on restart.
#[tokio::test]
async fn a_rebuild_keeps_a_paused_workflow_paused() {
    use crate::ports::types::OverlayWorkflow;
    use crate::store::FsCompanyStore;

    let home_dir = tmp_home("oc-paused-rebuild-");
    let home = home_dir.path().to_path_buf();
    let id = CompanyId::new("pause-co");
    let manifest = parse(
        r#"
        [company]
        name = "Pause Co"

        [[agent]]
        id = "assistant"
        role = "Assistant"
        "#,
    );

    // First build materializes the record.
    RuntimeBuilder::new(home.clone(), manifest.clone())
        .with_id(id.clone())
        .build()
        .await
        .unwrap();

    // An overlay workflow, switched off — the state an operator would have
    // left behind by clicking Pause.
    let store = FsCompanyStore::new(home.clone());
    let mut record = store.load(&id).await.unwrap().unwrap();
    record.overlay_workflows.push(OverlayWorkflow {
        id: "digest".to_string(),
        toml: "id = \"digest\"\nname = \"Digest\"\n[[node]]\nid = \"start\"\nkind = \"trigger\"\nname = \"Start\"\n"
            .to_string(),
    });
    record.set_workflow_enabled("digest", false);
    store.save(&record).await.unwrap();

    // Rebuild, exactly as a restart does.
    RuntimeBuilder::new(home.clone(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();

    let rebuilt = store.load(&id).await.unwrap().unwrap();
    assert!(
        !rebuilt.workflow_enabled("digest"),
        "the rebuild re-armed a paused workflow — `disabled_workflows` was not carried forward"
    );
    // And the merge it has to survive did run: the id is back in the
    // manifest's declaration list, which is exactly why that list could not
    // have been the switch.
    assert!(
        rebuilt
            .manifest
            .workflows
            .enabled
            .contains(&"digest".to_string()),
        "merge_enabled_workflows did not run, so this test proves nothing"
    );
}
