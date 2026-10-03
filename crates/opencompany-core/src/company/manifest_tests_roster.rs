//! Roster, bootstrap and budget/plan-section manifest tests: inline vs
//! bundle rosters, agent classes, bootstrap lists, and the workflow-cap
//! and plan/budget validation (split out of `manifest_tests.rs`).

use super::*;

fn parse(text: &str) -> CompanyManifest {
    toml::from_str(text).expect("valid toml")
}

/// A valid 32-byte base58 address, built rather than pasted so the test
/// cannot drift from what the decoder accepts.
fn wallet_address() -> String {
    bs58::encode([9u8; 32]).into_string()
}

/// Writes a company bundle: `company.toml` plus optional `agents/` files.
fn write_bundle(company_toml: &str, agent_files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(MANIFEST_FILE), company_toml).expect("write manifest");
    if !agent_files.is_empty() {
        let agents = dir.path().join(super::super::agent_file::AGENTS_DIR);
        std::fs::create_dir_all(&agents).expect("agents dir");
        for (name, body) in agent_files {
            std::fs::write(agents.join(name), body).expect("write agent");
        }
    }
    dir
}

/// The compatibility rule: a bare `company.toml` with `[[agent]]` entries
/// and no `agents/` directory keeps working exactly as it always has.
#[test]
fn an_inline_roster_still_parses_when_there_is_no_agents_directory() {
    let dir = write_bundle(
        "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n",
        &[],
    );
    let manifest = CompanyManifest::from_path(dir.path()).expect("parses");
    // The global baseline is appended to every roster, so this asserts the
    // company's own teammates — the thing this test is about.
    let own: Vec<&str> = manifest
        .agents
        .iter()
        .filter(|agent| !agent.global)
        .map(|agent| agent.id.as_str())
        .collect();
    assert_eq!(own, ["ceo"]);
}

/// The bundle roster replaces the inline one — so a company that has moved
/// to `agents/*.toml` gets exactly those teammates.
#[test]
fn a_bundle_roster_supplies_the_agents() {
    let dir = write_bundle(
        "[company]\nname = \"X\"\n",
        &[
            ("ceo.toml", "role = \"CEO\"\ntier = \"orchestrator\"\n"),
            ("writer.toml", "role = \"Writer\"\n"),
        ],
    );
    let manifest = CompanyManifest::from_path(dir.path()).expect("parses");
    let ids: Vec<&str> = manifest
        .agents
        .iter()
        .filter(|a| !a.global)
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(ids, ["ceo", "writer"]);
    // The globals are appended after the company's own roster and none is
    // tagged `orchestrator`, so who orchestrates is unchanged by them.
    assert_eq!(super::super::orchestrator_id(&manifest.agents), Some("ceo"));
}

/// Declaring both forms is refused rather than resolved by precedence:
/// either precedence rule silently discards teammates somebody wrote down.
#[test]
fn declaring_both_roster_forms_is_refused_in_prosumer_language() {
    let dir = write_bundle(
        "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n",
        &[("writer.toml", "role = \"Writer\"\n")],
    );
    let err = CompanyManifest::from_path(dir.path()).expect_err("refused");
    let problems = match err {
        OpenCompanyError::ManifestInvalid { problems, .. } => problems,
        other => panic!("expected ManifestInvalid, got {other}"),
    };
    assert_eq!(problems.len(), 1);
    // It must name both places and say what to do, not merely that something
    // is wrong: the operator has to know which half to delete.
    assert!(problems[0].contains("agents/*.toml"), "{problems:?}");
    assert!(problems[0].contains("[[agent]]"), "{problems:?}");
    assert!(problems[0].contains("company.toml"), "{problems:?}");
}

/// `opencompany check` must load the bundle roster too. It calls
/// [`discover`] itself (for the legacy-filename note) and so takes its own
/// route into loading — which is exactly how it came to validate a manifest
/// whose roster it had never read, reporting every desk member as missing
/// from the roster.
#[test]
fn run_check_accepts_a_bundle_roster() {
    let dir = write_bundle(
        "[company]\nname = \"X\"\n\n[[group_chat]]\nid = \"d\"\nname = \"D\"\nmembers = [\"ceo\"]\n",
        &[("ceo.toml", "role = \"CEO\"\n")],
    );
    assert!(
        super::super::run_check(dir.path()),
        "a bundle-roster company must validate through the check command"
    );
}

/// Cross-cutting validation still applies to a bundle roster: a
/// `delegates_to` target is checked against the desks in `company.toml`,
/// which the per-file loader cannot see on its own.
#[test]
fn a_bundle_roster_is_still_validated_against_the_rest_of_the_manifest() {
    let dir = write_bundle(
        "[company]\nname = \"X\"\n\n[[group_chat]]\nid = \"research\"\nname = \"Research\"\n",
        &[(
            "ceo.toml",
            "role = \"CEO\"\ndelegates_to = [\"marketing\"]\n",
        )],
    );
    let err = CompanyManifest::from_path(dir.path()).expect_err("refused");
    let problems = match err {
        OpenCompanyError::ManifestInvalid { problems, .. } => problems,
        other => panic!("expected ManifestInvalid, got {other}"),
    };
    assert!(
        problems.iter().any(|p| p.contains("marketing")),
        "{problems:?}"
    );
}

#[test]
fn an_unknown_agent_class_is_refused() {
    let manifest = parse(
        "[company]\nname = \"X\"\n\n[[agent]]\nid = \"critic\"\nrole = \"Critic\"\nclasses = [\"judgey\"]\n",
    );
    let problems = manifest.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("classes") && p.contains("judgey")),
        "{problems:?}"
    );
}

#[test]
fn the_known_agent_classes_are_accepted() {
    let manifest = parse(
        "[company]\nname = \"X\"\n\n[[agent]]\nid = \"critic\"\nrole = \"Critic\"\nclasses = [\"judge\", \"evidence\", \"directive\"]\n",
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

/// A desk `tools` ceiling is optional and absent by default, so every
/// manifest written before desks could scope tools keeps its meaning.
#[test]
fn a_desk_tool_ceiling_defaults_to_empty() {
    let manifest = parse(
        "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\n[[group_chat]]\nid = \"d\"\nname = \"D\"\nmembers = [\"ceo\"]\n",
    );
    assert!(manifest.group_chats[0].tools.is_empty());
    assert!(manifest.validate().is_empty());
}

/// A manifest naming no `[users].mode` signs people in by email, exactly as
/// every manifest did before the key existed.
#[test]
fn users_mode_defaults_to_email() {
    let manifest = parse("[company]\nname = \"X\"\n");
    assert_eq!(manifest.users.mode, "email");
    assert!(manifest.validate().is_empty());
}

#[test]
fn an_unknown_users_mode_is_named_in_prosumer_language() {
    let manifest = parse("[company]\nname = \"X\"\n[users]\nmode = \"walet\"\n");
    let problems = manifest.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`[users].mode`") && p.contains("walet")),
        "{problems:?}"
    );
}

/// The interesting failure is not a malformed value but a **silently
/// unread** one: each mode reads exactly one bootstrap list, and filling in
/// the other is an operator who believes they granted access and has not.
#[test]
fn a_bootstrap_list_the_mode_never_reads_is_a_problem() {
    let manifest = parse(&format!(
        "[company]\nname = \"X\"\n[users]\nmode = \"email\"\nwallets = [\"{}\"]\n",
        wallet_address()
    ));
    let problems = manifest.validate();
    assert!(
        problems.iter().any(|p| p.contains("`[users].wallets`")),
        "{problems:?}"
    );

    let manifest =
        parse("[company]\nname = \"X\"\n[users]\nmode = \"wallet\"\nadmins = [\"a@b.com\"]\n");
    assert!(
        manifest
            .validate()
            .iter()
            .any(|p| p.contains("`[users].admins`")),
        "{:?}",
        manifest.validate()
    );
}

/// `none` reads neither list, because it admits nobody but the person at the
/// machine and has no way to add a second.
#[test]
fn none_mode_reads_no_bootstrap_list_at_all() {
    let manifest =
        parse("[company]\nname = \"X\"\n[users]\nmode = \"none\"\nadmins = [\"a@b.com\"]\n");
    let problems = manifest.validate();
    assert!(
        problems.iter().any(|p| p.contains("no sign-in")),
        "{problems:?}"
    );

    // Naming no list is the correct `none` manifest, and validates clean.
    let manifest = parse("[company]\nname = \"X\"\n[users]\nmode = \"none\"\n");
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

/// A wallet that cannot be decoded can never verify a signature, so it is
/// caught by `opencompany check` rather than by a person who cannot sign in.
#[test]
fn a_malformed_bootstrap_wallet_is_rejected() {
    let manifest =
        parse("[company]\nname = \"X\"\n[users]\nmode = \"wallet\"\nwallets = [\"0OIl\"]\n");
    let problems = manifest.validate();
    assert!(
        problems.iter().any(|p| p.contains("`[users].wallets`")),
        "{problems:?}"
    );

    let manifest = parse(&format!(
        "[company]\nname = \"X\"\n[users]\nmode = \"wallet\"\nwallets = [\"{}\"]\n",
        wallet_address()
    ));
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

/// `normalize_email` only lowercases and trims, so an `[users].admins`
/// entry with no `@` can still be a normalized key — including one that
/// collides with the `local:owner` scheme `LoginIdentity::parse` reserves
/// for the `none`-mode owner. Caught here, before a bootstrapped user is
/// ever stored under that exact key.
#[test]
fn a_bootstrap_admin_that_is_not_an_email_address_is_rejected() {
    let manifest =
        parse("[company]\nname = \"X\"\n[users]\nmode = \"email\"\nadmins = [\"Local:Owner\"]\n");
    let problems = manifest.validate();
    assert!(
        problems.iter().any(|p| p.contains("`[users].admins`")),
        "{problems:?}"
    );

    let manifest = parse(
        "[company]\nname = \"X\"\n[users]\nmode = \"email\"\nadmins = [\"ada@example.com\"]\n",
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

#[test]
fn bare_agents_toml_is_valid() {
    let manifest = parse(
        r#"
        [company]
        name = "Agentic Marketing Agency"
        output = "Campaigns across every channel"
        human_role = "Campaign review and sign-off"

        [[agent]]
        id = "copywriter"
        role = "Copywriter"
        description = "Write ads."
        "#,
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

#[test]
fn defaults_are_prosumer_safe() {
    let manifest = parse("[company]\nname = \"Solo\"\n");
    assert_eq!(manifest.brain.mode, "hosted");
    assert_eq!(manifest.tools.provider, "openhuman");
    assert_eq!(manifest.policy.mode, "supervised");
    // Issue #684: this asserted the three-string default verbatim, which is
    // how the defect survived — the list's *contents* were pinned and its
    // *effect* never was, so a list that matched nothing passed. It is
    // empty now, and what makes the defaults prosumer-safe is the
    // `supervised` mode asserted above: `evaluate_supervised` parks every
    // Spend / Sign / Publish effect on its own.
    assert!(
        manifest.policy.always_approve.is_empty(),
        "the default always-approve list is empty on purpose — see \
         DEFAULT_ALWAYS_APPROVE"
    );
}

#[test]
fn workflows_run_cap_defaults_when_omitted() {
    // Issue #401: an absent `[workflows].max_in_flight_runs` takes the
    // generous default and never trips validation.
    let manifest = parse("[company]\nname = \"X\"\n");
    assert_eq!(
        manifest.workflows.max_in_flight_runs,
        crate::company::types::DEFAULT_MAX_IN_FLIGHT_RUNS
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

#[test]
fn workflows_run_cap_parses_explicit_value() {
    let manifest = parse("[company]\nname = \"X\"\n[workflows]\nmax_in_flight_runs = 3\n");
    assert_eq!(manifest.workflows.max_in_flight_runs, 3);
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

#[test]
fn workflows_run_cap_of_zero_is_rejected() {
    // Issue #401: `0` would refuse every run, so it is a validation error
    // named in prosumer language rather than a silently wedged company.
    let manifest = parse("[company]\nname = \"X\"\n[workflows]\nmax_in_flight_runs = 0\n");
    let problems = manifest.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`[workflows].max_in_flight_runs`") && p.contains("at least 1")),
        "{problems:?}"
    );
}

#[test]
fn valid_plan_section_passes() {
    let manifest = parse(
        "[company]\nname = \"X\"\n[plan]\nname = \"starter\"\nperiod = \"monthly\"\n[plan.token_budgets]\nweb = 500000\n",
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

#[test]
fn absent_plan_is_valid() {
    // No `[plan]` → gating off; the default section must not trip validation.
    let manifest = parse("[company]\nname = \"X\"\n");
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
}

#[test]
fn rejects_unknown_plan_name_in_prosumer_language() {
    let manifest = parse("[company]\nname = \"X\"\n[plan]\nname = \"enterprise\"\n");
    let problems = manifest.validate();
    assert!(
        problems.iter().any(|p| p.contains("`[plan].name`")
            && p.contains("free, starter, pro, unlimited")
            && p.contains("enterprise")),
        "{problems:?}"
    );
}

#[test]
fn rejects_bad_plan_period() {
    let manifest = parse("[company]\nname = \"X\"\n[plan]\nname = \"free\"\nperiod = \"hourly\"\n");
    let problems = manifest.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`[plan].period`") && p.contains("hourly")),
        "{problems:?}"
    );
}

#[test]
fn rejects_non_gateable_budget_namespace() {
    let manifest = parse(
        "[company]\nname = \"X\"\n[plan]\nname = \"pro\"\n[plan.token_budgets]\ntelepathy = 100\n",
    );
    let problems = manifest.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("telepathy") && p.contains("token_budgets")),
        "{problems:?}"
    );
}
