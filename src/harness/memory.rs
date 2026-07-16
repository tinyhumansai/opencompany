//! [`OcMemory`] — openhuman [`Memory`] over opencompany's [`ContextStore`].
//!
//! The harness gives each openhuman [`Agent`](super::CompanyAgent) a memory
//! backend that writes through opencompany's own persistence ports, so
//! everything an agent remembers stays inspectable / exportable via the same
//! [`ContextStore`] the rest of the platform reads (per the operator-rights
//! contract in `docs/spec/company-brain/memory.md`). Entries are namespaced
//! `{company}/{agent}/{namespace}` so multiple agents in one company never
//! collide.
//!
//! The backend is chosen by `OPENCOMPANY_STORAGE` (fs / sqlite / mongodb /
//! tinycortex), not by openhuman — that is the "memory pluggable and
//! configurable" requirement. On the fs backend
//! [`Memory::recall_relevant_by_vector`] has no vectors, so it degrades to the
//! store's substring/FTS `search` and, per the trait contract, **never
//! errors** — a failed search yields an empty recall rather than aborting the
//! turn.
//!
//! ## Known limitations (flagged seams)
//!
//! [`ContextStore`] is an append-only, content-addressed chunk store with no
//! delete and no per-key metadata, so a handful of [`Memory`] operations are
//! best-effort here:
//!
//! * [`Memory::forget`] cannot delete (no port) → returns `Ok(false)`.
//! * `category` / `session_id` / `taint` are not persisted, so list filters on
//!   them are ignored and recalled entries carry defaults.
//! * [`Memory::store`] appends; repeated keys are not coalesced.
//!
//! A `tinycortex`-backed [`ContextStore`] removes these gaps; the adapter code
//! is identical because it only speaks the port.

use std::sync::Arc;

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::memory::traits::{
    Memory, MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, RecallOpts,
};

use crate::ports::ContextStore;
use crate::ports::types::{CompanyId, ContextChunk};

/// openhuman [`Memory`] backed by an opencompany [`ContextStore`], namespaced to
/// one `{company}/{agent}` pair.
pub struct OcMemory {
    context: Arc<dyn ContextStore>,
    company: CompanyId,
    agent_id: String,
}

impl OcMemory {
    /// Builds a memory adapter scoped to `agent_id` within `company`.
    pub fn new(
        company: CompanyId,
        agent_id: impl Into<String>,
        context: Arc<dyn ContextStore>,
    ) -> Self {
        Self {
            context,
            company,
            agent_id: agent_id.into(),
        }
    }

    /// The `{company}/{agent}` scope prefix shared by every label this adapter
    /// writes.
    fn scope(&self) -> String {
        format!("{}/{}", self.company, self.agent_id)
    }

    /// Full label for a `(namespace, key)` pair: `{company}/{agent}/{ns}/{key}`.
    fn label_for(&self, namespace: &str, key: &str) -> String {
        format!("{}/{}/{}", self.scope(), namespace, key)
    }

    /// Prefix under which every entry in `namespace` lives.
    fn namespace_prefix(&self, namespace: &str) -> String {
        format!("{}/{}/", self.scope(), namespace)
    }

    /// Parse the `{ns}/{key}` tail of a label back into `(namespace, key)`.
    fn split_label<'a>(&self, label: &'a str) -> Option<(&'a str, &'a str)> {
        let scope = format!("{}/", self.scope());
        let tail = label.strip_prefix(&scope)?;
        let (ns, key) = tail.split_once('/')?;
        Some((ns, key))
    }

    /// Build a [`MemoryEntry`] from a stored chunk. `category`, `session_id`,
    /// and `taint` are not persisted by [`ContextStore`], so they take defaults.
    fn entry_from(
        &self,
        id: String,
        namespace: &str,
        key: &str,
        content: String,
        score: Option<f64>,
    ) -> MemoryEntry {
        MemoryEntry {
            id,
            key: key.to_string(),
            content,
            namespace: Some(namespace.to_string()),
            category: MemoryCategory::Core,
            timestamp: String::new(),
            session_id: None,
            score,
            taint: MemoryTaint::Internal,
        }
    }
}

#[async_trait]
impl Memory for OcMemory {
    fn name(&self) -> &str {
        "opencompany-context"
    }

    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let chunk = ContextChunk {
            label: self.label_for(namespace, key),
            body: content.to_string(),
        };
        self.context
            .put(&self.company, chunk)
            .await
            .map_err(|e| anyhow::anyhow!("context put failed: {e}"))?;
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let hits = self
            .context
            .search(&self.company, query, limit)
            .await
            .map_err(|e| anyhow::anyhow!("context search failed: {e}"))?;
        let min = opts.min_score.unwrap_or(0.0);
        let namespace = opts.namespace.unwrap_or("global");
        Ok(hits
            .into_iter()
            .filter(|h| h.score >= min)
            .map(|h| {
                let id: String = h.addr.as_ref().to_string();
                self.entry_from(id, namespace, "", h.snippet, Some(h.score))
            })
            .collect())
    }

    async fn recall_relevant_by_vector(
        &self,
        _namespace: &str,
        query: &str,
        limit: usize,
        min_vector_similarity: f64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        // fs backend has no vectors — degrade to substring/FTS search and never
        // error (per the trait contract). A failed search yields an empty recall.
        let hits = match self.context.search(&self.company, query, limit).await {
            Ok(hits) => hits,
            Err(e) => {
                log::debug!("[harness::memory] vector recall degraded search failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(hits
            .into_iter()
            .filter(|h| h.score >= min_vector_similarity)
            .map(|h| {
                let addr: String = h.addr.as_ref().to_string();
                (addr, h.snippet)
            })
            .collect())
    }

    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let label = self.label_for(namespace, key);
        let metas = self
            .context
            .list(&self.company, &label)
            .await
            .map_err(|e| anyhow::anyhow!("context list failed: {e}"))?;
        let Some(meta) = metas.into_iter().find(|m| m.label == label) else {
            return Ok(None);
        };
        let content = self
            .context
            .peek(&self.company, &meta.addr, None)
            .await
            .map_err(|e| anyhow::anyhow!("context peek failed: {e}"))?;
        let id: String = meta.addr.as_ref().to_string();
        Ok(Some(self.entry_from(id, namespace, key, content, None)))
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        // `category` / `session_id` are not persisted by ContextStore, so those
        // filters are ignored here (best-effort — documented seam).
        let prefix = match namespace {
            Some(ns) => self.namespace_prefix(ns),
            None => format!("{}/", self.scope()),
        };
        let metas = self
            .context
            .list(&self.company, &prefix)
            .await
            .map_err(|e| anyhow::anyhow!("context list failed: {e}"))?;
        let mut entries = Vec::with_capacity(metas.len());
        for meta in metas {
            let Some((ns, key)) = self.split_label(&meta.label) else {
                continue;
            };
            let (ns, key) = (ns.to_string(), key.to_string());
            let content = self
                .context
                .peek(&self.company, &meta.addr, None)
                .await
                .map_err(|e| anyhow::anyhow!("context peek failed: {e}"))?;
            let id: String = meta.addr.as_ref().to_string();
            entries.push(self.entry_from(id, &ns, &key, content, None));
        }
        Ok(entries)
    }

    async fn forget(&self, _namespace: &str, _key: &str) -> anyhow::Result<bool> {
        // ContextStore is append-only (no delete port), so forget cannot remove
        // the entry. Report "not deleted" rather than lying about success.
        log::debug!("[harness::memory] forget is a no-op: ContextStore has no delete");
        Ok(false)
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        let prefix = format!("{}/", self.scope());
        let metas = self
            .context
            .list(&self.company, &prefix)
            .await
            .map_err(|e| anyhow::anyhow!("context list failed: {e}"))?;
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for meta in metas {
            if let Some((ns, _key)) = self.split_label(&meta.label) {
                *counts.entry(ns.to_string()).or_default() += 1;
            }
        }
        Ok(counts
            .into_iter()
            .map(|(namespace, count)| NamespaceSummary {
                namespace,
                count,
                last_updated: None,
            })
            .collect())
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let prefix = format!("{}/", self.scope());
        let metas = self
            .context
            .list(&self.company, &prefix)
            .await
            .map_err(|e| anyhow::anyhow!("context list failed: {e}"))?;
        Ok(metas.len())
    }

    async fn health_check(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::ports::types::{ChunkAddr, ChunkHit, ChunkMeta};

    /// Minimal in-memory ContextStore for adapter isolation tests.
    #[derive(Default)]
    struct MockContext {
        chunks: Mutex<Vec<(ChunkAddr, ContextChunk)>>,
    }

    #[async_trait]
    impl ContextStore for MockContext {
        async fn put(&self, _id: &CompanyId, chunk: ContextChunk) -> crate::Result<ChunkAddr> {
            let mut guard = self.chunks.lock().unwrap();
            let addr = ChunkAddr::new(format!("addr-{}", guard.len()));
            guard.push((addr.clone(), chunk));
            Ok(addr)
        }

        async fn list(&self, _id: &CompanyId, prefix: &str) -> crate::Result<Vec<ChunkMeta>> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .filter(|(_, c)| c.label.starts_with(prefix))
                .map(|(addr, c)| ChunkMeta {
                    addr: addr.clone(),
                    label: c.label.clone(),
                    len: c.body.len(),
                })
                .collect())
        }

        async fn peek(
            &self,
            _id: &CompanyId,
            addr: &ChunkAddr,
            _range: Option<std::ops::Range<usize>>,
        ) -> crate::Result<String> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .find(|(a, _)| a == addr)
                .map(|(_, c)| c.body.clone())
                .unwrap_or_default())
        }

        async fn search(
            &self,
            _id: &CompanyId,
            query: &str,
            limit: usize,
        ) -> crate::Result<Vec<ChunkHit>> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .filter(|(_, c)| c.body.contains(query))
                .take(limit)
                .map(|(addr, c)| ChunkHit {
                    addr: addr.clone(),
                    snippet: c.body.clone(),
                    score: 1.0,
                })
                .collect())
        }
    }

    fn memory() -> OcMemory {
        OcMemory::new(
            CompanyId::new("acme"),
            "ceo",
            Arc::new(MockContext::default()),
        )
    }

    #[tokio::test]
    async fn store_and_get_roundtrip() {
        let mem = memory();
        mem.store("global", "fav_lang", "Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let got = mem.get("global", "fav_lang").await.unwrap().unwrap();
        assert_eq!(got.key, "fav_lang");
        assert_eq!(got.content, "Rust");
        assert_eq!(got.namespace.as_deref(), Some("global"));
    }

    #[tokio::test]
    async fn namespacing_isolates_agents_in_the_same_company() {
        let context: Arc<dyn ContextStore> = Arc::new(MockContext::default());
        let ceo = OcMemory::new(CompanyId::new("acme"), "ceo", context.clone());
        let cfo = OcMemory::new(CompanyId::new("acme"), "cfo", context.clone());

        ceo.store("global", "secret", "ceo-only", MemoryCategory::Core, None)
            .await
            .unwrap();

        // The CFO shares the company + ContextStore but must not see the CEO's
        // namespaced entry.
        assert!(cfo.get("global", "secret").await.unwrap().is_none());
        assert_eq!(ceo.count().await.unwrap(), 1);
        assert_eq!(cfo.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn recall_matches_by_substring() {
        let mem = memory();
        mem.store(
            "global",
            "note",
            "the quarterly report is ready",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        let hits = mem
            .recall("quarterly", 5, RecallOpts::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("quarterly"));
    }

    #[tokio::test]
    async fn vector_recall_degrades_and_never_errors() {
        let mem = memory();
        mem.store(
            "global",
            "note",
            "vector fallback body",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        let hits = mem
            .recall_relevant_by_vector("global", "fallback", 5, 0.5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "vector fallback body");
    }

    #[tokio::test]
    async fn forget_is_a_no_op_without_a_delete_port() {
        let mem = memory();
        mem.store("global", "k", "v", MemoryCategory::Core, None)
            .await
            .unwrap();
        assert!(!mem.forget("global", "k").await.unwrap());
    }

    #[tokio::test]
    async fn namespace_summaries_group_by_namespace() {
        let mem = memory();
        mem.store("global", "a", "1", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("global", "b", "2", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("user_profile", "c", "3", MemoryCategory::Core, None)
            .await
            .unwrap();
        let summaries = mem.namespace_summaries().await.unwrap();
        assert_eq!(summaries.len(), 2);
        let global = summaries.iter().find(|s| s.namespace == "global").unwrap();
        assert_eq!(global.count, 2);
    }
}
