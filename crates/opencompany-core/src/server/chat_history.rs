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
    Actor, ActorKind, Attachment, ChatOutput, ChatOutputKind, CompanyEvent, CompanyId,
    CompanyRecord, EventSeq, Mention, MentionTarget, StoredEvent, TurnStep,
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

    // **The private line this teammate shares with each of the others (#2368).**
    //
    // A pair thread is `dm:<a>+<b>` — neither a desk nor this agent's own
    // direct line — so it matched nothing above, and an agent could not read
    // back a conversation it had itself been part of. Two seats settled
    // something and rediscovered it from scratch the next day.
    //
    // Enumerable rather than searchable: `pair_conversation` sorts the two
    // ids, so the thread this agent shares with any teammate is computable
    // without an index, and one that never happened simply holds no rows.
    // Only threads this agent is IN: every key is built from its own id, so a
    // pair between two other people is not addressable here at all.
    //
    // Agent-scoped by construction. Every caller of this function reads on
    // behalf of ONE agent — its session delta, its own speech targets, and the
    // console's Session tab for that agent — so this adds no channel to the
    // operator's rail.
    let mut partners: Vec<String> = Vec::new();
    for agent in record
        .manifest
        .agents
        .iter()
        .map(|a| a.id.clone())
        .chain(record.overlay_agents.iter().map(|a| a.id.clone()))
    {
        if agent != agent_id && !partners.contains(&agent) {
            partners.push(agent);
        }
    }
    for partner in partners {
        let thread = crate::hivemind::referral::pair_conversation(agent_id, &partner);
        if seen.insert(thread.clone()) {
            channels.push(Channel {
                label: format!("@{partner}"),
                name: thread.clone(),
                id: thread,
            });
        }
    }

    channels
}

/// The desk's display name, falling back to its id.
pub(crate) fn desk_display_name(record: &CompanyRecord, desk_id: &str) -> String {
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
    /// Which side of the crossing this desk is on.
    ///
    /// Every other field is named from the asking desk's point of view, because
    /// until now that was the only desk a crossing was folded onto. The desk
    /// that was ASKED carries the same exchange with the roles swapped, and a
    /// label that cannot tell them apart renders it backwards: `exchanges`
    /// answering a question reads as "asked #order_ops", which is the one thing
    /// that did not happen. The host says it for the same reason
    /// [`ReferredFrom::returning`] exists — both sides are agent lines on a
    /// desk, so nothing else on the row distinguishes them.
    pub inbound: bool,
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
            outputs: Vec::new(),
            resolution_user_facing: false,
            resolution_code: None,
            resolution_pair_agent_id: None,
            resolution_provider_slug: None,
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
    /// Addressable workspace objects produced by this reply's turn.
    ///
    /// Empty for messages predating output tracking and after every recorded
    /// target has been removed. The journal remains append-only; history
    /// projects only targets that are still clickable.
    pub outputs: Vec<ChatOutput>,
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
    /// Whether this row is a classified resolution failure (keys rework
    /// #2306, round-2 review KR-L2-03) — a pinned provider gone or switched
    /// off, a broken company default, a provider with no key, or no model
    /// chosen at all. `false` for every ordinary reply and every other
    /// failure class (a tool timeout, an empty response, a rate limit),
    /// which keep only [`Self::text`]'s generic retry wording, exactly as
    /// before this field existed.
    ///
    /// Derived in [`Self::project`] from the stored `AgentReply`'s own
    /// `text` — `server::operator::spawn_chat_turn` writes the bare X9
    /// sentence there, unwrapped, for exactly this class of failure — rather
    /// than a new field on [`CompanyEvent::AgentReply`] itself, so the ~30
    /// other call sites that construct that variant need no change.
    pub resolution_user_facing: bool,
    /// One of the codes `docs/key-reworks/in-use-guards.md` §5 names, when
    /// [`Self::resolution_user_facing`] is `true`.
    pub resolution_code: Option<String>,
    /// The agent this failure's *pair* names, when the classifier could
    /// recover one (`company::inference::copy::classify`'s hidden marker —
    /// today, only the pin pre-check attaches one).
    pub resolution_pair_agent_id: Option<String>,
    /// The provider slug the failure names, when the classifier could
    /// recover one — only `pair_provider_removed`'s sentence names a raw
    /// slug rather than a display label (there is no row left to read one
    /// from); every other code leaves this `None`.
    pub resolution_provider_slug: Option<String>,
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
                outputs,
                parent,
                mentions,
                audience,
                ..
            } => {
                // Keys rework #2306, round-2 review KR-L2-03: re-classifies
                // the stored `text` against the same X9 sentences
                // `server::operator::spawn_chat_turn` writes verbatim (no
                // "Nothing was left half-done" wrapper) for exactly this
                // class of failure. `None` for every ordinary reply and
                // every other failure class, which keep their existing text
                // unchanged.
                let resolution = crate::company::inference::copy::classify(&text);
                MessageView {
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
                    outputs,
                    parent_id: parent.map(|seq| seq.value().to_string()),
                    reactions: Vec::new(),
                    mentions: project_mentions(&mentions, authors, viewer),
                    // A reply is the company's own voice and carries no operator
                    // upload (issue #1682).
                    attachments: Vec::new(),
                    resolution_user_facing: resolution.is_some(),
                    resolution_code: resolution.as_ref().map(|r| r.code.to_string()),
                    resolution_pair_agent_id: resolution
                        .as_ref()
                        .and_then(|r| r.pair_agent_id.clone()),
                    resolution_provider_slug: resolution.and_then(|r| r.provider_slug),
                }
            }
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
                    outputs: Vec::new(),
                    parent_id: parent.map(|seq| seq.value().to_string()),
                    reactions: Vec::new(),
                    mentions: project_mentions(&mentions, authors, viewer),
                    // Issue #1682: the operator's attached files, carried
                    // through so a reload renders the same chips the live send
                    // showed.
                    attachments,
                    // An operator message is never a resolution failure — that
                    // notice is authored by the runtime (`AgentReply`, above).
                    resolution_user_facing: false,
                    resolution_code: None,
                    resolution_pair_agent_id: None,
                    resolution_provider_slug: None,
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
                outputs: Vec::new(),
                // Rendered the same way an `OperatorMessage`'s parent is, a few
                // arms up — the console keys a thread off this string and does
                // not care which event minted it.
                parent_id: origin_parent.map(|seq| seq.value().to_string()),
                reactions: Vec::new(),
                mentions: Vec::new(),
                attachments: Vec::new(),
                // A dispatch marker is never a resolution failure.
                resolution_user_facing: false,
                resolution_code: None,
                resolution_pair_agent_id: None,
                resolution_provider_slug: None,
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
                outputs: Vec::new(),
                parent_id: None,
                reactions: Vec::new(),
                mentions: Vec::new(),
                attachments: Vec::new(),
                // The defensive fallback is never a resolution failure.
                resolution_user_facing: false,
                resolution_code: None,
                resolution_pair_agent_id: None,
                resolution_provider_slug: None,
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
    drop_dead_outputs(runtime, &mut messages).await?;
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
            rows: exchange_rows,
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
        // **A crossing that went to a PERSON lives in its own conversation.**
        //
        // A desk crossing runs the target's turn on the far desk, so its legs
        // are desk rows and the loop below finds them. A crossing addressed to
        // one teammate runs in the pair conversation the marker names — the
        // `dm:<a>+<b>` key `referral::pair_conversation` mints — and *no* row
        // lands on this desk at all. So the matcher found nothing, the marker
        // was skipped, and the exchange rendered nowhere: written, keyed,
        // durable, and invisible on every surface.
        //
        // Folded onto the asking row rather than the report, because there is
        // no report — the asker did not hand the room an answer, it asked a
        // neighbour and carried on. `direct` is already true here (it reads
        // `conversation.is_some()` below), which is what makes the console
        // label it `@name` rather than `#desk`.
        if let Some(pair) = conversation.as_deref() {
            // **This crossing's exchange, not every exchange this pair ever
            // had.**
            //
            // `pair_conversation` is deterministic — the same two agents always
            // produce the same `dm:<a>+<b>` key — so collecting to the end of
            // the page gave the FIRST crossing every row the pair went on to
            // exchange, the second all but the first, and so on. One live
            // episode rendered the same conversation five times in one thread,
            // labelled 20, 16, 12, 8 and 4 messages; only the last was true
            // (Codex, #2332).
            //
            // Bounded at the next crossing into the SAME pair thread, which is
            // where this one's exchange ends by construction: the rows between
            // two markers are the rows that marker caused.
            //
            // **Unless the marker names its own rows.** The forward scan assumes
            // the rows follow the marker, which holds for a deliberation
            // crossing — the marker is written first and the turns follow. A
            // tool-sent `desk_dm` inverts it: the rows are journaled while the
            // turn runs, and the marker folds onto that turn's own reply, which
            // is composed afterwards. Scanning forward from such a marker finds
            // the answer and misses the question it is a chip for. A marker that
            // carries its range is read by range instead, which is true whichever
            // side of it the rows landed on (#2368).
            let next_marker = page[index + 1..]
                .iter()
                .position(|later| {
                    matches!(
                        &later.event,
                        CompanyEvent::ReferralEnqueued {
                            conversation: Some(next),
                            ..
                        } if next == pair
                    )
                })
                .map(|at| index + 1 + at);
            // **The boundary is the next crossing's first ROW, not its marker.**
            //
            // A deliberation crossing mints its marker first and the turns
            // follow, so marker order and row order agree and cutting at the
            // next marker is exact. A `desk_dm` inverts it: the question is
            // journaled mid-turn and the marker minted afterwards, onto the
            // turn's own reply. The rows between marker A and marker B then
            // include the ones B was minted FOR, so cutting at B let a settled
            // crossing absorb the opening of the next one — a two-line chip
            // grew to four the moment the same pair spoke again, the two extra
            // lines being questions of a crossing still in flight, shown as
            // part of an exchange that had already finished.
            let next_for_pair = match next_marker {
                Some(at) => {
                    let claimed = match &page[at].event {
                        CompanyEvent::ReferralEnqueued {
                            rows: Some((opened, _)),
                            ..
                        } => page[index + 1..at]
                            .iter()
                            .position(|row| row.seq.value() >= *opened)
                            .map(|before| index + 1 + before),
                        _ => None,
                    };
                    claimed.unwrap_or(at)
                }
                None => page.len(),
            };
            // A range EXTENDS the forward scan, it does not replace it. The rows
            // a marker names are the ones already written when it was minted —
            // a tool's own DM, journaled mid-turn — and the rows that follow it
            // are the answer coming back. Reading only the range showed the
            // question and dropped the reply; reading only forward did the
            // reverse.
            let scanned = match exchange_rows {
                Some(_) => page.as_slice(),
                None => &page[index + 1..next_for_pair],
            };
            let mut lines = Vec::new();
            for (at, later) in scanned.iter().enumerate() {
                if let Some((opened, closed)) = exchange_rows {
                    let seq = later.seq.value();
                    let named = (*opened..=*closed).contains(&seq);
                    let follows = at > index && at < next_for_pair;
                    if !named && !follows {
                        continue;
                    }
                }
                let CompanyEvent::AgentReply {
                    chat_id, agent_id, ..
                } = &later.event
                else {
                    continue;
                };
                if chat_id != pair {
                    continue;
                }
                let Some(words) = strip_relay_note(&later.event) else {
                    continue;
                };
                lines.push(ReferralLine {
                    author_id: agent_id.clone(),
                    // Empty for the agent that asked: the console already names
                    // the row this is folded onto, exactly as the desk case does.
                    author_label: if agent_id == asker {
                        String::new()
                    } else {
                        target.clone()
                    },
                    // The ask is a committed MOVE — the grammar is addressed to
                    // the fold, never to a person reading a transcript.
                    text: readable_moves(words),
                    outbound: agent_id == asker,
                });
            }
            if !lines.is_empty()
                && let Some(view) = messages
                    .iter_mut()
                    .find(|m| m.id == trigger_sequence.to_string())
            {
                view.referral_conversation = Some(ReferralConversation {
                    direct: true,
                    // Folded onto the asking row, by construction: this branch
                    // matches on the asker's own trigger sequence.
                    inbound: false,
                    asker_id: asker.clone(),
                    other_id: target.clone(),
                    other_desk_id: to_desk.clone(),
                    other_desk_name: from_desk_name.clone(),
                    lines,
                });
            }
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
        // **Bounded at the next crossing.** A crossing's own child is journaled
        // by the turn that produced it, so it lands before any later crossing is
        // raised. Unbounded, a crossing that FAILED — which journals nothing on
        // the desk it went to — matched that agent's next ordinary reply there,
        // whenever it happened, and stamped it with the failed question's
        // provenance (Codex, #2332).
        //
        // This narrows the window rather than closing it: a failed crossing with
        // no later marker on the page can still reach forward. Closing it needs
        // the far desk to record the failure the way the asking desk does, which
        // is a journal change and not a projection one.
        //
        // Bounded at the next crossing BETWEEN THE SAME TWO DESKS, in the same
        // direction — not at the next marker on the company's journal. `page` is
        // company-wide, so any other pair's crossing was ending this window and
        // a child journaled after it was skipped, dropping an answered crossing
        // from the projection entirely. `to_desk` alone is not enough either:
        // two different desks can ask the same one (Codex and CodeRabbit both,
        // #2332).
        let next_marker = page[index + 1..]
            .iter()
            .position(|later| {
                matches!(
                    &later.event,
                    CompanyEvent::ReferralEnqueued {
                        from_desk: next_from,
                        to_desk: next_to,
                        ..
                    } if next_from == from_desk && next_to == to_desk
                )
            })
            .map_or(page.len(), |at| index + 1 + at);
        let Some(child) = page[index + 1..next_marker]
            .iter()
            .find(|later| match &later.event {
                CompanyEvent::OperatorMessage { chat, by, .. } => {
                    chat.as_deref() == Some(to_desk.as_str())
                        && by.as_ref().is_some_and(|actor| actor.id == *asker)
                }
                CompanyEvent::AgentReply {
                    chat_id, agent_id, ..
                } => {
                    chat_id == to_desk
                        && (agent_id == crate::hivemind::HIVE_REFERRAL_AUTHOR
                        // **The answering desk's own view of the crossing.**
                        //
                        // The two shapes above are both the ASKING desk's: the
                        // relay landing there, or the answer folded home under
                        // the reserved author. On the desk that was asked, the
                        // only row is the target's own turn — it is a real
                        // member of this desk speaking here, under its own id —
                        // so neither matched and the marker went unattached.
                        //
                        // That left the answering side reading an answer to a
                        // question nobody could see: `exchanges` explaining that
                        // a refund is not its tool, with no indication that
                        // `cancellations` on another desk had asked. The forward
                        // marker already names the asker, their desk and the
                        // sequence to link to; this is the row to hang it on.
                        || agent_id == target)
                }
                _ => false,
            })
        else {
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
                // The question row this crossing left on the desk it asked, when
                // it left one: a deliberated crossing journals it there so the
                // room can read what it is answering, and roots every turn on
                // it. The thread key for the fold below.
                let asked_row: Option<EventSeq> = answers.and_then(|seq| {
                    let forward = reachable.iter().find(|stored| stored.seq.value() == seq)?;
                    reachable
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
                        .map(|stored| stored.seq)
                });
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
                //
                // `asked_message` because a *deliberated* crossing journals the
                // whole referral prompt on the desk it asked — the room has to
                // read the question to deliberate on it — and that row is what
                // the matcher above finds. Unwrapped, or the fold renders the
                // instructions written for the far desk in place of the ask.
                if let Some(text) = question {
                    lines.push(ReferralLine {
                        author_id: target.clone(),
                        author_label: String::new(),
                        text: readable_moves(crate::hivemind::referral::asked_message(&text)),
                        outbound: true,
                    });
                }
                // **What the far desk actually said, when it was a room.**
                //
                // A single-seat crossing has one line to show and it is the
                // answer that came home. A crossing that convened the far desk
                // has a whole conversation — every turn journaled on that desk,
                // between the forward marker and the return — and folding only
                // the conclusion showed none of it: the collapsed crossing read
                // "asked #returns · 2 messages" over the question and one
                // summary, for an exchange that ran five turns across both
                // seats.
                //
                // Attributed per member, because that is the point: the reader
                // is looking at a conversation between two desks and needs to
                // see which seat said what. The relayed note is dropped when
                // these exist — for a converged room it summarises exactly
                // these lines, and for one that did not converge it *is* these
                // lines, joined.
                // **Scoped to the crossing's own thread, not to a window.**
                //
                // A sequence interval plus `chat_id` is not enough: a desk that
                // was asked can be running its own episode, or answering an
                // ordinary message, while the referred room deliberates — and
                // every one of those rows falls inside the same interval on the
                // same desk, so an unrelated reply would render as part of this
                // crossing (CodeRabbit, #2332).
                //
                // The referred episode roots every turn on the question row it
                // journals, which is what `asked_row` names. Matching that
                // parent is exact rather than probabilistic. A crossing with no
                // such row — the single-seat path, and every marker written
                // before it existed — matches nothing here and falls through to
                // the relayed answer below, which is what it had before.
                let room: Vec<ReferralLine> = reachable
                    .iter()
                    .filter(|later| {
                        matches!(
                            &later.event,
                            CompanyEvent::AgentReply { parent: Some(parent), .. }
                                if asked_row.is_some_and(|root| *parent == root)
                        )
                    })
                    .filter_map(|later| match &later.event {
                        CompanyEvent::AgentReply {
                            chat_id,
                            agent_id,
                            text,
                            audience,
                            ..
                        } if chat_id == from_desk
                            && !crate::hivemind::is_hive_author(agent_id)
                            // **An aside is not a turn, here as in `turns_of`.**
                            //
                            // A room's `!aside` is journaled with the same
                            // parent as its turns, so scoping by thread admits
                            // it. Folded into the crossing it would publish a
                            // private pair exchange on the ASKING desk — the
                            // same cross-desk leak the answer builder was fixed
                            // for, in the other projection of the same rule
                            // (Codex, #2332).
                            && audience.is_empty() =>
                        {
                            Some(ReferralLine {
                                author_id: agent_id.clone(),
                                author_label: agent_id.clone(),
                                text: readable_moves(text.clone()),
                                outbound: false,
                            })
                        }
                        _ => None,
                    })
                    .collect();
                match room.is_empty() {
                    false => lines.extend(room),
                    true => {
                        if let Some(text) = answer {
                            lines.push(ReferralLine {
                                author_id: asker.clone(),
                                author_label: asker_label.clone(),
                                // The room's attribution removed — this line is
                                // already attributed by the fold that carries
                                // it. See `referral::unattributed`.
                                text: readable_moves(crate::hivemind::referral::unattributed(
                                    asker,
                                    from_desk_name,
                                    &text,
                                )),
                                outbound: false,
                            });
                        }
                    }
                }
                relayed.push(child_id);
                if let Some(view) = messages.iter_mut().find(|m| m.id == id) {
                    if !lines.is_empty() {
                        view.referral_conversation = Some(ReferralConversation {
                            direct,
                            // The return leg, folded onto the asker's report on
                            // the desk that asked.
                            inbound: false,
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
                // **The question, on the desk that was asked it.**
                //
                // The chip already says a crossing happened and names who
                // raised it, and that was all this side got: `exchanges`
                // explaining that a refund is not its tool, over a chip reading
                // "Asked by @cancellations", with the question itself nowhere on
                // the desk. It lives on the ASKING desk — the asker's own
                // committed line, which `trigger_sequence` names — so a reader
                // here had to go and find another desk's transcript to learn
                // what was actually asked.
                //
                // Folded the same way the asking side folds the answer, with
                // the roles swapped (`inbound`). One line, not two: the row
                // this hangs on IS this desk's answer, so including it would
                // print the same words twice.
                //
                // Only for the answering shape — a forward marker whose child
                // is the target's own turn here. The relay fallback below is
                // the asking desk reading a delivered copy, which already has
                // the question as its own text.
                let answering = matches!(
                    &child.event,
                    CompanyEvent::AgentReply { agent_id, .. } if agent_id == target,
                ) && !*returning;
                // Read by sequence when the window does not reach it: this
                // desk's oldest visible row says nothing about how far back the
                // other desk's question sits.
                let asked_leg: Vec<StoredEvent> =
                    match answering && !page.iter().any(|st| st.seq.value() == *trigger_sequence) {
                        true => runtime
                            .events()
                            .read_from(runtime.id(), EventSeq::new(*trigger_sequence), 1)
                            .await
                            .unwrap_or_default(),
                        false => Vec::new(),
                    };
                let question = answering
                    .then(|| {
                        page.iter()
                            .chain(asked_leg.iter())
                            .find(|st| st.seq.value() == *trigger_sequence)
                            .and_then(|st| strip_relay_note(&st.event))
                    })
                    .flatten();
                if let Some(view) = messages.iter_mut().find(|m| m.id == child_id) {
                    if let Some(text) = question {
                        view.referral_conversation = Some(ReferralConversation {
                            direct,
                            inbound: true,
                            // Named from this desk's point of view, as on the
                            // asking side: the local agent, then the far one.
                            asker_id: target.clone(),
                            other_id: asker.clone(),
                            other_desk_id: from_desk.clone(),
                            other_desk_name: from_desk_name.clone(),
                            lines: vec![ReferralLine {
                                author_id: asker.clone(),
                                author_label: asker_label.clone(),
                                // A room's question is a committed MOVE, and the
                                // grammar is addressed to the fold.
                                text: readable_moves(text),
                                outbound: false,
                            }],
                        });
                    }
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
    if !text.lines().any(|line| {
        crate::hivemind::line_kind(line).is_some()
            || crate::hivemind::completion::is_completion_line(line)
    }) {
        return text;
    }
    text.lines()
        .map(|line| {
            // Completion grammar first: `!broadcast` and `!complete` are this
            // host's markers addressed to the *host*, and a reader shown them
            // is being shown plumbing.
            if let Some(rendered) = crate::hivemind::completion::readable(line) {
                // Nothing is dropped: a marker line is part of what the member
                // said, and a row rendering to nothing leaves an operator
                // looking at a turn that appears not to have happened. A bare
                // marker renders as a plain sentence instead — see
                // `completion::readable`.
                return rendered;
            }
            crate::hivemind::readable(line).unwrap_or_else(|| line.to_string())
        })
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

/// Removes reply output links whose target no longer exists.
///
/// Like [`drop_dead_cards`], this is a projection rule rather than a journal
/// rewrite: the turn really did produce the object, but history must not
/// rehydrate a button that now leads nowhere.
async fn drop_dead_outputs(
    runtime: &CompanyRuntime,
    messages: &mut [MessageView],
) -> Result<(), OpenCompanyError> {
    if !messages.iter().any(|message| !message.outputs.is_empty()) {
        return Ok(());
    }

    let needs_workspace = messages.iter().any(|message| {
        message
            .outputs
            .iter()
            .any(|output| output.kind == ChatOutputKind::WorkspaceNode)
    });
    let live_workspace: HashSet<String> = if needs_workspace {
        runtime
            .workspace()
            .tree(runtime.id())
            .await?
            .into_iter()
            .map(|node| node.id)
            .collect()
    } else {
        HashSet::new()
    };
    let needs_artifacts = messages.iter().any(|message| {
        message
            .outputs
            .iter()
            .any(|output| output.kind == ChatOutputKind::Artifact)
    });
    let live_artifacts: HashSet<(String, String, u32)> = if needs_artifacts {
        runtime
            .artifacts()
            .list(runtime.id(), None)
            .await?
            .into_iter()
            .flat_map(|artifact| {
                artifact.versions.into_iter().map(move |version| {
                    (
                        artifact.id.clone(),
                        artifact.task_id.clone(),
                        version.version,
                    )
                })
            })
            .collect()
    } else {
        HashSet::new()
    };

    for message in messages {
        let mut kept = Vec::with_capacity(message.outputs.len());
        for output in message.outputs.drain(..) {
            let live = match output.kind {
                ChatOutputKind::WorkspaceNode => live_workspace.contains(&output.target_id),
                ChatOutputKind::Artifact => {
                    let Some(task_id) = output.task_id.as_deref() else {
                        continue;
                    };
                    let Some(version) = output.version else {
                        continue;
                    };
                    live_artifacts.contains(&(
                        output.target_id.clone(),
                        task_id.to_string(),
                        version,
                    ))
                }
            };
            if live {
                kept.push(output);
            }
        }
        message.outputs = kept;
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
#[path = "chat_history_attribution_audit_tests.rs"]
mod attribution_audit;
#[cfg(test)]
#[path = "chat_history_mentions_tests.rs"]
mod tests_mentions;
#[cfg(test)]
#[path = "chat_history_moves_tests.rs"]
mod tests_moves;
#[cfg(test)]
#[path = "chat_history_reactions_tests.rs"]
mod tests_reactions;
#[cfg(test)]
#[path = "chat_history_terminal_tests.rs"]
mod tests_terminal;

#[cfg(test)]
#[path = "chat_history_dead_card_test.rs"]
mod dead_card_test;

#[cfg(test)]
#[path = "chat_history_referral_origin_crossing_test.rs"]
mod referral_origin_crossing_test;
#[cfg(test)]
#[path = "chat_history_referral_origin_episode_test.rs"]
mod referral_origin_episode_test;
#[cfg(test)]
#[path = "chat_history_referral_origin_relay_test.rs"]
mod referral_origin_relay_test;
/// Where a referred line says it came from, and who it says is speaking.
#[cfg(test)]
#[path = "chat_history_referral_origin_test_support.rs"]
mod referral_origin_test_support;

/// How a chat selector becomes the `(desk id, desk name)` pair [`owns`] filters
/// on — the one answer to "which desk is this", shared by the seed, the cycle's
/// briefings and `read_thread`.
#[cfg(test)]
#[path = "chat_history_desk_resolution_test.rs"]
mod desk_resolution_test;
