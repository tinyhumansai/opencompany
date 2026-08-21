//! The [`ContextStore`] port: addressable chunks the brain queries lazily.

use std::ops::Range;

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{ChunkAddr, ChunkHit, ChunkMeta, ChunkWithBody, CompanyId, ContextChunk};

/// The RLM environment: addressable context chunks. Mirrors Medulla's
/// `ContextStore` port.
#[async_trait]
pub trait ContextStore: Send + Sync {
    /// Stores a chunk, returning its content address.
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr>;
    /// Lists chunk metadata under `prefix`.
    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>>;
    /// Lists chunk metadata under `prefix`, each paired with its body.
    ///
    /// For a caller that needs every body — the console's Brain list — this is
    /// one enumeration rather than a `list` plus one `peek` per row. The default
    /// preserves exactly that shape (`list` then a `peek` each) so every existing
    /// backend keeps working unchanged; the durable backends override it to read
    /// bodies in the same pass. A read fault surfaces rather than being laundered
    /// into a blank body — the durable overrides read each body in the row that
    /// already carries it, so there is no separate per-row read to lose.
    async fn list_with_bodies(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkWithBody>> {
        let metas = self.list(id, prefix).await?;
        let mut out = Vec::with_capacity(metas.len());
        for meta in metas {
            let body = self.peek(id, &meta.addr, None).await?;
            out.push(ChunkWithBody { meta, body });
        }
        Ok(out)
    }
    /// Reads a chunk (optionally a byte range) as text.
    async fn peek(
        &self,
        id: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String>;
    /// Searches chunks for `query`, returning up to `limit` hits.
    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>>;
    /// Permanently removes the chunk at `addr`, returning whether it existed.
    ///
    /// Address-level: chunks are content-addressed, so on backends where one
    /// address can carry several index entries (fs), the whole address goes —
    /// every label that pointed at that body. `false` means nothing was there,
    /// which callers surface honestly rather than treating as an error: a
    /// forget of something already gone is a no-op, not a fault.
    ///
    /// Required, no default — a defaulted `Ok(false)` would let a backend
    /// silently serve a forget that forgets nothing, the exact dishonesty the
    /// null-engine warnings exist to prevent.
    async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool>;
}
