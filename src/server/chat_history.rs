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

use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::{
    Actor, ActorKind, CompanyEvent, CompanyRecord, EventSeq, StoredEvent, TurnStep,
};
use crate::server::ops::language::DEFAULT_DESK as GENERAL_DESK;

/// The console's default/orchestrator thread id
/// (`frontend/src/lib/threads.ts` `mainThread()`). The console addresses every
/// send on that thread with `chat: "main"`, so `AgentReply`s answering it are
/// journaled with `chat_id == "main"` rather than [`GENERAL_DESK`]. `owns`
/// admits both spellings for the General desk so a transcript is never split
/// across the two ids depending on which one happened to write it (issue #65).
pub const MAIN_THREAD_ID: &str = "main";

/// The largest message page either history surface may materialize. Keeping
/// the limit beside the shared reader prevents a new caller from turning its
/// `Vec` reservation back into an allocation controlled by the request.
pub const CHAT_HISTORY_PAGE_LIMIT: usize = 200;

/// Does this stored chat id mean the General desk?
///
/// **Four spellings, one desk.** The console addresses its default thread as
/// `"main"`, the chat route stores an unaddressed message as `None`, older
/// events carry `""`, and the desk's own id/name is `"General"`. [`owns`] has
/// admitted all four since issue #65, which is what stops a transcript from
/// splitting across whichever id happened to write each message.
///
/// Exposed because that equivalence is **not** local to history rendering.
/// `CompanyRuntime::resolvable_parent` compares a remembered thread root's chat
/// id against the channel being answered into, and comparing the raw strings
/// there made a root stored as `None` fail to match the `"General"` it is
/// rendered under — so a threaded approval rooted in an unaddressed message
/// silently resumed in the channel, which is the exact symptom issue #435 set
/// out to remove. Two places deciding "same conversation?" by different rules
/// is the drift; one function is the fix. See [`same_conversation`].
pub fn is_general_chat(chat: Option<&str>) -> bool {
    match chat {
        None => true,
        Some(chat) => {
            chat.is_empty()
                || chat.eq_ignore_ascii_case(MAIN_THREAD_ID)
                || chat.eq_ignore_ascii_case(GENERAL_DESK)
        }
    }
}

/// Do two stored chat ids name the same conversation (issue #435)?
///
/// Every spelling of the General desk is one conversation — see
/// [`is_general_chat`] — and everything else compares verbatim, because a desk
/// id is an opaque identifier and two desks differing only in case are two
/// desks. Deliberately **not** a general-purpose case-insensitive compare: the
/// folding is a fact about one desk's history, not a licence to loosen the
/// others.
pub fn same_conversation(a: Option<&str>, b: Option<&str>) -> bool {
    if is_general_chat(a) || is_general_chat(b) {
        return is_general_chat(a) && is_general_chat(b);
    }
    a == b
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
    /// The message text.
    pub text: String,
    /// When it was journaled, epoch millis.
    pub at_millis: f64,
    /// Whether it is the operator's own message.
    pub mine: bool,
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
                ..
            } => MessageView {
                id,
                channel: agent_id.clone(),
                author: agent_id,
                text,
                at_millis,
                mine: false,
                steps,
                task_id,
                parent_id: parent.map(|seq| seq.value().to_string()),
                reactions: Vec::new(),
            },
            CompanyEvent::OperatorMessage {
                text, by, parent, ..
            } => {
                let (author, mine) = match &by {
                    // Sent by a signed-in human.
                    Some(actor) if actor.kind == ActorKind::User => {
                        let label = authors
                            .get(&actor.id)
                            .cloned()
                            .unwrap_or_else(|| "someone".to_string());
                        (label, *viewer == Viewer::User(actor.id.clone()))
                    }
                    // Sent with a machine credential, or journaled before
                    // attribution existed. Either way there is no person to
                    // name, and it belongs to whoever holds that credential.
                    _ => ("operator".to_string(), matches!(viewer, Viewer::Operator)),
                };
                MessageView {
                    id,
                    channel: "operator".to_string(),
                    author,
                    text,
                    at_millis,
                    mine,
                    steps: Vec::new(),
                    task_id: None,
                    parent_id: parent.map(|seq| seq.value().to_string()),
                    reactions: Vec::new(),
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
            // No `steps` and no `parent_id`: a marker is not a turn and is
            // never threaded, so `ThreadPanel` needs nothing from it.
            CompanyEvent::DeskTaskCompleted {
                task_id, column, ..
            } => MessageView {
                id,
                channel: crate::ports::SYSTEM_AUTHOR.to_string(),
                author: crate::ports::SYSTEM_AUTHOR.to_string(),
                text: dispatch_marker_text(&column),
                at_millis,
                mine: false,
                steps: Vec::new(),
                task_id: Some(task_id),
                parent_id: None,
                reactions: Vec::new(),
            },
            // `owns` never admits other variants into a history.
            other => MessageView {
                id,
                channel: crate::ports::SYSTEM_AUTHOR.to_string(),
                author: crate::ports::SYSTEM_AUTHOR.to_string(),
                text: format!("{other:?}"),
                at_millis,
                mine: false,
                steps: Vec::new(),
                task_id: None,
                parent_id: None,
                reactions: Vec::new(),
            },
        }
    }
}

/// The blast radius of issue #885, for one company.
///
/// Reported rather than repaired. See [`channel_attributed_replies`] for why a
/// repair is not available.
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
/// The roster, **plus two ids that are truthful authors without being teammates**.
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
/// This is the single predicate the audit and any presentation of its result
/// must share; two copies would let the count and the rendering disagree about
/// which rows are unknown.
pub fn is_known_author(agent_id: &str, record: &CompanyRecord) -> bool {
    agent_id == crate::ports::CONFINED_AGENT_ID
        || agent_id == crate::ports::SYSTEM_AUTHOR
        || record.resolve_roster_agent_id(agent_id).is_some()
}

pub async fn channel_attributed_replies(
    runtime: &CompanyRuntime,
    record: &CompanyRecord,
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
        audit.fold(&page, |agent_id| is_known_author(agent_id, record));
        cursor = EventSeq::new(last.value() + 1);
    }
    Ok(audit)
}

/// Loads roster display labels for a company: user id → label.
///
/// Prefers a display name, and falls back to the email's *local part* rather
/// than the whole address: a desk history is read by every member, and it
/// should not hand each of them everyone else's email.
pub async fn author_labels(
    runtime: &CompanyRuntime,
) -> Result<HashMap<String, String>, OpenCompanyError> {
    let users = runtime.users().list_users(runtime.id()).await?;
    Ok(users
        .into_iter()
        .map(|user| {
            let label = user.display_name.unwrap_or_else(|| {
                user.email
                    .split('@')
                    .next()
                    .unwrap_or("someone")
                    .to_string()
            });
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
                messages.push(MessageView::project(event, viewer, &authors));
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
    Ok(messages)
}

/// Counts a desk's messages before a cursor without materialising them.
///
/// GraphQL's [`Page`](crate::server::graphql::pagination::Page) exposes an
/// unpaginated `total`, while the REST transcript endpoint deliberately does
/// not. Keep that potentially full journal walk out of [`history_for_desk`],
/// so bounded transcript readers stop as soon as their requested window is
/// complete.
pub async fn history_total_for_desk(
    runtime: &CompanyRuntime,
    desk_id: &str,
    desk_name: &str,
    before_seq: Option<u64>,
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
            if owns(desk_id, desk_name, &event.event) {
                total = total.saturating_add(1);
            }
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::tasks::{
        COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_PLANNING,
        COLUMN_TODO,
    };
    use crate::ports::types::Actor;

    fn agent_reply(chat_id: &str) -> CompanyEvent {
        CompanyEvent::AgentReply {
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
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: chat.map(str::to_string),
            deliverable: None,
        }
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
            parent: None,
            text: "hi".to_string(),
            by: Some(Actor {
                kind: ActorKind::User,
                id: "u1".to_string(),
            }),
            chat: Some(MAIN_THREAD_ID.to_string()),
            deliverable: None,
        };
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    // Regression: issue — operator messages vanished on reload because the read
    // filter ignored the stored chat id.
    #[test]
    fn main_thread_owns_operator_messages_it_stored() {
        let event = CompanyEvent::OperatorMessage {
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: Some(MAIN_THREAD_ID.to_string()),
            deliverable: None,
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
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: Some("strategy".to_string()),
            deliverable: None,
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

    /// A thread parent survives projection on both halves of an exchange, as
    /// the message id a reader can resolve rather than a raw sequence number.
    #[test]
    fn project_carries_the_thread_parent() {
        let operator = MessageView::project(
            at(
                12,
                CompanyEvent::OperatorMessage {
                    parent: Some(EventSeq::new(4)),
                    text: "a follow-up".to_string(),
                    by: None,
                    chat: Some("studio".to_string()),
                    deliverable: None,
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
            parent: None,
            text: "hi".to_string(),
            by: None,
            chat: None,
            deliverable: None,
        };
        assert!(owns(GENERAL_DESK, GENERAL_DESK, &event));
        assert!(!owns("strategy", "Strategy desk", &event));
    }

    /* ---- issue #377: the dispatch terminal as a channel marker ---- */

    /// A settled dispatch, as the harness journals it. `desk` is deliberately
    /// an agent id (`engineer`) and never a channel id (`engineering`) — that
    /// difference is the whole reason the origin has to be carried.
    fn desk_task_completed(origin: Option<&str>, column: &str) -> CompanyEvent {
        CompanyEvent::DeskTaskCompleted {
            task_id: "t-1".to_string(),
            desk: "engineer".to_string(),
            output: "the run's prose".to_string(),
            column: column.to_string(),
            artifact_ids: Vec::new(),
            origin_chat_id: origin.map(str::to_string),
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
        assert!(view.parent_id.is_none(), "a marker is never threaded");
        assert_eq!(view.id, "21", "the host id the console dedupes a reload on");
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
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
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
                            text: "hello".to_string(),
                            by: None,
                            chat: None,
                            parent: None,
                            deliverable: None,
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
