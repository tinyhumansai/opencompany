use super::*;

/// The enabled Git path, end to end through `build_agent`: the `docs.*`
/// grant wires the sandboxed `file_write`, `workspace_git_enabled: true`
/// decorates every tool with the checkpointer, and a tool call that writes
/// the workspace yields the baseline commit plus a post-call checkpoint.
#[tokio::test]
async fn workspace_git_enabled_checkpoints_a_tool_write() {
    use crate::company::Policy;
    use serde_json::json;

    let dir = tempfile::tempdir().expect("tempdir");
    let deps = enabled_git_deps(dir.path().to_path_buf());
    let company = CompanyId::new("acme");
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
        skills: None,
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
    // `full` so the sandboxed write executes without a supervised prompt.
    let policy = ApprovalPolicy::new(
        &Policy {
            mode: "full".to_string(),
            ..Policy::default()
        },
        None,
    );
    let grants = vec!["docs.*".to_string()];
    let agent = build_agent(
        &company,
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

    let workspace = agent_workspace(&deps.workspace_root, &company, "desk");
    let write = agent
        .tools()
        .iter()
        .find(|tool| tool.name() == "file_write")
        .expect("docs.* wires file_write");
    let result = write
        .execute(json!({"path": "answer.txt", "content": "42"}))
        .await
        .expect("file_write runs");
    assert!(!result.is_error, "unexpected failure: {result:?}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("answer.txt")).unwrap(),
        "42"
    );

    let history = git_log(&workspace);
    assert!(
        history.contains("checkpoint: initialize workspace"),
        "{history}"
    );
    assert!(
        history.contains("checkpoint: after file_write"),
        "{history}"
    );
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_registry_list_tools_is_withheld_from_an_agent_granted_nothing() {
    let names = built_tool_names(&[], false);
    assert!(
        !names.contains(&"mcp_registry_list_tools".to_string()),
        "an agent holding no grant must not receive the MCP registry reader: {names:?}"
    );
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_registry_tool_call_is_withheld_from_an_agent_granted_nothing() {
    let names = built_tool_names(&[], false);
    assert!(
        !names.contains(&"mcp_registry_tool_call".to_string()),
        "an agent holding no grant must not receive the MCP registry invoker: {names:?}"
    );
}

#[cfg(feature = "mcp")]
#[test]
fn a_docs_only_agent_receives_no_mcp_registry_tool() {
    let names = built_tool_names(&["docs.*"], false);
    for tool in ["mcp_registry_list_tools", "mcp_registry_tool_call"] {
        assert!(
            !names.contains(&tool.to_string()),
            "`docs.*` must not confer `{tool}`: {names:?}"
        );
    }
}

#[cfg(feature = "mcp")]
#[test]
fn no_configured_mcp_server_wires_no_server_backed_mcp_tool() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = pin_deps(dir.path().to_path_buf());
    assert!(
        deps.mcp_servers.is_empty(),
        "this test's premise is a company with no configured server"
    );
    assert!(
        crate::harness::mcp::registry_for_agent(&deps.mcp_servers, &["*".to_string()]).is_none(),
        "no configured server must yield no registry, even under `*`"
    );

    let names = built_tool_names(&["*"], false);
    for tool in ["mcp_list_servers", "mcp_list_tools", "mcp_call"] {
        assert!(
            !names.contains(&tool.to_string()),
            "`{tool}` must not be wired without a configured server: {names:?}"
        );
    }
}

/// The tool-iteration ceiling is one crate-wide constant with no per-agent
/// lever: a tier hint, a declared daily budget and the orchestrator flag all
/// build agents that run on exactly [`MAX_TOOL_ITERATIONS`]. An agent that
/// needs a longer loop has no way to ask for one, and — the direction that
/// matters — no manifest field can raise its own ceiling.
#[test]
fn the_tool_iteration_cap_is_uniform_and_not_manifest_configurable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deps = pin_deps(dir.path().to_path_buf());

    let build_with = |tier: Option<&str>, budget: Option<f64>, is_orchestrator: bool| {
        let manifest_agent = ManifestAgent {
            provider: None,
            global: false,
            id: "desk".to_string(),
            role: "Desk Lead".to_string(),
            name: None,
            description: None,
            tier: tier.map(str::to_string),
            harness: None,
            tools: None,
            skills: None,
            delegates_to: Vec::new(),
            context: None,
            budget_usd_daily: budget,
            prompt: None,
            prompt_files: Vec::new(),
            prompt_files_resolved: Vec::new(),
            classes: Vec::new(),
            ledgers: None,
            can_declare_ledgers: true,
            model: None,
        };
        build_agent(
            &CompanyId::new("acme"),
            "Acme",
            &manifest_agent,
            std::sync::Arc::new(ApprovalPolicy::new(&Policy::default(), None)),
            &deps,
            &["*".to_string()],
            &[],
            &[],
            None,
            is_orchestrator,
        )
        .expect("agent builds")
        .max_tool_iterations()
    };

    for (label, got) in [
        ("no tier", build_with(None, None, false)),
        ("deep tier", build_with(Some("deep"), None, false)),
        ("fast tier", build_with(Some("fast"), None, false)),
        ("budgeted", build_with(None, Some(500.0), false)),
        ("orchestrator", build_with(None, None, true)),
    ] {
        assert_eq!(
            got, MAX_TOOL_ITERATIONS,
            "`{label}` must run on the one stated ceiling, not its own"
        );
    }
}
