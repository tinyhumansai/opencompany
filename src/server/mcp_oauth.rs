//! The unauthenticated console MCP OAuth callback (issue #90).
//!
//! `GET /oauth/mcp/callback?code=…&state=…` is where the operator's browser
//! lands after approving an MCP server's OAuth sign-in. It is mounted at the
//! **top level** (next to `/healthz`), **without** console auth: the browser
//! redirect from the authorization server carries no console session cookie, so
//! the route cannot require one. Its trust comes instead from the opaque `state`
//! it round-trips — an unknown/expired/replayed `state` yields nothing.
//!
//! Flow: look up the parked [`PendingOAuth`](crate::company::mcp_oauth::PendingOAuth)
//! by `state`, exchange the `code` for a token, store it **write-only** under the
//! company's per-server credential key, probe-and-persist the server's health,
//! and render a small self-contained success (or failure) HTML page for the tab.
//!
//! Compiled only under `feature = "mcp"`.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::AppState;
use crate::company::mcp::{self, McpHealth};
use crate::company::mcp_oauth;

/// The OAuth callback query. All fields optional so a malformed/`error` redirect
/// is handled with a clean page rather than a 422 extractor rejection.
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// The OAuth `error` code, when the authorization server denied the request.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// The top-level, unauthenticated callback route fragment (merged in
/// [`crate::server::routes`]).
pub fn router() -> Router<AppState> {
    Router::new().route("/oauth/mcp/callback", get(callback))
}

async fn callback(State(state): State<AppState>, Query(query): Query<CallbackQuery>) -> Response {
    // 1. The authorization server denied the request.
    if let Some(err) = query.error.as_deref() {
        let detail = query.error_description.as_deref().unwrap_or(err);
        return failure_page(
            StatusCode::BAD_REQUEST,
            "Sign-in was declined",
            &format!("The authorization server returned an error: {detail}"),
        );
    }

    // 2. A well-formed callback must carry both a code and a state.
    let (Some(code), Some(cb_state)) = (
        non_empty(query.code.as_deref()),
        non_empty(query.state.as_deref()),
    ) else {
        return failure_page(
            StatusCode::BAD_REQUEST,
            "Invalid sign-in response",
            "The sign-in response was missing its authorization code or state. Start again from Connections.",
        );
    };

    // 3. Reclaim the parked flow (single-use — a replayed callback finds nothing).
    let Some(pending) = state.take_oauth(cb_state) else {
        return failure_page(
            StatusCode::BAD_REQUEST,
            "Sign-in expired",
            "This sign-in link is no longer valid (it may have already been used or expired). Start again from Connections.",
        );
    };

    // 4. Resolve the company the pending flow targets.
    let Some(runtime) = state.registry().get(&pending.company_id) else {
        log::warn!(
            "[mcp-oauth] callback for unknown company={}",
            pending.company_id.as_ref()
        );
        return failure_page(
            StatusCode::NOT_FOUND,
            "Company not found",
            "The company this sign-in belongs to is no longer available.",
        );
    };

    // 5. Exchange the code for a token (PKCE verifier + client creds).
    let material = match mcp_oauth::complete(&pending, code).await {
        Ok(material) => material,
        Err(err) => {
            // `complete` never echoes a secret in its error, but scrub anyway
            // against this flow's own known secrets as defence in depth.
            let scrubbed =
                crate::harness::mcp_probe::scrub(&err.to_string(), &pending_secret_hints(&pending));
            log::warn!(
                "[mcp-oauth] token exchange failed for company={} server={}: {scrubbed}",
                pending.company_id.as_ref(),
                pending.server_name
            );
            return failure_page(
                StatusCode::BAD_GATEWAY,
                "Couldn't complete sign-in",
                &format!("The token exchange failed: {scrubbed}"),
            );
        }
    };

    // 6. Store the token WRITE-ONLY under the per-server credential key.
    if let Err(err) = mcp::store_auth(
        runtime.id(),
        &pending.server_name,
        &material,
        runtime.secrets().as_ref(),
    )
    .await
    {
        log::error!(
            "[mcp-oauth] failed to persist token for company={} server={}: {}",
            pending.company_id.as_ref(),
            pending.server_name,
            err.code()
        );
        return failure_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Couldn't save the credential",
            "The sign-in completed but the credential couldn't be stored. Try again.",
        );
    }

    // 7. Probe-and-persist so the console badge flips to green (best-effort).
    let health = probe_and_persist(&runtime, &pending.server_name).await;

    log::info!(
        "[mcp-oauth] callback stored token for company={} server={} (status={})",
        pending.company_id.as_ref(),
        pending.server_name,
        health
            .as_ref()
            .map(|h| h.status.as_str())
            .unwrap_or("unknown"),
    );

    success_page(&pending.server_name)
}

/// The known-secret set from a pending flow (client secret only — the tokens
/// aren't known until after exchange). Feeds the scrubber on the error path.
fn pending_secret_hints(pending: &mcp_oauth::PendingOAuth) -> Vec<String> {
    pending.client_secret.iter().cloned().collect()
}

/// Probe the server through the same auth-included registry the agent uses, and
/// persist the scrubbed outcome as the server's health. Best-effort — a probe
/// failure never fails the callback.
async fn probe_and_persist(
    runtime: &crate::company::runtime::CompanyRuntime,
    name: &str,
) -> Option<McpHealth> {
    let manifest = runtime
        .store()
        .load(runtime.id())
        .await
        .ok()
        .flatten()
        .map(|record| record.manifest.mcp_servers)
        .unwrap_or_default();
    let decls = mcp::resolve_effective(runtime.id(), &manifest, runtime.secrets().as_ref())
        .await
        .ok()?;
    let decl = decls.iter().find(|d| d.name == name)?;
    let health = crate::harness::mcp_probe::probe_server(decl).await;
    let _ = mcp::save_health(runtime.id(), name, &health, runtime.secrets().as_ref()).await;
    Some(health)
}

/// `Some(trimmed)` when a query value is present and non-blank.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Minimal HTML-escaping for interpolated text (defence against a hostile
/// `error_description` from the authorization server).
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A self-contained success page telling the operator to return to the console.
fn success_page(server: &str) -> Response {
    Html(page(
        "Signed in",
        &format!(
            "You're connected to <strong>{}</strong>. You can close this tab and return to OpenCompany.",
            escape(server)
        ),
    ))
    .into_response()
}

/// A self-contained failure page with a scrubbed, escaped message.
fn failure_page(status: StatusCode, title: &str, message: &str) -> Response {
    (status, Html(page(title, &escape(message)))).into_response()
}

/// One inline, dependency-free HTML document (no external assets — this page is
/// served to a bare browser tab).
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{title}</title>\
<style>body{{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;\
background:#0b0f14;color:#e6edf3;display:flex;min-height:100vh;margin:0;\
align-items:center;justify-content:center}}.card{{max-width:26rem;padding:2rem;\
background:#111820;border:1px solid #1f2933;border-radius:12px;text-align:center}}\
h1{{font-size:1.25rem;margin:0 0 .75rem}}p{{margin:0;color:#9fb0c0;line-height:1.5}}\
</style></head><body><div class=\"card\"><h1>{title}</h1><p>{body}</p></div></body></html>",
        title = escape(title),
        body = body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_neutralizes_html() {
        let out = escape("<script>alert(1)</script>&\"");
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&amp;"));
        assert!(out.contains("&quot;"));
    }

    #[test]
    fn success_page_names_the_server_escaped() {
        let resp = success_page("no<tion");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn non_empty_trims_and_rejects_blank() {
        assert_eq!(non_empty(Some("  x ")), Some("x"));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }
}
