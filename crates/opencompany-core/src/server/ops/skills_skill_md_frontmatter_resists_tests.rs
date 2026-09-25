use std::path::Path as FsPath;

use super::*;

fn write_bundle(root: &FsPath, slug: &str, contents: &str) {
    let dir = root.join("skills").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), contents).unwrap();
}

#[test]
fn skill_md_frontmatter_resists_injection() {
    // A name carrying newlines, a stray `---`, and a fake field must not
    // inject frontmatter or hijack another field: newlines collapse to
    // spaces, so it all lands as the single `name` value.
    let nasty_name = "Evil\n---\ninjected: true\nname: hijacked";
    let doc = skill_md(nasty_name, "a real description", Some("Ops"), "body");
    let parsed = parse_skill_md("evil", &doc).expect("frontmatter stays valid");
    assert_eq!(parsed.name, "Evil --- injected: true name: hijacked");
    // The description was NOT overwritten by the injected `name: hijacked`.
    assert_eq!(parsed.description, "a real description");
    // A colon inside a value is preserved (split only on the first colon).
    let colon = skill_md("Name", "ratio 3:1 outcome", None, "body");
    assert_eq!(
        parse_skill_md("c", &colon).unwrap().description,
        "ratio 3:1 outcome"
    );
}

/// The projection every `GET …/skills` row goes through, over the same
/// resolution the harness materializes.
fn list(source_dir: Option<&FsPath>, deltas: &[SkillState]) -> Vec<InstalledSkill> {
    skill_effective::resolve(source_dir, &[], deltas)
        .expect("resolves")
        .iter()
        .map(InstalledSkill::from_effective)
        .collect()
}

pub(super) fn global_slug() -> String {
    crate::globals::skills()[0].slug.clone()
}

/// The global baseline is what every agent has before a company adds
/// anything, so it is what the console must list for a company with no
/// bundles and no deltas — the shape a platform-provisioned tenant boots in.
#[test]
fn the_list_includes_the_global_baseline_with_no_bundles_and_no_deltas() {
    let out = list(None, &[]);
    for doc in crate::globals::skills() {
        let row = out
            .iter()
            .find(|s| s.id == doc.slug)
            .unwrap_or_else(|| panic!("no `{}` row", doc.slug));
        assert!(row.enabled);
        assert_eq!(row.name, doc.name);
        assert_eq!(row.description, doc.description);
        assert_eq!(
            row.source,
            SkillSource::Company,
            "a global is a baseline install, not something an operator added, \
             so it carries no uninstall affordance"
        );
    }
}

/// A disabled global keeps its row. Hiding it would remove the only control
/// that could turn it back on.
#[test]
fn a_disabled_global_is_listed_as_disabled_rather_than_hidden() {
    let slug = global_slug();
    let out = list(
        None,
        &[SkillState {
            slug: slug.clone(),
            enabled: false,
            source: SkillSource::Company,
            custom_doc: None,
            install: None,
        }],
    );

    let row = out.iter().find(|s| s.id == slug).expect("row still listed");
    assert!(!row.enabled);
    assert!(!row.name.is_empty(), "a disabled row keeps its name");
}

/// `[globals].disable` is honoured by the reader exactly as the harness
/// honours it — via the same synthesized delta.
#[test]
fn a_manifest_opt_out_is_honoured_by_the_reader() {
    let slug = global_slug();
    let deltas = skill_effective::globals_skill_disables(&[format!("skill:{slug}")]);
    let out = list(None, &deltas);

    let row = out.iter().find(|s| s.id == slug).expect("row still listed");
    assert!(!row.enabled, "the manifest opt-out reaches the console");
}

#[test]
fn the_list_unions_bundles_with_deltas() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_bundle(
        root,
        "onboard",
        "---\nname: Onboard\ndescription: Get set up\ncategory: Ops\n---\n# Onboard\n",
    );

    let deltas = vec![
        SkillState {
            slug: "onboard".to_string(),
            enabled: false,
            source: SkillSource::Company,
            custom_doc: None,
            install: None,
        },
        SkillState {
            slug: "my-skill".to_string(),
            enabled: true,
            source: SkillSource::Custom,
            custom_doc: Some(
                "---\nname: My Skill\ndescription: Does a thing\n---\n# body\n".to_string(),
            ),
            install: None,
        },
    ];

    let out = list(Some(root), &deltas);

    let onboard = out
        .iter()
        .find(|s| s.id == "onboard")
        .expect("company bundle present");
    assert_eq!(onboard.name, "Onboard");
    assert_eq!(onboard.source, SkillSource::Company);
    assert!(!onboard.enabled, "delta flips the bundle disabled");

    let custom = out
        .iter()
        .find(|s| s.id == "my-skill")
        .expect("custom delta present");
    assert_eq!(custom.source, SkillSource::Custom);
    assert_eq!(custom.name, "My Skill");
    assert!(custom.enabled);

    let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "rows are ordered by slug");
}

/// A malformed company bundle costs that company its whole catalogue in the
/// harness, so the reader reports the failure instead of a tidy subset that
/// no agent actually has.
#[test]
fn a_malformed_company_bundle_surfaces_as_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    write_bundle(tmp.path(), "broken", "no frontmatter here\n");
    assert!(skill_effective::resolve(Some(tmp.path()), &[], &[]).is_err());
}

/// The REST list and the GraphQL resolver project the same resolution, so
/// the two transports cannot report different skills for one company.
#[test]
fn the_rest_list_and_the_graphql_resolver_agree() {
    let tmp = tempfile::tempdir().unwrap();
    write_bundle(
        tmp.path(),
        "onboard",
        "---\nname: Onboard\ndescription: Get set up\ncategory: Ops\nversion: 2.0.0\n---\n# Onboard\n",
    );
    let deltas = vec![
        SkillState {
            slug: global_slug(),
            enabled: false,
            source: SkillSource::Company,
            custom_doc: None,
            install: None,
        },
        SkillState {
            slug: "onboard".to_string(),
            enabled: true,
            source: SkillSource::Custom,
            custom_doc: Some(
                "---\nname: Onboard v2\ndescription: Rewritten\ncategory: Ops\nversion: 3.0.0\n---\n# v2\n"
                    .to_string(),
            ),
            install: None,
        },
    ];

    let effective = skill_effective::resolve(Some(tmp.path()), &[], &deltas).expect("resolves");
    let rest: Vec<InstalledSkill> = effective
        .iter()
        .map(InstalledSkill::from_effective)
        .collect();
    let gql = crate::server::graphql::skills::project(&effective);

    assert_eq!(rest.len(), gql.len());
    for (rest, gql) in rest.iter().zip(gql.iter()) {
        assert_eq!(rest.id, gql.id.0);
        assert_eq!(rest.name, gql.name);
        assert_eq!(rest.description, gql.description);
        assert_eq!(rest.category, gql.category);
        assert_eq!(rest.enabled, gql.enabled);
        assert_eq!(rest.version, gql.version);
        assert_eq!(
            serde_json::to_value(rest.source).unwrap(),
            serde_json::Value::String(gql.source.clone()),
        );
    }

    // The divergence this convergence closes: REST refreshed the display
    // fields from a delta's document where GraphQL kept the bundle's.
    let onboard = rest.iter().find(|s| s.id == "onboard").expect("row");
    assert_eq!(onboard.name, "Onboard v2");
    assert_eq!(onboard.version.as_deref(), Some("3.0.0"));
}

/// The test the bug needed: what the console lists and what the harness
/// writes into an agent's skill tree are the same set.
#[cfg(feature = "openhuman")]
#[test]
fn the_rest_list_and_the_harness_effective_set_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    write_bundle(
        tmp.path(),
        "onboard",
        "---\nname: Onboard\ndescription: Get set up\n---\n# Onboard\n",
    );
    let deltas = vec![
        SkillState {
            slug: global_slug(),
            enabled: false,
            source: SkillSource::Company,
            custom_doc: None,
            install: None,
        },
        SkillState {
            slug: "my-skill".to_string(),
            enabled: true,
            source: SkillSource::Custom,
            custom_doc: Some(
                "---\nname: My Skill\ndescription: Does a thing\n---\n# body\n".to_string(),
            ),
            install: None,
        },
    ];

    crate::harness::skills::EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(tmp.path()),
        &[],
        &deltas,
    )
    .expect("materializes");

    let mut materialized: Vec<String> = std::fs::read_dir(ws.path().join("skills"))
        .expect("skill tree")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    materialized.sort();

    let mut listed: Vec<String> = list(Some(tmp.path()), &deltas)
        .into_iter()
        .filter(|row| row.enabled)
        .map(|row| row.id)
        .collect();
    listed.sort();

    assert_eq!(
        listed, materialized,
        "the console's enabled rows are exactly the skills the agents read"
    );
    assert!(
        !materialized.contains(&global_slug()),
        "the disabled global really is withheld from the agents"
    );
    // Pinned against the baseline itself, so an agreement of two empty
    // halves cannot pass for agreement.
    for doc in crate::globals::skills() {
        if doc.slug == global_slug() {
            continue;
        }
        assert!(
            listed.contains(&doc.slug),
            "the console lists the global `{}` the agents read",
            doc.slug
        );
    }
}

/// `valid_slug` is the gate both write handlers share: a slug is also a
/// directory name under `skills/<slug>/`, so a traversal (`..`) or a path
/// separator (`/`) must never reach the filesystem, and the alphabet is
/// lowercase-only. The path extractor can only ever hand a handler a single
/// segment, so `a/b` cannot arrive as a path — but the function is the
/// contract every slug-bearing caller routes through, so it is the right
/// place to pin all three shapes the review named.
#[test]
fn valid_slug_rejects_traversal_separator_and_case() {
    // The shapes the review named.
    assert!(!valid_slug(".."), "parent traversal");
    assert!(!valid_slug("a/b"), "path separator");
    assert!(!valid_slug("A"), "uppercase start");
    // And the rest of the boundary.
    assert!(!valid_slug(""), "empty");
    assert!(!valid_slug("-leading"), "leading dash");
    assert!(!valid_slug("has space"), "interior space");
    assert!(
        !valid_slug("under_score"),
        "underscore is not in the alphabet"
    );
    assert!(!valid_slug("UPPER"), "all uppercase");
    // And the shape that must pass.
    assert!(valid_slug("a-1"), "lowercase, digit, dash");
    assert!(valid_slug("0"), "single digit");
    assert!(valid_slug("seo-audit"), "typical slug");
}

#[test]
fn check_skill_doc_size_rejects_only_over_cap() {
    assert!(check_skill_doc_size(&"x".repeat(MAX_SKILL_DOC_BYTES)).is_ok());
    let err = check_skill_doc_size(&"x".repeat(MAX_SKILL_DOC_BYTES + 1))
        .expect_err("over-cap doc must be refused");
    assert!(matches!(err.0, OpenCompanyError::InvalidRequest(_)));
}

/// The mutual-exclusion property [`write_lock`] exists for: two holders of
/// the same company's lock run strictly one after the other, never
/// interleaved, which is what keeps `set_enabled`'s
/// list-then-preserve-then-write window from landing between another
/// handler's write and its own read.
#[tokio::test]
async fn write_lock_serializes_same_company_writes() {
    let id = CompanyId::new("acme");
    let lock_a = write_lock(&id);
    let lock_b = write_lock(&id);
    assert!(
        Arc::ptr_eq(&lock_a, &lock_b),
        "the same company id must resolve to the same lock"
    );
    // A different company gets its own lock, so one tenant's writes never
    // block another's.
    let other = write_lock(&CompanyId::new("other"));
    assert!(!Arc::ptr_eq(&lock_a, &other));

    let order: Arc<tokio::sync::Mutex<Vec<&'static str>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let guard = lock_a.lock().await;

    let order_clone = Arc::clone(&order);
    let waiter = tokio::spawn(async move {
        // Blocks here until the holder below drops its guard.
        let _guard = lock_b.lock().await;
        order_clone.lock().await.push("second");
    });

    // Give the spawned task a chance to actually reach the blocked
    // `.lock().await` before the holder records its own turn.
    tokio::task::yield_now().await;
    order.lock().await.push("first");
    drop(guard);
    waiter.await.expect("waiter task did not panic");

    assert_eq!(
        *order.lock().await,
        vec!["first", "second"],
        "the second acquirer must not have run until the first released"
    );
}
