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

/// The environment override for the Composio backend URL. Only the **URL** has
/// an env path — the **token** deliberately does not (fail-closed isolation).
pub const COMPOSIO_BACKEND_URL_ENV: &str = "OPENCOMPANY_COMPOSIO_BACKEND_URL";

/// Default backend base URL for the Composio routes when the environment does
/// not override it. Mirrors the media backend's default host.
pub const DEFAULT_BACKEND_URL: &str = "https://api.tinyhumans.ai";

/// The effective backend URL: the trimmed env override when non-empty, else the
/// default. Credential-free — safe to surface on the console read plane.
pub fn backend_url_or_default(env_url: Option<String>) -> String {
    env_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
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
    fn backend_url_prefers_env_then_default() {
        assert_eq!(backend_url_or_default(None), DEFAULT_BACKEND_URL);
        assert_eq!(
            backend_url_or_default(Some("  ".into())),
            DEFAULT_BACKEND_URL
        );
        assert_eq!(
            backend_url_or_default(Some("https://custom.example".into())),
            "https://custom.example"
        );
    }
}
