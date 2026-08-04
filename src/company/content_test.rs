//! Content-validation walk over the shipped `companies/*` and `skills/*`.
//!
//! These tests parse every data file the WS1 readers cover against the real
//! on-disk content, so any future content edit that breaks the frozen formats
//! fails CI. This guards WS8 authoring forever (see `docs/specs/09-verification.md`).

use std::path::{Path, PathBuf};

use super::{
    CompanyManifest, Tools, grants_composio_explicit, grants_media_explicit,
    grants_search_explicit, load_dir_skills, parse_workflow, walk_workspace,
};
use crate::runtime::builder::effective_grants;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_company_manifest_is_valid() {
    let companies = repo_root().join("companies");
    let dirs = subdirs(&companies);
    assert!(!dirs.is_empty(), "no companies found under {companies:?}");

    for company in dirs {
        let manifest = CompanyManifest::from_path(&company)
            .unwrap_or_else(|err| panic!("{}: {err}", company.display()));
        let problems = manifest.validate();
        assert!(
            problems.is_empty(),
            "{} has manifest problems: {problems:?}",
            company.display()
        );
    }
}

#[test]
fn every_workflow_graph_parses() {
    for company in subdirs(&repo_root().join("companies")) {
        for file in toml_files(&company.join("workflows")) {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
            let workflow =
                parse_workflow(&text).unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            // The filename must match the declared workflow id.
            let stem = file.file_stem().and_then(|stem| stem.to_str()).unwrap();
            assert_eq!(
                workflow.id,
                stem,
                "{} declares id `{}` but is named `{stem}.toml`",
                file.display(),
                workflow.id
            );
        }
    }
}

#[test]
fn every_company_skill_and_workspace_parses() {
    for company in subdirs(&repo_root().join("companies")) {
        // Per-company skills (a missing dir yields an empty list).
        load_dir_skills(&company.join("skills"))
            .unwrap_or_else(|err| panic!("{}/skills: {err}", company.display()));
        // Workspace tree.
        walk_workspace(&company.join("workspace"))
            .unwrap_or_else(|err| panic!("{}/workspace: {err}", company.display()));
    }
}

/// Templates whose shipped skills (`web-research`, `seo-audit`,
/// `competitor-scan`) instruct agents to search the web and cite sources, so
/// each must carry an explicit `search` grant (issue #312).
const SEARCH_GRANTED_COMPANIES: [&str; 6] = [
    "agentic_consultation_firm",
    "agentic_design_studio",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_media_company",
    "signals_opportunity_studio",
];

/// Templates that must NEVER reach the metered search backend: `e2e_harness` is
/// a deterministic fixture (a priced network call would make it non-hermetic and
/// flaky), and `openhuman_demo` is a walkthrough nobody opted into spend for.
const SEARCH_DENIED_COMPANIES: [&str; 2] = ["e2e_harness", "openhuman_demo"];

/// The subset of [`SEARCH_GRANTED_COMPANIES`] that restates the default belt
/// verbatim and appends `search`. `signals_opportunity_studio` is deliberately
/// excluded: it overrides the default down to a research-only belt on purpose.
const FULL_BELT_PLUS_SEARCH: [&str; 5] = [
    "agentic_consultation_firm",
    "agentic_design_studio",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_media_company",
];

fn load_company(name: &str) -> CompanyManifest {
    let dir = repo_root().join("companies").join(name);
    CompanyManifest::from_path(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
}

/// One agent's effective grants: the company `[tools].allow` narrowed by that
/// agent's own `tools`. Runs the *real* narrowing (`effective_grants` over a
/// one-agent roster) rather than reimplementing it, so the test cannot drift
/// from the rule the harness applies.
fn grants_for_one_agent(manifest: &CompanyManifest, index: usize) -> Vec<String> {
    let mut solo = manifest.clone();
    solo.agents = vec![manifest.agents[index].clone()];
    effective_grants(&solo)
}

/// The metered `web_search` tool (issue #238) is wired only behind an explicit
/// `search` grant — the catch-all `*` deliberately does not confer it. That
/// grant is narrowed twice: by the company-wide `[tools].allow`, and again by
/// each agent's own `tools` list. An agent that declares `tools` and omits
/// `search` is silently searchless even when the company grants it, which is
/// exactly how `signals_opportunity_studio`'s scout shipped unable to search.
#[test]
fn research_templates_grant_search_at_both_layers() {
    for name in SEARCH_GRANTED_COMPANIES {
        let manifest = load_company(name);
        assert!(
            grants_search_explicit(&manifest.tools.allow),
            "{name}: company-wide `[tools].allow` must grant `search` \
             (found {:?}); note `web.*` confers nothing here — the check \
             matches only `search` / `search.`",
            manifest.tools.allow
        );
        assert!(
            !manifest.agents.is_empty(),
            "{name}: expected a roster to check per-agent grants against"
        );
        for (index, agent) in manifest.agents.iter().enumerate() {
            let grants = grants_for_one_agent(&manifest, index);
            assert!(
                grants_search_explicit(&grants),
                "{name}: agent `{}` ends up without `search`. Its own \
                 `tools` list ({:?}) narrows the company allow-list ({:?}), so \
                 `search` has to appear in BOTH — either edit alone is a \
                 silent no-op.",
                agent.id,
                agent.tools,
                manifest.tools.allow
            );
        }
    }
}

/// The deterministic fixture and the demo must stay off the priced path.
#[test]
fn fixture_templates_never_grant_search() {
    for name in SEARCH_DENIED_COMPANIES {
        let manifest = load_company(name);
        let grants = effective_grants(&manifest);
        assert!(
            !grants_search_explicit(&grants),
            "{name}: must not grant `search` — a priced network call here \
             makes the fixture non-hermetic (found {grants:?})"
        );
    }
}

/// The footgun this suite exists to catch: `[tools].allow` **replaces** the
/// default (`["*", "media", "composio"]`), it never extends it. A reviewer
/// "simplifying" a grant to `allow = ["search"]` would silently strip
/// files/docs/shell/code/web/subagent, `media` and `composio` from every agent
/// in the company — no parse error, no warning, just a company that quietly
/// lost its tool belt. This asserts both halves: the shipped form keeps the
/// inherited entries, and the reduced form provably loses them.
#[test]
fn granting_search_never_strips_the_inherited_default_belt() {
    let default_allow = Tools::default().allow;
    assert!(
        !grants_search_explicit(&default_allow),
        "the default belt is expected to stay search-free (opt-in per #238); \
         if that changed, these templates no longer need to restate it"
    );

    for name in FULL_BELT_PLUS_SEARCH {
        let manifest = load_company(name);
        for inherited in &default_allow {
            assert!(
                manifest.tools.allow.contains(inherited),
                "{name}: `[tools].allow` is {:?} and dropped the inherited \
                 default entry `{inherited}`. `allow` REPLACES the default \
                 ({default_allow:?}) — it must be restated verbatim, then \
                 extended.",
                manifest.tools.allow
            );
        }

        // Prove the loss is real rather than asserted: reduce the same
        // manifest to the "simplified" form and watch the belt vanish.
        let mut reduced = manifest.clone();
        reduced.tools.allow = vec!["search".to_string()];
        let grants = effective_grants(&reduced);
        assert!(
            !grants.iter().any(|grant| grant == "*"),
            "{name}: expected `allow = [\"search\"]` to strip the `*` belt"
        );
        assert!(
            !grants_media_explicit(&grants),
            "{name}: expected `allow = [\"search\"]` to strip `media`"
        );
        assert!(
            !grants_composio_explicit(&grants),
            "{name}: expected `allow = [\"search\"]` to strip `composio`"
        );
    }
}

#[test]
fn the_repo_skill_registry_parses() {
    let skills = load_dir_skills(&repo_root().join("skills"))
        .unwrap_or_else(|err| panic!("repo skills: {err}"));
    assert!(
        skills.iter().any(|skill| skill.slug == "web-research"),
        "expected the web-research skill in the shared registry"
    );
    for skill in &skills {
        assert!(!skill.name.is_empty(), "skill `{}` has no name", skill.slug);
        assert!(
            !skill.description.is_empty(),
            "skill `{}` has no description",
            skill.slug
        );
    }
}
