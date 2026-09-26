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
//! A workflow report addressed to the operator does not go through this
//! adapter: `workflows::delivery` journals it into the responsible agent's DM
//! and files a notification.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::Result;
use crate::ports::channel::ChannelAdapter;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, InboundMessage, OutboundMessage};

/// The `agent_id` a workflow-delivered report is journaled under, so the
/// console (and any other reader) can tell a workflow report apart from an
/// agent's own reply. Shared by [`DeskChannel`] and the operator reports
/// `workflows::delivery` journals, so every workflow bubble names the same
/// author whichever surface it lands on.
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
    format!("`{target}` is not an automation delivery channel — this runtime has: {has}")
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
                    audience: Vec::new(),
                    episode: None,
                    chat_id: self.desk_id.clone(),
                    agent_id: WORKFLOW_REPLY_AUTHOR.to_string(),
                    text: msg.text,
                    steps: msg.steps,
                    task_id: msg.task_id,
                    outputs: msg.outputs,
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
#[path = "channel_tests.rs"]
mod tests;
