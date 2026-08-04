//! The Telegram channel configuration write-plane: store the bot token +
//! (optional) webhook secret — both **write-only** — and register the webhook.
//!
//! `GET …/channels/telegram` returns only the non-secret
//! [`TelegramChannelStatus`]. Neither the bot token nor the webhook secret is
//! ever serialized into any response, by construction: they live in
//! [`SecretStore`](crate::ports::SecretStore) as raw strings under
//! [`TELEGRAM_TOKEN_KEY`] / [`TELEGRAM_SECRET_KEY`] and this module reads them
//! back only to *use* them (verify, deliver, `setWebhook`), never to echo them.
//!
//! ## A bot token is the whole setup (issue #203)
//!
//! Inbound arrives by `getUpdates` long-polling
//! ([`crate::runtime::telegram_poller`]), which dials out and therefore needs
//! no public URL — so [`TelegramChannelStatus::configured`] tracks the **bot
//! token alone**. The webhook is an optional hosted fast-path, and this module
//! surfaces it *only* on a host that Telegram could actually deliver to (see
//! [`AppConfig::public_webhook_base_url`](crate::app::AppConfig::public_webhook_base_url)).
//! On a local or self-hosted instance `webhookUrl` is `null` and `setWebhook`
//! is refused with an explanation, rather than handing the operator a
//! `http://127.0.0.1:<port>/hooks/…` URL that can never receive a delivery.
//!
//! `PUT …/channels/telegram` is a partial write: only the fields present in the
//! body are updated, so an operator can rotate the secret without re-entering
//! the token. `DELETE …/channels/telegram` clears both. `POST
//! …/channels/telegram/webhook` calls Telegram `setWebhook` through the injected
//! [`TelegramApi`](crate::company::telegram::TelegramApi) (the offline build
//! answers "not wired").

use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::company::telegram::{TELEGRAM_SECRET_KEY, TELEGRAM_TOKEN_KEY, scrub_token};
use crate::ports::types::{CompanyId, SecretValue};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The non-secret status of a company's Telegram channel. Carries presence
/// booleans, how inbound arrives, and the webhook URL when there is a usable
/// one — never the token or the secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramChannelStatus {
    /// True once a bot token is stored. That alone is a working channel:
    /// inbound arrives by `getUpdates` long-polling, which needs no public URL
    /// and no webhook secret.
    pub configured: bool,
    /// Whether a bot token is stored (never the token itself).
    pub token_set: bool,
    /// Whether a webhook secret is stored (never the secret itself). Only
    /// meaningful on a host that can serve the webhook fast-path.
    pub secret_set: bool,
    /// The URL to register with Telegram (`setWebhook`), or `null` when this
    /// host has no publicly reachable https URL for Telegram to deliver to —
    /// which is every local and most self-hosted deployments. Non-secret when
    /// present: it embeds only the public host and the company id.
    pub webhook_url: Option<String>,
    /// Whether this host long-polls Telegram for inbound updates — true
    /// whenever an outbound Telegram transport is wired. The default offline
    /// build has none, so it neither polls nor delivers.
    pub polling: bool,
}

/// The webhook URL Telegram should deliver this company's updates to, or `None`
/// when this host has no publicly reachable https base URL.
///
/// Deliberately **not** derived from
/// [`host_base_url`](crate::app::AppConfig::host_base_url): that falls back to
/// the bind address, which produced the `http://127.0.0.1:<port>/hooks/…` URL
/// of issue #203 — syntactically fine, undeliverable in practice.
fn webhook_url(state: &AppState, company: &CompanyId) -> Option<String> {
    let base = state.config().public_webhook_base_url()?;
    Some(format!("{base}/hooks/{}/telegram", company.as_ref()))
}

/// Reads a stored secret and reports whether it is present and non-empty.
async fn is_set(runtime: &CompanyRuntime, key: &str) -> Result<bool, ApiError> {
    Ok(runtime
        .secrets()
        .get(runtime.id(), key)
        .await?
        .map(|v| !v.expose().is_empty())
        .unwrap_or(false))
}

/// Builds the current status for a company.
async fn status_of(
    state: &AppState,
    runtime: &CompanyRuntime,
) -> Result<TelegramChannelStatus, ApiError> {
    let token_set = is_set(runtime, TELEGRAM_TOKEN_KEY).await?;
    let secret_set = is_set(runtime, TELEGRAM_SECRET_KEY).await?;
    Ok(TelegramChannelStatus {
        // The bot token alone is a working channel — polling covers inbound.
        configured: token_set,
        token_set,
        secret_set,
        webhook_url: webhook_url(state, runtime.id()),
        polling: state.connections().telegram.is_some(),
    })
}

/// Builds the Telegram channel route fragment.
pub fn router() -> Router<AppState> {
    scoped(
        "/channels/telegram",
        get(get_channel).put(put_channel).delete(delete_channel),
    )
    .merge(scoped("/channels/telegram/webhook", post(set_webhook)))
}

// -- GET --------------------------------------------------------------------

/// `GET …/channels/telegram` — non-secret status only.
async fn get_channel(
    company: ScopedCompany,
    State(state): State<AppState>,
) -> Result<Json<TelegramChannelStatus>, ApiError> {
    Ok(Json(status_of(&state, &company.runtime).await?))
}

// -- PUT --------------------------------------------------------------------

/// The write-only config body. Every field is optional; only fields present and
/// non-empty are applied, so the token can stay put while the secret rotates.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TelegramConfigBody {
    /// The bot token from BotFather (write-only). Omit to leave it unchanged.
    #[serde(default)]
    bot_token: Option<String>,
    /// The webhook secret token (write-only). Omit to leave it unchanged.
    #[serde(default)]
    webhook_secret: Option<String>,
}

/// `PUT …/channels/telegram` — store token and/or secret, return status.
async fn put_channel(
    company: ScopedCompany,
    State(state): State<AppState>,
    Json(body): Json<TelegramConfigBody>,
) -> Result<Json<TelegramChannelStatus>, ApiError> {
    let runtime = &company.runtime;
    if let Some(token) = body
        .bot_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        runtime
            .secrets()
            .set(
                runtime.id(),
                TELEGRAM_TOKEN_KEY,
                SecretValue(token.to_string()),
            )
            .await?;
    }
    if let Some(secret) = body
        .webhook_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        runtime
            .secrets()
            .set(
                runtime.id(),
                TELEGRAM_SECRET_KEY,
                SecretValue(secret.to_string()),
            )
            .await?;
    }
    Ok(Json(status_of(&state, runtime).await?))
}

// -- DELETE -----------------------------------------------------------------

/// `DELETE …/channels/telegram` — clear both credentials.
///
/// The [`SecretStore`](crate::ports::SecretStore) port has no delete, so a
/// cleared credential is stored as the empty string; every read site treats an
/// empty value as "unset" (the webhook rejects a blank secret, delivery skips a
/// blank token).
async fn delete_channel(
    company: ScopedCompany,
    State(state): State<AppState>,
) -> Result<Json<TelegramChannelStatus>, ApiError> {
    let runtime = &company.runtime;
    runtime
        .secrets()
        .set(runtime.id(), TELEGRAM_TOKEN_KEY, SecretValue(String::new()))
        .await?;
    runtime
        .secrets()
        .set(
            runtime.id(),
            TELEGRAM_SECRET_KEY,
            SecretValue(String::new()),
        )
        .await?;
    Ok(Json(status_of(&state, runtime).await?))
}

// -- POST webhook -----------------------------------------------------------

/// The `setWebhook` outcome. Carries the (non-secret) URL, never a credential.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetWebhookResult {
    /// Whether Telegram accepted the registration.
    ok: bool,
    /// A prosumer-friendly description of the outcome.
    message: String,
    /// The URL that was registered, or `null` when there was none to register.
    webhook_url: Option<String>,
}

/// `POST …/channels/telegram/webhook` — register the webhook with Telegram.
///
/// The hosted fast-path only. Refused outright on a host with no publicly
/// reachable https URL (issue #203): Telegram would accept nothing, or worse
/// accept a URL it can never deliver to — which also blocks `getUpdates` and so
/// would take the working polling path down with it.
async fn set_webhook(company: ScopedCompany, State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;

    let runtime = &company.runtime;
    let Some(url) = webhook_url(&state, runtime.id()) else {
        return unreachable_host().into_response();
    };

    // Not wired without a transport (default build / no `telegram` feature).
    let Some(api) = state.connections().telegram.clone() else {
        return super::not_wired("telegram setWebhook");
    };

    // Both credentials must be present to register a verifiable webhook.
    let token = match load_secret(runtime, TELEGRAM_TOKEN_KEY).await {
        Ok(Some(token)) => token,
        Ok(None) => return missing("a bot token").into_response(),
        Err(err) => return err.into_response(),
    };
    let secret = match load_secret(runtime, TELEGRAM_SECRET_KEY).await {
        Ok(Some(secret)) => secret,
        Ok(None) => return missing("a webhook secret").into_response(),
        Err(err) => return err.into_response(),
    };

    match api.set_webhook(&token, &url, &secret).await {
        Ok(()) => Json(SetWebhookResult {
            ok: true,
            message: "Webhook registered with Telegram.".to_string(),
            webhook_url: Some(url),
        })
        .into_response(),
        Err(err) => Json(SetWebhookResult {
            ok: false,
            // Scrub the token in case the transport error echoed the API URL.
            message: format!(
                "setWebhook failed: {}",
                scrub_token(&err.to_string(), &token)
            ),
            webhook_url: Some(url),
        })
        .into_response(),
    }
}

/// Loads a stored secret, treating an empty value as unset.
async fn load_secret(runtime: &CompanyRuntime, key: &str) -> Result<Option<String>, ApiError> {
    Ok(runtime
        .secrets()
        .get(runtime.id(), key)
        .await?
        .map(|v| v.expose().to_string())
        .filter(|v| !v.is_empty()))
}

/// A `400` for a `setWebhook` attempt before the channel is fully configured.
fn missing(what: &str) -> ApiError {
    ApiError(crate::error::OpenCompanyError::InvalidRequest(format!(
        "telegram channel needs {what} before its webhook can be set"
    )))
}

/// A `400` for a `setWebhook` attempt on a host Telegram cannot reach.
fn unreachable_host() -> ApiError {
    ApiError(crate::error::OpenCompanyError::InvalidRequest(
        "telegram cannot deliver a webhook to this host: set OPENCOMPANY_PUBLIC_URL to a public \
         https URL first. Inbound already works without one — the bot token alone enables \
         getUpdates polling."
            .to_string(),
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn status_never_serializes_a_credential() {
        let status = TelegramChannelStatus {
            configured: true,
            token_set: true,
            secret_set: true,
            webhook_url: Some("https://host/hooks/acme/telegram".to_string()),
            polling: true,
        };
        let json = serde_json::to_string(&status).unwrap();
        // Presence booleans, the mode, and the URL only — no field can carry
        // secret bytes.
        assert!(json.contains("\"tokenSet\":true"));
        assert!(json.contains("hooks/acme/telegram"));
        assert!(!json.contains("bot_token"));
        assert!(!json.contains("webhook_secret"));
    }

    /// Issue #203: a host with no public https URL must report `null` rather
    /// than a loopback URL Telegram can never deliver to.
    #[test]
    fn status_omits_the_webhook_url_on_an_unreachable_host() {
        let local = TelegramChannelStatus {
            configured: true,
            token_set: true,
            secret_set: false,
            webhook_url: None,
            polling: true,
        };
        let json = serde_json::to_string(&local).unwrap();
        assert!(json.contains("\"webhookUrl\":null"), "{json}");
        assert!(!json.contains("127.0.0.1"), "{json}");
        // The channel is still fully configured — polling covers inbound.
        assert!(json.contains("\"configured\":true"));
        assert!(json.contains("\"polling\":true"));
    }
}
