use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;

use crate::{AppState, Result};

/// Builds the Axum router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/spec", get(spec))
        .route("/tiny", get(tiny))
        .merge(crate::server::operator::router())
        .with_state(state)
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
