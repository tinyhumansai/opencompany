use std::path::PathBuf;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};

use crate::{AppState, Result};

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
    // `index.html` so the React router can take over. When no console dir is
    // configured this is skipped entirely and unknown paths keep 404ing.
    let router = match console_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            let serve = ServeDir::new(dir)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(index));
            router.fallback_service(serve)
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
        std::fs::write(dir.path().join("index.html"), "<!doctype html><title>console</title>")
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
