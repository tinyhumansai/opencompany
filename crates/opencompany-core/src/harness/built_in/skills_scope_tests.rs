use super::*;

use crate::ports::skills_state::SkillSource;

/// Writes a company-dir `skills/<slug>/SKILL.md` plus one bundled resource, so
/// a scoped-out skill has a file for `read_skill_resource` to fail to open.
fn seed(source_dir: &Path, slug: &str, marker: &str) {
    let dir = source_dir.join("skills").join(slug);
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {slug}\ndescription: {slug} does things\n---\n\n# {slug}\n"),
    )
    .unwrap();
    std::fs::write(dir.join("references").join("spec.md"), marker).unwrap();
}

fn scope(slugs: &[&str]) -> Vec<String> {
    slugs.iter().map(|s| s.to_string()).collect()
}

fn disabling_delta(slug: &str) -> SkillState {
    SkillState {
        slug: slug.to_string(),
        enabled: false,
        source: SkillSource::Company,
        custom_doc: None,
    }
}

/// Two teammates in one company, scoped apart: each tree holds its own slug and
/// not the other's. The filesystem is the assertion because the tree is what the
/// read tools are built over.
#[test]
fn each_agent_gets_only_the_skills_its_scope_admits() {
    let src = tempfile::tempdir().unwrap();
    seed(src.path(), "finance-playbook", "FINANCE-MARKER");
    seed(src.path(), "brand-voice", "BRAND-MARKER");

    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let finance = scope(&["finance-playbook"]);
    let brand = scope(&["brand-voice"]);

    EffectiveSkills::materialize(
        ws_a.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        Some(&finance),
    )
    .unwrap();
    EffectiveSkills::materialize(
        ws_b.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        Some(&brand),
    )
    .unwrap();

    assert!(ws_a.path().join("skills/finance-playbook").is_dir());
    assert!(!ws_a.path().join("skills/brand-voice").exists());
    assert!(ws_b.path().join("skills/brand-voice").is_dir());
    assert!(!ws_b.path().join("skills/finance-playbook").exists());
}

/// The leak the design turns on. Hiding a skill from the catalogue is not
/// enough: the three read tools are built over the materialized tree and nothing
/// else, so a skill an agent's scope excludes must never be written for it —
/// then `read_skill_resource` has nothing to open, whatever the agent asks for.
#[tokio::test]
async fn an_agent_cannot_read_a_resource_of_a_skill_scoped_to_another() {
    use serde_json::json;

    let src = tempfile::tempdir().unwrap();
    seed(src.path(), "finance-playbook", "FINANCE-MARKER");
    seed(src.path(), "brand-voice", "BRAND-MARKER");

    let ws = tempfile::tempdir().unwrap();
    let brand = scope(&["brand-voice"]);
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        Some(&brand),
    )
    .unwrap();

    let tools = eff.read_tools();

    let listed = tools
        .iter()
        .find(|t| t.name() == LIST_SKILLS_TOOL)
        .expect("list tool")
        .execute(json!({}))
        .await
        .expect("list")
        .output_for_llm(false);
    assert!(listed.contains("brand-voice"), "{listed}");
    assert!(!listed.contains("finance-playbook"), "{listed}");

    let read = tools
        .iter()
        .find(|t| t.name() == READ_SKILL_RESOURCE_TOOL)
        .expect("read tool");

    // Its own skill's resource is readable, so the assertion below is about the
    // scope and not about a broken fixture.
    let mine = read
        .execute(json!({ "skill_id": "brand-voice", "relative_path": "references/spec.md" }))
        .await
        .expect("read own resource")
        .output_for_llm(false);
    assert!(mine.contains("BRAND-MARKER"), "{mine}");

    // The other teammate's skill is unreachable, by slug and by traversal.
    let theirs = read
        .execute(json!({ "skill_id": "finance-playbook", "relative_path": "references/spec.md" }))
        .await;
    let denied = match theirs {
        Ok(result) => result.output_for_llm(false),
        Err(err) => err.to_string(),
    };
    assert!(!denied.contains("FINANCE-MARKER"), "{denied}");

    let escaped = read
        .execute(json!({
            "skill_id": "brand-voice",
            "relative_path": "../finance-playbook/references/spec.md",
        }))
        .await;
    let escaped = match escaped {
        Ok(result) => result.output_for_llm(false),
        Err(err) => err.to_string(),
    };
    assert!(!escaped.contains("FINANCE-MARKER"), "{escaped}");
}

/// An explicit empty scope is a real deny-all: nothing on disk, and no read
/// tools offered, because the catalogue is derived from what was materialized.
#[test]
fn an_explicit_empty_scope_materializes_nothing() {
    let src = tempfile::tempdir().unwrap();
    seed(src.path(), "brand-voice", "BRAND-MARKER");

    let ws = tempfile::tempdir().unwrap();
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        Some(&[]),
    )
    .unwrap();

    assert!(eff.is_empty());
    assert_eq!(eff.catalogue(), "");
    assert!(!ws.path().join("skills/brand-voice").exists());
}

/// An absent scope is what every company had before one could be written, so it
/// must still materialize the whole enabled set.
#[test]
fn an_absent_scope_materializes_every_enabled_skill() {
    let src = tempfile::tempdir().unwrap();
    seed(src.path(), "finance-playbook", "FINANCE-MARKER");
    seed(src.path(), "brand-voice", "BRAND-MARKER");

    let ws = tempfile::tempdir().unwrap();
    EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[],
        "agent-under-test",
        None,
    )
    .unwrap();

    assert!(ws.path().join("skills/finance-playbook").is_dir());
    assert!(ws.path().join("skills/brand-voice").is_dir());
}

/// Narrow-only: a scope naming a skill the company has disabled resolves to
/// nothing. A teammate's scope can never put back what the company took away.
#[test]
fn a_scope_cannot_restore_a_skill_the_company_disabled() {
    let src = tempfile::tempdir().unwrap();
    seed(src.path(), "brand-voice", "BRAND-MARKER");

    let ws = tempfile::tempdir().unwrap();
    let brand = scope(&["brand-voice"]);
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[disabling_delta("brand-voice")],
        "agent-under-test",
        Some(&brand),
    )
    .unwrap();

    assert!(!ws.path().join("skills/brand-voice").exists());
    assert!(
        !eff.catalogue().contains("brand-voice"),
        "{}",
        eff.catalogue()
    );
}
