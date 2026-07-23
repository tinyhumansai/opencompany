//! Memory-fact reads + writes: `GET /memory`, `GET /memory/stats`,
//! `POST /memory`, `DELETE /memory/{fact_id}` under both scope forms.
//!
//! Bodies mirror the console's `MemoryEntry` (`frontend/src/api/memory.ts`).
//! Facts land in the [`FactStore`](crate::ports::FactStore) — the console's
//! durable Memory/Brain view. A create *also* mirrors the fact into the
//! [`ContextStore`](crate::ports::ContextStore) the embedded agents recall from,
//! so an operator note is agent-recallable on the next turn (see
//! [`create_fact`]). A delete journals a [`CompanyEvent::MemoryFactDeleted`] to
//! the `EventLog` per the Operator-rights section of
//! `docs/spec/company-brain/memory.md`.
//!
//! ## Known limitation (flagged seam)
//!
//! [`ContextStore`] is append-only (no delete port), so deleting a fact removes
//! it from the `FactStore` and the console but its mirrored chunk stays
//! agent-recallable until a delete port lands. The `operator-fact/{id}` label
//! makes that future reap trivial. We deliberately do NOT fork the retrieval
//! path to work around this — the seam is documented, not hidden.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::facts::{FactKind, FactRecord};
use crate::ports::types::{CompanyEvent, ContextChunk};
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Label prefix for the [`ContextStore`](crate::ports::ContextStore) mirror of
/// an operator-authored fact. Keyed by fact id so a future delete port can reap
/// the mirror when the fact is deleted (today it lingers — see the module doc).
const OPERATOR_FACT_PREFIX: &str = "operator-fact";

/// Label prefix under which the harness stores completed task outcomes.
///
/// Mirrors `harness::memory_loop::OUTCOME_LABEL_PREFIX`, duplicated here because
/// that module is `openhuman`-gated while this route is always compiled. Kept in
/// sync by the `outcome_prefix_matches_harness` test under the feature.
const OUTCOME_LABEL_PREFIX: &str = "task-outcome";

/// Builds the memory route fragment.
pub fn router() -> Router<AppState> {
    scoped("/memory", post(create_fact).get(list_facts))
        .merge(scoped("/memory/stats", get(memory_stats)))
        .merge(scoped("/memory/{fact_id}", delete(delete_fact)))
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

/// Query params for `GET /memory`: an optional free-text `query` and `kind`
/// filter, both applied by the [`FactStore`](crate::ports::FactStore).
#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<FactKind>,
}

/// The sub-resource path (`fact_id`).
#[derive(Debug, Deserialize)]
struct FactPath {
    fact_id: String,
}

/// The Brain-tab health snapshot: how much the company remembers, across the
/// operator's durable facts and the agents' runtime context chunks. Lets the
/// console prove the store is live (non-fake) at a glance.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStats {
    /// Number of durable operator facts.
    facts: usize,
    /// The newest fact's last-updated epoch-millis (`0` when there are none).
    facts_updated_at_millis: u64,
    /// Total agent-accessible context chunks — learned context, task outcomes,
    /// and the operator-fact mirrors together.
    agent_chunks: usize,
    /// Of those chunks, how many are stored task outcomes.
    task_outcomes: usize,
}

/// `GET /memory` — the company's durable facts, newest-first, optionally
/// filtered by `?query=` (case-insensitive substring over title + body) and/or
/// `?kind=`. Same semantics as the GraphQL `Company.memory` resolver; the
/// console reads this instead of a client-side stub.
async fn list_facts(
    company: ScopedCompany,
    Query(ListQuery { query, kind }): Query<ListQuery>,
) -> Result<Json<Vec<MemoryEntry>>, ApiError> {
    let rows = company
        .runtime
        .facts()
        .list(company.id(), query.as_deref(), kind)
        .await?;
    Ok(Json(rows.into_iter().map(MemoryEntry::from).collect()))
}

/// `GET /memory/stats` — counts across the fact store and the agents' context
/// store, so the console's Brain health strip reflects the real backend.
async fn memory_stats(company: ScopedCompany) -> Result<Json<MemoryStats>, ApiError> {
    let facts = company
        .runtime
        .facts()
        .list(company.id(), None, None)
        .await?;
    // `list` is newest-first, so the head carries the freshest timestamp.
    let facts_updated_at_millis = facts.first().map(|f| f.updated_at_millis).unwrap_or(0);
    // Prefix `""` lists every chunk; the task-outcome prefix narrows to stored
    // outcomes (a subset of the total).
    let agent_chunks = company.runtime.context.list(company.id(), "").await?.len();
    let task_outcomes = company
        .runtime
        .context
        .list(company.id(), OUTCOME_LABEL_PREFIX)
        .await?
        .len();
    Ok(Json(MemoryStats {
        facts: facts.len(),
        facts_updated_at_millis,
        agent_chunks,
        task_outcomes,
    }))
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

    // Mirror the fact into the agents' ContextStore so an operator note becomes
    // recallable on the agent's next turn. The harness retrieve→inject step
    // searches the ContextStore (not the FactStore), so without this mirror an
    // operator-added fact would land in the console but never reach an agent —
    // the manual-ingest loop would stay open. Best-effort: the fact is already
    // durable, so a mirror failure degrades recall (logged) rather than failing
    // the operator's write. See the module doc for the append-only-delete seam.
    let chunk = ContextChunk {
        label: format!("{OPERATOR_FACT_PREFIX}/{}", record.id),
        body: format!("{}\n{}", record.title, record.body),
    };
    if let Err(err) = company.runtime.context.put(company.id(), chunk).await {
        tracing::warn!(
            company = %company.id(),
            fact = %record.id,
            error = %err,
            "operator-fact context mirror failed; fact saved but not agent-recallable"
        );
    }

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
        // Journal the operator deletion to the event log (audit trail). Note the
        // mirrored `operator-fact/{fact_id}` chunk is NOT removed here: the
        // ContextStore has no delete port, so it stays agent-recallable until
        // one lands (documented seam, module doc).
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

#[cfg(all(test, feature = "openhuman"))]
mod tests {
    /// The local prefix constant must track the harness's, since the two label
    /// the same chunks from opposite sides of the `openhuman` feature gate.
    #[test]
    fn outcome_prefix_matches_harness() {
        assert_eq!(
            super::OUTCOME_LABEL_PREFIX,
            crate::harness::memory_loop::OUTCOME_LABEL_PREFIX
        );
    }
}
