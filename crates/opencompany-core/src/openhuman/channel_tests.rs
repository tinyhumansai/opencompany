use super::*;
use crate::openhuman::rpc::MockOpenHumanRpc;

#[tokio::test]
async fn send_issues_channels_send_with_params() {
    let rpc = Arc::new(
        MockOpenHumanRpc::new().with_result("openhuman.channels_send", serde_json::json!({})),
    );
    let adapter = OpenHumanChannelAdapter::new("email", rpc.clone());
    assert_eq!(adapter.channel_id(), "email");
    adapter
        .send(OutboundMessage {
            message_id: None,
            task_id: None,
            outputs: Vec::new(),
            channel: "email".into(),
            agent: None,
            text: "hello".into(),
            steps: Vec::new(),
            reply_to: None,
            mentions: Vec::new(),
        })
        .await
        .unwrap();
    let calls = rpc.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "openhuman.channels_send");
    assert_eq!(calls[0].1["channel"], "email");
    assert_eq!(calls[0].1["text"], "hello");
}

#[tokio::test]
async fn send_propagates_rpc_error() {
    // No handler → the mock errors, standing in for a transport failure.
    let rpc = Arc::new(MockOpenHumanRpc::new());
    let adapter = OpenHumanChannelAdapter::new("email", rpc);
    let err = adapter
        .send(OutboundMessage {
            message_id: None,
            task_id: None,
            outputs: Vec::new(),
            channel: "email".into(),
            agent: None,
            text: "hi".into(),
            steps: Vec::new(),
            reply_to: None,
            mentions: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, crate::OpenCompanyError::OpenHuman { .. }));
}
