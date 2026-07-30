//! Mailbox reads + read-state writes under both scope forms:
//! `GET /inboxes`, `GET /inboxes/{key}/messages`, `POST /inboxes/{key}/read`.
//!
//! The reads project the [`InboxStore`](crate::ports::InboxStore) into the
//! operator console's Inbox tab — the REST twin of the GraphQL
//! `Company.inboxes` resolver ([`graphql::inbox`](crate::server::graphql::inbox)),
//! so a message filed by either inbound path (the ingest webhook or the IMAP
//! poller, see [`inbox`](super::inbox)) is visible in the web app. The write
//! marks messages read and returns the count still unread. Every field here is
//! non-secret message metadata/body; credentials never appear.
//!
//! These are the console's *only* per-agent inbox read — it ships no GraphQL
//! client, which is why the Inbox view used to fall back to a client-side
//! fixture that invented the same mail for every teammate (issue #173). Each
//! key reads its own mail and nothing else; the enable toggle behind
//! `enabled` lives in [`team`](super::team) (`PUT …/team/{agentId}/inbox`).

use axum::extract::Path;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::ports::inbox::EmailRecord;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the mailbox read + read-state route fragment.
pub fn router() -> Router<AppState> {
    scoped("/inboxes", get(list_inboxes))
        .merge(scoped("/inboxes/{key}/messages", get(list_messages)))
        .merge(scoped("/inboxes/{key}/read", post(mark_read)))
}

/// One inbox's non-secret status for the Inbox tab. Mirrors
/// [`InboxMeta`](crate::ports::inbox::InboxMeta) plus the unread count the
/// GraphQL `Inbox.unread` field computes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InboxDto {
    /// The inbox key (a teammate's local part / slug).
    key: String,
    /// The teammate's display name.
    name: String,
    /// The full address (`{key}@{domain}` when a domain is configured).
    address: String,
    /// Whether the inbox is enabled on the Team page.
    enabled: bool,
    /// The number of unread received (inbound) messages.
    unread: u64,
}

/// The mark-read body: an optional id list (omitted = mark the whole inbox).
#[derive(Debug, Deserialize, Default)]
struct MarkRead {
    #[serde(default)]
    ids: Option<Vec<String>>,
}

/// The mark-read response: the count of messages still unread in the inbox.
#[derive(Debug, Serialize)]
struct UnreadCount {
    unread: u64,
}

/// The sub-resource path (`key`).
#[derive(Debug, Deserialize)]
struct InboxKeyPath {
    key: String,
}

/// `GET …/inboxes` — every inbox with its non-secret metadata and unread count.
/// Mirrors the GraphQL `Company.inboxes` resolver.
async fn list_inboxes(company: ScopedCompany) -> Result<Json<Vec<InboxDto>>, ApiError> {
    let metas = company.runtime.inbox().inboxes(company.id()).await?;
    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let messages = company
            .runtime
            .inbox()
            .messages(company.id(), &meta.key, usize::MAX, 0)
            .await?;
        let unread = messages.iter().filter(|m| !m.outbound && !m.read).count() as u64;
        out.push(InboxDto {
            key: meta.key,
            name: meta.name,
            address: meta.address,
            enabled: meta.enabled,
            unread,
        });
    }
    Ok(Json(out))
}

/// `GET …/inboxes/{key}/messages` — one inbox's mail, oldest first. Records
/// serialize camelCase (`fromEmail`, `atMillis`, …) matching the console.
async fn list_messages(
    company: ScopedCompany,
    Path(InboxKeyPath { key }): Path<InboxKeyPath>,
) -> Result<Json<Vec<EmailRecord>>, ApiError> {
    let messages = company
        .runtime
        .inbox()
        .messages(company.id(), &key, usize::MAX, 0)
        .await?;
    Ok(Json(messages))
}

async fn mark_read(
    company: ScopedCompany,
    Path(InboxKeyPath { key }): Path<InboxKeyPath>,
    body: Option<Json<MarkRead>>,
) -> Result<Json<UnreadCount>, ApiError> {
    let ids = body.and_then(|b| b.0.ids);
    let unread = company
        .runtime
        .inbox()
        .mark_read(company.id(), &key, ids.as_deref())
        .await?;
    Ok(Json(UnreadCount { unread }))
}
