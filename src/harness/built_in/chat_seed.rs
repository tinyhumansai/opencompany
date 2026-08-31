//! Recent-chat history seed for a resumed agent turn (issue #1840).
//!
//! # Why this exists
//!
//! One `Agent` is reused for every chat of a `(company, agent_id)` pair, and its
//! in-memory `history` is cleared and re-bound on every chat switch
//! ([`super::CompanyAgent::run_with_steer`]). The re-seed there originally went
//! through OpenHuman's `seed_resume_from_thread_transcript`, which reads a
//! per-thread OpenHuman **file** transcript — a file OpenCompany never writes for
//! a `chat_id` (its web-channel session is built with `.auto_save(false)` and no
//! thread binding). So the lookup always missed, the agent started every chat
//! reply at `history_len = 0`, and the model answered without the recent
//! conversation in front of it (the #1725/#1730 regression).
//!
//! OpenCompany already holds the authoritative transcript: the company
//! [`EventLog`]. This module projects the last `window` messages **that belong to
//! the incoming desk** out of that log into the lossy `(role, content)` shape
//! [`Agent::seed_resume_from_messages`](openhuman_core::openhuman::agent::Agent::seed_resume_from_messages)
//! accepts, so the switch branch can seed the correct thread's own recent turns
//! directly instead of chasing a file that isn't there.
//!
//! # Isolation
//!
//! The ownership test is [`chat_history::owns`] — the *same* predicate the
//! console's history surfaces use, so a seed contains exactly the lines the UI
//! renders for that desk and nothing from any other. That reuse is deliberate:
//! it gives DM (`dm:<id>`) vs named-desk parity for free and keeps a switch from
//! ever leaking the previous chat's lines into the next one.
//!
//! **A desk is not the finest conversation there is** (#1890). Threads are
//! persisted — a message carries the `parent` of the line it answers — but
//! `owns` matches on chat id alone, so every live thread in a channel was
//! projected into one flat window and the model answering inside one thread
//! read the others as though they were its own recent turns. The seed's shape
//! is what made that undetectable: bare `(role, content)` pairs, with no
//! author, sequence or parent, so a sibling thread's turn is indistinguishable
//! from this thread's own. [`in_thread`] narrows `owns` by the parent pointer;
//! the console's one-level fold is the whole definition of membership.

use std::sync::Arc;

use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, StoredEvent};
use crate::ports::{CompanyStore, EventLog};
use crate::server::chat_history;
use crate::server::ops::language::DEFAULT_DESK;

/// What a chat turn needs to build its recent-history seed, carried into
/// [`super::CompanyAgent::run_with_steer`] rather than an already-projected
/// `Vec`.
///
/// The projection itself ([`build_chat_seed`]) is only done inside
/// `run_with_steer`, after the chat-switch decision (under the same
/// `bound_chat` lock) confirms a re-seed is actually needed — never for a
/// turn that keeps the same bound chat as the one before it. Handing the
/// caller-built `Vec` in unconditionally meant the (filesystem-backend-costly
/// — see [`build_chat_seed`]'s docs) journal walk ran on *every* chat turn,
/// switch or not, since the caller has no way to see the switch decision
/// before it (codex review finding).
///
/// `None` for every non-chat turn — background, workflow, or [confined]
/// (`confine::run_confined`) — which want no seed regardless of switch
/// status, exactly like passing an empty seed did before this type existed.
///
/// [confined]: super::confine
pub struct ChatSeedRequest {
    /// The turn's raw, pre-memory-injection text — what
    /// [`strip_current_message`] matches against. Deliberately NOT
    /// `run_with_steer`'s own `message` argument: that one is the
    /// memory-augmented turn text, which the journal never recorded (see
    /// `strip_current_message`'s docs).
    pub raw_message: String,
    /// The company journal [`build_chat_seed`] projects the seed from.
    pub events: Arc<dyn EventLog>,
    /// Resolves the incoming chat id to its desk id/name pair (see
    /// [`resolve_seed_desk`]).
    pub store: Arc<dyn CompanyStore>,
    /// The thread this turn belongs to, as its root message's sequence
    /// position — `None` for a turn posted straight into the channel.
    ///
    /// Carried from the call site rather than recovered here by re-reading the
    /// turn's own journaled message for its `parent`. The route already knows
    /// it (it parsed the operator's `parent` and journaled the message with
    /// it), and rediscovering a fact the caller holds is the ambient-context
    /// coupling this crate keeps getting bitten by — it would also cost a
    /// journal read on the *non*-switch turns the switch branch exists to keep
    /// free.
    pub thread_root: Option<EventSeq>,
}

impl ChatSeedRequest {
    /// Projects this desk's recent history — bounded at this turn's own
    /// message so a concurrently-accepted later message never leaks in (see
    /// [`build_chat_seed`]) — and strips the current message's own trailing
    /// duplicate, in one call: the two steps
    /// [`super::CompanyAgent::run_with_steer`]'s switch branch needs, together.
    pub async fn build(&self, company: &CompanyId, chat_id: &str) -> Vec<(String, String)> {
        let (desk_id, desk_name) = resolve_seed_desk(&self.store, company, Some(chat_id)).await;
        let mut seed = build_chat_seed(
            &self.events,
            company,
            &desk_id,
            &desk_name,
            self.thread_root,
            CHAT_SEED_WINDOW,
            &self.raw_message,
        )
        .await;
        strip_current_message(&mut seed, &self.raw_message);
        seed
    }
}

/// How many of the most-recent owning messages a chat seed carries.
///
/// A conversational window, not the whole transcript: enough that a reply lands
/// in the flow of the recent exchange, small enough that a resumed turn does not
/// re-send an unbounded history on every switch. OpenHuman's own
/// `max_history_messages` bound still applies on top of this (see
/// `bound_cached_transcript_messages`), so this is an upper request, not a
/// guarantee.
pub const CHAT_SEED_WINDOW: usize = 30;

/// How many raw journal events to pull per backward page while filtering down to
/// owning messages. A busy company interleaves unrelated events between two chat
/// turns, so the event page is larger than the message window — mirrors
/// [`chat_history::history_for_desk`]'s `EVENT_PAGE`.
const EVENT_PAGE: usize = 512;

/// Resolves an incoming `chat_id` to the `(desk_id, desk_name)` pair
/// [`chat_history::owns`] filters on, exactly as the REST history route's
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
///   spelling folds together in [`chat_history::same_conversation`], so no
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
    let Some(desk) = chat_id else {
        return (DEFAULT_DESK.to_string(), DEFAULT_DESK.to_string());
    };
    if chat_history::is_general_chat(Some(desk)) {
        return (desk.to_string(), desk.to_string());
    }
    match store.load(company).await {
        Ok(Some(record)) => record
            .manifest
            .group_chats
            .iter()
            .find(|chat| chat.id.eq_ignore_ascii_case(desk) || chat.name.eq_ignore_ascii_case(desk))
            .map(|chat| (chat.id.clone(), chat.name.clone()))
            .unwrap_or_else(|| (desk.to_string(), desk.to_string())),
        // A store miss or read error must not fail the turn — fall back to the
        // verbatim selector, which still owns everything journaled under that
        // exact string (the common case, where the console addresses id == name).
        Ok(None) | Err(_) => (desk.to_string(), desk.to_string()),
    }
}

/// Does this owned event belong to `thread_root`'s conversation?
///
/// A thread is "the messages pointing at this one" (`OperatorMessage::parent`'s
/// own docs) — there is no thread object to consult, so membership is decided
/// from the parent pointer and nothing else.
///
/// * `None` — the channel-level conversation: only unparented lines. This is
///   every message in a company that has never opened a thread, so an
///   unthreaded channel seeds exactly what it seeded before this filter
///   existed.
/// * `Some(root)` — the root message itself, plus everything parented to it.
///   One level deep, because that is all the console renders and all
///   `AgentReply::parent` can express: a reply is parented to *its question's*
///   parent, never to the question, precisely so a thread cannot nest.
///
/// Anything that is not a chat message answers `false`. The only such event
/// `owns` admits is a `DeskTaskCompleted` terminal, which the mapper drops
/// anyway for want of a conversational body — but it is dropped here first,
/// and deliberately: a card records `origin_chat_id` and no thread root, so
/// there is currently no honest answer to which thread it belongs to. See the
/// `origin_parent` sub-issue on #1890.
fn in_thread(stored: &StoredEvent, thread_root: Option<EventSeq>) -> bool {
    let parent = match &stored.event {
        CompanyEvent::OperatorMessage { parent, .. } => *parent,
        CompanyEvent::AgentReply { parent, .. } => *parent,
        _ => return false,
    };
    match thread_root {
        None => parent.is_none(),
        Some(root) => stored.seq == root || parent == Some(root),
    }
}

/// Projects the last `window` messages owned by `(desk_id, desk_name)` out of the
/// company [`EventLog`] into chronological `(role, content)` pairs for
/// [`Agent::seed_resume_from_messages`](openhuman_core::openhuman::agent::Agent::seed_resume_from_messages).
///
/// Walks the log newest-first (`read_before`), keeps only the events
/// [`chat_history::owns`] admits for this desk **and [`in_thread`] admits for
/// `thread_root`**, maps each to a role
/// (`OperatorMessage` → `user`, `AgentReply` → `agent`), stops once `window`
/// messages are gathered, and reverses to chronological order. Non-conversational
/// owned events (a settled-dispatch terminal, reactions, anything without body
/// text) are skipped even when `owns` admits them — a seed needs role + text, not
/// structural markers.
///
/// `thread_root` scopes the projection to one conversation within the desk:
/// `None` is the channel itself (unparented lines only — every message in a
/// company that has never threaded, so an unthreaded desk projects exactly what
/// it did before), and `Some(root)` is that root plus its replies. It is
/// applied **before** the `current_message` boundary below, which matters more
/// than it looks: the boundary is a text prefix compare, so without the thread
/// filter a sibling thread carrying the same words would match first and cut
/// the window at a message this turn never sent.
///
/// An `OperatorMessage` with attachments is composed through the same
/// [`with_attachment_refs`](crate::brain::medulla::effects::with_attachment_refs)
/// formatter the live turn path uses, not just its raw `text` — otherwise a
/// resumed turn's history quietly dropped every attachment a *prior* message
/// carried, even though the current turn's own attachments always reach the
/// model (codex review finding). This also means a seeded entry that turns out
/// to be the current turn's own duplicate is composed identically to
/// `ChatSeedRequest::raw_message`, so [`strip_current_message`] still matches
/// it exactly.
///
/// `current_message` bounds the scan at THIS turn's own operator message
/// (codex review finding on #1842): the chat route journals a message the
/// instant it is accepted, before it queues on the per-company cycle lock, so
/// two messages for the same desk accepted close together are both already in
/// the journal by the time either turn actually reads it. Scanning from the
/// unbounded tail let the earlier turn seed the later message as "prior
/// history" — and because the later message's text never matches the earlier
/// turn's `current_message`, [`strip_current_message`] cannot remove it
/// either, so the model saw a request nobody made it plus its own current one
/// twice over. The newest-first walk below therefore buffers every owning
/// turn into `pending` until it matches a `("user", _)` entry whose text is a
/// prefix-match of `current_message` (the same relationship
/// [`strip_current_message`] uses, since an attachment makes `current_message`
/// a superstring of the journaled text) — that match is this turn's own
/// boundary, so `pending` (everything newer) is discarded and only the log
/// content strictly at-or-before it is collected as history.
///
/// A boundary that is never matched — `current_message` empty, or a caller
/// with no real current turn to bound against (tests, chiefly) — degrades to
/// the unbounded-tail behaviour from before this bound existed: `pending`
/// (capped at `window` throughout, so this costs nothing extra) becomes the
/// answer. The bound is a tightening over that baseline, never a new way for
/// the seed to come back emptier than it did before. The search itself is
/// capped at a fixed raw-event budget so a boundary that is
/// genuinely never found cannot walk the whole company history — in
/// production the match is expected within the first page, since the message
/// was just journaled moments before this projection runs.
///
/// Best-effort: a read error yields an empty seed (the caller then falls back to
/// the OpenHuman transcript lookup) rather than failing the turn.
pub async fn build_chat_seed(
    events: &Arc<dyn EventLog>,
    company: &CompanyId,
    desk_id: &str,
    desk_name: &str,
    thread_root: Option<EventSeq>,
    window: usize,
    current_message: &str,
) -> Vec<(String, String)> {
    /// Safety valve on the self-boundary search: past this many raw journal
    /// events with no match, give up looking and fall back to the
    /// unbounded-tail behaviour rather than walking the entire company
    /// history for a boundary that may simply not exist in this desk's log.
    const SELF_SEARCH_BUDGET: usize = EVENT_PAGE * 4;

    if window == 0 {
        return Vec::new();
    }

    let current_message = current_message.trim();

    // Newest-first accumulation; reversed to chronological before returning.
    // `pending` holds owning turns seen before the boundary above is matched;
    // `collected` holds turns at-or-before it. Exactly one of the two ends up
    // as the answer — see the boundary discussion above.
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut collected: Vec<(String, String)> = Vec::with_capacity(window);
    let mut found_self = false;
    // Set once the requested root has been collected. Nothing older can belong
    // to the thread — a child always sequences after the message it answers —
    // so the backward walk is finished the moment the root is in hand. Without
    // it a short thread never fills `window`, and because the current message
    // sets `found_self` on the first page the `SELF_SEARCH_BUDGET` guard below
    // is already disabled: every rebind into a 3-message thread walked the
    // entire company journal (codex review finding).
    let mut reached_root = false;
    let mut scanned_raw: usize = 0;
    let mut cursor = None;

    loop {
        if reached_root || (found_self && collected.len() >= window) {
            break;
        }
        // The budget bounds the WHOLE walk, not only the pre-boundary search.
        // A conversation sparser than `window` — a short thread, or a channel
        // whose recent traffic is mostly threaded and therefore not its own —
        // can never satisfy the `collected.len() >= window` exit, so without
        // this it reads to the head of the log. A seed is a recent window;
        // returning a shorter one is a degradation, reading the whole journal
        // on every rebind is a defect.
        if scanned_raw >= SELF_SEARCH_BUDGET {
            break;
        }
        let page = match events.read_before(company, cursor, EVENT_PAGE).await {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(
                    company = %company,
                    desk = desk_id,
                    %error,
                    "[chat-seed] event-log read failed; seeding no recent history (falling back to transcript lookup)"
                );
                return Vec::new();
            }
        };
        if page.is_empty() {
            break;
        }
        scanned_raw += page.len();
        cursor = page.last().map(|event| event.seq);
        for stored in page {
            if !chat_history::owns(desk_id, desk_name, &stored.event) {
                continue;
            }
            // Thread scoping, applied BEFORE the boundary match below so the
            // self-search only ever sees this thread's own messages. The
            // boundary is a text prefix compare, so a sibling thread carrying
            // the same words ("make it shorter") would otherwise match first
            // and cut the window at the wrong message.
            if !in_thread(&stored, thread_root) {
                continue;
            }
            let mapped = match &stored.event {
                CompanyEvent::OperatorMessage {
                    text, attachments, ..
                } => Some((
                    "user",
                    crate::brain::medulla::effects::with_attachment_refs(text, attachments),
                )),
                CompanyEvent::AgentReply { text, .. } => Some(("agent", text.clone())),
                // `owns` also admits `DeskTaskCompleted` (a structural "finished →
                // In review" marker), but it carries no conversational body — do
                // not seed it as a turn.
                _ => None,
            };
            let Some((role, text)) = mapped else { continue };
            if text.trim().is_empty() {
                continue;
            }

            // The root is the oldest event this thread can hold, whichever
            // accumulator it lands in.
            let is_root = thread_root == Some(stored.seq);

            if !found_self {
                if role == "user"
                    && !current_message.is_empty()
                    && current_message.starts_with(text.trim())
                {
                    found_self = true;
                    collected.push((role.to_string(), text));
                    if is_root {
                        reached_root = true;
                        break;
                    }
                } else {
                    pending.push((role.to_string(), text));
                    if pending.len() > window {
                        pending.truncate(window);
                    }
                    if is_root {
                        reached_root = true;
                        break;
                    }
                }
                continue;
            }

            collected.push((role.to_string(), text));
            if is_root {
                reached_root = true;
                break;
            }
            if collected.len() == window {
                break;
            }
        }
    }

    if !found_self {
        collected = pending;
        collected.truncate(window);
    }

    collected.reverse();
    collected
}

/// Drops a trailing `("user", …)` entry whose text matches `current_message`.
///
/// The operator's current message is journaled **before** the harness turn runs
/// (the server appends it, then the brain dispatches), so it is already the
/// newest owning event when [`build_chat_seed`] reads the tail. Seeding it as
/// prior history would duplicate it on the wire — `run_single` appends the
/// current message to `history` itself. OpenHuman's `seed_resume_from_messages`
/// performs the same drop, but it can only match against the *augmented* message
/// the runner passes it; the raw operator text is only in scope here, so strip it
/// here where the match is exact.
///
/// `current_message` must be the raw, pre-memory-injection turn text (what
/// [`ChatSeedRequest::raw_message`] carries) — a `starts_with`, not an exact
/// match, because an attachment turns it into a *prefix* of what the journal
/// holds. `HarnessBrain::cycle` composes the wire body operator text passes
/// through `with_attachment_refs(text, attachments)` before it ever reaches a
/// turn, appending `"\n\n[Attached file: …]"` markers after the operator's own
/// words; the journaled `OperatorMessage.text` this seed reads is only the raw
/// text, with no markers. An exact match therefore missed on any message with
/// an attachment, leaving the un-stripped duplicate in the seed — `run_single`
/// then appends the (augmented) current message again, so the model saw the
/// operator's current request twice (codex review finding). `with_attachment_refs`
/// only ever *appends* markers — the operator's text always leads the
/// composition unless the 200k-char wire cap truncated it — so a prefix match
/// catches the attachment case without needing the pre-augmentation text
/// plumbed any further than it already is here.
pub fn strip_current_message(seed: &mut Vec<(String, String)>, current_message: &str) {
    if let Some((role, text)) = seed.last()
        && role == "user"
        && !text.trim().is_empty()
        && current_message.trim().starts_with(text.trim())
    {
        seed.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};

    use crate::ports::events::EventStreamItem;
    use crate::ports::types::{EventSeq, StoredEvent};

    /// A log that replays a fixed history in ascending sequence order. The
    /// trait's default `read_before` (forward-read + reverse + truncate) then
    /// gives us newest-first paging for free — exactly what production backends
    /// override but what a fixture does not need to.
    struct FixedLog(Vec<StoredEvent>);

    #[async_trait]
    impl EventLog for FixedLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("the seed projector only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Ok(self
                .0
                .iter()
                .filter(|e| e.seq.value() >= seq.value())
                .take(limit)
                .cloned()
                .collect())
        }
        fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// A [`FixedLog`] that counts how many PAGES the projector pulled, so a scan
    /// bound is observable rather than merely asserted. Pages, not events: the
    /// trait's default `read_before` reads forward and reverses, so an event
    /// count says more about the fixture than about the walk.
    #[derive(Default)]
    struct CountingLog {
        events: Vec<StoredEvent>,
        scanned: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl EventLog for CountingLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!("the seed projector only reads")
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            seq: EventSeq,
            limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            self.scanned
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .events
                .iter()
                .filter(|e| e.seq.value() >= seq.value())
                .take(limit)
                .cloned()
                .collect())
        }
        fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// A log whose reads always fail, to prove the projector degrades to an empty
    /// seed rather than propagating.
    struct BrokenLog;

    #[async_trait]
    impl EventLog for BrokenLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> crate::Result<EventSeq> {
            unreachable!()
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> crate::Result<Vec<StoredEvent>> {
            Err(OpenCompanyError::InvalidRequest("boom".to_string()))
        }
        fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    use crate::error::OpenCompanyError;

    fn operator(seq: u64, chat: Option<&str>, text: &str) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::OperatorMessage {
                text: text.to_string(),
                by: None,
                chat: chat.map(str::to_string),
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
            },
            at_millis: seq,
        }
    }

    fn operator_with_attachment(
        seq: u64,
        chat: Option<&str>,
        text: &str,
        attachment: crate::ports::types::Attachment,
    ) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::OperatorMessage {
                text: text.to_string(),
                by: None,
                chat: chat.map(str::to_string),
                parent: None,
                deliverable: None,
                mentions: Vec::new(),
                attachments: vec![attachment],
            },
            at_millis: seq,
        }
    }

    fn reply(seq: u64, chat_id: &str, text: &str) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::AgentReply {
                chat_id: chat_id.to_string(),
                agent_id: "ceo".to_string(),
                text: text.to_string(),
                steps: Vec::new(),
                task_id: None,
                parent: None,
                mentions: Vec::new(),
                mention_depth: 0,
            },
            at_millis: seq,
        }
    }

    fn desk_completed(seq: u64, origin_chat_id: Option<&str>) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            company: CompanyId::new("acme"),
            event: CompanyEvent::DeskTaskCompleted {
                task_id: "t-1".to_string(),
                desk: "eng".to_string(),
                output: "shipped".to_string(),
                column: "done".to_string(),
                artifact_ids: Vec::new(),
                origin_chat_id: origin_chat_id.map(str::to_string),
            },
            at_millis: seq,
        }
    }

    /// `current_message` empty means "no boundary to bound against" — the
    /// tests exercising desk ownership, folding, and window truncation below
    /// pass `""` on purpose, so they see the unbounded-tail fallback
    /// [`build_chat_seed`]'s doc describes and are unaffected by the
    /// self-boundary search.
    async fn seed_of(
        log: FixedLog,
        desk_id: &str,
        desk_name: &str,
        window: usize,
        current_message: &str,
    ) -> Vec<(String, String)> {
        let events: Arc<dyn EventLog> = Arc::new(log);
        build_chat_seed(
            &events,
            &CompanyId::new("acme"),
            desk_id,
            desk_name,
            // The channel-level conversation. Every fixture below journals
            // `parent: None`, which is what an unthreaded company writes — so
            // these cases assert the pre-#1890 behaviour is byte-identical.
            None,
            window,
            current_message,
        )
        .await
    }

    /// The core projection: a journal interleaving the General desk's own
    /// operator/agent turns with an unrelated desk's message, a structural
    /// dispatch terminal, and an empty reply. Only the General desk's real
    /// conversational turns survive, in chronological order, with the right roles.
    #[tokio::test]
    async fn projects_only_owning_conversational_turns_in_order() {
        let log = FixedLog(vec![
            operator(0, Some("general"), "u1"),
            reply(1, "general", "a1"),
            // Another desk entirely — must never appear in General's seed.
            operator(2, Some("engineering"), "OTHER-DESK"),
            reply(3, "engineering", "OTHER-REPLY"),
            // `owns` admits this (origin is General) but it is a structural
            // marker, not a turn — the projector must skip it.
            desk_completed(4, Some("general")),
            // A blank reply carries no body to seed.
            reply(5, "general", "   "),
            operator(6, Some("general"), "u2"),
        ]);

        let seed = seed_of(log, "general", "general", CHAT_SEED_WINDOW, "").await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "u1".to_string()),
                ("agent".to_string(), "a1".to_string()),
                ("user".to_string(), "u2".to_string()),
            ],
            "only General's own operator/agent turns, chronological, correctly roled"
        );
    }

    /// Codex review finding: a prior operator message's attachment must survive
    /// into the seed, not just its raw text — otherwise a follow-up like
    /// "summarize that file again" loses the file context on a resumed turn,
    /// even though the SAME message's attachment reached the model fine the
    /// first time it was live (via `with_attachment_refs` on the current-turn
    /// path). The seed must go through the identical formatter.
    #[tokio::test]
    async fn a_prior_message_with_an_attachment_keeps_its_attachment_marker_in_the_seed() {
        let attachment = crate::ports::types::Attachment {
            node_id: "node-1".to_string(),
            name: "report.pdf".to_string(),
            mime: "application/pdf".to_string(),
            size: 1234,
            extracted_text: Some("QUARTERLY_REPORT_MARKER".to_string()),
        };
        let log = FixedLog(vec![operator_with_attachment(
            0,
            Some("general"),
            "please review this",
            attachment,
        )]);

        let seed = seed_of(log, "general", "general", CHAT_SEED_WINDOW, "").await;

        assert_eq!(seed.len(), 1, "the one owning message is seeded");
        let (role, text) = &seed[0];
        assert_eq!(role, "user");
        assert!(
            text.starts_with("please review this"),
            "the operator's own words still lead: {text:?}"
        );
        assert!(
            text.contains("QUARTERLY_REPORT_MARKER"),
            "the attachment's extracted text must reach a resumed turn's \
             context, exactly like it reaches a live one: {text:?}"
        );
    }

    /// The General desk answers to every spelling of itself, so a reply journaled
    /// under `"General"` and a `"main"` operator line both land in the seed for a
    /// desk addressed as `"main"` — the folding `owns`/`same_conversation` give.
    #[tokio::test]
    async fn general_desk_folds_its_spellings() {
        let log = FixedLog(vec![
            operator(0, None, "unaddressed"),
            reply(1, "General", "under-General"),
            operator(2, Some("main"), "under-main"),
        ]);

        let seed = seed_of(log, "main", "main", CHAT_SEED_WINDOW, "").await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "unaddressed".to_string()),
                ("agent".to_string(), "under-General".to_string()),
                ("user".to_string(), "under-main".to_string()),
            ],
        );
    }

    /// DM parity: a `dm:<id>` thread is an opaque verbatim key, so its own turns
    /// project and a sibling DM's do not.
    #[tokio::test]
    async fn dm_thread_projects_and_isolates() {
        let log = FixedLog(vec![
            operator(0, Some("dm:alice"), "hi alice"),
            reply(1, "dm:alice", "hi back"),
            operator(2, Some("dm:bob"), "hi bob"),
        ]);

        let seed = seed_of(log, "dm:alice", "dm:alice", CHAT_SEED_WINDOW, "").await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "hi alice".to_string()),
                ("agent".to_string(), "hi back".to_string()),
            ],
            "only the addressed DM's own turns, never the sibling DM's"
        );
    }

    /// A named desk's turns can be journaled under either its id or its name;
    /// `owns` matches both, so passing the resolved `(id, name)` pair seeds every
    /// line regardless of which spelling wrote it.
    #[tokio::test]
    async fn named_desk_matches_id_and_name() {
        let log = FixedLog(vec![
            operator(0, Some("eng-123"), "by id"),
            reply(1, "Engineering", "by name"),
            operator(2, Some("marketing"), "OTHER"),
        ]);

        let seed = seed_of(log, "eng-123", "Engineering", CHAT_SEED_WINDOW, "").await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "by id".to_string()),
                ("agent".to_string(), "by name".to_string()),
            ],
        );
    }

    /// The window keeps the most-recent `window` owning turns and drops older
    /// ones, even when unrelated events sit between them.
    #[tokio::test]
    async fn window_keeps_the_most_recent_turns() {
        let mut events = Vec::new();
        for n in 0..10u64 {
            events.push(operator(n, Some("general"), &format!("m{n}")));
        }
        let log = FixedLog(events);

        let seed = seed_of(log, "general", "general", 3, "").await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "m7".to_string()),
                ("user".to_string(), "m8".to_string()),
                ("user".to_string(), "m9".to_string()),
            ],
            "the three newest owning turns, in chronological order"
        );
    }

    /// Codex review finding (P1): the chat route journals an operator message
    /// the instant it is accepted, before it queues on the per-company cycle
    /// lock — so two messages for the same desk accepted close together are
    /// both already in the journal by the time either turn's seed projection
    /// actually runs. Scanning the unbounded tail let the FIRST message's turn
    /// seed the SECOND message too, as if it were prior history — and because
    /// the second message's text never matches the first turn's own text,
    /// `strip_current_message` cannot remove it either. The seed for "my
    /// message"'s turn must stop at its own boundary: everything journaled
    /// after it is excluded, not just everything after the log's current tail.
    #[tokio::test]
    async fn a_concurrently_journaled_later_message_is_excluded_from_the_seed() {
        let log = FixedLog(vec![
            operator(0, Some("general"), "earlier turn"),
            reply(1, "general", "earlier reply"),
            operator(2, Some("general"), "my message"),
            // Accepted by the chat route microseconds later, before either
            // turn won this desk's per-company cycle lock — same shape as two
            // browser tabs firing at once.
            operator(3, Some("general"), "a second, concurrent message"),
        ]);

        let seed = seed_of(log, "general", "general", CHAT_SEED_WINDOW, "my message").await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "earlier turn".to_string()),
                ("agent".to_string(), "earlier reply".to_string()),
                ("user".to_string(), "my message".to_string()),
            ],
            "the later, concurrently-journaled message must not appear as \
             prior history for the earlier message's own turn: {seed:?}"
        );
    }

    /// The self-boundary in [`build_chat_seed`] degrades to the unbounded-tail
    /// behaviour when it is never matched — a message with no owning entry in
    /// this desk's log at all — rather than silently emptying the seed. This
    /// is the fallback path every other test in this module exercises via
    /// `seed_of`'s `current_message: ""`; this test names it explicitly with a
    /// non-empty, non-matching message so the fallback is proven on its own
    /// terms rather than only incidentally through the empty-string case.
    #[tokio::test]
    async fn an_unmatched_boundary_falls_back_to_the_unbounded_tail() {
        let log = FixedLog(vec![
            operator(0, Some("general"), "u1"),
            reply(1, "general", "a1"),
        ]);

        let seed = seed_of(
            log,
            "general",
            "general",
            CHAT_SEED_WINDOW,
            "no journaled message matches this text",
        )
        .await;

        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "u1".to_string()),
                ("agent".to_string(), "a1".to_string()),
            ],
            "an unmatched boundary must not come back emptier than the \
             unbounded scan did: {seed:?}"
        );
    }

    // ── Thread scoping (#1890) ───────────────────────────────────────────

    /// An operator message posted inside the thread rooted at `parent`.
    fn operator_in(seq: u64, chat: Option<&str>, text: &str, parent: u64) -> StoredEvent {
        let mut stored = operator(seq, chat, text);
        if let CompanyEvent::OperatorMessage { parent: p, .. } = &mut stored.event {
            *p = Some(EventSeq::new(parent));
        }
        stored
    }

    /// An agent reply journaled under the thread rooted at `parent` — the
    /// message's OWN parent, never the message itself, which is what stops a
    /// thread nesting inside a thread.
    fn reply_in(seq: u64, chat_id: &str, text: &str, parent: u64) -> StoredEvent {
        let mut stored = reply(seq, chat_id, text);
        if let CompanyEvent::AgentReply { parent: p, .. } = &mut stored.event {
            *p = Some(EventSeq::new(parent));
        }
        stored
    }

    async fn seed_of_thread(
        log: FixedLog,
        desk: &str,
        thread_root: Option<u64>,
        current_message: &str,
    ) -> Vec<(String, String)> {
        let events: Arc<dyn EventLog> = Arc::new(log);
        build_chat_seed(
            &events,
            &CompanyId::new("acme"),
            desk,
            desk,
            thread_root.map(EventSeq::new),
            CHAT_SEED_WINDOW,
            current_message,
        )
        .await
    }

    /// Two live threads in ONE channel. The turn answering inside thread A must
    /// see thread A's exchange and nothing of thread B's — the leak #1890 opens
    /// with, where "make it shorter" arrived directly after an unrelated CAC
    /// answer because the projection was scoped to the channel.
    #[tokio::test]
    async fn a_thread_sees_only_its_own_exchange() {
        let log = FixedLog(vec![
            operator(41, Some("growth"), "draft the launch email"), // root A
            reply_in(42, "growth", "here is a draft", 41),
            operator(43, Some("growth"), "what's our Q3 CAC?"), // root B
            reply_in(44, "growth", "$412, up 18%", 43),
            operator_in(45, Some("growth"), "make it shorter", 41),
        ]);
        let seed = seed_of_thread(log, "growth", Some(41), "make it shorter").await;
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "draft the launch email".to_string()),
                ("agent".to_string(), "here is a draft".to_string()),
                ("user".to_string(), "make it shorter".to_string()),
            ],
            "thread A's seed must not carry thread B's turns: {seed:?}"
        );
    }

    /// The channel-level conversation is the `None` thread: unparented lines
    /// only. A thread's body belongs to the thread, not to the channel that
    /// hosts it.
    #[tokio::test]
    async fn the_channel_sees_only_unparented_lines() {
        let log = FixedLog(vec![
            operator(41, Some("growth"), "draft the launch email"),
            reply_in(42, "growth", "THREAD-BODY", 41),
            operator_in(43, Some("growth"), "THREAD-FOLLOWUP", 41),
            operator(44, Some("growth"), "unrelated channel line"),
        ]);
        let seed = seed_of_thread(log, "growth", None, "").await;
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "draft the launch email".to_string()),
                ("user".to_string(), "unrelated channel line".to_string()),
            ],
            "the channel must not inherit a thread's body: {seed:?}"
        );
    }

    /// The root message is part of its own thread — a thread opened on a
    /// question must seed the question, or the first reply inside it answers
    /// against nothing.
    #[tokio::test]
    async fn a_thread_includes_its_root() {
        let log = FixedLog(vec![
            operator(7, Some("growth"), "the question"),
            operator_in(8, Some("growth"), "the follow-up", 7),
        ]);
        let seed = seed_of_thread(log, "growth", Some(7), "the follow-up").await;
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "the question".to_string()),
                ("user".to_string(), "the follow-up".to_string()),
            ]
        );
    }

    /// The self-boundary is a TEXT prefix compare, so a sibling thread carrying
    /// the same words would match first and cut the window at a message this
    /// turn never sent — dropping this thread's own history. Scoping to the
    /// thread before the boundary search is what makes the match unambiguous.
    #[tokio::test]
    async fn a_siblings_identical_wording_does_not_cut_the_window() {
        let log = FixedLog(vec![
            operator(1, Some("growth"), "root A"),
            reply_in(2, "growth", "A's answer", 1),
            operator(3, Some("growth"), "root B"),
            // Thread B says the very same words, and is NEWER, so a
            // channel-flat backward scan meets it first.
            operator_in(4, Some("growth"), "make it shorter", 3),
            reply_in(5, "growth", "B's shortened text", 3),
            operator_in(6, Some("growth"), "make it shorter", 1),
        ]);
        let seed = seed_of_thread(log, "growth", Some(1), "make it shorter").await;
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "root A".to_string()),
                ("agent".to_string(), "A's answer".to_string()),
                ("user".to_string(), "make it shorter".to_string()),
            ],
            "the boundary must be this thread's own message, not the sibling's: {seed:?}"
        );
    }

    /// A short thread must not walk the whole company journal to seed itself.
    ///
    /// The regression this pins: the current message is the newest event, so
    /// `found_self` is set on the first page and the pre-boundary budget stops
    /// applying — and a 3-message thread can never reach `CHAT_SEED_WINDOW`, so
    /// the `collected.len() >= window` exit is unreachable too. With no bound
    /// left, every rebind kept paging backwards through years of older channel
    /// history that could not possibly belong to the thread (codex review
    /// finding). The root is the oldest event the thread can hold, so the walk
    /// is finished the moment it is in hand.
    ///
    /// The history is deliberately OLDER than the thread: a newest-first walk
    /// meets the thread immediately and everything behind it is the waste.
    #[tokio::test]
    async fn a_short_thread_stops_scanning_at_its_root() {
        // ~4 pages of unrelated channel history, then the thread on top.
        const OLD: u64 = 2100;
        let mut events: Vec<StoredEvent> = (0..OLD)
            .map(|seq| operator(seq, Some("growth"), &format!("old line {seq}")))
            .collect();
        events.push(operator(OLD, Some("growth"), "root"));
        events.push(reply_in(OLD + 1, "growth", "an answer", OLD));
        events.push(operator_in(OLD + 2, Some("growth"), "follow-up", OLD));

        let log = Arc::new(CountingLog {
            events,
            scanned: Default::default(),
        });
        let events: Arc<dyn EventLog> = log.clone();
        let seed = build_chat_seed(
            &events,
            &CompanyId::new("acme"),
            "growth",
            "growth",
            Some(EventSeq::new(OLD)),
            CHAT_SEED_WINDOW,
            "follow-up",
        )
        .await;
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "root".to_string()),
                ("agent".to_string(), "an answer".to_string()),
                ("user".to_string(), "follow-up".to_string()),
            ],
            "the thread's own turns, whole: {seed:?}"
        );
        let pages = log.scanned.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            pages, 1,
            "the root is on the first newest-first page, so the walk is done \
             there — it pulled {pages} pages"
        );
    }

    /// A dispatch terminal is skipped whatever thread is asked for. It carries
    /// no conversational body, and a card records `origin_chat_id` with no
    /// thread root — so there is no honest thread to attribute it to yet.
    #[tokio::test]
    async fn a_dispatch_terminal_is_in_no_thread() {
        let log = FixedLog(vec![
            operator(1, Some("growth"), "root"),
            desk_completed(2, Some("growth")),
            operator_in(3, Some("growth"), "follow-up", 1),
        ]);
        let seed = seed_of_thread(log, "growth", Some(1), "follow-up").await;
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "root".to_string()),
                ("user".to_string(), "follow-up".to_string()),
            ]
        );
    }

    /// A read failure yields an empty seed, never a propagated error — the caller
    /// then falls back to the OpenHuman transcript lookup.
    #[tokio::test]
    async fn read_failure_degrades_to_empty() {
        let events: Arc<dyn EventLog> = Arc::new(BrokenLog);
        let seed = build_chat_seed(
            &events,
            &CompanyId::new("acme"),
            "general",
            "general",
            None,
            CHAT_SEED_WINDOW,
            "",
        )
        .await;
        assert!(seed.is_empty());
    }

    #[test]
    fn strip_current_message_drops_only_a_matching_trailing_user() {
        let mut seed = vec![
            ("user".to_string(), "u1".to_string()),
            ("agent".to_string(), "a1".to_string()),
            ("user".to_string(), "  current  ".to_string()),
        ];
        strip_current_message(&mut seed, "current");
        assert_eq!(
            seed,
            vec![
                ("user".to_string(), "u1".to_string()),
                ("agent".to_string(), "a1".to_string()),
            ],
            "a trailing user line matching the current message (trim-insensitive) is dropped"
        );

        // A trailing agent line is never the current operator message.
        let mut ends_in_agent = vec![("agent".to_string(), "current".to_string())];
        strip_current_message(&mut ends_in_agent, "current");
        assert_eq!(ends_in_agent.len(), 1, "an agent tail is never stripped");

        // A non-matching trailing user line stays.
        let mut different = vec![("user".to_string(), "something else".to_string())];
        strip_current_message(&mut different, "current");
        assert_eq!(different.len(), 1, "a non-matching user tail stays");
    }

    /// Codex review finding: on a message with an attachment, `HarnessBrain`
    /// passes `with_attachment_refs(text, attachments)` — the raw text plus an
    /// appended `"\n\n[Attached file: …]"` marker — as the turn's message,
    /// while the journaled `OperatorMessage` (what the seed reads) carries
    /// only the raw text. An exact match therefore never drops the duplicate,
    /// so the operator's current request reached the model twice: once from
    /// the un-stripped seed tail, once as the augmented current message
    /// `run_single` appends itself. RED on the old `==` comparison, GREEN with
    /// the `starts_with` fix.
    #[test]
    fn strip_current_message_drops_a_trailing_user_line_augmented_with_an_attachment_marker() {
        let mut seed = vec![
            ("user".to_string(), "prior turn".to_string()),
            ("user".to_string(), "please review this doc".to_string()),
        ];
        let augmented_with_attachment =
            "please review this doc\n\n[Attached file: report.pdf]\nEXTRACTED TEXT";
        strip_current_message(&mut seed, augmented_with_attachment);
        assert_eq!(
            seed,
            vec![("user".to_string(), "prior turn".to_string())],
            "the raw journaled text is a prefix of the attachment-augmented \
             message, so the trailing duplicate must still be dropped"
        );
    }

    // ---- resolve_seed_desk ------------------------------------------------

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

    use crate::ports::types::{CompanyRecord, CompanySummary};

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

    #[tokio::test]
    async fn resolve_none_is_the_general_desk() {
        assert_eq!(
            resolve(RecordStore(None), None).await,
            (DEFAULT_DESK.to_string(), DEFAULT_DESK.to_string())
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
