//! Hosted multi-tenant provisioning and per-company lifecycle controls.
//!
//! `POST /api/v1/companies` provisions a company from a manifest body (raw TOML
//! or `{ "manifest_toml", "id"? }` JSON), validates it, builds a
//! [`CompanyRuntime`](crate::company::runtime::CompanyRuntime) over the data
//! dir, registers it, and records its owning tenant. Provisioning and suspension
//! require the `platform` scope; pause/resume/archive are owner-scoped and never
//! cross tenants.
//!
//! Lifecycle transitions persist the new [`CompanyRecord`](crate::ports::types::CompanyRecord)
//! `lifecycle` and append a [`LifecycleChanged`](crate::ports::types::CompanyEvent::LifecycleChanged)
//! audit event. Archive additionally removes the company from the registry so it
//! is no longer addressable.
//!
//! Webhook emission (`approval.requested`, `work.completed`, `feedback.created`,
//! `budget.exhausted`) runs through the offline-mockable
//! [`WebhookSink`](crate::server::webhook::WebhookSink); the default build
//! records deliveries in memory.

use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::company::CompanyManifest;
use crate::ports::types::{Actor, ActorKind, CompanyId};
use crate::runtime::types::CycleReport;
use crate::runtime::{RuntimeBuilder, company_id_from_name};
use crate::server::error::ApiError;
use crate::server::graphql::auth::GqlAuth;
use crate::server::platform_auth::{PlatformScope, acting_tenant, authorize_address};
use crate::server::webhook::{WebhookEvent, WebhookKind};

/// Builds the provisioning + lifecycle route fragment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/companies", post(provision))
        .route("/api/v1/companies/{id}/pause", post(pause))
        .route("/api/v1/companies/{id}/resume", post(resume))
        .route("/api/v1/companies/{id}/suspend", post(suspend))
        .route("/api/v1/companies/{id}/archive", post(archive))
}

// ---------------------------------------------------------------------------
// Response envelopes
// ---------------------------------------------------------------------------

fn envelope(status: StatusCode, code: &str, error: &str) -> Response {
    (status, Json(json!({ "error": error, "code": code }))).into_response()
}

fn not_found(id: &str) -> Response {
    envelope(
        StatusCode::NOT_FOUND,
        "company_not_found",
        &format!("company not found: {id}"),
    )
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// The JSON provisioning body: a manifest string plus an optional explicit id.
#[derive(Debug, Deserialize)]
struct ProvisionBody {
    /// The company manifest as TOML.
    manifest_toml: String,
    /// An explicit company id; derived from the name when omitted.
    #[serde(default)]
    id: Option<String>,
}

/// `POST /api/v1/companies` — provision a company from a manifest body.
async fn provision(
    PlatformScope(claims): PlatformScope,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Accept raw TOML or a JSON envelope carrying the TOML.
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("json"))
        .unwrap_or(false);
    let (manifest_toml, explicit_id) = if is_json {
        match serde_json::from_str::<ProvisionBody>(&body) {
            Ok(parsed) => (parsed.manifest_toml, parsed.id),
            Err(err) => {
                return envelope(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    &format!("invalid provisioning body: {err}"),
                );
            }
        }
    } else {
        (body, None)
    };

    let manifest: CompanyManifest = match toml::from_str(&manifest_toml) {
        Ok(manifest) => manifest,
        Err(err) => {
            return envelope(
                StatusCode::BAD_REQUEST,
                "manifest_parse",
                &format!("manifest is not valid TOML: {}", err.message()),
            );
        }
    };
    let problems = manifest.validate();
    if !problems.is_empty() {
        // Render the prosumer problem list; never leak serde traces.
        return ApiError(crate::error::OpenCompanyError::ManifestInvalid {
            path: std::path::PathBuf::from("company.toml"),
            problems,
        })
        .into_response();
    }

    let id = match explicit_id {
        Some(raw) => CompanyId::new(raw),
        None => company_id_from_name(&manifest.company.name),
    };
    // The tenant that owns this company and namespaces its id.
    //
    // In shared-single-DB mode the workload's *configured* namespace
    // (`OPENCOMPANY_TENANT_ID`) is authoritative for its own data scope: config,
    // not the request's acting tenant, decides where this workload writes. Using
    // it keeps the id and the ownership record workload-local even when a
    // full-platform token provisions on behalf of another tenant, and it matches
    // the filter boot hydration applies to the persisted `owners` rows (also
    // `AppConfig::tenant_namespace`) — so an API-provisioned company survives a
    // restart instead of being orphaned by a foreign-tenant prefix.
    //
    // Outside shared-single-DB mode the acting tenant is recorded, feeding
    // per-tenant quota and db-per-tenant / self-hosted ownership as before.
    // Canonicalized (bare-slug) so the recorded owner, the persisted `owners`
    // row, and quota counting all key by the same identity that tenant-scoped
    // auth compares a `tenant:acme` claim against.
    let tenant = crate::app::canonical_tenant(
        &state
            .config()
            .tenant_namespace
            .clone()
            .unwrap_or_else(|| acting_tenant(&GqlAuth::Platform(claims.clone()))),
    )
    .to_string();
    // Namespace the id with the workload's tenant so API-provisioned companies
    // are globally unique in one logical database (the same template name under
    // two tenant workloads no longer collides on the `companies` unique index).
    // A no-op when tenant-namespace mode is off; idempotent for an already-
    // prefixed explicit id.
    let id = state.config().namespaced_company_id(id);

    // Reject a duplicate id.
    if state.registry().get(&id).is_some() {
        return envelope(
            StatusCode::CONFLICT,
            "company_exists",
            &format!("company already exists: {id}"),
        );
    }

    // Quota: per-tenant then global.
    if let Some(max) = state.config().max_companies_per_tenant
        && state.tenant_company_count(&tenant) >= max
    {
        return envelope(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            &format!("tenant company quota of {max} reached"),
        );
    }
    if let Some(max) = state.config().max_companies
        && state.registry().len() >= max
    {
        return envelope(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            &format!("global company quota of {max} reached"),
        );
    }

    // Build over the data dir, honoring the selected storage backend (fs
    // defaults when none is configured).
    let mut builder = RuntimeBuilder::new(state.home().to_path_buf(), manifest)
        .with_id(id.clone())
        .with_tinyplace_api_url(state.config().tinyplace_api_url.clone())
        .with_host_base_url(state.config().host_base_url());
    if let Some(stores) = state.stores() {
        builder = builder.with_stores(stores);
    }
    if let Some(overlay) = state.memory_overlay() {
        builder = builder.with_memory_overlay(overlay);
    }
    let runtime = match builder.build().await {
        Ok(runtime) => runtime,
        Err(err) => return ApiError(err).into_response(),
    };

    let status = match runtime.status().await {
        Ok(status) => status,
        Err(err) => return ApiError(err).into_response(),
    };
    state
        .registry()
        .insert(id.clone(), std::sync::Arc::new(runtime));
    state.set_owner(id.clone(), tenant.clone());
    // Persist ownership when the backend supports it, so the tenant map
    // survives restarts (best-effort: the in-memory map already reflects it).
    if let Some(ownership) = state.stores().and_then(|s| s.ownership.clone())
        && let Err(err) = ownership.set_owner(&id, &tenant).await
    {
        tracing::warn!(company = %id, error = %err, "failed to persist company ownership");
    }

    (StatusCode::CREATED, Json(status)).into_response()
}

// ---------------------------------------------------------------------------
// Lifecycle controls
// ---------------------------------------------------------------------------

/// The actor recorded for a platform/operator-driven lifecycle transition.
/// Who a lifecycle transition is recorded as.
///
/// A human is recorded as themselves; a machine credential as the tenant it
/// acts for. Previously everything was `Operator`, because that was the only
/// principal that could reach these routes.
fn lifecycle_actor(auth: &GqlAuth) -> Actor {
    match auth {
        GqlAuth::User(user) => Actor {
            kind: ActorKind::User,
            id: user.user_id.clone(),
        },
        GqlAuth::Platform(_) => Actor {
            kind: ActorKind::Operator,
            id: acting_tenant(auth),
        },
    }
}

/// Whether the request carries `?reason=budget`, without pulling in axum's
/// `query` feature.
fn reason_is_budget(uri: &Uri) -> bool {
    uri.query()
        .map(|q| q.split('&').any(|pair| pair == "reason=budget"))
        .unwrap_or(false)
}

/// Applies a lifecycle transition to `to`, returning the fresh status.
async fn transition(state: &AppState, auth: &GqlAuth, id: &CompanyId, to: &str) -> Response {
    let Some(runtime) = state.registry().get(id) else {
        return not_found(id.as_ref());
    };
    if let Err(err) = runtime.set_lifecycle(to, lifecycle_actor(auth)).await {
        return ApiError(err).into_response();
    }
    match runtime.status().await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(err) => ApiError(err).into_response(),
    }
}

/// `POST /api/v1/companies/{id}/pause` — stop accepting work (owner-scoped).
async fn pause(
    crate::server::platform_auth::CompanyAuth(auth): crate::server::platform_auth::CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let id = CompanyId::new(id);
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return resp;
    }
    if let Some(resp) = crate::server::platform_auth::refuse_until_password_changed(&auth) {
        return resp;
    }
    let response = transition(&state, &auth, &id, "paused").await;
    // A budget-triggered pause emits the `budget.exhausted` webhook.
    if response.status() == StatusCode::OK && reason_is_budget(&uri) {
        emit_budget_exhausted(&state, &id).await;
    }
    response
}

/// `POST /api/v1/companies/{id}/resume` — resume accepting work (owner-scoped).
async fn resume(
    crate::server::platform_auth::CompanyAuth(auth): crate::server::platform_auth::CompanyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let id = CompanyId::new(id);
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return resp;
    }
    if let Some(resp) = crate::server::platform_auth::refuse_until_password_changed(&auth) {
        return resp;
    }
    // `suspended` is a platform-forced pause (billing/abuse); only a
    // platform-scope caller may lift it. Neither an owner token nor a company's
    // own admin may resume a company the platform suspended.
    let platform = matches!(&auth, GqlAuth::Platform(c) if c.has_platform_scope());
    if !platform {
        match state.registry().get(&id) {
            Some(runtime) => match runtime.status().await {
                Ok(status) if status.lifecycle == "suspended" => {
                    return crate::server::platform_auth::forbidden();
                }
                Ok(_) => {}
                Err(err) => return ApiError(err).into_response(),
            },
            None => return not_found(id.as_ref()),
        }
    }
    transition(&state, &auth, &id, "running").await
}

/// `POST /api/v1/companies/{id}/suspend` — park a company (platform-scoped).
async fn suspend(
    PlatformScope(claims): PlatformScope,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let id = CompanyId::new(id);
    let auth = GqlAuth::Platform(claims);
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return resp;
    }
    transition(&state, &auth, &id, "suspended").await
}

/// `POST /api/v1/companies/{id}/archive` — terminally archive a company and
/// remove it from the registry (platform-scoped).
async fn archive(
    PlatformScope(claims): PlatformScope,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let id = CompanyId::new(id);
    let auth = GqlAuth::Platform(claims);
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return resp;
    }
    let response = transition(&state, &auth, &id, "archived").await;
    if response.status() == StatusCode::OK {
        state.registry().remove(&id);
        state.remove_owner(&id);
        if let Some(ownership) = state.stores().and_then(|s| s.ownership.clone())
            && let Err(err) = ownership.remove_owner(&id).await
        {
            tracing::warn!(company = %id, error = %err, "failed to remove persisted ownership");
        }
    }
    response
}

// ---------------------------------------------------------------------------
// Webhook emission (shared with the operator chat surface)
// ---------------------------------------------------------------------------

/// Emits the webhooks a completed cycle implies: one `approval.requested` per
/// newly parked approval, and one `work.completed` when the cycle produced
/// output. A no-op when no webhook is configured.
pub(crate) async fn emit_cycle_webhooks(state: &AppState, id: &CompanyId, report: &CycleReport) {
    let Some(webhook) = state.webhook() else {
        return;
    };
    for approval_id in &report.parked {
        let event = WebhookEvent::now(
            WebhookKind::ApprovalRequested,
            id.clone(),
            json!({ "approval_id": approval_id.as_ref() }),
        );
        webhook.emit(&event).await;
    }
    if !report.responses.is_empty() {
        let event = WebhookEvent::now(
            WebhookKind::WorkCompleted,
            id.clone(),
            json!({ "responses": report.responses.len() }),
        );
        webhook.emit(&event).await;
    }
}

/// Emits a `feedback.created` webhook. A no-op when no webhook is configured.
pub(crate) async fn emit_feedback_webhook(state: &AppState, id: &CompanyId, note: &str) {
    let Some(webhook) = state.webhook() else {
        return;
    };
    let event = WebhookEvent::now(
        WebhookKind::FeedbackCreated,
        id.clone(),
        json!({ "note": note }),
    );
    webhook.emit(&event).await;
}

/// Emits a `budget.exhausted` webhook. A no-op when no webhook is configured.
async fn emit_budget_exhausted(state: &AppState, id: &CompanyId) {
    let Some(webhook) = state.webhook() else {
        return;
    };
    let event = WebhookEvent::now(WebhookKind::BudgetExhausted, id.clone(), json!({}));
    webhook.emit(&event).await;
}

#[cfg(test)]
mod test;
