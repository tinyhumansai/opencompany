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

/// The console's DM channel key for the teammate `agent`: `dm:<agent>`.
pub fn dm_channel(agent: &str) -> String {
    format!("{DM_PREFIX}{agent}")
}

/// The thread a turn answers in: `chat` when it names one, else `agent`'s DM.
pub fn chat_or_dm(chat: Option<&str>, agent: &str) -> String {
    chat.map_or_else(|| dm_channel(agent), str::to_string)
}

/// The DM channel of the company's default agent — the orchestrator, else the
/// first teammate on the roster — which is where a message, reply or notice
/// that names no conversation lands. `None` on an empty roster.
pub fn default_agent_dm(record: &CompanyRecord) -> Option<String> {
    crate::company::orchestrator_id(&record.effective_agents()).map(dm_channel)
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
#[path = "assignee_tests.rs"]
mod tests;
