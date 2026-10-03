//! Typed facades over one [`MemoryEngine`].
//!
//! The three memory ports stay, because their types are the company's
//! vocabulary and every call site is written against them. What collapses is the
//! *backends*: instead of three independent stores, all three ports become thin
//! views onto a single bound engine.
//!
//! ## How a keyed record maps onto an item
//!
//! The ports are keyed (a fact id, a chunk address, a cycle id); the v2 contract
//! is not — `store` mints a content fingerprint and there is no `get(key)`. So
//! every record is one `Document` item whose metadata carries the addressing:
//!
//! - `workspace` = the record's [`Namespace`] (company root + scope), and
//! - `source.id` = the port's key.
//!
//! Those are exactly the two fields a hosted engine narrows on server-side
//! (CortexDB labels them), so a keyed read is one narrowed listing, not a walk.
//! A rewrite stores the new body first and forgets the old items second: a
//! crash in between leaves a duplicate the next read reconciles (newest
//! `observed_at` wins), never a lost record.
//!
//! ## Why the records are JSON, not item-native structure
//!
//! The port records are richer than any item kind (a trace, a fact with a
//! kind and a timestamp, a chunk with a label set), so the facade owns the
//! encoding and each one carries a round-trip test.
//!
//! ## Every read is re-checked against the namespace it asked for
//!
//! An engine is somebody else's code — increasingly, somebody else's *service*.
//! Asking for a workspace and trusting the answer to be within it is exactly
//! the assumption a hosted engine is in a position to violate, by bug or
//! otherwise. So every decode path drops items whose reported workspace is not
//! the one this facade owns. The filter should never fire; if it does, the
//! alternative was serving one tenant another's memory.

mod bound;
mod traces;

use std::collections::HashMap;
use std::ops::Range;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use bound::EXTERNAL_TAG;
use bound::decode;
pub(super) use bound::{Bound, ENVELOPE_MIME, Provenance};
pub use traces::ProviderMemoryStore;

use crate::error::OpenCompanyError;
use crate::ports::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyId, ContextChunk, ContextStore, FactKind, FactRecord,
    FactStore,
};
use crate::store::text::{ceil_boundary, slice_on_char_boundaries};
use crate::{Result, store::content_address};

// ---------------------------------------------------------------------------
// FactStore
// ---------------------------------------------------------------------------

/// The operator's hand-curated facts.
///
/// The closest fit of the three ports: `list`/`upsert`/`delete` map onto
/// `list`/`store`/`forget` almost exactly, and `forget` already returns the
/// bool `delete` needs.
pub struct ProviderFactStore {
    bound: Bound,
}

impl ProviderFactStore {
    pub(super) fn new(bound: Bound) -> Self {
        Self { bound }
    }
}

#[async_trait]
impl FactStore for ProviderFactStore {
    async fn list(
        &self,
        company: &CompanyId,
        query: Option<&str>,
        kind: Option<FactKind>,
    ) -> Result<Vec<FactRecord>> {
        let mut facts: Vec<FactRecord> = self.bound.list(company).await?;
        if let Some(kind) = kind {
            facts.retain(|fact| fact.kind == kind);
        }
        if let Some(needle) = query.map(str::trim).filter(|q| !q.is_empty()) {
            let needle = needle.to_lowercase();
            facts.retain(|fact| {
                fact.title.to_lowercase().contains(&needle)
                    || fact.body.to_lowercase().contains(&needle)
            });
        }
        // Most-recently-updated first, with the id as a tiebreak so the order is
        // total: two facts saved in the same millisecond must not swap places
        // between calls, or the console list flickers.
        facts.sort_by(|a, b| {
            b.updated_at_millis
                .cmp(&a.updated_at_millis)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(facts)
    }

    async fn upsert(&self, company: &CompanyId, fact: &FactRecord) -> Result<()> {
        self.bound.put(company, &fact.id, fact, "fact").await
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        self.bound.forget(company, id).await
    }
}

// ---------------------------------------------------------------------------
// ContextStore
// ---------------------------------------------------------------------------

/// The RLM environment.
///
/// Two host-side gaps the contract does not cover, both called out in
/// `docs/spec/runtime/orchestration/memory.md`:
///
/// - **Ranged `peek`** becomes a slice after a whole-entry read. The contract
///   has no ranged accessor, and inventing one per driver would be worse than
///   reading a chunk that is already bounded by construction.
/// - **`list` by label prefix** is a host-side filter, for the same reason.
pub struct ProviderContextStore {
    bound: Bound,
    /// Serializes the label-set read-merge-writes (`put`, `delete_label`) on
    /// the stored envelope (#1300). The contract's `store` is a whole-value
    /// upsert with no compare-and-set, so two concurrent puts of one body
    /// under different labels would otherwise both read the same envelope and
    /// one label would silently lose — the same reasoning as the fs backend's
    /// per-path lock, and process-local for the same reason it is there: this
    /// facade is the company's only writer of its partition.
    label_lock: tokio::sync::Mutex<()>,
}

impl ProviderContextStore {
    pub(super) fn new(bound: Bound) -> Self {
        Self {
            bound,
            label_lock: tokio::sync::Mutex::new(()),
        }
    }
}

/// A stored chunk: the port's [`ContextChunk`] plus the metadata `list` reports.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredChunk {
    /// The first label to claim this address — kept meaningful on its own so
    /// an envelope written by (or later read by) a binary from before
    /// `labels` existed still carries a real claim.
    label: String,
    body: String,
    stored_at_millis: u64,
    /// Every label claiming this address (#1300); envelopes from before the
    /// field decode empty, and [`stored_labels`] unions the scalar back in.
    #[serde(default)]
    labels: Vec<String>,
}

/// Every label claiming `chunk`, deduped, scalar (first-stored) label first.
fn stored_labels(chunk: &StoredChunk) -> Vec<String> {
    let mut labels = vec![chunk.label.clone()];
    for label in &chunk.labels {
        if !labels.iter().any(|have| have == label) {
            labels.push(label.clone());
        }
    }
    labels
}

#[async_trait]
impl ContextStore for ProviderContextStore {
    async fn put(&self, company: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
        // The shared content address, so this backend mints the same addr for
        // the same body as fs / sqlite / mongodb do.
        let addr = content_address(&chunk.body);
        // Under the label lock: the merge below is a read-merge-write over a
        // plain upsert (#1300).
        let _guard = self.label_lock.lock().await;
        // Chunks are append-only and never rewritten. `store` is an upsert, so
        // without this check a re-`put` of an identical body would restamp
        // `stored_at_millis` and move the Brain header's "last updated" backwards
        // in meaning — it would start reporting when a chunk was last *re-seen*
        // rather than when it was first written. sqlite and mongodb keep the
        // first write; match them. A new label on an existing body is folded
        // into the envelope's label set instead — one claim per (addr, label).
        if let Some(existing) = self.bound.get::<StoredChunk>(company, &addr).await? {
            // A hit is almost always the same body written twice. It can also be
            // a content-address collision: `content_address` is a 64-bit
            // non-cryptographic hash (`crate::store::content_address`, shared by
            // every backend), so two different bodies can mint one address. That
            // is a pre-existing property of the address scheme rather than
            // anything this facade introduces — sqlite and mongodb keep the
            // first write for a collision exactly as this does, and changing the
            // scheme would move every existing chunk's address on every backend.
            //
            // What is worth doing here is refusing to be *silent* about it. On a
            // collision `peek(addr)` returns a body the caller never wrote, and
            // an operator debugging that has no way to reach this conclusion
            // from the outside. So compare, and say so when they differ.
            if existing.body != chunk.body {
                tracing::error!(
                    addr = %addr,
                    label = %chunk.label,
                    existing_label = %existing.label,
                    "content-address collision: two different chunk bodies hashed to the same \
                     address. The first body is kept and this write is dropped, so reads of this \
                     address return the earlier chunk. See crate::store::content_address."
                );
                return Ok(ChunkAddr::new(addr));
            }
            let labels = stored_labels(&existing);
            if !labels.iter().any(|have| have == &chunk.label) {
                let mut updated = existing;
                updated.labels = labels;
                updated.labels.push(chunk.label);
                self.bound.put(company, &addr, &updated, "chunk").await?;
            }
            return Ok(ChunkAddr::new(addr));
        }
        let stored = StoredChunk {
            labels: vec![chunk.label.clone()],
            label: chunk.label,
            body: chunk.body,
            stored_at_millis: crate::ports::now_millis(),
        };
        self.bound.put(company, &addr, &stored, "chunk").await?;
        Ok(ChunkAddr::new(addr))
    }

    async fn list(&self, company: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let chunks: Vec<StoredChunk> = self.bound.list(company).await?;
        let mut metas: Vec<ChunkMeta> = chunks
            .into_iter()
            .flat_map(|chunk| {
                // One meta per label claiming the address (#1300); the stamp
                // is the address's first write, since the envelope is one
                // record however many labels claim it.
                let addr = content_address(&chunk.body);
                let len = chunk.body.len();
                let stored_at_millis = chunk.stored_at_millis;
                stored_labels(&chunk)
                    .into_iter()
                    .filter(|label| label.starts_with(prefix))
                    .map(move |label| ChunkMeta {
                        addr: ChunkAddr::new(addr.clone()),
                        label,
                        len,
                        stored_at_millis,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        metas.sort_by(|a, b| {
            a.stored_at_millis
                .cmp(&b.stored_at_millis)
                .then_with(|| a.addr.as_ref().cmp(b.addr.as_ref()))
                // The label completes the order: two labels claiming one
                // address (#1300) share a stamp and an addr, so without it
                // their relative order would rest on enumeration order alone.
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(metas)
    }

    async fn peek(
        &self,
        company: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String> {
        let chunk: StoredChunk =
            self.bound
                .get(company, addr.as_ref())
                .await?
                .ok_or_else(|| {
                    OpenCompanyError::NotFound(format!("context chunk {}", addr.as_ref()))
                })?;
        let Some(range) = range else {
            return Ok(chunk.body);
        };
        Ok(slice_on_char_boundaries(&chunk.body, range))
    }

    async fn peek_many(
        &self,
        company: &CompanyId,
        addrs: &[ChunkAddr],
    ) -> Result<Vec<Option<String>>> {
        // `bound.list` already decodes every body in the partition, so one
        // enumeration answers the whole batch — the default's per-addr `peek`
        // would walk the provider once per chunk for the same bytes.
        let chunks: Vec<StoredChunk> = self.bound.list(company).await?;
        let by_addr: HashMap<String, String> = chunks
            .into_iter()
            .map(|chunk| (content_address(&chunk.body), chunk.body))
            .collect();
        Ok(addrs
            .iter()
            .map(|addr| by_addr.get(addr.as_ref()).cloned())
            .collect())
    }

    async fn delete(&self, company: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
        // Under the label lock so an interleaved `put`'s read-merge-write
        // cannot resurrect an envelope this is removing.
        let _guard = self.label_lock.lock().await;
        // The engine keys chunks by their content address (see `put`), so the
        // port's addr IS the engine key — the envelope goes with every label
        // claiming it. On an address collision (64-bit non-cryptographic hash
        // — see `put`'s comment) the single stored body goes, whichever writer
        // minted it first; that is the same first-write-wins property every
        // backend already has.
        self.bound.forget(company, addr.as_ref()).await
    }

    async fn delete_label(
        &self,
        company: &CompanyId,
        addr: &ChunkAddr,
        label: &str,
    ) -> Result<bool> {
        // Label-scoped (#1300): remove one claim from the envelope's label
        // set, and forget the envelope exactly when the last claim goes. The
        // read-merge-write and the reap decision sit under the same lock every
        // put holds, so a concurrent put of identical content under another
        // label either lands its claim before this read or re-creates the
        // envelope after the forget — never loses its claim in between.
        let _guard = self.label_lock.lock().await;
        let Some(existing) = self
            .bound
            .get::<StoredChunk>(company, addr.as_ref())
            .await?
        else {
            // `get` answering `None` is two different facts, and only one of
            // them is "nothing to forget". If the engine DOES hold a record
            // here, this build simply cannot read its envelope — and returning
            // `Ok(false)` would tell `memory_forget` to reply "already gone"
            // about a chunk recall keeps serving, with nothing anywhere saying
            // otherwise. Refuse instead, naming the address, so the operator
            // gets a report rather than a lie.
            //
            // Deliberately NOT a forget-by-key fallback: the envelope is what
            // says which labels claim this address, so an unreadable one means
            // an unknown claim set, and removing the record could take a label
            // this caller never owned.
            if self.bound.exists(company, addr.as_ref()).await? {
                return Err(OpenCompanyError::Store(format!(
                    "context chunk {} exists but its envelope could not be decoded, so its \
                     label claims are unknown and `{label}` cannot be removed safely; the \
                     record needs repair or an operator-level delete",
                    addr.as_ref()
                )));
            }
            return Ok(false);
        };
        let mut labels = stored_labels(&existing);
        let before = labels.len();
        labels.retain(|have| have != label);
        if labels.len() == before {
            return Ok(false);
        }
        if labels.is_empty() {
            // The claim existed and is gone either way: `forget` answering
            // false here means another writer (a second process on a remote
            // driver, outside this process-local lock) reaped the envelope
            // first, which is the same end state.
            self.bound.forget(company, addr.as_ref()).await?;
            return Ok(true);
        }
        let updated = StoredChunk {
            // The scalar stays a real label so an envelope read by a binary
            // from before `labels` still carries a live claim.
            label: labels[0].clone(),
            labels,
            body: existing.body,
            stored_at_millis: existing.stored_at_millis,
        };
        self.bound
            .put(company, addr.as_ref(), &updated, "chunk")
            .await?;
        Ok(true)
    }

    async fn search(
        &self,
        company: &CompanyId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ChunkHit>> {
        let (namespace, hits) = self.bound.search(company, query, limit).await?;
        Ok(hits
            .iter()
            .filter_map(|hit| {
                let chunk: StoredChunk = decode(hit, &namespace)?;
                Some(ChunkHit {
                    addr: ChunkAddr::new(content_address(&chunk.body)),
                    snippet: snippet(&chunk.body),
                    // The port promises `[0, 1]`; engines rank on their own
                    // scale, so clamp rather than trust it.
                    score: f64::from(hit.score).clamp(0.0, 1.0),
                })
            })
            .take(limit)
            .collect())
    }
}

/// The leading window of a body, used as a search snippet.
fn snippet(body: &str) -> String {
    const MAX: usize = 200;
    if body.len() <= MAX {
        return body.to_string();
    }
    body[..ceil_boundary(body, MAX)].to_string()
}

#[cfg(test)]
#[path = "facades_tests.rs"]
mod tests;
