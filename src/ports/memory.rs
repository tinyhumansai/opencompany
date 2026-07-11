//! The [`MemoryStore`] port: compressed traces and task results.

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{CompanyId, CompressedTrace, EvictionPolicy, TaskResult};

/// Durable cycle memory: the kernel's equivalent of Medulla's
/// `CyclePersistence`.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Saves a compressed trace for a company.
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()>;
    /// Returns up to `limit` most-recent traces, newest last.
    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>>;
    /// Saves the result of a completed background task.
    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()>;
    /// Evicts memory per `policy`, returning the number of traces removed.
    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64>;
}
