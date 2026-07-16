//! Inbound email ingest transport.
//!
//! `POST …/inboxes/ingest` is the HMAC-signed webhook a mail-forwarding
//! provider (or, in managed deployments, the platform manager that owns MX)
//! pushes received mail into — there is no IMAP polling in v1. The request body
//! is verified against the per-company ingest secret in
//! [`SecretStore`](crate::ports::SecretStore) using the same signer seam as the
//! platform webhooks; an unverifiable payload is dropped with `401` and never
//! becomes an event.
//!
//! A verified payload is appended to the addressed teammate's
//! [`InboxStore`](crate::ports::InboxStore) and drives one cycle with a
//! [`CompanyEvent::WebhookReceived`](crate::ports::types::CompanyEvent) on the
//! `email` channel so the teammate can act on the mail.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::ports::inbox::EmailRecord;
use crate::ports::types::CompanyEvent;
use crate::ports::{generate_id, now_millis};
use crate::server::ops::smtp::local_part;
use crate::server::ops::{INGEST_SECRET_KEY, resolve, resolve_sole};
use crate::server::webhook::WebhookSigner;

/// The header carrying the ingest HMAC signature.
const SIGNATURE_HEADER: &str = "x-opencompany-signature";

/// Builds the ingest route fragment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/companies/{id}/inboxes/ingest", post(ingest_by_id))
        .route("/api/v1/company/inboxes/ingest", post(ingest_single))
}

/// The inbound email payload a forwarding provider pushes in.
#[derive(Debug, Deserialize)]
struct InboundEmail {
    /// Sender address.
    from: String,
    /// Recipient address (`{agent_id}@{domain}`); its local part selects the inbox.
    to: String,
    /// Subject line.
    #[serde(default)]
    subject: String,
    /// Plain-text body.
    #[serde(default)]
    body: String,
}

/// The ingest acknowledgement.
#[derive(Debug, Serialize)]
struct IngestAck {
    /// Always `true` on a verified, accepted message.
    ok: bool,
    /// The inbox the message was filed under.
    inbox: String,
}

/// The default-build / feature signer used to verify the ingest HMAC. Matches
/// the platform-webhook signer so a provider computes the same value.
fn signer() -> Box<dyn WebhookSigner> {
    #[cfg(feature = "webhooks")]
    {
        Box::new(crate::server::webhook::HmacSha256Signer)
    }
    #[cfg(not(feature = "webhooks"))]
    {
        Box::new(crate::server::webhook::DefaultHashSigner)
    }
}

/// A `401` drop for an unverifiable payload.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "invalid ingest signature", "code": "unauthorized" })),
    )
        .into_response()
}

/// Verifies the HMAC, files the mail, and drives a cycle.
async fn ingest(runtime: Arc<CompanyRuntime>, headers: &HeaderMap, raw: &[u8]) -> Response {
    // The provided signature header must be present.
    let Some(provided) = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        return unauthorized();
    };

    // The per-company ingest secret must exist to verify against.
    let secret = match runtime.secrets().get(runtime.id(), INGEST_SECRET_KEY).await {
        Ok(Some(secret)) => secret.expose().to_string(),
        Ok(None) => return unauthorized(),
        Err(err) => return crate::server::error::ApiError(err).into_response(),
    };

    let expected = signer().sign(&secret, raw);
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return unauthorized();
    }

    // Only parse the body after the signature checks out.
    let email: InboundEmail = match serde_json::from_slice(raw) {
        Ok(email) => email,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid ingest payload: {err}"), "code": "invalid_request" })),
            )
                .into_response();
        }
    };

    let inbox = local_part(&email.to);
    let record = EmailRecord {
        id: generate_id(),
        inbox: inbox.clone(),
        from_name: String::new(),
        from_email: email.from.clone(),
        subject: email.subject.clone(),
        body: email.body.clone(),
        at_millis: now_millis(),
        read: false,
        outbound: false,
    };
    if let Err(err) = runtime.inbox().append(runtime.id(), &record).await {
        return crate::server::error::ApiError(err).into_response();
    }

    // Drive one cycle so the addressed teammate can act on the mail. A paused or
    // archived company simply files the mail without running.
    if runtime.ensure_running().await.is_ok() {
        let event = CompanyEvent::WebhookReceived {
            channel: "email".to_string(),
            body: json!({
                "from": email.from,
                "to": email.to,
                "inbox": inbox,
                "subject": email.subject,
                "body": email.body,
            }),
        };
        if let Err(err) = runtime.run_cycle(vec![event]).await {
            tracing::warn!(company = %runtime.id(), "ingest cycle failed: {err}");
        }
    }

    (StatusCode::ACCEPTED, Json(IngestAck { ok: true, inbox })).into_response()
}

/// A length-checked, branch-independent byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `POST /api/v1/companies/{id}/inboxes/ingest`.
async fn ingest_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    match resolve(&state, &id) {
        Ok(runtime) => ingest(runtime, &headers, &raw).await,
        Err(err) => err.into_response(),
    }
}

/// `POST /api/v1/company/inboxes/ingest` (single-company alias).
async fn ingest_single(State(state): State<AppState>, headers: HeaderMap, raw: Bytes) -> Response {
    match resolve_sole(&state) {
        Ok(runtime) => ingest(runtime, &headers, &raw).await,
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
