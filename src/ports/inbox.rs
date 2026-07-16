//! The [`InboxStore`] port: durable per-teammate email.
//!
//! Each company owns a set of named inboxes (one per teammate address). Both
//! inbound mail (pushed through the ingest transport) and outbound mail (every
//! SMTP send) are appended here so the console renders a single thread. The
//! records are non-secret message metadata and body — credentials never live
//! here; they stay in [`SecretStore`](crate::ports::SecretStore).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// One email in a teammate's inbox — inbound or outbound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailRecord {
    /// Stable id for the message within the company.
    pub id: String,
    /// The inbox this belongs to — the teammate's local part (`{agent_id}`) or
    /// full address, however the ingest transport addressed it.
    pub inbox: String,
    /// The sender address.
    pub from: String,
    /// The recipient address.
    pub to: String,
    /// The subject line.
    pub subject: String,
    /// The plain-text body.
    pub body: String,
    /// `true` for mail the company sent, `false` for mail it received.
    pub outbound: bool,
    /// Epoch-millis timestamp the record was appended.
    pub at_millis: u64,
}

/// Durable per-company inboxes. Company A's mail MUST be invisible to company B.
#[async_trait]
pub trait InboxStore: Send + Sync {
    /// Appends one email to its addressed inbox.
    async fn append(&self, company: &CompanyId, record: EmailRecord) -> Result<()>;
    /// Lists a single inbox's mail, oldest first.
    async fn list(&self, company: &CompanyId, inbox: &str) -> Result<Vec<EmailRecord>>;
    /// Lists the names of every inbox that has at least one message.
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<String>>;
}
