use super::*;
use crate::ports::skills_state::{SkillInstall, SkillSource, SkillState, SkillStateStore};
use crate::ports::types::{Actor, ActorKind};

fn tmp_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-skillprov-")
        .tempdir()
        .expect("tempdir")
}

fn installed(slug: &str) -> SkillState {
    SkillState {
        slug: slug.to_string(),
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: Some("---\nname: Web research\nversion: 1.2.0\n---\nsteps".to_string()),
        install: Some(SkillInstall {
            digest: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_string(),
            version: Some("1.2.0".to_string()),
            installed_by: Some(Actor {
                kind: ActorKind::Operator,
                id: "ops@example.com".to_string(),
            }),
            installed_at_millis: 1_700_000_000_000,
        }),
    }
}

async fn read_skills_json(root: &std::path::Path, company: &CompanyId) -> String {
    let path = Bundle::new(root.to_path_buf(), company).skills_json();
    tokio::fs::read_to_string(&path)
        .await
        .expect("skills.json exists")
}

/// `skills.json` is a bundle file, so what the store writes is what an export
/// carries and what an import replays. The provenance must survive that trip
/// whole — a digest that does not come back is a pin nothing can check.
#[tokio::test]
async fn a_row_with_provenance_survives_the_round_trip() {
    let root = tmp_root();
    let ops = FsOps::new(root.path());
    let company = CompanyId::new("acme");
    let state = installed("web-research");

    SkillStateStore::set(&ops, &company, &state)
        .await
        .expect("set");

    let raw = read_skills_json(root.path(), &company).await;
    for key in [
        "\"install\"",
        "\"digest\"",
        "\"version\"",
        "\"installedBy\"",
        "\"installedAtMillis\"",
    ] {
        assert!(raw.contains(key), "{key} missing from {raw}");
    }

    let listed = SkillStateStore::list(&ops, &company).await.expect("list");
    assert_eq!(listed, vec![state]);
}

/// The pre-provenance shape: a row written before `install` existed, exactly as
/// an older host's exported bundle holds it. It must import, not fail, and the
/// absent provenance must read as absent rather than as an empty pin.
#[tokio::test]
async fn a_bundle_written_without_provenance_imports_and_defaults() {
    let root = tmp_root();
    let ops = FsOps::new(root.path());
    let company = CompanyId::new("acme");
    let bundle = Bundle::new(root.path().to_path_buf(), &company);
    bundle.ensure_dirs().await.expect("dirs");
    tokio::fs::write(
        bundle.skills_json(),
        r#"[{"slug":"legacy","enabled":true,"source":"registry",
             "customDoc":"---\nname: Legacy\n---\nbody"}]"#,
    )
    .await
    .expect("write legacy bundle");

    let listed = SkillStateStore::list(&ops, &company).await.expect("list");

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].slug, "legacy");
    assert_eq!(listed[0].source, SkillSource::Registry);
    assert!(listed[0].enabled);
    assert!(
        listed[0].custom_doc.as_deref().unwrap().contains("Legacy"),
        "the document a legacy row carried must survive untouched"
    );
    assert_eq!(
        listed[0].install, None,
        "an absent pin is unknown provenance, not an empty one"
    );
}

/// A row carrying no provenance must not gain any on the way back out, or an
/// export would differ from what it imported.
#[tokio::test]
async fn a_row_without_provenance_writes_no_install_key() {
    let root = tmp_root();
    let ops = FsOps::new(root.path());
    let company = CompanyId::new("acme");

    SkillStateStore::set(
        &ops,
        &company,
        &SkillState {
            slug: "toggle-only".to_string(),
            enabled: false,
            source: SkillSource::Company,
            custom_doc: None,
            install: None,
        },
    )
    .await
    .expect("set");

    let raw = read_skills_json(root.path(), &company).await;
    assert!(!raw.contains("install"), "{raw}");
}

/// The optional halves of a pin are optional on the wire too: a document with
/// no `version`, installed by a surface carrying no actor, is a real install
/// and must not be written as `null` or refused on the way back in.
#[tokio::test]
async fn a_pin_without_a_version_or_an_actor_round_trips() {
    let root = tmp_root();
    let ops = FsOps::new(root.path());
    let company = CompanyId::new("acme");
    let mut state = installed("anonymous");
    let install = state.install.as_mut().expect("install");
    install.version = None;
    install.installed_by = None;

    SkillStateStore::set(&ops, &company, &state)
        .await
        .expect("set");

    let raw = read_skills_json(root.path(), &company).await;
    assert!(!raw.contains("\"version\""), "{raw}");
    assert!(!raw.contains("\"installedBy\""), "{raw}");
    assert!(raw.contains("\"digest\""), "{raw}");

    assert_eq!(
        SkillStateStore::list(&ops, &company).await.expect("list"),
        vec![state]
    );
}

/// Company A's provenance must be as invisible to company B as its deltas
/// already are — the pin names who installed what, and that is the operator's.
#[tokio::test]
async fn provenance_stays_inside_its_company() {
    let root = tmp_root();
    let ops = FsOps::new(root.path());
    let alpha = CompanyId::new("alpha");
    let beta = CompanyId::new("beta");

    SkillStateStore::set(&ops, &alpha, &installed("web-research"))
        .await
        .expect("set");

    assert!(
        SkillStateStore::list(&ops, &beta)
            .await
            .expect("list")
            .is_empty()
    );
    let raw = read_skills_json(root.path(), &alpha).await;
    assert!(raw.contains("ops@example.com"));
}
