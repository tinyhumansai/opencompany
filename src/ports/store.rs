//! The [`CompanyStore`] port: durable company records.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{CompanyId, CompanyRecord, CompanySummary, LedgerEntry};

/// Durable company records: charter, roster, ledger, approval queue.
#[async_trait]
pub trait CompanyStore: Send + Sync {
    /// Loads a company record, or `None` if it does not exist.
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>>;
    /// Persists a company record.
    async fn save(&self, record: &CompanyRecord) -> Result<()>;
    /// Lists all known companies.
    async fn list(&self) -> Result<Vec<CompanySummary>>;
    /// Appends one entry to a company's ledger.
    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()>;
}

/// Per-company write serialization: a shared mutex map, keyed by company id, so
/// the orchestrator's `add_agent` tool and the console `POST .../team` route can
/// never clobber each other's `overlay_agents` list with concurrent
/// load→push→save cycles.
static COMPANY_WRITE_LOCKS: LazyLock<Mutex<HashMap<CompanyId, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns (or creates) the per-company write mutex for `company`, so callers
/// that do a `CompanyStore` load→mutate→save cycle can serialise their writes
/// against other concurrent writers.
pub(crate) fn company_write_lock(company: &CompanyId) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = COMPANY_WRITE_LOCKS.lock().expect("company write locks");
    map.entry(company.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}
