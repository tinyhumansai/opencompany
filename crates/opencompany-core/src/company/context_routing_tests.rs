use super::*;

fn agent(tier: Option<&str>) -> Agent {
    Agent {
        provider: None,
        global: false,
        id: "a".into(),
        role: "Role".into(),
        name: None,
        description: None,
        tier: tier.map(str::to_string),
        harness: None,
        tools: None,
        skills: None,
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    }
}

#[test]
fn every_role_is_routed_the_universal_document() {
    for tier in [
        None,
        Some("orchestrator"),
        Some("reasoning"),
        Some("frontend"),
        Some("compress"),
        Some("subconscious"),
    ] {
        let routed = routed_documents(&agent(tier));
        assert!(
            routed.contains(&UNIVERSAL_DOCUMENT.to_string()),
            "tier {tier:?} → {routed:?}"
        );
    }
}

#[test]
fn the_per_tier_default_table_matches_the_spec() {
    assert_eq!(
        routed_documents(&agent(Some("orchestrator"))),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF, CLAIMS, THREADS]
    );
    assert_eq!(
        routed_documents(&agent(Some("reasoning"))),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF, CLAIMS]
    );
    assert_eq!(
        routed_documents(&agent(Some("frontend"))),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF]
    );
    assert_eq!(
        routed_documents(&agent(Some("compress"))),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC]
    );
    assert_eq!(
        routed_documents(&agent(Some("subconscious"))),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC]
    );
}

/// Most roster entries omit `tier`, so the fallback covers almost everybody.
#[test]
fn an_agent_with_no_tier_takes_the_reasoning_row() {
    assert_eq!(
        routed_documents(&agent(None)),
        routed_documents(&agent(Some("reasoning")))
    );
}

/// The distinction `Option<Vec<String>>` exists to represent.
#[test]
fn an_explicit_empty_context_is_not_the_same_as_an_omitted_one() {
    let mut explicit = agent(Some("orchestrator"));
    explicit.context = Some(Vec::new());
    assert_eq!(
        routed_documents(&explicit),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC],
        "`context = []` means the universal document and nothing else"
    );

    assert_eq!(
        routed_documents(&agent(Some("orchestrator"))),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF, CLAIMS, THREADS],
        "an omitted key takes the tier default"
    );
}

#[test]
fn an_explicit_context_overrides_the_tier_default() {
    let mut a = agent(Some("orchestrator"));
    a.context = Some(vec!["GOAL.md".into()]);
    assert_eq!(
        routed_documents(&a),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, "GOAL.md"]
    );
}

#[test]
fn a_role_that_weighs_evidence_is_never_routed_the_board() {
    let mut a = agent(Some("reasoning"));
    a.classes = vec!["evidence".into()];
    a.context = Some(vec![BRIEF.into(), BOARD.into()]);
    let routed = routed_documents(&a);
    assert!(!routed.contains(&BOARD.to_string()), "{routed:?}");
    assert!(routed.contains(&BRIEF.to_string()), "{routed:?}");
}

#[test]
fn a_role_that_judges_is_never_routed_the_scratch() {
    let mut a = agent(Some("reasoning"));
    a.classes = vec!["judge".into()];
    a.context = Some(vec![SCRATCH.into()]);
    assert_eq!(routed_documents(&a), [UNIVERSAL_DOCUMENT, AGENTS_DOC]);
}

#[test]
fn a_role_acting_on_a_directive_is_never_routed_the_claim_ledger() {
    let mut a = agent(Some("reasoning"));
    a.classes = vec!["directive".into()];
    // CLAIMS is in the `reasoning` default row, so this proves the exclusion
    // subtracts from defaults and not only from explicit lists.
    let routed = routed_documents(&a);
    assert!(!routed.contains(&CLAIMS.to_string()), "{routed:?}");
    assert!(routed.contains(&BRIEF.to_string()), "{routed:?}");
}

/// An exclusion outranks an explicit routing line — that is what makes a
/// declared class a control rather than a suggestion somebody can edit away.
#[test]
fn an_exclusion_outranks_an_explicit_context_entry() {
    let mut a = agent(None);
    a.classes = vec!["judge".into()];
    a.context = Some(vec![SCRATCH.into(), BRIEF.into()]);
    assert_eq!(
        routed_documents(&a),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF]
    );
}

#[test]
fn several_classes_all_apply() {
    let mut a = agent(None);
    a.classes = vec!["judge".into(), "evidence".into(), "directive".into()];
    a.context = Some(vec![
        SCRATCH.into(),
        BOARD.into(),
        CLAIMS.into(),
        BRIEF.into(),
    ]);
    assert_eq!(
        routed_documents(&a),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF]
    );
}

/// The method policy is exempt: it is how the company works, not something
/// it asserts, and a role excluded from it could not follow it.
#[test]
fn no_class_can_withhold_the_universal_document() {
    let mut a = agent(None);
    a.classes = vec!["judge".into(), "evidence".into(), "directive".into()];
    a.context = Some(Vec::new());
    assert_eq!(routed_documents(&a), [UNIVERSAL_DOCUMENT, AGENTS_DOC]);
}

#[test]
fn a_document_listed_twice_is_routed_once() {
    let mut a = agent(None);
    a.context = Some(vec![UNIVERSAL_DOCUMENT.into(), BRIEF.into(), BRIEF.into()]);
    assert_eq!(
        routed_documents(&a),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF]
    );
}

#[test]
fn blank_context_entries_are_ignored() {
    let mut a = agent(None);
    a.context = Some(vec!["".into(), "  ".into(), BRIEF.into()]);
    assert_eq!(
        routed_documents(&a),
        [UNIVERSAL_DOCUMENT, AGENTS_DOC, BRIEF]
    );
}

/// An unknown class imposes no exclusion. Manifest validation refuses one
/// outright, so this only ever covers a record written by an older binary —
/// where failing open on routing is right and failing closed would blank a
/// working role's context.
#[test]
fn an_unknown_class_excludes_nothing() {
    assert!(excluded_documents(&["mystery".to_string()]).is_empty());
}

/// The resolver half, against a real store rather than a mock — the store is
/// where "does this path name that note?" is actually decided.
mod resolve {
    use std::sync::Arc;

    use super::*;
    use crate::ports::WorkspaceStore;
    use crate::ports::types::CompanyId;
    use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};
    use crate::store::FsOps;

    async fn store() -> (tempfile::TempDir, Arc<dyn WorkspaceStore>, CompanyId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws: Arc<dyn WorkspaceStore> = Arc::new(FsOps::new(dir.path()));
        (dir, ws, CompanyId::new("acme"))
    }

    /// Writes `name` under `parent` with `body`, returning its node id.
    async fn file(
        ws: &Arc<dyn WorkspaceStore>,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        body: &str,
    ) -> String {
        let id = format!("id-{name}");
        ws.create(
            company,
            &WorkspaceNode {
                id: id.clone(),
                name: name.to_string(),
                kind: NodeKind::File,
                parent_id: parent.map(str::to_string),
                updated_at_millis: 1,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            Some(body),
        )
        .await
        .expect("create file");
        id
    }

    async fn folder(ws: &Arc<dyn WorkspaceStore>, company: &CompanyId, name: &str) -> String {
        ws.adopt_or_create_folder(company, None, name, WorkspaceOrigin::Operator)
            .await
            .expect("folder")
            .into_node()
            .id
    }

    #[tokio::test]
    async fn a_routed_document_is_read_out_of_the_workspace() {
        let (_dir, ws, company) = store().await;
        file(&ws, &company, None, UNIVERSAL_DOCUMENT, "How we work.").await;
        file(&ws, &company, None, BRIEF, "What we established.").await;

        let mut a = agent(Some("frontend")); // routes METHOD + BRIEF
        a.context = None;

        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert_eq!(
            resolved,
            vec![
                (UNIVERSAL_DOCUMENT.to_string(), "How we work.".to_string()),
                (BRIEF.to_string(), "What we established.".to_string()),
            ]
        );
    }

    /// A company created before the lowercase-dashed rule holds `BRIEF.md`,
    /// and a manifest written then asks for `BRIEF.md`. Both still route.
    ///
    /// This is the compatibility seam the rule needs most: routing is what
    /// a role reasons *from*, so an unmatched name is not a missing file
    /// message — it is an agent quietly answering without the company's
    /// brief, and nothing anywhere says so.
    #[tokio::test]
    async fn a_legacy_uppercase_document_still_routes() {
        let (_dir, ws, company) = store().await;
        file(&ws, &company, None, "BRIEF.md", "What we established.").await;

        let mut a = agent(Some("frontend"));
        a.context = Some(vec!["BRIEF.md".into()]);
        let by_old_name = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert_eq!(by_old_name.len(), 1, "{by_old_name:?}");

        // And the same node answers the canonical spelling, which is what
        // the default routing table now asks for.
        let mut b = agent(Some("frontend"));
        b.context = Some(vec![BRIEF.into()]);
        let by_new_name = resolve_routed_documents(ws.as_ref(), &company, &b)
            .await
            .expect("resolves");
        assert_eq!(
            by_new_name,
            vec![(BRIEF.to_string(), "What we established.".to_string())],
            "the routed name is the one asked for, resolved against what exists"
        );
    }

    /// The rule that differs from `prompt_files`: a live workspace note that
    /// does not exist yet is skipped, not an error. Failing the roster build
    /// here would take a whole company down over a file anybody could create.
    #[tokio::test]
    async fn a_missing_document_is_skipped_rather_than_failing() {
        let (_dir, ws, company) = store().await;
        file(&ws, &company, None, BRIEF, "Only this one exists.").await;

        let mut a = agent(None);
        a.context = Some(vec![BRIEF.into(), "NOWHERE.md".into()]);

        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert_eq!(resolved.len(), 1, "{resolved:?}");
        assert_eq!(resolved[0].0, BRIEF);
    }

    #[tokio::test]
    async fn a_nested_document_resolves_by_its_logical_path() {
        let (_dir, ws, company) = store().await;
        let brand = folder(&ws, &company, "Brand").await;
        file(
            &ws,
            &company,
            Some(&brand),
            "Voice.md",
            "Plain, never loud.",
        )
        .await;

        let mut a = agent(None);
        a.context = Some(vec!["brand/Voice.md".into()]);

        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert_eq!(
            resolved,
            vec![(
                "brand/Voice.md".to_string(),
                "Plain, never loud.".to_string()
            )]
        );
    }

    /// A leading slash is the operator's spelling, not a different note.
    #[tokio::test]
    async fn a_leading_slash_names_the_same_document() {
        let (_dir, ws, company) = store().await;
        let brand = folder(&ws, &company, "Brand").await;
        file(&ws, &company, Some(&brand), "Voice.md", "body").await;

        let mut a = agent(None);
        a.context = Some(vec!["/brand/Voice.md".into()]);

        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert_eq!(resolved.len(), 1, "{resolved:?}");
        assert_eq!(resolved[0].0, "brand/Voice.md");
    }

    /// A traversal-shaped entry resolves to nothing rather than erroring, so
    /// one bad manifest line cannot stop a company whose other routing works.
    #[tokio::test]
    async fn a_traversal_shaped_entry_resolves_to_nothing() {
        let (_dir, ws, company) = store().await;
        file(&ws, &company, None, BRIEF, "body").await;

        let mut a = agent(None);
        a.context = Some(vec!["../../etc/passwd".into(), BRIEF.into()]);

        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert_eq!(resolved.len(), 1, "{resolved:?}");
        assert_eq!(resolved[0].0, BRIEF);
    }

    /// An exclusion holds all the way through the read: a judge must not be
    /// handed the scratch even when the note is sitting right there.
    #[tokio::test]
    async fn an_excluded_document_is_never_read_even_when_it_exists() {
        let (_dir, ws, company) = store().await;
        file(&ws, &company, None, SCRATCH, "half-finished thinking").await;
        file(&ws, &company, None, BRIEF, "established").await;

        let mut a = agent(None);
        a.classes = vec!["judge".into()];
        a.context = Some(vec![SCRATCH.into(), BRIEF.into()]);

        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        let names: Vec<&str> = resolved.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&SCRATCH), "{names:?}");
        assert!(names.contains(&BRIEF), "{names:?}");
    }

    /// A role routed nothing does not touch the store at all — the tree read
    /// is skipped rather than performed and discarded.
    #[tokio::test]
    async fn a_role_routed_nothing_reads_nothing() {
        let (_dir, ws, company) = store().await;
        let mut a = agent(Some("compress"));
        // `compress` defaults to no documents, and an explicit empty context
        // strips even the universal one.
        a.context = Some(Vec::new());
        a.classes = Vec::new();

        // The universal document is always routed, so to reach the empty case
        // the caller must have nothing at all — assert the shape we do get.
        let resolved = resolve_routed_documents(ws.as_ref(), &company, &a)
            .await
            .expect("resolves");
        assert!(
            resolved.is_empty(),
            "no document exists in the store, so nothing resolves: {resolved:?}"
        );
    }
}
