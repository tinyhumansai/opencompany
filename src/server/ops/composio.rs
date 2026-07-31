//! Per-tenant Composio connection management (issue #110, epic #26 Cell D): read
//! the company's Composio status and set a per-company OAuth bearer token.
//!
//! ## Where the credential normally comes from
//!
//! On the hosted platform a company needs to paste nothing. The instance already
//! authenticates with a platform-minted, audience-bound identity, and Composio
//! calls present that — the backend derives the Composio entity from it. The read
//! shape reports this as `credentialSource: "attested"`, and there is nothing
//! stored on the instance to leak, rotate, or lose.
//!
//! ## `PUT …/composio/token` — the BYO override
//!
//! The write route stays, and it is **not** the hosted path. It is:
//!
//! * the **per-company BYO override** — a company that has its own Composio
//!   account/token can set it here, and it wins over the instance identity for
//!   that company only; and
//! * the escape hatch for running this repo **standalone**. This repository is
//!   public and people do run it that way, but self-hosting is **unsupported**:
//!   there is no platform identity to present, so pasting a token is the only way
//!   to reach Composio at all. Do not read this as parity with the hosted
//!   product — it is a hatch, not a deployment mode, and nothing else about
//!   standalone operation is supported this milestone.
//!
//! Either way the token is **write-only** over the API: set through the `token`
//! field, stored in the secret store under
//! [`TOKEN_KEY`](crate::company::composio::TOKEN_KEY), and **never** echoed. The
//! read shape carries only `credentialSource` plus non-secret routing (backend
//! URL, toolkit allowlist) — never a token, and never a file path. A set / rotate
//! / clear takes effect on the agents' **next turn** with no restart (the harness
//! re-resolves the credential each turn and rebuilds the roster when the
//! *identity* behind it changes).

use axum::Json;
use axum::Router;
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::composio::{backend_url_or_default, store_token, token_configured};
use crate::company::credentials::{CredentialSource, TinyhumansTokenSource};
use crate::company::runtime::CompanyRuntime;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The reminder attached to every mutating response.
const SWITCH_NOTE: &str =
    "Agents pick up the new Composio token on their next turn — no restart needed.";

/// Builds the Composio management route fragment.
///
/// The read/write-token plane (`GET …/composio`, `PUT …/composio/token`) is
/// always present. The per-provider OAuth sign-in plane (`POST
/// …/composio/authorize`, `GET …/composio/connections`) is **also** always
/// present in the route table — mirroring how `get_status` stays wired and
/// reports `inBuild:false` rather than `#[cfg]`-ing itself out — but its
/// handlers only reach the live Composio client under the `composio` feature;
/// otherwise they answer `409 Conflict` "not in this build".
pub fn router() -> Router<AppState> {
    scoped("/composio", get(get_status))
        .merge(scoped("/composio/token", put(set_token)))
        .merge(scoped("/composio/authorize", post(authorize)))
        .merge(scoped("/composio/connections", get(connections)))
}

/// The company's Composio status as the console renders it. **Never** carries the
/// token — only the non-secret `credentialSource` plus routing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposioStatusDto {
    /// Whether the `composio` feature is compiled into this build at all (the
    /// tools only exist under it). `false` lets the console show a "not in this
    /// build" state rather than implying a missing credential.
    in_build: bool,
    /// Whether the company **explicitly** grants the `composio` namespace (a `*`
    /// wildcard does NOT count).
    granted: bool,
    /// Where this company's Composio credential comes from:
    ///
    /// * `attested` — the instance's platform identity (nothing is stored here);
    /// * `static` — a token this company pasted, or a static instance key;
    /// * `none` — no credential can be obtained, so no tools are wired.
    ///
    /// A tier name, never a credential and never a path.
    credential_source: CredentialSource,
    /// The effective Composio backend URL (env override or default). Non-secret.
    backend_url: String,
    /// The manifest toolkit allowlist (empty = defer to the backend allowlist).
    toolkits: Vec<String>,
}

/// A mutating response: the resulting status plus the switch reminder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    status: ComposioStatusDto,
    note: String,
}

/// Set-token body. `token` is write-only intake (never returned): a non-empty
/// value rotates the company's BYO override, an explicit empty string clears it
/// (reverting to the instance identity, where there is one).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetToken {
    token: String,
}

/// `POST …/composio/authorize` body: the toolkit slug (`gmail` / `slack` /
/// `github` / …) to begin an OAuth handoff for.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeBody {
    toolkit: String,
}

/// `POST …/composio/authorize` response: the Composio-hosted connect URL the
/// operator opens in a browser tab. Composio runs the OAuth itself; there is no
/// local callback — the console polls [`ConnectionDto`] until the toolkit
/// reports connected.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeDto {
    connect_url: String,
}

/// One per-toolkit connected state in the `GET …/composio/connections`
/// response: `connected` is true when the company has at least one active
/// connection for that toolkit.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionDto {
    toolkit: String,
    connected: bool,
}

/// Which tier a company's Composio credential comes from, mirroring the harness
/// resolver's precedence: the company's **own** stored token wins, else this
/// instance's platform identity, else nothing.
///
/// Takes the environment as a seam so the matrix is testable without mutating the
/// process environment.
fn credential_source_for(
    stored: bool,
    env: &dyn crate::app::config::EnvSource,
) -> CredentialSource {
    if stored {
        return CredentialSource::Static;
    }
    TinyhumansTokenSource::from_env(env)
        .map(|source| source.credential_source())
        .unwrap_or(CredentialSource::None)
}

/// Resolves the Composio status DTO for a company.
async fn effective_status(runtime: &CompanyRuntime) -> Result<ComposioStatusDto, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let (granted, toolkits) = match record {
        Some(record) => (
            crate::company::grants_composio_explicit(&record.manifest.tools.allow),
            record.manifest.tools.composio.toolkits.clone(),
        ),
        None => (false, Vec::new()),
    };
    // Mirrors the harness resolver: this company's own token wins, else the
    // instance's platform identity, else nothing.
    let stored = token_configured(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?;
    let env = crate::app::config::ProcessEnv;
    let (env_url, api_url) = {
        use crate::app::config::EnvSource;
        (
            env.get(crate::company::composio::COMPOSIO_BACKEND_URL_ENV),
            env.get(crate::company::composio::TINYHUMANS_API_URL_ENV),
        )
    };
    let credential_source = credential_source_for(stored, &env);
    Ok(ComposioStatusDto {
        in_build: cfg!(feature = "composio"),
        granted,
        credential_source,
        backend_url: backend_url_or_default(env_url, api_url),
        toolkits,
    })
}

/// `GET …/composio` — the company's Composio status.
async fn get_status(company: ScopedCompany) -> Result<Json<ComposioStatusDto>, ApiError> {
    Ok(Json(effective_status(company.runtime.as_ref()).await?))
}

/// `PUT …/composio/token` — set / rotate / clear this company's BYO token. See
/// the module docs: the hosted path needs no token, and standalone operation
/// (where this is the only option) is unsupported.
async fn set_token(
    company: ScopedCompany,
    Json(body): Json<SetToken>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    store_token(runtime.id(), runtime.secrets().as_ref(), &body.token)
        .await
        .map_err(ApiError)?;
    Ok(Json(MutationResponse {
        status: effective_status(runtime).await?,
        note: SWITCH_NOTE.to_string(),
    }))
}

/// `POST …/composio/authorize` — begin a per-provider OAuth handoff and return
/// the Composio-hosted connect URL the operator opens in a browser tab.
///
/// Composio runs the OAuth flow itself — there is **no** local callback route.
/// The console opens the URL and polls [`connections`] until the toolkit
/// reports connected. Under a non-`composio` build this answers `409 Conflict`
/// (see [`router`]).
async fn authorize(
    company: ScopedCompany,
    Json(body): Json<AuthorizeBody>,
) -> Result<Json<AuthorizeDto>, ApiError> {
    authorize_impl(company.runtime.as_ref(), body.toolkit).await
}

/// `GET …/composio/connections` — the company's per-toolkit connected state,
/// one [`ConnectionDto`] per toolkit that has at least one connection. The
/// console cross-references this against the granted `toolkits` to render each
/// provider row's connected/sign-in state. `409 Conflict` on a non-`composio`
/// build.
async fn connections(company: ScopedCompany) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    connections_impl(company.runtime.as_ref()).await
}

/// Resolve the per-tenant Composio config (bearer + backend URL + toolkit
/// allowlist) the way the harness roster build does, so the console dials the
/// backend with the exact same tenant identity. A `409 Conflict` when no
/// credential of any tier can be resolved — neither this company's own stored
/// token nor a platform identity — in which case the operator must paste a token
/// before OAuth.
#[cfg(feature = "composio")]
async fn resolve_tenant(
    runtime: &CompanyRuntime,
) -> Result<crate::harness::composio::TenantComposio, ApiError> {
    use crate::app::config::EnvSource;

    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let toolkits = record
        .map(|record| record.manifest.tools.composio.toolkits.clone())
        .unwrap_or_default();
    let env = crate::app::config::ProcessEnv;
    let backend_env = env.get(crate::company::composio::COMPOSIO_BACKEND_URL_ENV);
    let api_env = env.get(crate::company::composio::TINYHUMANS_API_URL_ENV);
    crate::harness::composio::TenantComposio::resolve(
        runtime.id(),
        runtime.secrets().as_ref(),
        toolkits,
        backend_env,
        api_env,
        crate::company::TinyhumansTokenSource::from_env(&env).map(std::sync::Arc::new),
    )
    .await
    .ok_or_else(|| {
        ApiError(crate::error::OpenCompanyError::Conflict(
            "no Composio credential is available for this company — this instance has no platform \
             identity, so paste the company's Composio token first"
                .to_string(),
        ))
    })
}

#[cfg(feature = "composio")]
async fn authorize_impl(
    runtime: &CompanyRuntime,
    toolkit: String,
) -> Result<Json<AuthorizeDto>, ApiError> {
    let config = resolve_tenant(runtime).await?;
    let connect_url = crate::harness::composio::authorize_connect_url(&config, &toolkit)
        .await
        .map_err(|err| {
            ApiError(crate::error::OpenCompanyError::TinyHumans {
                code: "composio_authorize".to_string(),
                message: err.to_string(),
            })
        })?;
    Ok(Json(AuthorizeDto { connect_url }))
}

#[cfg(feature = "composio")]
async fn connections_impl(runtime: &CompanyRuntime) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    let config = resolve_tenant(runtime).await?;
    let states = crate::harness::composio::list_connection_states(&config)
        .await
        .map_err(|err| {
            ApiError(crate::error::OpenCompanyError::TinyHumans {
                code: "composio_connections".to_string(),
                message: err.to_string(),
            })
        })?;
    Ok(Json(
        states
            .into_iter()
            .map(|(toolkit, connected)| ConnectionDto { toolkit, connected })
            .collect(),
    ))
}

/// A `409 Conflict` "Composio is not in this build" — the OAuth plane's off-state
/// under a non-`composio` build. Mirrors the status route's `inBuild:false`
/// semantics rather than pretending nothing is connected.
#[cfg(not(feature = "composio"))]
fn not_in_build() -> ApiError {
    ApiError(crate::error::OpenCompanyError::Conflict(
        "Composio is not compiled into this build".to_string(),
    ))
}

#[cfg(not(feature = "composio"))]
async fn authorize_impl(
    _runtime: &CompanyRuntime,
    _toolkit: String,
) -> Result<Json<AuthorizeDto>, ApiError> {
    Err(not_in_build())
}

#[cfg(not(feature = "composio"))]
async fn connections_impl(_runtime: &CompanyRuntime) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    Err(not_in_build())
}

#[cfg(test)]
mod tests {
    use super::{ComposioStatusDto, CredentialSource, credential_source_for};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const TOKEN: &str = "composio-tenant-bearer-SECRET-xyz";

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oc-composio-{}", crate::ports::generate_id()))
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

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", crate::server::test_support::fixed_cookie("acme"));
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8_lossy(&bytes).to_string();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value, raw)
    }

    /// The `credentialSource` matrix as the console consumes it, plus the
    /// write-only contract. Runs with no platform identity in the environment, so
    /// the instance contributes nothing and the company's own token is the only
    /// credential in play: `none` → `static` → `none`.
    #[tokio::test]
    async fn credential_source_tracks_the_byo_token_and_never_carries_it() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\", \"github\"]\n",
        )
        .await;

        // Initial status: granted, toolkits surfaced, no credential obtainable.
        let (status, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["granted"], true);
        assert_eq!(dto["credentialSource"], "none");
        assert_eq!(dto["toolkits"], json!(["gmail", "github"]));
        assert!(dto.get("backendUrl").is_some());
        assert!(
            dto.get("token").is_none(),
            "status must never carry a token"
        );
        assert!(
            dto.get("tokenConfigured").is_none(),
            "the boolean surface is replaced by credentialSource"
        );

        // Set the write-only BYO token → the static tier.
        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["credentialSource"], "static");
        assert!(!raw.contains(TOKEN), "PUT response leaked the token: {raw}");

        // GET reflects it and still never carries the token.
        let (_, dto, raw) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["credentialSource"], "static");
        assert!(!raw.contains(TOKEN), "GET status leaked the token: {raw}");

        // Clearing with "" reverts to whatever the instance offers — here nothing.
        let (_, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "" })),
        )
        .await;
        assert_eq!(resp["status"]["credentialSource"], "none");

        std::fs::remove_dir_all(&home).ok();
    }

    /// The hosted shape, driven through the env seam (no process mutation): a
    /// company that pasted nothing reads `attested` from the instance identity,
    /// its own token still outranks that, and neither yields `none`.
    #[test]
    fn credential_source_matrix_follows_the_resolver_precedence() {
        use crate::app::config::MapEnv;

        let dir = std::env::temp_dir().join(format!("oc-dto-{}", crate::ports::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "projected-instance-token").unwrap();
        let projected = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);

        // Nothing stored + a projected instance identity → attested.
        assert_eq!(
            credential_source_for(false, &projected),
            CredentialSource::Attested
        );
        // The company's own token outranks the instance identity.
        assert_eq!(
            credential_source_for(true, &projected),
            CredentialSource::Static
        );
        // A static instance key is the static tier too.
        assert_eq!(
            credential_source_for(
                false,
                &MapEnv::new([(crate::company::credentials::API_KEY_ENV, "th_static")])
            ),
            CredentialSource::Static
        );
        // Neither → nothing obtainable, so no tools.
        assert_eq!(
            credential_source_for(false, &MapEnv::default()),
            CredentialSource::None
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The DTO's whole surface, serialized: a tier name and non-secret routing.
    /// No token, and no token-file path either — the console never needs to know
    /// where on disk the instance's identity lives.
    #[test]
    fn the_dto_carries_a_tier_name_and_nothing_secret() {
        let dto = ComposioStatusDto {
            in_build: true,
            granted: true,
            credential_source: CredentialSource::Attested,
            backend_url: "https://api.tinyhumans.ai".to_string(),
            toolkits: vec!["gmail".to_string()],
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["credentialSource"], "attested");
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "inBuild",
                "granted",
                "credentialSource",
                "backendUrl",
                "toolkits"
            ],
            "the read shape must stay exactly this: {keys:?}"
        );
    }

    /// A `*` wildcard grant must NOT count as a composio grant on the status
    /// route (mirrors the harness build gate).
    #[tokio::test]
    async fn wildcard_grant_does_not_count_as_composio() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["granted"], false, "{dto}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// The OAuth sign-in plane is always wired into the route table (like the
    /// status route), so `POST …/composio/authorize` is never a 404. Without a
    /// usable Composio client it conflicts (`409`): on the default build because
    /// the feature is not compiled in; under the `composio` feature because no
    /// per-tenant token is configured yet. Either way the console gets a clear,
    /// non-404 signal rather than a missing route.
    #[tokio::test]
    async fn authorize_route_conflicts_without_build_or_token() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;

        let (status, body, raw) = send(
            &state,
            "POST",
            "/api/v1/company/composio/authorize",
            Some(json!({ "toolkit": "gmail" })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
        assert_eq!(body["code"], "conflict", "{body}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// `GET …/composio/connections` is likewise always wired and conflicts
    /// (`409`) when there is no usable client — no build feature or no token.
    #[tokio::test]
    async fn connections_route_conflicts_without_build_or_token() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;

        let (status, body, raw) =
            send(&state, "GET", "/api/v1/company/composio/connections", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
        assert_eq!(body["code"], "conflict", "{body}");

        std::fs::remove_dir_all(&home).ok();
    }
}
