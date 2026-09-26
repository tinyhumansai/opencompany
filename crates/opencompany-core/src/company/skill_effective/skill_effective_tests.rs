//! Resolution tests for the shared effective-skill derivation.

use std::path::Path;

use super::*;

/// The slugs the global baseline installs in every company.
fn global_slugs() -> Vec<String> {
    crate::globals::skills()
        .iter()
        .map(|doc| doc.slug.clone())
        .collect()
}

fn seed_bundle(source_dir: &Path, slug: &str, name: &str) {
    let dir = source_dir.join("skills").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {slug} does things\n---\n\n# {name}\n"),
    )
    .unwrap();
}

fn delta(slug: &str, enabled: bool, source: SkillSource, custom_doc: Option<&str>) -> SkillState {
    SkillState {
        slug: slug.to_string(),
        enabled,
        source,
        custom_doc: custom_doc.map(str::to_string),
    }
}

fn find<'a>(set: &'a [EffectiveSkill], slug: &str) -> &'a EffectiveSkill {
    set.iter()
        .find(|skill| skill.slug == slug)
        .unwrap_or_else(|| panic!("no `{slug}` in the effective set"))
}

/// The baseline reaches a company with no bundles and no deltas — the shape a
/// platform-provisioned tenant boots in.
#[test]
fn the_global_baseline_is_the_bottom_layer() {
    let set = resolve(None, &[], &[]).unwrap();

    let slugs = global_slugs();
    assert!(
        !slugs.is_empty(),
        "the baseline installs at least one skill"
    );
    for slug in &slugs {
        let skill = find(&set, slug);
        assert!(skill.enabled, "a global is enabled unless a delta says not");
        assert_eq!(skill.source, SkillSource::Company);
        assert!(skill.doc().is_some(), "a global carries its document");
    }
}

/// The set is ordered by slug, so every reader answers deterministically.
#[test]
fn the_set_is_ordered_by_slug() {
    let tmp = tempfile::tempdir().unwrap();
    seed_bundle(tmp.path(), "zebra", "Zebra");
    seed_bundle(tmp.path(), "alpha", "Alpha");

    let set = resolve(Some(tmp.path()), &[], &[]).unwrap();
    let slugs: Vec<&str> = set.iter().map(|skill| skill.slug.as_str()).collect();
    let mut sorted = slugs.clone();
    sorted.sort_unstable();
    assert_eq!(slugs, sorted);
}

/// A same-slug company bundle supersedes the global, and brings its own
/// directory so the bundle's resource files travel with it.
#[test]
fn a_company_bundle_supersedes_a_global_of_the_same_slug() {
    let tmp = tempfile::tempdir().unwrap();
    let slug = global_slugs()[0].clone();
    seed_bundle(tmp.path(), &slug, "Company Override");

    let set = resolve(Some(tmp.path()), &[], &[]).unwrap();
    let skill = find(&set, &slug);
    assert_eq!(skill.doc().unwrap().name, "Company Override");
    assert_eq!(
        skill.content.as_ref().unwrap().body,
        SkillBody::Bundle(tmp.path().join("skills").join(&slug))
    );
}

/// A disabled global stays in the set, flagged — the console needs the row to
/// render the switch that turns it back on.
#[test]
fn a_disabling_delta_reports_a_global_as_disabled_rather_than_hiding_it() {
    let slug = global_slugs()[0].clone();
    let set = resolve(
        None,
        &[],
        &[delta(&slug, false, SkillSource::Company, None)],
    )
    .unwrap();

    let skill = find(&set, &slug);
    assert!(!skill.enabled);
    assert!(
        skill.doc().is_some(),
        "a disabled row keeps its name and description"
    );
}

/// The manifest's `[globals].disable` reaches the resolution as a synthesized
/// delta, and wins over a console re-enable of the same slug.
#[test]
fn a_manifest_opt_out_disables_a_global_and_beats_an_enable() {
    let slug = global_slugs()[0].clone();
    let mut deltas = vec![delta(&slug, true, SkillSource::Company, None)];
    deltas.extend(globals_skill_disables(&[format!("skill:{slug}")]));

    let skill_set = resolve(None, &[], &deltas).unwrap();
    let skill = find(&skill_set, &slug);
    assert!(!skill.enabled, "the company's own declaration wins");
    assert_eq!(
        skill.source,
        SkillSource::Company,
        "an opt-out does not restate the skill's provenance"
    );
}

/// A `[globals].disable` entry naming another kind is that kind's business.
#[test]
fn globals_skill_disables_ignores_other_kinds() {
    let disables = globals_skill_disables(&[
        "skill:web-research".to_string(),
        "agent:researcher".to_string(),
        "ledger:invoices".to_string(),
    ]);
    assert_eq!(disables.len(), 1);
    assert_eq!(disables[0].slug, "web-research");
    assert!(!disables[0].enabled);
}

/// A delta carrying a document supersedes the body beneath it, bundle or global.
#[test]
fn a_custom_doc_supersedes_the_layer_beneath_it() {
    let tmp = tempfile::tempdir().unwrap();
    seed_bundle(tmp.path(), "onboard", "Onboard");
    let authored = "---\nname: Onboard v2\ndescription: The operator's own\n---\n# v2\n";

    let set = resolve(
        Some(tmp.path()),
        &[],
        &[delta("onboard", true, SkillSource::Custom, Some(authored))],
    )
    .unwrap();

    let skill = find(&set, "onboard");
    assert_eq!(skill.doc().unwrap().name, "Onboard v2");
    assert_eq!(skill.source, SkillSource::Custom);
    assert_eq!(
        skill.content.as_ref().unwrap().body,
        SkillBody::Inline(authored.to_string())
    );
}

/// A malformed `custom_doc` costs that skill its document, never the resolution.
#[test]
fn a_malformed_custom_doc_leaves_the_row_without_a_document() {
    let set = resolve(
        None,
        &[],
        &[delta(
            "broken",
            true,
            SkillSource::Custom,
            Some("no frontmatter here\n"),
        )],
    )
    .unwrap();

    let skill = find(&set, "broken");
    assert!(skill.doc().is_none());
    assert!(skill.enabled);
}

/// A traversal slug must never reach a `skills/<slug>/` write, so it never
/// reaches the set either.
#[test]
fn a_delta_with_an_unsafe_slug_is_skipped() {
    let set = resolve(
        None,
        &[],
        &[delta("../escape", true, SkillSource::Custom, None)],
    )
    .unwrap();
    assert!(set.iter().all(|skill| skill.slug != "../escape"));
}

/// A registry install with no snapshot at all contributes no document: the
/// agent gets nothing for it, so neither reader may dress it up with library
/// text the agent never sees.
#[test]
fn a_registry_delta_with_no_snapshot_contributes_no_document() {
    let library = [SkillDoc {
        slug: "competitor-scan".to_string(),
        name: "Competitor Scan".to_string(),
        description: "Profile competitors.".to_string(),
        category: Some("Research".to_string()),
        version: Some("1.0.0".to_string()),
        body: "\n## Steps\n\n1. Pick.\n".to_string(),
        extra_frontmatter: Vec::new(),
    }];

    let set = resolve(
        None,
        &library,
        &[delta("competitor-scan", true, SkillSource::Registry, None)],
    )
    .unwrap();

    assert!(find(&set, "competitor-scan").doc().is_none());
}

/// A malformed company bundle fails the resolution rather than yielding the
/// surviving subset: the harness loses the whole catalogue for that company, so
/// a tidy partial list would describe a set no agent has.
#[test]
fn a_malformed_company_bundle_fails_the_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    seed_bundle(tmp.path(), "onboard", "Onboard");
    let broken = tmp.path().join("skills").join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("SKILL.md"), "no frontmatter here\n").unwrap();

    assert!(resolve(Some(tmp.path()), &[], &[]).is_err());
}
