//! REST finances read surface (Phase 1): `GET …/finances`.
//!
//! A REST twin of the `Company.finances` GraphQL resolver
//! (`graphql/finances.rs`). The operator console is REST-only and ships no
//! GraphQL client, so the Finances view could only render sample data; this
//! exposes the same pure projection
//! ([`finances_from`](crate::metering::finances_from)) — the ledger + the
//! manifest `[budget]` folded into balance, budget-vs-spend, revenue, spend by
//! category, and the transaction journal. No ledger writes: the ledger stays the
//! single financial source of truth and this only reads it.
//!
//! Data maturity: the ledger fills on a harness (`openhuman`-feature) build via
//! `src/harness/cost.rs` (an `inference.spend` line per billed turn); the
//! offline build has no cost hook, so the journal is empty and every figure is
//! (correctly) zero. `balanceUsd` is the bookkeeping net of the ledger.

use axum::Json;
use axum::Router;
use axum::routing::get;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::metering::{Finances, finances_from};
use crate::ports::now_millis;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the finances read route fragment (both scope forms).
pub fn router() -> Router<AppState> {
    scoped("/finances", get(get_finances))
}

/// Loads the ledger + budget and projects a company's finances.
///
/// [`Finances`] is itself the camelCase DTO the console keys off (`balanceUsd`,
/// `budgetUsd`, `spentUsd`, `revenueUsd`, `netUsd`, `byCategory`,
/// `transactions`), so it serializes straight to the wire.
async fn project_finances(runtime: &CompanyRuntime) -> Result<Finances, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let (ledger, budget) = match &record {
        Some(record) => (record.ledger.clone(), record.manifest.budget.clone()),
        None => (Vec::new(), crate::company::Budget::default()),
    };
    Ok(finances_from(&ledger, &budget, now_millis()))
}

/// `GET …/finances` — the company's finance read surface.
async fn get_finances(company: ScopedCompany) -> Result<Json<Finances>, ApiError> {
    Ok(Json(project_finances(company.runtime.as_ref()).await?))
}

#[cfg(test)]
#[path = "finances_tests.rs"]
mod tests;
