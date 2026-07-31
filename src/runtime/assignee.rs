//! Resolving a task card's `assignee` against the company roster (issue #205).
//!
//! A card's `assignee` is a free-text string: an operator types it into the
//! board's Assignee field, and `spawn_task` / `delegate_to_desk` let a model
//! write one. It can therefore name four different things — nobody, a roster
//! teammate, a desk, or something that simply does not exist — and every
//! consumer used to decide for itself which of those it recognised.
//!
//! It decided badly. `HarnessBrain::task_responder` matched **only**
//! `manifest.agents`, so a card assigned to an operator-added overlay teammate,
//! or to a desk, fell through the same arm as a card assigned to nobody and the
//! same arm as a card assigned to a name nobody on the roster answers to — and
//! all four silently dispatched to the orchestrator while the card kept whatever
//! string it had. The board said "Shane"; the CEO did the work; nothing said
//! "Shane" is not a teammate.
//!
//! This module is the one resolver, brain-agnostic (it reads only
//! [`CompanyRecord`]) and compiled in every build so both the harness dispatch
//! path and the REST write boundary share one answer. Crucially it keeps the
//! four cases **distinct** — [`AssigneeResolution`] — so a caller can give the
//! blank card to the orchestrator and still refuse the invalid one.

use crate::ports::types::CompanyRecord;
use crate::runtime::delegation_tools::desk_lead;

/// What a card's `assignee` string names on the company roster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssigneeResolution {
    /// Blank (or whitespace) — nobody was named. Not an error: the card is
    /// simply unassigned, and dispatch hands it to the orchestrator.
    Unassigned,
    /// A roster teammate — a manifest agent or an operator-overlay one. Carries
    /// the **canonical** id, so a typed `"Engineer"` resolves to `engineer`.
    Agent(String),
    /// A desk. Its lead member works the card; both ids are canonical.
    Desk {
        /// The canonical desk id.
        desk: String,
        /// The desk's lead member — the agent that actually runs the turn.
        lead: String,
    },
    /// A desk that exists but has no roster-backed member to work the card.
    /// Real enough to assign to, not real enough to dispatch to.
    EmptyDesk(String),
    /// Names nothing on the roster. Carries the string as typed, so the
    /// operator is told back exactly what they wrote.
    Unknown(String),
}

impl AssigneeResolution {
    /// The agent that will actually run the card, when the assignee names one.
    ///
    /// `None` covers three different situations on purpose — [`Self::Unassigned`]
    /// (nobody named; the caller's default responder takes it) and the two
    /// unworkable forms. Callers separate them with [`Self::rejection`]: a
    /// `None` with no rejection is the legitimate unassigned card.
    pub fn working_agent(&self) -> Option<&str> {
        match self {
            Self::Agent(id) => Some(id.as_str()),
            Self::Desk { lead, .. } => Some(lead.as_str()),
            Self::Unassigned | Self::EmptyDesk(_) | Self::Unknown(_) => None,
        }
    }

    /// The canonical string to store for this assignee: the resolved teammate
    /// id, the resolved **desk** id (a desk assignment is ownership, and stays
    /// a desk assignment — dispatch is what picks the lead), or `""` when
    /// nobody was named. `None` for an [`Self::Unknown`], which has no
    /// canonical form because it names nothing.
    ///
    /// Writing this rather than the raw string is what keeps the board's
    /// assignee column one namespace of real ids: a typed `"Engineer"` or
    /// `"Engineering Desk"` is stored as `engineer` / `eng`, so every reader —
    /// the board, the timeline, `assignment_matches` — sees the same key.
    pub fn canonical(&self) -> Option<&str> {
        match self {
            Self::Unassigned => Some(""),
            Self::Agent(id) => Some(id.as_str()),
            Self::Desk { desk, .. } => Some(desk.as_str()),
            Self::EmptyDesk(desk) => Some(desk.as_str()),
            Self::Unknown(_) => None,
        }
    }

    /// Whether the string names something that exists — a teammate, a desk, or
    /// nothing at all. The **write-boundary** predicate: assigning a card to a
    /// desk whose members have yet to be added is a legitimate thing to do, so
    /// only [`Self::Unknown`] is refused on write.
    pub fn names_something_real(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// The operator-facing reason this assignee cannot be worked, or `None`
    /// when it can (including the unassigned card, which the orchestrator
    /// picks up).
    ///
    /// This sentence is the feedback that was missing: it lands in the card's
    /// note, in the relayed reply, and — for a write — in the `400`.
    pub fn rejection(&self) -> Option<String> {
        match self {
            Self::Unassigned | Self::Agent(_) | Self::Desk { .. } => None,
            Self::EmptyDesk(desk) => Some(format!(
                "the desk \"{desk}\" has no teammates on it, so there is nobody to work this — \
add a member to the desk, or assign the card to a teammate directly"
            )),
            Self::Unknown(raw) => Some(format!(
                "\"{raw}\" is not a teammate or a desk on this company's roster — \
assign the card to one of them, or leave the assignee blank to hand it to the orchestrator"
            )),
        }
    }
}

/// Resolves `assignee` against `record`'s full roster.
///
/// Resolution order, and the order matters: **desks first**, mirroring
/// [`responder_for`](crate::harness) — a desk whose id happens to match a
/// teammate id keeps routing as a desk, exactly as it does for an operator
/// message addressed to that chat. A teammate id is then matched
/// case-insensitively ([`CompanyRecord::resolve_roster_agent_id`]), because
/// unlike a desk key it is typed by hand and `resolve_desk_id` was already
/// forgiving about case.
pub fn resolve(record: &CompanyRecord, assignee: &str) -> AssigneeResolution {
    let key = assignee.trim();
    if key.is_empty() {
        return AssigneeResolution::Unassigned;
    }
    if let Some(desk) = record.resolve_desk_id(key) {
        return match desk_lead(record, &desk) {
            Some(lead) => AssigneeResolution::Desk { desk, lead },
            None => AssigneeResolution::EmptyDesk(desk),
        };
    }
    if let Some(id) = record.resolve_roster_agent_id(key) {
        return AssigneeResolution::Agent(id);
    }
    AssigneeResolution::Unknown(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, OverlayAgent, OverlayDeskMember};

    fn record(manifest: &str) -> CompanyRecord {
        let manifest: CompanyManifest = toml::from_str(manifest).expect("valid manifest");
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            template_provenance: None,
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
            id: "nova".into(),
            name: "Nova".into(),
            role: "Growth".into(),
            description: None,
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
            id: "nova".into(),
            name: "Nova".into(),
            role: "Growth".into(),
            description: None,
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
}
