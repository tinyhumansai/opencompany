use super::*;

/// The three gate states of the metered `web_search` surface (issue #238),
/// in one table.
///
/// The load-bearing row is the first: a broad `*` grant does **not** wire
/// `web_search` even with a credential present. Every call is a priced
/// request on the managed platform, so — like `media` and `composio` — it
/// must be opted into by name and can never ride in on the wildcard a
/// company set for its file and shell tools.
#[test]
fn web_search_is_wired_only_by_explicit_grant_and_credential() {
    // `*` + credential → absent. The wildcard never confers spend.
    let wildcard = built_tool_names_with_search(&["*"]);
    assert!(
        !wildcard.contains(&"web_search".to_string()),
        "a bare `*` must NOT confer the metered search family: {wildcard:?}"
    );

    // explicit `search` + credential → present.
    let granted = built_tool_names_with_search(&["search"]);
    assert!(
        granted.contains(&"web_search".to_string()),
        "an explicit `search` grant with a credential must wire web_search: {granted:?}"
    );
    // The sub-grant form works the same way `media.*` / `composio.*` do.
    let sub_granted = built_tool_names_with_search(&["search.web"]);
    assert!(
        sub_granted.contains(&"web_search".to_string()),
        "{sub_granted:?}"
    );

    // explicit `search`, NO credential → absent, fail-closed.
    let uncredentialed = built_tool_names(&["search"], false);
    assert!(
        !uncredentialed.contains(&"web_search".to_string()),
        "a search grant with no managed credential must wire nothing: {uncredentialed:?}"
    );

    // An unrelated grant confers nothing even with a credential wired.
    let unrelated = built_tool_names_with_search(&["web.*"]);
    assert!(
        !unrelated.contains(&"web_search".to_string()),
        "an unrelated grant must not confer web_search: {unrelated:?}"
    );
}

/// Granting `search` must not quietly hand over anything *else*: the
/// credentialed `["search"]` belt is the ungranted belt plus exactly one
/// tool. A namespace that widens the belt beyond its own family is how a
/// grant stops meaning what the operator read.
#[test]
fn the_search_grant_adds_exactly_one_tool() {
    let mut baseline = built_tool_names(&[], false);
    let granted = built_tool_names_with_search(&["search"]);
    baseline.push("web_search".to_string());
    baseline.sort();
    assert_eq!(granted, baseline, "the `search` grant widened the belt");
}

/// The four gate states of the workspace surface, in one table.
///
/// The load-bearing row is the second: a broad `*` grant yields the READ
/// tools but NOT `workspace_write`. Writes mutate operator-owned guidance
/// every other agent then trusts, so — like `media` and `composio` — they
/// must be opted into by name and can never ride in on a wildcard.
#[test]
fn workspace_tools_are_wired_by_grant_and_store_presence() {
    // No store wired → fail closed, nothing built, whatever the grant.
    let unwired = built_tool_names(&["workspace"], false);
    for tool in [
        "workspace_list",
        "workspace_read",
        "workspace_search",
        "workspace_write",
        "workspace_rename",
        "workspace_delete",
    ] {
        assert!(
            !unwired.contains(&tool.to_string()),
            "no store must mean no `{tool}`: {unwired:?}"
        );
    }

    // `*` → reads only. This is the whole asymmetry.
    let wildcard = built_tool_names_with_workspace(&["*"]);
    assert!(
        wildcard.contains(&"workspace_list".to_string()),
        "{wildcard:?}"
    );
    assert!(
        wildcard.contains(&"workspace_read".to_string()),
        "{wildcard:?}"
    );
    // Issue #607: search is a read and rides the read side of the gate. It
    // reads exactly what `workspace_read` already grants, and it is the
    // cheap path — behind the write grant it would be missing from every
    // default agent, leaving them on the list-then-read crawl.
    assert!(
        wildcard.contains(&"workspace_search".to_string()),
        "a bare `*` must confer workspace search: {wildcard:?}"
    );
    for tool in ["workspace_write", "workspace_rename", "workspace_delete"] {
        assert!(
            !wildcard.contains(&tool.to_string()),
            "a bare `*` must NOT confer `{tool}`: {wildcard:?}"
        );
    }

    // Explicit `workspace` → reads + all four mutations. The lifecycle pair
    // (issue #671) rides this grant rather than a new one: it reaches only
    // the agent's own folder, which is narrower than the unconfined
    // overwrite the same grant already confers.
    let explicit = built_tool_names_with_workspace(&["workspace"]);
    for tool in [
        "workspace_create",
        "workspace_write",
        "workspace_rename",
        "workspace_delete",
    ] {
        assert!(explicit.contains(&tool.to_string()), "{tool}: {explicit:?}");
    }

    // No workspace grant at all → nothing, even with a store wired.
    let ungranted = built_tool_names_with_workspace(&["web.*"]);
    for tool in [
        "workspace_list",
        "workspace_read",
        "workspace_search",
        "workspace_write",
        "workspace_rename",
        "workspace_delete",
    ] {
        assert!(
            !ungranted.contains(&tool.to_string()),
            "an unrelated grant must not confer `{tool}`: {ungranted:?}"
        );
    }
}

/// `workspace.read` is a genuinely read-only grant.
///
/// A deliberate divergence from the `media` / `composio` helpers, which
/// match any `<ns>.` prefix and would therefore let `workspace.read` confer
/// writes — a footgun on a destructive surface.
#[test]
fn a_workspace_read_grant_does_not_confer_writes() {
    let read_grant = built_tool_names_with_workspace(&["workspace.read"]);
    assert!(
        read_grant.contains(&"workspace_read".to_string()),
        "{read_grant:?}"
    );
    for tool in ["workspace_write", "workspace_rename", "workspace_delete"] {
        assert!(
            !read_grant.contains(&tool.to_string()),
            "`workspace.read` must not confer `{tool}`: {read_grant:?}"
        );
    }

    let write_grant = built_tool_names_with_workspace(&["workspace.write"]);
    for tool in ["workspace_write", "workspace_rename", "workspace_delete"] {
        assert!(
            write_grant.contains(&tool.to_string()),
            "{tool}: {write_grant:?}"
        );
    }
}

/// `workspace_search` rides the `workspace` READ grant and NEVER the
/// metered `search` grant (issue #607).
///
/// The names invite the wrong wiring, and the wrong wiring would defeat the
/// issue: `search` is the paid external-credential grant that carries
/// `web_search`, and putting workspace search behind it would mean an agent
/// needs a billed backend credential to read its own company's notes — the
/// crawl stays, and now it stays for a reason nobody would guess from the
/// tool's description. Search reads exactly what `workspace_read` already
/// grants, so it costs the operator no additional decision.
#[test]
fn workspace_search_rides_the_workspace_grant_and_not_the_metered_search_grant() {
    // The `search` grant alone confers `web_search` — and nothing from the
    // workspace family, which is not granted here at all.
    let metered = built_tool_names_with_search(&["search"]);
    assert!(metered.contains(&"web_search".to_string()), "{metered:?}");
    assert!(
        !metered.contains(&"workspace_search".to_string()),
        "the metered `search` grant must not confer workspace search: {metered:?}"
    );

    // …and the workspace read grant confers workspace search without
    // conferring the billed one.
    let workspace = built_tool_names_with_workspace(&["workspace.read"]);
    assert!(
        workspace.contains(&"workspace_search".to_string()),
        "`workspace.read` must confer workspace search: {workspace:?}"
    );
    assert!(
        !workspace.contains(&"web_search".to_string()),
        "reading company notes must not require a billed search credential: {workspace:?}"
    );
}

/// (a) The EXACT tool belt a dispatched desk agent receives with the broad
/// `*` grant. Any tool added to or removed from a dispatched agent flips
/// this snapshot and fails CI — the whole point of the pin. The set is the
/// curated exec subset (shell / code / web) plus the intrinsic memory + file
/// tools and the three hand-off tools every roster agent carries; it
/// contains NO orchestrator authority and NO deferred family, and — the #238
/// addition — no `web_search`, because a bare `*` does not confer the
/// `search` grant. Nor does it contain `mcp_registry_list_tools` /
/// `mcp_registry_tool_call`, for the identical reason: those two ride the
/// explicit `mcp_registry` grant, never the wildcard — see the
/// `mcp_registry_tools_are_wired_only_by_explicit_grant` test below for the
/// belt a company that names that grant actually receives.
#[test]
fn dispatched_desk_agent_tool_belt_is_pinned() {
    let names = built_tool_names(&["*"], false);
    let mut expected = vec![
        "apply_patch",
        "csv_export",
        "curl",
        "edit",
        "file_read",
        "file_write",
        "git_operations",
        "glob",
        "grep",
        "http_request",
        "image_info",
        "list",
        // The deliberate-memory trio (issue #1113): intrinsic, on every
        // belt — company-scoped by construction, see memory_tools.rs.
        "memory_forget",
        "memory_recall",
        "memory_store",
        "read_workspace_state",
        "request_approval",
        "shell",
        "web_fetch",
        // Issue #1861: intrinsic, on every belt and gated by nothing. The
        // ability to ask a person a question is not a capability an agent
        // can be too narrow to hold — a narrow agent is the one most likely
        // to hit something only the operator can answer, and its
        // alternatives are guessing or going quiet.
        "escalate_to_human",
        // The hand-off tools, on every roster agent's belt whatever its
        // `delegates_to` says (an empty list is unrestricted, not unwired):
        // a teammate that cannot reach the colleague beside it, and cannot
        // open a card, is one the runtime had to card *for* — which is how
        // every desk message became a task nobody asked for.
        "delegate_to_desk",
        "delegate_to_teammate",
        "spawn_task",
    ];
    // The global baseline installs skills in every company (issue: global
    // agents/skills/workflows), so the three skill read tools are on every
    // belt now — including a company with no skills source of its own.
    expected.extend(["describe_skill", "list_skills", "read_skill_resource"]);
    expected.sort();
    assert_eq!(names, expected, "dispatched desk belt drifted: {names:?}");
}

/// The three gate states of the `mcp_registry` surface, mirroring
/// [`web_search_is_wired_only_by_explicit_grant_and_credential`].
///
/// The load-bearing row is the first: a broad `*` grant does **not** wire
/// either `mcp_registry_list_tools` or `mcp_registry_tool_call`, even with
/// a configured registry home (`pin_deps` always sets one). Before this
/// gate existed, both tools were pushed unconditionally whenever
/// `deps.mcp_home` was set — this row is the regression check for that.
#[cfg(feature = "mcp")]
#[test]
fn mcp_registry_tools_are_wired_only_by_explicit_grant() {
    const REGISTRY_TOOLS: [&str; 2] = ["mcp_registry_list_tools", "mcp_registry_tool_call"];

    // No grant at all → absent.
    let ungranted = built_tool_names(&[], false);
    for tool in REGISTRY_TOOLS {
        assert!(
            !ungranted.contains(&tool.to_string()),
            "no grant must mean no `{tool}`: {ungranted:?}"
        );
    }

    // `*` (with a configured home) → still absent. The wildcard never
    // confers a third-party-reaching, mutating surface.
    let wildcard = built_tool_names(&["*"], false);
    for tool in REGISTRY_TOOLS {
        assert!(
            !wildcard.contains(&tool.to_string()),
            "a bare `*` must NOT confer `{tool}`: {wildcard:?}"
        );
    }

    // An unrelated grant → absent.
    let unrelated = built_tool_names(&["web.*"], false);
    for tool in REGISTRY_TOOLS {
        assert!(
            !unrelated.contains(&tool.to_string()),
            "an unrelated grant must not confer `{tool}`: {unrelated:?}"
        );
    }

    // Explicit `mcp_registry` grant, with a configured home → BOTH tools,
    // together — list is never conferred without call, or vice versa.
    let granted = built_tool_names(&["mcp_registry"], false);
    for tool in REGISTRY_TOOLS {
        assert!(
            granted.contains(&tool.to_string()),
            "an explicit `mcp_registry` grant must wire `{tool}`: {granted:?}"
        );
    }

    // The sub-grant form works the same way `search.web` / `composio.gmail`
    // do.
    let sub_granted = built_tool_names(&["mcp_registry.notion"], false);
    for tool in REGISTRY_TOOLS {
        assert!(
            sub_granted.contains(&tool.to_string()),
            "`mcp_registry.notion` must wire `{tool}`: {sub_granted:?}"
        );
    }
}

/// Explicit `mcp_registry` grant, NO configured registry home → absent,
/// fail-closed — matching every other explicit namespace's
/// granted-but-uncredentialed shape (`media`, `composio`, `chargebee`,
/// `paypal`, `hosting`, `search`).
#[cfg(feature = "mcp")]
#[test]
fn mcp_registry_tools_fail_closed_with_no_registry_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut deps = pin_deps(dir.path().to_path_buf());
    deps.mcp_home = None;
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "desk".to_string(),
        role: "Desk Lead".to_string(),
        name: None,
        description: None,
        tier: None,
        harness: None,
        tools: None,
        delegates_to: Vec::new(),
        context: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    };
    let policy = ApprovalPolicy::new(&Policy::default(), None);
    let grants: Vec<String> = vec!["mcp_registry".to_string()];
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &grants,
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");
    let names: Vec<String> = agent.tools().iter().map(|t| t.name().to_string()).collect();
    assert!(
        !names.contains(&"mcp_registry_list_tools".to_string()),
        "granted-but-unconfigured must not wire `mcp_registry_list_tools`: {names:?}"
    );
    assert!(
        !names.contains(&"mcp_registry_tool_call".to_string()),
        "granted-but-unconfigured must not wire `mcp_registry_tool_call`: {names:?}"
    );
}

#[test]
fn request_approval_is_intrinsic_and_needs_no_manifest_grant() {
    let names = built_tool_names(&[], false);
    assert!(names.contains(&"request_approval".to_string()), "{names:?}");
}

/// Issue #988: the tool-iteration ceiling is **stated** on every agent this
/// crate builds, not inherited by omission.
///
/// The distinction is the whole bug. `build_agent` never called
/// `set_max_tool_iterations`, so every company agent silently ran on
/// `AgentConfig::default()`'s ten — a number no OpenCompany source
/// mentioned, and one that a teammate doing real multi-step work spends
/// before it delivers anything (#926). Deleting the call again would put the
/// build back on the vendored default and fail here.
///
/// The second assertion is what keeps this honest across a vendored bump: it
/// checks the stated number is genuinely *higher* than what omission would
/// have given, rather than comparing a constant to itself.
#[test]
fn every_built_agent_states_a_raised_tool_iteration_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = pin_deps(dir.path().to_path_buf());
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "ceo".to_string(),
        role: "Chief Executive".to_string(),
        name: None,
        description: None,
        tier: None,
        tools: None,
        delegates_to: Vec::new(),
        context: None,
        harness: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    };
    let agent = build_agent(
        &CompanyId::new("acme"),
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(ApprovalPolicy::new(&Policy::default(), None)),
        &deps,
        &[],
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");

    assert_eq!(
        agent.max_tool_iterations(),
        MAX_TOOL_ITERATIONS,
        "the built agent is not running on the cap this crate states"
    );

    let inherited = oh::config::AgentConfig::default().max_tool_iterations;
    assert!(
        MAX_TOOL_ITERATIONS > inherited,
        "stating {MAX_TOOL_ITERATIONS} is only a fix while it exceeds the vendored \
         default of {inherited}"
    );
}

/// (b) A dispatched desk agent carries the three **hand-off** tools and
/// none of the orchestrator's **authority**; the orchestrator carries both.
/// Building both from the same grant and contrasting them is the
/// registration check that a desk lead can reach a colleague without
/// becoming a second CEO.
///
/// Issue #176 wired the hand-off tools only onto a member that opted in
/// with `delegates_to`; they are now on every belt, and the list only
/// narrows where they reach — see [`a_narrowed_member_gets_the_same_belt`].
#[test]
fn dispatched_agent_has_the_hand_off_tools_but_not_the_orchestrators_authority() {
    let hand_off = ["spawn_task", "delegate_to_desk", "delegate_to_teammate"];
    let authority = [
        "query_company",
        "assign_task",
        "review_task",
        "add_agent",
        "run_workflow",
        "create_workflow",
        "read_run_output",
    ];

    let dispatched = built_tool_names(&["*"], false);
    for tool in hand_off {
        assert!(
            dispatched.contains(&tool.to_string()),
            "dispatched desk agent MUST receive hand-off tool `{tool}`: {dispatched:?}"
        );
    }
    for tool in authority {
        assert!(
            !dispatched.contains(&tool.to_string()),
            "dispatched desk agent must NOT receive orchestrator authority `{tool}`: \
             {dispatched:?}"
        );
    }

    let orchestrator = built_tool_names(&["*"], true);
    for tool in hand_off.iter().chain(authority.iter()) {
        assert!(
            orchestrator.contains(&tool.to_string()),
            "orchestrator agent MUST receive `{tool}`: {orchestrator:?}"
        );
    }
}

/// (b2) A member whose manifest names a `delegates_to` allowlist gets the
/// **same belt** as one that names none: the list narrows where the hand-off
/// tools reach (checked at call time against the live record), it does not
/// decide whether they are wired. Expressed as an equality against the
/// pinned belt, so that snapshot stays the single place the dispatched belt
/// is written down.
#[test]
fn a_narrowed_member_gets_the_same_belt() {
    let plain = built_tool_names(&["*"], false);
    let narrowed = built_tool_names_delegating(&["*"], false, &["research"]);
    assert_eq!(
        narrowed, plain,
        "`delegates_to` narrows reach, never the belt: {narrowed:?}"
    );
}

/// (b3) An empty allowlist and an absent one build the same belt, and the
/// orchestrator's own belt is untouched by `delegates_to`.
///
/// The orchestrator half is what proves the `else` really is exclusive,
/// since a second scoped `delegate_to_desk` beside the orchestrator's
/// unrestricted one would put two tools of the same name on one belt.
#[test]
fn an_empty_allowlist_and_the_orchestrator_belt_are_unchanged() {
    assert_eq!(
        built_tool_names_delegating(&["*"], false, &[]),
        built_tool_names(&["*"], false),
        "an empty `delegates_to` must produce the ordinary belt byte-for-byte"
    );

    let orchestrator = built_tool_names(&["*"], true);
    assert_eq!(
        built_tool_names_delegating(&["*"], true, &["research"]),
        orchestrator,
        "an orchestrator's belt must not change when it also names `delegates_to`"
    );
    assert_eq!(
        orchestrator
            .iter()
            .filter(|t| *t == "delegate_to_desk")
            .count(),
        1,
        "exactly one `delegate_to_desk` may be wired: {orchestrator:?}"
    );
}

/// (c) No deferred family leaks into a dispatched belt: raw browser
/// automation, Node/NPM exec, OpenHuman sub-agent spawn tools (the
/// `subagent` namespace is reserved but empty in v1), skill *execution*,
/// the raw memory-tree tool surface, and `forget`. A negative assertion, so
/// it stays honest even as OpenHuman renames tools upstream — the pin is
/// "none of these shapes appear".
///
/// **`web_search` was removed from this list by issue #238** — and only
/// `web_search`. It was deferred for one *infrastructure* reason ("need
/// engine keys") that the managed-credential pattern dissolved; the other
/// families here were deferred for *safety* reasons that still hold. The
/// remaining `search` / `google_search` entries stay pinned, so OpenHuman's
/// broader search families cannot arrive on the back of this decision.
///
/// That `web_search` is nonetheless absent from a `*`-granted belt is not
/// this test's job any more — it is pinned deliberately by
/// [`web_search_is_wired_only_by_explicit_grant_and_credential`] and by the
/// exact snapshot in
/// [`dispatched_desk_agent_tool_belt_is_pinned`], which would both fail if
/// the wildcard ever started conferring spend.
#[test]
fn dispatched_belt_excludes_every_deferred_family() {
    let names = built_tool_names(&["*"], false);
    let forbidden = [
        // raw browser automation
        "browser",
        "browser_navigate",
        "browser_click",
        "browser_screenshot",
        // the search families still deferred (`web_search` is admitted
        // under an explicit `search` grant — issue #238)
        "search",
        "google_search",
        // Node / NPM exec
        "node",
        "npm",
        "run_node",
        "run_npm",
        // OpenHuman sub-agent spawn (subagent namespace reserved, empty v1)
        "spawn_subagent",
        "spawn_agent",
        "delegate_archivist",
        // skill execution
        "run_skill",
        "skill_run",
        "run_workflow",
        "await_workflow",
        // raw memory-tree tool surface
        "memory_tree",
        "memory_tree_search",
        "memory_tree_get",
        // destructive memory: upstream's raw `forget` stays out; the
        // scoped oc-authored `memory_forget` is a real belt tool now.
        "forget",
    ];
    for tool in forbidden {
        assert!(
            !names.contains(&tool.to_string()),
            "deferred tool `{tool}` leaked into the dispatched belt: {names:?}"
        );
    }
}
