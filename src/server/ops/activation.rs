//! `GET {scope}/activation` — the read projection over the account-activation
//! funnel (issue #1843): the shared answer the future onboarding gate (#1844)
//! and week-1 nudge (#1845) both poll, instead of each re-deriving it.
//!
//! Any member may read this — [`ScopedCompany`], not
//! [`AdminScopedCompany`](super::scope::AdminScopedCompany) — the same call
//! [`super::setup`] makes: this route decides nothing on the company's behalf,
//! it only answers a question, so the stricter guard would be a boundary this
//! route has no write to protect.
//!
//! Every call runs [`compute_and_latch`], which — see its own docs — is cheap
//! (no Composio round trip, no journal scan) once the company has already
//! activated, and is the write path that stamps the latch the first time every
//! step reads true.

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::AppState;
use crate::company::activation::{ActivationStatus, compute_and_latch};
use crate::company::runtime::CompanyRuntime;
use crate::server::error::ApiError;
use crate::server::ops::scope::{ScopedCompany, scoped};

/// Registers the activation route fragment under both addressing forms.
pub fn router() -> Router<AppState> {
    scoped("/activation", get(get_activation))
}

/// The funnel, as the console renders it: one boolean per step, the overall
/// verdict, and when the latch landed (`null` until it has).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivationDto {
    name_confirmed: bool,
    integration_connected: bool,
    workflow_run_succeeded: bool,
    /// The latch if set, else whether every step above currently reads `true`
    /// — see [`ActivationStatus::is_activated`] for why those are different
    /// questions with the same answer only up to the moment of activation.
    is_activated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    activation_completed_at_millis: Option<u64>,
}

impl From<ActivationStatus> for ActivationDto {
    fn from(status: ActivationStatus) -> Self {
        Self {
            name_confirmed: status.name_confirmed,
            integration_connected: status.integration_connected,
            workflow_run_succeeded: status.workflow_run_succeeded,
            is_activated: status.is_activated(),
            activation_completed_at_millis: status.activation_completed_at,
        }
    }
}

/// `GET {scope}/activation`.
async fn get_activation(company: ScopedCompany) -> Result<Json<ActivationDto>, ApiError> {
    // Composio connection state is fetched lazily, inside `compute_and_latch`,
    // and only when the company is not already latched — see that function's
    // docs. Awaiting it here unconditionally would pay for the round trip on
    // every poll of an already-activated company, defeating the fast path this
    // route's own module docs promise.
    let status = compute_and_latch(
        company.id(),
        company.runtime.store(),
        company.runtime.events(),
        async || has_composio_connection(&company.runtime).await,
    )
    .await
    .map_err(ApiError)?;
    Ok(Json(status.into()))
}

/// Whether the company holds at least one **live** Composio connection —
/// [`crate::company::activation::derive_steps`] separately ANDs this against
/// the `composio` namespace grant, so this answers only the connection half.
/// `Some(_)` either way: this build DOES have a client to ask, even when the
/// answer it gets back is "no". See the `#[cfg(not(feature = "composio"))]`
/// fallback below for the build that has no client at all.
///
/// A resolve or list failure reads as "no connection" rather than surfacing an
/// error: this route's job is to say where the operator stands in the funnel,
/// and a Composio outage should not turn that into a 500 — the operator sees
/// the integration step unmet, which is honest (a connection this route can't
/// currently confirm is not one an agent can currently use either).
#[cfg(feature = "composio")]
async fn has_composio_connection(runtime: &CompanyRuntime) -> Option<bool> {
    let Ok(config) = super::composio::resolve_tenant(runtime).await else {
        return Some(false);
    };
    Some(
        crate::harness::composio::list_connection_states(&config)
            .await
            .map(|states| states.iter().any(|(_, connected)| *connected))
            .unwrap_or(false),
    )
}

/// Without the `composio` feature there is no client to ask, and — the point
/// `None` exists to make — no company in THIS BUILD can ever hold a
/// connection either, `cargo run --bin opencompany -- serve` (AGENTS.md's own
/// documented default command) included. `None` carries that "no lever"
/// signal through to
/// [`derive_steps`](crate::company::activation::derive_steps), which waives
/// the integration step unconditionally for it — collapsing this to `Some(false)`
/// (the pre-#1850-review-finding-2 shape) would instead read as "not
/// connected yet" and permanently block activation for every company that
/// grants `composio` in the one build most operators actually run. Mirrors
/// the same no-client fallback shape `composio::fetch_catalog` answers its
/// own callers with.
#[cfg(not(feature = "composio"))]
async fn has_composio_connection(_runtime: &CompanyRuntime) -> Option<bool> {
    None
}
