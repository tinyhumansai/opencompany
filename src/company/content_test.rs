//! Content-validation walk over the shipped `companies/*` and `skills/*`.
//!
//! These tests parse every data file the WS1 readers cover against the real
//! on-disk content, so any future content edit that breaks the frozen formats
//! fails CI. This guards WS8 authoring forever (see `docs/specs/09-verification.md`).

use std::path::{Path, PathBuf};

use super::workflow_file::WorkflowNodeKind;
use super::{
    CompanyManifest, Tools, grants_chargebee_explicit, grants_composio_explicit,
    grants_media_explicit, grants_paypal_explicit, grants_search_explicit,
    grants_workspace_write_explicit, load_dir_ledgers, load_dir_skills, parse_workflow,
    walk_workspace,
};
use crate::runtime::builder::{agent_scoped_grants, effective_grants};

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
const SEARCH_GRANTED_COMPANIES: [&str; 21] = [
    "agentic_accounting_firm",
    "agentic_consultation_firm",
    "agentic_customer_support",
    "agentic_design_studio",
    "agentic_enterprise_sales",
    "agentic_game_business",
    "agentic_game_studio",
    "agentic_influencer_business",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_media_company",
    "agentic_pharma_startup",
    "agentic_product_team",
    "agentic_realestate_company",
    "agentic_recruiting_company",
    "agentic_research_lab",
    "agentic_software_company",
    "agentic_venture_capital",
    "agentic_venture_studio",
    "signals_opportunity_studio",
    "startup_accelerator",
];

/// Templates that must NEVER reach the metered search backend: `e2e_harness` and
/// `e2e_setup` are deterministic fixtures (a priced network call would make them
/// non-hermetic and flaky), `openhuman_demo` is a walkthrough nobody opted
/// into spend for, and `agentic_math_lab` is denied for a reason of its own —
/// its whole claim is that it *computes* an exact answer, and a lab that can
/// search can look one up. A run that looked the answer up passes the lab's
/// end-to-end spec while proving nothing about whether the roster can solve
/// anything, so withholding the network is what makes the number evidence.
const SEARCH_DENIED_COMPANIES: [&str; 4] = [
    "agentic_math_lab",
    "e2e_harness",
    "e2e_setup",
    "openhuman_demo",
];

/// Templates that simply do not grant `search` today. Unlike
/// [`SEARCH_DENIED_COMPANIES`] there is no rule keeping them off the priced
/// path — nobody has decided their roster needs the web. Moving one into
/// [`SEARCH_GRANTED_COMPANIES`] is an ordinary product call, not a violation.
///
/// **Empty, and kept anyway.** `search` is in the global `default_allow` now,
/// so a company that declares no `[tools]` section inherits it: the twelve
/// templates that used to sit here were never *deciding* against search, they
/// had simply never been edited, and their agents reported the tool as not
/// enabled. They moved to the granted list unchanged. The bucket stays because
/// the partition is the mechanism — the next template that genuinely wants to
/// leave search off, without the hermetic-fixture argument that puts a company
/// in [`SEARCH_DENIED_COMPANIES`], is declared here.
///
/// This list exists so the posture is a *partition* rather than an allow-list.
/// An allow-list asserts a decision someone remembered, so it cannot notice a
/// company nobody remembered: `agentic_software_company` shipped with nine
/// agents and no search grant, and the suite stayed green for it (issue #878).
/// [`every_company_declares_a_search_posture`] asserts this list plus the other
/// two covers `companies/` exactly, so a new template fails CI until whoever
/// adds it writes the decision down here.
const SEARCH_UNGRANTED_COMPANIES: [&str; 0] = [];

/// The subset of [`SEARCH_GRANTED_COMPANIES`] that restates the default belt
/// verbatim and appends `search`. `signals_opportunity_studio` is deliberately
/// excluded: it overrides the default down to a research-only belt on purpose,
/// and `agentic_research_lab` is excluded for the same reason — its belt is
/// `["*", "search"]`, dropping `media` and `composio`, because a research lab
/// has no use for image generation or third-party OAuth side effects and both
/// are opt-in spend. `agentic_product_team` is excluded on that same
/// research-lab argument: it produces documents and ledger rows, so it drops
/// both opt-in namespaces too.
const FULL_BELT_PLUS_SEARCH: [&str; 8] = [
    // Both restate the belt verbatim and append `chargebee` (#788) rather than
    // `search`, which is already inherited. They belong here for the property
    // this list actually guards — that an extended `allow` did not silently
    // drop an inherited entry — which is independent of *which* namespace the
    // template extended it with.
    "agentic_accounting_firm",
    "agentic_consultation_firm",
    "agentic_design_studio",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_media_company",
    "agentic_software_company",
    "agentic_venture_studio",
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
/// default (`globals/globals.toml`'s `default_allow`), it never extends it. A
/// reviewer "simplifying" a grant to `allow = ["search"]` would silently strip
/// files/docs/shell/code/web/subagent, workspace writes, `media`, `composio`
/// and the MCP grants from every agent in the company — no parse error, no
/// warning, just a company that quietly lost its tool belt. This asserts both
/// halves: the shipped form keeps the inherited entries, and the reduced form
/// provably loses them.
///
/// It used to open by asserting the default belt was search-free, which is no
/// longer true — `search` ships in `default_allow`, so these templates now
/// restate the default rather than restating-and-extending it. The invariant
/// that mattered survives the change untouched: whatever the default carries,
/// a template that writes its own `allow` must carry all of it.
#[test]
fn granting_search_never_strips_the_inherited_default_belt() {
    let default_allow = Tools::default().allow;
    assert!(
        grants_search_explicit(&default_allow),
        "`search` is expected to ship in the default belt now; if it was made \
         opt-in again, these templates have to restate-and-extend once more \
         and this test's premise needs rewriting rather than deleting"
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

/// Issue #788 follow-up, raised in review of the template ceilings: narrowing
/// the *manifest* teammates does not protect a teammate an operator adds at
/// runtime. `POST …/team` with no `tools` and no `focus` stores an empty grant,
/// and empty means "the standard company-wide grant" — which, on a company that
/// carries `chargebee`, silently included billing.
#[test]
fn a_teammate_created_with_no_grant_never_inherits_billing() {
    use super::{CreationGrant, creation_default_grants};

    let narrowed = |allow: &[String]| match creation_default_grants(allow) {
        CreationGrant::Narrowed(list) => list,
        other => panic!("expected a narrowed line for {allow:?}, got {other:?}"),
    };

    // The overwhelming majority: nothing withheld, so the inherit-everything
    // contract is untouched and the stored teammate stays empty.
    let plain = Tools::default().allow;
    assert_eq!(
        creation_default_grants(&plain),
        CreationGrant::Standard,
        "a company granting no BYO money namespace must keep `empty = standard`"
    );

    // The degenerate belt: filtering removes everything, and an empty line
    // would read back as "inherit the whole company grant" — handing over the
    // exact namespace the filter just removed. It must be refusable instead.
    for only_money in [
        vec!["chargebee"],
        vec!["paypal"],
        vec!["chargebee", "paypal", "hosting"],
    ] {
        let allow: Vec<String> = only_money.iter().map(|g| (*g).to_string()).collect();
        assert_eq!(
            creation_default_grants(&allow),
            CreationGrant::NothingLeft,
            "an all-withheld belt must not decode as inheritance: {allow:?}"
        );
    }

    // A company that named `chargebee` for one teammate does not hand it to the
    // next one somebody types into the console.
    let mut billing = plain.clone();
    billing.push("chargebee".to_string());
    let defaulted = narrowed(&billing);
    assert!(
        !grants_chargebee_explicit(&defaulted),
        "a new teammate must not inherit `chargebee`: {defaulted:?}"
    );
    // ...and loses nothing else on the way.
    for inherited in &plain {
        assert!(
            defaulted.contains(inherited),
            "withholding billing dropped the inherited `{inherited}`: {defaulted:?}"
        );
    }

    // The same for the other two namespaces `*` refuses to confer.
    for money in ["paypal", "hosting"] {
        let mut allow = plain.clone();
        allow.push(money.to_string());
        let defaulted = narrowed(&allow);
        assert!(
            !defaulted.iter().any(|g| g == money),
            "a new teammate must not inherit `{money}`: {defaulted:?}"
        );
    }

    // `media`/`composio`/`search` ship in the default belt (#1674) and are NOT
    // withheld — doing so would re-create that issue's complaint for every new
    // teammate.
    let defaulted = narrowed(&billing);
    assert!(grants_media_explicit(&defaulted), "{defaulted:?}");
    assert!(grants_composio_explicit(&defaulted), "{defaulted:?}");
    assert!(grants_search_explicit(&defaulted), "{defaulted:?}");
}

/// The second half of the same hole: a teammate MINTED by an orchestrator whose
/// own scope comes from its desk rather than its `tools` line. The minter copies
/// its (empty) line, the new teammate is on no desk, and an empty line reads
/// back as the whole company grant — so it would hold billing its own minter
/// does not. Pinned as a data property of the shipped templates: no marketing
/// teammate may be in a position to mint a biller by accident.
#[test]
fn a_deskless_teammate_minted_with_no_scope_never_inherits_billing() {
    use super::{CreationGrant, creation_default_grants};

    for company in [
        "agentic_marketing_agency",
        "agentic_accounting_firm",
        "agentic_venture_studio",
        "agentic_software_company",
    ] {
        let manifest = load_company(company);
        // The state that makes the escalation reachable: a minter scoped only by
        // its desk has an absent (`None`) `tools` line, so "copy the minter's
        // line" stores an inherit grant on a teammate that sits on no desk.
        let deskless = agent_scoped_grants(&manifest.tools.allow, &[], None);
        assert!(
            grants_chargebee_explicit(&deskless),
            "{company}: precondition — an absent line on no desk must resolve to \
             the company ceiling, or this test proves nothing"
        );

        // What both creation paths now store instead.
        match creation_default_grants(&manifest.tools.allow) {
            CreationGrant::Narrowed(narrowed) => {
                let resolved = agent_scoped_grants(&manifest.tools.allow, &[], Some(&narrowed));
                assert!(
                    !grants_chargebee_explicit(&resolved),
                    "{company}: a minted teammate must not inherit billing: {resolved:?}"
                );
            }
            other => panic!("{company}: expected a narrowed creation grant, got {other:?}"),
        }
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
/// feature; the `Rust (openhuman, tinymemory)` CI lane runs it.
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

/// The marketing agency's creative desk ceiling must not strip the company's
/// workspace-write grant.
///
/// The desk states `["*", "workspace.write"]`, and the `*` half deliberately
/// confers no workspace writes — [`grants_workspace_write_explicit`] matches
/// only the bare `workspace` or the exact `workspace.write` token — so the
/// write token has to be restated in the ceiling for the desk's agents to hold
/// it. Their own AGENTS.md promises the `agents/<id>/` folder is always
/// writable, and `agent_scoped_grants` would silently strip that promise with
/// a `["*"]`-only ceiling. Pinned through the *three-level* narrowing (the
/// `effective_grants` the search-posture tests use ignores desks, which is
/// precisely how this gap shipped) so a future edit cannot quietly reintroduce
/// the stripping.
#[test]
fn a_restricting_desk_does_not_strip_the_workspace_write_token() {
    let manifest = load_company("agentic_marketing_agency");
    let creative = manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == "creative")
        .expect("the marketing agency declares the creative desk");
    assert!(
        !creative.tools.is_empty(),
        "the creative desk must state a ceiling or this test asserts nothing"
    );

    for id in ["creative_director", "copywriter", "landing_page_builder"] {
        let agent = manifest
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .unwrap_or_else(|| panic!("{id} is a member of the creative desk"));
        let desk_refs: Vec<&[String]> = manifest
            .group_chats
            .iter()
            .filter(|chat| chat.members.iter().any(|member| member == id))
            .map(|chat| chat.tools.as_slice())
            .collect();
        let grants = agent_scoped_grants(&manifest.tools.allow, &desk_refs, agent.tools.as_deref());

        assert!(
            grants_workspace_write_explicit(&grants),
            "{id}: the creative desk ceiling ({:?}) must keep the company's \
             workspace write grant; effective grants: {grants:?}",
            creative.tools
        );
        // The desk still deliberately withholds the billed / third-party
        // opt-ins the company grants at the top level.
        assert!(
            !grants_search_explicit(&grants),
            "{id}: the creative desk must stay searchless; effective grants: {grants:?}"
        );
        assert!(
            !grants_media_explicit(&grants),
            "{id}: the creative desk must stay media-less; effective grants: {grants:?}"
        );
        assert!(
            !grants_composio_explicit(&grants),
            "{id}: the creative desk must stay composio-less; effective grants: {grants:?}"
        );
    }
}

/// The marketing agency's `chargebee` exclusion lives at the per-agent layer,
/// not on the strategy/growth desk ceilings, so the console flow the manifest
/// documents — naming a biller from the console — actually works.
///
/// A desk ceiling is manifest-only and cannot be widened from the console, so
/// an exclusion stated there would make the billing grant unreachable by any
/// shipped teammate. Pinned through the real three-level narrowing
/// (`agent_scoped_grants`): a shipped member's own `tools` line excludes
/// `chargebee`, while the desk level (no ceiling) admits an operator override
/// that names it.
#[test]
fn a_marketing_biller_can_be_named_from_the_console() {
    let manifest = load_company("agentic_marketing_agency");

    // The desks this PR touched state no ceiling — the exclusion must not live
    // on an unwidenable layer.
    for id in ["strategy", "growth"] {
        let desk = manifest
            .group_chats
            .iter()
            .find(|chat| chat.id == id)
            .unwrap_or_else(|| panic!("{id} desk"));
        assert!(
            desk.tools.is_empty(),
            "{id}: the `chargebee` exclusion must not live on the desk ceiling \
             (an unwidenable layer); found {:?}",
            desk.tools
        );
    }

    // Every shipped strategy/growth member holds the belt minus `chargebee`.
    for id in [
        "brand_strategist",
        "seo_specialist",
        "analytics_analyst",
        "paid_ads_manager",
        "email_marketer",
    ] {
        let agent = manifest
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .unwrap_or_else(|| panic!("{id} is a marketing teammate"));
        let desk_refs: Vec<&[String]> = manifest
            .group_chats
            .iter()
            .filter(|chat| chat.members.iter().any(|member| member == id))
            .map(|chat| chat.tools.as_slice())
            .collect();
        let grants = agent_scoped_grants(&manifest.tools.allow, &desk_refs, agent.tools.as_deref());
        assert!(
            !grants_chargebee_explicit(&grants),
            "{id}: a shipped marketing teammate must not hold billing tools; \
             effective grants: {grants:?}"
        );
    }

    // An operator naming the biller from the console — adding `chargebee` to a
    // strategy member's override — now survives the desk level.
    let brand = manifest
        .agents
        .iter()
        .find(|agent| agent.id == "brand_strategist")
        .unwrap();
    let mut override_tools = brand.tools.clone().unwrap_or_default();
    override_tools.push("chargebee".to_string());
    let strategy = manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == "strategy")
        .unwrap();
    let desk_refs: Vec<&[String]> = vec![strategy.tools.as_slice()];
    let grants = agent_scoped_grants(&manifest.tools.allow, &desk_refs, Some(&override_tools));
    assert!(
        grants_chargebee_explicit(&grants),
        "the console override naming the biller must survive the desk layer; \
         effective grants: {grants:?}"
    );
}

/// The software company ships the billing ceiling and nobody holding it.
///
/// The template had no `chargebee` at all until #1854, which made the
/// capability unreachable rather than withheld: `[tools].allow` has no runtime
/// write path, so a company booted from this bundle could store a Chargebee key
/// and have it reach no teammate, with nothing in the console able to fix it.
///
/// Granting it needed every agent to state a belt first. All nine omitted their
/// `tools` line, and an omitted line inherits the WHOLE company grant — so the
/// one-line version of this change would have handed billing to the QA engineer
/// and the docs writer rather than to nobody. This pins both halves: the
/// ceiling exists, and no shipped teammate resolves to holding it.
#[test]
fn the_software_company_ships_billing_that_reaches_nobody_yet() {
    let manifest = load_company("agentic_software_company");

    assert!(
        grants_chargebee_explicit(&manifest.tools.allow),
        "the ceiling must exist, or an operator has no way to name a biller: {:?}",
        manifest.tools.allow
    );

    // The exclusion must not live on a desk: desk ceilings are manifest-only,
    // so an exclusion there could never be widened from the console — which
    // would make the ceiling above decorative.
    for chat in &manifest.group_chats {
        assert!(
            chat.tools.is_empty(),
            "{}: the `chargebee` exclusion must not live on the desk ceiling              (an unwidenable layer); found {:?}",
            chat.id,
            chat.tools
        );
    }

    for agent in &manifest.agents {
        let desk_refs: Vec<&[String]> = manifest
            .group_chats
            .iter()
            .filter(|chat| chat.members.contains(&agent.id))
            .map(|chat| chat.tools.as_slice())
            .collect();
        let grants = agent_scoped_grants(&manifest.tools.allow, &desk_refs, agent.tools.as_deref());
        assert!(
            !grants_chargebee_explicit(&grants),
            "{}: a shipped teammate must not hold billing tools; effective              grants: {grants:?}",
            agent.id
        );
    }

    // …and naming one from the console reaches the tools, which is the whole
    // point of the ceiling being there.
    let support = manifest
        .agents
        .iter()
        .find(|agent| agent.id == "customer_support")
        .expect("customer_support is on this roster");
    let mut named = support.tools.clone().unwrap_or_default();
    named.push("chargebee".to_string());
    let desk_refs: Vec<&[String]> = manifest
        .group_chats
        .iter()
        .filter(|chat| {
            chat.members
                .iter()
                .any(|member| member == "customer_support")
        })
        .map(|chat| chat.tools.as_slice())
        .collect();
    let grants = agent_scoped_grants(&manifest.tools.allow, &desk_refs, Some(&named));
    assert!(
        grants_chargebee_explicit(&grants),
        "an operator naming the biller from the console must reach billing; \
         effective grants: {grants:?}"
    );
}

/// A creative member cross-seated onto an unrestricted desk must not widen to
/// the company grant.
///
/// Desks combine by **union**, and a member scoped only by the creative desk
/// ceiling would resolve to the full company grant — billing included — the
/// moment an operator seats them on the strategy or growth desk, which state
/// no ceiling. The `chargebee` exclusion must therefore ride on the member's
/// own `tools` line (the company belt minus `chargebee`), not on the desk
/// alone. Pinned through the same three-level narrowing the roster build uses.
#[test]
fn a_creative_member_cross_seated_on_an_unrestricted_desk_stays_billing_less() {
    let manifest = load_company("agentic_marketing_agency");
    let strategy = manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == "strategy")
        .unwrap();
    assert!(
        strategy.tools.is_empty(),
        "precondition: the strategy desk must be unrestricted or this test \
         proves nothing"
    );
    let creative = manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == "creative")
        .unwrap();

    for id in ["creative_director", "copywriter", "landing_page_builder"] {
        let agent = manifest
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .unwrap_or_else(|| panic!("{id} is a member of the creative desk"));
        assert!(
            agent.tools.as_deref().is_some_and(|t| !t.is_empty()),
            "{id}: the `chargebee` exclusion must ride on the member's own \
             `tools` line, not only on the creative desk ceiling"
        );

        // Seated on the creative desk AND the unrestricted strategy desk: the
        // union would otherwise be the company grant.
        let desk_refs: Vec<&[String]> = vec![creative.tools.as_slice(), strategy.tools.as_slice()];
        let grants = agent_scoped_grants(&manifest.tools.allow, &desk_refs, agent.tools.as_deref());
        assert!(
            !grants_chargebee_explicit(&grants),
            "{id}: cross-seating a creative member onto an unrestricted desk \
             must not hand back billing; effective grants: {grants:?}"
        );
    }
}

/// Every seeded `output` node names a destination its own manifest can resolve,
/// except the research lab, which deliberately proves that workflows can
/// coordinate without desks (issue #963).
///
/// Two failures this catches, and they are opposite:
///
///  1. **A destination that cannot resolve.** A `channel` target is a
///     [`ChannelAdapter`] id, and the adapters a company gets are one per desk in
///     its own manifest (`runtime::builder`, issue #837). A target naming a desk
///     that manifest does not declare parses fine, ships, and fails at run time
///     on a freshly provisioned tenant — which is the class of bug #947 is about,
///     one step further along.
///  2. **A template that quietly loses its destination.** All shipped
///     templates except the research lab now declare a desk for their terminal
///     output. The single exception is named below so removing any other
///     destination fails rather than passing as "well, some have none".
///
/// `agentic_research_lab` explains in its own manifest why it has no desk: its
/// workflow is the proving ground for collapsing desk coordination into the
/// graph itself.
#[test]
fn every_seeded_output_destination_resolves_against_its_own_manifest() {
    const DESKLESS_WORKFLOW_TEMPLATE: &str = "agentic_research_lab";

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
                        company == DESKLESS_WORKFLOW_TEMPLATE,
                        "{label} has an output node with no destination, so a run of it \
                         delivers nothing. Give it a channel destination backed by a \
                         manifest desk. The research lab is the only intentional exception."
                    );
                    assert!(
                        desks.is_empty(),
                        "{label} is the deskless-workflow exception but its manifest \
                         declares desks {desks:?}. Give its output node a destination."
                    );
                    continue;
                };
                with_destination += 1;
                assert_eq!(
                    destination.kind, "channel",
                    "{label} uses destination kind `{}`. A seeded template routes to a real \
                     desk channel: `owner` on a no-mailbox tenant now lands on the operator \
                     channel (issue #1757) rather than the desk a template means to post in, \
                     and `email` would hardcode a recipient into a shipped template.",
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

    // The relation, not a hand-maintained total. #963's count was a literal (22
    // by the time `e2e_harness/long_pipeline.toml` landed), which meant every
    // added workflow failed this test on arithmetic rather than on anything
    // about destinations — and the fix was always to bump the number, which is
    // a guard nobody reads. What the count was actually protecting is stated
    // directly instead: **exactly one** seeded output node in the whole
    // repository has no destination, and it is the research lab's.
    assert!(
        checked > 0,
        "no seeded output nodes were checked at all — the walk found nothing"
    );
    assert_eq!(
        with_destination,
        checked - 1,
        "every seeded output except the research lab's deliberate deskless workflow \
         carries a destination"
    );
}

/// Every shipped bundle's ledger declarations must parse, and the set a company
/// ends up with — the global baseline plus its own — must fit under the cap.
///
/// A declaration that does not parse is not a boot failure (the builder warns
/// and carries on, because a hand-edited bundle should still reach its console),
/// which is exactly why it has to fail *here*: a shipped template whose defining
/// axis silently never appears is the failure this whole surface exists to
/// prevent, and nothing at run time would say so.
#[test]
fn every_company_ledger_declaration_parses_and_fits_under_the_cap() {
    for company in subdirs(&repo_root().join("companies")) {
        let declared = load_dir_ledgers(&company)
            .unwrap_or_else(|err| panic!("{}/ledgers: {err}", company.display()));

        let mut slugs: Vec<String> = crate::globals::ledgers()
            .iter()
            .map(|spec| spec.slug.clone())
            .collect();
        for spec in &declared {
            // A company declaration of a baseline slug replaces it rather than
            // stacking with it — the precedence `seed_ledgers` applies.
            slugs.retain(|slug| slug != &spec.slug);
            slugs.push(spec.slug.clone());
        }
        assert!(
            slugs.len() <= crate::ledger::MAX_DECLARED,
            "{} ends up with {} ledgers, past the {} cap: {slugs:?}",
            company.display(),
            slugs.len(),
            crate::ledger::MAX_DECLARED
        );
    }
}

/// No shipped template declares a ledger with more than five statuses.
///
/// The same argument the built-ins were narrowed to three by (issue #1512),
/// applied to authored content and stopped one notch looser. A template ledger
/// is a *pipeline* far more often than a built-in is — a candidate, a deal, a
/// filing genuinely moves through stages — so five leaves room for two or three
/// real stages plus the outcomes, where three would have forced every template
/// to throw away either its pipeline or its outcome.
///
/// What five does forbid is the sprawl these started at: seven statuses, four of
/// which an agent had to choose between on every write with nothing to tell them
/// apart but a blurb. Past five, the extra status is reliably answering a second
/// question — how is it going, which flavour of over — and that answer belongs
/// in a field (`progress`, `reason`) where it does not have to be guessed.
///
/// Covers the `globals/` baseline as well as `companies/`: the baseline ships
/// into every company, so a sprawling one there is sprawl nobody opted into.
///
/// It fails here rather than at run time because nothing at run time would say
/// so: a ledger with nine statuses loads, renders and works, and only the
/// company using it discovers that its agents cannot keep the vocabulary
/// straight.
#[test]
fn no_shipped_template_ledger_declares_more_than_five_statuses() {
    /// Enough for a short pipeline and its outcomes; not enough for a taxonomy.
    const MAX_STATUSES: usize = 5;

    let mut checked = 0;
    let mut check = |origin: String, spec: &crate::ledger::LedgerSpec| {
        checked += 1;
        let names: Vec<&str> = spec.statuses.iter().map(|s| s.name.as_str()).collect();
        assert!(
            spec.statuses.len() <= MAX_STATUSES,
            "{origin}/{} declares {} statuses, past the {MAX_STATUSES} ceiling: {names:?}. \
             Merge the ones that answer a question other than *where does this row stand* \
             and keep the retired words as `aliases` so stored rows still render.",
            spec.slug,
            spec.statuses.len(),
        );
    };

    // The baseline first, and it matters more than any single template: these
    // ship into *every* company, so a sprawling one is sprawl every operator
    // gets whichever vertical they started from.
    for spec in crate::globals::ledgers() {
        check("globals/ledgers".to_string(), spec);
    }
    for company in subdirs(&repo_root().join("companies")) {
        let declared = load_dir_ledgers(&company)
            .unwrap_or_else(|err| panic!("{}/ledgers: {err}", company.display()));
        for spec in &declared {
            check(format!("{}/ledgers", company.display()), spec);
        }
    }
    // A walk that found nothing would pass this silently, which is the one way
    // a content test can be green and worthless.
    assert!(checked > 0, "no template ledgers were checked");
}

/// Every `[[agent]].ledgers` grant must name a ledger that company actually has.
///
/// A grant is a *narrowing*: an agent that declares one can see exactly the
/// slugs it lists and nothing else. So a typo does not fail, it silently hides
/// a ledger from the teammate that was meant to have it — and an agent granted
/// only `{ name = "pipelin" }` is an agent with no ledger access at all, with
/// nothing anywhere saying so. The slug cannot be checked at manifest-load time
/// (a company-declared ledger may not exist yet, by design), so the shipped
/// templates are checked here, where every one of their declarations is on disk.
#[test]
fn every_ledger_grant_on_a_shipped_template_names_a_ledger_that_company_has() {
    let (builtins, _) = crate::ledger::builtins();
    for company in subdirs(&repo_root().join("companies")) {
        let manifest = CompanyManifest::from_path(&company)
            .unwrap_or_else(|err| panic!("{}: {err}", company.display()));
        let declared = load_dir_ledgers(&company)
            .unwrap_or_else(|err| panic!("{}/ledgers: {err}", company.display()));

        let known: Vec<&str> = builtins
            .iter()
            .map(|spec| spec.slug.as_str())
            .chain(
                crate::globals::ledgers()
                    .iter()
                    .map(|spec| spec.slug.as_str()),
            )
            .chain(declared.iter().map(|spec| spec.slug.as_str()))
            .collect();

        for agent in &manifest.agents {
            let Some(grants) = &agent.ledgers else {
                continue;
            };
            for grant in grants {
                assert!(
                    known
                        .iter()
                        .any(|slug| slug.eq_ignore_ascii_case(&grant.name)),
                    "{}: agent `{}` is granted `{}`, which is not a ledger this company has — \
                     the real ones are {known:?}",
                    company.display(),
                    agent.id,
                    grant.name
                );
            }
        }
    }
}

/// A bundle ledger must close, and closing must demand a reason — the same bar
/// the baseline is held to in `globals::test`.
///
/// A vertical's own axis is the one most likely to be authored as a list that
/// only grows: a matter list with no `closed` status renders every matter the
/// firm ever opened, forever, and the cap then hides the live ones behind the
/// dead ones.
#[test]
fn every_company_ledger_can_be_closed_and_says_why() {
    for company in subdirs(&repo_root().join("companies")) {
        for spec in load_dir_ledgers(&company).expect("declarations parse") {
            let closing = spec.closing_statuses();
            assert!(
                !closing.is_empty(),
                "{}: `{}` declares no closing status, so nothing on it can ever be finished",
                company.display(),
                spec.slug
            );
            for name in closing {
                assert!(
                    spec.status(name).expect("a declared status").needs_reason,
                    "{}: `{}` closes into `{name}` without demanding a reason",
                    company.display(),
                    spec.slug
                );
            }
        }
    }
}

/// The bundles that ship a vertical's own setup cards and tool servers.
///
/// Together with [`FIXTURE_COMPANIES`] this is a **partition** of `companies/`,
/// asserted by [`every_company_declares_a_setup_posture`]. A partition rather
/// than an allow-list for the reason [`every_company_declares_a_search_posture`]
/// gives: an allow-list is satisfied by a new template nobody classified, and
/// "the board is empty because this vertical has no setup work" and "the board
/// is empty because whoever added this bundle forgot" are indistinguishable
/// afterwards.
const SETUP_SEEDED_COMPANIES: [&str; 22] = [
    "agentic_accounting_firm",
    "agentic_consultation_firm",
    "agentic_customer_support",
    "agentic_design_studio",
    "agentic_enterprise_sales",
    "agentic_game_business",
    "agentic_game_studio",
    "agentic_influencer_business",
    "agentic_law_firm",
    "agentic_marketing_agency",
    "agentic_math_lab",
    "agentic_media_company",
    "agentic_pharma_startup",
    "agentic_product_team",
    "agentic_realestate_company",
    "agentic_recruiting_company",
    "agentic_research_lab",
    "agentic_software_company",
    "agentic_venture_capital",
    "agentic_venture_studio",
    "signals_opportunity_studio",
    "startup_accelerator",
];

/// The bundles that deliberately ship neither, because they are fixtures.
///
/// A fixture proves a mechanism and is asserted against exactly — `e2e_harness`
/// and `openhuman_demo` also declare their own `[[mcp_server]]` inline — so
/// seeded cards and a second declaration of `deepwiki` would both perturb what
/// they exist to pin down.
const FIXTURE_COMPANIES: [&str; 3] = ["e2e_harness", "e2e_setup", "openhuman_demo"];

/// Every company is either a vertical that ships setup content or a fixture that
/// deliberately does not — and the classification is re-derived from the files
/// on disk, so it cannot be made true by editing the lists alone.
#[test]
fn every_company_declares_a_setup_posture() {
    use std::collections::BTreeSet;

    let seeded: BTreeSet<&str> = SETUP_SEEDED_COMPANIES.iter().copied().collect();
    let fixtures: BTreeSet<&str> = FIXTURE_COMPANIES.iter().copied().collect();
    assert_eq!(
        seeded.len(),
        SETUP_SEEDED_COMPANIES.len(),
        "SETUP_SEEDED_COMPANIES lists a company twice"
    );
    assert_eq!(
        fixtures.len(),
        FIXTURE_COMPANIES.len(),
        "FIXTURE_COMPANIES lists a company twice"
    );
    let overlap: Vec<&&str> = seeded.intersection(&fixtures).collect();
    assert!(
        overlap.is_empty(),
        "a company cannot be both a vertical and a fixture: {overlap:?}"
    );

    let on_disk: BTreeSet<String> = subdirs(&repo_root().join("companies"))
        .iter()
        .filter_map(|dir| dir.file_name()?.to_str().map(str::to_string))
        .collect();
    let classified: BTreeSet<String> = seeded
        .union(&fixtures)
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(
        on_disk, classified,
        "every company under `companies/` must be classified as a vertical or a fixture — \
         add it to SETUP_SEEDED_COMPANIES or FIXTURE_COMPANIES"
    );

    // The classification has to match the files, not merely the lists.
    for name in &seeded {
        let dir = repo_root().join("companies").join(name);
        assert!(
            super::has_mcp_file(&dir),
            "{name} is listed as a vertical but ships no `mcp.json`"
        );
        assert!(
            super::has_task_file(&dir),
            "{name} is listed as a vertical but ships no `tasks.toml`"
        );
    }
    for name in &fixtures {
        let dir = repo_root().join("companies").join(name);
        assert!(
            !super::has_task_file(&dir),
            "{name} is a fixture and must not seed cards onto its board"
        );
    }
}

/// Every shipped `mcp.json` parses cleanly, and every server it declares is safe
/// to hand an agent unattended: HTTP, credential-free, and either answering or
/// deliberately off pending a token.
///
/// `every_company_manifest_is_valid` already runs each file through the real
/// merge, so a malformed one fails there. This adds the rules that are about
/// *shipping* a server to everyone who runs the bundle rather than about the
/// declaration being well-formed.
#[test]
fn every_shipped_mcp_server_is_safe_to_ship() {
    for company in subdirs(&repo_root().join("companies")) {
        let name = company.file_name().unwrap().to_str().unwrap().to_string();
        if !super::has_mcp_file(&company) {
            continue;
        }
        let (servers, problems) = super::load_dir_mcp_servers(&company);
        assert!(problems.is_empty(), "{name}/mcp.json: {problems:?}");
        assert!(
            !servers.is_empty(),
            "{name} ships an `mcp.json` that declares nothing"
        );

        let readme = std::fs::read_to_string(company.join("README.md"))
            .unwrap_or_else(|err| panic!("{name}/README.md: {err}"));

        for server in &servers {
            assert!(
                server.endpoint.starts_with("https://"),
                "{name}/mcp.json: `{}` must be https — a shipped template must not send an \
                 agent's traffic in the clear",
                server.name
            );
            assert!(
                server
                    .description
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty()),
                "{name}/mcp.json: `{}` has no `description` — JSON carries no comments, so the \
                 description is the only place this choice can be explained",
                server.name
            );
            // A server that needs a credential must ship off. Enabled plus a
            // credential means it fails at an agent's first tool call, on every
            // install, until somebody notices why.
            if server.auth_secret.is_some() {
                assert!(
                    !server.enabled,
                    "{name}/mcp.json: `{}` names an `authSecret` and ships enabled — it would \
                     fail at the first tool call; ship it disabled",
                    server.name
                );
            }
            assert!(
                readme.contains(&format!("`{}`", server.name)),
                "{name}/README.md does not mention `{}` — an undocumented server is one nobody \
                 can decide whether to enable",
                server.name
            );
        }
    }
}

/// Every shipped `tasks.toml` parses, and every card on it is one an agent can
/// actually pick up.
#[test]
fn every_shipped_setup_card_is_pickable() {
    use std::collections::BTreeSet;

    let mut companies: Vec<(String, PathBuf)> = subdirs(&repo_root().join("companies"))
        .into_iter()
        .map(|dir| {
            let name = dir.file_name().unwrap().to_str().unwrap().to_string();
            (name, dir)
        })
        .collect();
    // The baseline is held to exactly the same rules as a vertical's own file.
    companies.push(("globals".to_string(), repo_root().join("globals")));

    for (name, dir) in companies {
        if !super::has_task_file(&dir) {
            continue;
        }
        let cards =
            super::load_dir_tasks(&dir).unwrap_or_else(|err| panic!("{name}/tasks.toml: {err}"));
        assert!(
            !cards.is_empty(),
            "{name} ships a `tasks.toml` that seeds nothing"
        );

        let manifest =
            (name != "globals").then(|| CompanyManifest::from_path(&dir).expect("manifest"));
        let known: BTreeSet<String> = manifest
            .as_ref()
            .map(|m| {
                m.agents
                    .iter()
                    .map(|a| a.id.clone())
                    .chain(m.group_chats.iter().map(|g| g.id.clone()))
                    .collect()
            })
            .unwrap_or_default();

        for card in &cards {
            let rendered = card.to_record(0);
            // The safety property, asserted against the shipped content and not
            // only against the parser: nothing seeded can enter the column that
            // dispatches a run or the one that bills a planning pass.
            assert_eq!(
                rendered.column,
                crate::ports::tasks::COLUMN_TODO,
                "{name}/tasks.toml: `{}` is not To-do",
                card.id
            );
            assert!(
                card.note.as_deref().is_some_and(|n| !n.trim().is_empty()),
                "{name}/tasks.toml: `{}` has no note — a card that does not say what done looks \
                 like gets handed back as an essay",
                card.id
            );
            // A baseline card ships to every vertical and can know no roster, so
            // it must name no owner; a vertical's card may, but only one that
            // exists — seeding writes below `resolve_assignee`, so a typo would
            // persist and only surface as a card that refuses to dispatch.
            match card.assignee.as_deref().map(str::trim) {
                None | Some("") => {}
                Some(assignee) => {
                    assert!(
                        name != "globals",
                        "globals/tasks.toml: `{}` names an assignee, but the baseline ships to \
                         every company and can know no roster",
                        card.id
                    );
                    assert!(
                        known.contains(assignee),
                        "{name}/tasks.toml: `{}` is assigned to `{assignee}`, which is neither a \
                         teammate nor a desk in this company",
                        card.id
                    );
                }
            }
        }
    }
}
