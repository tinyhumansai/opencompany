//! REST inbox read surface: `GET …/inboxes` and `GET …/inboxes/{key}/messages`.
//!
//! A REST twin of the `Company.inboxes` GraphQL resolver
//! ([`graphql::inbox`](crate::server::graphql::inbox)). The operator console is
//! REST-only and ships no GraphQL client, so the Inbox view had no way to reach
//! the real store and rendered a client-side fixture instead — the same four
//! seeded emails for every teammate (issue #173). These routes expose the real
//! per-agent [`InboxStore`](crate::ports::InboxStore) content that the ingest
//! webhook ([`inbox`](super::inbox)), the mailbox poller, and every outbound
//! send already append, so each teammate's inbox shows its own correspondence.
//!
//! Read-only. Read-state writes stay in [`mail`](super::mail)
//! (`POST …/inboxes/{key}/read`) and the enable toggle in [`team`](super::team)
//! (`PUT …/team/{agentId}/inbox`).
//!
//! Every inbox with metadata is listed — including an enabled-but-empty one —
//! so the console can render a teammate's inbox the moment it is switched on.
//! An unknown key reads as an empty page rather than a 404, the same soft-fail
//! the sibling read routes use.

use axum::extract::{Path, Query};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::ports::inbox::EmailRecord;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The default page size when `?limit=` is omitted — matches the GraphQL
/// resolver's `first` default.
const DEFAULT_LIMIT: usize = 50;

/// Builds the inbox read route fragment (both scope forms).
pub fn router() -> Router<AppState> {
    scoped("/inboxes", get(list_inboxes))
        .merge(scoped("/inboxes/{key}/messages", get(list_messages)))
}

/// One teammate inbox as the console lists it. Mirrors the GraphQL `Inbox`
/// object's scalar fields (`key`/`name`/`address`/`unread`) plus the `enabled`
/// flag the Team toggle writes and a `total` message count, so the console can
/// render the selector without a per-inbox message fetch.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InboxDto {
    /// The inbox key — the teammate's agent id / local part.
    key: String,
    /// The teammate's display name.
    name: String,
    /// The full address (`{key}@{domain}`), empty until a domain is configured.
    address: String,
    /// Whether the inbox is receiving mail (the Team page toggle).
    enabled: bool,
    /// Unread *received* mail — outbound copies are never unread.
    unread: usize,
    /// Every message filed under this inbox, inbound and outbound.
    total: usize,
}

/// A page of one inbox's mail. `total` is the unpaginated count so the console
/// can page without a second request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessagesPage {
    /// The page's messages, newest first.
    items: Vec<EmailRecord>,
    /// How many messages the inbox holds in total.
    total: usize,
}

/// Query params for `GET …/inboxes/{key}/messages`.
#[derive(Debug, Deserialize)]
struct PageQuery {
    /// Page size; defaults to [`DEFAULT_LIMIT`].
    #[serde(default = "default_limit")]
    limit: usize,
    /// How many messages to skip, counting from the newest.
    #[serde(default)]
    offset: usize,
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

/// The sub-resource path (`key`).
#[derive(Debug, Deserialize)]
struct InboxKeyPath {
    key: String,
}

/// `GET …/inboxes` — every inbox this company owns, with its unread count.
///
/// Mirrors [`graphql::inbox::resolve`](crate::server::graphql::inbox): unread
/// counts received mail only (`!outbound && !read`), so a sent copy sitting in
/// the same thread never inflates the badge.
async fn list_inboxes(company: ScopedCompany) -> Result<Json<Vec<InboxDto>>, ApiError> {
    let inbox = company.runtime.inbox();
    let metas = inbox.inboxes(company.id()).await?;
    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let messages = inbox
            .messages(company.id(), &meta.key, usize::MAX, 0)
            .await?;
        out.push(InboxDto {
            key: meta.key,
            name: meta.name,
            address: meta.address,
            enabled: meta.enabled,
            unread: messages.iter().filter(|m| !m.outbound && !m.read).count(),
            total: messages.len(),
        });
    }
    tracing::debug!(
        company = %company.id(),
        inboxes = out.len(),
        "[inbox] listed per-agent inboxes"
    );
    Ok(Json(out))
}

/// `GET …/inboxes/{key}/messages?limit=&offset=` — one teammate's mail.
///
/// **Newest first.** The [`InboxStore`](crate::ports::InboxStore) returns mail
/// oldest-first (append order); this read reverses it so a `limit` keeps the
/// newest mail rather than the oldest — an inbox that has outgrown one page
/// must still open on today's messages.
async fn list_messages(
    company: ScopedCompany,
    Path(InboxKeyPath { key }): Path<InboxKeyPath>,
    Query(PageQuery { limit, offset }): Query<PageQuery>,
) -> Result<Json<MessagesPage>, ApiError> {
    // Read the whole inbox: the store pages in append order, and the page has
    // to be cut from the newest end (plus `total` is the unpaginated count).
    let mut all = company
        .runtime
        .inbox()
        .messages(company.id(), &key, usize::MAX, 0)
        .await?;
    let total = all.len();
    all.sort_by_key(|m| std::cmp::Reverse(m.at_millis));
    let items: Vec<EmailRecord> = all.into_iter().skip(offset).take(limit).collect();
    tracing::debug!(
        company = %company.id(),
        inbox = %key,
        returned = items.len(),
        total,
        offset,
        limit,
        "[inbox] read one inbox's messages"
    );
    Ok(Json(MessagesPage { items, total }))
}
