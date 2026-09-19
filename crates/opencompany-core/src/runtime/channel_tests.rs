use super::*;
use crate::server::ops::language::DEFAULT_DESK;
use futures::stream::{self, BoxStream};

/// The shared refusal sentence names what IS deliverable, and says so
/// plainly when the answer is nothing — a desk-less company is a legitimate
/// state (issue #963), not a malformed one, so the empty case gets a
/// sentence rather than a dangling `has: `.
#[test]
fn the_refusal_sentence_names_the_live_set() {
    let message = undeliverable_channel_message("operator", &["engineering", "product"]);
    assert!(message.contains("`operator` is not an automation delivery channel"));
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
        "/../../frontend/src/views/WorkflowCreateDialog.tsx"
    ));
    const TAIL: &str = "is not an automation delivery channel — this runtime has:";

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
            outputs: Vec::new(),
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
            outputs: Vec::new(),
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
            outputs: Vec::new(),
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
    let channel = DurableOperatorChannel::new(CompanyId::new("acme"), Arc::new(FailingEventLog));
    let result = channel
        .send(OutboundMessage {
            message_id: None,
            task_id: None,
            outputs: Vec::new(),
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
