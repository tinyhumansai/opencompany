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
    /// Reads many chunk bodies at once: one entry per requested addr, in
    /// request order, `None` where nothing is stored (or a single read fails).
    ///
    /// `Err` is reserved for batch-level failures (connection, statement,
    /// enumeration); a chunk that is missing, or whose single read or decode
    /// fails, answers `None` so one bad row cannot fail a bulk read the
    /// caller degrades per-row anyway. The default loops [`Self::peek`] —
    /// exactly the call-site loop it replaces — and backends override it to
    /// shed the per-chunk overhead that loop pays. How much they shed is
    /// backend-specific, and only mongodb collapses to literally one read:
    /// it answers the batch with a single `$in` query, sqlite acquires one
    /// connection and prepares one statement it then executes per addr, and
    /// the provider facade reads its one enumerated partition once.
    async fn peek_many(&self, id: &CompanyId, addrs: &[ChunkAddr]) -> Result<Vec<Option<String>>> {
        let mut bodies = Vec::with_capacity(addrs.len());
        for addr in addrs {
            bodies.push(self.peek(id, addr, None).await.ok());
        }
        Ok(bodies)
    }
    /// Searches chunks for `query`, returning up to `limit` hits.
    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>>;
    /// Permanently removes the chunk at `addr`, returning whether it existed.
    ///
    /// Address-level: chunks are content-addressed, and one address can carry
    /// several labels (a byte-identical body stored under each — see
    /// [`Self::delete_label`] for the semantics that made every backend keep
    /// them), so the whole address goes — every label that pointed at that
    /// body. `false` means nothing was there, which callers surface honestly
    /// rather than treating as an error: a forget of something already gone is
    /// a no-op, not a fault.
    ///
    /// Required, no default — a defaulted `Ok(false)` would let a backend
    /// silently serve a forget that forgets nothing, the exact dishonesty the
    /// null-engine warnings exist to prevent.
    async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool>;
    /// Removes `label`'s claim on the chunk at `addr`, returning whether that
    /// (addr, label) pairing existed.
    ///
    /// Label-scoped where [`Self::delete`] is address-level (issue #1300).
    /// Chunks are content-addressed, so byte-identical bodies from different
    /// writers share one address under different labels; every backend keeps
    /// one index entry per (addr, label) — set semantics, a re-`put` of an
    /// identical (body, label) adds nothing — and this call removes exactly
    /// one such entry. The body is reaped when, and only when, the last label
    /// goes, and that reap is conditional *inside* the backend (a lock, a
    /// transaction, or a predicated delete): a concurrent `put` of identical
    /// content under another label can never lose its row to this call, which
    /// is the check-then-delete race the callers' old snapshot guards carried.
    ///
    /// `false` means the pairing was not there (wrong label, or already
    /// removed) — a no-op for idempotent retries, same as [`Self::delete`].
    ///
    /// # The default, and why it is not `Ok(false)`
    ///
    /// This port is publicly re-exported and `RuntimeBuilder::with_context`
    /// is an operator injection seam, so a required method here would be a
    /// source break for a backend maintained outside this crate. It is
    /// defaulted instead — but a defaulted `Ok(false)` would let such a
    /// backend silently serve a forget that forgets nothing, and a defaulted
    /// `delete` would let it delete a label the caller never owned, which is
    /// the griefing vector this method exists to close.
    ///
    /// So the default does the honest thing with only the other port methods:
    /// it reads the index, removes the address when `label` is its **only**
    /// claim, answers `false` when the pairing is not there, and refuses when
    /// the address is shared — the exact behaviour the callers had before
    /// label-scoped delete existed, and never worse than it. Its weakness is
    /// the one that motivated this method: the read and the delete are two
    /// steps, so a byte-identical write landing between them loses its row.
    /// **Every backend in this crate overrides it** with a version where the
    /// claim removal and the last-claim reap are one atomic step; a backend
    /// that does not should treat the override as the fix, not the default.
    async fn delete_label(&self, id: &CompanyId, addr: &ChunkAddr, label: &str) -> Result<bool> {
        let index = self.list(id, "").await?;
        let at_addr: Vec<&ChunkMeta> = index.iter().filter(|meta| &meta.addr == addr).collect();
        if !at_addr.iter().any(|meta| meta.label == label) {
            return Ok(false);
        }
        if at_addr.iter().all(|meta| meta.label == label) {
            return self.delete(id, addr).await;
        }
        Err(crate::error::OpenCompanyError::Store(format!(
            "this context backend cannot remove one label's claim on `{}`, and the address is \
             shared with another label, so removing it would take that one too; the backend \
             needs to implement ContextStore::delete_label",
            addr.as_ref()
        )))
    }
}

#[cfg(test)]
mod default_delete_label_tests {
    use super::*;
    use std::sync::Mutex;

    /// A backend that implements the port as it stood BEFORE label-scoped
    /// delete existed — no `delete_label` override. This is the shape an
    /// operator-supplied store injected through `RuntimeBuilder::with_context`
    /// has, and the reason the method is defaulted rather than required.
    #[derive(Default)]
    struct LegacyStore {
        rows: Mutex<Vec<(ChunkAddr, String)>>,
    }

    #[async_trait]
    impl ContextStore for LegacyStore {
        async fn put(&self, _: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
            let addr = ChunkAddr::new(crate::store::content_address(&chunk.body));
            self.rows.lock().unwrap().push((addr.clone(), chunk.label));
            Ok(addr)
        }
        async fn list(&self, _: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, label)| label.starts_with(prefix))
                .map(|(addr, label)| ChunkMeta {
                    addr: addr.clone(),
                    label: label.clone(),
                    len: 0,
                    stored_at_millis: 0,
                })
                .collect())
        }
        async fn peek(
            &self,
            _: &CompanyId,
            _: &ChunkAddr,
            _: Option<Range<usize>>,
        ) -> Result<String> {
            Ok(String::new())
        }
        async fn search(&self, _: &CompanyId, _: &str, _: usize) -> Result<Vec<ChunkHit>> {
            Ok(Vec::new())
        }
        async fn delete(&self, _: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|(have, _)| have != addr);
            Ok(rows.len() < before)
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    async fn put(store: &LegacyStore, label: &str, body: &str) -> ChunkAddr {
        store
            .put(
                &company(),
                ContextChunk {
                    label: label.to_string(),
                    body: body.to_string(),
                },
            )
            .await
            .unwrap()
    }

    /// The sole claim is removed with its body — the same outcome an
    /// overriding backend gives, so a legacy store keeps working.
    #[tokio::test]
    async fn the_default_removes_a_sole_claim() {
        let store = LegacyStore::default();
        let addr = put(&store, "notes/one", "body").await;
        assert!(
            store
                .delete_label(&company(), &addr, "notes/one")
                .await
                .unwrap()
        );
        assert!(store.list(&company(), "").await.unwrap().is_empty());
    }

    /// A pairing that is not there answers `false`, not an error — the
    /// idempotent-retry contract every backend shares.
    #[tokio::test]
    async fn the_default_answers_false_for_an_absent_pairing() {
        let store = LegacyStore::default();
        let addr = put(&store, "notes/one", "body").await;
        assert!(
            !store
                .delete_label(&company(), &addr, "notes/other")
                .await
                .unwrap()
        );
        assert_eq!(store.list(&company(), "").await.unwrap().len(), 1);
    }

    /// The case the griefing vector lives in: a shared address REFUSES rather
    /// than taking the other label's row with it. Never worse than the
    /// pre-#1300 callers, which refused here too — and emphatically not the
    /// over-delete a `delete`-shaped default would have produced.
    #[tokio::test]
    async fn the_default_refuses_a_shared_address_rather_than_over_deleting() {
        let store = LegacyStore::default();
        let addr = put(&store, "notes/mine", "same body").await;
        assert_eq!(put(&store, "notes/theirs", "same body").await, addr);

        let refused = store.delete_label(&company(), &addr, "notes/mine").await;
        assert!(
            refused.is_err(),
            "a shared address must refuse: {refused:?}"
        );
        assert_eq!(
            store.list(&company(), "").await.unwrap().len(),
            2,
            "neither claim may be removed by a refusal"
        );
    }
}
