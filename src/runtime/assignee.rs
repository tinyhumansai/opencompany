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

use crate::ports::types::{CompanyRecord, TeammateResolution};
use crate::runtime::delegation_tools::desk_default_responder;

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
    /// Names more than one operator-added teammate: two teammates carry the
    /// same display name. Real, but not resolvable to a single worker — kept
    /// distinct from [`Self::Unknown`] because the fix is different (rename one
    /// of them, or assign by id) and because silently taking the first is the
    /// misrouting this module exists to end.
    AmbiguousTeammate {
        /// The string as typed.
        raw: String,
        /// How many teammates answer to it.
        count: usize,
    },
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
            Self::Unassigned
            | Self::EmptyDesk(_)
            | Self::Unknown(_)
            | Self::AmbiguousTeammate { .. } => None,
        }
    }

    /// Whether dispatch should write the working agent back onto the card.
    ///
    /// True for [`Self::Unassigned`] and [`Self::Agent`] only. A **desk**
    /// assignment is ownership and stays a desk assignment — see
    /// [`Self::canonical`], which deliberately stores the desk id. The lead is
    /// who runs this turn, not who the card belongs to, so writing the lead
    /// back would erase the desk from the board the first time the card ran and
    /// contradict the invariant `canonical()` exists to hold. The unworkable
    /// kinds never reach the write-back: dispatch refuses them first.
    pub fn links_working_agent(&self) -> bool {
        matches!(self, Self::Unassigned | Self::Agent(_))
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
            Self::Unknown(_) | Self::AmbiguousTeammate { .. } => None,
        }
    }

    /// Whether the string names something that exists — a teammate, a desk, or
    /// nothing at all. The **write-boundary** predicate: assigning a card to a
    /// desk whose members have yet to be added is a legitimate thing to do, so
    /// only [`Self::Unknown`] is refused on write.
    pub fn names_something_real(&self) -> bool {
        !matches!(self, Self::Unknown(_) | Self::AmbiguousTeammate { .. })
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
            Self::AmbiguousTeammate { raw, count } => Some(format!(
                "\"{raw}\" names {count} teammates on this company's roster, so there is no way \
to tell which one you meant — rename one of them, or assign the card by teammate id"
            )),
        }
    }
}

/// The console's DM channel-key prefix: `#/chat/dm:designer` addresses the
/// teammate `designer`.
pub const DM_PREFIX: &str = "dm:";

/// The roster key a `dm:`-prefixed channel id addresses, or `None` when the key
/// carries no prefix (or nothing after it).
///
/// The console mints a DM channel id as `dm:<teammate-id>` (`chat/model.ts`'s
/// `dmChannelId`), and that id is a *documented* channel key on the chat route —
/// so it reaches both the responder lookup and the card the route opens, and
/// both used to read it as naming nothing. This is the one place that shape is
/// spelled, so the two cannot drift.
///
/// Callers must try the key **as sent** first and fall back to this only when it
/// resolves to nothing: stripping unconditionally would let `dm:x` claim a desk
/// or teammate literally named `dm:x`, which is a key that resolves today.
pub fn dm_key(chat: &str) -> Option<&str> {
    let key = chat.strip_prefix(DM_PREFIX)?.trim();
    (!key.is_empty()).then_some(key)
}

/// Resolves `assignee` against `record`'s full roster.
///
/// Resolution order, and the order matters: **desks first**, mirroring
/// [`responder_for`](crate::harness) — a desk whose id happens to match a
/// teammate id keeps routing as a desk, exactly as it does for an operator
/// message addressed to that chat. A teammate id is then matched
/// case-insensitively, then an operator-added teammate's display name — both
/// halves through [`CompanyRecord::resolve_teammate_key`], because unlike a
/// desk key a teammate key is typed by hand and `resolve_desk_id` was already
/// forgiving about case.
///
/// The name arm used to live here. #1162 moved it onto the record, where the
/// delegation path could reach it too: the same string an operator types into
/// an Assignee field is the string a model reads off `query_company`'s roster,
/// and having one of them resolve while the other refused is how #1162
/// happened. The rationale for trying names at all — teammates added before
/// #686 keep generated ids forever, and an id never follows a rename — is
/// documented on that method.
pub fn resolve(record: &CompanyRecord, assignee: &str) -> AssigneeResolution {
    let key = assignee.trim();
    if key.is_empty() {
        return AssigneeResolution::Unassigned;
    }
    if let Some(desk) = record.resolve_desk_id(key) {
        // `desk_default_responder`, not `desk_lead`: for a lead desk they are
        // the same teammate, and for an `auto` channel (issue #1835) — where
        // `desk_lead` is `None` by definition — a card assigned to the channel
        // still dispatches to its deterministic first member rather than
        // misreporting a staffed channel as empty. The per-message selector is
        // a chat-routing rung; a durable card wants a durable owner.
        return match desk_default_responder(record, &desk) {
            Some(lead) => AssigneeResolution::Desk { desk, lead },
            None => AssigneeResolution::EmptyDesk(desk),
        };
    }
    // Then the teammate namespace: id first, then an operator-added teammate's
    // display name. Both halves, in that order, live on
    // `CompanyRecord::resolve_teammate_key` — this was the only surface that
    // had them until #1162 gave the delegation path the same resolve, and the
    // two must not be able to disagree about who a name means.
    match record.resolve_teammate_key(key) {
        TeammateResolution::Agent(id) => AssigneeResolution::Agent(id),
        TeammateResolution::Unknown => AssigneeResolution::Unknown(key.to_string()),
        TeammateResolution::Ambiguous(ids) => AssigneeResolution::AmbiguousTeammate {
            raw: key.to_string(),
            count: ids.len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, OverlayAgent, OverlayDeskMember};

    fn record(manifest: &str) -> CompanyRecord {
        let manifest: CompanyManifest = toml::from_str(manifest).expect("valid manifest");
        CompanyRecord {
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
}
