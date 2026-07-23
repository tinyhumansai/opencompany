//! [`OpenHumanChannelAdapter`]: a [`ChannelAdapter`] backed by openhuman-core.
//!
//! Outbound messages are delivered over JSON-RPC (`openhuman.channels_send`).
//! Inbound delivery rides a signed webhook route in a later batch, so
//! [`inbound`](OpenHumanChannelAdapter::inbound) is an empty stream for now —
//! openhuman-core's `/events` schema is upstream-unstable and drives no control
//! flow here.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::Result;
use crate::openhuman::rpc::{OpenHumanRpc, rpc_method};
use crate::ports::channel::ChannelAdapter;
use crate::ports::types::{InboundMessage, OutboundMessage};

/// A conversation surface (email, slack, …) delegated to openhuman-core.
pub struct OpenHumanChannelAdapter {
    channel_id: String,
    rpc: Arc<dyn OpenHumanRpc>,
}

impl OpenHumanChannelAdapter {
    /// Wires an adapter for `channel_id` (e.g. `"email"`) over `rpc`.
    pub fn new(channel_id: impl Into<String>, rpc: Arc<dyn OpenHumanRpc>) -> Self {
        Self {
            channel_id: channel_id.into(),
            rpc,
        }
    }
}

#[async_trait]
impl ChannelAdapter for OpenHumanChannelAdapter {
    fn channel_id(&self) -> &str {
        &self.channel_id
    }

    fn inbound(&self) -> BoxStream<'static, InboundMessage> {
        // Inbound arrives via the HMAC webhook route (a later batch), not here.
        Box::pin(stream::empty())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let params = serde_json::json!({ "channel": msg.channel, "text": msg.text });
        self.rpc
            .call(&rpc_method("channels", "send"), params)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::openhuman::rpc::MockOpenHumanRpc;
    use futures::StreamExt;

    #[tokio::test]
    async fn send_issues_channels_send_with_params() {
        let rpc = Arc::new(
            MockOpenHumanRpc::new().with_result("openhuman.channels_send", serde_json::json!({})),
        );
        let adapter = OpenHumanChannelAdapter::new("email", rpc.clone());
        assert_eq!(adapter.channel_id(), "email");
        adapter
            .send(OutboundMessage {
                channel: "email".into(),
                text: "hello".into(),
                steps: Vec::new(),
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
    async fn inbound_is_empty() {
        let rpc = Arc::new(MockOpenHumanRpc::new());
        let adapter = OpenHumanChannelAdapter::new("email", rpc);
        assert_eq!(adapter.inbound().count().await, 0);
    }

    #[tokio::test]
    async fn send_propagates_rpc_error() {
        // No handler → the mock errors, standing in for a transport failure.
        let rpc = Arc::new(MockOpenHumanRpc::new());
        let adapter = OpenHumanChannelAdapter::new("email", rpc);
        let err = adapter
            .send(OutboundMessage {
                channel: "email".into(),
                text: "hi".into(),
                steps: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, crate::OpenCompanyError::OpenHuman { .. }));
    }
}
