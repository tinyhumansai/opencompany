use super::*;
use crate::ports::SecretStore;
use crate::ports::types::{Actor, ActorKind, CompanyEvent};
use crate::runtime::RuntimeBuilder;
use crate::store::paths::Bundle;
use crate::store::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore, FsSecretStore};

pub(super) fn tmp_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "opencompany-export-{tag}-{}-{}",
        std::process::id(),
        crate::ports::now_millis()
    ))
}

pub(super) fn manifest() -> CompanyManifest {
    let toml_src = r#"
            [company]
            name = "Export Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
        "#;
    toml::from_str(toml_src).expect("parse manifest")
}

/// A minimal running company record for tests that only need one to exist.
pub(super) fn company_record(id: &CompanyId) -> CompanyRecord {
    CompanyRecord {
        general_channel: Default::default(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".into(),
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
}

pub(super) fn fs_ports(root: &Path) -> Ports {
    (
        Arc::new(FsCompanyStore::new(root.to_path_buf())),
        Arc::new(FsEventLog::new(root.to_path_buf())),
        Arc::new(FsMemoryStore::new(root.to_path_buf())),
        Arc::new(FsContextStore::new(root.to_path_buf())),
    )
}

struct ArchiveScopes {
    archived: Vec<CompressedTrace>,
    restored: Arc<std::sync::Mutex<Vec<CompressedTrace>>>,
    context: Arc<FsContextStore>,
}

#[async_trait::async_trait]
impl MemoryScopes for ArchiveScopes {
    fn agent_context(&self, _agent_id: &str) -> Arc<dyn ContextStore> {
        self.context.clone()
    }

    fn desk_context(&self, _desk_id: &str) -> Arc<dyn ContextStore> {
        self.context.clone()
    }

    async fn archived_traces(&self, _company: &CompanyId) -> Result<Vec<CompressedTrace>> {
        Ok(self.archived.clone())
    }

    async fn restore_archived_traces(
        &self,
        _company: &CompanyId,
        traces: &[CompressedTrace],
    ) -> Result<()> {
        self.restored.lock().unwrap().extend_from_slice(traces);
        Ok(())
    }
}

#[tokio::test]
async fn archive_traces_survive_bundle_roundtrip_in_the_archive_tier() {
    let home1 = tmp_root("archive-src");
    let home2 = tmp_root("archive-dst");
    let dest = tmp_root("archive-bundle");
    let id = CompanyId::new("archive-co");
    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&company_record(&id)).await.unwrap();
    let archived = vec![CompressedTrace {
        cycle_id: "evicted-cycle".into(),
        summary: "retained recovery trace".into(),
        at_millis: 7,
    }];
    let source_scopes = Arc::new(ArchiveScopes {
        archived: archived.clone(),
        restored: Arc::new(std::sync::Mutex::new(Vec::new())),
        context: Arc::new(FsContextStore::new(home1.clone())),
    });
    export_bundle_with_scopes(
        &id,
        &dest,
        s1,
        e1,
        m1,
        c1,
        None,
        Some(source_scopes),
        ExportOpts::default(),
    )
    .await
    .unwrap();
    assert!(dest.join(MEMORY_DIR).join(ARCHIVES_JSONL).is_file());

    let (s2, e2, m2, c2) = fs_ports(&home2);
    let restored = Arc::new(std::sync::Mutex::new(Vec::new()));
    let target_scopes = Arc::new(ArchiveScopes {
        archived: Vec::new(),
        restored: restored.clone(),
        context: Arc::new(FsContextStore::new(home2)),
    });
    import_bundle_with_scopes(&dest, s2, e2, m2, c2, None, Some(target_scopes))
        .await
        .unwrap();
    assert_eq!(*restored.lock().unwrap(), archived);
}

/// The mandatory end-to-end round-trip: build a company, run a cycle to
/// populate events/traces/ledger, seed a ledger entry and context chunk,
/// export to a bundle directory, import into a *fresh* home through the fs
/// ports, and assert the charter + event log + ledger survive intact.
#[tokio::test]
async fn export_import_roundtrip_fs() {
    let home1 = tmp_root("src");
    let home2 = tmp_root("dst");
    let dest = tmp_root("bundle");

    // Build + populate the source company.
    let runtime = RuntimeBuilder::fs_defaults(home1.clone(), manifest())
        .await
        .expect("build");
    let id = runtime.id().clone();
    runtime
        .run_cycle(vec![CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "kick off".into(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        }])
        .await
        .expect("cycle");

    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.append_ledger(
        &id,
        LedgerEntry {
            at_millis: 42,
            kind: "inference.spend".into(),
            amount_usd: 1.25,
            memo: "seed".into(),
        },
    )
    .await
    .unwrap();
    c1.put(
        &id,
        ContextChunk {
            label: "notes/intro".into(),
            body: "the quick brown fox".into(),
        },
    )
    .await
    .unwrap();

    // Snapshot the source state through the ports for later comparison.
    let src_record = s1.load(&id).await.unwrap().unwrap();
    let src_events = e1
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert!(!src_events.is_empty(), "cycle should log the input event");

    // Export → import into a fresh home.
    export_bundle(
        &id,
        &dest,
        s1.clone(),
        e1.clone(),
        m1.clone(),
        c1.clone(),
        None,
        ExportOpts::default(),
    )
    .await
    .expect("export");

    let (s2, e2, m2, c2) = fs_ports(&home2);
    let imported_id = import_bundle(&dest, s2.clone(), e2.clone(), m2.clone(), c2.clone(), None)
        .await
        .expect("import");
    assert_eq!(imported_id, id, "id preserved through the bundle");

    // Charter + lifecycle identical.
    let dst_record = s2.load(&id).await.unwrap().expect("imported record");
    assert_eq!(
        dst_record.manifest.company.name,
        src_record.manifest.company.name
    );
    assert_eq!(dst_record.manifest.company.name, "Export Co");
    assert_eq!(dst_record.lifecycle, src_record.lifecycle);

    // Ledger byte-identical (entries carry their original timestamps).
    assert_eq!(dst_record.ledger, src_record.ledger);
    assert!(dst_record.ledger.iter().any(|e| e.memo == "seed"));

    // Event log identical: same seqs and payloads (timestamps are re-stamped
    // on append, so compare seq + event only).
    let dst_events = e2
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert_eq!(dst_events.len(), src_events.len());
    for (a, b) in src_events.iter().zip(dst_events.iter()) {
        assert_eq!(a.seq, b.seq);
        assert_eq!(a.event, b.event);
    }

    // Traces + context round-trip through the ports.
    let src_traces = m1.recent_traces(&id, usize::MAX).await.unwrap();
    let dst_traces = m2.recent_traces(&id, usize::MAX).await.unwrap();
    assert_eq!(src_traces, dst_traces);
    let chunk = c2.list(&id, "notes/").await.unwrap();
    assert_eq!(chunk.len(), 1);
    assert_eq!(
        c2.peek(&id, &chunk[0].addr, None).await.unwrap(),
        "the quick brown fox"
    );

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// PR #1875 review finding: importing a legacy bundle — one whose source
/// company predates activation tracking, so `store.activation_gate_seen`
/// answers `false` — must land in the target store still answering
/// `false`. Before the fix, `write_via_ports` called plain `store.save`,
/// which unconditionally stamps the marker `true`; every OTHER save
/// really is made by activation-aware code, but import is replaying
/// history, not writing it, so that stamp falsely marked a restored
/// legacy company as already seen. That permanently blocks
/// `RuntimeBuilder::build`'s pre-#1843 grandfather back-fill on the
/// imported company's very next boot, showing an established operator
/// the fresh-company onboarding gate.
#[tokio::test]
async fn import_preserves_unseen_activation_gate() {
    let home1 = tmp_root("gate-src");
    let home2 = tmp_root("gate-dst");
    let dest = tmp_root("gate-bundle");

    // A legacy company: `company.toml` on disk, no `meta.json` at all —
    // the exact shape a pre-#1843 bundle has. `FsCompanyStore::load`
    // reads a missing meta.json as `Meta::default()`
    // (`lifecycle: "running"`, no overlays), and its
    // `activation_gate_seen` reads the same absence as `false` — both
    // "never saved by activation-aware code".
    let (s1, e1, m1, c1) = fs_ports(&home1);
    let id = CompanyId::new("legacy-co");
    let bundle = Bundle::new(home1.clone(), &id);
    bundle.ensure_dirs().await.unwrap();
    tokio::fs::write(bundle.company_toml(), toml::to_string(&manifest()).unwrap())
        .await
        .unwrap();

    assert!(
        !s1.activation_gate_seen(&id).await.unwrap(),
        "fixture must start as a legacy, gate-unseen record"
    );

    export_bundle(
        &id,
        &dest,
        s1.clone(),
        e1.clone(),
        m1.clone(),
        c1.clone(),
        None,
        ExportOpts::default(),
    )
    .await
    .expect("export");

    let (s2, e2, m2, c2) = fs_ports(&home2);
    let imported_id = import_bundle(&dest, s2.clone(), e2.clone(), m2.clone(), c2.clone(), None)
        .await
        .expect("import");
    assert_eq!(imported_id, id);

    assert!(
        !s2.activation_gate_seen(&id).await.unwrap(),
        "importing a legacy bundle must not stamp the activation gate as \
             seen — doing so hides an established operator's grandfather \
             back-fill behind the fresh-company onboarding gate"
    );

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// Secrets and keys are excluded from an export by default and only appear
/// when `include_secrets` is set with a source bundle.
#[tokio::test]
async fn secrets_excluded_by_default() {
    let home = tmp_root("sec-home");
    let runtime = RuntimeBuilder::fs_defaults(home.clone(), manifest())
        .await
        .expect("build");
    let id = runtime.id().clone();

    // Seed a secret and a key file in the source fs bundle.
    let secrets = FsSecretStore::new(home.clone());
    secrets
        .set(
            &id,
            "github_token",
            crate::ports::SecretValue("ghp_x".into()),
        )
        .await
        .unwrap();
    let bundle = Bundle::new(home.clone(), &id);
    // A bundle written while tiny.place existed holds its retired identity
    // seed here; key material stays out of an export whatever it is for.
    tokio::fs::write(bundle.keys_dir().join("agent.ed25519"), b"seed-bytes")
        .await
        .unwrap();

    let (s, e, m, c) = fs_ports(&home);

    // Default: no secrets/ or keys/ in the export.
    let plain = tmp_root("sec-plain");
    export_bundle(
        &id,
        &plain,
        s.clone(),
        e.clone(),
        m.clone(),
        c.clone(),
        None,
        ExportOpts::default(),
    )
    .await
    .unwrap();
    assert!(
        !plain.join(SECRETS_DIR).exists(),
        "secrets leaked by default"
    );
    assert!(!plain.join(KEYS_DIR).exists(), "keys leaked by default");

    // With include_secrets + a source bundle: both are copied.
    let withsec = tmp_root("sec-with");
    export_bundle(
        &id,
        &withsec,
        s,
        e,
        m,
        c,
        None,
        ExportOpts {
            include_secrets: true,
            fs_bundle: Some(bundle.dir().to_path_buf()),
        },
    )
    .await
    .unwrap();
    assert!(withsec.join(SECRETS_DIR).exists(), "secrets not included");
    assert!(
        withsec.join(KEYS_DIR).join("agent.ed25519").exists(),
        "key not included"
    );

    for dir in [home, plain, withsec] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// A `LifecycleChanged` event survives an export/import round-trip, proving
/// the closed event enum tunnels through the bundle intact.
#[tokio::test]
async fn lifecycle_event_survives_roundtrip() {
    let home1 = tmp_root("lc-src");
    let home2 = tmp_root("lc-dst");
    let dest = tmp_root("lc-bundle");
    let id = CompanyId::new("lc-co");

    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&CompanyRecord {
        general_channel: Default::default(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "paused".into(),
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
    e1.append(
        &id,
        CompanyEvent::LifecycleChanged {
            from: "running".into(),
            to: "paused".into(),
            by: Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            },
        },
    )
    .await
    .unwrap();

    export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
        .await
        .unwrap();
    let (s2, e2, m2, c2) = fs_ports(&home2);
    import_bundle(&dest, s2.clone(), e2.clone(), m2, c2, None)
        .await
        .unwrap();

    let rec = s2.load(&id).await.unwrap().unwrap();
    assert_eq!(rec.lifecycle, "paused");
    let events = e2
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();
    assert!(matches!(
        events[0].event,
        CompanyEvent::LifecycleChanged { .. }
    ));

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// Issue #358, the half that actually closes it: a withdrawn discussion
/// message is not in the bundle, and an import cannot bring it back.
///
/// Asserted three ways, because each is a different way to leak it:
///
/// 1. the **bundle file** (`events.jsonl`) does not contain the secret —
///    this is the copy that leaves the instance, so grepping the bytes is
///    the assertion that matters most;
/// 2. the **imported journal** carries the placeholder, not the text;
/// 3. the **tombstone travels**, so the imported thread still reports that
///    a message was withdrawn rather than showing a bare placeholder that
///    reads like something a person typed.
///
/// A post with no tombstone is untouched in the same bundle, so this is a
/// substitution rather than a filter that eats discussion history.
#[tokio::test]
async fn a_withdrawn_discussion_message_does_not_survive_export_import() {
    let home1 = tmp_root("redact-src");
    let home2 = tmp_root("redact-dst");
    let dest = tmp_root("redact-bundle");
    let id = CompanyId::new("redact-co");
    const SECRET: &str = "sk-live-DO-NOT-SHIP-THIS";

    let (s1, e1, m1, c1) = fs_ports(&home1);
    s1.save(&CompanyRecord {
        general_channel: Default::default(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_desk_hive: Vec::new(),
        id: id.clone(),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".into(),
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

    let leaked = e1
        .append(
            &id,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t1".into(),
                text: format!("blocked on the API key: {SECRET}"),
                by: None,
            },
        )
        .await
        .unwrap();
    // A second post nobody withdrew, to prove the scrub is targeted.
    e1.append(
        &id,
        CompanyEvent::TaskDiscussionPosted {
            task_id: "t1".into(),
            text: "rotated it, we are unblocked".into(),
            by: None,
        },
    )
    .await
    .unwrap();
    e1.append(
        &id,
        CompanyEvent::TaskDiscussionRedacted {
            task_id: "t1".into(),
            seq: leaked.value(),
            by: Some(Actor {
                kind: ActorKind::Operator,
                id: "owner".into(),
            }),
        },
    )
    .await
    .unwrap();

    export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
        .await
        .unwrap();

    // 1. The bytes that leave the building.
    let shipped = tokio::fs::read_to_string(dest.join(EVENTS_JSONL))
        .await
        .unwrap();
    assert!(
        !shipped.contains(SECRET),
        "the withdrawn message shipped in the bundle: {shipped}"
    );
    assert!(
        shipped.contains(crate::ports::tasks::REDACTED_DISCUSSION_TEXT),
        "the withdrawn post is missing its placeholder: {shipped}"
    );
    assert!(
        shipped.contains("rotated it, we are unblocked"),
        "the scrub ate a post nobody withdrew: {shipped}"
    );

    // 2 and 3. What the importing instance ends up holding.
    let (s2, e2, m2, c2) = fs_ports(&home2);
    import_bundle(&dest, s2, e2.clone(), m2, c2, None)
        .await
        .unwrap();
    let events = e2
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .unwrap();

    let posted: Vec<&str> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::TaskDiscussionPosted { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        posted,
        vec![
            crate::ports::tasks::REDACTED_DISCUSSION_TEXT,
            "rotated it, we are unblocked"
        ],
        "the imported journal must carry the placeholder, not the secret"
    );
    assert!(
        events.iter().any(|stored| matches!(
            &stored.event,
            CompanyEvent::TaskDiscussionRedacted { task_id, seq, .. }
                if task_id == "t1" && *seq == leaked.value()
        )),
        "the tombstone did not survive the round trip, so the imported thread \
             cannot say the message was withdrawn"
    );

    for dir in [home1, home2, dest] {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}

/// A manifest naming two capped teammates, so a round-trip that dropped the
/// overrides would fall back to real caps rather than to "uncapped" — the
/// regression would still show as the *wrong* numbers, not as absent ones.
pub(super) fn budget_manifest() -> CompanyManifest {
    toml::from_str(
        r#"
            [company]
            name = "Budget Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"
            budget_usd_daily = 5.0

            [[agent]]
            id = "cto"
            role = "Tech"
            budget_usd_daily = 9.0

            # Carries no budget override, and exists so the retirement fixture
            # below can remove a teammate without leaving a cap behind for one
            # that is no longer on the roster — a pairing the product never
            # produces, since `remove_member` drops the override with the
            # teammate.
            [[agent]]
            id = "ops"
            role = "Operations"
        "#,
    )
    .expect("parse manifest")
}

pub(super) fn admin_actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "user-admin".into(),
    }
}
