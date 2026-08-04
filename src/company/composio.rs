//! Per-tenant Composio credential + backend routing (issue #110, epic #26 Cell
//! D). Always compiled (so the console read/write plane can manage the token
//! even in the default build); the live agent tools that consume it live in the
//! feature-gated [`harness::composio`](crate::harness::composio).
//!
//! The per-tenant OAuth bearer token is **write-only**: it is set through the
//! console `PUT …/composio/token` route, stored under [`TOKEN_KEY`], and never
//! returned. The read shape carries only a `tokenConfigured` boolean. The token
//! has **no environment fallback** — a missing token means no tools (fail
//! closed), never a borrowed identity. Only the backend URL may be overridden
//! from the environment.

use crate::Result;
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

/// The canonical per-company Composio credential key. The per-tenant OAuth
/// bearer token is stored here (write-only via the console); the value is the
/// raw token string.
pub const TOKEN_KEY: &str = "composio/token";

/// The explicit environment override for the Composio backend URL. Only the
/// **URL** has an env path — the **token** deliberately does not (fail-closed
/// isolation). When unset, resolution falls back to the tenant's shared API
/// base ([`TINYHUMANS_API_URL_ENV`]) so staging Composio follows staging.
pub const COMPOSIO_BACKEND_URL_ENV: &str = "OPENCOMPANY_COMPOSIO_BACKEND_URL";

/// The tenant's shared TinyHumans API base URL (the same backend inference and
/// the rest of the app already use). Used as the Composio backend fallback when
/// [`COMPOSIO_BACKEND_URL_ENV`] is unset, so a staging tenant's Composio calls
/// go to staging instead of the hardcoded prod default.
pub const TINYHUMANS_API_URL_ENV: &str = "TINYHUMANS_API_URL";

/// Default backend base URL for the Composio routes when neither the explicit
/// override nor the tenant API base is set. Mirrors the media backend's default
/// host (prod).
pub const DEFAULT_BACKEND_URL: &str = "https://api.tinyhumans.ai";

/// The effective Composio backend URL, resolved in this order (first non-empty,
/// trimmed, wins):
///
/// 1. `env_override` — [`COMPOSIO_BACKEND_URL_ENV`], the explicit override.
/// 2. `api_url` — [`TINYHUMANS_API_URL_ENV`], the tenant's shared backend base,
///    so Composio follows staging/prod with the rest of the app.
/// 3. [`DEFAULT_BACKEND_URL`] (prod) — last resort.
///
/// Credential-free — safe to surface on the console read plane.
pub fn backend_url_or_default(env_override: Option<String>, api_url: Option<String>) -> String {
    [env_override, api_url]
        .into_iter()
        .flatten()
        .map(|u| u.trim().to_string())
        .find(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_BACKEND_URL.to_string())
}

/// Store (or rotate/clear) the per-tenant Composio token. A non-empty value
/// rotates it; an empty string clears it. Write-only — the value is never read
/// back over the API.
pub async fn store_token(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    token: &str,
) -> Result<()> {
    secrets
        .set(company, TOKEN_KEY, SecretValue(token.trim().to_string()))
        .await
}

/// Whether a non-empty per-tenant token is stored — never the token itself.
pub async fn token_configured(company: &CompanyId, secrets: &dyn SecretStore) -> Result<bool> {
    Ok(secrets
        .get(company, TOKEN_KEY)
        .await?
        .map(|SecretValue(token)| !token.trim().is_empty())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_url_prefers_override_then_api_url_then_default() {
        // Neither set → prod default.
        assert_eq!(backend_url_or_default(None, None), DEFAULT_BACKEND_URL);

        // Explicit override wins over everything.
        assert_eq!(
            backend_url_or_default(
                Some("https://custom.example".into()),
                Some("https://staging-api.tinyhumans.ai".into())
            ),
            "https://custom.example"
        );

        // No override → follow the tenant API base (the staging case).
        assert_eq!(
            backend_url_or_default(None, Some("https://staging-api.tinyhumans.ai".into())),
            "https://staging-api.tinyhumans.ai"
        );

        // Whitespace/empty override falls through to the api_url fallback.
        assert_eq!(
            backend_url_or_default(
                Some("  ".into()),
                Some("https://staging-api.tinyhumans.ai".into())
            ),
            "https://staging-api.tinyhumans.ai"
        );

        // Whitespace/empty api_url falls through to the prod default.
        assert_eq!(
            backend_url_or_default(Some("".into()), Some("   ".into())),
            DEFAULT_BACKEND_URL
        );

        // api_url is trimmed before use.
        assert_eq!(
            backend_url_or_default(None, Some("  https://staging-api.tinyhumans.ai  ".into())),
            "https://staging-api.tinyhumans.ai"
        );
    }
}
