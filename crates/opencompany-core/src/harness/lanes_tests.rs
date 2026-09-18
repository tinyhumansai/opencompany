use super::*;
use crate::ports::types::OverlayAgent;

/// The ACP session-key snapshot carries **every** routable desk.
///
/// A desk created from the console lives only in `overlay_desks`, so a
/// manifest-only snapshot left it out — and the ACP key then split its two
/// spellings into two durable sessions in the external agent, which is the
/// context loss the canonicalisation exists to prevent (coderabbit + codex
/// on #1972).
#[test]
fn the_desk_snapshot_includes_console_created_desks() {
    let mut record = record();
    record
        .manifest
        .group_chats
        .push(toml::from_str("id = \"growth_desk\"\nname = \"Growth\"").expect("a desk"));
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "ops_desk".to_string(),
        name: "Operations".to_string(),
        description: None,
        members: Vec::new(),
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });

    let desks = declared_desks(&record);
    assert!(
        desks.contains(&("growth_desk".to_string(), "Growth".to_string())),
        "the manifest desk: {desks:?}"
    );
    assert!(
        desks.contains(&("ops_desk".to_string(), "Operations".to_string())),
        "and the console-created one: {desks:?}"
    );
}

/// A two-harness company with a console-created overlay teammate.
fn record() -> CompanyRecord {
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "researcher"
role = "Researcher"
harness = "deep"

[[harness]]
id = "embedded"
kind = "built_in"
default = true

[[harness]]
id = "deep"
kind = "built_in"
"#,
    )
    .expect("valid manifest");
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: vec![OverlayAgent {
            provider: None,
            id: "writer".into(),
            name: "Writer".into(),
            role: "Content Writer".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        }],
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_tool_grants: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

/// The default lane serves the whole default-bound roster **including**
/// every overlay teammate, whose only harness is the default.
#[test]
fn the_default_lane_serves_every_overlay_agent() {
    let rec = record();
    let default = agents_on(&rec, "embedded", "embedded");
    assert!(default.contains("ceo"));
    assert!(!default.contains("researcher"), "bound to the deep lane");
    assert!(
        default.contains("writer"),
        "a console-created teammate runs on the default harness"
    );

    // And the named lane must not claim it — the overlay is nobody's but
    // the default's.
    let deep = agents_on(&rec, "deep", "embedded");
    assert!(deep.contains("researcher"));
    assert!(!deep.contains("writer"));
    assert!(!deep.contains("ceo"));
}

/// **The property the whole detected-harness design rests on** (issue
/// #1245's follow-up): a company that binds nobody to a coding CLI
/// synthesizes nothing.
///
/// Not a tidiness assertion. `HarnessBrain::run_turn` returns the plain
/// engine when `lanes` *and* `unavailable` are both empty, so folding a
/// lane per known CLI into every company would quietly take every
/// company in every deployment off that path — and on a server build
/// (no ACP factory) leave three `unavailable` entries behind as well.
#[test]
fn an_unreferenced_coding_cli_synthesizes_no_lane() {
    let rec = record();
    let declared = rec.manifest.effective_harnesses();
    assert!(
        referenced_implicit_locals(&rec, &declared, "embedded").is_empty(),
        "nobody names a coding CLI, so nothing is synthesized"
    );
}

/// A binding to a coding CLI no `[[harness]]` declares is picked up from
/// **either** roster half — a console-added teammate on a detected CLI is
/// the case the feature exists for.
#[test]
fn a_referenced_coding_cli_is_synthesized_from_either_roster_half() {
    let mut rec = record();
    rec.manifest.agents[0].harness = Some("claude".into());
    rec.overlay_agents[0].harness = Some("codex".into());
    let declared = rec.manifest.effective_harnesses();

    // Sorted and de-duplicated, so a rebuild keeps the same lane order.
    assert_eq!(
        referenced_implicit_locals(&rec, &declared, "embedded"),
        vec!["claude".to_string(), "codex".to_string()]
    );

    // And each is a `local` acp harness the ACP path can resolve.
    let synthesized = Harness::implicit_local("claude");
    assert_eq!(synthesized.kind, "acp");
    assert!(!synthesized.default, "never the company default");
    assert_eq!(synthesized.acp.expect("acp").transport, "local".to_string());
}

/// A declared harness of the same id is never shadowed by a synthesized
/// one — otherwise a company that deliberately pinned a model on its own
/// `claude` harness would get a second, bare lane of the same name.
#[test]
fn a_declared_or_default_coding_cli_is_not_synthesized_twice() {
    let mut rec = record();
    rec.manifest.agents[0].harness = Some("claude".into());

    // Declared under that exact id.
    let declared = vec![
        Harness::implicit(),
        Harness {
            id: "claude".into(),
            kind: "acp".into(),
            default: false,
            inference: None,
            acp: None,
        },
    ];
    assert!(
        referenced_implicit_locals(&rec, &declared, "embedded").is_empty(),
        "the declared harness wins"
    );

    // And when it *is* the default, which is resolved separately.
    assert!(
        referenced_implicit_locals(&rec, &[], "claude").is_empty(),
        "the default is resolved by the default path, not synthesized here"
    );
}

/// The adapter's own start-up error is a diagnostic: it can name a resolved
/// binary path or the argv it was invoked with. The reason recorded here
/// travels into the router's sentence and from there into company chat, so it
/// must carry none of it.
#[cfg(feature = "acp")]
#[test]
fn a_failing_adapter_keeps_its_own_error_out_of_the_reason() {
    const LEAK: &str = "/Users/someone/.secrets/claude --api-key=sk-live-abcdef";

    struct FailingFactory;
    impl crate::ports::acp::AcpAgentFactory for FailingFactory {
        fn build(
            &self,
            _agent: &str,
            _model: Option<&str>,
            _agent_models: &std::collections::HashMap<String, String>,
            _workspace_root: &std::path::Path,
        ) -> crate::Result<std::sync::Arc<dyn crate::ports::acp::AcpAgent>> {
            Err(crate::OpenCompanyError::Harness(format!(
                "could not start `{LEAK}`: No such file or directory"
            )))
        }
    }

    let harness = Harness::implicit_local("claude");
    let reason = match resolve_acp_engine(
        &harness,
        Some(&FailingFactory),
        std::path::Path::new("/tmp"),
        &std::collections::HashMap::new(),
        Vec::new(),
    ) {
        Ok(_) => panic!("the adapter was supposed to fail to start"),
        Err(reason) => reason,
    };

    assert!(
        !reason.contains(".secrets") && !reason.contains("sk-live"),
        "the adapter's own error must not reach the recorded reason: {reason}"
    );
    assert!(
        !reason.contains("No such file or directory"),
        "nor any part of it: {reason}"
    );
    assert!(
        reason.contains("claude"),
        "the operator still learns which adapter failed: {reason}"
    );
}
