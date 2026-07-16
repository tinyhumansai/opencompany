//! Custom domain + DNS verification.
//!
//! `PUT …/domain` sets the domain and returns a [`DomainStatus`] carrying the
//! records the operator must add (persisted as JSON at
//! [`DOMAIN_KEY`](super::DOMAIN_KEY)). `POST …/domain/verify` runs server-side
//! DNS lookups through the injected [`DnsResolver`](crate::company::dns::DnsResolver)
//! and returns the updated status. Without an injected resolver (default build /
//! no `dns` feature) verify is "not wired yet" (404).
//!
//! The domain config is non-secret — it shares the secret store only because
//! that is the per-company durable key/value seam.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Response;
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;

use crate::AppState;
use crate::company::dns::{self, DomainStatus};
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::SecretValue;
use crate::server::error::ApiError;
use crate::server::ops::{DOMAIN_KEY, ScopedCompany, scoped};

/// Builds the domain route fragment.
pub fn router() -> Router<AppState> {
    scoped("/domain", put(put_domain)).merge(scoped("/domain/verify", post(verify_domain)))
}

/// The set-domain request body.
#[derive(Debug, Deserialize)]
struct SetDomain {
    /// The custom domain to configure.
    domain: String,
}

/// Persists a fresh domain status and returns it.
async fn store_domain(
    runtime: Arc<CompanyRuntime>,
    domain: &str,
) -> Result<Json<DomainStatus>, ApiError> {
    let status = DomainStatus::fresh(domain);
    persist(&runtime, &status).await?;
    Ok(Json(status))
}

/// Writes the status JSON to the secret store.
async fn persist(runtime: &CompanyRuntime, status: &DomainStatus) -> Result<(), ApiError> {
    let json = serde_json::to_string(status)?;
    runtime
        .secrets()
        .set(runtime.id(), DOMAIN_KEY, SecretValue(json))
        .await?;
    Ok(())
}

/// Loads the stored domain config, if any.
async fn load_domain(runtime: &CompanyRuntime) -> Result<Option<DomainStatus>, ApiError> {
    let Some(value) = runtime.secrets().get(runtime.id(), DOMAIN_KEY).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(value.expose())?))
}

/// `PUT …/domain` (both scope forms).
async fn put_domain(
    company: ScopedCompany,
    Json(body): Json<SetDomain>,
) -> Result<Json<DomainStatus>, ApiError> {
    store_domain(company.runtime, &body.domain).await
}

/// Runs a verification pass through the injected resolver and persists it.
async fn run_verify(
    state: &AppState,
    runtime: Arc<CompanyRuntime>,
) -> Result<Json<DomainStatus>, Response> {
    use axum::response::IntoResponse;
    let Some(resolver) = state.connections().dns.clone() else {
        return Err(super::not_wired("domain verification"));
    };
    let stored = load_domain(&runtime)
        .await
        .map_err(IntoResponse::into_response)?;
    let Some(stored) = stored else {
        return Err(ApiError(crate::error::OpenCompanyError::InvalidRequest(
            "no domain configured".to_string(),
        ))
        .into_response());
    };
    let status = dns::verify(&stored.domain, resolver.as_ref())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    persist(&runtime, &status)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(Json(status))
}

/// `POST …/domain/verify` (both scope forms).
async fn verify_domain(
    company: ScopedCompany,
    State(state): State<AppState>,
) -> Result<Json<DomainStatus>, Response> {
    run_verify(&state, company.runtime).await
}
