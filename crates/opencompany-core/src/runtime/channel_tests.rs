use super::*;

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
