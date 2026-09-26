use super::*;
use crate::company::CompanyManifest;
use crate::ports::types::{CompanyId, OverlayAgent, OverlayDeskMember};

fn record(manifest: &str) -> CompanyRecord {
    let manifest: CompanyManifest = toml::from_str(manifest).expect("valid manifest");
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        overlay_agents: Vec::new(),
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

fn acme() -> CompanyRecord {
    record(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "eng"
name = "Engineering desk"
members = ["engineer"]

[[group_chat]]
id = "empty"
name = "Nobody home"
members = []
"#,
    )
}

/// Blank is not an error — it is the ordinary unassigned card, and the
/// caller's default responder takes it.
#[test]
fn blank_is_unassigned_not_unknown() {
    let record = acme();
    assert_eq!(resolve(&record, ""), AssigneeResolution::Unassigned);
    assert_eq!(resolve(&record, "   "), AssigneeResolution::Unassigned);
    assert!(resolve(&record, "").rejection().is_none());
    assert!(resolve(&record, "").working_agent().is_none());
}

/// A manifest teammate resolves to itself, case-folded to the canonical id.
#[test]
fn a_manifest_teammate_resolves_case_insensitively() {
    let record = acme();
    assert_eq!(
        resolve(&record, "engineer"),
        AssigneeResolution::Agent("engineer".into())
    );
    assert_eq!(
        resolve(&record, "Engineer"),
        AssigneeResolution::Agent("engineer".into()),
        "a typed capital must not read as an unknown agent"
    );
    assert_eq!(
        resolve(&record, " engineer ").working_agent(),
        Some("engineer"),
        "the board's text field keeps whatever whitespace was typed"
    );
}

/// The narrow-lookup half of #205: an operator-added overlay teammate is a
/// roster teammate, and used to fall through to the orchestrator.
#[test]
fn an_overlay_teammate_resolves() {
    let mut record = acme();
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "nova".into(),
        name: "Nova".into(),
        role: "Growth".into(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    });
    assert_eq!(
        resolve(&record, "nova"),
        AssigneeResolution::Agent("nova".into())
    );
}

/// A desk resolves to its lead — by id and by (case-insensitive) name, the
/// two keys `resolve_desk_id` already accepts. `delegate_to_desk` writes a
/// desk id into `assignee`, so this is a live shape, not a hypothetical.
#[test]
fn a_desk_resolves_to_its_lead() {
    let record = acme();
    let expected = AssigneeResolution::Desk {
        desk: "eng".into(),
        lead: "engineer".into(),
    };
    assert_eq!(resolve(&record, "eng"), expected);
    assert_eq!(resolve(&record, "Engineering Desk"), expected);
    assert_eq!(resolve(&record, "eng").working_agent(), Some("engineer"));
    assert!(resolve(&record, "eng").rejection().is_none());
}

/// A desk whose membership is entirely operator-overlay still resolves —
/// the effective roster, not just the manifest one.
#[test]
fn an_overlay_member_can_lead_a_manifest_empty_desk() {
    let mut record = acme();
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "nova".into(),
        name: "Nova".into(),
        role: "Growth".into(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    });
    record.overlay_desk_members.push(OverlayDeskMember {
        desk_id: "empty".into(),
        agent_id: "nova".into(),
    });
    assert_eq!(
        resolve(&record, "empty"),
        AssigneeResolution::Desk {
            desk: "empty".into(),
            lead: "nova".into(),
        }
    );
}

/// Issue #1835: a card assigned to an `auto` channel dispatches to the
/// channel's deterministic first member, never to `EmptyDesk` — that arm's
/// wording ("nobody on it") would be a lie about a staffed channel. The
/// per-message selector is a chat-routing rung; a durable card wants a
/// durable owner.
#[test]
fn an_auto_channel_is_assignable_and_dispatches_to_its_first_member() {
    let mut record = acme();
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "launch".into(),
        name: "Launch week".into(),
        description: None,
        members: vec!["engineer".into(), "ceo".into()],
        responder: crate::ports::types::ResponderMode::Auto,
        hive: Default::default(),
    });
    assert_eq!(
        resolve(&record, "launch"),
        AssigneeResolution::Desk {
            desk: "launch".into(),
            lead: "engineer".into(),
        }
    );
}

/// A real desk with nobody on it: assignable, but not dispatchable, and the
/// refusal says which of the two it is.
#[test]
fn an_empty_desk_is_real_but_unworkable() {
    let record = acme();
    let resolved = resolve(&record, "empty");
    assert_eq!(resolved, AssigneeResolution::EmptyDesk("empty".into()));
    assert!(resolved.names_something_real(), "the desk does exist");
    assert!(resolved.working_agent().is_none());
    let rejection = resolved.rejection().expect("unworkable");
    assert!(rejection.contains("empty"), "{rejection}");
}

/// The reported bug: "Shane" is nobody. It must not resolve, and the
/// refusal must quote what was typed back at the operator.
#[test]
fn an_off_roster_name_is_unknown_and_quoted_back() {
    let record = acme();
    let resolved = resolve(&record, "Shane");
    assert_eq!(resolved, AssigneeResolution::Unknown("Shane".into()));
    assert!(!resolved.names_something_real());
    assert!(resolved.working_agent().is_none());
    let rejection = resolved.rejection().expect("unknown is a rejection");
    assert!(rejection.contains("Shane"), "{rejection}");
}

/// What a write stores: the canonical key, never what was typed — except
/// for the unknown, which has no canonical form and is refused instead.
/// A desk assignment stays a **desk** assignment; picking the lead is
/// dispatch's job, not the write boundary's.
#[test]
fn canonical_is_the_stored_key() {
    let record = acme();
    assert_eq!(resolve(&record, "  ").canonical(), Some(""));
    assert_eq!(resolve(&record, "Engineer").canonical(), Some("engineer"));
    assert_eq!(
        resolve(&record, "Engineering desk").canonical(),
        Some("eng")
    );
    assert_eq!(resolve(&record, "empty").canonical(), Some("empty"));
    assert_eq!(resolve(&record, "Shane").canonical(), None);
}

/// Desks win a key collision, matching `responder_for`'s documented
/// precedence for an addressed chat.
#[test]
fn a_desk_wins_a_key_collision_with_a_teammate() {
    let record = record(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "engineer"
role = "Engineer"

[[group_chat]]
id = "engineer"
name = "Engineering desk"
members = ["ceo"]
"#,
    );
    assert_eq!(
        resolve(&record, "engineer"),
        AssigneeResolution::Desk {
            desk: "engineer".into(),
            lead: "ceo".into(),
        }
    );
}

/// An operator-added teammate is reachable by the only string the operator
/// may recognise. `server::ops::team` minted these with `id: generate_id()`
/// before #686, so an id-only match made every teammate the operator added
/// unassignable on a free-text Assignee field with no picker (#214 review).
/// The fixture keeps a generated-shaped id deliberately: those records still
/// exist and are never migrated, so this arm must keep working for them.
#[test]
fn an_operator_added_teammate_resolves_by_display_name() {
    let mut record = acme();
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "01J9XKQ2M7Z4B8N0".into(),
        name: "Shane".into(),
        role: "Support".into(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    });
    assert_eq!(
        resolve(&record, "Shane"),
        AssigneeResolution::Agent("01J9XKQ2M7Z4B8N0".into()),
        "the display name is the only key the operator can discover"
    );
    assert_eq!(
        resolve(&record, "shane"),
        AssigneeResolution::Agent("01J9XKQ2M7Z4B8N0".into()),
        "matched case-insensitively, like every other typed key"
    );
    assert!(resolve(&record, "Shane").rejection().is_none());
}

/// The id namespace still wins. A teammate whose *display name* happens to
/// equal a manifest *id* must not steal that id's assignments.
#[test]
fn a_display_name_cannot_shadow_a_manifest_id() {
    let mut record = acme();
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "01J9XKQ2M7Z4B8N0".into(),
        name: "engineer".into(),
        role: "Support".into(),
        description: None,
        tools: None,
        model: None,
        harness: None,
    });
    assert_eq!(
        resolve(&record, "engineer"),
        AssigneeResolution::Agent("engineer".into()),
        "ids are resolved before display names"
    );
}

/// Two teammates sharing a display name is the operator's own doing, and it
/// is reported as such. Silently taking the first would reintroduce exactly
/// the misrouting this module exists to end.
#[test]
fn two_teammates_sharing_a_display_name_are_refused_not_guessed() {
    let mut record = acme();
    for id in ["01J9XKQ2M7Z4B8N0", "01J9XKQ2M7Z4B8N1"] {
        record.overlay_agents.push(OverlayAgent {
            provider: None,
            id: id.into(),
            name: "Shane".into(),
            role: "Support".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        });
    }
    let resolution = resolve(&record, "Shane");
    assert_eq!(
        resolution,
        AssigneeResolution::AmbiguousTeammate {
            raw: "Shane".into(),
            count: 2,
        }
    );
    assert!(resolution.working_agent().is_none());
    assert!(resolution.canonical().is_none());
    assert!(
        !resolution.names_something_real(),
        "an ambiguous name is refused at the write boundary"
    );
    let reason = resolution.rejection().expect("must be refused");
    assert!(reason.contains("2 teammates"), "reason={reason}");
    assert!(
        reason.contains("teammate id"),
        "the operator is told how to disambiguate: reason={reason}"
    );
}

/// A desk assignment is ownership and survives dispatch. `working_agent()`
/// returns the desk's *lead* so the turn runs, but the card must keep the
/// desk id — otherwise a card assigned to `eng` silently becomes assigned
/// to `engineer` the first time it runs (#214 review).
#[test]
fn a_desk_assignment_is_not_relinked_to_its_lead() {
    let record = acme();
    let desk = resolve(&record, "eng");
    assert_eq!(desk.working_agent(), Some("engineer"), "the lead runs it");
    assert_eq!(
        desk.canonical(),
        Some("eng"),
        "but the card stays the desk's"
    );
    assert!(
        !desk.links_working_agent(),
        "dispatch must not write the lead over a desk assignment"
    );

    assert!(
        AssigneeResolution::Unassigned.links_working_agent(),
        "an unassigned card DOES get the orchestrator linked to it (#205)"
    );
    assert!(
        AssigneeResolution::Agent("engineer".into()).links_working_agent(),
        "a teammate assignment is already canonical and stays linked"
    );
}

#[test]
fn the_default_agent_dm_is_the_orchestrators_dm() {
    assert_eq!(default_agent_dm(&acme()).as_deref(), Some("dm:ceo"));

    let tagged = record(
        r#"
[company]
name = "Acme"

[[agent]]
id = "engineer"
role = "Engineer"

[[agent]]
id = "boss"
role = "Chief Executive"
tier = "orchestrator"
"#,
    );
    assert_eq!(default_agent_dm(&tagged).as_deref(), Some("dm:boss"));

    let empty = record("[company]\nname = \"Acme\"\n");
    assert_eq!(default_agent_dm(&empty), None);
}

#[test]
fn a_turn_with_no_thread_answers_in_the_agents_dm() {
    assert_eq!(chat_or_dm(Some("eng"), "engineer"), "eng");
    assert_eq!(chat_or_dm(None, "engineer"), "dm:engineer");
}
