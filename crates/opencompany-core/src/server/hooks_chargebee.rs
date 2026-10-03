//! The inbound Chargebee webhook: `POST /hooks/{company}/chargebee` (issue #788).
//!
//! Chargebee posts here when a payment succeeds or fails. A verified delivery
//! raises a [`CompanyEvent::WebhookReceived`] on the `chargebee` channel, which
//! drives one cycle — so the operator hears "Alan paid the $100 invoice" in
//! chat **without having asked**. That push is the whole point of this route.
//!
//! # Why this does not persist invoice state
//!
//! Issue #788's TC-03 describes the webhook updating stored state that the agent
//! then reads to answer "has Alan paid?". It deliberately does not do that.
//! `chargebee_get_invoice` already answers that question **live from
//! Chargebee**, and does it strictly better: stored state goes stale the moment
//! a delivery is dropped, retried, or replayed out of order, and then the agent
//! confidently reports a payment status that Chargebee disagrees with. A cache
//! that can silently diverge from the system of record is worse than no cache
//! when the subject is money.
//!
//! So the split is: **pull** stays live (`chargebee_get_invoice`), and this
//! route owns **push** — the thing a live read genuinely cannot do.
//!
//! # Verification comes before parsing
//!
//! Chargebee protects a webhook URL with HTTP Basic auth, configured beside the
//! URL in its dashboard. The handler compares that header, constant-time,
//! against the company's stored secret **before it parses anything**: an
//! unverifiable POST is dropped with `401` and never becomes an event. Same
//! order, and the same reason, as the Telegram hook next door.
//!
//! # The event names are Chargebee's, not the issue's
//!
//! #788 calls them `invoice_paid` and `invoice_payment_failed`. Chargebee has no
//! such events — the real ones are `payment_succeeded` and `payment_failed`
//! (plus `invoice_generated`). The names here are the ones an operator will
//! actually find in the Chargebee dashboard's webhook configuration.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyEvent;
use crate::server::ops::resolve;

pub use crate::company::billing::WEBHOOK_SECRET_KEY;

/// The channel a verified delivery is raised on.
pub const CHANNEL: &str = "chargebee";

/// The Chargebee events this route acts on.
///
/// An unlisted event is acknowledged and ignored rather than refused: Chargebee
/// sends whatever the dashboard subscribes to, an operator will over-subscribe,
/// and answering non-2xx would make Chargebee retry — then disable the endpoint
/// — over an event we simply had no interest in.
const ACTED_ON: &[&str] = &["payment_succeeded", "payment_failed", "invoice_generated"];

/// The most body this route will buffer, well above any real Chargebee event.
///
/// The second of two bounds, and the weaker one. [`VerifiedDelivery`] means an
/// unauthenticated caller's body is never read at all; this caps what a caller
/// who DID authenticate can make the host allocate. A Chargebee event is a few
/// KiB — an invoice with a long line-item list is the largest realistic case and
/// nowhere near this — so the 2 MiB axum defaults to is simply more room than
/// the endpoint has any use for.
const MAX_EVENT_BYTES: usize = 256 * 1024;

/// Builds the Chargebee webhook route fragment.
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/hooks/{company}/chargebee",
        post(chargebee_hook).layer(axum::extract::DefaultBodyLimit::max(MAX_EVENT_BYTES)),
    )
}

/// A `401` drop for an unverifiable delivery.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "invalid webhook credentials", "code": "unauthorized" })),
    )
        .into_response()
}

/// Decodes an `Authorization: Basic <base64>` header into `user:pass`.
///
/// Hand-rolled because `base64` is behind the `mcp` feature and this route ships
/// in every build. Decoding is 20 lines; taking a feature dependency for it
/// would make an always-on route conditional on an unrelated one.
fn decode_basic(header: &str) -> Option<String> {
    let encoded = header.strip_prefix("Basic ")?.trim();
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    // Structure first. An earlier version decoded until it met a `=` or an
    // unknown byte, so `QQ` and `QQ=garbage` both yielded a prefix that then
    // went into a credential comparison — a decoder that accepts more than it
    // should is a poor thing to put in front of an auth check, even when the
    // comparison itself would fail.
    let bytes = encoded.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if padding > 2 {
        return None;
    }
    let body = &bytes[..bytes.len() - padding];
    if body.iter().any(|b| !ALPHABET.contains(b)) {
        return None;
    }

    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out: Vec<u8> = Vec::new();
    for byte in body {
        let value = ALPHABET.iter().position(|c| c == byte)? as u32;
        bits = (bits << 6) | value;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    // Leftover bits must be zero in canonical base64; anything else means the
    // input was not produced by an encoder.
    if nbits > 0 && (bits & ((1 << nbits) - 1)) != 0 {
        return None;
    }
    String::from_utf8(out).ok()
}

/// A delivery whose credential has already been verified.
///
/// This is an **extractor**, not a check inside the handler, and the difference
/// is the point: axum runs every `FromRequestParts` extractor before the one
/// `FromRequest` extractor that consumes the body, so an unverifiable POST is
/// rejected while its body is still on the socket. With the check inside the
/// handler, `Bytes` had already buffered whatever an unauthenticated caller
/// chose to send — bounded by the route's `DefaultBodyLimit`, but bounded is
/// not the same as never read.
///
/// Carrying the runtime in the type also means the handler cannot forget: there
/// is no path to the body that does not go through a verified credential.
struct VerifiedDelivery(Arc<CompanyRuntime>);

impl axum::extract::FromRequestParts<AppState> for VerifiedDelivery {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let Path(company) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let runtime = resolve(state, &company).map_err(IntoResponse::into_response)?;
        verify(&runtime, &parts.headers)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Self(runtime))
    }
}

/// Compares the delivery's HTTP Basic credential against the company's stored
/// one, in constant time.
///
/// A stored credential must exist to verify against. An empty stored value
/// counts as "not configured" — reject rather than accept anything.
async fn verify(
    runtime: &Arc<CompanyRuntime>,
    headers: &HeaderMap,
) -> std::result::Result<(), crate::server::Rejection> {
    let expected = match runtime
        .secrets()
        .get(runtime.id(), WEBHOOK_SECRET_KEY)
        .await
    {
        Ok(Some(secret)) if !secret.expose().is_empty() => secret.expose().to_string(),
        Ok(_) => return Err(unauthorized().into()),
        Err(err) => return Err(crate::server::error::ApiError(err).into_response().into()),
    };

    let Some(provided) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(decode_basic)
    else {
        return Err(unauthorized().into());
    };
    if !bool::from(subtle::ConstantTimeEq::ct_eq(
        provided.as_bytes(),
        expected.as_bytes(),
    )) {
        return Err(unauthorized().into());
    }
    Ok(())
}

/// `POST /hooks/{company}/chargebee`.
///
/// `VerifiedDelivery` comes first deliberately — it is the extractor that
/// authenticates, and `raw` is only read once it has succeeded.
async fn chargebee_hook(VerifiedDelivery(runtime): VerifiedDelivery, raw: Bytes) -> Response {
    handle(runtime, &raw).await
}

/// Raises one cycle for an event worth telling the operator about.
///
/// The credential is already verified — see [`VerifiedDelivery`].
async fn handle(runtime: Arc<CompanyRuntime>, raw: &[u8]) -> Response {
    let Ok(event) = serde_json::from_slice::<Value>(raw) else {
        // Malformed body from a caller that DID authenticate: accept it so
        // Chargebee stops retrying, and say so in the log rather than silently.
        tracing::warn!(company = %runtime.id().as_ref(), "[chargebee] webhook body was not JSON");
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "ignored": "unparseable"})),
        )
            .into_response();
    };

    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !ACTED_ON.contains(&event_type.as_str()) {
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "ignored": event_type})),
        )
            .into_response();
    }

    let summary = summarize(&event_type, &event);
    tracing::info!(company = %runtime.id().as_ref(), %event_type, "[chargebee] webhook");

    // Drive one cycle so the company *says* something. A paused or archived
    // company acknowledges without running — Chargebee must not retry because
    // the operator happened to have the company stopped.
    if runtime.ensure_running().await.is_ok() {
        let event = CompanyEvent::WebhookReceived {
            channel: CHANNEL.to_string(),
            // The summary, not the raw event: `body` reaches the brain, and a
            // whole Chargebee payload spends a great deal of context to say
            // "Alan paid". The original is still in the log line above.
            body: json!({"event_type": event_type, "summary": summary}),
        };
        if let Err(err) = runtime.run_cycle(vec![event]).await {
            tracing::warn!(company = %runtime.id(), "chargebee cycle failed: {err}");
        }
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

/// Renders the event as the sentence the agent is asked to relay.
///
/// A summary rather than the raw payload: a Chargebee event carries the whole
/// invoice, customer, transaction and card objects, and handing that to a model
/// spends a large amount of context to say "Alan paid". Amounts stay in minor
/// units with the currency beside them — this text reaches a model, and a bare
/// `10000` with no unit is exactly how a $100 payment gets reported as $10,000.
fn summarize(event_type: &str, event: &Value) -> String {
    let content = event.get("content").cloned().unwrap_or(Value::Null);
    let invoice = content.get("invoice");
    let field = |obj: Option<&Value>, key: &str| -> Option<String> {
        obj?.get(key).and_then(Value::as_str).map(str::to_string)
    };
    let id = field(invoice, "id").unwrap_or_else(|| "(unknown)".to_string());
    let currency = field(invoice, "currency_code").unwrap_or_default();
    let total = invoice
        .and_then(|i| i.get("total"))
        .and_then(Value::as_i64)
        .map(|t| format!("{t} {currency} (minor units)"))
        .unwrap_or_else(|| "an unknown amount".to_string());
    // The customer id, never their email. This string is persisted in the
    // company journal and replayed into model prompts, so a counterparty's
    // address would outlive the notification it was needed for. The id is
    // sufficient to look them up in Chargebee.
    let who = field(content.get("customer"), "id").unwrap_or_else(|| "the customer".to_string());

    match event_type {
        "payment_succeeded" => format!(
            "Chargebee: invoice {id} for {total} was PAID by {who}. Tell the operator, briefly."
        ),
        "payment_failed" => format!(
            "Chargebee: a payment FAILED for invoice {id} ({total}) from {who}. Tell the operator, \
             briefly, and say the invoice is still outstanding."
        ),
        _ => format!("Chargebee: invoice {id} for {total} was generated for {who}."),
    }
}

#[cfg(test)]
#[path = "hooks_chargebee_tests.rs"]
mod tests;
