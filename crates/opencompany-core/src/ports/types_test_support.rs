use super::*;

pub(super) fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de>,
{
    let json = serde_json::to_string(value).expect("serialize");
    serde_json::from_str(&json).expect("deserialize")
}

pub(super) fn desk_record(toml_src: &str, overlay: Vec<OverlayDeskMember>) -> CompanyRecord {
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: toml::from_str(toml_src).expect("parse manifest"),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
        overlay_desk_members: overlay,
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

pub(super) fn mint_record() -> CompanyRecord {
    desk_record(
        "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"backend_engineer\"\nrole = \"Backend Engineer\"\n",
        Vec::new(),
    )
}

pub(super) fn add_overlay(record: &mut CompanyRecord, id: &str, name: &str) {
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: id.into(),
        name: name.into(),
        role: "Worker".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
}

pub(super) const BUDGET_ROSTER: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\nbudget_usd_daily = 5.0\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n";

pub(super) fn budget_entry(agent_id: &str, cap: Option<f64>) -> BudgetOverride {
    BudgetOverride {
        agent_id: agent_id.to_string(),
        budget_usd_daily: cap,
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-1".to_string(),
        },
        at_millis: 1_700_000_000_000,
    }
}

pub(super) const POLICY_MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
         [policy]\nmode = \"supervised\"\n\
         always_approve = [\"payment.send\", \"filing.submit\"]\n";

pub(super) fn policy_entry(mode: Option<&str>, always: Option<Vec<&str>>) -> PolicyOverride {
    PolicyOverride {
        mode: mode.map(str::to_string),
        always_approve: always.map(|v| v.into_iter().map(str::to_string).collect()),
        auto_approve_under_usd: None,
        approval_ttl_hours: None,
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-1".to_string(),
        },
        at_millis: 1_700_000_000_000,
    }
}
