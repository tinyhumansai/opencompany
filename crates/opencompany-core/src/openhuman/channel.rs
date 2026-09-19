//! [`OpenHumanChannelAdapter`]: a [`ChannelAdapter`] backed by openhuman-core.
//!
//! Outbound messages are delivered over JSON-RPC (`openhuman.channels_send`).
//! Inbound delivery is not this port's job (issue #1958). This adapter covers
//! channels such as email whose inbound path is `InboxStore` / `WebhookReceived`,
//! not `OperatorMessage`. openhuman-core's `/events` schema is upstream-unstable
//! and drives no control flow here.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::openhuman::rpc::{OpenHumanRpc, rpc_method};
use crate::ports::channel::ChannelAdapter;
use crate::ports::types::OutboundMessage;

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

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        let params = serde_json::json!({ "channel": msg.channel, "text": msg.text });
        self.rpc
            .call(&rpc_method("channels", "send"), params)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
#[path = "channel_tests.rs"]
mod tests;
