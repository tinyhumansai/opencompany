//! The engine plumbing every facade shares: the JSON envelope, the
//! namespace re-check, and [`Bound`] — one scope of a company's memory over
//! one engine. See the parent module for how a keyed record maps onto an item.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tinymemory::{
    DocumentBody, Error as EngineError, FetchMode, FetchRequest, ForgetTarget, Hit, ItemId,
    ItemKind, ListRequest, MemoryEngine, MemoryMeta, MetaFilter, SourceKind, SourceRef, StoreItem,
};

use super::super::namespace::{Namespace, Scope};
use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::CompanyId;

/// Envelope version. Bumped only if the on-the-wire shape of a record changes
/// incompatibly; a decoder that meets a version it does not know refuses rather
/// than guessing, because a half-understood memory record is worse than a
/// missing one.
const ENVELOPE_VERSION: u8 = 1;

/// The tag every inbound-channel write carries, beside `SourceKind::Link`.
///
/// The v1 contract had a `MemoryTaint::ExternalSync` the engine stored; v2 has
/// no taint, so external provenance is metadata this host stamps. A tag (not
/// only the source kind) so an operator filtering an engine's own console by
/// tag sees it, and so a future reader can refuse it by `tags_any`.
pub const EXTERNAL_TAG: &str = "oc:provenance:external";

/// Page size for walking a partition. Large enough that a typical partition is
/// one round trip, small enough that one page is a bounded response.
const LIST_PAGE: usize = 200;

/// Hard ceiling on pages walked in one listing, so an engine that keeps handing
/// back a cursor cannot turn one port call into an unbounded walk.
const MAX_LIST_PAGES: usize = 500;

/// The MIME type stamped on every envelope, so an engine's own tooling (and an
/// operator reading it) can tell these are structured records, not prose.
pub(in crate::store::memory) const ENVELOPE_MIME: &str = "application/vnd.opencompany.memory+json";

/// The wire form of a typed port record inside an item's text.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope<T> {
    /// Format version — see [`ENVELOPE_VERSION`].
    v: u8,
    /// The port's own record, verbatim.
    record: T,
}

/// Characters a hosted engine removes from text, escaped on the way out.
///
/// Hosted engines have been measured stripping `U+FFFD` server-side
/// (tinymemory#80). An engine is within its rights to sanitise text it is
/// handed; what breaks is that this host does not hand it text, it hands it a
/// JSON envelope, and a character removed from the middle of that envelope
/// comes back as a record whose body is quietly one character shorter than it
/// was written.
///
/// `U+0000` is deliberately absent: RFC 8259 requires escaping `U+0000`
/// through `U+001F`, so `serde_json` already emits it as `\u0000`.
pub(super) const CHARACTERS_ENGINES_STRIP: [char; 1] = ['\u{FFFD}'];

/// Encodes a typed record for an item's text.
///
/// Rewriting the serialized text is safe: JSON's structural characters are all
/// ASCII, so a character from [`CHARACTERS_ENGINES_STRIP`] can only ever occur
/// inside a string literal, and substituting its `\uXXXX` form yields an
/// equivalent document.
pub(super) fn encode<T: Serialize>(record: &T) -> Result<String> {
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

/// Whether `hit` is inside `namespace` — the re-check every read applies.
fn owned_by(hit: &Hit, namespace: &Namespace) -> bool {
    let reported = hit.meta.workspace.as_deref().unwrap_or_default();
    if reported == namespace.as_str() {
        return true;
    }
    tracing::warn!(
        expected = namespace.as_str(),
        reported,
        "memory engine returned an item outside the requested namespace; dropping it"
    );
    false
}

/// Decodes one item, or `None` when it is not ours to read.
///
/// Returns `None` — rather than an error — for an item outside `namespace` or
/// written by a version we do not understand. A single unreadable row must not
/// fail a whole `list`: on a shared hosted engine the store may legitimately
/// hold rows this build did not write.
///
/// A row *inside* our namespace that fails to parse is different: nothing else
/// writes there, so it is a record this host stored and can no longer read — a
/// corrupted write, not foreign data. It is still skipped, but loudly (#1201).
pub(super) fn decode<T: DeserializeOwned>(hit: &Hit, namespace: &Namespace) -> Option<T> {
    if !owned_by(hit, namespace) {
        return None;
    }
    let key = hit.meta.source.id.as_deref().unwrap_or_default();
    let corrupt = |error: &dyn std::fmt::Display| {
        tracing::warn!(
            namespace = namespace.as_str(),
            key,
            %error,
            "memory item in our namespace failed to decode; dropping it \
             (a record we wrote and can no longer read — see #1201)"
        );
    };
    let envelope: Envelope<serde_json::Value> = match serde_json::from_str(&hit.text) {
        Ok(envelope) => envelope,
        Err(error) => {
            corrupt(&error);
            return None;
        }
    };
    if envelope.v != ENVELOPE_VERSION {
        tracing::debug!(
            namespace = namespace.as_str(),
            key,
            version = envelope.v,
            "memory item has an envelope version this build does not understand; skipping it"
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

/// Maps an engine error onto the crate error type.
pub(in crate::store::memory) fn store_error(error: EngineError) -> OpenCompanyError {
    match error {
        EngineError::NotFound(what) => OpenCompanyError::NotFound(what),
        EngineError::InvalidRequest(why) => OpenCompanyError::InvalidRequest(why),
        // Not `Unimplemented`: that variant means *this build* has no code for a
        // port. This means the operator bound an engine that cannot do what was
        // asked, which is a deployment fact they can act on, so the engine's own
        // words are worth keeping.
        EngineError::Unsupported(what) => OpenCompanyError::Store(format!(
            "the bound memory engine does not support this: {what}"
        )),
        other => OpenCompanyError::Store(other.to_string()),
    }
}

/// Where a facade's writes come from — the v2 stand-in for the v1 taint enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::store::memory) enum Provenance {
    /// The company writing about itself: operator facts, agent working-out.
    Internal,
    /// Content that arrived from an inbound channel or the web.
    External,
}

impl Provenance {
    /// The source kind stamped on every item this facade writes.
    fn source_kind(self) -> SourceKind {
        match self {
            Self::Internal => SourceKind::Agent,
            Self::External => SourceKind::Link,
        }
    }
}

/// Shared plumbing: an engine, and which partition of a company's memory this
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
/// method was given: it cannot be stale, cannot be mismatched with the caller's
/// intent, and cannot be set to a company the caller was not holding.
#[derive(Clone)]
pub(in crate::store::memory) struct Bound {
    engine: Arc<dyn MemoryEngine>,
    scope: Scope,
    provenance: Provenance,
}

impl Bound {
    pub(in crate::store::memory) fn new(
        engine: Arc<dyn MemoryEngine>,
        scope: Scope,
        provenance: Provenance,
    ) -> Self {
        Self {
            engine,
            scope,
            provenance,
        }
    }

    /// The namespace this facade addresses for `company`.
    fn namespace(&self, company: &CompanyId) -> Namespace {
        Namespace::company_root(company).child(&self.scope)
    }

    /// The filter every read of `namespace` carries, optionally narrowed to
    /// one key.
    fn filter(namespace: &Namespace, key: Option<&str>) -> MetaFilter {
        MetaFilter {
            workspace: Some(namespace.as_str().to_string()),
            source_id: key.map(str::to_string),
            kinds: vec![ItemKind::Document],
            ..MetaFilter::default()
        }
    }

    /// Every item in `namespace` (optionally one key), walked page by page and
    /// re-checked against the namespace.
    async fn items(&self, namespace: &Namespace, key: Option<&str>) -> Result<Vec<Hit>> {
        let mut request = ListRequest::new(Self::filter(namespace, key), LIST_PAGE);
        let mut out = Vec::new();
        for _ in 0..MAX_LIST_PAGES {
            let page = self
                .engine
                .list(request.clone())
                .await
                .map_err(store_error)?;
            out.extend(page.items.into_iter().filter(|hit| {
                owned_by(hit, namespace)
                    && key.is_none_or(|key| hit.meta.source.id.as_deref() == Some(key))
            }));
            match page.next_cursor {
                Some(cursor) => request.cursor = Some(cursor),
                None => return Ok(out),
            }
        }
        Err(OpenCompanyError::Store(format!(
            "memory engine kept paging past {MAX_LIST_PAGES} pages listing one partition; \
             refusing to walk further"
        )))
    }

    /// The live item for `key`: the newest when a crashed rewrite left two.
    async fn current(&self, namespace: &Namespace, key: &str) -> Result<Option<Hit>> {
        Ok(self
            .items(namespace, Some(key))
            .await?
            .into_iter()
            .max_by_key(|hit| hit.meta.observed_at))
    }

    /// Stores one typed record under `key`, replacing whatever was there.
    pub(super) async fn put<T: Serialize + Sync>(
        &self,
        company: &CompanyId,
        key: &str,
        record: &T,
        tag: &str,
    ) -> Result<()> {
        let namespace = self.namespace(company);
        let previous = self.items(&namespace, Some(key)).await?;
        let mut tags = vec![format!("oc:{tag}")];
        if self.provenance == Provenance::External {
            tags.push(EXTERNAL_TAG.to_string());
        }
        let meta = MemoryMeta {
            workspace: Some(namespace.as_str().to_string()),
            // The same namespace again, as a path: `workspace` matches exactly,
            // `folder` by `/`-aware prefix, which is what lets `migrate` select
            // every record this host wrote (`folder: "oc"`) and nothing else.
            folder: Some(namespace.as_str().to_string()),
            source: SourceRef {
                kind: self.provenance.source_kind(),
                id: Some(key.to_string()),
            },
            tags,
            // From the host clock the ports already use; the contract re-exports
            // `chrono` without its `clock` feature.
            observed_at: i64::try_from(crate::ports::now_millis())
                .ok()
                .and_then(tinymemory::chrono::DateTime::from_timestamp_millis),
            ..MemoryMeta::default()
        };
        let receipt = self
            .engine
            .store(StoreItem::Document {
                title: None,
                body: DocumentBody::Text(encode(record)?),
                mime: Some(ENVELOPE_MIME.to_string()),
                meta,
            })
            .await
            .map_err(store_error)?;
        // Store first, forget second: a crash between the two leaves a
        // duplicate that `current` reconciles, never a lost record.
        let stale: Vec<ItemId> = previous
            .into_iter()
            .map(|hit| hit.id)
            .filter(|id| *id != receipt.id)
            .collect();
        if !stale.is_empty() {
            self.engine
                .forget(ForgetTarget::Ids(stale))
                .await
                .map_err(store_error)?;
        }
        Ok(())
    }

    /// Fetches one typed record by key.
    pub(super) async fn get<T: DeserializeOwned>(
        &self,
        company: &CompanyId,
        key: &str,
    ) -> Result<Option<T>> {
        let namespace = self.namespace(company);
        Ok(self
            .current(&namespace, key)
            .await?
            .and_then(|hit| decode(&hit, &namespace)))
    }

    /// Whether the engine holds a record at `key` at all, **without decoding
    /// it**.
    ///
    /// [`Self::get`] answers `None` for two different facts: the engine has no
    /// such record, and the engine has one this build cannot read. A caller
    /// that reports "there was nothing there" to a user must not conflate them.
    pub(super) async fn exists(&self, company: &CompanyId, key: &str) -> Result<bool> {
        let namespace = self.namespace(company);
        Ok(self.current(&namespace, key).await?.is_some())
    }

    /// Lists every typed record in this company's partition, one per key.
    pub(super) async fn list<T: DeserializeOwned>(&self, company: &CompanyId) -> Result<Vec<T>> {
        let namespace = self.namespace(company);
        let mut newest: HashMap<String, Hit> = HashMap::new();
        for hit in self.items(&namespace, None).await? {
            let key = hit.meta.source.id.clone().unwrap_or_default();
            match newest.get(&key) {
                Some(held) if held.meta.observed_at >= hit.meta.observed_at => {}
                _ => {
                    newest.insert(key, hit);
                }
            }
        }
        Ok(newest
            .values()
            .filter_map(|hit| decode(hit, &namespace))
            .collect())
    }

    /// Deletes one record, reporting whether it existed.
    pub(super) async fn forget(&self, company: &CompanyId, key: &str) -> Result<bool> {
        let namespace = self.namespace(company);
        let report = self
            .engine
            .forget(ForgetTarget::Filter(Self::filter(&namespace, Some(key))))
            .await
            .map_err(store_error)?;
        Ok(report.forgotten > 0)
    }

    /// Ranked retrieval, narrowed to this partition on the way in and
    /// re-checked on the way out.
    ///
    /// `fetch`, not `recall`: recall synthesises an answer over the engine's
    /// whole scope (derived facts included) and only filters its citations,
    /// while fetch returns the stored items themselves, ranked.
    ///
    /// `Keyword` first when the engine offers it: `ContextStore::search` is a
    /// lexical contract on every other backend (fs, sqlite, mongodb), and a
    /// vector-ranked mode returns the nearest items whether or not they match.
    /// Engines that serve only `Hybrid` (CortexDB) get that.
    pub(super) async fn search(
        &self,
        company: &CompanyId,
        query: &str,
        limit: usize,
    ) -> Result<(Namespace, Vec<Hit>)> {
        let namespace = self.namespace(company);
        let descriptor = self.engine.descriptor();
        let Some(mode) = [FetchMode::Keyword, FetchMode::Hybrid, FetchMode::Vector]
            .into_iter()
            .find(|mode| descriptor.supports(*mode))
        else {
            return Ok((namespace, Vec::new()));
        };
        if query.trim().is_empty() || limit == 0 {
            return Ok((namespace, Vec::new()));
        }
        let mut request = FetchRequest::new(query, mode, limit);
        request.filter = Self::filter(&namespace, None);
        let hits = self
            .engine
            .fetch(request)
            .await
            .map_err(store_error)?
            .hits
            .into_iter()
            .filter(|hit| owned_by(hit, &namespace))
            .collect();
        Ok((namespace, hits))
    }
}
