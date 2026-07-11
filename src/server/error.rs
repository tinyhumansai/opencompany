//! HTTP error mapping.
//!
//! [`OpenCompanyError`] stays axum-free (it is the shared crate error), so the
//! `IntoResponse` mapping lives here in a thin server-local newtype. Every error
//! renders the api.md envelope `{ "error": <message>, "code": <stable_code> }`
//! with a status derived from the variant.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::OpenCompanyError;

/// A server-local wrapper that renders an [`OpenCompanyError`] as an HTTP
/// response. Handlers return `Result<T, ApiError>`.
#[derive(Debug)]
pub struct ApiError(pub OpenCompanyError);

impl From<OpenCompanyError> for ApiError {
    fn from(error: OpenCompanyError) -> Self {
        Self(error)
    }
}

impl ApiError {
    /// The HTTP status this error maps to.
    pub fn status(&self) -> StatusCode {
        match &self.0 {
            OpenCompanyError::CompanyNotFound(_) => StatusCode::NOT_FOUND,
            OpenCompanyError::ManifestInvalid { .. }
            | OpenCompanyError::ManifestParse(_, _)
            | OpenCompanyError::MissingManifest(_)
            | OpenCompanyError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            OpenCompanyError::LifecycleConflict(_) => StatusCode::CONFLICT,
            OpenCompanyError::ToolNotGranted(_) => StatusCode::FORBIDDEN,
            OpenCompanyError::BudgetExceeded(_) => StatusCode::PAYMENT_REQUIRED,
            // tiny.place transport: an unreachable backend degrades to 503 so
            // callers retry; any other protocol failure is an upstream 502.
            OpenCompanyError::Tinyplace { code, .. } if code == "unreachable" => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            OpenCompanyError::Tinyplace { .. } => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = Json(json!({
            "error": self.0.to_string(),
            "code": self.0.code(),
        }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn maps_variants_to_status_and_code() {
        let not_found = ApiError(OpenCompanyError::CompanyNotFound("acme".into()));
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(not_found.0.code(), "company_not_found");

        let conflict = ApiError(OpenCompanyError::LifecycleConflict("paused".into()));
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let invalid = ApiError(OpenCompanyError::ManifestInvalid {
            path: PathBuf::from("company.toml"),
            problems: vec!["missing name".into()],
        });
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let tool = ApiError(OpenCompanyError::ToolNotGranted("payment.send".into()));
        assert_eq!(tool.status(), StatusCode::FORBIDDEN);

        let other = ApiError(OpenCompanyError::Store("disk full".into()));
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn maps_tinyplace_transport_to_503_and_502() {
        let unreachable = ApiError(OpenCompanyError::tinyplace("unreachable", "offline"));
        assert_eq!(unreachable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unreachable.0.code(), "tinyplace_unreachable");

        let upstream = ApiError(OpenCompanyError::tinyplace("http_500", "boom"));
        assert_eq!(upstream.status(), StatusCode::BAD_GATEWAY);
    }
}
