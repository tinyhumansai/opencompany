//! The [`ChannelAdapter`] port: inbound/outbound conversation surfaces.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::Result;
use crate::ports::types::{InboundMessage, OutboundMessage};

/// A conversation surface. The built-in `"operator"` channel is always
/// present; others (email, tinyplace-dm, …) usually delegate to OpenHuman.
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// The channel's stable id, e.g. `"operator"` or `"email"`.
    fn channel_id(&self) -> &str;
    /// A stream of inbound messages on this channel.
    fn inbound(&self) -> BoxStream<'static, InboundMessage>;
    /// Sends an outbound message on this channel.
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}
