//! Channel, skill, connection, MCP and inference manifest tests, plus the
//! effective-summary and `discover` surfaces (split out of
//! `manifest_tests.rs`).

use super::*;

fn parse(text: &str) -> CompanyManifest {
    toml::from_str(text).expect("valid toml")
}

#[test]
fn rejects_unknown_channel_and_bad_tier() {
    let manifest = parse(
        r#"
        [company]
        name = "X"
        [[agent]]
        id = "a"
        role = "A"
        tier = "genius"
        [channels.telepathy]
        enabled = true
        "#,
    );
    let problems = manifest.validate();
    assert!(problems.iter().any(|p| p.contains("telepathy")));
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`tier`") && p.contains("genius"))
    );
}

/// tiny.place is gone, but manifests written for it are not: a
/// `company.toml` — or a persisted company record — may still carry
/// `[company].handle` and a `[place]` table. Both are ignored, not refused,
/// and nothing about them is a validation problem any more: a public company
/// with no handle, or a skill priced `"free"`, used to be one.
#[test]
fn retired_tiny_place_keys_are_ignored_not_refused() {
    let manifest = parse(
        r#"
        [company]
        name = "X"
        handle = "x"
        [place]
        discoverable = true
        skills = [
            { id = "seo.audit", price_usd = "free" },
            { id = "seo.audit", price_usd = "-5.00" },
        ]
        "#,
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    assert!(!manifest.effective_summary().contains("Discover:"));

    let unhandled = parse("[company]\nname = \"X\"\n[place]\ndiscoverable = true\n");
    assert!(
        unhandled.validate().is_empty(),
        "{:?}",
        unhandled.validate()
    );
}

#[test]
fn rejects_a_bad_cron() {
    let manifest = parse(
        r#"
        [company]
        name = "X"
        [[schedule]]
        cron = "every monday"
        prompt = "review"
        "#,
    );
    let problems = manifest.validate();
    assert!(problems.iter().any(|p| p.contains("5 fields")));
}

#[test]
fn accepts_group_chats_connections_and_workflows() {
    let manifest = parse(
        r#"
        [company]
        name = "Agentic Marketing Agency"

        [[agent]]
        id = "creative_director"
        role = "Creative Director"
        [[agent]]
        id = "copywriter"
        role = "Copywriter"

        [[group_chat]]
        id = "creative"
        name = "Creative studio"
        description = "Copy, design, and campaigns"
        members = ["creative_director", "copywriter"]

        [[connection]]
        provider = "slack"
        priority = "high"
        scopes = ["chat:write"]
        reason = "Post campaign updates"

        [workflows]
        enabled = ["campaign_pipeline"]
        "#,
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    assert_eq!(manifest.group_chats.len(), 1);
    assert_eq!(manifest.group_chats[0].members.len(), 2);
    assert_eq!(manifest.connections[0].provider, "slack");
    assert_eq!(manifest.workflows.enabled, vec!["campaign_pipeline"]);
}

#[test]
fn rejects_unknown_member_bad_priority_and_workflow_id() {
    let manifest = parse(
        r#"
        [company]
        name = "X"
        [[agent]]
        id = "a"
        role = "A"

        [[group_chat]]
        id = "team"
        name = "Team"
        members = ["ghost"]

        [[connection]]
        provider = "slack"
        priority = "urgent"

        [workflows]
        enabled = ["Bad-Id"]
        "#,
    );
    let problems = manifest.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("ghost") && p.contains("not an agent")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`priority`") && p.contains("urgent")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("workflow id") && p.contains("Bad-Id")),
        "{problems:?}"
    );
}

/// A bundle laying an `mcp.json` beside its `company.toml` gets those
/// servers, and they are held to the same validator an inline entry is.
#[test]
fn a_bundle_mcp_json_reaches_the_manifest() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(MANIFEST_FILE), "[company]\nname = \"X\"\n")
        .expect("write manifest");
    std::fs::write(
        dir.path().join("mcp.json"),
        r#"{"mcpServers": {"deepwiki": {"url": "https://mcp.deepwiki.com/mcp"}}}"#,
    )
    .expect("write mcp.json");

    let manifest = CompanyManifest::from_path(dir.path()).expect("loads");
    let names: Vec<&str> = manifest
        .mcp_servers
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["deepwiki"]);
}

/// A server declared in both forms is refused rather than resolved by
/// precedence — the roster's rule, for the roster's reason: either
/// precedence rule silently discards a declaration somebody wrote down.
#[test]
fn a_server_declared_in_both_forms_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(MANIFEST_FILE),
        "[company]\nname = \"X\"\n[[mcp_server]]\nname = \"deepwiki\"\nendpoint = \"https://one.test/mcp\"\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.path().join("mcp.json"),
        r#"{"mcpServers": {"deepwiki": {"url": "https://two.test/mcp"}}}"#,
    )
    .expect("write mcp.json");

    let err = CompanyManifest::from_path(dir.path()).expect_err("must refuse");
    let text = err.to_string();
    assert!(text.contains("deepwiki"), "{text}");
}

/// A bad entry in `mcp.json` is reported against the manifest rather than
/// swallowed — the file is genuinely read, and its problems genuinely land.
#[test]
fn a_bad_bundle_server_is_reported_against_the_manifest() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(MANIFEST_FILE), "[company]\nname = \"X\"\n")
        .expect("write manifest");
    std::fs::write(
        dir.path().join("mcp.json"),
        r#"{"mcpServers": {"local": {"command": "npx some-mcp"}}}"#,
    )
    .expect("write mcp.json");

    let err = CompanyManifest::from_path(dir.path()).expect_err("must refuse");
    let text = err.to_string();
    assert!(
        text.contains("stdio") && text.contains("mcp.json"),
        "{text}"
    );
}

#[test]
fn accepts_http_mcp_server_and_rejects_stdio() {
    let ok = parse(
        r#"
        [company]
        name = "X"
        [[mcp_server]]
        name = "notion"
        endpoint = "https://notion.example/mcp"
        "#,
    );
    assert!(ok.validate().is_empty(), "{:?}", ok.validate());

    let bad = parse(
        r#"
        [company]
        name = "X"
        [[mcp_server]]
        name = "local"
        command = "npx some-mcp"
        "#,
    );
    let problems = bad.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("stdio") && p.contains("hosted v1")),
        "{problems:?}"
    );
}

#[test]
fn accepts_byok_inference_and_rejects_bad_provider() {
    let ok = parse(
        r#"
        [company]
        name = "X"
        [inference]
        provider = "openrouter"
        [inference.models]
        "chat-v1" = "deepseek/deepseek-chat"
        "#,
    );
    assert!(ok.validate().is_empty(), "{:?}", ok.validate());
    assert_eq!(ok.inference.provider.as_deref(), Some("openrouter"));
    assert_eq!(
        ok.inference.models.get("chat-v1").map(String::as_str),
        Some("deepseek/deepseek-chat")
    );

    let bad = parse(
        r#"
        [company]
        name = "X"
        [inference]
        provider = "ollama"
        "#,
    );
    // Ollama needs a base_url.
    assert!(
        bad.validate()
            .iter()
            .any(|p| p.contains("base_url") && p.contains("required")),
        "{:?}",
        bad.validate()
    );
}

#[test]
fn effective_summary_lists_roster() {
    let manifest = parse(
        r#"
        [company]
        name = "Agentic Marketing Agency"
        [[agent]]
        id = "copywriter"
        role = "Copywriter"
        "#,
    );
    let summary = manifest.effective_summary();
    assert!(summary.contains("Agentic Marketing Agency"));
    assert!(summary.contains("copywriter"));
    assert!(summary.contains("Roster (1)"));
}

#[test]
fn signals_opportunity_studio_template_passes_lint() {
    // The Signals + Opportunity Engine ship as a venture-studio template,
    // not kernel code. This guards that the shipped manifest keeps passing
    // the same lint `opencompany check` runs — unique agent ids, priced +
    // described `[place].skills`, a `[policy]`, and a stated `human_role`.
    // The company *directory*, not its `company.toml`: this template's
    // roster lives in `agents/*.toml`, and loading the file alone would
    // leave `manifest.agents` empty — making every roster assertion below
    // pass by having nothing to check.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../companies/signals_opportunity_studio");
    let manifest = CompanyManifest::from_path(&path).expect("template manifest is valid");

    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    assert!(
        !manifest.agents.is_empty(),
        "the roster must actually load, or the assertions below check nothing"
    );
    assert!(
        manifest.company.human_role.is_some(),
        "the template must name what the human keeps"
    );
    // Unique agent ids.
    let mut ids: Vec<&str> = manifest.agents.iter().map(|a| a.id.as_str()).collect();
    ids.sort_unstable();
    let unique = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), unique, "agent ids must be unique");
    // A supervised policy with a defined always-approve fence. Asserting
    // only `!is_empty()` is what let the template ship three entries that
    // matched nothing on its harness path (issue #684): a list's length
    // says nothing about whether it fires.
    assert_eq!(manifest.policy.mode, "supervised");
    assert!(!manifest.policy.always_approve.is_empty());
    // What was actually wrong is that none of the old entries named a tool,
    // and the template runs the openhuman harness. A shipped template must
    // demonstrate a gate that works on its own path, not merely a plausible
    // effect-kind string.
    assert!(
        crate::policy::always_approve::matches(&manifest.policy.always_approve, "publish_artifact"),
        "the template's fence names no declared tool, so nothing in it can \
         park a harness tool call — the shape of issue #684"
    );
    // The weekly opportunity loop is a schedule.
    assert!(!manifest.schedules.is_empty());
}

#[test]
fn discover_prefers_company_toml() {
    let dir = std::env::temp_dir().join(format!("oc-discover-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(LEGACY_MANIFEST_FILE), "[company]\nname=\"L\"\n").unwrap();
    let located = discover(&dir).unwrap();
    assert!(located.legacy);
    std::fs::write(dir.join(MANIFEST_FILE), "[company]\nname=\"C\"\n").unwrap();
    let located = discover(&dir).unwrap();
    assert!(!located.legacy);
    std::fs::remove_dir_all(&dir).ok();
}

// --- `[group_chat.routing]` (plan hive-desks, Phase 4) ---------------------

#[test]
fn a_routing_block_parses_and_its_zero_keys_are_refused() {
    let manifest = parse(
        r#"
        [company]
        name = "X"
        [[agent]]
        id = "a"
        role = "A"
        [[agent]]
        id = "b"
        role = "B"
        [[group_chat]]
        id = "desk"
        name = "Desk"
        members = ["a", "b"]
        [group_chat.routing]
        round_width = 2
        max_rounds = 4
        [group_chat.routing.referral]
        enabled = true
        max_hops = 1
        returns = true
        "#,
    );
    assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    let desk = &manifest.group_chats[0];
    assert_eq!(desk.hive.round_width, Some(2));
    assert_eq!(desk.hive.max_rounds, Some(4));
    assert_eq!(
        desk.hive
            .referral
            .as_ref()
            .and_then(|referral| referral.max_hops),
        Some(1)
    );
    // Round-trips under the `routing` key, never `hive`.
    let rendered = toml::to_string(&manifest).expect("serializes");
    assert!(rendered.contains("[group_chat.routing]"), "{rendered}");
    assert!(!rendered.contains("[group_chat.hive]"), "{rendered}");

    let broken = parse(
        r#"
        [company]
        name = "X"
        [[agent]]
        id = "a"
        role = "A"
        [[group_chat]]
        id = "desk"
        name = "Desk"
        members = ["a"]
        [group_chat.routing]
        round_width = 0
        turn_timeout_secs = 0
        "#,
    );
    let problems = broken.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`routing.round_width = 0`")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`routing.turn_timeout_secs = 0`")),
        "{problems:?}"
    );
}

#[test]
fn a_stale_hive_block_is_refused_with_a_migration_hint() {
    let text = r#"
        [company]
        name = "X"
        [[agent]]
        id = "a"
        role = "A"
        [[group_chat]]
        id = "desk"
        name = "Desk"
        members = ["a"]
        [group_chat.hive]
        quorum = 2
        "#;
    let problem = CompanyManifest::legacy_hive_block(text).expect("refused");
    assert!(problem.contains("group chat `desk`"), "{problem}");
    assert!(problem.contains("[group_chat.routing]"), "{problem}");
    assert!(problem.contains("docs/spec/runtime/hive.md"), "{problem}");
    let dir = std::env::temp_dir().join(format!(
        "oc-stale-hive-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("company.toml");
    std::fs::write(&path, text).unwrap();
    let err = CompanyManifest::from_file(&path).expect_err("a stale block does not load");
    assert!(err.to_string().contains("[group_chat.routing]"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        CompanyManifest::legacy_hive_block("[company]\nname = \"X\"\n[[group_chat]]\nid = \"d\"\nname = \"D\"\n[group_chat.routing]\nround_width = 1\n").is_none()
    );
}
