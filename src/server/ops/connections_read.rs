//! Read-only connection status (`GET …/connections`).
//!
//! Deliberately **un-gated**: connection *status* is readable even when the
//! token-exchanging OAuth write routes ([`ops::connections`](super::connections),
//! the `oauth` feature) are not compiled. Without this route the console's
//! `GET …/connections` 404s and the page falls back to the read-only
//! "connections unavailable" banner; with it the console renders real
//! per-provider state and lights up the Connect buttons.
//!
//! Every field here is a **non-secret projection** — the provider id, a
//! `connected` boolean, an optional account label, and the credential tier the
//! Connect would route through — mirroring the GraphQL `Company.connections`
//! resolver
//! ([`resolve_connections`](crate::server::graphql::connections::resolve_connections)).
//! The stored OAuth token material never appears in the response or any log.
//!
//! ## Hosted versus the self-hosted hatch (issue #319)
//!
//! [`connect_route`] answers one question per provider: *can a Connect click
//! possibly succeed on this host, and by which route?* It reports a
//! [`CredentialSource`] tier — the same vocabulary
//! [`ops::composio`](super::composio) already uses, so the two console surfaces
//! read the same to an operator:
//!
//! * `attested` — the pod carries a platform-projected identity, so connections
//!   are the platform's to run. Nothing to register here, and the console offers
//!   no local Connect. A hosted tenant is injected no `OPENCOMPANY_OAUTH_*`
//!   variable, so a local Connect on such a host could only ever fail; reporting
//!   the tier makes that failure unreachable from the console rather than a
//!   400 the operator has to interpret.
//! * `static` — either a token this company already stored (a BYO override), or
//!   this host's own registered provider application: the **self-hosted hatch**
//!   documented on [`ops::connections`](super::connections). Connect works
//!   exactly as it does today.
//! * `none` — neither, so no Connect can succeed and the console says so.
//!
//! ### Provider mapping, for when the hosted route lands
//!
//! The platform backend's registered OAuth providers are `notion`, `google`,
//! `gmail`, `github`, `twitter`, `discord` and `instagram`. Two consequences for
//! this console's catalog:
//!
//! * `gmail` is a registered provider **name**, but not a separate provider
//!   application: it is Google's app requested with the Gmail skill scopes. A
//!   Gmail connect and a Google connect therefore share one grant, which is why
//!   the backend merges scopes incrementally rather than replacing them.
//! * There is **no Slack provider** at all (the backend's only Slack credential
//!   is an internal alerting bot). Slack has no hosted route except Composio,
//!   which runs its own OAuth — see [`ops::composio`](super::composio).

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;
use crate::app::config::EnvSource;
use crate::company::credentials::{CredentialSource, TinyhumansTokenSource, TokenTier};
use crate::company::runtime::CompanyRuntime;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// One connection's non-secret status, matching the console `ConnectionState`
/// wire type (`frontend/src/api/types.ts`):
/// `{ provider, connected, credentialSource, account? }`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionStateDto {
    /// The provider id (e.g. `github`, `slack`, `gmail`).
    provider: String,
    /// Whether a non-empty OAuth token is stored for this provider.
    connected: bool,
    /// Which route a Connect for this provider would take on this host — see
    /// [`connect_route`]. A tier name, never a credential and never a path.
    credential_source: CredentialSource,
    /// The connected account label, when known — never token material. Omitted
    /// when not connected or when the stored blob carries no account.
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

/// Can a Connect click for `provider` possibly succeed on this host, and by
/// which route?
///
/// Stored-wins, mirroring [`ops::composio`](super::composio)'s precedence:
///
/// 1. a token already stored for this provider → [`CredentialSource::Static`]
///    (the BYO override — whatever else the host offers, this company is
///    already connected through its own grant);
/// 2. else a platform-projected instance identity →
///    [`CredentialSource::Attested`];
/// 3. else this host's own registered provider application →
///    [`CredentialSource::Static`] (the self-hosted hatch);
/// 4. else [`CredentialSource::None`].
///
/// Step 2 is deliberately restricted to the **projected-file** tier.
/// [`TinyhumansTokenSource::from_env`] also resolves a long-lived
/// [`API_KEY_ENV`](crate::company::credentials::API_KEY_ENV) as its static tier,
/// and a self-hoster commonly sets exactly that to buy inference. Accepting it
/// here would tell such an operator their working Connect button is "managed by
/// the platform" and take it away — the platform runs no connection on their
/// behalf. Only a pod the platform actually projected an identity into is
/// hosted.
///
/// Takes the environment as a seam so the matrix is testable without mutating
/// the process environment.
///
/// Test-only because production resolves the host-level half once per request
/// and then loops — see [`HostConnectRoutes`]. It is defined *in terms of* that
/// type rather than restating the precedence, so what the tests exercise is the
/// same code the request path runs.
#[cfg(test)]
fn connect_route(provider: &str, stored: bool, env: &dyn EnvSource) -> CredentialSource {
    HostConnectRoutes::resolve(env).route(provider, stored, env)
}

/// The host-level half of [`connect_route`], resolved once.
///
/// Step 2 of the precedence — "is this pod carrying a platform-projected
/// identity?" — reads the environment and then *stats a file*. Its answer is a
/// property of the pod, not of a provider, so resolving it inside a loop over a
/// company's connections repeats a syscall for an answer that cannot differ
/// between iterations. Both read planes build this once per request and then ask
/// it per provider.
///
/// Deliberately still one rule: [`route`](Self::route) holds the whole
/// precedence, and [`connect_route`] is defined in terms of it, so a single-shot
/// caller and a looping caller cannot diverge.
pub(crate) struct HostConnectRoutes {
    /// Whether the pod carries a platform-**projected** identity — the only
    /// tier that means the platform runs connections here.
    attested: bool,
}

impl HostConnectRoutes {
    /// Resolves the host-level facts once, against an environment seam.
    fn resolve(env: &dyn EnvSource) -> Self {
        Self {
            attested: TinyhumansTokenSource::from_env(env).map(|source| source.tier())
                == Some(TokenTier::ProjectedFile),
        }
    }

    /// Resolves against the real process environment. Call once per request,
    /// then [`route_from_env`](Self::route_from_env) per provider.
    pub(crate) fn from_env() -> Self {
        Self::resolve(&crate::app::config::ProcessEnv)
    }

    /// The full precedence for one provider, given the already-resolved
    /// host-level facts.
    fn route(&self, provider: &str, stored: bool, env: &dyn EnvSource) -> CredentialSource {
        if stored {
            return CredentialSource::Static;
        }
        if self.attested {
            return CredentialSource::Attested;
        }
        if host_provider_app_configured(provider, env) {
            return CredentialSource::Static;
        }
        CredentialSource::None
    }

    /// [`route`](Self::route) against the real process environment. The one
    /// entry point the two read projections — this module and the GraphQL
    /// [`resolve_connections`](crate::server::graphql::connections::resolve_connections)
    /// — share, so they cannot drift apart.
    pub(crate) fn route_from_env(&self, provider: &str, stored: bool) -> CredentialSource {
        self.route(provider, stored, &crate::app::config::ProcessEnv)
    }
}

/// Whether this host has a provider application registered for `provider`,
/// delegated to the hatch module so the rule lives in exactly one place.
#[cfg(feature = "oauth")]
fn host_provider_app_configured(provider: &str, env: &dyn EnvSource) -> bool {
    super::connections::provider_app_configured(provider, env)
}

/// Without the `oauth` feature the token-exchanging write routes are not
/// compiled at all (they 404), so no local Connect can succeed on this host no
/// matter what the environment holds.
#[cfg(not(feature = "oauth"))]
fn host_provider_app_configured(_provider: &str, _env: &dyn EnvSource) -> bool {
    false
}

/// Builds the connection-status route fragment (both scope forms).
pub fn router() -> Router<AppState> {
    scoped("/connections", get(list))
}

/// Projects each manifest connection into its non-secret status by reading the
/// `oauth/{provider}` secret. Mirrors
/// [`resolve_connections`](crate::server::graphql::connections::resolve_connections):
/// only `provider` / `connected` / `credentialSource` / `account` ever leave
/// this function — the token blob stays in the
/// [`SecretStore`](crate::ports::SecretStore), and `credentialSource` is a tier
/// name, never a credential and never a path.
async fn project(runtime: &CompanyRuntime) -> Result<Vec<ConnectionStateDto>, ApiError> {
    let Some(record) = runtime.store().load(runtime.id()).await.map_err(ApiError)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(record.manifest.connections.len());
    // Host-level, so resolved once rather than per connection below.
    let host = HostConnectRoutes::from_env();
    for connection in &record.manifest.connections {
        let key = format!("oauth/{}", connection.provider);
        let (connected, account) = match runtime
            .secrets()
            .get(runtime.id(), &key)
            .await
            .map_err(ApiError)?
        {
            Some(value) if !value.expose().trim().is_empty() => {
                // Read only the `account` label out of the stored blob; the
                // `token` field is intentionally never touched.
                let account = serde_json::from_str::<serde_json::Value>(value.expose())
                    .ok()
                    .and_then(|json| {
                        json.get("account")
                            .and_then(|a| a.as_str())
                            .map(str::to_string)
                    });
                (true, account)
            }
            _ => (false, None),
        };
        out.push(ConnectionStateDto {
            credential_source: host.route_from_env(&connection.provider, connected),
            provider: connection.provider.clone(),
            connected,
            account,
        });
    }
    Ok(out)
}

/// `GET …/connections` — the company's non-secret connection status list.
async fn list(company: ScopedCompany) -> Result<Json<Vec<ConnectionStateDto>>, ApiError> {
    Ok(Json(project(company.runtime.as_ref()).await?))
}

#[cfg(test)]
mod tests {
    use super::{ConnectionStateDto, CredentialSource, connect_route};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-connections-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn get_connections(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/connections")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// The list route projects a connected provider (token stored) and a
    /// not-connected one (no token) side by side — the shape the console needs
    /// to flip from "unavailable" to live buttons.
    #[tokio::test]
    async fn projects_connected_and_not_connected() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[connection]]\nprovider = \"github\"\n\
             [[connection]]\nprovider = \"slack\"\n\
             [[connection]]\nprovider = \"gmail\"\n",
        )
        .await;

        // Store a GitHub token blob (github connected, with an account label);
        // a Gmail token blob with NO account label (connected, account omitted);
        // slack stays untouched (not connected).
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .secrets()
            .set(
                &id,
                "oauth/github",
                SecretValue(
                    serde_json::json!({
                        "token": { "access_token": "gho_secret_should_never_leak" },
                        "account": "octocat"
                    })
                    .to_string(),
                ),
            )
            .await
            .unwrap();
        runtime
            .secrets()
            .set(
                &id,
                "oauth/gmail",
                SecretValue(
                    serde_json::json!({
                        "token": { "access_token": "ya29_secret_should_never_leak" }
                    })
                    .to_string(),
                ),
            )
            .await
            .unwrap();

        let (status, body) = get_connections(&state).await;
        assert_eq!(status, StatusCode::OK);
        let list = body.as_array().expect("array body");
        assert_eq!(list.len(), 3, "one row per manifest connection: {body}");

        let github = list
            .iter()
            .find(|c| c["provider"] == "github")
            .expect("github row");
        assert_eq!(github["connected"], true);
        assert_eq!(github["account"], "octocat");
        // A stored token is the BYO override and outranks every host tier, so
        // this is `static` whatever the ambient environment happens to hold.
        assert_eq!(github["credentialSource"], "static", "{github}");

        let slack = list
            .iter()
            .find(|c| c["provider"] == "slack")
            .expect("slack row");
        assert_eq!(slack["connected"], false);
        assert!(
            slack.get("account").is_none(),
            "no account when not connected: {slack}"
        );
        // Nothing stored, so this row reports whichever host tier the ambient
        // environment resolves to. Assert only that the field is present and a
        // known tier name — the exhaustive precedence matrix is pinned against a
        // `MapEnv` in `connect_route_*` below, where it does not depend on the
        // test runner's environment.
        let tier = slack["credentialSource"]
            .as_str()
            .expect("every row carries a credentialSource");
        assert!(
            matches!(tier, "attested" | "static" | "none"),
            "unknown credential tier {tier:?}"
        );

        // A connected provider whose stored blob carries no `account` label is
        // still `connected: true`, and the `account` field is omitted entirely
        // (never serialized as null) — the `skip_serializing_if` path.
        let gmail = list
            .iter()
            .find(|c| c["provider"] == "gmail")
            .expect("gmail row");
        assert_eq!(gmail["connected"], true);
        assert!(
            gmail.get("account").is_none(),
            "no account when the stored blob carries no label: {gmail}"
        );

        // SECURITY: no token material may appear anywhere in the response.
        assert!(
            !body.to_string().contains("gho_secret_should_never_leak"),
            "token material leaked into the connections response: {body}"
        );
        assert!(
            !body.to_string().contains("ya29_secret_should_never_leak"),
            "token material leaked into the connections response: {body}"
        );
        assert!(
            !body.to_string().contains("access_token"),
            "token field leaked into the connections response: {body}"
        );
        // The new field is a tier name, so it must not have opened a path for a
        // token file, an api key, or a client secret to ride along.
        for forbidden in [
            crate::company::credentials::TOKEN_FILE_ENV,
            crate::company::credentials::API_KEY_ENV,
            "OPENCOMPANY_OAUTH_",
            "clientSecret",
            "client_secret",
        ] {
            assert!(
                !body.to_string().contains(forbidden),
                "{forbidden} leaked into the connections response: {body}"
            );
        }
    }

    /// The precedence matrix from [`connect_route`], driven through the env seam
    /// so nothing mutates the process environment: stored wins, else a projected
    /// platform identity, else this host's own provider app, else nothing.
    #[test]
    fn connect_route_follows_stored_then_attested_then_hatch() {
        use crate::app::config::MapEnv;

        let dir = tempfile::Builder::new()
            .prefix("oc-connect-route-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-instance-token").unwrap();
        let projected = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);
        // An obviously fake host provider application — the self-hosted hatch.
        // The state signing secret is part of the hatch being *open*: without
        // one no nonce can be issued, so no handshake can complete (issue #318).
        let hatch = MapEnv::new([
            ("OPENCOMPANY_OAUTH_GITHUB_ID", "fake-client-id"),
            ("OPENCOMPANY_OAUTH_GITHUB_SECRET", "fake-client-secret"),
            ("OPENCOMPANY_OAUTH_STATE_SECRET", "a-private-value"),
        ]);

        // 1. A stored token is the BYO override and outranks everything else,
        //    including a projected platform identity.
        assert_eq!(
            connect_route("github", true, &projected),
            CredentialSource::Static
        );
        assert_eq!(
            connect_route("github", true, &MapEnv::default()),
            CredentialSource::Static
        );

        // 2. Nothing stored + a projected platform identity → the platform owns
        //    the connection, for every provider (the identity is host-level).
        assert_eq!(
            connect_route("github", false, &projected),
            CredentialSource::Attested
        );
        assert_eq!(
            connect_route("slack", false, &projected),
            CredentialSource::Attested
        );

        // 3. No platform identity, but this host registered its own provider
        //    application → the hatch is open and Connect works as it does today.
        //    Only for the provider it was registered for.
        if cfg!(feature = "oauth") {
            assert_eq!(
                connect_route("github", false, &hatch),
                CredentialSource::Static
            );
        }
        assert_eq!(
            connect_route("slack", false, &hatch),
            CredentialSource::None
        );

        // 4. Neither → no Connect can succeed on this host.
        assert_eq!(
            connect_route("github", false, &MapEnv::default()),
            CredentialSource::None
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The regression this restriction exists for: a self-hoster who set
    /// `TINYHUMANS_API_KEY` to buy inference has **no** platform-run connection
    /// route, so their Connect must not be reported as platform-managed and
    /// taken away from them.
    ///
    /// `TinyhumansTokenSource::from_env` happily resolves that key as its static
    /// tier; [`connect_route`] deliberately accepts only the projected-file tier.
    #[test]
    fn a_static_api_key_is_not_a_hosted_connection_route() {
        use crate::app::config::MapEnv;

        // Inference credential only, nothing else: NOT attested.
        let inference_only =
            MapEnv::new([(crate::company::credentials::API_KEY_ENV, "th_fake_key")]);
        assert_eq!(
            connect_route("github", false, &inference_only),
            CredentialSource::None,
            "a static inference key must not be read as a platform connection route"
        );

        // Same operator, with their own GitHub app registered: the hatch stays
        // open and still reads `static`, not `attested`.
        if cfg!(feature = "oauth") {
            let with_hatch = MapEnv::new([
                (crate::company::credentials::API_KEY_ENV, "th_fake_key"),
                ("OPENCOMPANY_OAUTH_GITHUB_ID", "fake-client-id"),
                ("OPENCOMPANY_OAUTH_GITHUB_SECRET", "fake-client-secret"),
                ("OPENCOMPANY_OAUTH_STATE_SECRET", "a-private-value"),
            ]);
            assert_eq!(
                connect_route("github", false, &with_hatch),
                CredentialSource::Static,
                "the self-hosted hatch must keep working when an inference key is also set"
            );

            // Issue #318: the same operator without a state signing secret. The
            // provider application is fully registered, and the hatch is still
            // shut — an unsigned-nonce flow has no CSRF check worth the name, so
            // the honest answer is that no Connect can succeed here.
            let no_state_secret = MapEnv::new([
                (crate::company::credentials::API_KEY_ENV, "th_fake_key"),
                ("OPENCOMPANY_OAUTH_GITHUB_ID", "fake-client-id"),
                ("OPENCOMPANY_OAUTH_GITHUB_SECRET", "fake-client-secret"),
            ]);
            assert_eq!(
                connect_route("github", false, &no_state_secret),
                CredentialSource::None,
                "a registered provider app with no state signing secret is not connectable"
            );
        }

        // And a `TINYHUMANS_TOKEN_FILE` naming a path that does not exist
        // degrades to the static tier inside the resolver — which is likewise
        // not a hosted connection route.
        let dangling = MapEnv::new([
            (
                crate::company::credentials::TOKEN_FILE_ENV,
                "/nonexistent/oc/projected/token",
            ),
            (crate::company::credentials::API_KEY_ENV, "th_fake_key"),
        ]);
        assert_eq!(
            connect_route("github", false, &dangling),
            CredentialSource::None,
            "a token file that was never projected is not a hosted connection route"
        );
    }

    /// The DTO's whole serialized surface: three non-secret facts plus an
    /// optional label. Pins the field set so a future addition has to be a
    /// deliberate edit here, and pins the camelCase spelling the console reads.
    #[test]
    fn the_dto_carries_a_tier_name_and_nothing_secret() {
        let dto = ConnectionStateDto {
            provider: "github".to_string(),
            connected: true,
            credential_source: CredentialSource::Attested,
            account: Some("octocat".to_string()),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["credentialSource"], "attested");
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["provider", "connected", "credentialSource", "account"],
            "the read shape must stay exactly this: {keys:?}"
        );
    }

    /// A company with no `[[connection]]` entries returns an empty list (200),
    /// not a 404 — so the console renders "ready" with an empty catalog rather
    /// than the "unavailable" fallback.
    #[tokio::test]
    async fn empty_when_no_connections_declared() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, body) = get_connections(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!([]), "empty array: {body}");
    }
}
