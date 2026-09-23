//! Content-validation tests: manifest/workflow/skill parsing, and the
//! search- and billing-grant rules every shipped template must obey
//! (split out of `content_tests.rs`).

use super::content_tests_support::*;
use super::{
    CompanyManifest, Tools, grants_chargebee_explicit, grants_composio_explicit,
    grants_media_explicit, grants_paypal_explicit, grants_search_explicit, load_dir_skills,
    parse_workflow, walk_workspace,
};
use crate::runtime::builder::{agent_scoped_grants, effective_grants};

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
/// disk under the company: the search-dependent skills (`web-research` in the
/// baseline, `seo-audit` and `competitor-scan` in the bundles that author
/// them) are all in the skill registry, which is global and unscoped, so an
/// operator can install any of them into any company at runtime. Whether a
/// template belongs here is therefore a judgement about its charter —
/// research, editorial, marketing, legal, product engineering — recorded here
/// because it cannot be derived from content.
const SEARCH_GRANTED_COMPANIES: [&str; 21] = [
    "accounting_firm",
    "consultation_firm",
    "customer_support",
    "design_studio",
    "enterprise_sales",
    "game_business",
    "game_studio",
    "influencer_business",
    "law_firm",
    "marketing_agency",
    "media_company",
    "pharma_startup",
    "product_team",
    "realestate_company",
    "recruiting_company",
    "research_lab",
    "software_company",
    "venture_capital",
    "venture_studio",
    "signals_opportunity_studio",
    "startup_accelerator",
];

/// Templates that must NEVER reach the metered search backend: `e2e_harness` and
/// `e2e_setup` are deterministic fixtures (a priced network call would make them
/// non-hermetic and flaky), `openhuman_demo` is a walkthrough nobody opted
/// into spend for, and `math_lab` is denied for a reason of its own —
/// its whole claim (and `hive_math_lab`'s, the same lab on one deliberating desk) is that it *computes* an exact answer, and a lab that can
/// search can look one up. A run that looked the answer up passes the lab's
/// end-to-end spec while proving nothing about whether the roster can solve
/// anything, so withholding the network is what makes the number evidence.
///
/// `vending_machine_co` is denied on a variant of the same argument. Every fact
/// that bundle reasons from — what is on a shelf, what a line costs today, which
/// host site is unhappy — is a tool call against its own simulated operation, and
/// a desk that could reach the web would answer about vending machines in general
/// instead of about these eight. Withholding the network is what makes a decision
/// there attributable to the fleet it was made about.
const SEARCH_DENIED_COMPANIES: [&str; 9] = [
    "math_lab",
    "hive_math_lab",
    "e2e_harness",
    "e2e_setup",
    "openhuman_demo",
    // `openhuman_demo` with the CEO shared across two desks — the hive-desks
    // measurement fixture (plan hive-desks, Track B); same posture, same reason.
    "hive_demo",
    "vending_machine_co",
    // Denied on exactly `vending_machine_co`'s argument. Every fact this bundle
    // reasons from — what is on the order, which variants are in stock, what the
    // customer paid — is a tool call against the shared tau2 retail state, and a
    // desk that could reach the web would answer about online retail in general
    // instead of about THIS order. It is also scored against that state, so a
    // fact from outside it is not merely off-topic, it is unattributable.
    "retail_co",
    // The same argument again, against a cluster that is real rather than
    // simulated. Every fact this bundle reasons from is a reading taken through
    // its monitoring server, and a finding is worth something only if it can be
    // traced back to one. A desk that could search would answer about Kubernetes
    // in general — plausibly, fluently, and about somebody else's cluster, which
    // is the one failure a monitor cannot afford.
    "ops_watch",
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
/// company nobody remembered: `software_company` shipped with nine
/// agents and no search grant, and the suite stayed green for it (issue #878).
/// [`every_company_declares_a_search_posture`] asserts this list plus the other
/// two covers `companies/` exactly, so a new template fails CI until whoever
/// adds it writes the decision down here.
const SEARCH_UNGRANTED_COMPANIES: [&str; 0] = [];

/// The subset of [`SEARCH_GRANTED_COMPANIES`] that restates the default belt
/// verbatim and appends `search`. `signals_opportunity_studio` is deliberately
/// excluded: it overrides the default down to a research-only belt on purpose,
/// and `research_lab` is excluded for the same reason — its belt is
/// `["*", "search"]`, dropping `media` and `composio`, because a research lab
/// has no use for image generation or third-party OAuth side effects and both
/// are opt-in spend. `product_team` is excluded on that same
/// research-lab argument: it produces documents and ledger rows, so it drops
/// both opt-in namespaces too.
const FULL_BELT_PLUS_SEARCH: [&str; 8] = [
    // Both restate the belt verbatim and append `chargebee` (#788) rather than
    // `search`, which is already inherited. They belong here for the property
    // this list actually guards — that an extended `allow` did not silently
    // drop an inherited entry — which is independent of *which* namespace the
    // template extended it with.
    "accounting_firm",
    "consultation_firm",
    "design_studio",
    "law_firm",
    "marketing_agency",
    "media_company",
    "software_company",
    "venture_studio",
];

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
/// fifteen it did not list. `software_company` therefore shipped nine
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
/// default (`companies/_globals/globals.toml`'s `default_allow`), it never extends it. A
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
        "marketing_agency",
        "accounting_firm",
        "venture_studio",
        "software_company",
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
