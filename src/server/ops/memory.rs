//! Memory-fact writes: `POST /memory`, `DELETE /memory/{fact_id}` under both
//! scope forms.
//!
//! Bodies mirror the console's `MemoryEntry` (`frontend/src/lib/memory.ts`).
//! Writes land in the [`FactStore`](crate::ports::FactStore); a delete also
//! journals a [`CompanyEvent::MemoryFactDeleted`] to the `EventLog` per the
//! Operator-rights section of `docs/spec/company-brain/memory.md`.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::facts::{FactKind, FactRecord};
use crate::ports::types::CompanyEvent;
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the memory route fragment.
pub fn router() -> Router<AppState> {
    scoped("/memory", post(create_fact)).merge(scoped("/memory/{fact_id}", delete(delete_fact)))
}

/// A durable memory entry as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryEntry {
    id: String,
    kind: FactKind,
    title: String,
    body: String,
    source: String,
    updated_at: u64,
}

impl From<FactRecord> for MemoryEntry {
    fn from(f: FactRecord) -> Self {
        Self {
            id: f.id,
            kind: f.kind,
            title: f.title,
            body: f.body,
            source: f.source,
            updated_at: f.updated_at_millis,
        }
    }
}

/// The create-fact body.
#[derive(Debug, Deserialize)]
struct CreateFact {
    kind: FactKind,
    title: String,
    body: String,
    #[serde(default)]
    source: Option<String>,
}

/// The sub-resource path (`fact_id`).
#[derive(Debug, Deserialize)]
struct FactPath {
    fact_id: String,
}

async fn create_fact(
    company: ScopedCompany,
    Json(body): Json<CreateFact>,
) -> Result<Json<MemoryEntry>, ApiError> {
    let record = FactRecord {
        id: generate_id(),
        kind: body.kind,
        title: body.title,
        body: body.body,
        source: body.source.unwrap_or_else(|| "You".to_string()),
        updated_at_millis: now_millis(),
    };
    company
        .runtime
        .facts()
        .upsert(company.id(), &record)
        .await?;
    Ok(Json(record.into()))
}

async fn delete_fact(
    company: ScopedCompany,
    Path(FactPath { fact_id }): Path<FactPath>,
) -> Result<StatusCode, ApiError> {
    if company
        .runtime
        .facts()
        .delete(company.id(), &fact_id)
        .await?
    {
        // Journal the operator deletion to the event log (audit trail).
        company
            .runtime
            .events()
            .append(
                company.id(),
                CompanyEvent::MemoryFactDeleted {
                    fact_id: fact_id.clone(),
                },
            )
            .await?;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "fact {fact_id}"
        ))))
    }
}
