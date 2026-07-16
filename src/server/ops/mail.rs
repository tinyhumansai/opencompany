//! Mailbox read-state writes: `POST /inboxes/{key}/read` under both scope forms.
//!
//! Marks messages in an inbox read (the given `ids`, or all when omitted) and
//! returns the count still unread. Delivery/ingest lives in
//! [`inbox`](super::inbox); message metadata lands in the
//! [`InboxStore`](crate::ports::InboxStore).

use axum::extract::Path;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the mailbox read-state route fragment.
pub fn router() -> Router<AppState> {
    scoped("/inboxes/{key}/read", post(mark_read))
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
