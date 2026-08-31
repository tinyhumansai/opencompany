//! The built-in `"operator"` channel adapter.
//!
//! Every company has an operator channel — the human's chat surface. The
//! *interactive* side is backed by an in-memory buffer ([`OperatorChannel`]):
//! outbound messages the runtime routes here are captured so the HTTP layer
//! (and tests) can read them back, while the console's live POST already reads
//! `CycleReport.responses` directly. Inbound operator messages arrive as
//! `OperatorMessage` events through the HTTP chat route, not through this
//! stream, so `inbound` is an empty stream for now.
//!
//! The *delivery* side is backed by [`DurableOperatorChannel`] (issue #1757).
//! A workflow `owner` report on a company with no mailbox used to dead-end on
//! the in-memory buffer — which has no durable reader, so the one human who
//! could act on it never saw it. The durable adapter instead journals the
//! report onto its own `operator` chat line through the same event-log
//! mechanism [`DeskChannel`] uses, so it survives a restart and is rendered by
//! the console's standing **Operator channel** — a first-class, always-present
//! system desk the desk list enumerates alongside the real desks. It carries the
//! `operator` channel id but is wired only into the workflow-delivery adapter
//! set, never into the interactive runtime channels — so it can never
//! double-journal an interactive reply.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::Result;
use crate::ports::channel::ChannelAdapter;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, InboundMessage, OutboundMessage};

/// The `agent_id` a workflow-delivered report is journaled under, so the
/// console (and any other reader) can tell a workflow report apart from an
/// agent's own reply. Shared by [`DeskChannel`] and [`DurableOperatorChannel`]
/// so every workflow bubble names the same author whichever surface it lands
/// on.
///
/// Hyphenated on purpose, the same way
/// [`CONFINED_AGENT_ID`](crate::ports::CONFINED_AGENT_ID) is: `agent_slug`
/// (console-minted teammates) and `is_snake_case` (manifest-declared ones)
/// both reject a hyphen, so no roster id — minted before this constant
/// existed, minted after, or hand-written into a manifest — can ever equal
/// this value. That is load-bearing, not cosmetic: the bare word `"workflow"`
/// was a legal slug (`agent_slug("Workflow") == "workflow"`), so a company
/// that named a teammate "Workflow" *before* this reservation shipped would
/// otherwise still be holding the id today, and every workflow report
/// delivered to it would misattribute to that teammate the moment this
/// adapter went live — a collision no manifest-validation or mint-time guard
/// can retroactively undo for data that already exists. Picking a value nothing
/// could ever have minted sidesteps needing one.
pub const WORKFLOW_REPLY_AUTHOR: &str = "workflow-report";

/// The `agent_id` an `owner`-destination report is journaled under when it
/// falls back to the operator channel (no mailbox, or no active admin has an
/// address) — issue #1781 review (Codex P1).
///
/// The `owner` destination's whole contract, on the ordinary email branch, is
/// "active admins only" (`server::workflows::delivery::owner_recipients`
/// filters to `UserRole::Admin` + `UserStatus::Active`). The channel fallback
/// used to break that contract silently: it journaled under
/// [`WORKFLOW_REPLY_AUTHOR`], the same id every other operator-channel report
/// uses, and `chat_history` authorizes any signed-in company user for a desk
/// — admin or Member — with no role check at all. A Member could therefore
/// read a report an unavailable mailbox would otherwise have sent only to
/// administrators. Journaling under a distinct author id lets the read path
/// (`server::chat_history::history_for_desk`) drop exactly these rows for a
/// non-admin viewer, without touching any other report's visibility.
///
/// Hyphenated for the same reason `WORKFLOW_REPLY_AUTHOR` is: unmintable by
/// any roster id, so nothing can ever masquerade as — or be mistaken for — an
/// owner-fallback report.
pub const OWNER_FALLBACK_REPORT_AUTHOR: &str = "owner-fallback-report";

/// The channel id of the always-present operator surface.
pub const OPERATOR_CHANNEL: &str = "operator";

/// Where the durable Operator system feed journals when [`OPERATOR_CHANNEL`]
/// is already claimed by a grandfathered roster **teammate** with no desk of
/// the same id — see
/// [`CompanyRecord::operator_feed_channel`](crate::ports::types::CompanyRecord::operator_feed_channel)
/// for the full account (issue #1781 review: CodeRabbit Major + Codex P2).
///
/// Hyphenated, so — like [`WORKFLOW_REPLY_AUTHOR`] — no desk id
/// (`is_valid_desk_id`, manifest `is_snake_case`) or roster agent id
/// (`agent_slug`, manifest `is_snake_case`) can ever equal it, minted or
/// declared before this constant existed or after. The collision the system
/// feed diverts to avoid can therefore never re-open by a company later
/// minting or declaring something at this address.
pub const OPERATOR_CHANNEL_COLLISION_FALLBACK: &str = "operator-feed";

/// The operator-readable sentence for a `channel` destination that names
/// something outside the deliverable set, built from the set that is live right
/// now so the fix is legible without a second lookup.
///
/// Shared by the delivery-time refusal and the save-time rejection (issue
/// #981), so an author who trips the guard at save and an author who reads a
/// failed delivery row are told the same thing about the same runtime.
pub fn undeliverable_channel_message(target: &str, deliverable: &[&str]) -> String {
    let has = if deliverable.is_empty() {
        "no durable channels".to_string()
    } else {
        deliverable.join(", ")
    };
    format!("`{target}` is not a workflow delivery channel — this runtime has: {has}")
}

/// A desk-backed [`ChannelAdapter`]. Sending appends an agent reply to the
/// company's durable event log, which is the existing read path for desk chat
/// history. The adapter is deliberately one-per-desk so channel lookup and
/// chat-thread ownership use the same canonical desk id.
#[derive(Clone)]
pub struct DeskChannel {
    company: CompanyId,
    desk_id: String,
    events: Arc<dyn EventLog>,
}

impl DeskChannel {
    /// Creates a channel for an already-resolved desk id.
    pub fn new(company: CompanyId, desk_id: String, events: Arc<dyn EventLog>) -> Self {
        Self {
            company,
            desk_id,
            events,
        }
    }
}

#[async_trait]
impl ChannelAdapter for DeskChannel {
    fn channel_id(&self) -> &str {
        &self.desk_id
    }

    fn inbound(&self) -> BoxStream<'static, InboundMessage> {
        Box::pin(stream::empty())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.events
            .append(
                &self.company,
                CompanyEvent::AgentReply {
                    chat_id: self.desk_id.clone(),
                    agent_id: WORKFLOW_REPLY_AUTHOR.to_string(),
                    text: msg.text,
                    steps: msg.steps,
                    task_id: msg.task_id,
                    parent: msg
                        .reply_to
                        .and_then(|reply| reply.chat_id.parse::<u64>().ok())
                        .map(EventSeq::new),
                    // Workflow node output. This adapter holds no company
                    // record, so it has nothing to resolve an `@name` against
                    // — and a workflow bubble addresses the channel it posts
                    // into, not a person in it. Left empty rather than
                    // half-resolved.
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await?;
        Ok(())
    }
}

impl std::fmt::Debug for DeskChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeskChannel")
            .field("company", &self.company)
            .field("desk_id", &self.desk_id)
            .finish()
    }
}

/// The built-in operator [`ChannelAdapter`], buffering sent messages in memory.
#[derive(Clone, Default)]
pub struct OperatorChannel {
    sent: Arc<StdMutex<Vec<OutboundMessage>>>,
}

impl OperatorChannel {
    /// Creates an empty operator channel.
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of every message sent on this channel so far.
    pub fn sent(&self) -> Vec<OutboundMessage> {
        self.sent.lock().expect("operator buffer poisoned").clone()
    }
}

#[async_trait]
impl ChannelAdapter for OperatorChannel {
    fn channel_id(&self) -> &str {
        OPERATOR_CHANNEL
    }

    fn inbound(&self) -> BoxStream<'static, InboundMessage> {
        Box::pin(stream::empty())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.sent
            .lock()
            .expect("operator buffer poisoned")
            .push(msg);
        Ok(())
    }
}

impl std::fmt::Debug for OperatorChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorChannel")
            .field("sent", &self.sent().len())
            .finish()
    }
}

/// The **durable** operator delivery channel (issue #1757).
///
/// Carries the [`OPERATOR_CHANNEL`] id, but unlike [`OperatorChannel`] it does
/// not buffer in memory — it appends the report to the company's event log as an
/// `AgentReply` on the **dedicated operator line** (`chat_id == OPERATOR_CHANNEL`,
/// NOT the General/main line). That is the same durable write path [`DeskChannel`]
/// uses, so a report survives a restart; and because it lands on its own
/// `operator` chat id, the console renders it in the standing **Operator channel**
/// — a first-class, always-present system desk the desk list enumerates
/// alongside the real desks (see `server::operator::list_desks`). It is the
/// aggregating "what happened" surface for workflow-run reports and the
/// owner/no-mailbox fallback.
///
/// Authored under [`WORKFLOW_REPLY_AUTHOR`], and the report text carries a source
/// header ([`operator_report`](crate::workflows::delivery)), so a workflow report
/// is distinguishable at a glance from any other message.
///
/// Wired **only** into the workflow-delivery adapter set (see the runtime
/// builder), never into the interactive runtime channels the cycle's
/// `route_response` sends to — so an interactive reply that already journals
/// itself can never be double-recorded here.
#[derive(Clone)]
pub struct DurableOperatorChannel {
    company: CompanyId,
    events: Arc<dyn EventLog>,
}

impl DurableOperatorChannel {
    /// Creates a durable operator channel for `company`, journaling through
    /// `events`.
    pub fn new(company: CompanyId, events: Arc<dyn EventLog>) -> Self {
        Self { company, events }
    }
}

#[async_trait]
impl ChannelAdapter for DurableOperatorChannel {
    fn channel_id(&self) -> &str {
        OPERATOR_CHANNEL
    }

    fn inbound(&self) -> BoxStream<'static, InboundMessage> {
        Box::pin(stream::empty())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.events
            .append(
                &self.company,
                CompanyEvent::AgentReply {
                    // The dedicated operator line, normally `OPERATOR_CHANNEL`
                    // itself — `owns("operator","operator",…)` matches it (it is
                    // NOT folded into General — see
                    // `server::chat_history::is_general_chat`), so the console's
                    // standing Operator channel renders exactly these reports and
                    // nothing else.
                    //
                    // The caller (`workflows::delivery::send_to_channel_adapter`)
                    // sets `msg.channel` to
                    // `CompanyRecord::operator_feed_channel()`'s result, not
                    // always the literal `OPERATOR_CHANNEL`: a company whose
                    // roster already grandfathers a **teammate** at that literal
                    // id resolves to `OPERATOR_CHANNEL_COLLISION_FALLBACK`
                    // instead, so a report can never land on the same address as
                    // that teammate's own DM (issue #1781 review).
                    chat_id: msg.channel,
                    // Ordinarily `WORKFLOW_REPLY_AUTHOR`. The owner-fallback
                    // report overrides this to `OWNER_FALLBACK_REPORT_AUTHOR`
                    // via `msg.agent` so the read path can restrict exactly
                    // those rows to admins (issue #1781 review, Codex P1) —
                    // every other producer leaves `agent` unset and gets the
                    // ordinary author, unchanged.
                    agent_id: msg
                        .agent
                        .unwrap_or_else(|| WORKFLOW_REPLY_AUTHOR.to_string()),
                    text: msg.text,
                    steps: msg.steps,
                    task_id: msg.task_id,
                    parent: msg
                        .reply_to
                        .and_then(|reply| reply.chat_id.parse::<u64>().ok())
                        .map(EventSeq::new),
                    // A workflow report addresses the thread it posts into, not a
                    // person in it — nothing to resolve an `@name` against.
                    mentions: Vec::new(),
                    mention_depth: 0,
                },
            )
            .await?;
        Ok(())
    }
}

impl std::fmt::Debug for DurableOperatorChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableOperatorChannel")
            .field("company", &self.company)
            .finish()
    }
}

/// A durable-looking channel that records what it was sent, for tests whose
/// subject is the runner's delivery bookkeeping rather than any one adapter.
///
/// Those tests used [`OperatorChannel`] as their spy, which stopped working
/// when workflow delivery began refusing `operator` outright: the count they
/// assert is "how many times did the report reach the channel", and a refusal
/// answers a different question. This carries an ordinary channel id so it
/// clears the refusal, and keeps the buffer so the counting still works.
// Every consumer of this lives behind `openhuman`/`tinymemory`, so a
// default-feature build compiles it and constructs it nowhere. That is a
// feature-configuration fact, not dead code: the runner and delivery suites
// that use it are simply not selected in that lane (issue #770).
#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone, Default)]
pub(crate) struct RecordingChannel {
    id: String,
    sent: Arc<StdMutex<Vec<OutboundMessage>>>,
}

#[cfg(test)]
#[allow(dead_code)]
impl RecordingChannel {
    pub(crate) fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            sent: Arc::default(),
        }
    }

    pub(crate) fn sent(&self) -> Vec<OutboundMessage> {
        self.sent.lock().expect("recording buffer poisoned").clone()
    }
}

#[cfg(test)]
#[async_trait]
impl ChannelAdapter for RecordingChannel {
    fn channel_id(&self) -> &str {
        &self.id
    }

    fn inbound(&self) -> BoxStream<'static, InboundMessage> {
        Box::pin(stream::empty())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.sent
            .lock()
            .expect("recording buffer poisoned")
            .push(msg);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::server::ops::language::DEFAULT_DESK;

    /// The shared refusal sentence names what IS deliverable, and says so
    /// plainly when the answer is nothing — a desk-less company is a legitimate
    /// state (issue #963), not a malformed one, so the empty case gets a
    /// sentence rather than a dangling `has: `.
    #[test]
    fn the_refusal_sentence_names_the_live_set() {
        let message = undeliverable_channel_message("operator", &["engineering", "product"]);
        assert!(message.contains("`operator` is not a workflow delivery channel"));
        assert!(message.ends_with("this runtime has: engineering, product"));

        let empty = undeliverable_channel_message("engineering", &[]);
        assert!(
            empty.ends_with("this runtime has: no durable channels"),
            "{empty}"
        );
    }

    /// The console's own pre-flight tells an author the same thing the host
    /// does, and this fails if either side is reworded alone — the same
    /// contract `destination_messages_match_the_console` holds for the other
    /// destination rules (issue #260).
    ///
    /// It matters more here than elsewhere: the console's list and the host's
    /// refusal disagreeing about which channels are real is exactly issue #981.
    #[test]
    fn the_consoles_pre_flight_says_the_same_thing() {
        const CONSOLE_DIALOG: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/frontend/src/views/WorkflowCreateDialog.tsx"
        ));
        const TAIL: &str = "is not a workflow delivery channel — this runtime has:";

        assert!(undeliverable_channel_message("operator", &["engineering"]).contains(TAIL));
        assert!(
            CONSOLE_DIALOG.contains(TAIL),
            "frontend/src/views/WorkflowCreateDialog.tsx no longer says `{TAIL}` — the console's \
             client-side pre-flight has drifted from the host's rule. Reword both sides together, \
             or drop the pre-flight and surface the host's message on the failed save."
        );
    }

    #[tokio::test]
    async fn buffers_sent_messages() {
        let channel = OperatorChannel::new();
        assert_eq!(channel.channel_id(), "operator");
        channel
            .send(OutboundMessage {
                message_id: None,
                task_id: None,
                channel: "operator".into(),
                agent: None,
                text: "hello".into(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(channel.sent().len(), 1);
        assert_eq!(channel.sent()[0].text, "hello");
    }

    /// An [`EventLog`] whose `append` always errors, so the desk channel's own
    /// failure path is reachable.
    struct FailingEventLog;

    #[async_trait]
    impl EventLog for FailingEventLog {
        async fn append(&self, _company: &CompanyId, _event: CompanyEvent) -> Result<EventSeq> {
            Err(crate::OpenCompanyError::Config(
                "event journal is unwritable".into(),
            ))
        }

        async fn read_from(
            &self,
            _company: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<crate::ports::types::StoredEvent>> {
            Ok(Vec::new())
        }

        fn subscribe(
            &self,
            _company: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// A desk send is a durable write, so a journal that refuses it is a failed
    /// delivery — not a silent one. `send` propagates rather than logging and
    /// answering `Ok`, and this pins that: swallowing the error would leave
    /// delivery reporting `Sent` for a report that reached nobody, which is the
    /// exact defect the desk channel exists to end (issue #835).
    #[tokio::test]
    async fn a_desk_send_fails_when_the_journal_refuses_it() {
        let channel = DeskChannel::new(
            CompanyId::new("acme"),
            "engineering".to_string(),
            Arc::new(FailingEventLog),
        );
        assert_eq!(channel.channel_id(), "engineering");
        let result = channel
            .send(OutboundMessage {
                message_id: None,
                task_id: None,
                channel: "engineering".into(),
                agent: None,
                text: "the weekly digest".into(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
            })
            .await;
        assert!(result.is_err(), "an unwritable journal must fail the send");
    }

    /// An [`EventLog`] that records every appended event, for asserting what a
    /// channel journals.
    #[derive(Default)]
    struct RecordingEventLog {
        events: StdMutex<Vec<CompanyEvent>>,
    }

    #[async_trait]
    impl EventLog for RecordingEventLog {
        async fn append(&self, _company: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
            let mut events = self.events.lock().expect("recording log poisoned");
            events.push(event);
            Ok(EventSeq::new(events.len() as u64))
        }

        async fn read_from(
            &self,
            _company: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<crate::ports::types::StoredEvent>> {
            Ok(Vec::new())
        }

        fn subscribe(
            &self,
            _company: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(stream::empty())
        }
    }

    /// The durable operator channel carries the `operator` id and journals its
    /// report onto that dedicated Operator line — never the General desk,
    /// authored by `workflow-report` — so the owner/no-mailbox fallback lands
    /// somewhere the console renders and survives a restart, and reads as a
    /// workflow report rather than an agent's own reply (issue #1757).
    #[tokio::test]
    async fn the_durable_operator_channel_journals_to_the_operator_line() {
        let log = Arc::new(RecordingEventLog::default());
        let channel = DurableOperatorChannel::new(CompanyId::new("acme"), log.clone());
        assert_eq!(channel.channel_id(), OPERATOR_CHANNEL);

        channel
            .send(OutboundMessage {
                message_id: None,
                task_id: None,
                channel: OPERATOR_CHANNEL.into(),
                agent: None,
                text: "[Acme] Weekly digest — Owner summary\n\nQ3 is up 12%.".into(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
            })
            .await
            .expect("a durable operator send journals rather than buffering");

        let events = log.events.lock().expect("recording log poisoned");
        assert_eq!(events.len(), 1, "the report must be journaled");
        match &events[0] {
            CompanyEvent::AgentReply {
                chat_id,
                agent_id,
                text,
                ..
            } => {
                assert_eq!(
                    chat_id, OPERATOR_CHANNEL,
                    "lands on the dedicated operator line, not General"
                );
                assert_ne!(chat_id, DEFAULT_DESK, "must NOT fold into the main line");
                assert_eq!(agent_id, WORKFLOW_REPLY_AUTHOR, "authored by the workflow");
                assert!(text.contains("Q3 is up 12%."), "{text}");
                assert!(text.contains("Weekly digest"), "carries its subject header");
            }
            other => panic!("expected an AgentReply, got {other:?}"),
        }
    }

    /// A durable operator send is a real write, so a journal that refuses it is a
    /// failed delivery — the same fail-loud contract [`DeskChannel`] holds. This
    /// is what lets the owner fallback report `Failed` on a broken journal instead
    /// of a silent discard.
    #[tokio::test]
    async fn a_durable_operator_send_fails_when_the_journal_refuses_it() {
        let channel =
            DurableOperatorChannel::new(CompanyId::new("acme"), Arc::new(FailingEventLog));
        let result = channel
            .send(OutboundMessage {
                message_id: None,
                task_id: None,
                channel: OPERATOR_CHANNEL.into(),
                agent: None,
                text: "the owner report".into(),
                steps: Vec::new(),
                reply_to: None,
                mentions: Vec::new(),
            })
            .await;
        assert!(result.is_err(), "an unwritable journal must fail the send");
    }
}
