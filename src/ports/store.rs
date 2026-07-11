//! The [`CompanyStore`] port: durable company records.

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
