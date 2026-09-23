//! Content-validation tests: shipped ledger declarations, setup
//! posture, MCP server safety, setup cards, and MCP reachability
//! (split out of `content_tests.rs`).

use std::path::{Path, PathBuf};

use super::content_tests_support::*;
use super::{CompanyManifest, load_dir_ledgers};
use crate::runtime::builder::agent_scoped_grants;

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
/// Covers the `companies/_globals/` baseline as well as `companies/`: the baseline ships
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
        check("companies/_globals/ledgers".to_string(), spec);
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
const SETUP_SEEDED_COMPANIES: [&str; 25] = [
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
    "math_lab",
    "ops_watch",
    "media_company",
    "pharma_startup",
    "product_team",
    "realestate_company",
    "recruiting_company",
    "research_lab",
    "software_company",
    "venture_capital",
    "venture_studio",
    "hive_math_lab",
    "signals_opportunity_studio",
    "startup_accelerator",
    "vending_machine_co",
];

/// The bundles that deliberately ship neither, because they are fixtures.
///
/// A fixture proves a mechanism and is asserted against exactly — `e2e_harness`
/// and `openhuman_demo` also declare their own `[[mcp_server]]` inline — so
/// seeded cards and a second declaration of `deepwiki` would both perturb what
/// they exist to pin down.
const FIXTURE_COMPANIES: [&str; 5] = [
    "e2e_harness",
    "e2e_setup",
    "openhuman_demo",
    // The hive-desks measurement fixture: `openhuman_demo` with one agent on
    // two desks, asserted against exactly by `scripts/measure-coordination.*`.
    "hive_demo",
    // A benchmark fixture: it proves a mechanism and is asserted against
    // exactly, by tau2's own `evaluation_criteria`. Seeded cards would be
    // work nobody asked for sitting in a company whose only job is to answer
    // one replayed task and be scored on the end state.
    "retail_co",
];

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
    companies.push((
        "globals".to_string(),
        repo_root().join("companies").join("_globals"),
    ));

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
                        "companies/_globals/tasks.toml: `{}` names an assignee, but the baseline ships to \
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

/// One teammate's effective grants under the full three-level narrowing a
/// running company applies: `[tools].allow ∩ group_chat.tools ∩ [[agent]].tools`.
///
/// Runs the real `agent_scoped_grants` over the desks this teammate actually
/// sits on, so a bundle cannot pass here and fail in the harness. The
/// two-level `grants_for_one_agent` above skips the desk ceiling, which is the
/// level a reachability question turns on.
fn desk_scoped_grants(manifest: &CompanyManifest, agent: &super::Agent) -> Vec<String> {
    let desk_tools: Vec<Vec<String>> = manifest
        .group_chats
        .iter()
        .filter(|chat| chat.members.iter().any(|member| member == &agent.id))
        .map(|chat| chat.tools.clone())
        .collect();
    let desk_refs: Vec<&[String]> = desk_tools.iter().map(Vec::as_slice).collect();
    agent_scoped_grants(&manifest.tools.allow, &desk_refs, agent.tools.as_deref())
}

/// Every bundle, with its global baseline left on — the roster a running
/// company actually has.
fn load_company_with_globals(dir: &Path) -> CompanyManifest {
    CompanyManifest::from_path(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
}

/// A declared MCP server must be callable by somebody.
///
/// Declaring a server and granting the namespace are separate edits in
/// separate files, and nothing until this test compared them. A bundle could
/// ship a server, document it in its README, pass every parse and safety check
/// above, and still hand it to a roster where no teammate holds `mcp:*` — an
/// install that connects, reports healthy, and answers no call anyone can make.
///
/// Asserted for declared servers whether or not they ship enabled: enabling is
/// an operator's one click, and the grant has to already be right when they
/// make it.
#[test]
fn every_declared_mcp_server_is_reachable_by_some_teammate() {
    let mut checked = 0usize;
    let mut unreachable = Vec::new();
    for company in subdirs(&repo_root().join("companies")) {
        let name = company.file_name().unwrap().to_str().unwrap().to_string();
        let manifest = load_company_with_globals(&company);
        for server in &manifest.mcp_servers {
            checked += 1;
            let reached = manifest.agents.iter().any(|agent| {
                crate::runtime::tools::grants_cover_server(
                    &desk_scoped_grants(&manifest, agent),
                    &server.name,
                )
            });
            if !reached {
                unreachable.push(format!(
                    "  {name}: `{}` — company grants {:?}",
                    server.name, manifest.tools.allow
                ));
            }
        }
    }
    assert!(
        unreachable.is_empty(),
        "{} declared MCP server(s) no teammate can reach. Connecting one reports healthy \
         and answers no call anybody can make:\n{}",
        unreachable.len(),
        unreachable.join("\n")
    );
    assert!(
        checked > 0,
        "no shipped bundle declared an MCP server, so this check looked at nothing — \
         the walk found no manifests rather than finding them clean"
    );
}

/// A desk that can reach none of the company's MCP servers is a dead end.
///
/// A desk is a conversation an operator opens, so "somebody in the company can
/// call it" is not the promise the screen makes — the promise is that the
/// teammates in front of them can.
///
/// Deliberately "at least one server", not "every server". Scoping a single
/// server to the desk that owns it is the point of the middle level:
/// `retail_co` gives each teammate exactly one `mcp:<server>` and its desks
/// reach only their own, which is correct and must keep passing. What a desk
/// may not be is cut off entirely from a company that installed servers.
///
/// This is the level that `every_declared_mcp_server_is_reachable_by_some_teammate`
/// cannot see: a bundle whose other desks hold the grant passes it while the
/// desk an operator is typing into holds nothing.
#[test]
fn every_desk_can_reach_at_least_one_declared_mcp_server() {
    let mut dead_ends = Vec::new();
    for company in subdirs(&repo_root().join("companies")) {
        let name = company.file_name().unwrap().to_str().unwrap().to_string();
        let manifest = load_company_with_globals(&company);
        if manifest.mcp_servers.is_empty() {
            continue;
        }
        for chat in &manifest.group_chats {
            if chat.members.is_empty() {
                continue;
            }
            let reached = chat.members.iter().any(|member| {
                manifest
                    .agents
                    .iter()
                    .find(|agent| &agent.id == member)
                    .is_some_and(|agent| {
                        let grants = desk_scoped_grants(&manifest, agent);
                        manifest.mcp_servers.iter().any(|server| {
                            crate::runtime::tools::grants_cover_server(&grants, &server.name)
                        })
                    })
            });
            if !reached {
                dead_ends.push(format!(
                    "  {name}: desk `{}` reaches none of {} installed server(s) — desk ceiling {:?}",
                    chat.id,
                    manifest.mcp_servers.len(),
                    chat.tools
                ));
            }
        }
    }
    assert!(
        dead_ends.is_empty(),
        "{} desk(s) narrow the company grant past a server the company installed. An \
         operator talking to one gets a teammate that cannot call a server the console \
         reports as connected:\n{}",
        dead_ends.len(),
        dead_ends.join("\n")
    );
}

/// The global baseline reaches MCP.
///
/// Every company inherits these teammates whichever vertical it started from,
/// including one minted by the setup wizard, and they answer in the main
/// channel. A baseline teammate without the namespace makes MCP unreachable in
/// a company whose own roster and grants are entirely correct — the one gap
/// neither check above can see, because it is not any bundle's fault.
///
/// `BASE_BELT` already grants it to every teammate the wizard mints; this holds
/// the hand-authored baseline to the same rule.
#[test]
fn every_global_teammate_can_reach_an_installed_mcp_server() {
    let globals = crate::globals::agents();
    assert!(
        !globals.is_empty(),
        "the global baseline is empty, so this check looked at nothing"
    );
    let allow = vec!["mcp:*".to_string()];
    let unreachable: Vec<String> = globals
        .iter()
        .filter(|agent| {
            !crate::runtime::tools::grants_cover_server(
                &agent_scoped_grants(&allow, &[], agent.tools.as_deref()),
                "any-installed-server",
            )
        })
        .map(|agent| format!("  `{}` — belt {:?}", agent.id, agent.tools))
        .collect();
    assert!(
        unreachable.is_empty(),
        "{} global teammate(s) cannot reach an installed MCP server. Every company \
         inherits these, so the gap follows the baseline into companies whose own grants \
         are correct:\n{}",
        unreachable.len(),
        unreachable.join("\n")
    );
}
