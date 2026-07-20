use std::path::PathBuf;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::{AppState, Result};

/// Path prefixes owned by the server's API and discovery surfaces. The console
/// SPA fallback must never answer these: a request under one of them either
/// hits a real handler or is genuinely absent (e.g. a feature-gated route in a
/// build without that feature), and absent server routes must keep 404ing so
/// API and external clients can detect an unwired surface instead of receiving
/// the `index.html` shell with a `200`.
const RESERVED_PREFIXES: [&str; 7] = [
    "/api", "/graphql", "/healthz", "/spec", "/tiny", "/a2a", "/hooks",
];

/// True when `path` is server-owned and so must 404 rather than fall through to
/// the console shell. That is either a path under a reserved prefix — an exact
/// match (`/spec`) or a sub-path (`/api/v1/...`) — or any `.well-known`
/// discovery URI (RFC 8615). The latter is reserved wherever the segment
/// appears, not just at the root: `/companies/{handle}/.well-known/agent-card.json`
/// is a tiny.place Agent Card endpoint (feature-gated on `tinyplace`), and the
/// SPA must never masquerade as one for a directory client probing it.
fn is_reserved_path(path: &str) -> bool {
    if path.split('/').any(|segment| segment == ".well-known") {
        return true;
    }
    RESERVED_PREFIXES.iter().any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The directory whose built operator console the host serves at `/`, read from
/// `OPENCOMPANY_CONSOLE_DIR`. Returns `None` when the variable is unset, empty,
/// or does not point at an existing directory — in which case the host keeps
/// its historical behavior and 404s on unknown paths (no console fallback).
fn console_dir_from_env() -> Option<PathBuf> {
    let raw = std::env::var_os("OPENCOMPANY_CONSOLE_DIR")?;
    if raw.is_empty() {
        return None;
    }
    let dir = PathBuf::from(raw);
    dir.is_dir().then_some(dir)
}

/// Builds the Axum router, mounting the operator console at `/` when
/// `OPENCOMPANY_CONSOLE_DIR` is configured.
pub fn router(state: AppState) -> Router {
    router_with_console(state, console_dir_from_env())
}

/// Router builder with an explicit console directory, so tests can inject a
/// temporary console tree without touching process environment.
fn router_with_console(state: AppState, console_dir: Option<PathBuf>) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/spec", get(spec))
        .route("/tiny", get(tiny))
        .merge(crate::server::operator::router())
        .merge(crate::server::ops::router())
        .merge(crate::server::provision::router())
        .merge(crate::server::feedback::router())
        .merge(crate::server::users::router())
        .merge(crate::server::users::admin::router())
        .merge(crate::server::graphql::router());
    // tiny.place A2A inbound + discovery routes, only when the feature is on.
    #[cfg(feature = "tinyplace")]
    let router = router.merge(crate::server::a2a::router());
    let router = router.with_state(state.clone());

    // Operator console: the lowest-priority fallback. Every real route above —
    // `/api`, `/graphql`, `/spec`, `/tiny`, `/healthz` — is matched first and
    // always wins. Only when nothing else matches does `ServeDir` answer:
    // asset paths (`/assets/app.js`, `/favicon.ico`) serve their file, and any
    // other unknown path (a client-side SPA route) falls through to
    // `index.html` so the React router can take over. Unmatched paths under a
    // reserved server prefix (`/api`, `/a2a`, `/.well-known`, ...) are the one
    // exception: they 404 rather than serve the shell, so a feature-gated or
    // otherwise absent API/discovery route stays detectable by its callers.
    // When no console dir is configured this is skipped entirely and unknown
    // paths keep 404ing.
    let router = match console_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            let serve = ServeDir::new(dir)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(index));
            router.fallback(move |request: Request| {
                let serve = serve.clone();
                async move {
                    if is_reserved_path(request.uri().path()) {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    match serve.oneshot(request).await {
                        Ok(response) => response.into_response(),
                        Err(err) => match err {},
                    }
                }
            })
        }
        None => router,
    };

    // CORS is off unless origins are configured, which is every same-origin
    // deployment. When it is on, `map_response` cannot see the request, so the
    // origin is captured per-request in a closure instead — cheap, and it keeps
    // this to two small pieces rather than a middleware stack the codebase
    // otherwise has none of.
    let cors = state.cors().clone();
    if !cors.is_enabled() {
        return router;
    }
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let cors = cors.clone();
            async move {
                let headers = request.headers().clone();
                // A preflight never reaches a handler: answer it here.
                if crate::server::cors::is_preflight(request.method())
                    && let Some(response) = cors.preflight(&headers)
                {
                    return response;
                }
                let mut response: Response = next.run(request).await;
                for (name, value) in cors.headers_for(&headers) {
                    response.headers_mut().insert(name, value);
                }
                response.into_response()
            }
        },
    ))
}

/// Serves the Axum application.
pub async fn serve(state: AppState) -> Result<()> {
    let listener = TcpListener::bind(&state.config().bind).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn spec(State(state): State<AppState>) -> Json<crate::app::AppSpec> {
    Json(state.spec())
}

async fn tiny(State(state): State<AppState>) -> Json<Vec<crate::tiny::RuntimeModuleStatus>> {
    Json(state.spec().runtime_modules)
}

#[derive(Clone, Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::AppConfig;

    /// Writes a minimal console tree (just `index.html`) into a temp dir.
    fn console_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            "<!doctype html><title>console</title>",
        )
        .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn console_serves_index_at_root() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn console_falls_back_to_index_for_spa_routes() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/some/spa/route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn console_does_not_shadow_api_routes() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        // A real API route still reaches the API and answers with its own auth
        // status (401), never the SPA shell.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/companies")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!body_text(response).await.contains("<title>console</title>"));
    }

    #[tokio::test]
    async fn console_does_not_shadow_unmatched_reserved_paths() {
        let (_guard, dir) = console_fixture();
        let app = router_with_console(AppState::new(AppConfig::default()), Some(dir));

        // An unmatched path under a reserved API/discovery prefix (e.g. a
        // feature-gated route in a build without that feature) must 404, not
        // fall through to the SPA shell, so callers can still detect the
        // surface as unwired.
        for path in [
            "/api/v1/does-not-exist",
            "/.well-known/agent-card.json",
            "/companies/acme/.well-known/agent-card.json",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "path: {path}");
            assert!(!body_text(response).await.contains("<title>console</title>"));
        }
    }

    #[test]
    fn reserved_path_matches_prefixes_and_subpaths_only() {
        assert!(is_reserved_path("/api"));
        assert!(is_reserved_path("/api/v1/companies"));
        // `/hooks/{companyId}/{channel}` inbound webhooks (api.md) — a
        // server-owned namespace, reserved even before the route is wired.
        assert!(is_reserved_path("/hooks/acme/slack"));
        assert!(is_reserved_path("/.well-known/agent-card.json"));
        assert!(is_reserved_path("/a2a/handle"));
        // A `.well-known` discovery URI is reserved wherever the segment sits,
        // including under a company handle the SPA otherwise owns.
        assert!(is_reserved_path(
            "/companies/acme/.well-known/agent-card.json"
        ));
        // A console route that merely shares a prefix substring is not reserved.
        assert!(!is_reserved_path("/apidocs"));
        assert!(!is_reserved_path("/tinyplace-console"));
        // `/companies/{handle}` client-side console routes still fall through.
        assert!(!is_reserved_path("/companies/acme"));
        assert!(!is_reserved_path("/"));
        assert!(!is_reserved_path("/some/spa/route"));
    }

    #[tokio::test]
    async fn root_404s_without_console_dir() {
        let app = router_with_console(AppState::new(AppConfig::default()), None);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn spec_returns_axum_framework() {
        let app = router(AppState::new(AppConfig::default()));

        let response = app
            .oneshot(Request::builder().uri("/spec").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
