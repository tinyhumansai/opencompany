//! The [`ContextStore`] port: addressable chunks the brain queries lazily.

use std::ops::Range;

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{ChunkAddr, ChunkHit, ChunkMeta, CompanyId, ContextChunk};

/// The RLM environment: addressable context chunks. Mirrors Medulla's
/// `ContextStore` port.
#[async_trait]
pub trait ContextStore: Send + Sync {
    /// Stores a chunk, returning its content address.
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr>;
    /// Lists chunk metadata under `prefix`.
    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>>;
    /// Reads a chunk (optionally a byte range) as text.
    async fn peek(
        &self,
        id: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String>;
    /// Searches chunks for `query`, returning up to `limit` hits.
    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>>;
}
