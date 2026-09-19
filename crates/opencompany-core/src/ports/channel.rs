//! The [`ChannelAdapter`] port: outbound conversation surfaces.
//!
//! Inbound messages do **not** flow through this trait (issue #1958). Ingress
//! is route-specific: operator chat arrives as `CompanyEvent::OperatorMessage`
//! via the HTTP chat route and the ACP `session/prompt` route; email/webhook
//! ingress is filed into [`crate::ports::InboxStore`] and emits
//! `CompanyEvent::WebhookReceived`; other integrations have their own paths.
//!
//! **API migration:** [`ChannelAdapter::inbound`] remains as a deprecated
//! default that returns an empty stream so out-of-tree implementers and callers
//! keep compiling. New code must not call or override it; remove any override
//! when convenient. There is no replacement stream — ingress is route-specific.

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::Result;
#[allow(deprecated)]
use crate::ports::types::{InboundMessage, OutboundMessage};

/// A conversation surface. The built-in `"operator"` channel is always
/// present; others (email, tinyplace-dm, …) usually delegate to OpenHuman.
///
/// Outbound-only: the live contract is [`send`](Self::send). Inbound messages
/// reach the runtime through route-specific paths, not through this trait.
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// The channel's stable id, e.g. `"operator"` or `"email"`.
    fn channel_id(&self) -> &str;

    /// Sends an outbound message on this channel.
    async fn send(&self, msg: OutboundMessage) -> Result<()>;

    /// Deprecated empty inbound stream retained for source compatibility
    /// (issue #1958).
    ///
    /// Every in-tree implementation historically returned `stream::empty()` and
    /// nothing consumed the result. The default preserves that behaviour so
    /// downstream crates that still declare or call `inbound` compile with a
    /// deprecation warning instead of a hard break. New code must not use this
    /// method — route inbound traffic through `CompanyEvent::OperatorMessage`
    /// (operator chat / ACP), or through [`crate::ports::InboxStore`] plus
    /// `CompanyEvent::WebhookReceived` (email / webhooks).
    #[deprecated(
        since = "0.2.4",
        note = "ChannelAdapter is outbound-only (issue #1958). Inbound messages \
                arrive through CompanyEvent / InboxStore, not this stream. The \
                default returns an empty stream for source compatibility — do \
                not call or override it in new code."
    )]
    #[allow(deprecated)]
    fn inbound(&self) -> BoxStream<'static, InboundMessage> {
        Box::pin(stream::empty())
    }
}
