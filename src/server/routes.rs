use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;

use crate::{AppState, Result};

/// Builds the Axum router.
pub fn router(state: AppState) -> Router {
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
