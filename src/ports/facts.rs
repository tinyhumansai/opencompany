//! The [`FactStore`] port: the console's durable Memory view.
//!
//! Facts are the Operator's hand-curated, durable knowledge — preferences,
//! people, projects, references — distinct from the two runtime memory ports
//! in [`docs/spec/company-brain/memory.md`](../../docs/spec/company-brain/memory.md):
//! [`MemoryStore`](crate::ports::MemoryStore) holds cycle traces / task
//! results, and [`ContextStore`](crate::ports::ContextStore) is the RLM
//! environment. Per the Operator-rights section of that spec, deletes propagate
//! to the backing store and the deletion is journaled to the `EventLog` by the
//! caller.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// The kind of durable fact. Mirrors the console's `MemoryKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactKind {
    /// A standalone fact.
    Fact,
    /// A stated preference.
    Preference,
    /// A person the company knows.
    Person,
    /// A project the company is running.
    Project,
    /// A reference document or link.
    Reference,
}

/// One durable fact in the company's memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FactRecord {
    /// Stable id for the fact within the company.
    pub id: String,
    /// The fact's kind.
    pub kind: FactKind,
    /// A short title.
    pub title: String,
    /// The fact body.
    pub body: String,
    /// Which desk/teammate captured it.
    pub source: String,
    /// Epoch-millis timestamp of the last update.
    pub updated_at_millis: u64,
}

/// Durable per-company facts. Company A's facts MUST be invisible to company B.
#[async_trait]
pub trait FactStore: Send + Sync {
    /// Lists facts, most-recently-updated first, optionally filtered by a
    /// free-text `query` (case-insensitive substring over title + body) and/or
    /// a `kind`.
    async fn list(
        &self,
        company: &CompanyId,
        query: Option<&str>,
        kind: Option<FactKind>,
    ) -> Result<Vec<FactRecord>>;
    /// Inserts or replaces a fact by id.
    async fn upsert(&self, company: &CompanyId, fact: &FactRecord) -> Result<()>;
    /// Deletes a fact by id; returns whether a fact was removed.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}
