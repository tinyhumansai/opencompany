//! The [`InboxStore`] port: durable per-teammate email.
//!
//! Each company owns a set of named inboxes (one per teammate address). Both
//! inbound mail (pushed through the ingest transport) and outbound mail (every
//! SMTP send) are appended here so the console renders a single thread. The
//! records are non-secret message metadata and body — credentials never live
//! here; they stay in [`SecretStore`](crate::ports::SecretStore).
//!
//! An inbox's *metadata* (display name, address, whether it is enabled) is
//! toggled from the Team page (`PUT …/team/{agentId}/inbox`) and stored
//! separately from its messages, so an enabled-but-empty inbox is still listed.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// The non-secret metadata of one inbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxMeta {
    /// The inbox key (a teammate's local part / slug).
    pub key: String,
    /// The teammate's display name.
    pub name: String,
    /// The full email address (`{key}@{domain}`), when a domain is configured.
    pub address: String,
    /// Whether the inbox is enabled (receiving mail on the Team page).
    pub enabled: bool,
}

/// One email in a teammate's inbox — inbound or outbound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailRecord {
    /// Stable id for the message within the company.
    pub id: String,
    /// The inbox this belongs to — the teammate's local part (`{agent_id}`).
    pub inbox: String,
    /// The sender's display name (may be empty).
    #[serde(default)]
    pub from_name: String,
    /// The sender's email address.
    pub from_email: String,
    /// The subject line.
    pub subject: String,
    /// The plain-text body.
    pub body: String,
    /// Epoch-millis timestamp the record was appended.
    pub at_millis: u64,
    /// Whether the operator has read the message.
    #[serde(default)]
    pub read: bool,
    /// `true` for mail the company sent, `false` for mail it received.
    pub outbound: bool,
}

/// Durable per-company inboxes. Company A's mail MUST be invisible to company B.
#[async_trait]
pub trait InboxStore: Send + Sync {
    /// Lists every inbox — those with explicit metadata plus any that only have
    /// messages — with its non-secret metadata.
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<InboxMeta>>;
    /// Upserts an inbox's metadata (its enabled flag, name, address).
    async fn set_enabled(&self, company: &CompanyId, key: &str, meta: &InboxMeta) -> Result<()>;
    /// Lists one inbox's mail, oldest first, paginated by `offset`/`limit`
    /// (`limit == usize::MAX` reads to the end).
    async fn messages(
        &self,
        company: &CompanyId,
        key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<EmailRecord>>;
    /// Appends one email to its addressed inbox.
    async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> Result<()>;
    /// Marks messages in `key` as read — the given `ids`, or every message when
    /// `ids` is `None`. Returns the count of messages still unread in `key`.
    async fn mark_read(
        &self,
        company: &CompanyId,
        key: &str,
        ids: Option<&[String]>,
    ) -> Result<u64>;
}
