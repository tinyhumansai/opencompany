//! The built-in `"operator"` channel adapter.
//!
//! Every company has an operator channel — the human's chat surface. Phase 1
//! backs it with an in-memory buffer: outbound messages the runtime routes here
//! are captured so the HTTP layer (and tests) can read them back. Inbound
//! operator messages arrive as `OperatorMessage` events through the HTTP chat
//! route, not through this stream, so `inbound` is an empty stream for now.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::Result;
use crate::ports::channel::ChannelAdapter;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, InboundMessage, OutboundMessage};

/// The channel id of the always-present operator surface.
pub const OPERATOR_CHANNEL: &str = "operator";

/// Whether a channel with this id may be named as the target of an `output`
/// node's `channel` delivery destination.
///
/// The operator channel is the single exclusion, and it is deliberate:
/// [`OperatorChannel`] is an in-memory response spy with no durable reader.
/// Interactive chat journals its own replies after the cycle, but a workflow
/// report posted here reaches nobody — accepting it would report a successful
/// discard. Everything else that is wired (desk channels, provider channels) is
/// a real write path.
///
/// Issue #981: this is the one place the rule is stated. It previously lived as
/// an inline `!=` where the builder assembles the delivery deps, as a `by name`
/// refusal in [`crate::workflows::delivery`], and as a prose sentence in the
/// accessor the console's destination picker reads — and that third copy said
/// the opposite, so the picker offered authors the one target guaranteed to
/// fail. Call this instead of re-deciding.
pub fn is_deliverable_channel(channel_id: &str) -> bool {
    channel_id != OPERATOR_CHANNEL
}

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
                    agent_id: "workflow".to_string(),
                    text: msg.text,
                    steps: msg.steps,
                    task_id: msg.task_id,
                    parent: msg
                        .reply_to
                        .and_then(|reply| reply.chat_id.parse::<u64>().ok())
                        .map(EventSeq::new),
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

/// A durable-looking channel that records what it was sent, for tests whose
/// subject is the runner's delivery bookkeeping rather than any one adapter.
///
/// Those tests used [`OperatorChannel`] as their spy, which stopped working
/// when workflow delivery began refusing `operator` outright: the count they
/// assert is "how many times did the report reach the channel", and a refusal
/// answers a different question. This carries an ordinary channel id so it
/// clears the refusal, and keeps the buffer so the counting still works.
// Every consumer of this lives behind `openhuman`/`tinycortex`, so a
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

    /// The operator channel is excluded from workflow delivery, and every other
    /// wired id is not. The rule reads the same for a desk, a provider channel
    /// and an id nobody wired — deliverability is about `operator`, not about
    /// whether the caller has already checked the adapter exists.
    #[test]
    fn only_the_operator_channel_is_undeliverable() {
        assert!(!is_deliverable_channel(OPERATOR_CHANNEL));
        assert!(is_deliverable_channel("engineering"));
        assert!(is_deliverable_channel("email"));
        assert!(is_deliverable_channel("Operator"));
    }

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
            })
            .await;
        assert!(result.is_err(), "an unwritable journal must fail the send");
    }
}
