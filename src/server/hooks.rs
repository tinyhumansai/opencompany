//! Inbound channel webhooks: `POST /hooks/{company}/{channel}`.
//!
//! Today the one wired channel is Telegram. Telegram delivers each bot update
//! to `POST /hooks/{company}/telegram` with an
//! `X-Telegram-Bot-Api-Secret-Token` header carrying the secret configured via
//! `setWebhook`. The handler verifies that header (constant-time) against the
//! company's stored webhook secret **before parsing anything**; an unverifiable
//! POST is dropped with `401` and never becomes an event.
//!
//! A verified update drives one cycle with a
//! [`CompanyEvent::WebhookReceived`](crate::ports::types::CompanyEvent) on the
//! `telegram` channel — the brain routes it as a web/chat turn and addresses the
//! reply back to the origin chat via
//! [`OutboundMessage::reply_to`](crate::ports::types::OutboundMessage). The
//! handler then delivers each telegram reply through the injected
//! [`TelegramApi`](crate::company::telegram::TelegramApi), reading the bot token
//! from [`SecretStore`](crate::ports::SecretStore). The token is never logged:
//! any transport error is passed through
//! [`scrub_token`](crate::company::telegram::scrub_token) first.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::company::telegram::{
    SECRET_TOKEN_HEADER, TELEGRAM_SECRET_KEY, TELEGRAM_TOKEN_KEY, scrub_token,
};
use crate::ports::types::CompanyEvent;
use crate::server::ops::resolve;

/// Builds the inbound-webhook route fragment.
pub fn router() -> Router<AppState> {
    Router::new().route("/hooks/{company}/telegram", post(telegram_hook))
}

/// A `401` drop for an unverifiable webhook.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "invalid webhook secret", "code": "unauthorized" })),
    )
        .into_response()
}

/// Length-checked, branch-independent byte comparison for the secret token.
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

/// `POST /hooks/{company}/telegram`.
async fn telegram_hook(
    State(state): State<AppState>,
    Path(company): Path<String>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let runtime = match resolve(&state, &company) {
        Ok(runtime) => runtime,
        Err(err) => return err.into_response(),
    };
    handle_telegram(&state, runtime, &headers, &raw).await
}

/// Verifies the secret, runs the turn, and delivers the reply back to Telegram.
async fn handle_telegram(
    state: &AppState,
    runtime: Arc<CompanyRuntime>,
    headers: &HeaderMap,
    raw: &[u8],
) -> Response {
    // The stored webhook secret must exist to verify against. An empty stored
    // value counts as "no secret configured" — reject rather than accept blank.
    let secret = match runtime
        .secrets()
        .get(runtime.id(), TELEGRAM_SECRET_KEY)
        .await
    {
        Ok(Some(secret)) if !secret.expose().is_empty() => secret.expose().to_string(),
        Ok(_) => return unauthorized(),
        Err(err) => return crate::server::error::ApiError(err).into_response(),
    };

    // The header Telegram sends with every delivery must match, constant-time.
    let Some(provided) = headers
        .get(SECRET_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return unauthorized();
    };
    if !constant_time_eq(provided.as_bytes(), secret.as_bytes()) {
        return unauthorized();
    }

    // Only parse the body after the secret checks out. A non-JSON body is a
    // malformed delivery; a well-formed update with no actionable text (a
    // sticker, a channel post) simply produces no telegram reply below.
    let update: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(value) => value,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid telegram update: {err}"),
                    "code": "invalid_request",
                })),
            )
                .into_response();
        }
    };

    // Drive one cycle. A paused/archived company acknowledges without running.
    let mut delivered = 0usize;
    if runtime.ensure_running().await.is_ok() {
        let event = CompanyEvent::WebhookReceived {
            channel: crate::company::telegram::TELEGRAM_CHANNEL.to_string(),
            body: update,
        };
        match runtime.run_cycle(vec![event]).await {
            Ok(report) => delivered = deliver_replies(state, &runtime, &report.responses).await,
            Err(err) => {
                tracing::warn!(company = %runtime.id(), "telegram cycle failed: {err}");
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "ok": true, "delivered": delivered })),
    )
        .into_response()
}

/// Posts every `telegram`-channel reply back to its origin chat. Returns how
/// many were delivered. Missing token / unwired transport / a bad chat id are
/// logged and skipped — the turn already ran, so a delivery gap never fails the
/// webhook. Any transport error is scrubbed of the bot token before logging.
async fn deliver_replies(
    state: &AppState,
    runtime: &CompanyRuntime,
    responses: &[crate::ports::types::OutboundMessage],
) -> usize {
    use crate::company::telegram::TELEGRAM_CHANNEL;

    // Nothing to deliver: skip the token/transport lookups entirely.
    if !responses
        .iter()
        .any(|m| m.channel == TELEGRAM_CHANNEL && m.reply_to.is_some())
    {
        return 0;
    }

    let Some(api) = state.connections().telegram.clone() else {
        tracing::warn!(
            company = %runtime.id(),
            "telegram reply produced but no transport is wired; not delivered"
        );
        return 0;
    };

    let token = match runtime
        .secrets()
        .get(runtime.id(), TELEGRAM_TOKEN_KEY)
        .await
    {
        Ok(Some(token)) if !token.expose().is_empty() => token.expose().to_string(),
        _ => {
            tracing::warn!(
                company = %runtime.id(),
                "telegram reply produced but no bot token is configured; not delivered"
            );
            return 0;
        }
    };

    let mut delivered = 0usize;
    for msg in responses {
        if msg.channel != TELEGRAM_CHANNEL {
            continue;
        }
        let Some(reply_to) = &msg.reply_to else {
            continue;
        };
        let Ok(chat_id) = reply_to.chat_id.parse::<i64>() else {
            tracing::warn!(
                company = %runtime.id(),
                "telegram reply has a non-numeric chat id; skipped"
            );
            continue;
        };
        match api.send_message(&token, chat_id, &msg.text).await {
            Ok(()) => delivered += 1,
            Err(err) => {
                // Never let the bot token reach the log line.
                tracing::warn!(
                    company = %runtime.id(),
                    "telegram delivery failed: {}",
                    scrub_token(&err.to_string(), &token)
                );
            }
        }
    }
    delivered
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }
}
