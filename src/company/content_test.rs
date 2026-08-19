//! Content-validation walk over the shipped `companies/*` and `skills/*`.
//!
//! These tests parse every data file the WS1 readers cover against the real
//! on-disk content, so any future content edit that breaks the frozen formats
//! fails CI. This guards WS8 authoring forever (see `docs/specs/09-verification.md`).

use std::path::{Path, PathBuf};

use super::workflow_file::WorkflowNodeKind;
use super::{
    CompanyManifest, Tools, grants_chargebee_explicit, grants_composio_explicit,
    grants_media_explicit, grants_paypal_explicit, grants_search_explicit, load_dir_skills,
    parse_workflow, walk_workspace,
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

/// Templates that must carry an explicit `search` grant (issues #312, #878).
///
/// The reason is the work the roster is described as doing, not anything on
/// disk under the company: the search-dependent skills (`web-research`,
/// `seo-audit`, `competitor-scan`) live in the *repo-level* `skills/` registry,
/// which is global and unscoped, so an operator can install any of them into
/// any company at runtime. No company ships a copy in its own `skills/` dir.
/// Whether a template belongs here is therefore a judgement about its charter —
/// research, editorial, marketing, legal, product engineering — recorded here
/// because it cannot be derived from content.
const SEARCH_GRANTED_COMPANIES: [&str; 8] = [
    "agentic_consultation_firm",
    "agentic_design_studio",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_media_company",
    "agentic_research_lab",
    "agentic_software_company",
    "signals_opportunity_studio",
];

/// Templates that must NEVER reach the metered search backend: `e2e_harness` is
/// a deterministic fixture (a priced network call would make it non-hermetic and
/// flaky), and `openhuman_demo` is a walkthrough nobody opted into spend for.
const SEARCH_DENIED_COMPANIES: [&str; 2] = ["e2e_harness", "openhuman_demo"];

/// Templates that simply do not grant `search` today. Unlike
/// [`SEARCH_DENIED_COMPANIES`] there is no rule keeping them off the priced
/// path — nobody has decided their roster needs the web. Moving one into
/// [`SEARCH_GRANTED_COMPANIES`] is an ordinary product call, not a violation.
///
/// This list exists so the posture is a *partition* rather than an allow-list.
/// An allow-list asserts a decision someone remembered, so it cannot notice a
/// company nobody remembered: `agentic_software_company` shipped with nine
/// agents and no search grant, and the suite stayed green for it (issue #878).
/// [`every_company_declares_a_search_posture`] asserts this list plus the other
/// two covers `companies/` exactly, so a new template fails CI until whoever
/// adds it writes the decision down here.
const SEARCH_UNGRANTED_COMPANIES: [&str; 12] = [
    "agentic_accounting_firm",
    "agentic_customer_support",
    "agentic_enterprise_sales",
    "agentic_game_business",
    "agentic_game_studio",
    "agentic_influencer_business",
    "agentic_pharma_startup",
    "agentic_realestate_company",
    "agentic_recruiting_company",
    "agentic_venture_capital",
    "agentic_venture_studio",
    "startup_accelerator",
];

/// The subset of [`SEARCH_GRANTED_COMPANIES`] that restates the default belt
/// verbatim and appends `search`. `signals_opportunity_studio` is deliberately
/// excluded: it overrides the default down to a research-only belt on purpose,
/// and `agentic_research_lab` is excluded for the same reason — its belt is
/// `["*", "search"]`, dropping `media` and `composio`, because a research lab
/// has no use for image generation or third-party OAuth side effects and both
/// are opt-in spend.
const FULL_BELT_PLUS_SEARCH: [&str; 6] = [
    "agentic_consultation_firm",
    "agentic_design_studio",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_media_company",
    "agentic_software_company",
];

fn load_company(name: &str) -> CompanyManifest {
    let dir = repo_root().join("companies").join(name);
    let mut manifest =
        CompanyManifest::from_path(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    // These tests are about what a bundle's author declared, so the global
    // baseline appended to every roster is dropped: a global teammate's tool
    // request is not this company's search posture, and holding a shipped
    // bundle responsible for one would make every company look like it granted
    // whatever the baseline asks for.
    manifest.agents.retain(|agent| !agent.global);
    manifest
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

/// Every shipped company must appear in exactly one of the three search-posture
/// lists, and the three together must cover `companies/` exactly (issue #878).
///
/// The guard #312 left behind was allow-list shaped: it checked that the six
/// companies someone listed do grant `search`, and said nothing about the
/// fifteen it did not list. `agentic_software_company` therefore shipped nine
/// agents whose `web_search` was never wired, with a green suite. An allow-list
/// can only ever assert a decision somebody remembered.
///
/// A partition inverts that. Adding a template to `companies/` fails this test
/// as unclassified until its author states the posture; deleting one fails it as
/// stale. And the classification cannot be made true by editing the list alone:
/// every `SEARCH_UNGRANTED_COMPANIES` entry is re-derived from its manifest
/// through the real `effective_grants` narrowing, so a company that actually
/// grants search cannot hide in the ungranted bucket.
#[test]
fn every_company_declares_a_search_posture() {
    use std::collections::BTreeSet;

    let buckets: [(&str, &[&str]); 3] = [
        ("SEARCH_GRANTED_COMPANIES", &SEARCH_GRANTED_COMPANIES),
        ("SEARCH_DENIED_COMPANIES", &SEARCH_DENIED_COMPANIES),
        ("SEARCH_UNGRANTED_COMPANIES", &SEARCH_UNGRANTED_COMPANIES),
    ];

    // (a) Each list is duplicate-free, and no company sits in two of them —
    // otherwise "exactly one posture" degrades to "at least one".
    for (name, list) in buckets {
        let unique: BTreeSet<&str> = list.iter().copied().collect();
        assert_eq!(
            unique.len(),
            list.len(),
            "{name} lists a company twice: {list:?}"
        );
    }
    for (index, (left_name, left)) in buckets.iter().enumerate() {
        for (right_name, right) in &buckets[index + 1..] {
            let left_set: BTreeSet<&str> = left.iter().copied().collect();
            let overlap: Vec<&str> = right
                .iter()
                .copied()
                .filter(|name| left_set.contains(name))
                .collect();
            assert!(
                overlap.is_empty(),
                "{left_name} and {right_name} both claim {overlap:?} — a \
                 company has exactly one search posture"
            );
        }
    }

    // (b) The union is exactly what is on disk.
    let declared: BTreeSet<&str> = buckets
        .iter()
        .flat_map(|(_, list)| list.iter().copied())
        .collect();
    let on_disk: BTreeSet<String> = subdirs(&repo_root().join("companies"))
        .iter()
        .map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_else(|| panic!("non-UTF-8 company dir {}", path.display()))
                .to_string()
        })
        .collect();
    assert!(!on_disk.is_empty(), "no companies found under companies/");

    let unclassified: Vec<&str> = on_disk
        .iter()
        .map(String::as_str)
        .filter(|name| !declared.contains(name))
        .collect();
    assert!(
        unclassified.is_empty(),
        "companies/{unclassified:?} declare no search posture. Every template \
         must be listed in exactly one of SEARCH_GRANTED_COMPANIES (its roster \
         needs the web), SEARCH_DENIED_COMPANIES (it must never reach the \
         priced backend) or SEARCH_UNGRANTED_COMPANIES (no decision to grant \
         it yet). Grants are not inherited from `*` — a company left out of the \
         granted list has `web_search` wired for none of its agents (#878)."
    );

    let stale: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|name| !on_disk.contains(*name))
        .collect();
    assert!(
        stale.is_empty(),
        "{stale:?} are listed in a search-posture const but no longer exist \
         under companies/ — delete the entries"
    );

    // (c) The ungranted bucket is verified against the manifests, not taken on
    // trust: a company that does grant search cannot be parked here.
    for name in SEARCH_UNGRANTED_COMPANIES {
        let manifest = load_company(name);
        let grants = effective_grants(&manifest);
        assert!(
            !grants_search_explicit(&grants),
            "{name}: sits in SEARCH_UNGRANTED_COMPANIES but its effective \
             grants ({grants:?}) do include `search`. If the grant is \
             intentional, move it to SEARCH_GRANTED_COMPANIES so \
             `research_templates_grant_search_at_both_layers` checks every \
             agent actually receives it."
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
fn a_wildcard_never_confers_a_billing_namespace() {
    // The point of these helpers: `*` is set for file and shell tools and must
    // not quietly hand out invoicing or a wallet balance.
    for grants in [
        vec!["*".to_string()],
        vec!["workspace".to_string(), "*".to_string()],
        vec![],
        vec!["chargebeeish".to_string(), "paypalish".to_string()],
        vec!["mcp:chargebee".to_string()],
    ] {
        assert!(!grants_chargebee_explicit(&grants), "{grants:?}");
        assert!(!grants_paypal_explicit(&grants), "{grants:?}");
    }
}

#[test]
fn a_billing_namespace_is_granted_bare_or_dotted_and_never_by_its_sibling() {
    assert!(grants_chargebee_explicit(&["chargebee".to_string()]));
    assert!(grants_chargebee_explicit(&["chargebee.read".to_string()]));
    assert!(grants_paypal_explicit(&["paypal".to_string()]));
    assert!(grants_paypal_explicit(&["paypal.wallet".to_string()]));
    // Two namespaces, neither implying the other.
    assert!(!grants_paypal_explicit(&["chargebee".to_string()]));
    assert!(!grants_chargebee_explicit(&["paypal".to_string()]));
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

/// Every bundled workflow must be *runnable*, not merely parseable (issue #530).
/// `every_workflow_graph_parses` only proves the TOML deserializes; it never
/// checks that a `tool_call` names a wired tool or that an `agent` names a real
/// teammate — which is exactly how the marketing agency preset shipped pointing
/// `research` at an unwired slug (halt) and `publish` at a nonexistent HTTP node.
///
/// This translates each graph the way the engine will, **compiles** it onto the
/// tinyflows engine (so a graph that parses but can't compile — an unbounded
/// guarded cycle, say — is caught at load, not first run; issue #661), and
/// asserts the two facts that decide whether a run halts: every `tool_call` slug
/// resolves to a real toolbelt namespace ([`namespace_of`]), and every `agent`
/// ref is on that company's roster.
///
/// Gated on `openhuman` because `translate` and `namespace_of` live behind that
/// feature; the `Rust (openhuman, tinycortex)` CI lane runs it.
#[cfg(feature = "openhuman")]
#[test]
fn every_bundled_workflow_is_runnable_against_its_roster() {
    use std::collections::BTreeSet;

    use tinyflows::model::NodeKind;

    use crate::harness::toolbelt::namespace_of;
    use crate::workflows::translate;

    for company in subdirs(&repo_root().join("companies")) {
        let manifest = CompanyManifest::from_path(&company)
            .unwrap_or_else(|err| panic!("{}: {err}", company.display()));
        let roster: BTreeSet<&str> = manifest.agents.iter().map(|a| a.id.as_str()).collect();

        for file in toml_files(&company.join("workflows")) {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
            let workflow =
                parse_workflow(&text).unwrap_or_else(|err| panic!("{}: {err}", file.display()));

            // `parse_workflow` is lenient on the #661 author-time rules (issue
            // #682) so pre-#661 tenant graphs still load. That leniency must NOT
            // apply to what WE ship: a seed a human could no longer author from
            // the console (a field-less condition, a branch not labeled yes/no, a
            // slug-less tool_call, an http_request missing method/url) has to fail
            // CI here. Run the STRICT pass over every shipped seed.
            let raw = super::raw_workflow_from_toml(&text)
                .unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            let strict_problems = super::workflow_file::validate(&raw, true);
            assert!(
                strict_problems.is_empty(),
                "{}: shipped seed fails strict author-time validation (issue #661/#682): {strict_problems:?}",
                file.display()
            );

            let graph = translate(&workflow);

            // Beyond parse+translate, every seed must COMPILE onto the tinyflows
            // engine (issue #661). Compile is the pass that rejects an unbounded
            // guarded cycle (`IllegalCycle`) and other structural faults a bare
            // parse misses — a seed that parses but cannot compile would fail at
            // first run, not at load, so the whole company's workflows break.
            tinyflows::compiler::compile(&graph).unwrap_or_else(|err| {
                panic!(
                    "{}: translated graph does not compile: {err}",
                    file.display()
                )
            });

            for node in &graph.nodes {
                match node.kind {
                    NodeKind::ToolCall => {
                        let slug = node
                            .config
                            .get("slug")
                            .and_then(|v| v.as_str())
                            .unwrap_or_else(|| {
                                panic!(
                                    "{} node `{}`: a tool_call with no slug",
                                    file.display(),
                                    node.id
                                )
                            });
                        assert!(
                            namespace_of(slug).is_some(),
                            "{} node `{}`: tool_call slug `{slug}` maps to no toolbelt namespace, so \
                             the run halts on it — every tool_call must name a wired tool (shell / \
                             code / web / search / …).",
                            file.display(),
                            node.id
                        );
                        // Beyond "is it wired", the company must GRANT the slug's
                        // namespace or the run is denied at the invoke gate. Use
                        // the same search-explicit / grants_cover split the
                        // run-time invoker and the author-time create path use.
                        let namespace = namespace_of(slug).expect("asserted present just above");
                        let granted = if namespace == "search" {
                            grants_search_explicit(&manifest.tools.allow)
                        } else {
                            crate::harness::build::grants_cover(&manifest.tools.allow, namespace)
                        };
                        assert!(
                            granted,
                            "{} node `{}`: tool_call slug `{slug}` (namespace `{namespace}`) is not \
                             granted by this company's [tools].allow ({:?}) — the run is denied at \
                             the invoke gate. Grant it in `[tools].allow`.",
                            file.display(),
                            node.id,
                            manifest.tools.allow
                        );
                    }
                    NodeKind::Agent => {
                        let agent_ref = node
                            .config
                            .get("agent_ref")
                            .and_then(|v| v.as_str())
                            .unwrap_or_else(|| {
                                panic!(
                                    "{} node `{}`: an agent node with no agent_ref",
                                    file.display(),
                                    node.id
                                )
                            });
                        assert!(
                            roster.contains(agent_ref),
                            "{} node `{}`: agent_ref `{agent_ref}` is not on the roster ({roster:?}) \
                             — the step would route to a teammate that does not exist.",
                            file.display(),
                            node.id
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The marketing agency's default desktop preset specifically — the three
/// defects issue #530 fixed, pinned so a future edit cannot silently reintroduce
/// them: `research` calls the metered `web_search` and continues past a search
/// failure rather than halting, and `publish` is the copywriter assembly step
/// (there is no CMS to POST to).
#[cfg(feature = "openhuman")]
#[test]
fn the_marketing_campaign_preset_is_runnable() {
    use crate::workflows::translate;

    let path =
        repo_root().join("companies/agentic_marketing_agency/workflows/campaign_pipeline.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let graph = translate(&parse_workflow(&text).expect("campaign parses"));
    let node = |id: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("no node `{id}`"))
    };

    let research = node("research");
    assert_eq!(research.config["slug"], "web_search");
    assert_eq!(research.config["on_error"], "continue");

    assert_eq!(node("publish").config["agent_ref"], "copywriter");
}

/// Every seeded `output` node either names a destination its own manifest can
/// resolve, or names none at all — and the ones that name none are exactly the
/// templates that declare no desk (issue #947).
///
/// Two failures this catches, and they are opposite:
///
///  1. **A destination that cannot resolve.** A `channel` target is a
///     [`ChannelAdapter`] id, and the adapters a company gets are one per desk in
///     its own manifest (`runtime::builder`, issue #837). A target naming a desk
///     that manifest does not declare parses fine, ships, and fails at run time
///     on a freshly provisioned tenant — which is the class of bug #947 is about,
///     one step further along.
///  2. **A desk-bearing template that quietly loses its destination.** The
///     allowlist below is the 18 templates that declare no desk and therefore
///     have nothing to address. It is an allowlist rather than a `!is_empty()`
///     check so that removing a destination from one of the three that *can*
///     deliver fails here instead of passing as "well, some have none".
///
/// The 18 are tracked in #963: they need desks before they can have
/// destinations, which is a content judgement per company rather than a stanza,
/// and `agentic_research_lab` declares no desk deliberately.
#[test]
fn every_seeded_output_destination_resolves_against_its_own_manifest() {
    /// Templates with an `output` node and no desk to deliver it to (#963).
    const NO_DESK_YET: &[&str] = &[
        "agentic_accounting_firm",
        "agentic_consultation_firm",
        "agentic_customer_support",
        "agentic_design_studio",
        "agentic_enterprise_sales",
        "agentic_game_business",
        "agentic_game_studio",
        "agentic_influencer_business",
        "agentic_law_firm",
        "agentic_media_company",
        "agentic_pharma_startup",
        "agentic_realestate_company",
        "agentic_recruiting_company",
        "agentic_research_lab",
        "agentic_venture_capital",
        "agentic_venture_studio",
        "signals_opportunity_studio",
        "startup_accelerator",
    ];

    let mut checked = 0;
    let mut with_destination = 0;
    for dir in subdirs(&repo_root().join("companies")) {
        let company = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        let manifest_path = dir.join("company.toml");
        if !manifest_path.exists() {
            continue;
        }
        let manifest: CompanyManifest =
            toml::from_str(&std::fs::read_to_string(&manifest_path).unwrap())
                .unwrap_or_else(|err| panic!("{company}/company.toml: {err}"));
        let desks: Vec<&str> = manifest
            .group_chats
            .iter()
            .map(|chat| chat.id.as_str())
            .collect();

        for path in toml_files(&dir.join("workflows")) {
            let text = std::fs::read_to_string(&path).unwrap();
            let file =
                parse_workflow(&text).unwrap_or_else(|err| panic!("{}: {err:?}", path.display()));
            let label = format!("{company}/{}", path.file_name().unwrap().to_string_lossy());

            for node in file
                .nodes
                .iter()
                .filter(|n| n.kind == WorkflowNodeKind::Output)
            {
                checked += 1;
                let Some(destination) = node.destination.as_ref() else {
                    assert!(
                        NO_DESK_YET.contains(&company.as_str()),
                        "{label} has an output node with no destination, so a run of it \
                         delivers nothing. Give it a destination, or — if this template \
                         declares no desk to deliver to — add it to `NO_DESK_YET` and to \
                         issue #963."
                    );
                    assert!(
                        desks.is_empty(),
                        "{label} is listed in `NO_DESK_YET` but its manifest declares \
                         desks {desks:?}, so it has somewhere to deliver. Give its output \
                         node a destination and drop it from the list."
                    );
                    continue;
                };
                with_destination += 1;
                assert_eq!(
                    destination.kind, "channel",
                    "{label} uses destination kind `{}`. `owner` reports \
                     Failed/OwnerFallbackFailed on a tenant with no mailbox — every freshly \
                     provisioned one — and `email` would hardcode a recipient into a \
                     shipped template.",
                    destination.kind
                );
                let target = destination.target.as_deref().unwrap_or("");
                assert!(
                    desks.contains(&target),
                    "{label} delivers to channel `{target}`, which is not a desk \
                     {company}'s manifest declares. A company's channel adapters are one \
                     per desk, so this resolves nowhere at run time. Declared: {desks:?}"
                );
            }
        }
    }

    assert_eq!(
        checked, 21,
        "the seeded output-node count changed; this test and #963's list describe 21"
    );
    assert_eq!(
        with_destination, 3,
        "exactly the three desk-bearing templates carry a destination today"
    );
}
