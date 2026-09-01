//! Native OAuth credential cleanup, plus the bounded retirement bridge for its
//! former start and callback routes.
//!
//! Native OAuth stored a real `oauth/{provider}` secret, but no agent path can
//! resolve it. The console stopped offering the flow in #828; #838 makes direct
//! callers honest too. `start` now returns a structured explanation and the
//! browser callback renders one, without exchanging a code or writing a secret.
//!
//! The bridge remains through 2026-09-30 for console bundles cached before #979.
//! Its removal is tracked by #1023. `disconnect` and the read projection stay:
//! they are the only way for a tenant to release a credential written before the
//! console offer disappeared.

use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::SecretValue;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, oauth_key, scoped};

/// The final day the compatibility responses may remain registered.
const NATIVE_OAUTH_REMOVAL_DATE: &str = "2026-09-30";
const NATIVE_OAUTH_SUNSET: &str = "Wed, 30 Sep 2026 00:00:00 GMT";
const NATIVE_OAUTH_RETIREMENT: &str = concat!(
    "Native OAuth connections are retired because the credentials this flow stored are not reachable by agents. ",
    "Connect the provider through Composio instead.",
);

/// Builds native OAuth cleanup routes and the temporary retirement bridge.
///
/// `start` and `callback` intentionally remain reachable until #1023's removal
/// date. A cacheable console shell used to leave old bundles calling `start`
/// after deploy (#979); a 410 that explains the unusable credential is safer
/// than a 404 that leaves those callers debugging a missing endpoint.
pub fn router() -> Router<AppState> {
    scoped("/connections/{provider}/start", post(start))
        .merge(scoped(
            "/connections/{provider}/disconnect",
            post(disconnect),
        ))
        // The callback is unscoped because it only renders the retirement page.
        .route("/api/v1/oauth/callback", get(callback))
}

/// The provider sub-resource path (`provider`); the scope `id` is consumed by
/// the [`ScopedCompany`] extractor.
#[derive(Debug, Deserialize)]
struct ProviderPath {
    provider: String,
}

// ---------------------------------------------------------------------------
// Reporting a connection change
// ---------------------------------------------------------------------------

/// What happened to a connection, as the closed vocabulary
/// [`Event::ConnectionChanged`](crate::analytics::Event::ConnectionChanged)
/// declares it.
///
/// An enum rather than a `&str` argument at each call site, for the reason
/// [`Outcome`](crate::analytics::Outcome) and [`Trigger`](crate::analytics::Trigger)
/// are enums: a call site that can pass a string is a call site that can one day
/// pass an error message, and the four words below are the whole of what this
/// event is allowed to say about *what happened*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transition {
    /// A credential or a tool server was wired up.
    Connected,
    /// One was released, revoked or removed.
    Disconnected,
    /// An existing one was re-checked, rotated or edited in place.
    Refreshed,
    /// The attempt did not leave a working connection behind.
    Failed,
}

impl Transition {
    /// The stable slug.
    fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Refreshed => "refreshed",
            Self::Failed => "failed",
        }
    }
}

/// Reports one connection change: the folded provider and the transition.
///
/// The **only** constructor of
/// [`Event::ConnectionChanged`](crate::analytics::Event::ConnectionChanged) in
/// the crate, so no route can choose to pass its own provider string through.
/// That matters more here than almost anywhere: the values in reach of these
/// handlers are an OAuth access token, a Composio connection id, an MCP server
/// name the operator typed (frequently a customer's or a project's), and a
/// provider error message. `raw_provider` is folded through
/// [`provider_slug`](crate::analytics::types::provider_slug) — the same closed
/// vocabulary the envelope uses for cognition providers, so there is one list
/// rather than two that drift — and anything it does not recognise becomes
/// `other`. Nothing else about the connection is offered at all.
///
/// Synchronous and infallible, like every
/// [`Tracker::track`](crate::analytics::Tracker::track): a disconnect must
/// never be delayed by, or fail because of, telemetry.
pub(super) fn track_connection(
    runtime: &CompanyRuntime,
    raw_provider: &str,
    transition: Transition,
) {
    runtime
        .tracker
        .track(crate::analytics::Event::ConnectionChanged {
            provider: crate::analytics::types::provider_slug(raw_provider),
            transition: transition.as_str(),
        });
}

// ---------------------------------------------------------------------------
// Historical credential revocation
// ---------------------------------------------------------------------------

/// The host-level app credentials needed to revoke a historical grant.
struct ProviderConfig {
    client_id: String,
    client_secret: String,
}

/// Resolves the app credentials needed to revoke a historical provider grant.
///
/// They no longer make this host a provider connection endpoint: `start` always
/// refuses, regardless of whether these values are present. The seam is an
/// injected [`crate::app::config::EnvSource`] so revocation tests can point it
/// at a [`crate::app::config::MapEnv`]; production goes through
/// [`best_effort_revoke`]'s `ProcessEnv` pin.
fn provider_config_from(
    provider: &str,
    env: &dyn crate::app::config::EnvSource,
) -> Option<ProviderConfig> {
    let key = provider.to_ascii_uppercase();
    let env = |suffix: &str| env.get(&format!("OPENCOMPANY_OAUTH_{key}_{suffix}"));
    let client_id = env("ID")?;
    let client_secret = env("SECRET")?;
    Some(ProviderConfig {
        client_id,
        client_secret,
    })
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/// `POST …/connections/{provider}/start` (both scope forms).
///
/// This retains its existing admin boundary while cached pre-#828 bundles age
/// out. It must not issue another signed state: a new handshake would once again
/// produce a credential that no agent can use.
async fn start(_company: AdminScopedCompany, Path(_provider): Path<ProviderPath>) -> Response {
    native_oauth_start_retired()
}

// ---------------------------------------------------------------------------
// Callback
// ---------------------------------------------------------------------------

/// `GET /api/v1/oauth/callback` remains for a browser redirected by an OAuth
/// flow that began immediately before this deployment. The callback deliberately
/// ignores its query: accepting an in-flight signed state would still store the
/// agent-unreachable credential that retired `start` now refuses to create.
async fn callback() -> Response {
    native_oauth_callback_retired()
}

fn native_oauth_start_retired() -> Response {
    (
        StatusCode::GONE,
        [("deprecation", "true"), ("sunset", NATIVE_OAUTH_SUNSET)],
        Json(json!({
            "code": "native_oauth_retired",
            "error": NATIVE_OAUTH_RETIREMENT,
            "removalAfter": NATIVE_OAUTH_REMOVAL_DATE,
        })),
    )
        .into_response()
}

fn native_oauth_callback_retired() -> Response {
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Native OAuth retired</title></head><body><h1>Native OAuth connection is no longer available</h1><p>{}</p><p>Nothing was saved from this authorization. Use the connection path offered in the OpenCompany console, typically Composio.</p><p>This temporary compatibility page will be removed after {}.</p></body></html>",
        NATIVE_OAUTH_RETIREMENT, NATIVE_OAUTH_REMOVAL_DATE,
    );
    (
        StatusCode::GONE,
        [
            ("cache-control", "no-store"),
            ("deprecation", "true"),
            ("sunset", NATIVE_OAUTH_SUNSET),
        ],
        Html(page),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Disconnect
// ---------------------------------------------------------------------------

/// The provider's token-revocation endpoint, if one is known. GitHub's URL
/// carries the app `client_id` in its path. Overridable per provider via
/// `OPENCOMPANY_OAUTH_<P>_REVOKE_URL` (tests point this at a local mock).
/// `None` means "no known revoke flow" — disconnect still blanks the local
/// secret; there is simply no remote call to make.
fn revoke_url_from(
    provider: &str,
    config: &ProviderConfig,
    env: &dyn crate::app::config::EnvSource,
) -> Option<String> {
    let key = provider.to_ascii_uppercase();
    if let Some(url) = env.get(&format!("OPENCOMPANY_OAUTH_{key}_REVOKE_URL")) {
        return Some(url);
    }
    match provider {
        "slack" => Some("https://slack.com/api/auth.revoke".to_string()),
        "github" => Some(format!(
            "https://api.github.com/applications/{}/grant",
            config.client_id
        )),
        _ => None,
    }
}

/// Reads the stored `access_token` for a provider, if a non-empty token blob is
/// present. Returns `None` when nothing is stored or the blob has no token.
/// The token is used only to build the revoke request and is never logged.
async fn stored_access_token(runtime: &CompanyRuntime, provider: &str) -> Option<String> {
    let value = runtime
        .secrets()
        .get(runtime.id(), &oauth_key(provider))
        .await
        .ok()
        .flatten()?;
    if value.expose().trim().is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(value.expose())
        .ok()?
        .get("token")
        .and_then(|t| t.get("access_token"))
        .and_then(|a| a.as_str())
        .map(str::to_string)
}

/// Best-effort provider-side token revocation, run before the local secret is
/// blanked. Any failure — unknown provider, unconfigured app credentials, a
/// network error, or a non-success status — is logged and swallowed so the
/// disconnect always proceeds. Token material is never logged or returned.
async fn best_effort_revoke(runtime: &CompanyRuntime, provider: &str) {
    best_effort_revoke_from(runtime, provider, &crate::app::config::ProcessEnv).await;
}

async fn best_effort_revoke_from(
    runtime: &CompanyRuntime,
    provider: &str,
    // `+ Sync` so the returned future is `Send`, which axum requires of a
    // handler: a `&T` is `Send` only when `T` is `Sync`, and the bare trait
    // object is not. Same rule as `server/setup.rs`'s `apply_inner`.
    env: &(dyn crate::app::config::EnvSource + Sync),
) {
    let Some(access_token) = stored_access_token(runtime, provider).await else {
        return;
    };
    let Some(config) = provider_config_from(provider, env) else {
        return;
    };
    let Some(url) = revoke_url_from(provider, &config, env) else {
        return;
    };
    // Bound the best-effort revoke: a provider that accepts the connection but
    // hangs (or a slow `_REVOKE_URL` override) must not block the disconnect
    // handler indefinitely. A builder failure skips the remote revoke entirely
    // — the caller blanks the local secret unconditionally, so this stays
    // best-effort without blocking or panicking.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(provider, "oauth revoke client build failed: {err}");
            return;
        }
    };
    let request = match provider {
        // GitHub revokes a grant with an authenticated DELETE carrying the token
        // in the JSON body and the app credentials as Basic auth.
        "github" => client
            .delete(&url)
            .basic_auth(&config.client_id, Some(&config.client_secret))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "opencompany")
            .json(&json!({ "access_token": access_token })),
        // Slack (and any override that behaves like it) revokes with a POST form.
        _ => client.post(&url).form(&[("token", access_token.as_str())]),
    };
    match request.send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            tracing::warn!(provider, status = %resp.status(), "oauth revoke returned non-success")
        }
        Err(err) => tracing::warn!(provider, "oauth revoke request failed: {err}"),
    }
}

/// Revokes the provider grant (best-effort) then deletes the stored tokens.
async fn do_disconnect(
    runtime: Arc<CompanyRuntime>,
    provider: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Ask the provider to invalidate the grant first, best-effort: a failed or
    // absent revoke must never block the local disconnect below.
    best_effort_revoke(&runtime, provider).await;
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
    // After the store write, so a failed disconnect is never reported as one
    // that happened. The path segment is folded, not carried: `{provider}` is
    // whatever the caller put in the URL.
    // After the store write, so a failed disconnect is never reported as one
    // that happened. The path segment is folded, not carried: `{provider}` is
    // whatever the caller put in the URL.
    track_connection(&runtime, provider, Transition::Disconnected);
    Ok(Json(json!({ "connected": false, "provider": provider })))
}

/// [`do_disconnect`] over an injected environment seam, for revocation tests.
/// Test-only: production `disconnect` goes through [`do_disconnect`]'s
/// `ProcessEnv` pin, so this is compiled only under `cfg(test)` rather than
/// carrying an `allow(dead_code)` that would hide a real orphan.
#[cfg(test)]
async fn do_disconnect_from(
    runtime: Arc<CompanyRuntime>,
    provider: &str,
    env: &(dyn crate::app::config::EnvSource + Sync),
) -> Result<Json<serde_json::Value>, ApiError> {
    best_effort_revoke_from(&runtime, provider, env).await;
    runtime
        .secrets()
        .set(
            runtime.id(),
            &oauth_key(provider),
            SecretValue(String::new()),
        )
        .await?;
    track_connection(&runtime, provider, Transition::Disconnected);
    Ok(Json(json!({ "connected": false, "provider": provider })))
}

/// `POST …/connections/{provider}/disconnect` (both scope forms).
/// Requires authority over the company (issue #403). Disconnecting deletes the
/// stored tokens and best-effort revokes them upstream — irreversible, and a
/// decision about the company's access rather than the caller's.
async fn disconnect(
    company: AdminScopedCompany,
    Path(ProviderPath { provider }): Path<ProviderPath>,
) -> Result<Json<serde_json::Value>, ApiError> {
    do_disconnect(company.runtime, &provider).await
}

#[cfg(test)]
mod test {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use tower::ServiceExt;

    use crate::ports::types::CompanyId;
    use crate::{AppConfig, AppState};

    #[tokio::test]
    async fn start_returns_an_expiring_structured_retirement_error() {
        let response = native_oauth_start_retired();
        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(
            response.headers().get("deprecation").unwrap(),
            "true",
            "a caller must learn this is a bounded compatibility response"
        );
        assert_eq!(
            response.headers().get("sunset").unwrap(),
            NATIVE_OAUTH_SUNSET,
            "the response itself names when the bridge is removed"
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "native_oauth_retired");
        assert_eq!(body["removalAfter"], NATIVE_OAUTH_REMOVAL_DATE);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("not reachable by agents"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn callback_ends_an_inflight_flow_without_accepting_its_code() {
        let response = router()
            .with_state(AppState::new(AppConfig::default()))
            .oneshot(
                Request::get(
                    "/api/v1/oauth/callback?code=CANARY-authz-code&state=CANARY-signed-state",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::GONE);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            response.headers().get("sunset").unwrap(),
            NATIVE_OAUTH_SUNSET
        );
        assert!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );

        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("Native OAuth connection is no longer available"));
        assert!(body.contains("Nothing was saved from this authorization"));
        assert!(body.contains(NATIVE_OAUTH_REMOVAL_DATE));
        assert!(
            !body.contains("CANARY"),
            "the callback neither accepts nor reflects its former OAuth inputs: {body}"
        );
    }

    // ---- disconnect / best-effort revoke ----------------------------------

    use crate::company::CompanyManifest;
    use crate::company::runtime::CompanyRuntime;
    use crate::runtime::RuntimeBuilder;

    /// Builds an isolated in-memory company runtime for disconnect tests.
    ///
    /// The caller must hold the returned handle for the life of the test: it
    /// owns the runtime's home directory and removes it on drop.
    async fn test_runtime() -> (Arc<CompanyRuntime>, tempfile::TempDir) {
        let home = tempfile::Builder::new()
            .prefix("oc-disc-")
            .tempdir()
            .expect("tempdir");
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
        let runtime = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(CompanyId::new("acme"))
            .build()
            .await
            .unwrap();
        (Arc::new(runtime), home)
    }

    /// A process-unique provider id (env-key safe) so the env vars each test
    /// sets never collide with a sibling test running in parallel.
    fn unique_provider() -> String {
        format!("revtest{}", crate::ports::generate_id().replace('-', ""))
    }

    async fn store_token(runtime: &CompanyRuntime, provider: &str, access_token: &str) {
        runtime
            .secrets()
            .set(
                runtime.id(),
                &oauth_key(provider),
                SecretValue(
                    json!({ "token": { "access_token": access_token }, "account": "acc" })
                        .to_string(),
                ),
            )
            .await
            .unwrap();
    }

    async fn is_blanked(runtime: &CompanyRuntime, provider: &str) -> bool {
        runtime
            .secrets()
            .get(runtime.id(), &oauth_key(provider))
            .await
            .unwrap()
            .map(|v| v.expose().trim().is_empty())
            .unwrap_or(true)
    }

    fn revoke_env(provider: &str, url: String) -> crate::app::config::MapEnv {
        let key = provider.to_ascii_uppercase();
        crate::app::config::MapEnv::new([
            (format!("OPENCOMPANY_OAUTH_{key}_ID"), "cid".to_string()),
            (
                format!("OPENCOMPANY_OAUTH_{key}_SECRET"),
                "csec".to_string(),
            ),
            (
                format!("OPENCOMPANY_OAUTH_{key}_AUTHORIZE_URL"),
                "http://x/a".to_string(),
            ),
            (
                format!("OPENCOMPANY_OAUTH_{key}_TOKEN_URL"),
                "http://x/t".to_string(),
            ),
            (format!("OPENCOMPANY_OAUTH_{key}_REVOKE_URL"), url),
        ])
    }

    /// No app credentials configured → provider_config is `None`, so there is no
    /// remote to revoke, but the disconnect must still blank the local secret.
    #[tokio::test]
    async fn disconnect_blanks_secret_without_revoke_config() {
        let (runtime, _home) = test_runtime().await;
        let provider = unique_provider();
        store_token(&runtime, &provider, "CANARY-should-never-leak").await;

        let resp = do_disconnect(runtime.clone(), &provider).await.unwrap();
        assert_eq!(resp.0["connected"], false);
        assert!(is_blanked(&runtime, &provider).await, "secret not blanked");
    }

    /// The best-effort revoke path is invoked: with app credentials + a revoke
    /// URL configured, disconnect POSTs the token to the provider endpoint AND
    /// blanks the local secret.
    #[tokio::test]
    async fn disconnect_invokes_provider_revoke() {
        use axum::extract::State;
        use axum::routing::post;

        let hits: Arc<tokio::sync::Mutex<Vec<String>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/revoke",
                post(|State(hits): State<Arc<tokio::sync::Mutex<Vec<String>>>>, body: String| async move {
                    hits.lock().await.push(body);
                    "ok"
                }),
            )
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (runtime, _home) = test_runtime().await;
        let provider = unique_provider();
        let env = revoke_env(&provider, format!("http://{addr}/revoke"));

        store_token(&runtime, &provider, "CANARY-revoke-me").await;
        let _ = do_disconnect_from(runtime.clone(), &provider, &env)
            .await
            .unwrap();

        let received = hits.lock().await;
        assert_eq!(received.len(), 1, "revoke endpoint was not called");
        assert!(
            received[0].contains("CANARY-revoke-me"),
            "revoke request did not carry the stored token"
        );
        drop(received);
        assert!(is_blanked(&runtime, &provider).await, "secret not blanked");
    }

    // ---- connection_changed analytics (issue #1739) -----------------------

    use crate::analytics::types::OpaqueId;
    use crate::analytics::{Envelope, Event, RecordingTracker, payload};
    use crate::app::deployment::Deployment;
    use crate::ports::brain::{Cognition, UsageMetering};

    /// A runtime whose analytics go to a recorder rather than nowhere.
    async fn recording_runtime() -> (
        Arc<CompanyRuntime>,
        Arc<RecordingTracker>,
        tempfile::TempDir,
    ) {
        let home = tempfile::Builder::new()
            .prefix("oc-disc-analytics-")
            .tempdir()
            .expect("tempdir");
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
        let recorder = Arc::new(RecordingTracker::new());
        let runtime = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(CompanyId::new("acme"))
            .with_analytics(recorder.clone())
            .build()
            .await
            .unwrap();
        (Arc::new(runtime), recorder, home)
    }

    fn envelope() -> Envelope {
        Envelope::new(
            OpaqueId::instance("0123456789abcdef0123456789abcdef"),
            Deployment::HostedTenant,
            Cognition {
                path: "harness",
                provider: "openrouter",
                model: None,
                metering: UsageMetering::PerTurn,
            },
        )
    }

    /// Only the connection events, so a runtime that reports something else
    /// during build cannot make these assertions read on the wrong row.
    fn connection_events(recorder: &RecordingTracker) -> Vec<Event> {
        recorder
            .events()
            .into_iter()
            .filter(|event| matches!(event, Event::ConnectionChanged { .. }))
            .collect()
    }

    /// A disconnect reports the folded provider and the word `disconnected`.
    #[tokio::test]
    async fn a_disconnect_reports_its_provider_and_transition() {
        let (runtime, recorder, _home) = recording_runtime().await;
        store_token(&runtime, "github", "test-token").await;

        // The `Json` body is deliberately discarded: `.unwrap()` already
        // asserts the disconnect succeeded, and what the body says is the
        // subject of `disconnect_blanks_secret_without_revoke_config` above.
        // This test is about the event that the disconnect sends.
        let _ = do_disconnect(runtime.clone(), "github").await.unwrap();

        assert_eq!(
            connection_events(&recorder),
            vec![Event::ConnectionChanged {
                provider: "github",
                transition: "disconnected",
            }]
        );
        let rendered = payload(&envelope(), &connection_events(&recorder)[0]);
        assert_eq!(rendered["event"], "connection_changed");
        assert_eq!(rendered["properties"]["provider"], "github");
        assert_eq!(rendered["properties"]["transition"], "disconnected");
    }

    /// Each of the four transitions is its own compiled-in word, and there are
    /// only four. A fifth would be a word nobody reviewed.
    #[test]
    fn every_transition_is_a_compiled_in_word() {
        assert_eq!(Transition::Connected.as_str(), "connected");
        assert_eq!(Transition::Disconnected.as_str(), "disconnected");
        assert_eq!(Transition::Refreshed.as_str(), "refreshed");
        assert_eq!(Transition::Failed.as_str(), "failed");
    }

    /// **The guarantee**: nothing this route touches — the stored access token,
    /// the provider name the caller put in the URL — reaches the payload.
    ///
    /// Built the way `crate::analytics::test` builds its guard: a distinctive
    /// needle, searched for case-insensitively, **with a self-check** proving
    /// the same search does find that needle in a rendering that carries it.
    /// Without the second half the assertion could pass because the needle was
    /// unfindable rather than because it was absent, which is how a redaction
    /// test rots into a test of nothing.
    #[tokio::test]
    async fn no_credential_or_provider_string_reaches_the_payload() {
        // The two kinds of content in reach here: the credential, and the
        // caller-chosen provider segment, which is frequently a customer name.
        const HOSTILE: &[&str] = &["sk-not-a-real-key", "acmecorp-holdings-crm"];

        let (runtime, recorder, _home) = recording_runtime().await;
        store_token(&runtime, "acmecorp-holdings-crm", "sk-not-a-real-key").await;

        // The body is discarded for the reason given above; the payload is
        // what this test reads.
        let _ = do_disconnect(runtime.clone(), "acmecorp-holdings-crm")
            .await
            .unwrap();

        let events = connection_events(&recorder);
        // A count, not a value: without this the needle search below could pass
        // on an event that simply never fired. The value is asserted at the end
        // — the tighter check must not run first and mask the guard a leak has
        // to get past.
        assert_eq!(events.len(), 1, "the disconnect did not report itself");

        let envelope = envelope();
        let rendered = payload(&envelope, &events[0])
            .to_string()
            .to_ascii_lowercase();
        for hostile in HOSTILE {
            let needle = hostile.to_ascii_lowercase();
            assert!(
                !rendered.contains(&needle),
                "a connection_changed payload leaked {needle:?}: {rendered}"
            );
            // The self-check, against a payload that really does carry it.
            let mut unredacted = payload(&envelope, &events[0]);
            unredacted["properties"]["provider"] = serde_json::Value::from(*hostile);
            assert!(
                unredacted
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(&needle),
                "the needle must be findable in an unredacted rendering, or the guard above is \
                 vacuous"
            );
        }

        // And, last, the stricter statement: the unrecognised provider reached
        // the catch-all rather than being carried, and the transition is one of
        // the four compiled-in words.
        assert_eq!(
            events,
            vec![Event::ConnectionChanged {
                provider: crate::analytics::types::OTHER,
                transition: "disconnected",
            }]
        );
    }

    /// A revoke endpoint that refuses the connection must not fail the
    /// disconnect: the local secret is still blanked.
    #[tokio::test]
    async fn disconnect_blanks_secret_when_revoke_fails() {
        // Bind then drop to obtain a port with nothing listening on it.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let (runtime, _home) = test_runtime().await;
        let provider = unique_provider();
        let env = revoke_env(&provider, format!("http://{dead_addr}/revoke"));

        store_token(&runtime, &provider, "CANARY-unreachable").await;
        // Must still succeed even though the revoke POST cannot connect.
        let _ = do_disconnect_from(runtime.clone(), &provider, &env)
            .await
            .unwrap();
        assert!(is_blanked(&runtime, &provider).await, "secret not blanked");
    }
}
