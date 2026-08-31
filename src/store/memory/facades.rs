//! Typed facades over one [`MemoryProvider`].
//!
//! The three memory ports stay, because their types are the company's
//! vocabulary and every call site is written against them. What collapses is the
//! *backends*: instead of three independent stores, all three ports become thin
//! views onto a single bound provider.
//!
//! ## Why the records are JSON, not provider-native structure
//!
//! `MemoryCore::store` takes `content: &str`. The contract does have a
//! documents family that could carry structure natively — but it is optional,
//! and the composition every driver we can actually bind goes through
//! (`MemoryTraitProvider`) advertises exactly the three mandatory families and
//! leaves `as_documents()` at `None`. Encoding here rather than reaching for a
//! family the bound driver may not have is what keeps one facade working against
//! the embedded engine and a hosted service alike; the price is that the facade
//! owns the encoding, so each one carries a round-trip test.
//!
//! ## Every read is re-checked against the namespace it asked for
//!
//! A driver is somebody else's code — increasingly, somebody else's *service*.
//! Asking for a namespace and trusting the answer to be within it is exactly
//! the assumption a hosted engine is in a position to violate, by bug or
//! otherwise. So every decode path drops entries whose reported namespace falls
//! outside the one this facade owns (`Namespace::contains`). The filter should
//! never fire; if it does, the alternative was serving one tenant another's
//! memory.

use std::collections::HashMap;
use std::ops::Range;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tinymemory_api::error::MemoryError;
use tinymemory_api::provider::MemoryProvider;
use tinymemory_api::types::{MemoryCategory, MemoryEntry, MemoryTaint};

use super::namespace::{Namespace, Scope};
use crate::error::OpenCompanyError;
use crate::ports::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyId, CompressedTrace, ContextChunk, ContextStore,
    EvictionPolicy, FactKind, FactRecord, FactStore, MemoryStore, TaskResult,
};
use crate::runtime::maintenance::TRACE_RETENTION_LIMIT;
use crate::store::text::{ceil_boundary, slice_on_char_boundaries};
use crate::{Result, store::content_address};

/// Envelope version. Bumped only if the on-the-wire shape of a record changes
/// incompatibly; a decoder that meets a version it does not know refuses rather
/// than guessing, because a half-understood memory record is worse than a
/// missing one.
const ENVELOPE_VERSION: u8 = 1;

/// The wire form of a typed port record inside a provider entry's `content`.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope<T> {
    /// Format version — see [`ENVELOPE_VERSION`].
    v: u8,
    /// The port's own record, verbatim.
    record: T,
}

/// Characters a hosted engine removes from `content`, escaped on the way out.
///
/// Supermemory strips `U+FFFD` server-side (tinymemory#80, measured against
/// the live API rather than inferred). An engine is within its rights to
/// sanitise text it is handed; what breaks is that this host does not hand it
/// text, it hands it a JSON envelope, and a character removed from the middle
/// of that envelope comes back as a record whose body is quietly one character
/// shorter than it was written.
///
/// `U+0000` is deliberately absent: RFC 8259 requires escaping `U+0000`
/// through `U+001F`, so `serde_json` already emits it as `\u0000` and it never
/// reaches an engine as a literal. Measured, not assumed — a NUL survives this
/// path against live Supermemory today, and a `U+FFFD` does not. Listing it
/// here would be dead weight implying a protection that JSON already provides.
const CHARACTERS_ENGINES_STRIP: [char; 1] = ['\u{FFFD}'];

/// Encodes a typed record for the provider's `content` field.
///
/// The escaping pass exists because the envelope has to survive engines that
/// sanitise content. `\ufffd` and a literal `U+FFFD` are the same string to
/// every JSON reader, so this changes nothing a decoder sees — including for
/// records already written the other way, which keep decoding unchanged.
///
/// Rewriting the serialized text is safe here in a way it would not be in
/// general: JSON's structural characters are all ASCII, so a character from
/// [`CHARACTERS_ENGINES_STRIP`] can only ever occur inside a string literal,
/// and `serde_json` has already escaped any backslash around it. Substituting
/// its `\uXXXX` form therefore yields an equivalent document and cannot
/// introduce or terminate an escape sequence.
fn encode<T: Serialize>(record: &T) -> Result<String> {
    let json = serde_json::to_string(&Envelope {
        v: ENVELOPE_VERSION,
        record,
    })
    .map_err(|error| OpenCompanyError::Store(format!("could not encode memory record: {error}")))?;
    Ok(CHARACTERS_ENGINES_STRIP
        .iter()
        .fold(json, |text, character| {
            if text.contains(*character) {
                text.replace(*character, &format!("\\u{:04x}", *character as u32))
            } else {
                text
            }
        }))
}

/// Decodes one entry, or `None` when it is not ours to read.
///
/// Returns `None` — rather than an error — for an entry outside `namespace` or
/// written by a version we do not understand. A single unreadable row must not
/// fail a whole `list`: on a shared hosted engine the store may legitimately
/// hold rows this build did not write.
///
/// A row *inside* our namespace that fails to parse is different: nothing else
/// writes there, so it is a record this host stored and can no longer read — a
/// corrupted write, not foreign data. It is still skipped (one bad row must not
/// fail the list), but loudly: #1201 was exactly this shape — the embedded
/// driver's PII scrubber redacted digits out of the JSON envelope, and the
/// silent `None` here made a corrupted record indistinguishable from one that
/// was never written.
fn decode<T: DeserializeOwned>(entry: &MemoryEntry, namespace: &Namespace) -> Option<T> {
    let reported = entry.namespace.as_deref().unwrap_or_default();
    if !namespace.contains(reported) {
        tracing::warn!(
            expected = namespace.as_str(),
            reported,
            "memory driver returned an entry outside the requested namespace; dropping it"
        );
        return None;
    }
    // Two-stage parse, version before record: a row written by an envelope
    // version this build does not know may carry a record shape `T` cannot
    // deserialize, and collapsing both steps into one `Envelope<T>` parse
    // would misreport that legitimate skip as corruption. Only a row whose
    // envelope is unreadable, or whose version matches and record still does
    // not parse, is a record we wrote and can no longer read.
    let corrupt = |error: &dyn std::fmt::Display| {
        tracing::warn!(
            namespace = namespace.as_str(),
            key = %entry.key,
            %error,
            "memory entry in our namespace failed to decode; dropping it \
             (a record we wrote and can no longer read — see #1201)"
        );
    };
    let envelope: Envelope<serde_json::Value> = match serde_json::from_str(&entry.content) {
        Ok(envelope) => envelope,
        Err(error) => {
            corrupt(&error);
            return None;
        }
    };
    if envelope.v != ENVELOPE_VERSION {
        tracing::debug!(
            namespace = namespace.as_str(),
            key = %entry.key,
            version = envelope.v,
            "memory entry has an envelope version this build does not understand; skipping it"
        );
        return None;
    }
    match serde_json::from_value(envelope.record) {
        Ok(record) => Some(record),
        Err(error) => {
            corrupt(&error);
            None
        }
    }
}

/// Maps a provider error onto the crate error type.
pub(super) fn store_error(error: MemoryError) -> OpenCompanyError {
    match error {
        MemoryError::NotFound(what) => OpenCompanyError::NotFound(what),
        MemoryError::Invalid(why) => OpenCompanyError::InvalidRequest(why),
        // Not `Unimplemented`: that variant means *this build* has no code for a
        // port. This means the operator bound an engine that cannot do what was
        // asked, which is a deployment fact they can act on, so the driver's own
        // words are worth keeping.
        MemoryError::Unsupported { capability } => OpenCompanyError::Store(format!(
            "the bound memory engine does not support the `{capability}` capability"
        )),
        other => OpenCompanyError::Store(other.to_string()),
    }
}

/// Category tags. Namespaces already partition these records; the category is a
/// second, driver-visible axis so an engine's own tooling shows something
/// meaningful, and it is `Custom` so it can never be confused with an engine's
/// native semantics for `Core` / `Daily` / `Conversation`.
fn category(tag: &str) -> MemoryCategory {
    MemoryCategory::Custom(format!("oc:{tag}"))
}

/// Shared plumbing: a provider, and which partition of a company's memory this
/// facade addresses.
///
/// # Why the company is a per-call argument, not a field
///
/// One `MemoryOverlay` is opened per *process* and injected into every
/// company's runtime, so a facade instance is shared by every tenant this host
/// serves. A namespace fixed at construction would therefore be one company's
/// namespace serving all of them — a cross-tenant leak, and exactly the defect
/// this module exists to prevent.
///
/// So the namespace is derived on every call from the `&CompanyId` the port
/// method was given. That is strictly stronger than deriving it once: the
/// namespace is a pure function of the argument the port contract already
/// requires, so it cannot be stale, cannot be mismatched with the caller's
/// intent, and cannot be set to a company the caller was not holding.
#[derive(Clone)]
pub(super) struct Bound {
    provider: std::sync::Arc<dyn MemoryProvider>,
    scope: Scope,
    taint: MemoryTaint,
}

impl Bound {
    pub(super) fn new(
        provider: std::sync::Arc<dyn MemoryProvider>,
        scope: Scope,
        taint: MemoryTaint,
    ) -> Self {
        Self {
            provider,
            scope,
            taint,
        }
    }

    /// The namespace this facade addresses for `company`.
    fn namespace(&self, company: &CompanyId) -> Namespace {
        Namespace::company_root(company).child(&self.scope)
    }

    /// Stores one typed record.
    ///
    /// Taint is passed on every call because [`MemoryCore::store`] requires it —
    /// there is no defaulted, taint-dropping overload on the provider contract
    /// to fall into. (The engine-side `Memory::store_with_taint` *does* have one,
    /// which is precisely why nothing here wraps a bare `Memory`.)
    async fn put<T: Serialize + Sync>(
        &self,
        company: &CompanyId,
        key: &str,
        record: &T,
        tag: &str,
    ) -> Result<()> {
        self.provider
            .store(
                self.namespace(company).as_str(),
                key,
                &encode(record)?,
                category(tag),
                None,
                self.taint,
            )
            .await
            .map_err(store_error)
    }

    /// Fetches one typed record by key.
    async fn get<T: DeserializeOwned>(&self, company: &CompanyId, key: &str) -> Result<Option<T>> {
        let namespace = self.namespace(company);
        Ok(self
            .provider
            .get(namespace.as_str(), key)
            .await
            .map_err(store_error)?
            .and_then(|entry| decode(&entry, &namespace)))
    }

    /// Whether the engine holds a record at `key` at all, **without decoding
    /// it**.
    ///
    /// [`Self::get`] answers `None` for two different facts: the engine has no
    /// such record, and the engine has one this build cannot read (a foreign
    /// envelope version, or the corrupted-write shape #1201 was). Callers that
    /// only read may treat those alike. A caller that reports "there was
    /// nothing there" to a user must not — that turns an unreadable record
    /// into a silent no-op and leaves it serving recall forever.
    async fn exists(&self, company: &CompanyId, key: &str) -> Result<bool> {
        let namespace = self.namespace(company);
        Ok(self
            .provider
            .get(namespace.as_str(), key)
            .await
            .map_err(store_error)?
            .is_some())
    }

    /// Lists every typed record in this company's partition.
    async fn list<T: DeserializeOwned>(&self, company: &CompanyId) -> Result<Vec<T>> {
        let namespace = self.namespace(company);
        Ok(self
            .provider
            .list(Some(namespace.as_str()), None, None)
            .await
            .map_err(store_error)?
            .iter()
            .filter_map(|entry| decode(entry, &namespace))
            .collect())
    }

    /// Deletes one record, reporting whether it existed.
    async fn forget(&self, company: &CompanyId, key: &str) -> Result<bool> {
        self.provider
            .forget(self.namespace(company).as_str(), key)
            .await
            .map_err(store_error)
    }

    /// Ranked recall, narrowed to this partition on the way in and re-checked on
    /// the way out.
    async fn recall(
        &self,
        company: &CompanyId,
        query: &str,
        limit: usize,
    ) -> Result<(Namespace, Vec<MemoryEntry>)> {
        let namespace = self.namespace(company);
        let opts = tinymemory_api::recall::OwnedRecallOpts {
            namespace: Some(namespace.as_str().to_string()),
            ..Default::default()
        };
        let hits = self
            .provider
            .recall(query, limit, &opts, None)
            .await
            .map_err(store_error)?
            .into_iter()
            .filter(|entry| namespace.contains(entry.namespace.as_deref().unwrap_or_default()))
            .collect();
        Ok((namespace, hits))
    }
}

// ---------------------------------------------------------------------------
// FactStore
// ---------------------------------------------------------------------------

/// The operator's hand-curated facts, over `MemoryCore`.
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

/// The RLM environment, over `MemoryCore` + `MemoryRecall`.
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
        let (namespace, entries) = self.bound.recall(company, query, limit).await?;
        Ok(entries
            .iter()
            .filter_map(|entry| {
                let chunk: StoredChunk = decode(entry, &namespace)?;
                Some(ChunkHit {
                    addr: ChunkAddr::new(content_address(&chunk.body)),
                    snippet: snippet(&chunk.body),
                    // The port promises `[0, 1]`. A driver that reports no score
                    // (the mandatory-only composition does not) still produced a
                    // hit, so it ranks above nothing rather than being dropped.
                    score: entry.score.unwrap_or(1.0).clamp(0.0, 1.0),
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

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// The brain's compressed traces and task results.
///
/// The spec sequences this port last, and says it may reasonably never move:
/// append-only, eviction-driven trace rows are the shape the contract suits
/// least. It is here because leaving one port on a different backend would mean
/// the export bundle spans two engines.
///
/// The gap this closes is `evict`. The contract has no archive tier and no bulk
/// delete by predicate, so eviction is a **move** between two namespaces — see
/// [`ProviderMemoryStore::evict`].
pub struct ProviderMemoryStore {
    traces: Bound,
    archive: Bound,
    task_results: Bound,
}

impl ProviderMemoryStore {
    pub(super) fn new(traces: Bound, archive: Bound, task_results: Bound) -> Self {
        Self {
            traces,
            archive,
            task_results,
        }
    }

    /// Reads the live trace set, oldest first.
    async fn ordered_traces(&self, company: &CompanyId) -> Result<Vec<CompressedTrace>> {
        let mut traces: Vec<CompressedTrace> = self.traces.list(company).await?;
        // Total order, not just by timestamp: two traces stamped in the same
        // millisecond must not reorder between reads, or `recent_traces` returns
        // a different window each call and eviction evicts a different set.
        traces.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.cycle_id.cmp(&b.cycle_id))
        });
        Ok(traces)
    }

    /// Reads the archived trace set, for the operator's inspection.
    ///
    /// The archive is a bounded recovery tier on this facade: eviction moves
    /// traces here rather than destroying them. The export path carries this
    /// tier separately from the live `GET /memory/traces` window — both read
    /// distinct namespaces. This accessor exists so the operator tier and the
    /// "archives rather than destroys" property tests can observe the tier itself.
    pub(super) async fn archived_traces(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<CompressedTrace>> {
        self.archive.list(company).await
    }

    /// Restores traces directly into the archive tier.
    pub(super) async fn restore_archived_traces(
        &self,
        company: &CompanyId,
        traces: &[CompressedTrace],
    ) -> Result<()> {
        for trace in traces {
            self.archive
                .put(company, &trace.cycle_id, trace, "trace")
                .await?;
        }
        Ok(())
    }

    /// Bounds the archive tier to the newest `n` archived traces.
    ///
    /// Eviction moves traces OUT of the live window rather than destroying
    /// them; without a matching cap here the archive would retain every trace a
    /// company ever evicted, so the documented retention policy would bound the
    /// inspectable window but not storage. Keeping the newest `n` evicted
    /// traces bounds the tier at `n` and total trace storage at `2n` — the live
    /// window plus the eviction history nearest to it.
    async fn prune_archive(&self, id: &CompanyId, n: usize) -> Result<()> {
        if n == 0 {
            let archived = self.archive.list::<CompressedTrace>(id).await?;
            for trace in archived {
                self.archive.forget(id, &trace.cycle_id).await?;
            }
            return Ok(());
        }
        let mut archived = self.archive.list::<CompressedTrace>(id).await?;
        if archived.len() <= n {
            return Ok(());
        }
        // Same total order as the live set, so "newest" is unambiguous even
        // when two traces share a millisecond.
        archived.sort_by(|a, b| {
            a.at_millis
                .cmp(&b.at_millis)
                .then_with(|| a.cycle_id.cmp(&b.cycle_id))
        });
        let prune = archived.len() - n;
        for trace in archived.into_iter().take(prune) {
            self.archive.forget(id, &trace.cycle_id).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for ProviderMemoryStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        self.traces.put(id, &trace.cycle_id, &trace, "trace").await
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        // Avoid even touching the provider when the caller requests no rows.
        // This matters for the provider-backed facade because `list` has no
        // limit argument and otherwise decodes the entire trace partition.
        if limit == 0 {
            return Ok(Vec::new());
        }
        let traces = self.ordered_traces(id).await?;
        // Newest last, per the port contract, so the tail is the window.
        let skip = traces.len().saturating_sub(limit);
        Ok(traces.into_iter().skip(skip).collect())
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        self.task_results
            .put(id, &result.task_id, &result, "task-result")
            .await
    }

    /// Evicts per `policy`, **archiving rather than destroying**.
    ///
    /// `docs/spec/company-brain/memory.md` makes this normative: "evicted traces
    /// are archived, not deleted, until retention policy or the Operator says
    /// otherwise". The contract offers no archive tier, so the behaviour lives
    /// here as a move between two namespaces.
    ///
    /// Order matters and is not arbitrary: the archive write happens **before**
    /// the live delete. There is no transaction spanning two provider calls, so
    /// one of the two orders has to be chosen for what it does when the process
    /// dies in between. Archive-then-delete leaves a trace in both places — a
    /// duplicate the next read reconciles. Delete-then-archive loses it. For a
    /// port whose whole promise is "not destroyed", that asymmetry decides it.
    ///
    /// The returned count is **traces this call removed from the live set**, not
    /// traces archived. Those differ when `forget` reports a key was already
    /// gone: the archive write has happened by then, so the archive can hold an
    /// entry this call did not remove. That is a concurrent eviction having got
    /// there first, and the entry is archived either way — which is the
    /// behaviour the port promises. Reporting it as removed *here* would be the
    /// lie, so the count stays narrow.
    ///
    /// The same asymmetry appears if a `put` or `forget` fails mid-loop: the
    /// error propagates and the traces already processed stay archived. That is
    /// the archive-then-delete order behaving as designed under partial failure
    /// — a duplicate the next read reconciles, never a loss. What must NOT be
    /// skipped on that path is the archive bound itself: traces already moved
    /// by the failed pass are still in the archive, so the prune below runs
    /// before the error propagates, keeping the tier at its limit even when a
    /// maintenance pass repeatedly fails partway.
    ///
    /// Every eviction additionally bounds the archive itself to the newest
    /// `n` evicted traces (see [`ProviderMemoryStore::prune_archive`]): a
    /// `KeepRecent { n }` eviction bounds it to `n`, and `OlderThan` — which
    /// has no `n` of its own — to the retention limit, so the policy that
    /// bounds the live window also bounds storage on every path: a company
    /// that runs for years does not accumulate every trace it ever evicted
    /// beside the 32 it keeps, and an operator-sized `OlderThan` sweep cannot
    /// grow the archive without bound. That bound is what keeps
    /// `GET /memory/archives` a bounded read by construction rather than a
    /// download of the whole archive followed by a discard.
    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let traces = self.ordered_traces(id).await?;
        let doomed: Vec<CompressedTrace> = match &policy {
            EvictionPolicy::KeepRecent { n } => {
                let keep_from = traces.len().saturating_sub(*n);
                traces.into_iter().take(keep_from).collect()
            }
            EvictionPolicy::OlderThan { before_millis } => traces
                .into_iter()
                .filter(|trace| trace.at_millis < *before_millis)
                .collect(),
        };
        let mut evicted = 0u64;
        let move_result = (async {
            for trace in doomed {
                self.archive
                    .put(id, &trace.cycle_id, &trace, "trace")
                    .await?;
                if self.traces.forget(id, &trace.cycle_id).await? {
                    evicted += 1;
                }
            }
            Ok::<(), OpenCompanyError>(())
        })
        .await;
        // Bound the archive on every eviction path — the partial-failure path
        // included. `KeepRecent` prunes to its own `n`; `OlderThan` has no `n`
        // to bound by, so it prunes to the retention limit — the same window
        // the live set is held to, which is what keeps the tier "the eviction
        // history nearest to the live window" and the archive read bounded for
        // any policy.
        let bound = match policy {
            EvictionPolicy::KeepRecent { n } => n,
            EvictionPolicy::OlderThan { .. } => TRACE_RETENTION_LIMIT,
        };
        if let Err(move_err) = move_result {
            // A provider failure mid-loop still leaves the traces already
            // moved sitting in the archive, and a maintenance pass that keeps
            // failing partway must not grow the tier past its bound across
            // retries. Prune best-effort, then report the failure that
            // actually happened.
            if let Err(prune_err) = self.prune_archive(id, bound).await {
                tracing::warn!(
                    error = %prune_err,
                    "archive prune failed after a partial eviction failure; the archive may exceed \
                     its retention bound"
                );
            }
            return Err(move_err);
        }
        self.prune_archive(id, bound).await?;
        Ok(evicted)
    }
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, Mutex};

    use super::super::namespace::Scope;
    use super::*;

    /// Captures warnings emitted synchronously on this test's thread.
    ///
    /// Every exercise of a warn callsite this helper later asserts on must
    /// itself run through `warnings_from` — never bare. A `tracing::warn!`
    /// fired on a thread with no subscriber registers its callsite against the
    /// global NoSubscriber, whose `register_callsite` is `Interest::never()`,
    /// and tracing caches that answer process-wide: the warn silently stops
    /// firing everywhere, including here. The CI flake this helper was built
    /// to make deterministic (`unreadable_content` racing
    /// `decode_classifies`) was exactly that. See the load-bearing comments on
    /// those two tests.
    fn warnings_from(body: impl FnOnce()) -> String {
        use std::io::Write;

        #[derive(Clone)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        struct Writer(Arc<Mutex<Vec<u8>>>);

        impl Write for Writer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("warning sink").extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Writer;

            fn make_writer(&'a self) -> Self::Writer {
                Writer(Arc::clone(&self.0))
            }
        }

        let sink = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Sink(Arc::clone(&sink)))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            // `tracing` caches each callsite's interest globally. A different
            // test can first register this warning callsite under the no-op
            // subscriber, caching `Interest::never` before this scoped
            // subscriber is installed. Rebuild while this thread's subscriber
            // is active so the warning assertion remains order-independent.
            tracing::callsite::rebuild_interest_cache();
            body();
        });
        let bytes = sink.lock().expect("warning sink").clone();
        String::from_utf8(bytes).expect("warnings are utf-8")
    }

    /// Derives a real namespace the same way production does. These tests never
    /// build one from a raw string, because production code cannot either —
    /// a helper that could would be testing a constructor that does not exist.
    fn ns(company: &str, scope: Scope) -> Namespace {
        Namespace::company_root(&CompanyId::new(company)).child(&scope)
    }

    fn entry_in(namespace: &Namespace, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: "id".into(),
            key: "key".into(),
            content: content.to_string(),
            namespace: Some(namespace.as_str().to_string()),
            category: category("test"),
            timestamp: "1970-01-01T00:00:00Z".into(),
            session_id: None,
            score: None,
            taint: MemoryTaint::Internal,
        }
    }

    fn a_fact() -> FactRecord {
        FactRecord {
            id: "f1".into(),
            kind: FactKind::Preference,
            title: "Ships on Fridays".into(),
            body: "The team releases at the end of the week.".into(),
            source: "cto".into(),
            updated_at_millis: 1_700_000_000_000,
        }
    }

    #[test]
    fn envelope_round_trips_a_fact() {
        let facts = ns("acme", Scope::Facts);
        let fact = a_fact();
        let entry = entry_in(&facts, &encode(&fact).unwrap());
        assert_eq!(decode::<FactRecord>(&entry, &facts).unwrap(), fact);
    }

    #[test]
    fn envelope_round_trips_a_trace() {
        let traces = ns("acme", Scope::Traces);
        let trace = CompressedTrace {
            cycle_id: "c1".into(),
            summary: "shipped the thing".into(),
            at_millis: 42,
        };
        let entry = entry_in(&traces, &encode(&trace).unwrap());
        assert_eq!(decode::<CompressedTrace>(&entry, &traces).unwrap(), trace);
    }

    #[test]
    fn envelope_round_trips_a_chunk_including_its_stamp() {
        let context = ns("acme", Scope::Context);
        let chunk = StoredChunk {
            label: "notes/one".into(),
            body: "the quick brown fox".into(),
            stored_at_millis: 99,
            labels: vec!["notes/one".into()],
        };
        let entry = entry_in(&context, &encode(&chunk).unwrap());
        let decoded: StoredChunk = decode(&entry, &context).unwrap();
        assert_eq!(decoded.label, chunk.label);
        assert_eq!(decoded.body, chunk.body);
        assert_eq!(decoded.stored_at_millis, chunk.stored_at_millis);
        assert_eq!(decoded.labels, chunk.labels);
    }

    #[test]
    fn the_encoding_leaves_no_character_a_hosted_engine_would_strip() {
        // The escape is only worth anything if it removes every literal from
        // the serialized text; an engine sanitises the bytes it receives, not
        // the record they represent.
        let chunk = StoredChunk {
            label: "notes/one".into(),
            body: "before\u{FFFD}after".into(),
            stored_at_millis: 1,
            labels: vec!["notes/one".into()],
        };
        let json = encode(&chunk).unwrap();
        for character in CHARACTERS_ENGINES_STRIP {
            assert!(
                !json.contains(character),
                "the encoded envelope still carries U+{:04X} as a literal: {json:?}",
                character as u32
            );
        }
        assert!(
            json.contains("\\ufffd"),
            "the character must be escaped rather than dropped: {json:?}"
        );
    }

    #[test]
    fn escaping_preserves_the_record_including_around_a_backslash() {
        // The escape rewrites serialized JSON rather than the record, which is
        // safe only because a stripped character can appear solely inside a
        // string literal and `serde_json` has already escaped any backslash
        // beside it. A body that puts the two together is where a naive
        // substitution would corrupt the document, so pin it: this must decode
        // back to exactly what went in.
        let context = ns("acme", Scope::Context);
        let chunk = StoredChunk {
            label: "notes/one".into(),
            body: "a\\\u{FFFD}b\u{FFFD}\\u{FFFD}c\u{0}d".into(),
            stored_at_millis: 7,
            labels: vec!["notes/one".into()],
        };
        let entry = entry_in(&context, &encode(&chunk).unwrap());
        let decoded: StoredChunk = decode(&entry, &context).unwrap();
        assert_eq!(
            decoded.body, chunk.body,
            "every character must survive the escape and the decode"
        );
    }

    #[test]
    fn another_companys_entry_is_dropped_not_decoded() {
        // The cross-tenant guard: a driver answering with somebody else's row
        // must not reach a caller holding this company's id.
        let mine = ns("acme", Scope::Facts);
        let theirs = ns("globex", Scope::Facts);
        let entry = entry_in(&theirs, &encode(&a_fact()).unwrap());
        assert!(decode::<FactRecord>(&entry, &mine).is_none());
    }

    #[test]
    fn a_sibling_scope_of_the_same_company_is_dropped() {
        // Scope separation is not decoration: scratch must not decode as
        // context, or the firewall is a routing convention rather than a rule.
        let context = ns("acme", Scope::Context);
        let scratch = ns("acme", Scope::Scratch);
        let chunk = StoredChunk {
            label: "l".into(),
            body: "b".into(),
            stored_at_millis: 1,
            labels: vec!["l".into()],
        };
        let entry = entry_in(&scratch, &encode(&chunk).unwrap());
        assert!(decode::<StoredChunk>(&entry, &context).is_none());
    }

    #[test]
    fn an_entry_with_no_namespace_is_dropped() {
        let facts = ns("acme", Scope::Facts);
        let mut entry = entry_in(&facts, &encode(&a_fact()).unwrap());
        entry.namespace = None;
        assert!(decode::<FactRecord>(&entry, &facts).is_none());
    }

    #[test]
    fn an_unknown_envelope_version_is_dropped() {
        let traces = ns("acme", Scope::Traces);
        let raw = serde_json::json!({ "v": 99, "record": { "cycle_id": "c1" } }).to_string();
        let entry = entry_in(&traces, &raw);
        assert!(decode::<CompressedTrace>(&entry, &traces).is_none());
    }

    #[test]
    fn unreadable_content_is_dropped_rather_than_failing_the_read() {
        let facts = ns("acme", Scope::Facts);
        let entry = entry_in(&facts, "not json at all");
        // The `warnings_from` wrapper is LOAD-BEARING, not decoration: a corrupt
        // decode must always run under a subscriber, or tracing's global
        // callsite-interest cache can freeze this warn callsite to `never` for
        // the whole process. A bare test thread has no subscriber, so its
        // `get_default` is the global NoSubscriber whose `register_callsite`
        // answers `Interest::never()` — and once cached, every later capture of
        // this same callsite (in `warnings_from`) is silently dropped. The race
        // needs the thread-local sink to have already raised the global max
        // level to WARN, which `decode_classifies`'s own sink does concurrently,
        // so `unreadable_content` — run bare — was exactly the poisoner that
        // made the corruption warning intermittently vanish in CI.
        let warnings = warnings_from(|| {
            assert!(decode::<FactRecord>(&entry, &facts).is_none());
        });
        assert!(
            warnings.contains("memory entry in our namespace failed to decode"),
            "unreadable content in our namespace must be reported: {warnings:?}"
        );
    }

    // The range-widening behavior `peek` relies on is pinned where the helper
    // now lives: `crate::store::text` (shared with the fs/sqlite/mongo
    // backends' peek and search-snippet slices).

    #[test]
    fn a_snippet_never_splits_a_character() {
        let body = "é".repeat(300);
        let cut = snippet(&body);
        assert!(body.starts_with(&cut));
        assert!(cut.len() >= 200);
    }

    /// The decode classification #1248 review asked to pin: an entry from an
    /// envelope version this build does not know is a legitimate skip even
    /// when its record shape does not fit today's `T` — it must not be read
    /// as corruption — while a matching version with an unreadable record,
    /// or no envelope at all, is the #1201 corruption path. All three return
    /// `None` rather than failing the list.
    #[test]
    fn decode_classifies_unknown_versions_and_corruption_separately() {
        let namespace = Namespace::company_root(&CompanyId::new("acme"));
        let entry = |content: &str| MemoryEntry {
            id: "id".into(),
            key: "key".into(),
            content: content.into(),
            namespace: Some(namespace.as_str().to_string()),
            category: tinymemory_api::types::MemoryCategory::Custom("oc:trace".into()),
            timestamp: String::new(),
            session_id: None,
            score: None,
            taint: MemoryTaint::Internal,
        };

        // Round-trip control: a current-version envelope decodes.
        let good = encode(&42u32).unwrap();
        assert_eq!(decode::<u32>(&entry(&good), &namespace), Some(42));

        // Unknown version, record shape incompatible with `T`: skipped, and
        // reachable only through the version gate — the record is never
        // parsed as `T`, so this cannot trip the corruption path.
        let future = r#"{"v":9,"record":{"shape":"unknown"}}"#;
        assert_eq!(decode::<u32>(&entry(future), &namespace), None);

        // Matching version, unreadable record: the corruption path. The valid
        // envelope ensures this reaches record deserialization after the
        // version check instead of duplicating the malformed-JSON case below.
        #[derive(Deserialize)]
        struct Timestamp {
            #[allow(dead_code)]
            at_millis: u64,
        }
        let mangled = r#"{"v":1,"record":{"at_millis":"not-a-number"}}"#;
        let warnings = warnings_from(|| {
            assert!(decode::<Timestamp>(&entry(mangled), &namespace).is_none());
        });
        assert!(
            warnings.contains("memory entry in our namespace failed to decode"),
            "matching-version record corruption must be reported: {warnings:?}"
        );

        // No envelope at all: also the corruption path.
        let warnings = warnings_from(|| {
            assert_eq!(decode::<u32>(&entry("not json"), &namespace), None);
        });
        assert!(
            warnings.contains("memory entry in our namespace failed to decode"),
            "envelope-less content in our namespace must be reported: {warnings:?}"
        );
    }
}
