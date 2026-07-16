//! OAuth connection lifecycle: start, callback, disconnect.
//!
//! Gated behind the `oauth` feature because token exchange needs `reqwest`;
//! without it these routes are absent (404) and the console shows the read-only
//! connections catalog. Provider **app** credentials (client id/secret) are
//! host-level configuration read from the environment
//! (`OPENCOMPANY_OAUTH_<PROVIDER>_ID` / `_SECRET`); per-company state is tokens
//! only, and those live in [`SecretStore`](crate::ports::SecretStore) under
//! `oauth/{provider}` — token material never appears in any response.
//!
//! The authorize URL carries a signed `state` nonce binding the flow to one
//! company + provider + expiry, verified on callback so a tampered `state` is
//! rejected with `401`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::ports::types::{CompanyId, SecretValue};
use crate::server::error::ApiError;
use crate::server::operator::OperatorAuth;
use crate::server::ops::{oauth_key, resolve, resolve_sole};
use crate::server::webhook::{DefaultHashSigner, WebhookSigner};

/// How long a signed `state` nonce stays valid.
const STATE_TTL_MS: u64 = 10 * 60 * 1000;

/// Builds the OAuth route fragment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/companies/{id}/connections/{provider}/start",
            post(start),
        )
        .route(
            "/api/v1/companies/{id}/connections/{provider}/disconnect",
            post(disconnect),
        )
        .route(
            "/api/v1/company/connections/{provider}/start",
            post(start_single),
        )
        .route(
            "/api/v1/company/connections/{provider}/disconnect",
            post(disconnect_single),
        )
        // The callback is unscoped: the signed `state` carries the company id.
        .route("/api/v1/oauth/callback", get(callback))
}

// ---------------------------------------------------------------------------
// Provider app configuration (host-level env)
// ---------------------------------------------------------------------------

/// A provider's OAuth endpoints and host-level app credentials.
struct ProviderConfig {
    client_id: String,
    client_secret: String,
    authorize_url: String,
    token_url: String,
    default_scopes: String,
}

/// Well-known authorize/token URLs for the built-in providers; overridable per
/// provider via `OPENCOMPANY_OAUTH_<P>_AUTHORIZE_URL` / `_TOKEN_URL`.
fn well_known(provider: &str) -> Option<(&'static str, &'static str)> {
    match provider {
        "slack" => Some((
            "https://slack.com/oauth/v2/authorize",
            "https://slack.com/api/oauth.v2.access",
        )),
        "google" | "gmail" => Some((
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
        )),
        "github" => Some((
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
        )),
        _ => None,
    }
}

/// Resolves a provider's config from the environment, or `None` when the app
/// credentials are not configured (the provider is not enabled on this host).
fn provider_config(provider: &str) -> Option<ProviderConfig> {
    let key = provider.to_ascii_uppercase();
    let env = |suffix: &str| std::env::var(format!("OPENCOMPANY_OAUTH_{key}_{suffix}")).ok();
    let client_id = env("ID")?;
    let client_secret = env("SECRET")?;
    let (default_authorize, default_token) = well_known(provider).unwrap_or(("", ""));
    let authorize_url = env("AUTHORIZE_URL").unwrap_or_else(|| default_authorize.to_string());
    let token_url = env("TOKEN_URL").unwrap_or_else(|| default_token.to_string());
    if authorize_url.is_empty() || token_url.is_empty() {
        return None;
    }
    Some(ProviderConfig {
        client_id,
        client_secret,
        authorize_url,
        token_url,
        default_scopes: env("SCOPES").unwrap_or_default(),
    })
}

/// The redirect URI advertised to the provider. `OPENCOMPANY_OAUTH_REDIRECT_BASE`
/// overrides the origin so the authorize URL points where the operator's
/// browser can reach the callback (managed deployments front it via the manager).
fn redirect_uri(state: &AppState) -> String {
    let base = std::env::var("OPENCOMPANY_OAUTH_REDIRECT_BASE")
        .unwrap_or_else(|_| state.config().host_base_url());
    format!("{}/api/v1/oauth/callback", base.trim_end_matches('/'))
}

/// The host-level secret the `state` nonce is signed with.
fn state_secret() -> String {
    std::env::var("OPENCOMPANY_OAUTH_STATE_SECRET")
        .unwrap_or_else(|_| "opencompany-oauth-state".to_string())
}

// ---------------------------------------------------------------------------
// Signed state nonce
// ---------------------------------------------------------------------------

/// Encodes `company:provider:exp:sig` into an opaque `state` value.
fn encode_state(company: &str, provider: &str, exp: u64) -> String {
    let payload = format!("{company}:{provider}:{exp}");
    let sig = DefaultHashSigner.sign(&state_secret(), payload.as_bytes());
    format!("{payload}:{sig}")
}

/// Verifies and decodes a `state` value into `(company, provider)`, or `None`
/// when the signature is wrong or the nonce has expired.
fn decode_state(state: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = state.splitn(4, ':').collect();
    if parts.len() != 4 {
        return None;
    }
    let (company, provider, exp, sig) = (parts[0], parts[1], parts[2], parts[3]);
    let payload = format!("{company}:{provider}:{exp}");
    let expected = DefaultHashSigner.sign(&state_secret(), payload.as_bytes());
    if sig != expected {
        return None;
    }
    let exp: u64 = exp.parse().ok()?;
    if now_millis() > exp {
        return None;
    }
    Some((company.to_string(), provider.to_string()))
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/// The authorize-URL response.
#[derive(Debug, Serialize)]
struct StartResponse {
    /// The provider authorize URL the operator's browser should visit.
    url: String,
}

/// Builds the authorize URL for `provider` scoped to `company`.
fn build_authorize(
    state: &AppState,
    company: &CompanyId,
    provider: &str,
) -> Result<StartResponse, ApiError> {
    let Some(config) = provider_config(provider) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "provider '{provider}' is not enabled on this host"
        ))));
    };
    let nonce = encode_state(company.as_ref(), provider, now_millis() + STATE_TTL_MS);
    let redirect = redirect_uri(state);
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        config.authorize_url,
        urlencode(&config.client_id),
        urlencode(&redirect),
        urlencode(&config.default_scopes),
        urlencode(&nonce),
    );
    Ok(StartResponse { url })
}

/// `POST /api/v1/companies/{id}/connections/{provider}/start`.
async fn start(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, provider)): Path<(String, String)>,
) -> Result<Json<StartResponse>, ApiError> {
    let runtime = resolve(&state, &id)?;
    Ok(Json(build_authorize(&state, runtime.id(), &provider)?))
}

/// `POST /api/v1/company/connections/{provider}/start` (single-company alias).
async fn start_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<StartResponse>, ApiError> {
    let runtime = resolve_sole(&state)?;
    Ok(Json(build_authorize(&state, runtime.id(), &provider)?))
}

// ---------------------------------------------------------------------------
// Callback
// ---------------------------------------------------------------------------

/// Reads a single query parameter from a raw query string, percent-decoding it.
fn query_param(uri: &Uri, key: &str) -> Option<String> {
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Minimal percent-decode (and `+` → space) for query values.
fn percent_decode(value: &str) -> String {
    let bytes = value.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /api/v1/oauth/callback` — verify state, exchange code, store tokens.
async fn callback(State(state): State<AppState>, uri: Uri) -> Response {
    if let Some(err) = query_param(&uri, "error") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("provider returned: {err}"), "code": "oauth_error" })),
        )
            .into_response();
    }
    let (Some(code), Some(raw_state)) = (query_param(&uri, "code"), query_param(&uri, "state"))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing code or state", "code": "invalid_request" })),
        )
            .into_response();
    };
    // A tampered or expired `state` is rejected before any exchange.
    let Some((company, provider)) = decode_state(&raw_state) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid oauth state", "code": "unauthorized" })),
        )
            .into_response();
    };
    let Some(runtime) = state.registry().get(&CompanyId::new(&company)) else {
        return ApiError(OpenCompanyError::CompanyNotFound(company)).into_response();
    };
    let Some(config) = provider_config(&provider) else {
        return ApiError(OpenCompanyError::InvalidRequest(format!(
            "provider '{provider}' is not enabled on this host"
        )))
        .into_response();
    };

    match exchange_code(&state, &config, &code).await {
        Ok(token_json) => {
            let account = extract_account(&token_json);
            let stored = json!({ "token": token_json, "account": account });
            if let Err(err) = runtime
                .secrets()
                .set(
                    runtime.id(),
                    &oauth_key(&provider),
                    SecretValue(stored.to_string()),
                )
                .await
            {
                return ApiError(err).into_response();
            }
            // Redirect the browser back to the console connections view.
            let console = std::env::var("OPENCOMPANY_CONSOLE_URL")
                .unwrap_or_else(|_| state.config().host_base_url());
            Redirect::to(&format!(
                "{}/connections?connected={provider}",
                console.trim_end_matches('/')
            ))
            .into_response()
        }
        Err(err) => ApiError(err).into_response(),
    }
}

/// Exchanges an authorization code for tokens at the provider's token endpoint.
async fn exchange_code(
    state: &AppState,
    config: &ProviderConfig,
    code: &str,
) -> Result<serde_json::Value, OpenCompanyError> {
    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
        ("redirect_uri", &redirect_uri(state)),
    ];
    let resp = client
        .post(&config.token_url)
        .header("Accept", "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|e| OpenCompanyError::Store(format!("oauth token exchange failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(OpenCompanyError::Store(format!(
            "oauth token endpoint returned {}",
            resp.status()
        )));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| OpenCompanyError::Store(format!("oauth token response not JSON: {e}")))
}

/// Extracts a human-friendly account label from a token response, if present.
fn extract_account(token: &serde_json::Value) -> Option<String> {
    for key in ["account", "email", "login", "user_login"] {
        if let Some(value) = token.get(key).and_then(|v| v.as_str()) {
            return Some(value.to_string());
        }
    }
    // Slack nests the workspace under `team.name`.
    token
        .get("team")
        .and_then(|team| team.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Disconnect
// ---------------------------------------------------------------------------

/// Deletes stored tokens (best-effort revoke is a follow-up).
async fn do_disconnect(
    runtime: Arc<CompanyRuntime>,
    provider: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Overwrite with an empty marker: the secret store has no delete; an empty
    // value reads back as "not connected" on the read side.
    runtime
        .secrets()
        .set(
            runtime.id(),
            &oauth_key(provider),
            SecretValue(String::new()),
        )
        .await?;
    Ok(Json(json!({ "connected": false, "provider": provider })))
}

/// `POST /api/v1/companies/{id}/connections/{provider}/disconnect`.
async fn disconnect(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, provider)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    do_disconnect(resolve(&state, &id)?, &provider).await
}

/// `POST /api/v1/company/connections/{provider}/disconnect` (single-company alias).
async fn disconnect_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    do_disconnect(resolve_sole(&state)?, &provider).await
}

/// Minimal percent-encoding for URL query values (RFC 3986 unreserved set kept).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn state_round_trips_and_rejects_tampering() {
        let state = encode_state("acme", "slack", now_millis() + STATE_TTL_MS);
        let (company, provider) = decode_state(&state).expect("valid state");
        assert_eq!(company, "acme");
        assert_eq!(provider, "slack");

        // A tampered signature fails.
        let mut tampered = state.clone();
        tampered.pop();
        tampered.push('0');
        assert!(decode_state(&tampered).is_none());
    }

    #[test]
    fn expired_state_is_rejected() {
        let state = encode_state("acme", "slack", now_millis().saturating_sub(1));
        assert!(decode_state(&state).is_none());
    }

    #[test]
    fn urlencode_escapes_reserved() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("plain-id_1.0~"), "plain-id_1.0~");
    }

    #[test]
    fn extract_account_reads_common_fields() {
        assert_eq!(
            extract_account(&json!({ "email": "ceo@acme.test" })),
            Some("ceo@acme.test".to_string())
        );
        assert_eq!(
            extract_account(&json!({ "team": { "name": "Acme" } })),
            Some("Acme".to_string())
        );
        assert_eq!(extract_account(&json!({ "access_token": "x" })), None);
    }
}
