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

/// Whether something that recorded *where it was raised* was raised in `desk`.
///
/// The same question [`same_conversation`] answers, minus its one fold: a
/// `None` here means **no conversation raised this**, not "unaddressed,
/// therefore General".
///
/// The distinction is the whole point, and [`owns`] below already draws it
/// inline for a dispatch terminal — "**`None` is not the General desk** …
/// It is the single most bug-prone line in this function." It is named here
/// because a second caller needed it and did not have it: a blocker parked by
/// a workflow node records `thread: None`, deliberately ("a workflow run has
/// no board card behind it and no conversation to raise the question in"), and
/// `pending_blocker_groups` matched it with `same_conversation` against the
/// General desk. An unaddressed console post is exactly `chat: None` folded to
/// the `General` default desk, and almost any substantive sentence classifies
/// as an amend — so an ordinary message about something else silently settled
/// an approval nobody had decided. It left the approvals queue, the run went on
/// waiting, and the three screens that read it disagreed for the rest of its
/// life.
///
/// Use this wherever a `None` means "nobody raised this"; use
/// [`same_conversation`] where it means "the id was never addressed".
pub fn raised_in(thread: Option<&str>, desk: &str) -> bool {
    match thread {
        None => false,
        Some(thread) => same_conversation(Some(thread), Some(desk)),
    }
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
                ..
            } => MessageView {
                id,
                channel: agent_id.clone(),
                admin_only: agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR,
                author: agent_id,
                text,
                at_millis,
                mine: false,
                // The runtime wrote this, whichever brain produced it.
                by_person: false,
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
                    admin_only: false,
                    author,
                    text,
                    at_millis,
                    mine,
                    // A person typed this — the one arm where that is true.
                    by_person: true,
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
                author: crate::ports::SYSTEM_AUTHOR.to_string(),
                text: dispatch_marker_text(&column),
                at_millis,
                mine: false,
                by_person: false,
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
                author: crate::ports::SYSTEM_AUTHOR.to_string(),
                text: format!("{other:?}"),
                at_millis,
                mine: false,
                by_person: false,
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
    Ok(messages)
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
    /// The one line B-012 turned on: a `None` thread names no conversation, so
    /// it is raised in none — including the General desk, whose every spelling
    /// `same_conversation` otherwise folds a `None` into.
    #[test]
    fn nothing_was_raised_in_a_conversation_it_never_named() {
        for desk in ["General", "main", "general", "dm:eng", ""] {
            assert!(
                !super::raised_in(None, desk),
                "a blocker that named no conversation must not answer to {desk}"
            );
            // The contrast: `same_conversation` deliberately folds the same
            // `None` into General, which is right for an unaddressed *message*
            // and wrong for a park that named nobody.
            assert_eq!(
                super::same_conversation(None, Some(desk)),
                super::is_general_chat(Some(desk)),
                "{desk}"
            );
        }
    }

    /// And a thread that *was* named still matches its desk, through every
    /// spelling of the General one.
    #[test]
    fn a_named_thread_still_matches_its_desk() {
        assert!(super::raised_in(Some("General"), "main"));
        assert!(super::raised_in(Some("main"), "General"));
        assert!(super::raised_in(Some("dm:eng"), "dm:eng"));
        assert!(!super::raised_in(Some("dm:eng"), "dm:ceo"));
        assert!(!super::raised_in(Some("dm:eng"), "General"));
    }

    use super::*;
    use crate::ports::tasks::{
        COLUMN_DONE, COLUMN_IN_PROGRESS, COLUMN_IN_REVIEW, COLUMN_PAUSED, COLUMN_PLANNING,
        COLUMN_TODO,
    };
    use crate::ports::types::Actor;

    fn agent_reply(chat_id: &str) -> CompanyEvent {
        CompanyEvent::AgentReply {
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
