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
use crate::ports::types::{InboundMessage, OutboundMessage};

/// The channel id of the always-present operator surface.
pub const OPERATOR_CHANNEL: &str = "operator";

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

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn buffers_sent_messages() {
        let channel = OperatorChannel::new();
        assert_eq!(channel.channel_id(), "operator");
        channel
            .send(OutboundMessage {
                channel: "operator".into(),
                text: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(channel.sent().len(), 1);
        assert_eq!(channel.sent()[0].text, "hello");
    }
}
