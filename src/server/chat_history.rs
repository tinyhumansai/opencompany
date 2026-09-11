//! Shared desk-history read logic (issue #65).
//!
//! Both the GraphQL `Chat.history` resolver
//! ([`crate::server::graphql::company`]) and the REST `GET .../chat/history`
//! route ([`crate::server::operator`]) need to answer the same question — "what
//! messages belong to this desk, as seen by this viewer?" — and they must never
//! be allowed to disagree about it. This module is the one place that answers
//! it; both surfaces call through it instead of each keeping their own copy of
//! the filter + projection logic.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::CompanyStore;
use crate::ports::types::{
    Actor, ActorKind, Attachment, CompanyEvent, CompanyId, CompanyRecord, EventSeq, Mention,
    MentionTarget, StoredEvent, TurnStep,
};
use crate::server::ops::language::DEFAULT_DESK;

// Conversation identity now lives in `tinyhivemind_core::chat`, and these are
// re-exported so every existing caller keeps its path (issue #65, #435).
//
// The move is what lets `ports::types` stop reaching *upward* into
// `crate::server::` to fold a General spelling: `resolve_desk_id` and
// `desk_alias_is_ambiguous` call this rule, and a port calling a server module
// was a layering violation that only a shared crate could remove.
pub use tinyhivemind_core::chat::{
    GENERAL_DESK, MAIN_THREAD_ID, is_general_chat, same_conversation,
};

// `DEFAULT_DESK` is the prosumer glossary string mirroring
// `frontend/src/lib/language.ts`; `GENERAL_DESK` is the desk's identity. They
// are different concerns that happen to be the same literal, so neither imports
// the other — but they must never drift, because a message journaled under the
// glossary word has to fold into the identity. Pinned here rather than
// duplicated, and it costs nothing at runtime.
const _: () = assert!(
    matches!(DEFAULT_DESK.as_bytes(), b"General") && matches!(GENERAL_DESK.as_bytes(), b"General"),
    "the operator-facing default desk name and the General desk id must agree",
);

/// The largest message page either history surface may materialize. Keeping
/// the limit beside the shared reader prevents a new caller from turning its
/// `Vec` reservation back into an allocation controlled by the request.
pub const CHAT_HISTORY_PAGE_LIMIT: usize = 200;

/// Where this lives, and why it is not beside its first caller.
///
/// It began in the chat seed, under `src/harness/`, which compiles only
/// with the `openhuman` feature. Two later callers — the thread index in
/// [`crate::runtime::cycle`] and `read_thread` — need the same resolution,
/// and the first of those is in the ungated runtime, so the default build
/// stopped compiling. Beside [`owns`] is where it belonged anyway: this
/// module is the one place that answers what a desk id means, and a
/// second copy is exactly what it exists to prevent.
/// Resolves an incoming `chat_id` to the `(desk_id, desk_name)` pair
/// [`owns`] filters on, exactly as the REST history route's
/// `resolve_desk` does (issue #65).
///
/// `owns` matches a stored event's chat id against *both* the desk id and the
/// desk name, because a named desk's messages can be journaled under either
/// spelling. Passing `(chat_id, chat_id)` for a desk the operator addressed by
/// id would therefore silently miss any line stored under its name — a seed that
/// "looks fixed" but is empty. So a non-General selector is resolved against the
/// manifest's group chats the same way the console resolves it.
///
/// * `None` → the synthetic General/operator desk.
/// * A General spelling (`"main"` / `"general"` / `""`) short-circuits: every
///   spelling folds together in [`same_conversation`], so no
///   manifest read is needed and `(chat, chat)` already owns all of them.
/// * Anything else is matched (case-insensitive, by id or name) against the
///   manifest's group chats; an unmatched selector passes through as `(id, name)
///   = (chat, chat)`, so an ad-hoc thread id still finds what was journaled under
///   that exact string.
pub async fn resolve_seed_desk(
    store: &Arc<dyn CompanyStore>,
    company: &CompanyId,
    chat_id: Option<&str>,
) -> (String, String) {
    let Some(desk) = trivially_resolved(chat_id) else {
        // Only a named desk needs the manifest, and only then is it read.
        return match store.load(company).await {
            Ok(Some(record)) => desk_aliases(&record, chat_id),
            // A store miss or read error must not fail the turn — fall back to
            // the verbatim selector, which still owns everything journaled
            // under that exact string (the common case, where the console
            // addresses id == name).
            Ok(None) | Err(_) => {
                let desk = chat_id.unwrap_or(GENERAL_DESK);
                (desk.to_string(), desk.to_string())
            }
        };
    };
    desk
}

/// [`resolve_seed_desk`] for a caller that already holds the record.
///
/// The cycle's briefings do: they are handed a `&CompanyRecord` and were paying
/// for a `load` per message to answer a question the record in their hand
/// already answers. Same resolution, no store round-trip — and one body, so the
/// two cannot drift into disagreeing about what a desk id means.
pub fn desk_aliases(record: &CompanyRecord, chat_id: Option<&str>) -> (String, String) {
    if let Some(resolved) = trivially_resolved(chat_id) {
        return resolved;
    }
    let desk = chat_id.unwrap_or(GENERAL_DESK);
    // **Through `resolve_desk_id`, not a second lookup of its own** (codex +
    // coderabbit on #1972). That function already answers "which desk is this
    // key", and it answers two things a one-pass `id == key || name == key`
    // find gets wrong: an **overlay desk** — one created from the console, which
    // lives in `overlay_desks` and not in the manifest at all — is a routable
    // desk, and an **exact id beats a display-name alias**, because desk
    // creation enforces unique ids but not unique names, so `{id: "ops", name:
    // "sales"}` can sit ahead of `{id: "sales", …}` and answer for it. Getting
    // that wrong here does not merely miss lines, it *merges* two desks: `owns`
    // would then be handed one desk's id and another's name.
    let Some(id) = record.resolve_desk_id(desk) else {
        // Not a desk this company declares — an ad-hoc thread id or a DM. It
        // still owns everything journaled under that exact string, which is
        // what the verbatim pair says.
        return (desk.to_string(), desk.to_string());
    };
    let name = record
        .manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == id)
        .map(|chat| chat.name.clone())
        .or_else(|| {
            record
                .overlay_desks
                .iter()
                .find(|overlay| overlay.id == id)
                .map(|overlay| overlay.name.clone())
        })
        .unwrap_or_else(|| id.clone());
    (id, name)
}

/// The two selectors that resolve without consulting a manifest at all.
///
/// `None` is the General desk — an unaddressed message is *routed* there
/// (`chat_and_emit`), so treating it as "addressed to nothing" is what left
/// those turns out of every desk-scoped read. Any other General spelling
/// short-circuits too: they all fold in [`same_conversation`], so `(chat, chat)`
/// already owns each other's lines.
fn trivially_resolved(chat_id: Option<&str>) -> Option<(String, String)> {
    match chat_id {
        None => Some((GENERAL_DESK.to_string(), GENERAL_DESK.to_string())),
        Some(desk) if is_general_chat(Some(desk)) => Some((desk.to_string(), desk.to_string())),
        Some(_) => None,
    }
}

/// Does a conversation id **stamped onto a record** name `desk`?
///
/// The fold is [`same_conversation`]'s — every spelling of General is one
/// conversation, every other id compares verbatim — with the one difference
/// this function exists to state:
///
/// **`None` is not the General desk.** [`same_conversation`] reads a missing id
/// as *the id was never addressed*, which for a chat message is right: an
/// unaddressed post went to the company-wide line, so it folds into General. A
/// `None` **stamped on a record** means the opposite — *no conversation
/// produced this*. A blocker parked by the planning pass, a card created on the
/// board, a scheduler tick: each carries no thread because none of them
/// happened in a conversation, and folding that into General hands every one of
/// them to whoever next types in `#general`.
///
/// That is not hypothetical. `pending_blocker_groups` matched thread-less
/// parked blockers against `#general` through [`same_conversation`], so a
/// founder's first line in the channel was consumed as the *answer* to one of
/// them: the send settled in milliseconds with no cycle, no run and no reply,
/// and the console showed a message that read exactly like one being worked on.
/// `owns` had already carved the same rule out by hand for a
/// `DeskTaskCompleted` with no origin ("**`None` is not the General desk**",
/// see its doc) — one carve-out written twice and missed a third time is the
/// drift; one named predicate is the fix, the same argument
/// [`same_conversation`] itself was extracted under.
///
/// The `desk` side stays a plain `&str` on purpose: a caller asking "is this
/// record's origin the desk I am reading?" always has a desk, and taking an
/// `Option` there would re-open the question this answers.
pub fn stamped_conversation_is(origin: Option<&str>, desk: &str) -> bool {
    origin.is_some_and(|origin| same_conversation(Some(origin), Some(desk)))
}

/// Whether a stored event belongs to the desk identified by `desk_id` /
/// `desk_name`.
///
/// Both `AgentReply`s and `OperatorMessage`s route by their stored chat id,
/// matched against the desk's id and its name through [`same_conversation`] — so
/// a named desk still compares verbatim and the General desk answers to every
/// spelling of itself, and no historical message is orphaned by the id it
/// happened to be journaled under (issue #65).
///
/// **Folded on both sides, not just the event's** (issue #435). The General
/// check used to key on the *desk being asked for* being spelled `"General"`,
/// which the console never does: its default thread is `"main"`, so
/// `?desk=main` resolves to `("main", "main")` — no group chat is named `main` —
/// and every event journaled under `"General"` was excluded from the one
/// transcript that should hold them. An unaddressed chat post is exactly that
/// pair: the operator message stores `chat: None` and its answer is journaled
/// with `chat_id: "General"`, so the console's main line dropped both halves of
/// its own conversation. The asymmetry also put this function at odds with
/// `resolvable_parent`, which now folds through the same rule: a continuation
/// could be parented to a root the main line refuses to render, and the console
/// drops a reply whose parent it cannot find rather than showing it flat.
///
/// **A third kind of event routes here since issue #377**: the dispatch
/// terminal. A card raised from a channel settles somewhere — `in_review`,
/// `paused`, `todo` — and until #377 nothing structural said so in the channel
/// it came from, so a reader saw the agent's relay prose and reasonably
/// concluded the work had finished when it had in fact parked. The terminal
/// routes by the origin the card recorded at raise time, matched on exactly the
/// terms the other two are.
///
/// **`None` is not the General desk.** Everywhere else in this module a missing
/// chat id means *the id was never addressed* and folds into General; on a
/// terminal it means *no conversation raised this card* — it was created on the
/// board, by a scheduler, or before the origin was recorded. Folding that into
/// General would post a marker about board-only work into the operator's main
/// line, which is a different bug from the one #377 fixes, so this arm answers
/// `false` for every desk including General. It is the single most bug-prone
/// line in this function and has its own test.
///
/// That rule is [`stamped_conversation_is`] now. This arm keeps its own
/// `return false` because it must also skip the shared tail below, but anywhere
/// *else* asking "does this record's stamped origin name my desk?" calls the
/// predicate rather than writing the carve-out again — writing it twice and
/// forgetting it a third time is what let a thread-less parked blocker read as
/// pending in `#general`.
/// One channel this agent can read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Channel {
    /// The desk id the journal stores rows under.
    pub id: String,
    /// The desk's display name — `chat_history::owns` matches either.
    pub name: String,
    /// How the cue names this channel to the agent.
    pub label: String,
}

/// Every channel this agent can read: each desk it sits on, its own DM, and
/// the company's General line.
///
/// Enumerated the way [`CompanyRecord::agent_desk_tools`] enumerates desks —
/// manifest desks first, then operator-created overlay desks, deduplicated —
/// so a teammate seated through the console is in its own session exactly as a
/// manifest member is.
pub fn agent_channels(record: &CompanyRecord, agent_id: &str) -> Vec<Channel> {
    let mut seen = std::collections::HashSet::new();
    let mut channels = Vec::new();

    let manifest = record
        .manifest
        .group_chats
        .iter()
        .map(|chat| chat.id.clone());
    let overlay = record.overlay_desks.iter().map(|desk| desk.id.clone());
    for desk_id in manifest.chain(overlay) {
        if !seen.insert(desk_id.clone()) {
            continue;
        }
        if !record
            .effective_desk_members(&desk_id)
            .iter()
            .any(|member| member == agent_id)
        {
            continue;
        }
        let name = desk_display_name(record, &desk_id);
        channels.push(Channel {
            label: format!("#{}", name),
            id: desk_id,
            name,
        });
    }

    // This agent's own direct line — under **both** spellings it is journaled
    // under, because the console and the route disagree and both are correct.
    //
    // `dmThreadId` (`views/room/channels.ts`) posts a DM under the teammate's
    // **bare id**; `dm:<id>` is the console's channel key and is *also* a
    // documented key on the chat route (`assignee::dm_key`), which a teammate
    // whose id is a General spelling is always addressed by. A session that
    // listed only one of them would miss every DM keyed the other way — which
    // is the whole of the operator's own line to this agent.
    //
    // Keyed on the id and never the name: renaming somebody must not move their
    // DM or orphan its history (issue #364).
    for (label, id) in [
        ("dm", agent_id.to_string()),
        (
            "dm",
            format!("{}{agent_id}", crate::runtime::assignee::DM_PREFIX),
        ),
    ] {
        if seen.insert(id.clone()) {
            channels.push(Channel {
                label: label.to_string(),
                name: id.clone(),
                id,
            });
        }
    }

    // The company's own line. Not a desk (issue #1743) unless a blueprint
    // declared one under a General spelling, in which case the loop above
    // already claimed it and this is a no-op.
    let general = tinyhivemind_core::chat::GENERAL_DESK.to_string();
    if seen.insert(general.clone()) {
        channels.push(Channel {
            label: "#general".to_string(),
            name: general.clone(),
            id: general,
        });
    }

    channels
}

/// The desk's display name, falling back to its id.
fn desk_display_name(record: &CompanyRecord, desk_id: &str) -> String {
    record
        .manifest
        .group_chats
        .iter()
        .find(|chat| chat.id == desk_id)
        .map(|chat| chat.name.clone())
        .or_else(|| {
            record
                .overlay_desks
                .iter()
                .find(|desk| desk.id == desk_id)
                .map(|desk| desk.name.clone())
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| desk_id.to_string())
}

pub fn owns(desk_id: &str, desk_name: &str, event: &CompanyEvent) -> bool {
    let stored = match event {
        CompanyEvent::AgentReply { chat_id, .. } => Some(chat_id.as_str()),
        CompanyEvent::OperatorMessage { chat, .. } => chat.as_deref(),
        // Issue #377. `None` short-circuits to `false` here rather than
        // falling through to the shared tail: `same_conversation` reads a
        // `None` as "unaddressed, therefore General", and this event's `None`
        // means the opposite — no conversation raised this card, so it belongs
        // to no conversation's history.
        CompanyEvent::DeskTaskCompleted { origin_chat_id, .. } => match origin_chat_id.as_deref() {
            Some(origin) => Some(origin),
            None => return false,
        },
        _ => return false,
    };
    same_conversation(stored, Some(desk_id)) || same_conversation(stored, Some(desk_name))
}

/// The channel line a settled dispatch leaves behind (issue #377) —
/// `finished → In review`.
///
/// Deliberately **structural and short**: where the card landed, and nothing
/// else. The run's prose already reaches the same channel as the orchestrator's
/// relay bubble (#151), so repeating it here would put one run's words into one
/// conversation twice. What was missing was never the words — it was the fact
/// that the card *settled*, and *where*, which a reader watching only the prose
/// could not tell apart from "still working".
///
/// "finished" means *the run stopped*, not *it succeeded* — the same reading
/// [`CompanyEvent::DeskTaskCompleted`] itself takes. A cancelled or failed
/// dispatch lands in To-do and says so; a paused one says Paused. That is the
/// whole point: the misleading case this exists for is precisely the run that
/// stopped without finishing the work.
///
/// An unrecognised column id passes through **verbatim**, the same posture
/// `harness::lifecycle::relay_text` takes — a newer host naming a column this
/// build has not heard of should read a little raw, never render blank.
///
/// The label is [`crate::ledger::board`]'s, not a fourth copy of it. This
/// function used to carry its own `match` from id to label — one of three on
/// the host and a fourth in the console — and each was a place a renamed column
/// could half-land.
///
/// Pinned by tests on both sides of the wire: the console has its own
/// `dispatchMarkerText` (`frontend/src/lib/chat.ts`), because the live SSE
/// frame carries the raw column id rather than prose and a marker renders
/// synchronously from it, with no ledger read to await. That copy is the one
/// remaining exception, and it is the safe one: two spellings of a sentence can
/// only *reword* a marker across a reload — never double it, since the dedupe
/// is on identity, and never lose a card.
pub fn dispatch_marker_text(column: &str) -> String {
    format!("finished → {}", crate::ports::tasks::column_label(column))
}

/// Where a crossing referral came from, folded onto the message it caused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferredFrom {
    /// The asking desk, by id — for the link.
    pub desk_id: String,
    /// Its display name as captured when the referral was made.
    pub desk_name: String,
    /// The agent that asked.
    pub asker_id: String,
    /// Its display label as captured when the referral was made.
    pub asker_label: String,
    /// The asking message, so the console can link to it.
    pub sequence: u64,
    /// Whether this is the answer coming home rather than the outbound ask.
    /// Carried from the marker; see `CompanyEvent::ReferralEnqueued`.
    pub returning: bool,
    /// Whether a PERSON was asked rather than a desk.
    ///
    /// The chip names whoever was actually addressed. "Answered by Front Desk"
    /// for a question put to one person names a room that was never asked — its
    /// other members had no part in it, and on a direct crossing it holds none
    /// of the exchange.
    pub direct: bool,
}

/// One line of a crossing, in the order it was said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferralLine {
    /// The agent that wrote it.
    pub author_id: String,
    /// Their display label, captured when the referral was made.
    pub author_label: String,
    /// What they said, with any host note to the asker already stripped.
    pub text: String,
    /// True when this desk's own agent wrote it — the question going out.
    pub outbound: bool,
}

/// The whole exchange between an agent on this desk and somebody who is not,
/// folded onto the report that brought it home.
///
/// The relayed rows themselves are still dropped from the transcript: a desk's
/// history is the conversation that happened ON it, and an agent who does not
/// work here did not speak here. But dropping them outright left an operator
/// able to see that a question crossed and never what was said either way,
/// with only the asker's paraphrase to go on. This carries the exchange as its
/// own collapsible line — closed by default, so the desk still reads as its
/// own conversation and the crossing is one click away rather than gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferralConversation {
    /// The agent on this desk that asked.
    pub asker_id: String,
    /// Who they asked.
    pub other_id: String,
    /// And where that person sits, for the label and the link.
    pub other_desk_id: String,
    pub other_desk_name: String,
    /// Whether a PERSON was asked, rather than a desk.
    ///
    /// The two are different acts and the label says which: `@name` went to
    /// somebody, `#desk` was put to a room. Taken from whether the crossing
    /// named its own pair conversation — a desk crossing runs on the desk and
    /// names none.
    pub direct: bool,
    /// The exchange, oldest first. Its length is the message count the
    /// collapsed label shows.
    pub lines: Vec<ReferralLine>,
}

/// One line of a private aside, in the order it was said.
///
/// No `author_label`: unlike a referral, both sides of an aside sit on the desk
/// being read, so the console already knows their names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsideLine {
    /// The agent that wrote it.
    pub author_id: String,
    /// What they said, with the `!aside @peer` head already stripped.
    pub text: String,
}

/// A private exchange between members of one desk, folded onto the move it rode
/// under.
///
/// The same idiom as [`ReferralConversation`] and for the same reason: it is
/// detail behind a line, not part of the desk's own conversation. It differs in
/// who may read it — an operator reads every row in full (`Audience::admits`
/// admits `Viewer::Operator` unconditionally), because privacy here is between
/// agents and is a deliberation device, never a security boundary. Collapsing it
/// is a rendering choice, not an access-control one, and nothing here withholds
/// anything from the person reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsideConversation {
    /// Everyone in it — the author first, then who they addressed.
    pub members: Vec<String>,
    /// The exchange, oldest first. Its length is the collapsed label's count.
    pub lines: Vec<AsideLine>,
}

#[cfg(test)]
impl MessageView {
    /// A bare row, for tests that exercise the folds rather than the projection.
    ///
    /// Test-only on purpose: the real constructor is `From<StoredEvent>` and has
    /// to stay the only way a view is built from a journal, or a projection
    /// concern could be skipped by a caller that assembled one by hand.
    pub(crate) fn for_test(id: &str, author: &str, text: &str, audience: Vec<String>) -> Self {
        Self {
            id: id.to_owned(),
            channel: author.to_owned(),
            admin_only: false,
            cue_author: author.to_owned(),
            author: author.to_owned(),
            cue_text: text.to_owned(),
            text: text.to_owned(),
            at_millis: 0.0,
            mine: false,
            by_person: false,
            referred_from: None,
            referral_conversation: None,
            aside_audience: audience,
            aside_conversation: None,
            steps: Vec::new(),
            task_id: None,
            parent_id: None,
            reactions: Vec::new(),
            mentions: Vec::new(),
            attachments: Vec::new(),
        }
    }
}

/// Who is reading a desk history. `mine` is relative to this.
///
/// There is no `From<StoredEvent> for MessageView`, and there cannot be:
/// `mine` depends on who is asking. With one operator it was safe to hardcode
/// `true`; with several users it would mark everyone's messages as everyone
/// else's.
#[derive(Clone, Debug, PartialEq)]
pub enum Viewer {
    /// An operator or platform credential. Legacy unattributed messages are
    /// theirs, because that is who sent them before users existed.
    Operator,
    /// A human collaborator, by user id.
    User(String),
}

/// The label a message with no nameable human author carries in an agent's cue.
///
/// Safe to sit in the same namespace as roster ids and user ids: a manifest
/// refuses the reserved ids (`company/manifest.rs`), and a minted user id is
/// not this word. Nothing a message *body* can say matters either, because
/// bodies are nested under their own speaker's label by
/// `chat_seed::prefix_every_line`.
pub const CUE_OPERATOR_LABEL: &str = "operator";

/// The signed-in person behind a message, when there is one.
///
/// `Some(id)` for a [`ActorKind::User`] actor and nothing else. An agent-sent
/// message (a crossing referral arrives as one) and a machine credential both
/// answer `None` — neither names a person.
pub fn cue_author_id(by: &Option<Actor>) -> Option<String> {
    match by {
        // CodeRabbit: an agent-authored crossing arrives as an
        // `OperatorMessage` too (the `ActorKind::Agent` arm a few lines below
        // this function's own callers, in `MessageView::project`) — this
        // matched only `User` and fell through to the `operator` fallback for
        // that arm, so the raw view and the cue line both attributed the
        // teammate's own line to "operator". Both actor kinds name a real
        // sender; only a machine credential (`None`, or neither kind) has
        // nobody to name.
        Some(actor) if matches!(actor.kind, ActorKind::User | ActorKind::Agent) => {
            Some(actor.id.clone())
        }
        _ => None,
    }
}

/// **What an agent is told to call the sender** — a stable id, not a screen name.
///
/// This is the single source of truth for that string. `chat_seed::operator_label`
/// delegates to it, [`MessageView::cue_author`] is projected from it, and the
/// per-agent session route ships it to the console, so the byline an agent was
/// handed, the one the seed writes and the one an operator reads in the raw
/// view cannot drift apart.
///
/// # Why an id and not the display name the console shows
///
/// Two reasons, and the second is the one that matters.
///
/// Resolving a name costs a store read per distinct author, on a projection
/// that runs inside the per-company cycle lock and whose whole design note is
/// that it must not do avoidable I/O.
///
/// And a display name is **neither unique nor unforgeable**. The label becomes
/// a per-line attribution prefix ([`prefix_every_line`](crate::harness::built_in::chat_seed),
/// issues #1956 / #2075), so a person who set their display name to a
/// teammate's id would have their own lines prefixed as if that teammate had
/// said them. An id cannot be chosen, so it cannot be chosen to impersonate.
///
/// The console is under the opposite constraint — it shows a person to other
/// people, and an id is not a name there — which is why `author_labels` walks
/// the display ladder and lands on `"someone"`. The two answers are different
/// on purpose; what must never happen is a surface claiming to show one and
/// showing the other.
pub fn cue_author(by: &Option<Actor>) -> String {
    cue_author_id(by).unwrap_or_else(|| CUE_OPERATOR_LABEL.to_string())
}

/// One message in a desk history, independent of transport. Mirrors
/// `frontend/src/lib/chat.ts`. The GraphQL `Message` type and the REST
/// `chat/history` JSON shape both project from this.
#[derive(Clone, Debug)]
pub struct MessageView {
    /// The message id (its EventLog sequence position).
    pub id: String,
    /// The channel the message came in on.
    pub channel: String,
    /// The author label.
    pub author: String,
    /// **What an agent is told to call this row's author** — see
    /// [`cue_author`].
    ///
    /// Deliberately not [`Self::author`]: that one is the display name a
    /// *person* reads, and the two resolve differently on purpose. Projected
    /// here so the per-agent session route can ship the agent's own byline to
    /// the console without the console guessing at it — a raw view that showed
    /// the display name while claiming to show the cue would be asserting the
    /// agent saw something it did not.
    ///
    /// Mirrors `agent_session::body_of` arm for arm; the two are pinned
    /// together by `cue_author_matches_the_envelope_the_agent_is_handed`.
    pub cue_author: String,
    /// The message text.
    pub text: String,
    /// **What the agent was actually handed for this row** — the text before
    /// [`readable_moves`] rewrote it into operator-facing prose.
    ///
    /// Same reasoning as [`Self::cue_author`], applied to the other half of
    /// the cue line: `render_cues` in `agent_session.rs` prepends
    /// `[channel · author] text` using the **pre-rewrite** body (`body_of`
    /// reads the stored event directly, never a projected `MessageView`), so
    /// a surface that claims to show what the model saw — the raw-turns view
    /// — must not feed it [`Self::text`], which has already had `!support
    /// #topic ^3` turned into prose. Equal to [`Self::text`] on every row
    /// `readable_moves` does not touch.
    pub cue_text: String,
    /// When it was journaled, epoch millis.
    pub at_millis: f64,
    /// Whether it is the operator's own message.
    pub mine: bool,
    /// Whether a **person** wrote this line, as opposed to the runtime.
    ///
    /// Not derivable downstream, which is why it is projected here (issue
    /// #1734). [`Self::mine`] answers "did *you* write it" and is per-viewer, so
    /// a colleague's message reads `mine: false` and reaches the console on the
    /// company side of the transcript — indistinguishable there from an agent
    /// reply. [`Self::channel`] cannot separate them either: the offline echo
    /// brain names its own outbound channel `operator`, exactly as the
    /// `OperatorMessage` arm does, so a journaled echo reply and a human's
    /// message carry the same label.
    ///
    /// The host is the only layer that still knows the difference — it is
    /// reading the event variant. Anything downstream is guessing, and the guess
    /// this exists to stop is chat marking a colleague's own words as the echo
    /// brain's, which fabricates an attribution rather than merely missing one.
    ///
    /// `true` for [`CompanyEvent::OperatorMessage`] and nothing else. A
    /// dispatch marker and an agent reply are both `false`: neither was typed by
    /// a person.
    pub by_person: bool,
    /// Set when a crossing referral caused this message (tinyhivemind P15).
    ///
    /// Folded from the `ReferralEnqueued` marker rather than stored on the
    /// message: the marker is written inside the enqueue transaction, before
    /// the child turn exists, so the message cannot carry it at write time.
    pub referred_from: Option<ReferredFrom>,
    /// The crossing this report brought home, when it brought one.
    pub referral_conversation: Option<ReferralConversation>,
    /// The addressees of this row when it is a private aside, else empty.
    ///
    /// Fold input, not output: `fold_asides` reads it to know which rows to
    /// collect, and no DTO carries it — what the console renders is the folded
    /// [`Self::aside_conversation`] on the row the aside rode under.
    pub aside_audience: Vec<String>,
    /// The private exchange this move carried, when it carried one.
    pub aside_conversation: Option<AsideConversation>,
    /// Whether this row may reach only administrators (issue #1781 review,
    /// Codex P1).
    ///
    /// `true` for exactly one shape today: an `owner`-destination workflow
    /// report that fell back to the operator channel because the company has
    /// no mailbox, or no active admin has an address. The ordinary email
    /// branch of that same destination reaches active admins only
    /// (`workflows::delivery::owner_recipients`); this field is what lets the
    /// channel fallback honour the same restriction rather than silently
    /// widening the audience to every signed-in company user. The caller
    /// (`server::operator::chat_history_response`) drops any row with this set
    /// before returning to a non-admin viewer — see
    /// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
    /// for how the underlying event is marked.
    pub admin_only: bool,
    /// The scrubbed processing steps behind a company reply, so a rehydrated
    /// transcript renders the same tool-call timeline the live turn showed.
    /// Empty for operator messages and tool-less replies.
    pub steps: Vec<TurnStep>,
    /// The board card this reply is about (issue #246) — the card the turn
    /// opened, or the dispatched card the turn ran for (#185).
    ///
    /// Projected here so a rehydrated transcript renders the same "card opened"
    /// chip the live turn showed. Both surfaces read it from this one field, so
    /// REST and GraphQL cannot disagree about which messages carry a card
    /// (issue #65's whole point). `None` on operator messages and on every
    /// reply journaled before the field existed.
    ///
    /// Also `None` once the card itself is gone, whoever deleted it — see
    /// [`drop_dead_cards`]. The journal still records that the turn opened a
    /// card, because it did; this field answers the narrower question the
    /// renderer actually asks, which is whether there is still a card to link
    /// to (issue #984).
    pub task_id: Option<String>,
    /// The message this one replies to (issue #364), by that message's own id —
    /// what makes a thread survive a reload rather than living in one browser.
    ///
    /// `None` on a message posted straight into the channel, which is every
    /// message journaled before threads were persisted.
    pub parent_id: Option<String>,
    /// Who reacted to this message with what (issue #364), one row per person
    /// per emoji, oldest reaction first.
    ///
    /// Rows rather than a tally, because a tally cannot answer the two
    /// questions the console actually asks of a reaction — *who* reacted, and
    /// *have I* — and the second is what makes the chip a toggle rather than an
    /// ever-increasing counter. Grouping rows into a count is the renderer's
    /// job; deriving names from a count is impossible.
    pub reactions: Vec<ReactionView>,
    /// Who this message names, in reading order.
    ///
    /// Spans plus a **label**, never a target id: this is the surface a member
    /// reads other members' messages through, and handing every reader the raw
    /// user id of everyone ever mentioned would widen who-sees-what for no gain
    /// the renderer can use. Same discipline as [`ReactionView::by_label`], and
    /// the same reason.
    ///
    /// Empty for a message that mentions nobody, which is every message
    /// journaled before mentions existed.
    pub mentions: Vec<MentionView>,
    /// Files attached to this message (issue #1682), each a durable reference
    /// into the company workspace with the store-computed name / mime / size.
    ///
    /// Projected straight from the stored [`Attachment`] rows — the name and
    /// mime are already the store's, resolved server-side at send time, so this
    /// surface adds no viewer-scoping the way [`MentionView`] does: an
    /// attachment names a file the operator themself put in this company's own
    /// workspace, reachable by the same person through the blob route.
    ///
    /// Empty on an [`AgentReply`](CompanyEvent::AgentReply), a system pill, and
    /// every operator message journaled before this field existed — the shared
    /// [`MessageView`], so REST and GraphQL carry the same rows (issue #65).
    pub attachments: Vec<Attachment>,
}

/// One mention inside one message, as a reader sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionView {
    /// The literal span the author typed, so the renderer highlights what is
    /// actually in the text rather than what the target is called now.
    pub text: String,
    /// Byte offset of [`Self::text`] in the message body.
    pub offset: usize,
    /// Who was named, as a display label — a teammate's id, a person's label, a
    /// desk's name, or `everyone`. Never a raw user id.
    pub label: String,
    /// Whether this mention is the viewer's own — what the console renders as a
    /// highlighted chip and counts as "this message is for me". Relative to the
    /// [`Viewer`], on the same terms [`MessageView::mine`] is.
    ///
    /// True for a direct mention of the viewer **and** for `@everyone`, because
    /// a broadcast is addressed to them too.
    pub mine: bool,
    /// Whether this mention renders but does not ping — a duplicate, a mention
    /// past the cap, or a target that has since left the company.
    pub quiet: bool,
}

/// One person's reaction to one message, as a reader sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReactionView {
    /// The emoji.
    pub emoji: String,
    /// Who reacted, as a display label — never a raw user id, for the same
    /// reason [`author_labels`] never hands out an email.
    pub by_label: String,
    /// Whether this row is the viewer's own. Relative to the [`Viewer`], on the
    /// same terms [`MessageView::mine`] is.
    pub mine: bool,
}

/// How a reaction's author is keyed while folding.
///
/// A signed-in person keys on their user id; anything else — a platform
/// credential, an event journaled before attribution existed — keys on the one
/// shared "operator" identity, which is the same collapse
/// [`MessageView::project`] makes for authorship. Two different machine
/// credentials therefore share a reaction row, which is correct: they are the
/// same principal as far as this company's history is concerned.
fn reaction_actor_key(by: &Option<Actor>) -> String {
    match by {
        Some(actor) if actor.kind == ActorKind::User => format!("user:{}", actor.id),
        _ => "operator".to_string(),
    }
}

/// Folds every [`CompanyEvent::ReactionToggled`] in a log into per-message
/// reaction rows, keyed by the reacted-to message's id.
///
/// Last event per `(message, actor, emoji)` wins — that is what makes the
/// route's explicit `on` flag idempotent — and a row that ends up `off` is
/// dropped entirely rather than kept as a zero. Order is first-set order, so a
/// message's chips do not reshuffle between reads.
struct ReactionFold {
    // (message, actor, emoji) → (position among first-seen keys, currently on).
    state: HashMap<(u64, String, String), (usize, bool)>,
    seen: usize,
}

impl ReactionFold {
    fn observe(&mut self, event: &StoredEvent, wanted: Option<&HashSet<u64>>) {
        let CompanyEvent::ReactionToggled {
            message_seq,
            emoji,
            on,
            by,
        } = &event.event
        else {
            return;
        };
        if wanted.is_some_and(|ids| !ids.contains(&message_seq.value())) {
            return;
        }
        let key = (message_seq.value(), reaction_actor_key(by), emoji.clone());
        match self.state.get_mut(&key) {
            Some(slot) => slot.1 = *on,
            None => {
                self.state.insert(key, (self.seen, *on));
                self.seen += 1;
            }
        }
    }

    fn finish(
        self,
        viewer: &Viewer,
        authors: &HashMap<String, String>,
    ) -> HashMap<String, Vec<ReactionView>> {
        let mut rows: Vec<(usize, u64, String, String)> = self
            .state
            .into_iter()
            .filter(|(_, (_, on))| *on)
            .map(|((message, actor, emoji), (order, _))| (order, message, actor, emoji))
            .collect();
        rows.sort_unstable();

        let mut out: HashMap<String, Vec<ReactionView>> = HashMap::new();
        for (_, message, actor, emoji) in rows {
            let (by_label, mine) = match actor.strip_prefix("user:") {
                Some(user_id) => (
                    authors
                        .get(user_id)
                        .cloned()
                        .unwrap_or_else(|| "someone".to_string()),
                    *viewer == Viewer::User(user_id.to_string()),
                ),
                None => ("operator".to_string(), matches!(viewer, Viewer::Operator)),
            };
            out.entry(message.to_string())
                .or_default()
                .push(ReactionView {
                    emoji,
                    by_label,
                    mine,
                });
        }
        out
    }
}

#[cfg(test)]
fn fold_reactions(
    stored: &[StoredEvent],
    viewer: &Viewer,
    authors: &HashMap<String, String>,
) -> HashMap<String, Vec<ReactionView>> {
    let mut fold = ReactionFold {
        state: HashMap::new(),
        seen: 0,
    };
    for event in stored {
        fold.observe(event, None);
    }
    fold.finish(viewer, authors)
}

impl MessageView {
    /// Projects a stored event for one viewer.
    ///
    /// `authors` maps user id → display label, resolved once per history
    /// rather than per message.
    pub fn project(
        stored: StoredEvent,
        viewer: &Viewer,
        authors: &HashMap<String, String>,
    ) -> Self {
        let id = stored.seq.value().to_string();
        let at_millis = stored.at_millis as f64;
        match stored.event {
            CompanyEvent::AgentReply {
                agent_id,
                text,
                steps,
                task_id,
                parent,
                mentions,
                audience,
                ..
            } => MessageView {
                id,
                channel: agent_id.clone(),
                admin_only: agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR,
                // `body_of`'s `AgentReply` arm names the agent, so this does.
                cue_author: agent_id.clone(),
                author: agent_id,
                // `body_of`'s `AgentReply` arm hands the agent `text.clone()`
                // untouched — clone before `readable_moves` consumes it below.
                cue_text: text.clone(),
                text: readable_moves(text),
                at_millis,
                mine: false,
                // The runtime wrote this, whichever brain produced it.
                by_person: false,
                // Set by the referral fold in `history_for_desk`, never here.
                referred_from: None,
                referral_conversation: None,
                aside_audience: audience,
                aside_conversation: None,
                steps,
                task_id,
                parent_id: parent.map(|seq| seq.value().to_string()),
                reactions: Vec::new(),
                mentions: project_mentions(&mentions, authors, viewer),
                // A reply is the company's own voice and carries no operator
                // upload (issue #1682).
                attachments: Vec::new(),
            },
            CompanyEvent::OperatorMessage {
                text,
                by,
                parent,
                mentions,
                attachments,
                ..
            } => {
                // `voice` is what the console draws the byline from: `senderOf`
                // reads `channel`, not `author`, and treats the values in its
                // COMPANY_VOICE set ("operator", "console", …) as "no distinct
                // speaker — use the room's own name".
                //
                // That is right for a message a person sent. It is wrong for a
                // referral, which arrives authored by a TEAMMATE: falling into
                // that set made design's channel name the speaker, so an
                // engineer asking design read as design talking to itself. An
                // agent-authored line names the agent, exactly as an
                // `AgentReply` already does one arm below.
                let (author, mine, by_person, voice) = match &by {
                    // Sent by a signed-in human.
                    Some(actor) if actor.kind == ActorKind::User => {
                        let label = authors
                            .get(&actor.id)
                            .cloned()
                            .unwrap_or_else(|| "someone".to_string());
                        (
                            label,
                            *viewer == Viewer::User(actor.id.clone()),
                            true,
                            "operator".to_string(),
                        )
                    }
                    // A TEAMMATE sent it — a crossing referral arrives on the
                    // target's desk as a message authored by the agent that
                    // asked (tinyhivemind P15).
                    //
                    // Without this arm it fell to the machine-credential
                    // fallback below and was projected as `operator`, with
                    // `mine: true` for the operator's own view — so a question
                    // engineering asked read as one the person at the console
                    // had asked. That is the same shape as the `agent: None`
                    // → `agent_id: "operator"` defect (#885): a fallback that
                    // means "no person to name" rendered as a specific, wrong
                    // person. `mine` is emphatically false: nobody typed it.
                    Some(actor) if actor.kind == ActorKind::Agent => {
                        (actor.id.clone(), false, false, actor.id.clone())
                    }
                    // Sent with a machine credential, or journaled before
                    // attribution existed. Either way there is no person to
                    // name, and it belongs to whoever holds that credential.
                    _ => (
                        "operator".to_string(),
                        matches!(viewer, Viewer::Operator),
                        true,
                        "operator".to_string(),
                    ),
                };
                MessageView {
                    // Set by the referral fold in `history_for_desk`, never here.
                    referred_from: None,
                    referral_conversation: None,
                    aside_audience: Vec::new(),
                    aside_conversation: None,
                    id,
                    channel: voice,
                    admin_only: false,
                    // The agent's byline for this row, resolved by the one
                    // function that decides it. NOT `author` above: that is the
                    // display name a person reads, and it lands on "someone"
                    // where this lands on the user id.
                    cue_author: cue_author(&by),
                    author,
                    // `body_of`'s `OperatorMessage` arm applies no rewrite
                    // either, so the cue and the rendered text agree here.
                    cue_text: text.clone(),
                    text,
                    at_millis,
                    mine,
                    // Decided with the author above, not assumed: this arm used
                    // to be "the one arm where a person typed it", which stopped
                    // being true when a referral began arriving here authored by
                    // a teammate. `byPerson` gates real behaviour downstream —
                    // the console's inline-reply promotion reads it — so a
                    // teammate's line claiming to be a person's is not cosmetic.
                    by_person,
                    steps: Vec::new(),
                    task_id: None,
                    parent_id: parent.map(|seq| seq.value().to_string()),
                    reactions: Vec::new(),
                    mentions: project_mentions(&mentions, authors, viewer),
                    // Issue #1682: the operator's attached files, carried
                    // through so a reload renders the same chips the live send
                    // showed.
                    attachments,
                }
            }
            // The dispatch terminal (issue #377), as the channel marker a
            // reader needs to see the card settle.
            //
            // A **dedicated arm**, not a lean on the defensive fallback below:
            // that one renders `format!("{other:?}")`, so without this the
            // marker would reach a person as a line of Rust `Debug` output —
            // and it would do so only on reload, which is the half of this
            // feature nobody watches while developing it.
            //
            // Authored as `system` on both keys, which is what makes the
            // console render it as a centred pill rather than a company bubble
            // (`MessageRow`), and `mine: false` because nobody said it.
            // `task_id` carries the card so the pill can link to it — the same
            // field, and therefore the same renderer, an `AgentReply`'s "card
            // opened" chip uses. No new `MessageView` field: this type is
            // shared with the GraphQL `Message` projection, and the reuse is
            // what keeps #377 additive on both wire surfaces at once.
            //
            // No `steps`: a marker is not a turn, so there is no timeline on it.
            //
            // `parent_id` **is** carried, since issue #1890 B. A marker was
            // never threaded because a card recorded no thread to thread it
            // into — not because a marker cannot be threaded — so a card raised
            // inside a thread settled flat in the channel and the thread that
            // asked for the work never showed it finishing. The card carries
            // its root now, the terminal captures it, and this is where it
            // reaches the reader. `None` is still the overwhelmingly common
            // case: it is every card raised straight into a channel.
            CompanyEvent::DeskTaskCompleted {
                task_id,
                column,
                origin_parent,
                ..
            } => MessageView {
                id,
                channel: crate::ports::SYSTEM_AUTHOR.to_string(),
                admin_only: false,
                // `body_of` hands an agent no structural marker, so nothing
                // ever reads this — named rather than left to drift.
                cue_author: crate::ports::SYSTEM_AUTHOR.to_string(),
                author: crate::ports::SYSTEM_AUTHOR.to_string(),
                // `body_of` never delivers this marker to an agent (see the
                // comment on `cue_author` above); equal to `text` for the same
                // reason that one is named rather than left to drift.
                cue_text: dispatch_marker_text(&column),
                text: dispatch_marker_text(&column),
                at_millis,
                mine: false,
                by_person: false,
                // Set by the referral fold in `history_for_desk`, never here.
                referred_from: None,
                referral_conversation: None,
                aside_audience: Vec::new(),
                aside_conversation: None,
                steps: Vec::new(),
                task_id: Some(task_id),
                // Rendered the same way an `OperatorMessage`'s parent is, a few
                // arms up — the console keys a thread off this string and does
                // not care which event minted it.
                parent_id: origin_parent.map(|seq| seq.value().to_string()),
                reactions: Vec::new(),
                mentions: Vec::new(),
                attachments: Vec::new(),
            },
            // `owns` never admits other variants into a history.
            other => MessageView {
                id,
                channel: crate::ports::SYSTEM_AUTHOR.to_string(),
                admin_only: false,
                // `body_of` hands an agent no structural marker, so nothing
                // ever reads this — named rather than left to drift.
                cue_author: crate::ports::SYSTEM_AUTHOR.to_string(),
                author: crate::ports::SYSTEM_AUTHOR.to_string(),
                // `body_of` never delivers this fallback marker to an agent
                // either — same reasoning as the arm above.
                cue_text: format!("{other:?}"),
                text: format!("{other:?}"),
                at_millis,
                mine: false,
                by_person: false,
                // Set by the referral fold in `history_for_desk`, never here.
                referred_from: None,
                referral_conversation: None,
                aside_audience: Vec::new(),
                aside_conversation: None,
                steps: Vec::new(),
                task_id: None,
                parent_id: None,
                reactions: Vec::new(),
                mentions: Vec::new(),
                attachments: Vec::new(),
            },
        }
    }
}

/// Turns stored mentions into what a particular reader should see.
///
/// Two things happen here and nowhere else:
///
/// * **Ids become labels.** A [`MentionTarget::User`] carries a user id, which
///   no member-facing surface hands out; it is resolved through the same
///   `authors` map the byline above the message uses, so a chip and the author
///   line can never disagree about what somebody is called. A target that
///   resolves to nothing falls back to the literal text the author typed, minus
///   its `@` — which is exactly what a reader would have seen anyway.
/// * **`mine` is decided.** Per viewer, and `true` for `@everyone` as well as
///   for a direct mention, because a broadcast is addressed to this reader too.
pub(crate) fn project_mentions(
    mentions: &[Mention],
    authors: &HashMap<String, String>,
    viewer: &Viewer,
) -> Vec<MentionView> {
    mentions
        .iter()
        .map(|mention| {
            let fallback = || mention.text.trim_start_matches('@').to_string();
            let (label, mine) = match &mention.target {
                MentionTarget::Agent { id } => (id.clone(), false),
                MentionTarget::Desk { id } => (id.clone(), false),
                MentionTarget::User { id } => (
                    authors.get(id).cloned().unwrap_or_else(fallback),
                    *viewer == Viewer::User(id.clone()),
                ),
                // Addressed to the room, so it is addressed to whoever is
                // reading — including the operator credential, which is a
                // reader even though it is not a person.
                MentionTarget::Everyone => ("everyone".to_string(), true),
            };
            MentionView {
                text: mention.text.clone(),
                offset: mention.offset,
                label,
                mine,
                quiet: mention.quiet,
            }
        })
        .collect()
}

/// The blast radius of issue #885, for one company.
///
/// Reported rather than repaired. See [`channel_attributed_replies`] for why a
/// repair is not available.
///
/// # The figure is not comparable across the #966 cutover
///
/// Host-authored notices — the approval-overflow line, the `"Acknowledged."`
/// fallback, the failed-continuation report — used to journal under the
/// operator channel, so every one already on disk is counted here as damage.
/// Since #966 they journal under [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR)
/// and are not counted, because they are correct rows and inflating this number
/// with them would make the one figure that has to be trustworthy the least
/// trustworthy one.
///
/// The consequence is a step in the series that nothing on the wire labels: a
/// company's `affected` can fall without a single row being repaired, purely
/// because it stopped minting new false positives. Read a decline across that
/// boundary as "the bleeding stopped", never as "history got better" — no row
/// counted here has ever become attributable, and none can.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AttributionAudit {
    /// Every `AgentReply` inspected.
    pub replies: usize,
    /// Those whose stored `agent_id` names no roster teammate.
    pub affected: usize,
    /// The distinct bad `agent_id` values, with a count each — so an operator
    /// can see at a glance whether they are all `operator` (the #885 shape) or
    /// whether something else is also writing a non-agent into the field.
    pub by_agent_id: std::collections::BTreeMap<String, usize>,
}

impl AttributionAudit {
    /// Folds one page of journal events in.
    ///
    /// Split out from the paging so the rule itself is testable without a
    /// `CompanyRuntime` — the classification is the part that can be silently
    /// wrong, and a store fixture would only obscure it.
    pub fn fold(&mut self, page: &[StoredEvent], is_roster_agent: impl Fn(&str) -> bool) {
        for stored in page {
            let CompanyEvent::AgentReply { agent_id, .. } = &stored.event else {
                continue;
            };
            self.replies += 1;
            if !is_roster_agent(agent_id) {
                self.affected += 1;
                *self.by_agent_id.entry(agent_id.clone()).or_insert(0) += 1;
            }
        }
    }
}

/// Counts desk replies whose author was overwritten with a destination (#885).
///
/// # The rule
///
/// [`CompanyEvent::AgentReply`]'s `agent_id` is documented as *"the agent that
/// produced the reply"*, so a value naming no roster teammate is by definition
/// not an author. That is the whole test, and it is deliberately not `== "operator"`:
/// the same defect on any other channel — a Telegram chat id, a desk slug —
/// produces a different wrong string and has to be counted too.
///
/// # Why this only counts, and never repairs
///
/// **The true author is not recoverable from what is on disk.** `agent_id` was
/// the only field that carried it and it was overwritten in place. Nothing else
/// on the event, and nothing beside it, records who spoke:
///
/// * `steps` — [`TurnStep`] has no agent field;
/// * `task_id` — `None` on exactly these rows (it is set on the dispatch path,
///   which is the one writer that was already correct);
/// * `parent` — names the question, never the answerer;
/// * `chat_id` — the desk, which yields *today's* desk lead. Desk membership is
///   mutable (manifest members unioned with operator-added overlay members), so
///   that is a re-derivation against current state, not a recovery — and it is
///   silently wrong for any desk whose lead has changed since.
/// * the metering store — bucketed per calendar **day** with per-agent
///   aggregates, so it cannot name the author of one message.
///
/// So a backfill would synthesise an author rather than restore one, and a
/// confident wrong name in a transcript is worse than an admitted gap. These
/// rows are ambiguous, permanently, and this reports how many there are.
///
/// # One deliberate false positive
///
/// `CompanyRuntime::announce_continuation_failure` journals a **system** notice
/// as `agent_id: "operator"` on purpose — it is the runtime telling the operator
/// a continuation failed, not an agent speaking. It is indistinguishable from a
/// #885 row on disk, so it is counted here. The count is therefore an upper
/// bound; in practice that notice is rare enough not to move it.
/// Whether a stored `agent_id` names an author we can actually resolve.
///
/// The roster, **plus three ids that are truthful authors without being teammates**.
///
/// [`SYSTEM_AUTHOR`](crate::ports::SYSTEM_AUTHOR) (issue #966) is the runtime
/// speaking for itself — an approval-overflow notice, the `"Acknowledged."`
/// fallback, a failed-continuation report. Those rows are *correct*, so counting
/// them as damage would inflate the one figure in #965 that has to be
/// trustworthy, and would caption a legitimate system message as unattributable.
///
/// `CONFINED_AGENT_ID`
/// is deliberately not a roster id — it names no teammate, carries no manifest
/// grants and cannot be addressed — but a copilot turn genuinely authored its
/// reply, so the id is a truthful author rather than a destination that leaked
/// into the field. Counting it would swap one wrong answer for a permanent
/// false positive, and would make the audit's number drift upward on a company
/// doing nothing wrong.
///
/// [`WORKFLOW_REPLY_AUTHOR`](crate::runtime::WORKFLOW_REPLY_AUTHOR) is the same
/// case as `SYSTEM_AUTHOR`: a delivered workflow report is journaled under it
/// on purpose, not a destination that leaked into the author field, so it must
/// not inflate the count either — and unlike `SYSTEM_AUTHOR`, no roster entry
/// can *ever* shadow it, on this company or any other: the id is hyphenated,
/// so neither a minted slug nor a manifest-declared one can equal it (see the
/// constant's doc).
///
/// [`OWNER_FALLBACK_REPORT_AUTHOR`](crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
/// is the same case again, one level narrower: it is `WORKFLOW_REPLY_AUTHOR`'s
/// own admin-only sibling, journaled when an `owner` report has no mailbox to
/// reach (issue #1781 review, Codex P2) — a legitimate report, deliberately
/// unmintable for the same reason, and it must not inflate the count either.
///
/// This is the single predicate the audit and any presentation of its result
/// must share; two copies would let the count and the rendering disagree about
/// which rows are unknown.
pub fn is_known_author(agent_id: &str, record: &CompanyRecord) -> bool {
    agent_id == crate::ports::CONFINED_AGENT_ID
        || agent_id == crate::ports::SYSTEM_AUTHOR
        || agent_id == crate::runtime::WORKFLOW_REPLY_AUTHOR
        || agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR
        || record.resolve_roster_agent_id(agent_id).is_some()
}

/// `is_admin` gates the same admin-only rows [`history_for_desk`] and
/// [`history_total_for_desk`] already exclude for a non-admin viewer (issue
/// #1781 review, Codex P2): an owner-fallback report is invisible on the
/// transcript and over SSE, but the raw `replies` count previously included
/// it regardless of caller, so a Member watching the count tick up around an
/// owner-fallback delivery could infer a hidden admin-only message exists.
/// Excluded here — before `fold` — so a non-admin's count can never expose
/// that inference.
pub async fn channel_attributed_replies(
    runtime: &CompanyRuntime,
    record: &CompanyRecord,
    is_admin: bool,
) -> Result<AttributionAudit, OpenCompanyError> {
    const PAGE: usize = 512;
    let mut audit = AttributionAudit::default();
    let mut cursor = EventSeq::new(0);
    loop {
        let page = runtime
            .events()
            .read_from(runtime.id(), cursor, PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        let last = page[page.len() - 1].seq;
        if is_admin {
            audit.fold(&page, |agent_id| is_known_author(agent_id, record));
        } else {
            let visible: Vec<StoredEvent> = page
                .into_iter()
                .filter(|stored| !is_admin_only_event(&stored.event))
                .collect();
            audit.fold(&visible, |agent_id| is_known_author(agent_id, record));
        }
        cursor = EventSeq::new(last.value() + 1);
    }
    Ok(audit)
}

/// Loads roster display labels for a company: user id → label.
///
/// Prefers a display name, and falls back to one derived from the email's
/// local part rather than the whole address: a desk history is read by every
/// member, and it should not hand each of them everyone else's email. The
/// ladder is [`UserRecord::display_label`] — the same one the profile pane and
/// the mention picker use, so the same person reads the same way everywhere.
pub async fn author_labels(
    runtime: &CompanyRuntime,
) -> Result<HashMap<String, String>, OpenCompanyError> {
    let users = runtime.users().list_users(runtime.id()).await?;
    Ok(users
        .into_iter()
        .map(|user| {
            let label = user
                .display_label()
                .unwrap_or_else(|| "someone".to_string());
            (user.id, label)
        })
        .collect())
}

/// One desk's message history for one viewer, most-recent last.
///
/// `before_seq` is an opaque EventLog cursor (a sequence position); only
/// messages before it are considered. `first` caps how many of the remaining,
/// most-recent messages come back.
///
/// `is_admin` gates [`MessageView::admin_only`] rows (issue #1781 review,
/// Codex P1): a non-admin viewer never sees one, and the exclusion happens
/// **inside** the paging loop, before a row counts toward `first` — filtering
/// the returned `Vec` afterward would silently short a non-admin's page by
/// however many admin-only rows it held, which is a pagination bug, not
/// merely a display one.
///
/// Shared by the GraphQL `Chat.history` resolver and the REST
/// `GET .../chat/history` route so the two can never disagree about what a
/// desk's history contains (issue #65).
pub async fn history_for_desk(
    runtime: &CompanyRuntime,
    desk_id: &str,
    desk_name: &str,
    viewer: &Viewer,
    before_seq: Option<u64>,
    first: usize,
    is_admin: bool,
) -> Result<Vec<MessageView>, OpenCompanyError> {
    // A page is events rather than messages: a busy company can put unrelated
    // events between two chat turns. Walking backward keeps the newest `first`
    // transcript entries without ever materialising that unrelated journal.
    const EVENT_PAGE: usize = 512;

    // A zero-sized GraphQL page is a valid request, and the REST limit can be
    // clamped to zero. It must not touch the journal merely to construct an
    // empty response.
    let first = first.min(CHAT_HISTORY_PAGE_LIMIT);
    if first == 0 {
        return Ok(Vec::new());
    }

    // One roster read per history, not one per message.
    let authors = author_labels(runtime).await?;
    let mut cursor = before_seq.map(EventSeq::new);
    let mut messages = Vec::with_capacity(first);
    while messages.len() < first {
        let page = runtime
            .events()
            .read_before(runtime.id(), cursor, EVENT_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|event| event.seq);
        for event in page {
            if owns(desk_id, desk_name, &event.event) {
                let message = MessageView::project(event, viewer, &authors);
                // Excluded before it counts toward `first` — see this fn's
                // doc. A non-admin viewer's page fills with the next visible
                // row instead of coming back short.
                if message.admin_only && !is_admin {
                    continue;
                }
                // The room's own bookkeeping, excluded on the same terms and
                // in the same place, for the same reason: filtered after the
                // page was built, an episode's closing row silently shortened
                // every page it appeared on.
                //
                // An episode journals every turn as an ordinary reply by the
                // teammate that took it, and then one closing row under
                // `HIVE_REPORT_AUTHOR`. The turns are the conversation and
                // belong on screen. The closing row is the host summarising a
                // tally whose inputs are those turns — and rendered here it
                // appeared as a *teammate* in a channel where no such teammate
                // exists and none can, the id being hyphenated precisely so no
                // roster id can equal it. The fold already reads it as
                // `SessionAuthor::System`; this makes the console agree.
                //
                // Dropped from the projection, never from the journal: the next
                // episode folds it as context, the memory note keeps the long
                // form, and the chat POST still returns it to its caller. A
                // FAILED turn's notice carries its own reserved id and is not
                // dropped — the turn it describes does not exist, so there is
                // no gap for a reader to notice.
                if message.channel == crate::hivemind::HIVE_REPORT_AUTHOR {
                    continue;
                }
                messages.push(message);
                if messages.len() == first {
                    break;
                }
            }
        }
    }

    // `read_before` supplies each page newest-first, as does `messages` above.
    // Restore chronological order for the renderer before attaching reactions.
    messages.reverse();

    // Reactions necessarily follow their message. Once the displayed window is
    // known, fold only toggles that could affect one of its messages, streaming
    // forward from the window's oldest id through the current tail. The cursor
    // limits *messages*, not the reaction snapshot: a later toggle still
    // changes the state displayed on an older message.
    let wanted: HashSet<u64> = messages
        .iter()
        .filter_map(|message| message.id.parse::<u64>().ok())
        .collect();
    if let Some(oldest) = wanted.iter().min().copied() {
        let mut next = oldest.saturating_add(1);
        let mut fold = ReactionFold {
            state: HashMap::new(),
            seen: 0,
        };
        loop {
            let page = runtime
                .events()
                .read_from(runtime.id(), EventSeq::new(next), EVENT_PAGE)
                .await?;
            if page.is_empty() {
                break;
            }
            for event in &page {
                fold.observe(event, Some(&wanted));
            }
            next = page
                .last()
                .map(|event| event.seq.value().saturating_add(1))
                .unwrap_or(next);
            if page.len() < EVENT_PAGE {
                break;
            }
        }
        let mut reactions = fold.finish(viewer, &authors);
        for message in &mut messages {
            message.reactions = reactions.remove(&message.id).unwrap_or_default();
        }
    }

    drop_dead_cards(runtime, &mut messages).await?;
    attach_referral_origins(runtime, desk_id, &mut messages).await?;
    fold_asides(&mut messages);
    Ok(messages)
}

/// Fold each private aside onto the move it rode under.
///
/// A seat writes its move and may add one `!aside @peer` line beneath it; the
/// host journals that line as its own row carrying an `audience`. Left alone it
/// renders in the transcript as an ordinary message with the raw marker still in
/// its body — which is both a leak of the grammar into the operator's view and a
/// misreading of what happened, since the row was never addressed to the room.
///
/// So the aside rows are lifted out of the transcript and hung on the nearest
/// preceding desk-visible row by the same author — the move they rode under.
///
/// **An orphan is kept, never dropped.** An aside with no move above it (the
/// author's first row, or a history page that begins mid-exchange) stays where it
/// is as an ordinary row. A rendered line in the wrong shape is a cosmetic
/// defect; a dropped one is a lost message, and this projection already refuses
/// that trade for referrals one function below.
pub(crate) fn fold_asides(messages: &mut Vec<MessageView>) {
    if messages
        .iter()
        .all(|message| message.aside_audience.is_empty())
    {
        return;
    }
    let mut folded = Vec::with_capacity(messages.len());
    for message in std::mem::take(messages) {
        if message.aside_audience.is_empty() {
            folded.push(message);
            continue;
        }
        // The move this aside rode under: the nearest row above it that this same
        // seat wrote in the open. Searching by author rather than by adjacency
        // keeps the pairing right when two seats aside in the same round.
        let anchor = folded.iter_mut().rev().find(|earlier| {
            // Same seat, and speaking in the open: an orphaned aside kept
            // above must not become the anchor for the one below it.
            earlier.author == message.author && earlier.aside_audience.is_empty()
        });
        let Some(anchor) = anchor else {
            // Orphan: no move of ours above it. Keep the row.
            folded.push(message);
            continue;
        };
        let line = AsideLine {
            author_id: message.author.clone(),
            text: aside_body(&message.text),
        };
        match &mut anchor.aside_conversation {
            Some(conversation) => {
                for member in &message.aside_audience {
                    if !conversation.members.contains(member) {
                        conversation.members.push(member.clone());
                    }
                }
                conversation.lines.push(line);
            }
            slot @ None => {
                let mut members = vec![message.author.clone()];
                members.extend(message.aside_audience.iter().cloned());
                *slot = Some(AsideConversation {
                    members,
                    lines: vec![line],
                });
            }
        }
    }
    *messages = folded;
}

/// `!aside @peer the body` -> `the body`.
///
/// The marker and the addressee are what the collapsed chip's header already
/// says; leaving them in the body is the leak this fold exists to close.
/// `line_kind` cannot do it — `MOVE_KINDS` deliberately omits `aside`, because a
/// marker the fold discards is not a move anybody made — so the strip is here.
fn aside_body(text: &str) -> String {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix("!aside") else {
        return text.to_string();
    };
    let mut rest = rest.trim_start();
    // Every leading `@name`, not just the first: an aside may name more than one
    // peer when a desk raised `max_members`.
    while let Some(after_at) = rest.strip_prefix('@') {
        let cut = after_at.find(char::is_whitespace).unwrap_or(after_at.len());
        rest = after_at[cut..].trim_start();
    }
    // Trailing space too: the head strip is the only thing standing between the
    // authored line and a chat bubble, and a bubble padded with the whitespace
    // that used to separate `@peer` from the body is a rendering artefact.
    rest.trim_end().to_string()
}

/// Fold each `ReferralEnqueued` marker onto the message it caused
/// (tinyhivemind P15).
///
/// # Why a fold and not a field on the message
///
/// The marker is written INSIDE the enqueue transaction, which is necessarily
/// before the child turn exists — that ordering is what makes it an idempotency
/// marker at all. So the message it causes cannot carry the provenance at write
/// time, and the projection is the only place the two can meet.
///
/// # How they are matched
///
/// A marker names the desk the child runs on and the agent that asked. The
/// child is the first message on that desk, after the marker, authored by that
/// agent. Both are written by one task with nothing in between, so "first
/// after" is exact rather than probabilistic — and the match still requires the
/// author to agree, so an unrelated line landing between them is not adopted.
///
/// # The return leg is an INPUT, not a line in the channel
///
/// One agent speaks in both rooms, and it is the ASKER. It goes to the other
/// desk and asks there under its own name; the desk that answers, answers on
/// its OWN desk and never appears in the room it was asked from. When the asker
/// judges the answer sufficient, it comes home and reports — in its own words,
/// under its own name.
///
/// So the child of a RETURN marker is not a message anyone should read. It is
/// the answer being handed back to the asker so the asker can run a turn on it,
/// and rendering it produced exactly the double the single-accountable-voice
/// rule exists to prevent: the other desk's agent posting its answer verbatim
/// into a room it is not part of, immediately followed by the asker summarising
/// that same answer. The reader saw the same content twice, in two voices, one
/// of which does not belong there.
///
/// The relay is therefore dropped from the projection and its provenance moves
/// onto the asker's report — which is the line a reader wants the chip on
/// anyway, because that is the message whose origin is not otherwise visible.
///
/// **Only when the report actually exists.** If the asker's turn has not landed
/// yet, or failed, the relay renders as it did before. A rendered line in the
/// wrong voice is a cosmetic defect; a dropped one is a lost answer, and this
/// projection already refuses that trade once (see the orphan arm below).
async fn attach_referral_origins(
    runtime: &CompanyRuntime,
    desk_id: &str,
    messages: &mut Vec<MessageView>,
) -> Result<(), OpenCompanyError> {
    let Some(oldest) = messages
        .iter()
        .filter_map(|m| m.id.parse::<u64>().ok())
        .min()
    else {
        return Ok(());
    };
    // Two events back was enough while this only needed the marker sitting just
    // before the first visible row. Reading the crossing itself needs the
    // OUTBOUND leg too, and that is a round trip earlier — the ask, the far
    // desk's turn, the answer — with whatever else the company journalled in
    // between. `LOOKBACK` is a bounded widening, not a guarantee: a crossing
    // whose ask fell outside it renders with the answer alone, which is what
    // this showed before the question was captured at all.
    const LOOKBACK: u64 = 64;
    let page = runtime
        .events()
        .read_from(
            runtime.id(),
            EventSeq::new(oldest.saturating_sub(LOOKBACK)),
            4096,
        )
        .await?;
    // Relays that a report has superseded, dropped once the scan is done —
    // removing inside the loop would invalidate the positions it is still using.
    let mut relayed: Vec<String> = Vec::new();
    for (index, stored) in page.iter().enumerate() {
        let CompanyEvent::ReferralEnqueued {
            from_desk,
            from_desk_name,
            asker,
            asker_label,
            trigger_sequence,
            to_desk,
            target,
            returning,
            answers,
            conversation,
        } = &stored.event
        else {
            continue;
        };
        if to_desk != desk_id {
            continue;
        }
        // The row the marker is about, in either shape the two paths write.
        //
        // A chat-path crossing lands as an `OperatorMessage` on the target desk
        // authored by the agent that asked — it IS that agent speaking there. A
        // crossing raised inside a room lands as an `AgentReply` under the
        // reserved `hive-referral` author, because the room folds it into its
        // own transcript for the next speaker to read. Same event, two shapes,
        // and a matcher that knew only the first left every room crossing
        // unattached: marker on the journal, nothing on the message.
        let Some(child) = page[index + 1..].iter().find(|later| match &later.event {
            CompanyEvent::OperatorMessage { chat, by, .. } => {
                chat.as_deref() == Some(to_desk.as_str())
                    && by.as_ref().is_some_and(|actor| actor.id == *asker)
            }
            CompanyEvent::AgentReply {
                chat_id, agent_id, ..
            } => chat_id == to_desk && agent_id == crate::hivemind::HIVE_REFERRAL_AUTHOR,
            _ => false,
        }) else {
            continue;
        };
        let child_id = child.seq.value().to_string();
        // Whether this crossing went to a person. Read off the FORWARD, which
        // is the leg that names a pair conversation; a return names none, so it
        // cannot answer this about itself.
        let direct = match returning {
            true => answers
                .and_then(|seq| page.iter().find(|st| st.seq.value() == seq))
                .is_some_and(|fwd| {
                    matches!(
                        &fwd.event,
                        CompanyEvent::ReferralEnqueued {
                            conversation: Some(_),
                            ..
                        }
                    )
                }),
            false => conversation.is_some(),
        };
        let origin = ReferredFrom {
            desk_id: from_desk.clone(),
            desk_name: from_desk_name.clone(),
            asker_id: asker.clone(),
            asker_label: asker_label.clone(),
            sequence: *trigger_sequence,
            returning: *returning,
            direct,
        };

        // On a return, the chip belongs on the ASKER's report — the first thing
        // they say on this desk after the answer reached them. `target` is who
        // the referral was routed to, which on a return is the agent that asked
        // in the first place, so this needs no second notion of "the asker".
        let report = returning
            .then(|| {
                messages
                    .iter()
                    .filter(|m| m.id.parse::<u64>().is_ok_and(|seq| seq > child.seq.value()))
                    .find(|m| m.channel == *target && !m.by_person)
                    .map(|m| m.id.clone())
            })
            .flatten();

        match report {
            Some(id) => {
                // The answer's own words, before the row is dropped. The host's
                // note rides the same text and is addressed to the asker alone
                // ("you are the only one who has seen it"), so it is stripped
                // here for the same reason the fallback strips it.
                let answer = strip_relay_note(&child.event);
                // The question that opened this crossing: the forward leg that
                // left THIS desk for the desk now answering, addressed to the
                // agent now speaking. Matched on the pair rather than on
                // sequence order, because a room may have more than one
                // crossing open and their legs interleave.
                // The forward this answers, named by the marker itself when the
                // host recorded it (`answers`). One event, fetched by sequence:
                // no window to fall outside of, and no second copy of the match
                // rules to drift from the host's.
                // The forward this answers, by the sequence the host recorded.
                //
                // Read straight from the journal when the page does not reach
                // it, which is the whole point of recording a pointer: a
                // crossing whose ask is older than the window is exactly the
                // case the scan could not serve. Two events, because a forward
                // marker is immediately followed by the row it caused.
                let forward_leg: Vec<StoredEvent> = match answers {
                    Some(seq) if !page.iter().any(|st| st.seq.value() == *seq) => runtime
                        .events()
                        .read_from(runtime.id(), EventSeq::new(*seq), 2)
                        .await
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let reachable: Vec<&StoredEvent> = page.iter().chain(forward_leg.iter()).collect();
                let paired = answers.and_then(|seq| {
                    let forward = reachable.iter().find(|stored| stored.seq.value() == seq)?;
                    // A crossing that ran in the pair's own thread keeps BOTH
                    // sides there, in order, and neither desk carries them. That
                    // is the whole exchange already — no delivered copy to hunt
                    // for, no trigger row to fall back to.
                    if let CompanyEvent::ReferralEnqueued {
                        conversation: Some(thread),
                        ..
                    } = &forward.event
                    {
                        let said: Vec<String> = reachable
                            .iter()
                            .filter(|stored| stored.seq > forward.seq)
                            .filter(|stored| {
                                matches!(
                                    &stored.event,
                                    CompanyEvent::AgentReply { chat_id, .. } if chat_id == thread
                                )
                            })
                            .filter_map(|stored| strip_relay_note(&stored.event))
                            .collect();
                        // The question is the first thing said there; anything
                        // after it is the answer, which may run to several turns.
                        if let Some(question) = said.first() {
                            return Some(question.clone());
                        }
                        return None;
                    }
                    // The question, in whichever form this crossing left behind.
                    //
                    // A chat-path crossing delivers a copy onto the far desk —
                    // the asking agent speaking there — so that copy is the
                    // question, already addressed to its reader.
                    let delivered = reachable
                        .iter()
                        .find(|later| {
                            later.seq > forward.seq
                                && matches!(
                                    &later.event,
                                    CompanyEvent::OperatorMessage { chat, by, .. }
                                        if chat.as_deref() == Some(from_desk.as_str())
                                            && by.as_ref().is_some_and(|a| a.id == *target)
                                )
                        })
                        .and_then(|m| strip_relay_note(&m.event));
                    // A room's crossing delivers no copy: the far seat simply
                    // takes a turn, and only its answer is journalled. What
                    // asked is the member's own line back home — the committed
                    // reply the marker was raised from, which `trigger_sequence`
                    // names exactly.
                    delivered.or_else(|| {
                        let CompanyEvent::ReferralEnqueued {
                            trigger_sequence, ..
                        } = &forward.event
                        else {
                            return None;
                        };
                        reachable
                            .iter()
                            .find(|stored| stored.seq.value() == *trigger_sequence)
                            .and_then(|m| strip_relay_note(&m.event))
                    })
                });
                // Markers written before `answers` existed carry no pointer, so
                // they still pair by scanning. Same result when the ask is in
                // range, and the same under-count when it is not — this path is
                // the old behaviour kept for old rows, not a second opinion.
                let question = paired.or_else(|| page
                    .iter()
                    .take(index)
                    .rev()
                    .find_map(|earlier| match &earlier.event {
                        CompanyEvent::ReferralEnqueued {
                            from_desk: out_from,
                            to_desk: out_to,
                            asker: out_asker,
                            target: out_target,
                            returning: false,
                            ..
                        } if out_from == desk_id
                            && out_to == from_desk
                            && out_asker == target
                            && out_target == asker =>
                        {
                            page[..index]
                                .iter()
                                .find(|later| {
                                    later.seq > earlier.seq
                                        && matches!(
                                            &later.event,
                                            CompanyEvent::OperatorMessage { chat, by, .. }
                                                if chat.as_deref() == Some(out_to.as_str())
                                                    && by.as_ref().is_some_and(|a| a.id == *out_asker)
                                        )
                                })
                                .map(|m| strip_relay_note(&m.event))
                        }
                        _ => None,
                    })
                    .flatten());
                let mut lines = Vec::new();
                // A room's question is a committed MOVE — `!question @#triage
                // …` — because that is how it was said. The move grammar is
                // addressed to the fold, not to a person reading a transcript,
                // and it is stripped everywhere else a person sees a room's
                // words; a crossing is no different.
                if let Some(text) = question {
                    lines.push(ReferralLine {
                        author_id: target.clone(),
                        author_label: String::new(),
                        text: readable_moves(text),
                        outbound: true,
                    });
                }
                if let Some(text) = answer {
                    lines.push(ReferralLine {
                        author_id: asker.clone(),
                        author_label: asker_label.clone(),
                        text: readable_moves(text),
                        outbound: false,
                    });
                }
                relayed.push(child_id);
                if let Some(view) = messages.iter_mut().find(|m| m.id == id) {
                    if !lines.is_empty() {
                        view.referral_conversation = Some(ReferralConversation {
                            direct,
                            asker_id: target.clone(),
                            other_id: asker.clone(),
                            other_desk_id: from_desk.clone(),
                            other_desk_name: from_desk_name.clone(),
                            lines,
                        });
                    }
                    view.referred_from = Some(origin);
                }
            }
            None => {
                if let Some(view) = messages.iter_mut().find(|m| m.id == child_id) {
                    // Rendering a relay at all is the fallback; rendering the
                    // note the host appended to it would publish text written
                    // FOR the asker — "you are the only one who has seen it" —
                    // in a channel, over the name of an agent that is not even
                    // on this desk. Only the other desk's own words survive.
                    //
                    // Through the shared helper, not a second copy of the split:
                    // two readers of one rule is how a marker change leaves one
                    // path publishing a private note. It also drops a relay
                    // whose words are only whitespace, which the hand-rolled
                    // split kept as an empty line.
                    if let Some(words) = strip_relay_note(&child.event) {
                        view.text = words;
                    }
                    view.referred_from = Some(origin);
                }
            }
        }
    }
    messages.retain(|m| !relayed.contains(&m.id));
    Ok(())
}

/// An agent line's own words, with the host's private note to the asker removed.
///
/// The note is appended to the SAME text the other desk wrote and is addressed
/// to the asker alone, so anything published to a channel — the relay fallback,
/// or a crossing folded onto a report — has to cut it off at the marker. One
/// function, so the two readers cannot disagree about where the words end.
fn strip_relay_note(event: &CompanyEvent) -> Option<String> {
    let text = match event {
        CompanyEvent::OperatorMessage { text, .. } => text,
        // The shape a room's crossing takes; see the matcher in
        // `attach_referral_origins`.
        CompanyEvent::AgentReply { text, .. } => text,
        _ => return None,
    };
    let words = text
        .split_once(crate::ports::types::RELAY_NOTE_MARKER)
        .map_or(text.as_str(), |(answer, _)| answer)
        .trim();
    (!words.is_empty()).then(|| words.to_string())
}

/// Renders a deliberation turn for a person, leaving every other reply alone.
///
/// A room's grammar — `!move`, `#topic`, `^N`, `>N` — is addressed to the fold
/// and was reaching the operator verbatim: `!support #lazy-load ^3 agreed`
/// rendered as-is in a chat window. Each line that carries a move is rewritten
/// to a plain-English lead; a line that carries none passes through untouched,
/// which is every reply on every desk that does not deliberate.
///
/// Line by line, because a turn may pair prose with its move, and only the
/// marked line is grammar.
///
/// **The journal keeps the original.** The fold reads markers off the stored
/// line, so this rewrite lives here and nowhere earlier — a room whose own
/// transcript had been cleaned could not count itself.
pub(crate) fn readable_moves(text: String) -> String {
    if !text
        .lines()
        .any(|line| crate::hivemind::line_kind(line).is_some())
    {
        return text;
    }
    text.lines()
        .map(|line| crate::hivemind::readable(line).unwrap_or_else(|| line.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Blanks `task_id` on any row naming a card the board no longer has
/// (issue #984).
///
/// # Why this is a projection concern and not a write
///
/// The obvious fix — clear `task_id` on the journaled rows when the card is
/// deleted — is not available, and it is worth saying why so nobody reaches for
/// it later. `task_id` is not a column on a mutable chat row: it is a field of
/// the [`CompanyEvent::AgentReply`] that *happened*, and the journal is
/// append-only. Rewriting it would be editing history to record that a turn
/// never opened a card, when it did.
///
/// So the id stays in the journal and the **projection** stops reporting it once
/// the card is gone. That is also strictly more correct than a write would have
/// been:
///
/// - It covers a card deleted by **any** path, not just the chat chip — the
///   board's own `TaskEditDialog` delete leaves exactly the same stale chip, and
///   always did.
/// - It covers cards deleted **before** this change, which no write-time fix
///   could reach.
/// - It cannot drift: there is one board, read at render time, rather than a
///   denormalised copy that a missed call site leaves stale.
///
/// Without this a dismissal survives only until the next full reload:
/// `transcripts` is React state and is never serialised, but the console
/// rehydrates from this projection (`lib/chat.ts`'s `fromHistory`) and merges by
/// message id, so an empty transcript takes every row back — chip included. The
/// chip would return pointing at a `404`, which reads as the delete having
/// failed.
///
/// One board read per history, and only when the window actually carries a
/// card — the same shape as the single roster read above, not a read per
/// message.
async fn drop_dead_cards(
    runtime: &CompanyRuntime,
    messages: &mut [MessageView],
) -> Result<(), OpenCompanyError> {
    if !messages.iter().any(|message| message.task_id.is_some()) {
        return Ok(());
    }

    let live: HashSet<String> = runtime
        .tasks()
        .list(runtime.id())
        .await?
        .into_iter()
        .map(|task| task.id)
        .collect();

    for message in messages {
        if message
            .task_id
            .as_deref()
            .is_some_and(|id| !live.contains(id))
        {
            message.task_id = None;
        }
    }
    Ok(())
}

/// Counts a desk's messages before a cursor without materialising them.
///
/// GraphQL's [`Page`](crate::server::graphql::pagination::Page) exposes an
/// unpaginated `total`, while the REST transcript endpoint deliberately does
/// not. Keep that potentially full journal walk out of [`history_for_desk`],
/// so bounded transcript readers stop as soon as their requested window is
/// complete.
///
/// `is_admin` excludes an owner-fallback report the same way
/// [`history_for_desk`]'s `is_admin` param excludes it from `items` (issue
/// #1781 review, Codex P2): without this, a non-admin querying a GraphQL desk
/// that holds one — notably a grandfathered real desk at the literal
/// `operator` id — got a `total` counting a row `items` had already hidden,
/// which both breaks `Page.total`'s item-count contract and reveals that a
/// hidden admin report exists.
pub async fn history_total_for_desk(
    runtime: &CompanyRuntime,
    desk_id: &str,
    desk_name: &str,
    before_seq: Option<u64>,
    is_admin: bool,
) -> Result<i32, OpenCompanyError> {
    const EVENT_PAGE: usize = 512;

    let mut next = EventSeq::new(0);
    let mut total = 0i32;
    loop {
        let page = runtime
            .events()
            .read_from(runtime.id(), next, EVENT_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        for event in &page {
            if before_seq.is_some_and(|before| event.seq.value() >= before) {
                return Ok(total);
            }
            if !owns(desk_id, desk_name, &event.event) {
                continue;
            }
            // Same admin-only exclusion `MessageView::project` applies to
            // `history_for_desk`'s rows (see `is_admin_only_event`'s doc).
            if !is_admin && is_admin_only_event(&event.event) {
                continue;
            }
            total = total.saturating_add(1);
        }
        let Some(last) = page.last() else {
            break;
        };
        next = EventSeq::new(last.seq.value().saturating_add(1));
        if page.len() < EVENT_PAGE {
            break;
        }
    }
    Ok(total)
}

/// Whether `event` is the owner-fallback report — admin-only on both
/// [`history_for_desk`] (via [`MessageView::project`]'s `admin_only` field,
/// which applies the identical `agent_id == OWNER_FALLBACK_REPORT_AUTHOR`
/// check inline) and [`history_total_for_desk`]'s count (issue #1781 review,
/// Codex P2), so the two projections of the same journal cannot disagree
/// about which rows a non-admin is shown.
fn is_admin_only_event(event: &CompanyEvent) -> bool {
    matches!(
        event,
        CompanyEvent::AgentReply { agent_id, .. }
            if agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR
    )
}

#[cfg(test)]
mod test {
    use super::{AsideConversation, MessageView, aside_body, fold_asides};

    /// A desk-visible row by `author`, or an aside when `to` names somebody.
    fn row(id: &str, author: &str, text: &str, to: &[&str]) -> MessageView {
        MessageView {
            id: id.to_owned(),
            channel: author.to_owned(),
            admin_only: false,
            cue_author: author.to_owned(),
            author: author.to_owned(),
            cue_text: text.to_owned(),
            text: text.to_owned(),
            at_millis: 0.0,
            mine: false,
            by_person: false,
            referred_from: None,
            referral_conversation: None,
            aside_audience: to.iter().map(|id| (*id).to_owned()).collect(),
            aside_conversation: None,
            steps: Vec::new(),
            task_id: None,
            parent_id: None,
            reactions: Vec::new(),
            mentions: Vec::new(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn an_aside_folds_onto_the_move_it_rode_under() {
        let mut messages = vec![
            row("1", "exchanges", "!propose #swap the clicky variant", &[]),
            row(
                "2",
                "exchanges",
                "!aside @refunds the difference is -$16.63",
                &["refunds"],
            ),
            row("3", "refunds", "!support #swap ^1", &[]),
        ];
        fold_asides(&mut messages);

        // The aside is lifted out of the transcript...
        assert_eq!(
            messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["1", "3"],
            "the aside row no longer stands in the desk's own conversation"
        );
        // ...and hangs on its author's move, with the marker head stripped.
        let Some(AsideConversation { members, lines }) = &messages[0].aside_conversation else {
            panic!("the move carries the aside");
        };
        assert_eq!(members, &["exchanges".to_owned(), "refunds".to_owned()]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "the difference is -$16.63");
        assert!(
            !lines[0].text.contains("!aside"),
            "the grammar never reaches the operator's view"
        );
    }

    #[test]
    fn an_orphan_aside_is_kept_rather_than_dropped() {
        // No move above it — a page that begins mid-exchange. A line in the wrong
        // shape beats a line nobody can read.
        let mut messages = vec![row(
            "1",
            "exchanges",
            "!aside @refunds mid-page",
            &["refunds"],
        )];
        fold_asides(&mut messages);
        assert_eq!(messages.len(), 1, "the row survives");
        assert!(messages[0].aside_conversation.is_none());
    }

    #[test]
    fn an_aside_never_hangs_on_another_seats_move() {
        let mut messages = vec![
            row("1", "refunds", "!propose #refund take the return", &[]),
            row(
                "2",
                "exchanges",
                "!aside @refunds are you sure?",
                &["refunds"],
            ),
        ];
        fold_asides(&mut messages);
        // `exchanges` has no move above it, so its aside stays put rather than
        // being attributed to the seat that happened to speak last.
        assert_eq!(messages.len(), 2);
        assert!(messages[0].aside_conversation.is_none());
    }

    #[test]
    fn aside_body_strips_every_addressee_and_leaves_other_text_alone() {
        assert_eq!(aside_body("!aside @a @b the point"), "the point");
        assert_eq!(aside_body("  !aside   @a   spaced  "), "spaced");
        // Not an aside: untouched, including a marker this host does not police.
        assert_eq!(aside_body("!propose #x y"), "!propose #x y");
        assert_eq!(aside_body("plain prose"), "plain prose");
    }

    use super::*;
    use crate::ports::tasks::{
        COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_PLANNING,
        COLUMN_TODO,
    };
    use crate::ports::types::Actor;

    fn agent_reply(chat_id: &str) -> CompanyEvent {
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: None,
            chat_id: chat_id.to_string(),
            agent_id: "ceo".to_string(),
            text: "hi".to_string(),
            steps: Vec::new(),
        }
    }

    /// `None` is the shape the chat route stores for an unaddressed post.
    fn operator_message(chat: Option<&str>) -> CompanyEvent {
        CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: chat.map(str::to_string),
            deliverable: None,
            attachments: Vec::new(),
        }
    }

    /// The whole difference between the two predicates, in one place.
    ///
    /// `same_conversation` folds a missing id into General because an
    /// unaddressed *message* went to the company-wide line.
    /// `stamped_conversation_is` refuses to, because a missing id *stamped on a
    /// record* means no conversation produced it — and handing those to General
    /// is what let a thread-less parked blocker eat a founder's first line in
    /// `#general` (B-059).
    #[test]
    fn a_stamped_origin_of_none_names_no_conversation_including_general() {
        for desk in [GENERAL_DESK, MAIN_THREAD_ID, "general", ""] {
            assert!(
                same_conversation(None, Some(desk)),
                "an unaddressed message still folds into General ({desk:?})"
            );
            assert!(
                !stamped_conversation_is(None, desk),
                "but a record stamped with no conversation belongs to none, {desk:?} included"
            );
        }
        assert!(!stamped_conversation_is(None, "engineering"));
    }

    /// Everything that *is* stamped compares exactly as `same_conversation`
    /// does, so the carve-out cannot quietly become "refuse everything".
    #[test]
    fn a_stamped_origin_folds_general_and_compares_every_other_desk_verbatim() {
        for origin in [GENERAL_DESK, MAIN_THREAD_ID, "general", ""] {
            for desk in [GENERAL_DESK, MAIN_THREAD_ID, "general", ""] {
                assert!(
                    stamped_conversation_is(Some(origin), desk),
                    "every spelling of General is one conversation: {origin:?} vs {desk:?}"
                );
            }
        }
        assert!(stamped_conversation_is(Some("dm:eng"), "dm:eng"));
        assert!(!stamped_conversation_is(Some("dm:eng"), "dm:ops"));
        assert!(!stamped_conversation_is(Some("dm:eng"), GENERAL_DESK));
        assert!(
            !stamped_conversation_is(Some("Engineering"), "engineering"),
            "a desk id is opaque — the General fold is not a licence to loosen the rest"
        );
    }

    #[test]
    fn general_desk_owns_agent_replies_under_general_and_main() {
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &agent_reply(GENERAL_DESK)));
        assert!(owns(
            GENERAL_DESK,
            GENERAL_DESK,
            &agent_reply(MAIN_THREAD_ID)
        ));
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &agent_reply("")));
        assert!(!owns(GENERAL_DESK, GENERAL_DESK, &agent_reply("strategy")));
    }

    /// The console asks for its default line as `?desk=main`, which resolves to
    /// `("main", "main")` — no group chat is named `main` — so the desk side has
    /// to fold too (issue #435).
    ///
    /// The pair that made this reachable: an unaddressed chat post journals the
    /// operator message with `chat: None` and its answer with
    /// `chat_id: "General"`, so before this both halves of that conversation were
    /// missing from the one transcript that should hold them.
    #[test]
    fn the_main_line_owns_what_was_journaled_under_general() {
        for stored in [GENERAL_DESK, MAIN_THREAD_ID, ""] {
            assert!(
                owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &agent_reply(stored)),
                "a reply stored as `{stored}` belongs to the main line",
            );
            assert!(
                owns(
                    MAIN_THREAD_ID,
                    MAIN_THREAD_ID,
                    &operator_message(Some(stored))
                ),
                "an operator message stored as `{stored}` belongs to the main line",
            );
        }
        // The unaddressed post itself — the case that produces the pair above.
        assert!(owns(
            MAIN_THREAD_ID,
            MAIN_THREAD_ID,
            &operator_message(None)
        ));

        // …and the fold stops at the General family: a named desk's traffic
        // does not join the main line, in either direction.
        assert!(!owns(
            MAIN_THREAD_ID,
            MAIN_THREAD_ID,
            &agent_reply("strategy")
        ));
        assert!(!owns(
            "strategy",
            "Strategy desk",
            &operator_message(Some(GENERAL_DESK))
        ));
    }

    #[test]
    fn non_general_desk_only_owns_its_own_id_or_name() {
        assert!(owns("strategy", "Strategy desk", &agent_reply("strategy")));
        assert!(owns(
            "strategy",
            "Strategy desk",
            &agent_reply("Strategy desk")
        ));
        assert!(!owns(
            "strategy",
            "Strategy desk",
            &agent_reply(MAIN_THREAD_ID)
        ));
        assert!(!owns("strategy", "Strategy desk", &agent_reply("")));
    }

    #[test]
    fn general_desk_owns_every_operator_message() {
        let event = CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".to_string(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u1".to_string(),
            }),
            chat: Some(MAIN_THREAD_ID.to_string()),
            deliverable: None,
            attachments: Vec::new(),
        };
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    // Regression: issue — operator messages vanished on reload because the read
    // filter ignored the stored chat id.
    #[test]
    fn main_thread_owns_operator_messages_it_stored() {
        let event = CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: Some(MAIN_THREAD_ID.to_string()),
            deliverable: None,
            attachments: Vec::new(),
        };
        // The console queries the main thread with desk = ("main", "main").
        assert!(owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event));
        // And it is still owned when read under the General desk's own id/name.
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        // But it must not leak into an unrelated desk.
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    #[test]
    fn desk_addressed_operator_message_belongs_to_that_desk() {
        let event = CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: Some("strategy".to_string()),
            deliverable: None,
            attachments: Vec::new(),
        };
        assert!(owns("strategy", "Strategy desk", &event));
        assert!(!owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event));
    }

    /* ---- issue #364: threads and reactions ---- */

    fn user(id: &str) -> Option<Actor> {
        Some(Actor {
            kind: ActorKind::User,
            id: id.to_string(),
        })
    }

    fn at(seq: u64, event: CompanyEvent) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: crate::ports::types::CompanyId::new("acme"),
            event,
            at_millis: 1_700_000_000_000 + seq,
        }
    }

    fn reaction(seq: u64, message: u64, emoji: &str, on: bool, by: Option<Actor>) -> StoredEvent {
        at(
            seq,
            CompanyEvent::ReactionToggled {
                message_seq: EventSeq::new(message),
                emoji: emoji.to_string(),
                on,
                by,
            },
        )
    }

    fn labels() -> HashMap<String, String> {
        HashMap::from([
            ("u1".to_string(), "Ada".to_string()),
            ("u2".to_string(), "Grace".to_string()),
        ])
    }

    /// Two people reacting with the same emoji are two rows, not a count of
    /// two, and only the reader's own row is `mine` — which is the whole reason
    /// the durable record is per-person.
    #[test]
    fn reactions_fold_into_one_row_per_person() {
        let log = vec![
            reaction(10, 4, "👍", true, user("u1")),
            reaction(11, 4, "👍", true, user("u2")),
        ];
        let folded = fold_reactions(&log, &Viewer::User("u1".to_string()), &labels());
        let rows = folded.get("4").expect("message 4 has reactions");
        assert_eq!(
            rows,
            &vec![
                ReactionView {
                    emoji: "👍".to_string(),
                    by_label: "Ada".to_string(),
                    mine: true,
                },
                ReactionView {
                    emoji: "👍".to_string(),
                    by_label: "Grace".to_string(),
                    mine: false,
                },
            ]
        );

        // The same log read by the other person flips only `mine`.
        let folded = fold_reactions(&log, &Viewer::User("u2".to_string()), &labels());
        let mine: Vec<bool> = folded["4"].iter().map(|r| r.mine).collect();
        assert_eq!(mine, vec![false, true]);
    }

    /// The last event per (message, person, emoji) wins, so a clear removes the
    /// row and a repeated set leaves exactly one — which is what makes the
    /// route's explicit `on` flag idempotent rather than a toggle that drifts.
    #[test]
    fn reactions_fold_to_the_last_event_per_person_and_emoji() {
        let log = vec![
            reaction(10, 4, "👍", true, user("u1")),
            reaction(11, 4, "👍", true, user("u1")),
            reaction(12, 4, "🎉", true, user("u1")),
            reaction(13, 4, "🎉", false, user("u1")),
        ];
        let folded = fold_reactions(&log, &Viewer::User("u1".to_string()), &labels());
        let emojis: Vec<&str> = folded["4"].iter().map(|r| r.emoji.as_str()).collect();
        assert_eq!(emojis, vec!["👍"], "a cleared reaction leaves no row");
    }

    /// A reaction made with a machine credential reads back as the operator's,
    /// exactly as an unattributed message does — the same collapse `project`
    /// makes for authorship, so the two surfaces cannot disagree about who a
    /// credential is.
    #[test]
    fn an_unattributed_reaction_belongs_to_the_operator() {
        let log = vec![reaction(10, 4, "👀", true, None)];
        let folded = fold_reactions(&log, &Viewer::Operator, &labels());
        assert_eq!(folded["4"][0].by_label, "operator");
        assert!(folded["4"][0].mine);
        // …and is nobody's own when a signed-in person reads it.
        let folded = fold_reactions(&log, &Viewer::User("u1".to_string()), &labels());
        assert!(!folded["4"][0].mine);
    }

    fn mention(target: MentionTarget, text: &str, offset: usize) -> Mention {
        Mention {
            target,
            text: text.to_string(),
            offset,
            quiet: false,
        }
    }

    fn message_mentioning(mentions: Vec<Mention>) -> CompanyEvent {
        CompanyEvent::OperatorMessage {
            mentions,
            parent: None,
            text: "ping".to_string(),
            by: None,
            chat: Some("studio".to_string()),
            deliverable: None,
            attachments: Vec::new(),
        }
    }

    /// Who *typed* a line is a fact only the host still holds (issue #1734).
    ///
    /// Every downstream shortcut for it is wrong, and the two obvious ones are
    /// wrong in ways that look right:
    ///
    /// * `mine` is per-viewer, so a colleague's own message is `mine: false`
    ///   and lands on the company side of their reader's transcript, beside the
    ///   agent replies.
    /// * `channel == "operator"` collides head-on. The offline echo brain names
    ///   its own outbound channel `operator` (`brain::echo`), exactly as this
    ///   arm does, so a journaled echo reply and a human's message carry the
    ///   same label. A console that split on it marked neither, which suppressed
    ///   the marker on precisely the replies it exists for — caught in a browser
    ///   against a live host, not by a unit test.
    ///
    /// So the projection says it, and this test pins both directions with the
    /// echo brain's own channel label in play, because that is the collision.
    #[test]
    fn only_a_persons_message_is_projected_as_by_person() {
        let typed = MessageView::project(
            at(
                1,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "on it".to_string(),
                    by: Some(Actor {
                        kind: ActorKind::User,
                        id: "u1".to_string(),
                    }),
                    chat: Some("studio".to_string()),
                    deliverable: None,
                    attachments: Vec::new(),
                },
            ),
            // Projected for *another* reader, which is the case that matters:
            // for them this is `mine: false` and nothing else distinguishes it.
            &Viewer::User("u2".to_string()),
            &labels(),
        );
        assert!(typed.by_person, "a person typed this");
        assert!(!typed.mine, "and it is not this reader's own line");

        // The echo brain's reply as the runtime journals it: an `AgentReply`
        // whose agent id is the outbound channel the brain named — `operator`,
        // the very label the arm above hardcodes.
        let echoed = MessageView::project(
            at(
                2,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: "studio".to_string(),
                    agent_id: "operator".to_string(),
                    text: "You said: on it".to_string(),
                    steps: Vec::new(),
                },
            ),
            &Viewer::User("u2".to_string()),
            &labels(),
        );
        assert!(!echoed.by_person, "no person typed the echo brain's reply");
        assert_eq!(
            echoed.channel, typed.channel,
            "the collision is real: the channel label cannot tell these apart",
        );
    }

    /// A person's mention reaches a reader as a **label**, never as the user id
    /// it is stored under — the same rule `by_label` follows for reactions.
    #[test]
    fn project_resolves_a_person_to_a_label_and_never_to_an_id() {
        let view = MessageView::project(
            at(
                7,
                message_mentioning(vec![mention(
                    MentionTarget::User {
                        id: "u1".to_string(),
                    },
                    "@Ada",
                    0,
                )]),
            ),
            &Viewer::Operator,
            &labels(),
        );
        assert_eq!(view.mentions.len(), 1);
        assert_eq!(view.mentions[0].label, "Ada");
        assert_eq!(view.mentions[0].text, "@Ada");
        assert_eq!(view.mentions[0].offset, 0);
        assert!(
            !view.mentions[0].label.contains("u1"),
            "the stored id must not reach a reader"
        );
    }

    /// `mine` is per viewer: the same stored row is the reader's own mention
    /// for one person and somebody else's for everyone else.
    #[test]
    fn project_decides_mine_per_viewer() {
        let event = at(
            8,
            message_mentioning(vec![mention(
                MentionTarget::User {
                    id: "u1".to_string(),
                },
                "@Ada",
                0,
            )]),
        );
        let ada = MessageView::project(event.clone(), &Viewer::User("u1".to_string()), &labels());
        assert!(ada.mentions[0].mine);

        let grace = MessageView::project(event, &Viewer::User("u2".to_string()), &labels());
        assert!(!grace.mentions[0].mine);
    }

    /// A broadcast is addressed to whoever is reading, so it is everybody's own
    /// mention — that is what makes it badge every recipient.
    #[test]
    fn everyone_is_mine_for_every_reader() {
        let event = at(
            9,
            message_mentioning(vec![mention(MentionTarget::Everyone, "@everyone", 0)]),
        );
        for viewer in [
            Viewer::Operator,
            Viewer::User("u1".to_string()),
            Viewer::User("u2".to_string()),
        ] {
            let view = MessageView::project(event.clone(), &viewer, &labels());
            assert!(view.mentions[0].mine, "viewer: {viewer:?}");
            assert_eq!(view.mentions[0].label, "everyone");
        }
    }

    /// A person who has since been removed has no label to resolve to. The
    /// literal text the author typed is the honest fallback — it is what a
    /// reader would have seen anyway — and it must not be the raw id.
    #[test]
    fn a_mention_of_a_departed_person_falls_back_to_the_typed_text() {
        let view = MessageView::project(
            at(
                10,
                message_mentioning(vec![mention(
                    MentionTarget::User {
                        id: "gone".to_string(),
                    },
                    "@Bob",
                    0,
                )]),
            ),
            &Viewer::Operator,
            &labels(),
        );
        assert_eq!(view.mentions[0].label, "Bob");
    }

    #[test]
    fn a_teammate_and_a_desk_project_their_ids_as_labels() {
        let view = MessageView::project(
            at(
                11,
                message_mentioning(vec![
                    mention(
                        MentionTarget::Agent {
                            id: "engineer".to_string(),
                        },
                        "@engineer",
                        0,
                    ),
                    mention(
                        MentionTarget::Desk {
                            id: "engineering".to_string(),
                        },
                        "@engineering",
                        10,
                    ),
                ]),
            ),
            &Viewer::Operator,
            &labels(),
        );
        let labels: Vec<&str> = view.mentions.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, vec!["engineer", "engineering"]);
        assert!(
            view.mentions.iter().all(|m| !m.mine),
            "a teammate or a desk is never the human reader"
        );
    }

    #[test]
    fn a_quiet_mention_projects_as_quiet() {
        let view = MessageView::project(
            at(
                12,
                message_mentioning(vec![Mention {
                    quiet: true,
                    ..mention(
                        MentionTarget::User {
                            id: "u1".to_string(),
                        },
                        "@Ada",
                        0,
                    )
                }]),
            ),
            &Viewer::User("u1".to_string()),
            &labels(),
        );
        assert!(view.mentions[0].quiet);
    }

    #[test]
    fn a_message_that_mentions_nobody_projects_an_empty_list() {
        let view = MessageView::project(
            at(13, message_mentioning(Vec::new())),
            &Viewer::Operator,
            &labels(),
        );
        assert!(view.mentions.is_empty());
    }

    /// A thread parent survives projection on both halves of an exchange, as
    /// the message id a reader can resolve rather than a raw sequence number.
    #[test]
    fn project_carries_the_thread_parent() {
        let operator = MessageView::project(
            at(
                12,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: Some(EventSeq::new(4)),
                    text: "a follow-up".to_string(),
                    by: None,
                    chat: Some("studio".to_string()),
                    deliverable: None,
                    attachments: Vec::new(),
                },
            ),
            &Viewer::Operator,
            &labels(),
        );
        assert_eq!(operator.parent_id.as_deref(), Some("4"));

        let reply = MessageView::project(
            at(
                13,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: Some(EventSeq::new(4)),
                    task_id: None,
                    chat_id: "studio".to_string(),
                    agent_id: "ceo".to_string(),
                    text: "on it".to_string(),
                    steps: Vec::new(),
                },
            ),
            &Viewer::Operator,
            &labels(),
        );
        assert_eq!(reply.parent_id.as_deref(), Some("4"));

        // A message with no parent is in the channel, not in a thread — which
        // is every message journaled before threads were persisted.
        let plain =
            MessageView::project(at(14, agent_reply("studio")), &Viewer::Operator, &labels());
        assert!(plain.parent_id.is_none());
    }

    #[test]
    fn legacy_operator_message_without_chat_stays_on_general() {
        let event = CompanyEvent::OperatorMessage {
            mentions: Vec::new(),
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: None,
            deliverable: None,
            attachments: Vec::new(),
        };
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    /* ---- issue #377: the dispatch terminal as a channel marker ---- */

    /// A settled dispatch, as the harness journals it. `desk` is deliberately
    /// an agent id (`engineer`) and never a channel id (`engineering`) — that
    /// difference is the whole reason the origin has to be carried.
    fn desk_task_completed(origin: Option<&str>, column: &str) -> CompanyEvent {
        threaded_desk_task_completed(origin, None, column)
    }

    /// The same settle, for a card raised inside a thread (#1890 B).
    fn threaded_desk_task_completed(
        origin: Option<&str>,
        origin_parent: Option<u64>,
        column: &str,
    ) -> CompanyEvent {
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "engineer".to_string(),
            output: "the run's prose".to_string(),
            column: column.to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: origin.map(str::to_string),
            origin_parent: origin_parent.map(EventSeq::new),
        }
    }

    /// The terminal routes by the origin the card recorded, on exactly the same
    /// terms a reply does: the desk's id or its name, and nothing else.
    #[test]
    fn a_terminal_belongs_to_the_channel_its_card_was_raised_in() {
        let event = desk_task_completed(Some("engineering"), COLUMN_IN_REVIEW);
        assert!(owns("engineering", "Engineering desk", &event));
        // …and by the desk's *name*, for a card whose origin was journaled
        // under it — the same either-spelling rule a reply routes by.
        let by_name = desk_task_completed(Some("Engineering desk"), COLUMN_IN_REVIEW);
        assert!(owns("engineering", "Engineering desk", &by_name));
        // …and nowhere else. A settle in one channel must not surface in
        // another, which is what would make the marker worse than no marker.
        assert!(!owns("strategy", "Strategy desk", &event));
        assert!(!owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event));
        // The responder is not the channel — matching on it would file every
        // settle under a desk whose id happens to equal an agent's.
        assert!(!owns("engineer", "engineer", &event));
    }

    /// **The most bug-prone line in `owns`.** A card no conversation raised
    /// belongs to no conversation's history — General emphatically included.
    ///
    /// Everywhere else in this module a missing chat id means "unaddressed,
    /// therefore General". On a terminal it means the opposite: the card was
    /// created on the board, by a scheduler, or before the origin was recorded.
    /// Folding it would post markers about board-only work into the operator's
    /// main line, which is a *new* bug rather than the one #377 fixes.
    #[test]
    fn a_terminal_with_no_origin_belongs_to_nobody_not_to_general() {
        let event = desk_task_completed(None, COLUMN_IN_REVIEW);
        assert!(
            !owns(GENERAL_DESK, GENERAL_DESK, &event),
            "an origin-less terminal must not fold into the General desk",
        );
        assert!(
            !owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event),
            "nor into the console's main line, which is General's other spelling",
        );
        assert!(!owns("", "", &event));
        assert!(!owns("engineering", "Engineering desk", &event));
    }

    /// A terminal whose origin *is* one of General's four spellings still folds
    /// like every other event does — the exception above is about `None`, not
    /// about loosening [`same_conversation`].
    #[test]
    fn a_terminal_raised_on_the_main_line_folds_like_any_other_event() {
        for origin in [GENERAL_DESK, MAIN_THREAD_ID, ""] {
            let event = desk_task_completed(Some(origin), COLUMN_PAUSED);
            assert!(
                owns(MAIN_THREAD_ID, MAIN_THREAD_ID, &event),
                "a terminal stored as `{origin}` belongs to the main line",
            );
            assert!(
                owns(GENERAL_DESK, GENERAL_DESK, &event),
                "…and to the General desk's own id/name",
            );
            assert!(
                !owns("strategy", "Strategy desk", &event),
                "…and to no named desk",
            );
        }
    }

    /// The marker's wording, pinned per column. The console holds the same
    /// literals (`dispatchMarkerText`, `frontend/src/lib/chat.ts`) because the
    /// live frame carries the raw column id; these two tests are what couple
    /// them.
    #[test]
    fn the_marker_names_where_the_card_landed() {
        assert_eq!(
            dispatch_marker_text(COLUMN_IN_REVIEW),
            "finished → In review"
        );
        assert_eq!(dispatch_marker_text(COLUMN_PAUSED), "finished → Paused");
        assert_eq!(dispatch_marker_text(COLUMN_TODO), "finished → To-do");
        assert_eq!(dispatch_marker_text(COLUMN_DONE), "finished → Done");
        assert_eq!(dispatch_marker_text(COLUMN_PLANNING), "finished → Planning");
        assert_eq!(
            dispatch_marker_text(COLUMN_IN_PROGRESS),
            "finished → In progress"
        );
    }

    /// A column this build has not heard of reads a little raw rather than
    /// rendering blank — the same fallback `relay_text` takes, and the reason a
    /// newer host cannot produce an empty pill here.
    #[test]
    fn an_unknown_column_passes_through_verbatim() {
        assert_eq!(
            dispatch_marker_text("shipped_to_orbit"),
            "finished → shipped_to_orbit"
        );
    }

    /// The terminal projects as a system line carrying its card — not as the
    /// `Debug` dump the defensive fallback would have rendered into a person's
    /// transcript.
    #[test]
    fn project_renders_a_terminal_as_a_card_linked_system_marker() {
        let view = MessageView::project(
            at(
                21,
                desk_task_completed(Some("engineering"), COLUMN_IN_REVIEW),
            ),
            &Viewer::Operator,
            &labels(),
        );
        assert_eq!(view.author, "system");
        assert_eq!(view.channel, "system");
        assert_eq!(view.text, "finished → In review");
        assert_eq!(
            view.task_id.as_deref(),
            Some("t-1"),
            "the pill links the card"
        );
        assert!(!view.mine);
        assert!(view.steps.is_empty(), "a marker is not a turn");
        assert!(
            view.parent_id.is_none(),
            "a card raised at channel level settles flat in the channel",
        );
        assert_eq!(view.id, "21", "the host id the console dedupes a reload on");
    }

    /// Issue #1890 B — the whole of what this sub-issue repairs.
    ///
    /// A card raised inside a thread used to settle flat in the channel, so the
    /// thread that asked for the work never showed it finishing. The marker
    /// carries the root now, in the same field and the same rendering an
    /// operator message's parent takes, so the console files it into the thread
    /// with no renderer change at all.
    #[test]
    fn a_terminal_raised_in_a_thread_projects_into_that_thread() {
        let view = MessageView::project(
            at(
                50,
                threaded_desk_task_completed(Some("engineering"), Some(41), COLUMN_IN_REVIEW),
            ),
            &Viewer::Operator,
            &labels(),
        );
        assert_eq!(
            view.parent_id.as_deref(),
            Some("41"),
            "the marker hangs off the root the card recorded",
        );
        // The channel half is unchanged: routing still runs through `owns` on
        // the origin channel, and the thread only narrows within it. A marker
        // that threaded but stopped belonging to its channel would vanish.
        assert!(owns(
            "engineering",
            "Engineering desk",
            &threaded_desk_task_completed(Some("engineering"), Some(41), COLUMN_IN_REVIEW),
        ));
    }

    /// The run's prose stays out of the marker. It already reaches this same
    /// channel as the orchestrator's relay bubble (#151); repeating it here
    /// would put one run's words into one conversation twice.
    #[test]
    fn the_marker_does_not_repeat_the_runs_prose() {
        let view = MessageView::project(
            at(22, desk_task_completed(Some("engineering"), COLUMN_PAUSED)),
            &Viewer::Operator,
            &labels(),
        );
        assert!(!view.text.contains("the run's prose"), "{}", view.text);
        assert_eq!(view.text, "finished → Paused");
    }

    /// Issue #885: the audit's classification rule.
    ///
    /// The rule is "an `agent_id` naming no roster teammate", not
    /// `== "operator"`, so these pin both the shape actually observed and the
    /// generalisation — the same writer bug on another channel produces a
    /// different wrong string and still has to be counted.
    mod attribution_audit {
        use super::*;

        fn reply(seq: u64, agent_id: &str) -> StoredEvent {
            at(
                seq,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    chat_id: "engineering".to_string(),
                    agent_id: agent_id.to_string(),
                    text: "…".to_string(),
                    steps: Vec::new(),
                    task_id: None,
                    parent: None,
                },
            )
        }

        /// The roster for these: two real teammates and nothing else.
        ///
        /// Deliberately *excludes* the confined copilot, because that is the
        /// point of `is_known_author` — the copilot is a real author that no
        /// roster will ever resolve.
        fn on_roster(agent_id: &str) -> bool {
            matches!(agent_id, "engineer" | "product_manager")
        }

        /// A record whose roster is exactly `on_roster`'s two teammates.
        ///
        /// Built so the tests below call the **real** `is_known_author` rather
        /// than a local restatement of it. The first version of these tests
        /// re-implemented the predicate in the test module, which meant
        /// reverting the production function changed nothing and the tests
        /// passed either way — proving only that the test agreed with itself.
        fn record() -> CompanyRecord {
            let src = "[company]\nname = \"Acme\"\n\n[policy]\nmode = \"full\"\n\
                       \n[[agent]]\nid = \"engineer\"\nrole = \"Worker\"\ntier = \"orchestrator\"\n\
                       \n[[agent]]\nid = \"product_manager\"\nrole = \"Worker\"\ntier = \"orchestrator\"\n";
            let manifest: crate::company::CompanyManifest =
                toml::from_str(src).expect("manifest parses");
            CompanyRecord {
                overlay_desk_hive: Vec::new(),
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: crate::ports::types::CompanyId::new("acme"),
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

        /// Issue #966. The runtime speaking for itself is a *correct* row, not
        /// damage. Counting it would inflate the blast-radius figure on a company
        /// doing nothing wrong, and would caption a legitimate system message as
        /// something nobody can attribute.
        #[test]
        fn a_host_authored_notice_is_a_known_author_not_an_affected_row() {
            let record = record();
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[reply(1, crate::ports::SYSTEM_AUTHOR), reply(2, "engineer")],
                |agent_id| is_known_author(agent_id, &record),
            );
            assert_eq!(audit.replies, 2);
            assert_eq!(audit.affected, 0);
        }

        /// Issue #966. The console reaches the centred system pill by comparing
        /// the projected author against a literal `"system"`
        /// (`frontend/src/lib/chat.ts`), and `MessageView` projects an
        /// `AgentReply`'s `agent_id` straight into that field. So the *value* is
        /// the contract with the console, not merely the constant's identity.
        ///
        /// Redefining `SYSTEM_AUTHOR` to anything else keeps every other test
        /// here green and silently returns these three notices to rendering as
        /// company bubbles — the exact appearance this change exists to end.
        /// Two copies of one literal is the same coupling
        /// `dispatch_marker_text` already carries with that file, and it is
        /// deliberate for the same reason.
        #[test]
        fn the_notice_author_is_the_literal_the_console_keys_on() {
            assert_eq!(
                crate::ports::SYSTEM_AUTHOR,
                "system",
                "frontend/src/lib/chat.ts renders `author === \"system\"` as the centred pill"
            );
        }

        /// The whole point of the reserved id: a notice and a damaged reply used
        /// to be the same bytes. This pins that they are now different ones, so
        /// the distinction a marker would rely on actually exists in the data.
        #[test]
        fn a_notice_and_an_overwritten_reply_are_no_longer_the_same_author() {
            let record = record();
            assert_ne!(
                crate::ports::SYSTEM_AUTHOR,
                "operator",
                "a notice must not share the author a destination-overwrite produces"
            );
            assert!(is_known_author(crate::ports::SYSTEM_AUTHOR, &record));
            assert!(!is_known_author("operator", &record));
        }

        /// Issue #966. A copilot turn genuinely authored its reply, so the id it
        /// stores is a truthful author — not a destination that leaked into the
        /// field. Counting it would swap one wrong answer for a permanent false
        /// positive that climbs on a company doing nothing wrong.
        #[test]
        fn the_confined_copilot_is_a_known_author_not_an_affected_row() {
            let record = record();
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[
                    reply(1, crate::ports::CONFINED_AGENT_ID),
                    reply(2, "engineer"),
                ],
                |agent_id| is_known_author(agent_id, &record),
            );
            assert_eq!(audit.replies, 2);
            assert_eq!(audit.affected, 0);
        }

        /// …and it is still not on the roster, which is what makes the widening
        /// necessary rather than incidental. If `resolve_roster_agent_id` ever
        /// started answering for it, this says so before the extra arm quietly
        /// becomes dead code.
        #[test]
        fn the_confined_copilot_is_not_reachable_through_the_roster_alone() {
            let record = record();
            assert!(
                record
                    .resolve_roster_agent_id(crate::ports::CONFINED_AGENT_ID)
                    .is_none(),
                "the confined id resolved on the roster; `is_known_author`'s extra arm is now \
                 unnecessary and this test should be deleted deliberately, not left passing"
            );
            assert!(is_known_author(crate::ports::CONFINED_AGENT_ID, &record));
            assert!(is_known_author("engineer", &record));
            assert!(!is_known_author("operator", &record));
        }

        /// A delivered workflow report is journaled under
        /// [`crate::runtime::WORKFLOW_REPLY_AUTHOR`] on purpose — it is the
        /// workflow speaking, not a teammate's own reply. Counting it would
        /// flag every delivered report on a company with no roster match for
        /// "workflow" as damaged, and — worse — a teammate who *did* mint that
        /// id would have every report silently misattributed to them by
        /// `senderOf` before this reservation existed.
        #[test]
        fn a_workflow_report_is_a_known_author_not_an_affected_row() {
            let record = record();
            assert!(
                record
                    .resolve_roster_agent_id(crate::runtime::WORKFLOW_REPLY_AUTHOR)
                    .is_none(),
                "workflow reports resolve through the extra arm, not the roster"
            );
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[
                    reply(1, crate::runtime::WORKFLOW_REPLY_AUTHOR),
                    reply(2, "engineer"),
                ],
                |agent_id| is_known_author(agent_id, &record),
            );
            assert_eq!(audit.replies, 2);
            assert_eq!(audit.affected, 0);
        }

        /// Issue #1781 review, Codex P2: an owner-fallback report is journaled
        /// under [`crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR`] on purpose —
        /// same reservation as `WORKFLOW_REPLY_AUTHOR`, one arm narrower — so it
        /// must not inflate the audit either. Before this arm existed, every
        /// legitimate no-mailbox fallback counted as damaged attribution.
        #[test]
        fn an_owner_fallback_report_is_a_known_author_not_an_affected_row() {
            let record = record();
            assert!(
                record
                    .resolve_roster_agent_id(crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR)
                    .is_none(),
                "owner-fallback reports resolve through the extra arm, not the roster"
            );
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[
                    reply(1, crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR),
                    reply(2, "engineer"),
                ],
                |agent_id| is_known_author(agent_id, &record),
            );
            assert_eq!(audit.replies, 2);
            assert_eq!(audit.affected, 0);
        }

        /// Review on PR #1781 (Codex P2): a company that named an overlay
        /// teammate "Workflow" before this reservation existed would have
        /// minted the bare id `workflow` — the id `WORKFLOW_REPLY_AUTHOR`
        /// itself used to be, until it was reshaped to the unmintable,
        /// hyphenated `workflow-report`. That persisted teammate is not
        /// migrated or renamed by this fix — there is nothing to migrate: the
        /// pseudo-author a workflow report is now journaled under is a
        /// **different, disjoint id** from the one that teammate holds, so
        /// the collision this reservation exists to prevent cannot occur for
        /// it, retroactively as well as going forward. Proven here rather than
        /// asserted, since the whole point is that the two ids must never
        /// again be able to resolve to the same author.
        #[test]
        fn a_persisted_teammate_named_workflow_does_not_shadow_the_reply_author() {
            let mut record = record();
            record
                .overlay_agents
                .push(crate::ports::types::OverlayAgent {
                    id: "workflow".to_string(),
                    name: "Workflow".to_string(),
                    role: "Worker".to_string(),
                    description: None,
                    tools: Some(Vec::new()),
                    model: None,
                    harness: None,
                });

            assert_ne!(
                "workflow",
                crate::runtime::WORKFLOW_REPLY_AUTHOR,
                "the two ids must be disjoint for the rest of this test to mean anything"
            );
            assert!(
                record.resolve_roster_agent_id("workflow").is_some(),
                "the pre-existing teammate is still on the roster, unmigrated"
            );
            assert!(
                record
                    .resolve_roster_agent_id(crate::runtime::WORKFLOW_REPLY_AUTHOR)
                    .is_none(),
                "the reply-author id does not resolve to that (or any) teammate"
            );

            let mut audit = AttributionAudit::default();
            audit.fold(
                &[
                    // The teammate's own reply — attributed to them, as before.
                    reply(1, "workflow"),
                    // A new workflow report, delivered after this fix ships —
                    // journaled under the disjoint id, not theirs.
                    reply(2, crate::runtime::WORKFLOW_REPLY_AUTHOR),
                ],
                |agent_id| is_known_author(agent_id, &record),
            );
            assert_eq!(audit.replies, 2);
            assert_eq!(
                audit.affected, 0,
                "both rows resolve, to two different authors"
            );
        }

        #[test]
        fn a_reply_authored_by_a_real_teammate_is_not_counted() {
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[reply(1, "engineer"), reply(2, "product_manager")],
                on_roster,
            );
            assert_eq!(audit.replies, 2);
            assert_eq!(audit.affected, 0);
            assert!(audit.by_agent_id.is_empty());
        }

        /// The observed #885 shape: the operator channel copied into the author.
        #[test]
        fn a_reply_authored_by_the_operator_channel_is_counted() {
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[
                    reply(1, "operator"),
                    reply(2, "engineer"),
                    reply(3, "operator"),
                ],
                on_roster,
            );
            assert_eq!(audit.replies, 3);
            assert_eq!(audit.affected, 2);
            assert_eq!(audit.by_agent_id.get("operator"), Some(&2));
        }

        /// The generalisation. A Telegram chat id or a desk slug in the author
        /// field is the same defect, and a rule keyed on the literal
        /// `"operator"` would report a clean company.
        #[test]
        fn any_non_roster_author_is_counted_not_just_the_operator_channel() {
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[reply(1, "operator"), reply(2, "-100123456789")],
                on_roster,
            );
            assert_eq!(audit.affected, 2);
            assert_eq!(audit.by_agent_id.get("-100123456789"), Some(&1));
        }

        /// Only replies. An operator's own message is not an `AgentReply` and
        /// has no `agent_id` to be wrong, so counting it would inflate the
        /// blast radius of a data-integrity bug — the one number that has to be
        /// trustworthy here.
        #[test]
        fn a_non_reply_event_is_neither_scanned_nor_counted() {
            let mut audit = AttributionAudit::default();
            audit.fold(
                &[
                    at(
                        1,
                        CompanyEvent::OperatorMessage {
                            mentions: Vec::new(),
                            text: "hello".to_string(),
                            by: None,
                            chat: None,
                            parent: None,
                            deliverable: None,
                            attachments: Vec::new(),
                        },
                    ),
                    reply(2, "operator"),
                ],
                on_roster,
            );
            assert_eq!(audit.replies, 1);
            assert_eq!(audit.affected, 1);
        }
    }
}

#[cfg(test)]
mod dead_card_test {
    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::tasks::TaskTitle;
    use crate::ports::tasks::{COLUMN_TODO, TaskDeliverable, TaskRecord};
    use crate::ports::types::CompanyId;
    use crate::runtime::RuntimeBuilder;
    use std::sync::Arc;

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n")
            .expect("parse manifest")
    }

    fn card(id: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: TaskTitle::authored("Draft the launch note"),
            note: None,
            column: COLUMN_TODO.to_string(),
            priority: "medium".to_string(),
            assignee: String::new(),
            updated_at_millis: 1,
            origin: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            origin_message_seq: None,
            bounced: None,
        }
    }

    /// A reply that opened a card, exactly as the dispatch path journals it.
    fn reply_naming(task_id: &str) -> CompanyEvent {
        CompanyEvent::AgentReply {
            audience: Vec::new(),
            mentions: Vec::new(),
            mention_depth: 0,
            parent: None,
            task_id: Some(task_id.to_string()),
            chat_id: MAIN_THREAD_ID.to_string(),
            agent_id: "ceo".to_string(),
            text: "Opened a card for that.".to_string(),
            steps: Vec::new(),
        }
    }

    async fn runtime(home: &std::path::Path) -> Arc<CompanyRuntime> {
        Arc::new(
            RuntimeBuilder::new(home.to_path_buf(), manifest())
                .with_id(CompanyId::new("acme"))
                .build()
                .await
                .expect("build a runtime"),
        )
    }

    /// The chip survives a reload while the card is still on the board — the
    /// behaviour issue #246 added and `chat-to-card.spec.ts` pins.
    ///
    /// Asserted first so the test below cannot pass by the projection simply
    /// dropping every `task_id` it sees.
    #[tokio::test]
    async fn a_reply_keeps_its_card_while_the_card_exists() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        runtime
            .tasks()
            .upsert(&id, &card("card-1"))
            .await
            .expect("seed the board");
        runtime
            .events()
            .append(&id, reply_naming("card-1"))
            .await
            .expect("journal the reply");

        let history = history_for_desk(
            &runtime,
            MAIN_THREAD_ID,
            MAIN_THREAD_ID,
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");

        assert_eq!(
            history.iter().filter_map(|m| m.task_id.as_deref()).count(),
            1,
            "the chip is projected while the card is on the board: {history:?}"
        );
    }

    /// **The reload half of the dismissal (issue #984).**
    ///
    /// The journal still records that the turn opened a card — it did, and that
    /// event is not rewritten. What must not happen is the *projection* handing
    /// the console an id it can only render as a link to a `404`, which is how a
    /// completed delete comes back looking like a failed one.
    ///
    /// Deleting the card is the only difference from the test above.
    #[tokio::test]
    async fn a_reply_loses_its_card_once_the_card_is_deleted() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        runtime
            .tasks()
            .upsert(&id, &card("card-1"))
            .await
            .expect("seed the board");
        runtime
            .events()
            .append(&id, reply_naming("card-1"))
            .await
            .expect("journal the reply");
        assert!(
            runtime
                .tasks()
                .delete(&id, "card-1")
                .await
                .expect("delete the card"),
            "the card was there to delete"
        );

        let history = history_for_desk(
            &runtime,
            MAIN_THREAD_ID,
            MAIN_THREAD_ID,
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");

        assert!(
            !history.is_empty(),
            "the reply itself still belongs in the transcript — only its card is gone"
        );
        assert!(
            history.iter().all(|m| m.task_id.is_none()),
            "a rehydrated chip for a deleted card is a link to a 404, which reads \
             as the delete having failed: {history:?}"
        );
    }

    /// The board is read once per history, and not at all when no row carries a
    /// card — the cost argument for doing this in the projection.
    ///
    /// Asserted through behaviour rather than a call count: a transcript with no
    /// cards comes back unchanged.
    #[tokio::test]
    async fn a_transcript_with_no_cards_is_untouched() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: MAIN_THREAD_ID.to_string(),
                    agent_id: "ceo".to_string(),
                    text: "just talking".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the reply");

        let history = history_for_desk(
            &runtime,
            MAIN_THREAD_ID,
            MAIN_THREAD_ID,
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");

        assert_eq!(history.len(), 1, "{history:?}");
        assert!(history[0].task_id.is_none(), "{history:?}");
    }

    /// Issue #1781 review (Codex P1): an `owner`-fallback report — marked via
    /// [`crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR`] — must never reach a
    /// non-admin viewer, while an ordinary operator-channel report (any other
    /// author) is unaffected. Pre-fix, `history_for_desk` had no concept of
    /// `admin_only` at all: every signed-in company user, admin or Member, saw
    /// every row on a desk they could address, which is exactly the leak this
    /// test pins shut.
    #[tokio::test]
    async fn an_owner_fallback_row_is_hidden_from_a_non_admin_viewer() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
                    text: "admin-only owner report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the owner-fallback report");
        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::WORKFLOW_REPLY_AUTHOR.to_string(),
                    text: "ordinary workflow report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the ordinary report");

        let as_member = history_for_desk(
            &runtime,
            crate::runtime::OPERATOR_CHANNEL,
            crate::runtime::OPERATOR_CHANNEL,
            &Viewer::Operator,
            None,
            50,
            false,
        )
        .await
        .expect("history");
        assert_eq!(
            as_member.len(),
            1,
            "a non-admin must not see the owner-fallback row: {as_member:?}"
        );
        assert_eq!(as_member[0].text, "ordinary workflow report");
        assert!(!as_member[0].admin_only, "{as_member:?}");

        let as_admin = history_for_desk(
            &runtime,
            crate::runtime::OPERATOR_CHANNEL,
            crate::runtime::OPERATOR_CHANNEL,
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");
        assert_eq!(
            as_admin.len(),
            2,
            "an admin must see both rows: {as_admin:?}"
        );
        assert!(as_admin.iter().any(|m| m.admin_only), "{as_admin:?}");
    }

    /// The exclusion happens inside the paging loop, before a row counts
    /// toward `first` (see `history_for_desk`'s doc) — proven by requesting
    /// exactly one row as a non-admin with an admin-only row sorted newest: a
    /// post-fetch filter would come back empty here, not with the one visible
    /// row underneath it.
    #[tokio::test]
    async fn a_non_admin_page_fills_past_an_admin_only_row_instead_of_coming_back_short() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        // Oldest first: the visible row, then the admin-only row on top of it.
        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::WORKFLOW_REPLY_AUTHOR.to_string(),
                    text: "visible report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the ordinary report");
        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
                    text: "admin-only report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the owner-fallback report");

        let as_member = history_for_desk(
            &runtime,
            crate::runtime::OPERATOR_CHANNEL,
            crate::runtime::OPERATOR_CHANNEL,
            &Viewer::Operator,
            None,
            1,
            false,
        )
        .await
        .expect("history");

        assert_eq!(
            as_member.len(),
            1,
            "a non-admin's page must fill with the next visible row, not come \
             back short: {as_member:?}"
        );
        assert_eq!(as_member[0].text, "visible report");
    }

    /// Issue #1781 review (Codex P2): `history_total_for_desk` must agree with
    /// `history_for_desk` about which rows a non-admin can see. Pre-fix, this
    /// count had no `is_admin` param at all — a non-admin querying a desk
    /// holding an owner-fallback row (e.g. a grandfathered real desk at the
    /// literal `operator` id) got a `total` one higher than `items.len()`
    /// could ever be, breaking `Page.total`'s item-count contract and
    /// revealing that a hidden admin report exists.
    #[tokio::test]
    async fn total_excludes_the_owner_fallback_row_for_a_non_admin_but_counts_it_for_an_admin() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
                    text: "admin-only owner report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the owner-fallback report");
        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::WORKFLOW_REPLY_AUTHOR.to_string(),
                    text: "ordinary workflow report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the ordinary report");

        let as_member = history_total_for_desk(
            &runtime,
            crate::runtime::OPERATOR_CHANNEL,
            crate::runtime::OPERATOR_CHANNEL,
            None,
            false,
        )
        .await
        .expect("total");
        assert_eq!(
            as_member, 1,
            "a non-admin's total must match what history_for_desk would ever show them"
        );

        let as_admin = history_total_for_desk(
            &runtime,
            crate::runtime::OPERATOR_CHANNEL,
            crate::runtime::OPERATOR_CHANNEL,
            None,
            true,
        )
        .await
        .expect("total");
        assert_eq!(as_admin, 2, "an admin's total must count both rows");
    }

    /// Issue #1781 review (Codex P2, follow-up): `channel_attributed_replies`
    /// must agree with `history_for_desk` / `history_total_for_desk` about
    /// which rows a non-admin can see. Pre-fix, it had no `is_admin` param at
    /// all — a Member polling `/chat/attribution-audit` around an
    /// owner-fallback delivery watched `replies` tick up for a row neither
    /// the transcript nor SSE ever showed them, confirming a hidden
    /// admin-only message exists even though its content stayed hidden.
    #[tokio::test]
    async fn attribution_audit_excludes_the_owner_fallback_row_for_a_non_admin_but_counts_it_for_an_admin()
     {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");
        let record = runtime
            .store()
            .load(&id)
            .await
            .expect("load")
            .expect("record exists");

        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR.to_string(),
                    text: "admin-only owner report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the owner-fallback report");
        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    audience: Vec::new(),
                    mentions: Vec::new(),
                    mention_depth: 0,
                    parent: None,
                    task_id: None,
                    chat_id: crate::runtime::OPERATOR_CHANNEL.to_string(),
                    agent_id: crate::runtime::WORKFLOW_REPLY_AUTHOR.to_string(),
                    text: "ordinary workflow report".to_string(),
                    steps: Vec::new(),
                },
            )
            .await
            .expect("journal the ordinary report");

        let as_member = channel_attributed_replies(&runtime, &record, false)
            .await
            .expect("audit");
        assert_eq!(
            as_member.replies, 1,
            "a non-admin's replies count must match what history_for_desk would \
             ever show them: {as_member:?}"
        );

        let as_admin = channel_attributed_replies(&runtime, &record, true)
            .await
            .expect("audit");
        assert_eq!(as_admin.replies, 2, "an admin's count must count both rows");
    }
}

/// Where a referred line says it came from, and who it says is speaking.
#[cfg(test)]
mod referral_origin_test {
    use super::*;
    use crate::company::CompanyManifest;
    use crate::ports::types::CompanyId;
    use crate::runtime::RuntimeBuilder;

    fn manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n")
            .expect("parse manifest")
    }

    async fn runtime(home: &std::path::Path) -> Arc<CompanyRuntime> {
        Arc::new(
            RuntimeBuilder::new(home.to_path_buf(), manifest())
                .with_id(CompanyId::new("acme"))
                .build()
                .await
                .expect("build a runtime"),
        )
    }

    /// Helper: the marker and the agent-authored line it caused, as one leg.
    fn referral_leg(
        from_desk: &str,
        from_desk_name: &str,
        asker: &str,
        to_desk: &str,
        target: &str,
        returning: bool,
        text: &str,
    ) -> [CompanyEvent; 2] {
        referral_leg_answering(
            from_desk,
            from_desk_name,
            asker,
            to_desk,
            target,
            returning,
            text,
            None,
        )
    }

    /// The same, with the forward this return answers named explicitly — the
    /// pointer the host records so the projection need not scan for it.
    #[expect(clippy::too_many_arguments, reason = "a journal event's own shape")]
    fn referral_leg_answering(
        from_desk: &str,
        from_desk_name: &str,
        asker: &str,
        to_desk: &str,
        target: &str,
        returning: bool,
        text: &str,
        answers: Option<u64>,
    ) -> [CompanyEvent; 2] {
        [
            CompanyEvent::ReferralEnqueued {
                // These fixtures are desk crossings, which run on the target's
                // own desk and name no pair conversation.
                conversation: None,
                answers,
                from_desk: from_desk.to_string(),
                from_desk_name: from_desk_name.to_string(),
                asker: asker.to_string(),
                asker_label: asker.to_string(),
                trigger_sequence: 1,
                to_desk: to_desk.to_string(),
                target: target.to_string(),
                returning,
            },
            CompanyEvent::OperatorMessage {
                text: text.to_string(),
                by: Some(Actor {
                    kind: ActorKind::Agent,
                    id: asker.to_string(),
                }),
                chat: Some(to_desk.to_string()),
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            },
        ]
    }

    /// **An episode's turns are the conversation; its closing row is not.**
    ///
    /// The room journals every turn as an ordinary reply by the teammate that
    /// took it, then one summary under `hive-report`. Rendered, that summary
    /// appeared as a *teammate* — a participant in a channel where no such
    /// teammate exists and none can, since the id is hyphenated exactly so no
    /// roster id can equal it. The fold already reads it as `System`; this
    /// makes the console agree.
    ///
    /// The turns must survive: dropping the room and keeping only its summary
    /// would hide the reasoning, the losing options and every objection — the
    /// one thing a room produces that a single answer cannot.
    #[tokio::test]
    async fn an_episodes_turns_render_but_its_closing_row_does_not() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        for (agent, text) in [
            (
                "software_engineer",
                "!propose #lazy-load defer each section",
            ),
            (
                "junior_engineer",
                "!object >1 ^1 users bounce between sections",
            ),
            (
                crate::hivemind::HIVE_REPORT_AUTHOR,
                "The desk settled after 2 turns (#lazy-load, backed by software_engineer): defer each section",
            ),
        ] {
            runtime
                .events()
                .append(
                    &id,
                    CompanyEvent::AgentReply {
                        chat_id: "engineering".to_string(),
                        agent_id: agent.to_string(),
                        text: text.to_string(),
                        steps: Vec::new(),
                        task_id: None,
                        parent: None,
                        mentions: Vec::new(),
                        mention_depth: 0,
                        audience: Vec::new(),
                    },
                )
                .await
                .expect("journal");
        }

        let history = history_for_desk(
            &runtime,
            "engineering",
            "engineering",
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");

        let voices: Vec<&str> = history.iter().map(|m| m.channel.as_str()).collect();
        assert!(
            voices.contains(&"software_engineer") && voices.contains(&"junior_engineer"),
            "every teammate's turn is on screen, the objection included: {voices:?}"
        );
        assert!(
            !voices.contains(&crate::hivemind::HIVE_REPORT_AUTHOR),
            "and the room's own bookkeeping is not a participant in it: {voices:?}"
        );
    }

    /// **A suppressed row must not shorten the page.**
    ///
    /// Filtered after the page was assembled, an episode's closing row silently
    /// cost the reader a message: a page asked for `n` came back with `n - 1`,
    /// and the row that should have taken its place stayed unfetched. The
    /// admission point already excludes an admin-only row for exactly this
    /// reason, and says so.
    #[tokio::test]
    async fn a_suppressed_report_does_not_shorten_the_page() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        // Four teammate turns with the room's closing row in the middle.
        for (agent, text) in [
            ("software_engineer", "first"),
            ("junior_engineer", "second"),
            (crate::hivemind::HIVE_REPORT_AUTHOR, "The desk settled."),
            ("qa_engineer", "third"),
            ("software_engineer", "fourth"),
        ] {
            runtime
                .events()
                .append(
                    &id,
                    CompanyEvent::AgentReply {
                        chat_id: "engineering".to_string(),
                        agent_id: agent.to_string(),
                        text: text.to_string(),
                        steps: Vec::new(),
                        task_id: None,
                        parent: None,
                        mentions: Vec::new(),
                        mention_depth: 0,
                        audience: Vec::new(),
                    },
                )
                .await
                .expect("journal");
        }

        let page = history_for_desk(
            &runtime,
            "engineering",
            "engineering",
            &Viewer::Operator,
            None,
            4,
            true,
        )
        .await
        .expect("history");

        assert_eq!(
            page.len(),
            4,
            "a page of four is four teammate turns, not three and a hole: {page:?}"
        );
        assert!(
            page.iter()
                .all(|m| m.channel != crate::hivemind::HIVE_REPORT_AUTHOR),
            "and none of them is the room's bookkeeping: {page:?}"
        );
    }

    /// **A failed turn still shows.** The report restates a tally whose inputs
    /// are the visible turns, so hiding it costs nothing. A failure notice
    /// describes a turn that does not exist — there is no gap for a reader to
    /// notice — so hiding it would leave a transcript with an unaccounted hole.
    #[tokio::test]
    async fn a_failed_turn_is_still_reported_to_the_room() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        for (agent, text) in [
            (
                crate::hivemind::HIVE_FAILURE_AUTHOR,
                "qa_engineer was asked and could not answer.",
            ),
            (
                crate::hivemind::HIVE_REPORT_AUTHOR,
                "The desk settled after 2 turns.",
            ),
        ] {
            runtime
                .events()
                .append(
                    &id,
                    CompanyEvent::AgentReply {
                        chat_id: "engineering".to_string(),
                        agent_id: agent.to_string(),
                        text: text.to_string(),
                        steps: Vec::new(),
                        task_id: None,
                        parent: None,
                        mentions: Vec::new(),
                        mention_depth: 0,
                        audience: Vec::new(),
                    },
                )
                .await
                .expect("journal");
        }

        let history = history_for_desk(
            &runtime,
            "engineering",
            "engineering",
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");
        let voices: Vec<&str> = history.iter().map(|m| m.channel.as_str()).collect();

        assert!(
            voices.contains(&crate::hivemind::HIVE_FAILURE_AUTHOR),
            "a seat that could not answer is accounted for: {voices:?}"
        );
        assert!(
            !voices.contains(&crate::hivemind::HIVE_REPORT_AUTHOR),
            "while the closing summary stays out of the room: {voices:?}"
        );
    }

    /// **A referred line is the ASKING AGENT speaking, not the desk.**
    ///
    /// `senderOf` in the console draws the byline off `channel`, and treats
    /// "operator" as "no distinct speaker — use the room's own name". That is
    /// right for a message a person sent and wrong for a referral, which
    /// arrives authored by a teammate: hardcoding "operator" made design's own
    /// name the speaker, so an engineer asking design read as design talking to
    /// itself. An `AgentReply` already names its agent here; this makes the two
    /// paths agree rather than teaching the console a second rule.
    #[tokio::test]
    async fn a_referred_message_is_voiced_by_the_agent_that_asked() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        for event in referral_leg(
            "engineering",
            "Engineering",
            "software_engineer",
            "design",
            "product_designer",
            false,
            "what would you change about the error messages?",
        ) {
            runtime.events().append(&id, event).await.expect("journal");
        }

        let history = history_for_desk(
            &runtime,
            "design",
            "design",
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");
        let referred = history
            .iter()
            .find(|m| m.referred_from.is_some())
            .expect("the referred line");

        assert_eq!(
            referred.channel, "software_engineer",
            "the byline names the agent, not the desk it landed on: {referred:?}"
        );
        assert!(
            !referred.by_person,
            "an agent is not a person, whatever the event it rides on"
        );
    }

    /// **One agent speaks in both rooms, and it is the asker.**
    ///
    /// The asker asks on the other desk under its own name; that desk answers
    /// on its own desk; the asker comes home and reports. The relay that
    /// carried the answer back is an input to the asker, not a line anyone
    /// reads — rendering it put the other desk's agent in a room it is not part
    /// of, saying the same thing the asker was about to say.
    #[tokio::test]
    async fn the_asker_brings_the_answer_home_and_the_other_desk_stays_out_of_the_room() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        let mut events: Vec<CompanyEvent> = referral_leg(
            "engineering",
            "Engineering",
            "software_engineer",
            "design",
            "product_designer",
            false,
            "what would you change about the error messages?",
        )
        .into_iter()
        .chain(referral_leg(
            "design",
            "Design",
            "product_designer",
            "engineering",
            "software_engineer",
            true,
            "error messages look like a copy task and are not one",
        ))
        .collect();
        // The asker's report — the only thing #engineering should show.
        events.push(CompanyEvent::AgentReply {
            chat_id: "engineering".to_string(),
            agent_id: "software_engineer".to_string(),
            text: "design came back: error messages are a design-system problem".to_string(),
            steps: Vec::new(),
            task_id: None,
            parent: None,
            mentions: Vec::new(),
            mention_depth: 0,
            audience: Vec::new(),
        });
        for event in events {
            runtime.events().append(&id, event).await.expect("journal");
        }

        let history = history_for_desk(
            &runtime,
            "engineering",
            "engineering",
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");

        assert!(
            history.iter().all(|m| m.channel != "product_designer"),
            "the answering desk never speaks in the room it was asked from: {history:?}"
        );
        let referred = history
            .iter()
            .find(|m| m.referred_from.is_some())
            .expect("something carries the provenance");
        assert_eq!(
            referred.channel, "software_engineer",
            "the chip rides the asker's own report: {referred:?}"
        );
        let origin = referred.referred_from.as_ref().expect("origin");
        assert!(origin.returning, "and it reads as an answer, not an ask");
        assert_eq!(origin.desk_name, "Design");

        // **The crossing itself rides the report, both legs of it.**
        //
        // The relayed rows still go — that is the assertion above, and the
        // reason for it — but the exchange they carried is kept here so an
        // operator can read what was actually asked and answered instead of
        // only the asker's paraphrase of it. `lines.len()` is the count the
        // collapsed label shows, which is why the QUESTION has to be captured
        // too: an answer on its own would always read "1 message".
        let crossing = referred
            .referral_conversation
            .as_ref()
            .expect("the crossing rides the report that brought it home");
        assert_eq!(crossing.asker_id, "software_engineer");
        assert_eq!(crossing.other_id, "product_designer");
        assert_eq!(crossing.other_desk_name, "Design");
        assert_eq!(crossing.lines.len(), 2, "{:?}", crossing.lines);
        assert!(crossing.lines[0].outbound, "the question goes out first");
        assert_eq!(
            crossing.lines[0].text,
            "what would you change about the error messages?"
        );
        assert!(!crossing.lines[1].outbound, "then the answer comes back");
        assert_eq!(
            crossing.lines[1].text,
            "error messages look like a copy task and are not one"
        );
    }

    /// **The marker names its own forward, so the ask is found however far back
    /// it is.**
    ///
    /// The scan this replaces looked back a fixed number of events from the
    /// oldest visible row, so a crossing whose ask fell outside that window
    /// rendered with the answer alone and said "1 message" — quietly wrong, and
    /// wrong in the direction that looks plausible. The host already located
    /// that marker to authorize the return and was keeping only a bool;
    /// `answers` records it instead.
    ///
    /// Here the two legs are separated by far more than the scan's `LOOKBACK`,
    /// so the fallback cannot reach the ask and only the pointer can.
    #[tokio::test]
    async fn a_marker_that_names_its_forward_pairs_beyond_the_scan_window() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        // The marker's OWN sequence, not a guess at it: the runtime journals
        // its own setup rows first, so the first referral event is not
        // sequence zero. Pointing `answers` at a sequence that happens to hold
        // something else is the confusion the pointer exists to remove.
        let mut forward_seq = 0u64;
        for (i, event) in referral_leg(
            "engineering",
            "Engineering",
            "software_engineer",
            "design",
            "product_designer",
            false,
            "what would you change about the error messages?",
        )
        .into_iter()
        .enumerate()
        {
            let seq = runtime.events().append(&id, event).await.expect("journal");
            if i == 0 {
                forward_seq = seq.value();
            }
        }
        // The forward marker is sequence 0, its question 1. Bury them under
        // enough unrelated traffic that the scan's window cannot reach back.
        for i in 0..200 {
            runtime
                .events()
                .append(
                    &id,
                    CompanyEvent::AgentReply {
                        chat_id: "engineering".to_string(),
                        agent_id: "software_engineer".to_string(),
                        text: format!("unrelated line {i}"),
                        steps: Vec::new(),
                        task_id: None,
                        parent: None,
                        mentions: Vec::new(),
                        mention_depth: 0,
                        audience: Vec::new(),
                    },
                )
                .await
                .expect("journal");
        }
        for event in referral_leg_answering(
            "design",
            "Design",
            "product_designer",
            "engineering",
            "software_engineer",
            true,
            "error messages look like a copy task and are not one",
            Some(forward_seq),
        ) {
            runtime.events().append(&id, event).await.expect("journal");
        }
        runtime
            .events()
            .append(
                &id,
                CompanyEvent::AgentReply {
                    chat_id: "engineering".to_string(),
                    agent_id: "software_engineer".to_string(),
                    text: "design came back: it is a design-system problem".to_string(),
                    steps: Vec::new(),
                    task_id: None,
                    parent: None,
                    mentions: Vec::new(),
                    mention_depth: 0,
                    audience: Vec::new(),
                },
            )
            .await
            .expect("journal");

        // Only the tail is on screen, so the ask is far outside the scan.
        let history = history_for_desk(
            &runtime,
            "engineering",
            "engineering",
            &Viewer::Operator,
            None,
            5,
            true,
        )
        .await
        .expect("history");
        let crossing = history
            .iter()
            .find_map(|m| m.referral_conversation.as_ref())
            .expect("the crossing rides the report");
        assert_eq!(
            crossing.lines.len(),
            2,
            "the pointer reaches an ask the scan cannot: {:?}",
            crossing.lines
        );
        assert!(crossing.lines[0].outbound);
        assert_eq!(
            crossing.lines[0].text,
            "what would you change about the error messages?"
        );
    }

    /// **A rendered relay shows the answer and none of the host's note.**
    ///
    /// Seen in the console, not reasoned about: the asker's turn died on an
    /// empty model response, the fallback rendered the relay, and #engineering
    /// was told "you are the only one who has seen it" by the design desk's
    /// agent. The note is written FOR the asker and is private to it; the
    /// fallback exists to preserve the ANSWER, so that is all it may publish.
    #[tokio::test]
    async fn a_rendered_relay_keeps_the_answer_and_drops_the_note() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        let answer = "use a skeleton, not a spinner";
        let note = format!(
            "{}product_designer on the Design desk answered what you asked them. \
             This did not appear in your channel — you are the only one who has seen it.",
            crate::ports::types::RELAY_NOTE_MARKER
        );
        // No reply follows, so the fallback renders this relay.
        for event in referral_leg(
            "design",
            "Design",
            "product_designer",
            "engineering",
            "software_engineer",
            true,
            &format!("{answer}{note}"),
        ) {
            runtime.events().append(&id, event).await.expect("journal");
        }

        let history = history_for_desk(
            &runtime,
            "engineering",
            "engineering",
            &Viewer::Operator,
            None,
            50,
            true,
        )
        .await
        .expect("history");
        let relayed = history
            .iter()
            .find(|m| m.referred_from.is_some())
            .expect("the relay renders, because nothing else carries the answer");

        assert_eq!(
            relayed.text, answer,
            "the other desk's own words, and only those"
        );
        assert!(
            !relayed.text.contains("only one who has seen it"),
            "a note addressed to the asker is not published to the channel"
        );
    }

    /// **The fail-safe half: a relay renders while the report is still missing.**
    ///
    /// The test below drops the relay once the asker has reported. Until then
    /// there is nothing else carrying design's answer, and dropping it would
    /// lose the answer outright — so it renders, in the wrong voice, saying
    /// truthfully that it is an answer rather than an ask.
    ///
    /// **Which leg this is, is the host's to say (the "Answered by" chip).**
    ///
    /// Both legs are agent-authored lines on a desk, so every signal the
    /// console holds reads identically on each — it guessed from `from` and
    /// called every returning answer an ask. `tinyhivemind` decided it already
    /// (`ReferralKind`), and the marker carries that decision.
    #[tokio::test]
    async fn a_relay_with_no_report_yet_still_renders_and_says_it_is_an_answer() {
        let home = tempfile::tempdir().expect("tempdir");
        let runtime = runtime(home.path()).await;
        let id = CompanyId::new("acme");

        for event in referral_leg(
            "engineering",
            "Engineering",
            "software_engineer",
            "design",
            "product_designer",
            false,
            "what would you change about the error messages?",
        )
        .into_iter()
        .chain(referral_leg(
            "design",
            "Design",
            "product_designer",
            "engineering",
            "software_engineer",
            true,
            "error messages look like a copy task and are not one",
        )) {
            runtime.events().append(&id, event).await.expect("journal");
        }

        for (desk, desk_name, returning) in [
            ("design", "Engineering", false),
            ("engineering", "Design", true),
        ] {
            let history = history_for_desk(&runtime, desk, desk, &Viewer::Operator, None, 50, true)
                .await
                .expect("history");
            let origin = history
                .iter()
                .find_map(|m| m.referred_from.as_ref())
                .unwrap_or_else(|| panic!("#{desk} carries a referral origin"));
            assert_eq!(origin.desk_name, desk_name, "on #{desk}");
            assert_eq!(
                origin.returning,
                returning,
                "#{desk} draws the {} chip",
                if returning {
                    "\"Answered by\""
                } else {
                    "\"Asked by\""
                }
            );
        }
    }
}

/// How a chat selector becomes the `(desk id, desk name)` pair [`owns`] filters
/// on — the one answer to "which desk is this", shared by the seed, the cycle's
/// briefings and `read_thread`.
#[cfg(test)]
mod desk_resolution_test {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};

    struct RecordStore(Option<CompanyRecord>);

    #[async_trait]
    impl CompanyStore for RecordStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(self.0.clone())
        }
        async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
            unreachable!("resolve only reads")
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            unreachable!("resolve only reads")
        }
        async fn append_ledger(
            &self,
            _id: &CompanyId,
            _entry: crate::ports::types::LedgerEntry,
        ) -> crate::Result<()> {
            unreachable!("resolve only reads")
        }
    }

    use crate::ports::types::CompanySummary;

    fn record_with_group_chat(id: &str, name: &str) -> CompanyRecord {
        let manifest = toml::from_str(&format!(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."

[[group_chat]]
id = "{id}"
name = "{name}"
"#,
        ))
        .expect("valid manifest");
        CompanyRecord {
            overlay_desk_hive: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_tool_grants: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            setup: None,
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        }
    }

    async fn resolve(store: RecordStore, chat_id: Option<&str>) -> (String, String) {
        let store: Arc<dyn CompanyStore> = Arc::new(store);
        resolve_seed_desk(&store, &CompanyId::new("acme"), chat_id).await
    }

    /// A desk created from the console is a desk.
    ///
    /// It lives in `overlay_desks` and never in the manifest, so a lookup that
    /// reads only `group_chats` fell through to the verbatim selector — and
    /// every line journaled under the desk's *other* spelling was orphaned from
    /// the thread index, `read_thread` and the seed alike (coderabbit + codex
    /// on #1972).
    #[test]
    fn an_overlay_desk_resolves_by_either_spelling() {
        let mut record = record_with_group_chat("growth_desk", "Growth");
        record.overlay_desks.push(crate::ports::types::OverlayDesk {
            id: "ops_desk".to_string(),
            name: "Operations".to_string(),
            description: None,
            members: Vec::new(),
            responder: crate::ports::types::ResponderMode::default(),
            hive: Default::default(),
        });
        for spelling in ["ops_desk", "Operations"] {
            assert_eq!(
                desk_aliases(&record, Some(spelling)),
                ("ops_desk".to_string(), "Operations".to_string()),
                "{spelling:?} is the console-created desk"
            );
        }
    }

    /// An exact id beats another desk's display name.
    ///
    /// Desk creation enforces unique ids but **not** unique names, so
    /// `{id: "ops_desk", name: "sales"}` is valid and can sit ahead of
    /// `{id: "sales", …}`. A single pass matching `id == key || name == key`
    /// answers with whichever came first, so asking for the desk `sales` got
    /// `ops_desk` — and since this returns a *pair*, the damage is worse than a
    /// miss: `owns` would be handed one desk's id and another's name, merging
    /// two conversations that have nothing to do with each other.
    ///
    /// The precedence itself is `CompanyRecord::resolve_desk_id`'s, which this
    /// now defers to rather than keeping a second, laxer copy of.
    #[test]
    fn an_exact_id_wins_over_an_earlier_desks_display_name() {
        let mut record = record_with_group_chat("ops_desk", "sales");
        record
            .manifest
            .group_chats
            .push(toml::from_str("id = \"sales\"\nname = \"Sales\"").expect("a desk"));
        assert_eq!(
            desk_aliases(&record, Some("sales")).0,
            "sales",
            "the desk whose id is `sales` owns that key"
        );
    }

    #[tokio::test]
    async fn resolve_none_is_the_general_desk() {
        assert_eq!(
            resolve(RecordStore(None), None).await,
            (GENERAL_DESK.to_string(), GENERAL_DESK.to_string())
        );
    }

    #[tokio::test]
    async fn resolve_general_spelling_short_circuits_without_a_store_read() {
        // The store would panic on `save`/`list`, but a General spelling must not
        // even reach `load` — it returns `(chat, chat)`, which owns folds.
        assert_eq!(
            resolve(RecordStore(None), Some("main")).await,
            ("main".to_string(), "main".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_named_desk_by_id_returns_the_manifest_name() {
        // Addressed by id; the seed must carry the name too, or a line journaled
        // under the name would be missed. This is the exact "looks fixed but seeds
        // nothing" trap the resolution guards against.
        let store = RecordStore(Some(record_with_group_chat("eng-123", "Engineering")));
        assert_eq!(
            resolve(store, Some("eng-123")).await,
            ("eng-123".to_string(), "Engineering".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_unmatched_selector_passes_through_verbatim() {
        let store = RecordStore(Some(record_with_group_chat("eng-123", "Engineering")));
        assert_eq!(
            resolve(store, Some("ad-hoc-thread")).await,
            ("ad-hoc-thread".to_string(), "ad-hoc-thread".to_string())
        );
    }
}
