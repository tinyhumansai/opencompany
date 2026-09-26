use super::*;

use crate::company::parse_skill_md;
use crate::ports::skills_state::SkillSource;

/// Writes a company-dir `skills/<slug>/SKILL.md` (plus an optional resource).
fn seed_company_skill(source_dir: &Path, slug: &str, name: &str, resource: Option<&str>) {
    let dir = source_dir.join("skills").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {slug} does things\n---\n\n# {name}\n"),
    )
    .unwrap();
    if let Some(body) = resource {
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("references").join("spec.md"), body).unwrap();
    }
}

fn delta(slug: &str, enabled: bool, custom_doc: Option<&str>) -> SkillState {
    SkillState {
        slug: slug.to_string(),
        enabled,
        source: if custom_doc.is_some() {
            SkillSource::Custom
        } else {
            SkillSource::Company
        },
        custom_doc: custom_doc.map(str::to_string),
    }
}

/// A `Registry`-sourced delta, as `install` persists one.
fn registry_delta(slug: &str, custom_doc: &str) -> SkillState {
    SkillState {
        slug: slug.to_string(),
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: Some(custom_doc.to_string()),
    }
}

/// How many docs an effective set has once the always-installed global
/// baseline is accounted for: this fixture's own slugs, unioned with the
/// baseline's (a fixture that uses a baseline slug supersedes it rather than
/// adding to it).
fn with_baseline(slugs: &[&str]) -> usize {
    let mut all: std::collections::BTreeSet<&str> = slugs.iter().copied().collect();
    all.extend(crate::globals::skills().iter().map(|doc| doc.slug.as_str()));
    all.len()
}

/// The effective doc for `slug` — the assertions below are about one skill
/// each, and indexing stopped identifying it once every company installs a
/// baseline that sorts among the fixture's own.
fn doc<'a>(eff: &'a EffectiveSkills, slug: &str) -> &'a SkillDoc {
    eff.docs
        .iter()
        .find(|doc| doc.slug == slug)
        .unwrap_or_else(|| panic!("no `{slug}` in the effective set"))
}

/// A shared-library document with a real multi-section body.
fn library_doc(slug: &str) -> SkillDoc {
    SkillDoc {
        slug: slug.to_string(),
        name: "Competitor Scan".to_string(),
        description: "Profile competitors.".to_string(),
        category: Some("Research".to_string()),
        version: Some("1.0.0".to_string()),
        body: "\n# Competitor Scan\n\n## Steps\n\n1. Pick.\n\n## Output\n\nA table.\n".to_string(),
        extra_frontmatter: Vec::new(),
    }
}

/// Exactly what the pre-fix `install` wrote: the description doubling as the
/// body. Such a row must be re-served from the live library.
#[test]
fn a_pre_fix_registry_stub_is_healed_from_the_live_library() {
    let ws = tempfile::tempdir().unwrap();
    let stub = "---\nname: Competitor Scan\ndescription: Profile competitors.\ncategory: Research\n---\nProfile competitors.\n";
    let library = [library_doc("competitor-scan")];

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &library,
        &[registry_delta("competitor-scan", stub)],
    )
    .unwrap();

    assert_eq!(eff.docs.len(), with_baseline(&["competitor-scan"]));
    let healed = doc(&eff, "competitor-scan");
    assert!(healed.body.contains("## Steps"));
    assert!(healed.body.contains("## Output"));

    // The agent reads the tree on disk, so the heal must land there too.
    let on_disk =
        std::fs::read_to_string(ws.path().join("skills/competitor-scan/SKILL.md")).unwrap();
    assert!(on_disk.contains("## Steps"), "{on_disk}");
    assert!(on_disk.contains("## Output"), "{on_disk}");
    assert!(on_disk.contains("version: 1.0.0"), "{on_disk}");
}

#[test]
fn a_post_fix_registry_snapshot_is_left_pinned() {
    let ws = tempfile::tempdir().unwrap();
    // A real snapshot (body ≠ description) is never second-guessed, even
    // when the live library has since moved on.
    let pinned = "---\nname: Competitor Scan\ndescription: Profile competitors.\n---\n\n## Steps\n\n1. The pinned revision.\n";
    let mut newer = library_doc("competitor-scan");
    newer.body = "\n## Steps\n\n1. A NEWER revision.\n".to_string();

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[newer],
        &[registry_delta("competitor-scan", pinned)],
    )
    .unwrap();

    assert!(
        eff.docs[0].body.contains("The pinned revision"),
        "an installed snapshot must not silently track the library"
    );
}

#[test]
fn a_custom_skill_is_never_healed_even_when_its_body_is_one_line() {
    let ws = tempfile::tempdir().unwrap();
    // Same degenerate shape, but operator-authored: the heal must not touch
    // it, or console-written content would be replaced by library content.
    let authored = "---\nname: Competitor Scan\ndescription: My own note.\n---\nMy own note.\n";
    let mut delta = registry_delta("competitor-scan", authored);
    delta.source = SkillSource::Custom;

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[library_doc("competitor-scan")],
        &[delta],
    )
    .unwrap();

    assert_eq!(eff.docs[0].body.trim(), "My own note.");
    assert!(!doc(&eff, "competitor-scan").body.contains("## Steps"));
}

/// The pre-fix path wrote `description:` with an empty value when the client
/// sent no description, which the parser rejects — so the row was dropped
/// from the effective set and the agent never saw the skill at all. Once the
/// library can serve the slug, such a row heals.
#[test]
fn an_unparseable_registry_snapshot_is_healed_rather_than_dropped() {
    let ws = tempfile::tempdir().unwrap();
    let broken = "---\nname: Competitor Scan\ndescription: \n---\n\n";
    assert!(
        parse_skill_md("competitor-scan", broken).is_err(),
        "this shape really is unparseable"
    );

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[library_doc("competitor-scan")],
        &[registry_delta("competitor-scan", broken)],
    )
    .unwrap();

    assert_eq!(
        eff.docs.len(),
        with_baseline(&["competitor-scan"]),
        "the skill is no longer silently dropped"
    );
    assert!(doc(&eff, "competitor-scan").body.contains("## Steps"));
}

/// The same unparseable row, but for a slug the library cannot serve (a
/// phantom the old console offered). Nothing exists to heal from, so it stays
/// dropped — unchanged from today.
#[test]
fn an_unparseable_snapshot_the_library_lacks_stays_dropped() {
    let ws = tempfile::tempdir().unwrap();
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[library_doc("competitor-scan")],
        &[registry_delta(
            "social-scheduler",
            "---\nname: X\ndescription: \n---\n",
        )],
    )
    .unwrap();
    assert_eq!(
        eff.docs.len(),
        with_baseline(&[]),
        "the phantom stays dropped; only the baseline remains"
    );
}

#[test]
fn a_stub_for_a_slug_the_library_lacks_is_left_alone() {
    let ws = tempfile::tempdir().unwrap();
    let stub =
        "---\nname: Retired\ndescription: Gone from the library.\n---\nGone from the library.\n";

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[library_doc("competitor-scan")],
        &[registry_delta("retired", stub)],
    )
    .unwrap();

    assert_eq!(
        eff.docs.len(),
        with_baseline(&["retired"]),
        "the skill survives rather than vanishing"
    );
    assert_eq!(doc(&eff, "retired").body.trim(), "Gone from the library.");
}

#[test]
fn company_dir_skills_materialize_with_resources() {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    seed_company_skill(src.path(), "web-research", "Web Research", Some("# spec"));

    let eff =
        EffectiveSkills::materialize(ws.path().to_path_buf(), Some(src.path()), &[], &[]).unwrap();

    // The parsed doc surfaces in the catalogue.
    assert_eq!(eff.docs.len(), with_baseline(&["web-research"]));
    let cat = eff.catalogue();
    assert!(cat.contains("Web Research"), "{cat}");
    assert!(cat.contains("`web-research`"), "{cat}");

    // `web-research` is also a global baseline skill, so this doubles as
    // the precedence check: the company's bundle supersedes it, resources
    // and all, rather than the two merging.
    assert_eq!(doc(&eff, "web-research").name, "Web Research");

    // The bundle (SKILL.md + resource) is copied verbatim into the scratch.
    let out = ws.path().join("skills").join("web-research");
    assert!(out.join("SKILL.md").is_file());
    assert_eq!(
        std::fs::read_to_string(out.join("references").join("spec.md")).unwrap(),
        "# spec"
    );
}

#[test]
fn disabled_delta_drops_a_company_skill() {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    seed_company_skill(src.path(), "keep", "Keep", None);
    seed_company_skill(src.path(), "drop", "Drop", None);

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[delta("drop", false, None)],
    )
    .unwrap();

    let slugs: Vec<&str> = eff
        .docs
        .iter()
        .map(|d| d.slug.as_str())
        .filter(|slug| ["keep", "drop"].contains(slug))
        .collect();
    assert_eq!(slugs, vec!["keep"]);
    assert!(!ws.path().join("skills").join("drop").exists());
    assert!(ws.path().join("skills").join("keep").exists());
}

#[test]
fn custom_doc_installs_a_new_skill() {
    let ws = tempfile::tempdir().unwrap();
    let body = "---\nname: Invoicing\ndescription: Draft an invoice\n---\n\n# Invoicing\n";

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[],
        &[delta("invoicing", true, Some(body))],
    )
    .unwrap();

    assert_eq!(eff.docs.len(), with_baseline(&["invoicing"]));
    assert_eq!(doc(&eff, "invoicing").name, "Invoicing");
    let written =
        std::fs::read_to_string(ws.path().join("skills").join("invoicing").join("SKILL.md"))
            .unwrap();
    assert_eq!(written, body);
}

/// A delta whose slug is not a safe directory name must never reach the
/// `skills_out.join(slug)` write: `..` would escape the scratch tree, and
/// the console validates slugs at write time, so such a row is either a
/// pre-check row or a non-console write — skip it either way.
#[test]
fn a_traversal_slug_delta_is_skipped_not_written_outside() {
    let ws = tempfile::tempdir().unwrap();
    let body = "---\nname: Escape\ndescription: Should never land\n---\n\n# Escape\n";

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[],
        &[delta("..", true, Some(body))],
    )
    .unwrap();

    // The bogus delta contributed nothing to the effective set…
    assert_eq!(eff.docs.len(), with_baseline(&[]));
    assert!(eff.docs.iter().all(|doc| doc.slug != ".."));
    // …and its write never escaped the scratch tree: `skills/..` resolves
    // to the workspace root, which is where the escaped SKILL.md would have
    // landed.
    assert!(
        !ws.path().join("SKILL.md").exists(),
        "the traversal delta wrote nothing outside the skills tree"
    );
    // Only the baseline dirs materialize — no `..` directory inside either.
    assert_eq!(
        std::fs::read_dir(ws.path().join("skills")).unwrap().count(),
        eff.docs.len()
    );
}

#[test]
fn custom_doc_supersedes_company_body() {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    seed_company_skill(src.path(), "report", "Old Report", None);
    let body = "---\nname: New Report\ndescription: Updated\n---\n\n# New\n";

    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        Some(src.path()),
        &[],
        &[delta("report", true, Some(body))],
    )
    .unwrap();

    assert_eq!(eff.docs.len(), with_baseline(&["report"]));
    assert_eq!(doc(&eff, "report").name, "New Report");
    let written =
        std::fs::read_to_string(ws.path().join("skills").join("report").join("SKILL.md")).unwrap();
    assert_eq!(written, body);
}

#[test]
fn malformed_custom_doc_is_skipped_not_fatal() {
    let ws = tempfile::tempdir().unwrap();
    let eff = EffectiveSkills::materialize(
        ws.path().to_path_buf(),
        None,
        &[],
        &[delta("broken", true, Some("no frontmatter here"))],
    )
    .expect("malformed custom doc must not fail the build");
    // The malformed doc is skipped; what remains is the baseline every
    // company installs, so the set is not empty — it just never gained the
    // broken skill.
    assert_eq!(eff.docs.len(), with_baseline(&[]));
    assert!(eff.docs.iter().all(|doc| doc.slug != "broken"));
}

/// A manifest opt-out drops a baseline skill, through the same disabling
/// delta an operator's console toggle writes.
#[test]
fn a_manifest_opt_out_drops_a_global_skill() {
    let ws = tempfile::tempdir().unwrap();
    let dropped = crate::globals::skills()[0].slug.clone();
    let deltas = skill_effective::globals_skill_disables(&[format!("skill:{dropped}")]);

    let eff = EffectiveSkills::materialize(ws.path().to_path_buf(), None, &[], &deltas).unwrap();

    assert!(eff.docs.iter().all(|doc| doc.slug != dropped));
    assert_eq!(eff.docs.len(), crate::globals::skills().len() - 1);
    assert!(!ws.path().join("skills").join(&dropped).exists());
}

#[test]
fn a_company_with_no_sources_still_gets_the_global_baseline() {
    // This used to assert an empty set. Nothing about the layering changed:
    // the baseline is simply installed in every company, including one with
    // no source dir and no deltas — a platform-provisioned tenant.
    let ws = tempfile::tempdir().unwrap();
    let eff = EffectiveSkills::materialize(ws.path().to_path_buf(), None, &[], &[]).unwrap();
    assert_eq!(eff.docs.len(), with_baseline(&[]));
    assert!(!eff.is_empty());
    assert!(!eff.catalogue().is_empty());
}

#[test]
fn read_tools_expose_three_named_tools() {
    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    seed_company_skill(src.path(), "web-research", "Web Research", None);
    let eff =
        EffectiveSkills::materialize(ws.path().to_path_buf(), Some(src.path()), &[], &[]).unwrap();

    let tools = eff.read_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    // Named after skills, not upstream's "workflow" (issue #845) — the
    // naming boundary itself is covered in `naming::test`.
    assert_eq!(
        names,
        vec![
            LIST_SKILLS_TOOL,
            DESCRIBE_SKILL_TOOL,
            READ_SKILL_RESOURCE_TOOL
        ]
    );
    // The tools point at the materialized scratch dir.
    assert_eq!(eff.workspace_dir(), ws.path());
}

/// The console writes custom + registry skills as `SkillState` rows carrying
/// the full `SKILL.md` inline in `custom_doc` (the registry path since PR
/// #47). Both shapes must materialize and surface their **content** through
/// the agent's read tools — a green build isn't enough, the body has to be
/// readable. Also covers a frontmatter-only (empty-body) custom skill.
#[tokio::test]
async fn console_custom_docs_surface_content_through_read_tools() {
    use serde_json::json;

    let ws = tempfile::tempdir().unwrap();

    // Registry-install shape: source = Registry, full SKILL.md in custom_doc.
    let registry = SkillState {
        slug: "web-research".to_string(),
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: Some(
            "---\nname: Web Research\ndescription: Research a topic online\n---\n\n\
             # Web Research\n\nBODY-RESEARCH-MARKER\n"
                .to_string(),
        ),
    };
    // Console-authored custom skill with an empty body (frontmatter only).
    let empty_body = SkillState {
        slug: "quick-note".to_string(),
        enabled: true,
        source: SkillSource::Custom,
        custom_doc: Some("---\nname: Quick Note\ndescription: Jot a quick note\n---\n".to_string()),
    };

    let eff =
        EffectiveSkills::materialize(ws.path().to_path_buf(), None, &[], &[registry, empty_body])
            .unwrap();
    assert_eq!(
        eff.docs.len(),
        with_baseline(&["web-research", "quick-note"]),
        "both console deltas materialize"
    );

    let tools = eff.read_tools();
    let list = tools
        .iter()
        .find(|t| t.name() == LIST_SKILLS_TOOL)
        .expect("list tool");
    let listed = list
        .execute(json!({}))
        .await
        .expect("list")
        .output_for_llm(false);
    // Both enumerate, each carrying its parsed description (content).
    assert!(listed.contains("web-research"), "{listed}");
    assert!(listed.contains("Research a topic online"), "{listed}");
    assert!(listed.contains("quick-note"), "{listed}");

    let describe = tools
        .iter()
        .find(|t| t.name() == DESCRIBE_SKILL_TOOL)
        .expect("describe tool");

    // The registry skill's inline body is readable — content, not just name.
    let desc = describe
        .execute(json!({ "skill_id": "web-research" }))
        .await
        .expect("describe registry skill")
        .output_for_llm(false);
    assert!(desc.contains("BODY-RESEARCH-MARKER"), "{desc}");
    assert!(desc.contains("Research a topic online"), "{desc}");

    // The empty-body custom skill still describes cleanly (frontmatter → def).
    let desc_empty = describe
        .execute(json!({ "skill_id": "quick-note" }))
        .await
        .expect("describe empty-body skill")
        .output_for_llm(false);
    assert!(desc_empty.contains("Jot a quick note"), "{desc_empty}");
}

#[tokio::test]
async fn list_skills_tool_sees_the_materialized_skill() {
    use serde_json::json;

    let src = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    seed_company_skill(src.path(), "web-research", "Web Research", None);
    let eff =
        EffectiveSkills::materialize(ws.path().to_path_buf(), Some(src.path()), &[], &[]).unwrap();

    let tools = eff.read_tools();
    let list = tools
        .iter()
        .find(|t| t.name() == LIST_SKILLS_TOOL)
        .expect("list tool");
    let result = list.execute(json!({})).await.expect("execute");
    let text = result.output_for_llm(false);
    // The legacy `<workspace>/skills/` root is scanned without a trust
    // marker, so the materialized bundle shows up in the tool's output.
    assert!(
        text.contains("web-research") || text.contains("Web Research"),
        "{text}"
    );
}
