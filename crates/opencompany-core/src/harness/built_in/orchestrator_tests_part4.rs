use super::*;

#[tokio::test]
async fn query_company_tool_reports_no_data_when_unwired() {
    let tool = QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, None, None);
    let result = tool.execute(json!({})).await.expect("execute");
    // The insight surface lives in the markdown; `output()` is the summary.
    let out = result.output_for_llm(true);
    assert!(out.contains("No durable facts recorded"), "{out}");
    assert!(out.contains("No recent activity"), "{out}");
    // Not "no saved workflows" any more: the global baseline ships graphs
    // every company has, wired store or not.
    for workflow in crate::globals::workflows() {
        assert!(out.contains(&workflow.id), "{out}");
    }
}

/// Regression: a saved workflow (on disk) and an operator-added overlay
/// teammate both show up in `query_company`. Before this the orchestrator
/// had no way to enumerate either, so a freshly created workflow / added
/// teammate looked unpersisted when the operator asked about it.
#[tokio::test]
async fn query_company_tool_lists_saved_workflows_and_roster() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("workflows")).unwrap();
    std::fs::write(
        dir.path().join("workflows").join("daily-standup.toml"),
        r#"
id = "daily-standup"
name = "Daily Standup"
description = "Morning summary."
[[node]]
id = "start"
kind = "trigger"
name = "Morning"
"#,
    )
    .unwrap();

    // A record whose overlay adds a teammate — the `add_agent`
    // persistence shape.
    let mut record = seeded_record(&CompanyId::new("acme"));
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "fact-fetcher".to_string(),
        name: "Fact Fetcher".to_string(),
        role: "Researcher".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(record));

    let tool = QueryCompanyTool::new(
        CompanyId::new("acme"),
        None,
        None,
        Some(dir.path().to_path_buf()),
        Some(store),
        None,
    );
    let out = tool
        .execute(json!({}))
        .await
        .expect("execute")
        .output_for_llm(true);

    assert!(out.contains("Daily Standup"), "workflow missing: {out}");
    assert!(out.contains("daily-standup"), "workflow id missing: {out}");
    assert!(
        out.contains("Fact Fetcher"),
        "overlay teammate name missing: {out}"
    );
}

/// Every roster row leads with the name a person knows and carries the id the
/// delegation tools ground, and that printed id resolves.
#[tokio::test]
async fn query_company_lists_a_teammate_under_the_id_delegation_grounds() {
    let mut record = seeded_record(&CompanyId::new("acme"));
    let id = record.mint_agent_id("Dana Designer");
    assert_eq!(id, "dana_designer", "the id and the name must differ");
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: id.clone(),
        name: "Dana Designer".to_string(),
        role: "Designer".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(record.clone()));

    let out = QueryCompanyTool::new(CompanyId::new("acme"), None, None, None, Some(store), None)
        .execute(json!({}))
        .await
        .expect("execute")
        .output_for_llm(true);

    let line = out
        .lines()
        .find(|line| line.contains("Designer"))
        .unwrap_or_else(|| panic!("no teammate line: {out}"));
    assert!(
        line.starts_with("- **Dana Designer**, Designer"),
        "the row must lead with the name: {line}"
    );
    let printed = line
        .split_once("(id `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("no id for tool calls: {line}"));
    assert_eq!(printed, id, "{line}");
    assert_eq!(
        record.resolve_teammate_key(printed),
        crate::ports::types::TeammateResolution::Agent(id),
        "the roster printed a token delegation cannot ground: {line}"
    );
}

/// Issue #272: `query_company` is the grounding surface the orchestrator is
/// told to consult, but it listed the roster and not the **desks** — so an
/// orchestrator about to delegate had no authoritative id to read and
/// reached for a teammate's name instead. Every desk is listed by the id
/// `delegate_to_desk` takes, with its lead, and a desk nobody leads says so.
#[tokio::test]
async fn query_company_tool_lists_the_desks_delegation_accepts() {
    let company = CompanyId::new("acme");
    let store: Arc<dyn CompanyStore> = Arc::new(MemStore::seeded(desks_record(&company)));
    let tool = QueryCompanyTool::new(company, None, None, None, Some(store), None);
    let out = tool
        .execute(json!({}))
        .await
        .expect("execute")
        .output_for_llm(true);

    assert!(out.contains("## Desks"), "{out}");
    let row = |id: &str| {
        out.lines()
            .find(|line| line.contains(&format!("(id `{id}` for tool calls)")))
            .unwrap_or_else(|| panic!("no row for `{id}`: {out}"))
            .to_string()
    };
    let strategy = row("strategy");
    assert!(
        strategy.starts_with("- **") && !strategy.starts_with("- **strategy**"),
        "a desk row leads with its name: {strategy}"
    );
    assert!(
        strategy.contains("lead: ") && !strategy.contains("lead: writer"),
        "the lead is named, not given by id: {strategy}"
    );
    assert!(
        row("archive").contains("no member on the roster"),
        "a leadless desk must say it cannot be handed work: {out}"
    );
}

#[tokio::test]
async fn add_agent_tool_persists_an_overlay_teammate() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({
            "name": "Jamie",
            "role": "Growth Lead",
            "description": "Owns acquisition experiments."
        }))
        .await
        .expect("execute");
    assert!(!result.is_error, "add_agent should succeed");

    let record = store
        .load(&company)
        .await
        .unwrap()
        .expect("record persisted");
    assert_eq!(record.overlay_agents.len(), 1);
    let added = &record.overlay_agents[0];
    assert_eq!(added.name, "Jamie");
    assert_eq!(added.role, "Growth Lead");
    assert_eq!(
        added.description.as_deref(),
        Some("Owns acquisition experiments.")
    );
    assert!(!added.id.is_empty(), "a stable id must be minted");
    // No `tools` given → inherit the standard company-wide grant, which for
    // an unscoped minter is `None` (keeps tracking `[tools].allow`), NOT an
    // explicit empty list (which since #1804 is a deny-all).
    assert!(
        added.tools.is_none(),
        "an add with no `tools` inherits the standard grant (None), not an empty deny-all shelf"
    );
}

/// Issue #619: a teammate minted by a **scoped** agent inherits that
/// agent's line, not the company's whole grant.
///
/// #661 clamped an explicit `tools` argument to the company grant, which
/// leaves this open: omitting `tools` still yields the company's *entire*
/// grant, so a narrowly scoped agent could mint a teammate holding
/// everything the company holds. `add_agent` is `Reach::Nothing` and never
/// asks, so nothing else in the path would catch it.
#[tokio::test]
async fn a_minted_teammate_is_bounded_by_its_minter_not_the_company() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = scoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "got {:?}", result.text());

    let record = store.load(&company).await.unwrap().expect("persisted");
    assert_eq!(
        record.overlay_agents[0].tools,
        Some(vec!["workspace".to_string()]),
        "the minted teammate must be bounded by the agent that minted it, \
         not by the company"
    );
}

/// An **unscoped** minter still mints an unscoped teammate — the pre-#619
/// behaviour, kept deliberately. Copying the minter's *line* rather than
/// its resolved grant is what keeps the teammate tracking `[tools].allow`
/// instead of freezing today's copy of it into the record.
#[tokio::test]
async fn an_unscoped_minter_mints_an_unscoped_teammate() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "got {:?}", result.text());

    let record = store.load(&company).await.unwrap().expect("persisted");
    assert!(
        record.overlay_agents[0].tools.is_none(),
        "an absent line (None) means the company's standard grant (#264/#1804), \
         and an unscoped minter hands on exactly it — None, not an empty deny-all"
    );
}

/// An explicit `tools` request is narrowed against what the **minter**
/// holds, so the tool cannot hand out a grant its caller does not have.
#[tokio::test]
async fn an_explicit_scope_is_narrowed_to_what_the_minter_holds() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = scoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({
            "name": "Jamie",
            "role": "Growth Lead",
            "tools": ["workspace", "composio"]
        }))
        .await
        .expect("execute");
    assert!(!result.is_error, "got {:?}", result.text());

    let record = store.load(&company).await.unwrap().expect("persisted");
    assert_eq!(
        record.overlay_agents[0].tools,
        Some(vec!["workspace".to_string()]),
        "`composio` is outside the minter's own grant and must be dropped"
    );
}

/// A request that narrows to **nothing** is a refusal, not a stored empty
/// list.
///
/// This is the sharp edge: an empty `tools` list means "inherit the
/// company's standard grant". Storing the empty result of a narrowing
/// would turn the most deliberate narrowing an agent can ask for into the
/// widest grant in the company — the exact inversion #619 exists to remove.
#[tokio::test]
async fn a_scope_entirely_outside_the_minters_grant_is_refused() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = scoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({
            "name": "Jamie",
            "role": "Growth Lead",
            "tools": ["composio"]
        }))
        .await
        .expect("execute");
    assert!(result.is_error, "got {:?}", result.text());

    let record = store.load(&company).await.unwrap().expect("persisted");
    assert!(
        record.overlay_agents.is_empty(),
        "and no teammate was written at all, scoped or otherwise"
    );
}

/// Issue #661 / L5: `add_agent` carries a per-teammate tool grant onto the
/// overlay record, trimming and dropping blank globs. The grant is narrowed
/// against `[tools].allow` later (at roster build); persistence keeps the
/// authored list verbatim so the Team tab and the roster read the same thing.
#[tokio::test]
async fn add_agent_tool_persists_a_tool_grant() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({
            "name": "Ravi",
            "role": "Researcher",
            "tools": ["docs.*", "   ", "email"]
        }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.text());

    let record = store.load(&company).await.unwrap().expect("record");
    assert_eq!(
        record.overlay_agents[0].tools,
        Some(vec!["docs.*".to_string(), "email".to_string()]),
        "blanks are dropped and globs trimmed"
    );
}

/// Since issue #1804 an explicit empty `tools` array is a deliberate
/// **deny-all**, NOT the standard grant — the contract inversion. Omitting
/// the field entirely is what inherits the standard grant (`None`); passing
/// `[]` deliberately hands the teammate no tools, stored as `Some(vec![])`.
#[tokio::test]
async fn add_agent_tool_empty_tools_is_an_explicit_deny_all() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({ "name": "Ravi", "role": "Researcher", "tools": [] }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.text());
    assert!(
        result.text().contains("hold no tools"),
        "the mint result must state the deny-all plainly: {}",
        result.text()
    );

    let record = store.load(&company).await.unwrap().expect("record");
    assert_eq!(
        record.overlay_agents[0].tools,
        Some(Vec::new()),
        "an explicit empty array is a deny-all (Some(vec![])), not the standard grant (None)"
    );
}

/// A minter whose own line names `chargebee` (the shipped bookkeeper) hands
/// that line on when `tools` is omitted — but an unstated grant never
/// confers billing (#788/#789), so the copied line is filtered before it is
/// stored. The #619 copy-the-line rule still holds for the non-BYO parts.
#[tokio::test]
async fn an_unstated_mint_from_a_billing_holding_minter_withholds_chargebee() {
    let company = CompanyId::new("acme");
    let mut record = seeded_record(&company);
    record.manifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\", \"chargebee\"]\n",
    )
    .expect("valid manifest");
    let store = Arc::new(MemStore::seeded(record));
    let belt = vec![
        "*".to_string(),
        "workspace.*".to_string(),
        "workspace.write".to_string(),
        "media".to_string(),
        "composio".to_string(),
        "search".to_string(),
        "mcp:*".to_string(),
        "chargebee".to_string(),
    ];
    let tool = AddAgentTool::new(
        company.clone(),
        store.clone(),
        "bookkeeper".to_string(),
        Some(belt.clone()),
        belt,
    );

    let result = tool
        .execute(json!({ "name": "Jamie", "role": "Data Entry" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.text());

    let record = store.load(&company).await.unwrap().expect("persisted");
    let added = &record.overlay_agents[0];
    assert!(
        !added
            .tools
            .iter()
            .flatten()
            .any(|g| g == "chargebee" || g.starts_with("chargebee.")),
        "an unstated mint must not hand on billing: {:?}",
        added.tools
    );
    assert!(
        added.tools.iter().flatten().any(|g| g == "*"),
        "the rest of the minter's line is still copied verbatim (#619): {:?}",
        added.tools
    );
}

/// An EXPLICIT `tools` request naming `chargebee` survives — an unstated
/// grant is withheld, a stated one is narrowed to what the minter holds.
#[tokio::test]
async fn an_explicit_chargebee_request_from_a_billing_minter_is_honored() {
    let company = CompanyId::new("acme");
    let mut record = seeded_record(&company);
    record.manifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [tools]\n\
         allow = [\"*\", \"workspace.*\", \"workspace.write\", \"media\", \"composio\", \
         \"search\", \"mcp:*\", \"chargebee\"]\n",
    )
    .expect("valid manifest");
    let store = Arc::new(MemStore::seeded(record));
    let belt = vec![
        "*".to_string(),
        "workspace.*".to_string(),
        "workspace.write".to_string(),
        "media".to_string(),
        "composio".to_string(),
        "search".to_string(),
        "mcp:*".to_string(),
        "chargebee".to_string(),
    ];
    let tool = AddAgentTool::new(
        company.clone(),
        store.clone(),
        "bookkeeper".to_string(),
        Some(belt.clone()),
        belt,
    );

    let result = tool
        .execute(json!({ "name": "Jamie", "role": "Data Entry", "tools": ["chargebee"] }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.text());

    let record = store.load(&company).await.unwrap().expect("persisted");
    assert_eq!(
        record.overlay_agents[0].tools,
        Some(vec!["chargebee".to_string()]),
        "a stated billing namespace is narrowed to the minter's grant, not dropped"
    );
}

/// A non-string `tools` item is a clean argument error, the same shape as a
/// missing `name`/`role` — a malformed grant must not persist a half-parsed
/// teammate.
#[tokio::test]
async fn add_agent_tool_rejects_a_non_string_tool() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    assert!(
        tool.execute(json!({ "name": "Ravi", "role": "Researcher", "tools": [123] }))
            .await
            .is_err(),
        "a non-string tool glob must be rejected"
    );
    // Also rejects a non-array `tools`.
    assert!(
        tool.execute(json!({ "name": "Ravi", "role": "Researcher", "tools": "docs.*" }))
            .await
            .is_err(),
        "a non-array `tools` must be rejected"
    );

    let record = store.load(&company).await.unwrap().expect("record");
    assert!(
        record.overlay_agents.is_empty(),
        "a rejected add must not persist a teammate"
    );
}

/// Issue #686 — the tool mints the same readable, name-derived id the
/// console route does, and hands it back in the result so the orchestrator
/// can delegate to the teammate it just created.
#[tokio::test]
async fn add_agent_tool_mints_a_readable_id_and_reports_it() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({ "name": "Dana Designer", "role": "Designer" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.text());
    assert!(
        result.text().contains("`dana_designer`"),
        "the id must be in the result, not only in the record: {}",
        result.text()
    );

    let record = store.load(&company).await.unwrap().expect("record");
    assert_eq!(record.overlay_agents[0].id, "dana_designer");
}

/// The name guard still fires, and it fires *before* minting — so a
/// duplicate display name is refused rather than quietly given a `_2` id.
/// Two teammates the orchestrator cannot tell apart is the thing that guard
/// exists to stop, and readable ids do not make it less true.
#[tokio::test]
async fn add_agent_tool_still_refuses_a_duplicate_display_name() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    for _ in 0..1 {
        let first = tool
            .execute(json!({ "name": "Dana Designer", "role": "Designer" }))
            .await
            .expect("execute");
        assert!(!first.is_error, "{}", first.text());
    }

    let second = tool
        .execute(json!({ "name": "dana designer", "role": "Illustrator" }))
        .await
        .expect("execute");
    assert!(second.is_error, "{}", second.text());
    assert!(
        second.text().contains("already exists"),
        "{}",
        second.text()
    );

    let record = store.load(&company).await.unwrap().expect("record");
    assert_eq!(
        record.overlay_agents.len(),
        1,
        "the refusal must not have persisted a `dana_designer_2`"
    );
}

/// A name colliding with a **manifest** agent's id passes the name guard —
/// it compares overlay names — and is caught by the minter instead. The
/// roster-level consequence is pinned in `harness::tests`.
#[tokio::test]
async fn add_agent_tool_suffixes_past_a_manifest_agent_id() {
    let company = CompanyId::new("acme");
    let mut record = seeded_record(&company);
    record.manifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"backend_engineer\"\nrole = \"Backend Engineer\"\n",
    )
    .expect("valid manifest");
    let store = Arc::new(MemStore::seeded(record));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    let result = tool
        .execute(json!({ "name": "Backend Engineer", "role": "Platform" }))
        .await
        .expect("execute");
    assert!(!result.is_error, "{}", result.text());

    let record = store.load(&company).await.unwrap().expect("record");
    assert_eq!(record.overlay_agents[0].id, "backend_engineer_2");
}

#[tokio::test]
async fn add_agent_tool_requires_name_and_role() {
    let company = CompanyId::new("acme");
    let store = Arc::new(MemStore::seeded(seeded_record(&company)));
    let tool = unscoped_add_agent(company.clone(), store.clone());

    assert!(
        tool.execute(json!({ "role": "Growth Lead" }))
            .await
            .is_err(),
        "missing `name` must be rejected"
    );
    assert!(
        tool.execute(json!({ "name": "Jamie" })).await.is_err(),
        "missing `role` must be rejected"
    );
    let record = store.load(&company).await.unwrap().expect("record");
    assert!(
        record.overlay_agents.is_empty(),
        "a rejected call must not persist a half-formed teammate"
    );
}

#[tokio::test]
async fn add_agent_tool_reports_company_not_found() {
    let company = CompanyId::new("ghost");
    let store: Arc<dyn CompanyStore> = Arc::new(MemStore::default());
    let tool = unscoped_add_agent(company, store);

    let err = tool
        .execute(json!({ "name": "Jamie", "role": "Growth Lead" }))
        .await
        .expect_err("no record for this company id");
    assert!(err.to_string().contains("ghost"), "{err}");
}
