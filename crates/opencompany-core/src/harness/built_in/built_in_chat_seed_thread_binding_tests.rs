//! `chat_seed_regression`, continued (issue #1840): the thread-binding half
//! of this coverage. Split out because the combined inline module exceeded
//! the 750-line file limit; see `built_in_chat_seed_seed_tests` for the rest
//! and the shared fixtures.

use super::*;

use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tinyinference::model::{ChatModel, ModelRequest, ModelResponse};

use super::built_in_test_fixtures::*;
use crate::ports::events::EventStreamItem;
use crate::ports::types::{CompanyEvent, EventSeq, StoredEvent};

/// An appendable in-memory journal. `read_from` returns ascending order,
/// so the trait's default `read_before` yields the newest-first paging the
/// seed projector walks.
///
/// `reads` counts every `read_from` call (the default `read_before`'s
/// only path into a backend) — a stand-in for the filesystem backend's
/// whole-file JSONL scan (`store::fs::read_before`'s docs), so a test
/// can assert the seed projector only walks the journal when a chat
/// switch actually needs it, not on every chat turn (codex review
/// finding).
#[derive(Default)]
struct InMemoryLog {
    events: StdMutex<Vec<StoredEvent>>,
    reads: std::sync::atomic::AtomicUsize,
}

impl InMemoryLog {
    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn operator(&self, chat: &str, text: &str) {
        self.push(CompanyEvent::OperatorMessage {
            text: text.to_string(),
            by: None,
            chat: Some(chat.to_string()),
            parent: None,
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        });
    }
    /// An operator message posted inside the thread rooted at `parent`.
    fn operator_in(&self, chat: &str, text: &str, parent: u64) {
        self.push(CompanyEvent::OperatorMessage {
            text: text.to_string(),
            by: None,
            chat: Some(chat.to_string()),
            parent: Some(EventSeq::new(parent)),
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        });
    }
    fn reply(&self, chat_id: &str, text: &str) {
        self.push(CompanyEvent::AgentReply {
            audience: Vec::new(),
            chat_id: chat_id.to_string(),
            agent_id: "ceo".to_string(),
            text: text.to_string(),
            steps: Vec::new(),
            task_id: None,
            outputs: Vec::new(),
            parent: None,
            mentions: Vec::new(),
            mention_depth: 0,
        });
    }
    /// A reply by `agent_id` posted inside the thread rooted at `parent`.
    fn reply_in(&self, chat_id: &str, agent_id: &str, text: &str, parent: u64) {
        self.push(CompanyEvent::AgentReply {
            audience: Vec::new(),
            chat_id: chat_id.to_string(),
            agent_id: agent_id.to_string(),
            text: text.to_string(),
            steps: Vec::new(),
            task_id: None,
            outputs: Vec::new(),
            parent: Some(EventSeq::new(parent)),
            mentions: Vec::new(),
            mention_depth: 0,
        });
    }
    fn push(&self, event: CompanyEvent) {
        let mut log = self.events.lock().unwrap();
        let seq = EventSeq::new(log.len() as u64);
        log.push(StoredEvent {
            seq,
            company: CompanyId::new("acme"),
            event,
            at_millis: seq.value(),
        });
    }
}

#[async_trait]
impl EventLog for InMemoryLog {
    async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> crate::Result<EventSeq> {
        let mut log = self.events.lock().unwrap();
        let seq = EventSeq::new(log.len() as u64);
        log.push(StoredEvent {
            seq,
            company: CompanyId::new("acme"),
            event,
            at_millis: seq.value(),
        });
        Ok(seq)
    }
    async fn read_from(
        &self,
        _id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> crate::Result<Vec<StoredEvent>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .events
            .lock()
            .unwrap()
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

/// A model that records the full text of every request it is handed, so a
/// test can assert which prior turns reached the model's context.
#[derive(Default)]
struct RecordingProvider {
    seen: Arc<StdMutex<Vec<String>>>,
}

#[async_trait]
impl ChatModel<()> for RecordingProvider {
    async fn invoke(
        &self,
        _state: &(),
        request: ModelRequest,
    ) -> tinyinference::Result<ModelResponse> {
        let joined = request
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        self.seen.lock().unwrap().push(joined);
        // A fixed non-empty reply: empty would trip the empty-response
        // retry wrapper into a second invoke.
        Ok(ModelResponse::assistant("ok"))
    }
}

impl HarnessModel for RecordingProvider {
    fn telemetry_provider_id(&self) -> String {
        "recording".to_string()
    }
}

/// A fixture whose journal and model are observable: the returned `log` is
/// pre-populated by the test, and `seen` collects every model request.
fn recording_fixture() -> (Fixture, Arc<InMemoryLog>, Arc<StdMutex<Vec<String>>>) {
    let mut fx = fixture();
    let log = Arc::new(InMemoryLog::default());
    let provider = Arc::new(RecordingProvider::default());
    let seen = provider.seen.clone();
    fx.deps.events = Some(log.clone());
    fx.deps.provider = provider;
    (fx, log, seen)
}

/// An UNADDRESSED threaded message still binds to its thread.
///
/// Issue #1890 I: an **unstreamed** turn still binds when its caller
/// names a conversation.
///
/// The approval re-dispatch is the case. It runs through
/// `run_steered_background` — no live stream, because a re-issued call
/// shows no chat bubble — and before this its identity was read off
/// that absent stream, so it bound to nothing: it ran against whatever
/// history the agent happened to be holding and then published its
/// answer into the origin thread regardless.
///
/// A dispatched card's turn is the other side of the same rule and must
/// keep binding to nothing, since it answers the board rather than a
/// conversation.
#[test]
fn identity_and_streaming_are_separate_questions() {
    use crate::runtime::delegation::ChatTarget;

    // What the approval re-dispatch now passes: the conversation the
    // grant recorded, with no stream at all.
    let reissued = ChatTarget::in_thread(Some("growth"), Some(EventSeq::new(41)));
    assert_eq!(reissued.chat_id, Some("growth"));
    assert_eq!(reissued.thread_root, Some(EventSeq::new(41)));

    // What a dispatched card's turn passes — unchanged behaviour.
    let card = ChatTarget::default();
    assert_eq!(card.chat_id, None);
    assert_eq!(card.thread_root, None);

    // The two are distinguishable, which is the whole of the fix: before
    // it, both arrived at the binding as "no stream, therefore no
    // conversation".
    assert_ne!(reissued, card);
}

/// A codex review on #1896 read `run_with_steer`'s `if let Some(incoming)
/// = turn_chat_id` guard and concluded that a client sending `parent`
/// without `chat` loses its root, so sibling threads on the default desk
/// keep sharing one history. That is not what happens: `turn_chat_id`
/// comes from the turn-stream route, which already falls back to
/// `DEFAULT_DESK` when no desk was addressed.
///
/// This test exists because the obvious "fix" — normalizing the id where
/// the target is built — is actively harmful: the same `chat_id` reaches
/// card creation and a card's `origin_chat_id`, where `None` means "no
/// conversation raised this card" and `chat_history::owns` deliberately
/// routes it to no desk. Pinning the real behaviour here is what stops
/// that being re-applied.
#[tokio::test]
async fn an_unaddressed_message_still_binds_to_its_thread() {
    let fx = fixture();
    let rec = record();
    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    // `chat_id: None` — the operator addressed no desk — with a root.
    pool.run(
        &rec.id,
        "ceo",
        "first",
        &fx.deps,
        crate::runtime::delegation::ChatTarget::in_thread(None, Some(EventSeq::new(1))),
    )
    .await
    .expect("unaddressed threaded turn");

    let agent = {
        let guard = pool.agents.read().await;
        guard
            .get(&rec.id)
            .and_then(|roster| roster.iter().find(|a| a.agent_id == "ceo"))
            .cloned()
            .expect("the agent stays resident")
    };
    assert_eq!(
        *agent.bound_chat.lock().await,
        Some((
            crate::server::ops::language::DEFAULT_DESK.to_string(),
            Some(EventSeq::new(1))
        )),
        "an unaddressed turn binds to the General desk AND keeps its \
         thread root — the stream route already supplies the fallback"
    );
}

/// The binding must not depend on the event log being wired
/// (coderabbit review finding).
///
/// `incoming_root` used to be read off `chat_seed`, which is `None`
/// whenever `deps.events` is — so on such a host two different threads
/// of one channel compared equal, the clear-and-re-seed never ran, and
/// the leak this change exists to close was reopened in exactly the
/// configuration that cannot re-seed its way out of it.
///
/// Asserted through `bound_chat` rather than through a seed, because a
/// host with no journal has no seed to inspect: the binding is the only
/// observable, and it is the thing that was wrong.
#[tokio::test]
async fn the_thread_binding_holds_with_no_event_log_wired() {
    let mut fx = fixture();
    fx.deps.events = None;
    let rec = record();

    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");

    let thread_a =
        crate::runtime::delegation::ChatTarget::in_thread(Some("general"), Some(EventSeq::new(1)));
    let thread_b =
        crate::runtime::delegation::ChatTarget::in_thread(Some("general"), Some(EventSeq::new(2)));

    pool.run(&rec.id, "ceo", "first", &fx.deps, thread_a)
        .await
        .expect("thread A turn");
    // The pool keeps one `CompanyAgent` per (company, agent) and
    // reuses it across turns — which is exactly why the binding exists.
    let agent = {
        let guard = pool.agents.read().await;
        guard
            .get(&rec.id)
            .and_then(|roster| roster.iter().find(|a| a.agent_id == "ceo"))
            .cloned()
            .expect("the agent stays resident between turns")
    };
    assert_eq!(
        *agent.bound_chat.lock().await,
        Some(("general".to_string(), Some(EventSeq::new(1)))),
        "the first turn binds to its own thread, journal or no journal"
    );

    pool.run(&rec.id, "ceo", "second", &fx.deps, thread_b)
        .await
        .expect("thread B turn");
    assert_eq!(
        *agent.bound_chat.lock().await,
        Some(("general".to_string(), Some(EventSeq::new(2)))),
        "a different thread of the same channel must rebind — with no \
         event log the re-seed is empty, but the history clear is not \
         optional"
    );
}

/// ...and the cost guard survives the finer key: consecutive turns in
/// the SAME thread are still not a switch, so they still pay for no
/// journal walk. Keyed only on the channel this held by accident; it
/// has to hold on purpose now.
#[tokio::test]
async fn a_second_turn_in_the_same_thread_does_not_re_read_the_journal() {
    let (fx, log, _seen) = recording_fixture();
    let rec = record();
    log.operator("general", "root"); // seq 0
    log.operator_in("general", "first", 0); // seq 1

    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");
    let thread =
        crate::runtime::delegation::ChatTarget::in_thread(Some("general"), Some(EventSeq::new(0)));

    pool.run(&rec.id, "ceo", "first", &fx.deps, thread)
        .await
        .expect("first chat turn");
    let reads_after_first = log.reads();

    log.operator_in("general", "second", 0); // seq 2, same thread
    pool.run(&rec.id, "ceo", "second", &fx.deps, thread)
        .await
        .expect("second chat turn");
    // Bounded, not zero — see the sibling test above for why the
    // property changed. A second turn in the same thread is still not
    // a switch, and still re-seeds nothing; what it now does is ask
    // whether anything was said elsewhere while it was answering here.
    let delta_reads = log.reads() - reads_after_first;
    assert!(
        delta_reads > 0,
        "a chat turn must ask what it missed; that is the session"
    );
    assert!(
        delta_reads <= reads_after_first,
        "the delta must cost no more than the seed it replaced: \
         {delta_reads} reads against {reads_after_first}"
    );
}

/// A journal that can be made to fail, so a test can break the seed's
/// one dependency after a binding has already been established.
struct BreakingLog {
    inner: Arc<InMemoryLog>,
    failing: std::sync::atomic::AtomicBool,
}

impl BreakingLog {
    fn break_now(&self) {
        self.failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl EventLog for BreakingLog {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> crate::Result<EventSeq> {
        self.inner.append(id, event).await
    }
    async fn read_from(
        &self,
        id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> crate::Result<Vec<StoredEvent>> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::OpenCompanyError::Store(
                "the journal is unreadable".into(),
            ));
        }
        self.inner.read_from(id, seq, limit).await
    }
    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
        self.inner.subscribe(id)
    }
}

/// The clear-and-reseed runs under the agent and binding locks, so it
/// cannot interleave — but it still depends on the journal, and the
/// journal can fail. When it does, the switch has already cleared the
/// outgoing desk's history and has nothing to put in its place.
///
/// The invariant that must survive that is the one the switch exists
/// for: a turn on `beta` never sees `alpha`. Starting blind is the
/// correct answer to an unreadable journal; falling back to the
/// transcript autoload — which on a switch points at the OUTGOING
/// thread — would answer beta's question out of alpha's conversation.
#[tokio::test]
async fn a_seed_that_cannot_be_built_starts_blind_rather_than_leaking_the_bound_desk() {
    let (mut fx, log, seen) = recording_fixture();
    let breaking = Arc::new(BreakingLog {
        inner: log.clone(),
        failing: std::sync::atomic::AtomicBool::new(false),
    });
    fx.deps.events = Some(breaking.clone());
    let rec = record();
    log.operator("alpha", "ALPHA_USER_MARKER");
    log.reply("alpha", "ALPHA_AGENT_MARKER");
    log.operator("beta", "BETA_USER_MARKER");
    log.reply("beta", "BETA_AGENT_MARKER");

    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");
    pool.run(
        &rec.id,
        "ceo",
        "hello alpha",
        &fx.deps,
        crate::runtime::delegation::ChatTarget::channel(Some("alpha")),
    )
    .await
    .expect("alpha chat turn");

    let bound_to_alpha = seen.lock().unwrap().join("\n===\n");
    assert!(
        bound_to_alpha.contains("ALPHA_USER_MARKER"),
        "the fixture must actually bind to alpha first, or this proves nothing: \
         {bound_to_alpha:?}"
    );

    breaking.break_now();
    let before = seen.lock().unwrap().len();

    pool.run(
        &rec.id,
        "ceo",
        "hello beta",
        &fx.deps,
        crate::runtime::delegation::ChatTarget::channel(Some("beta")),
    )
    .await
    .expect("a switch whose seed cannot be built must still answer");

    let after: Vec<String> = seen.lock().unwrap()[before..].to_vec();
    let last = after.last().expect("the beta turn made a model call");
    assert!(
        !last.contains("ALPHA_USER_MARKER") && !last.contains("ALPHA_AGENT_MARKER"),
        "an unreadable journal let the previously-bound desk's history into an \
         unrelated turn: {last:?}"
    );
    assert!(
        last.contains("hello beta"),
        "the turn still has to answer the message it was given: {last:?}"
    );
}

/// Issue #1957: a teammate's reply in the same thread reaches the next turn.
///
/// A second turn in the same thread is not a switch, and before the session
/// was continuous that meant nothing was re-read: the agent answered from its
/// own history, and a teammate who had spoken in the thread in between was
/// invisible to it. The session delta is what hands that line over now.
#[tokio::test]
async fn a_teammate_reply_in_the_same_thread_reaches_the_next_turn() {
    let (mut fx, log, seen) = recording_fixture();
    let rec = record();
    // `session_delta` reads the desks this agent sits on from the company
    // record, which the default fixture store does not hold.
    fx.deps.store = Arc::new(super::built_in_test_fixtures_2::LiveStore {
        record: StdMutex::new(Some(rec.clone())),
    });
    log.operator("general", "root"); // seq 0
    log.operator_in("general", "first", 0); // seq 1

    let pool = HarnessPool::new();
    pool.ensure(&rec, &fx.deps).await.expect("ensure");
    let thread =
        crate::runtime::delegation::ChatTarget::in_thread(Some("general"), Some(EventSeq::new(0)));

    // Each turn is bound to the journaled message it answers, as the chat
    // route does. Without that the session never gets a watermark, every turn
    // is a cold start that re-seeds the whole thread, and this test would pass
    // without the delta ever running.
    pool.run(
        &rec.id,
        "ceo",
        "first",
        &fx.deps,
        thread.answering(Some(EventSeq::new(1))),
    )
    .await
    .expect("first chat turn");
    let before = seen.lock().unwrap().len();

    log.reply_in("general", "ceo", "ceo answers first", 0); // seq 2
    log.reply_in("general", "engineer", "TEAMMATE_REPLY_MARKER", 0); // seq 3
    log.operator_in("general", "second", 0); // seq 4
    pool.run(
        &rec.id,
        "ceo",
        "second",
        &fx.deps,
        thread.answering(Some(EventSeq::new(4))),
    )
    .await
    .expect("second chat turn");

    let after: Vec<String> = seen.lock().unwrap()[before..].to_vec();
    let last = after.last().expect("the second turn made a model call");
    assert!(
        last.contains("TEAMMATE_REPLY_MARKER"),
        "a teammate's reply in the same thread never reached the next turn: {last:?}"
    );
}
