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
//! The backend is chosen by `OPENCOMPANY_STORAGE` (fs / sqlite / mongodb),
//! not by openhuman — that is the "memory pluggable and
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
//! A hosted-provider [`ContextStore`] with richer semantics removes these
//! gaps; the adapter code is identical because it only speaks the port.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::memory::{Memory, MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, RecallOpts};

use crate::ports::ContextStore;
use crate::ports::types::{ChunkAddr, CompanyId, ContextChunk};

/// The one-time-secret URL path. Exact match: those URLs are lowercase.
const SECRET_URL_MARKER: &str = "/secret/";
/// The HTTP `Authorization` scheme, matched ASCII-case-insensitively (RFC
/// 9110's auth-scheme ABNF is case-insensitive, so `bearer sk-...` and
/// `BEARER sk-...` are credentials too).
const BEARER_MARKER: &str = "bearer ";

/// Byte offset of the next [`BEARER_MARKER`] occurrence, case-insensitively.
/// The marker is pure ASCII, so a byte scan is safe and allocation-free.
fn find_bearer_marker(text: &str) -> Option<usize> {
    text.as_bytes()
        .windows(BEARER_MARKER.len())
        .position(|w| w.eq_ignore_ascii_case(BEARER_MARKER.as_bytes()))
}

/// The earliest marker in `rest`, as `(byte offset, marker)`. The marker
/// string is only used for its byte length; the output always carries the
/// caller's original casing.
fn next_marker(rest: &str) -> Option<(usize, &'static str)> {
    rest.find(SECRET_URL_MARKER)
        .map(|p| (p, SECRET_URL_MARKER))
        .into_iter()
        .chain(find_bearer_marker(rest).map(|p| (p, BEARER_MARKER)))
        .min_by_key(|(p, _)| *p)
}

/// Redacts secrets from text on its way into memory.
///
/// Measured in a live deployment: chat messages of the form "here is the link
/// with the password" carried a full one-time-secret URL
/// (`.../secret/<key>`), and because every operator message is remembered
/// verbatim, `recall` could pull the still-unopened secret back into a later
/// turn's context — leaving it readable in the store.
///
/// Deliberately *replaces* rather than *rejects*: memory should still record
/// that a link was shared, since that is the context an agent needs to
/// understand what happened. Only the secret value itself is removed.
///
/// openhuman's [`redact_text`](oh::agent::experience::redact_text) does the same
/// for its own experience records; this is the opencompany-side of that rule.
/// `pub(crate)` because the outcome-chunk store path
/// ([`memory_loop::outcome_chunk`](crate::harness::built_in::memory_loop::outcome_chunk))
/// writes operator text directly to the [`ContextStore`], bypassing
/// [`Memory::store`], and must apply the same redaction.
pub(crate) fn redact_secrets(text: &str) -> std::borrow::Cow<'_, str> {
    // Two unambiguous shapes. A generic "anything that looks like a token"
    // regex would mangle ordinary prose.
    if !text.contains(SECRET_URL_MARKER) && find_bearer_marker(text).is_none() {
        return std::borrow::Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((pos, marker)) = next_marker(rest) {
        out.push_str(&rest[..pos + marker.len()]);
        // The HTTP scheme marker is matched case-insensitively, but its casing
        // changes how aggressive the redaction may be: the capital "Bearer "
        // form is the unambiguous auth-header spelling, so a plain digit-free
        // word after it like `secret` is still a credential; the lower-case
        // form is also ordinary English prose ("ring bearer", "standard bearer
        // candidate"), so a plain word there is only redacted once it is long
        // enough that prose is implausible.
        let aggressive = marker == SECRET_URL_MARKER || rest.as_bytes()[pos].is_ascii_uppercase();
        let tail = &rest[pos + marker.len()..];
        // The MCP config trims the value after the marker, so extra whitespace
        // (`Bearer   sk-...`) is legal. Keep it in the output but skip it here —
        // the value scan would otherwise stop at the first space and store the
        // credential verbatim.
        let value_start = tail.len() - tail.trim_start().len();
        out.push_str(&tail[..value_start]);
        let mut value = &tail[value_start..];
        // Formatted chat can wrap a credential in Markdown backticks or quotes
        // (`Bearer `sk-...``); skip one leading wrapper so the scan reaches the
        // credential instead of stopping at length zero. The wrapper is kept in
        // the output, and its closing mate stays in `rest`.
        let wrapper_len = usize::from(matches!(
            value.as_bytes().first(),
            Some(b'`' | b'\'' | b'"')
        ));
        out.push_str(&value[..wrapper_len]);
        value = &value[wrapper_len..];
        // The value runs up to the first character that cannot be part of a token.
        let end = value
            .find(|c: char| !is_token_char(c))
            .unwrap_or(value.len());
        // Long values are always a secret. A short value is still redacted when
        // it is token-shaped — it contains a digit, which prose after a marker
        // does not — so a valid short bearer credential like `s3cret` (the MCP
        // config accepts any non-empty bearer value) does not slip through. A
        // digit-free short value is redacted once it reaches six characters
        // when the marker is the unambiguous capital form, or when the value
        // is not a plain word (`sk-longsecret`, a JWT): "or" and "token" stay
        // shorter, while a genuine if weak digit-free credential like `secret`
        // does not. A plain word after a lower-case "bearer " is ordinary
        // prose too, so it is only redacted past twelve characters.
        let has_digit = value[..end].chars().any(|c| c.is_ascii_digit());
        let plain_word = !value[..end].chars().any(|c| !c.is_ascii_alphanumeric());
        let threshold = if has_digit {
            4
        } else if aggressive || !plain_word {
            6
        } else {
            12
        };
        let mut consumed = end;
        if end >= threshold {
            out.push_str("[REDACTED]");
            // The MCP config keeps the whole trimmed remainder as the bearer
            // value, so a credential can be several space-separated fragments
            // (`Bearer firstpart secondpart`). Once the first fragment looked
            // credential-like, keep redacting the run — but only fragments
            // that themselves clear a floor, so trailing prose ("please", "the
            // token was rotated") survives. The digit-free floor is higher for
            // continuation so a short prose word cannot enter the run. Only a
            // space stop triggers this: a wrapper or punctuation stop means the
            // credential was a single token.
            if value.as_bytes().get(end) == Some(&b' ') {
                let mut cursor = end;
                while let Some(rel) = value[cursor..].find(' ') {
                    // Probe the fragment after this run of spaces without
                    // consuming the space yet: if it clears the floor it is
                    // part of the credential run, otherwise it is prose and
                    // the space must survive into `rest`.
                    let token_start = cursor + rel + 1;
                    let frag_end = value[token_start..]
                        .find(|c: char| !is_token_char(c))
                        .unwrap_or(value.len() - token_start);
                    if frag_end == 0 {
                        cursor = token_start; // repeated spaces; keep probing
                        continue;
                    }
                    let frag = &value[token_start..token_start + frag_end];
                    let floor = if frag.chars().any(|c| c.is_ascii_digit()) {
                        4
                    } else {
                        8
                    };
                    if frag.chars().count() >= floor {
                        out.push(' ');
                        out.push_str("[REDACTED]");
                        cursor = token_start + frag_end;
                    } else {
                        break; // cursor stays at the space before the prose
                    }
                }
                consumed = cursor;
            }
        } else {
            // Too short to be a secret: leave it, or this function would mangle
            // ordinary text like "Bearer or not".
            out.push_str(&value[..end]);
        }
        rest = &tail[consumed + value_start + wrapper_len..];
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// A character that can appear inside a credential value. Beyond base64url's
/// `-` and `_`, a JWT joins its `header.payload.signature` segments with `.`,
/// and opaque keys use the base64 punctuation `+`, `/`, `~`, `=`. Stopping at
/// any of these would leak the un-redacted remainder of the credential into
/// memory, so the value runs until a character that cannot be part of a token.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '~' | '=')
}

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
            body: redact_secrets(content).into_owned(),
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
        // `search` scans the whole company, so a hit is only this agent's when
        // its addr carries one of our labels. Fetch the in-scope label set once
        // and filter by addr; the stored `{ns}/{key}` tail also corrects the
        // entry metadata, which previously reported the requested namespace
        // with an empty key.
        let prefix = match opts.namespace {
            Some(ns) => self.namespace_prefix(ns),
            None => format!("{}/", self.scope()),
        };
        let in_scope: HashMap<ChunkAddr, String> = self
            .context
            .list(&self.company, &prefix)
            .await
            .map_err(|e| anyhow::anyhow!("context list failed: {e}"))?
            .into_iter()
            .map(|meta| (meta.addr, meta.label))
            .collect();
        let hits = self
            .context
            .search(&self.company, query, limit)
            .await
            .map_err(|e| anyhow::anyhow!("context search failed: {e}"))?;
        let min = opts.min_score.unwrap_or(0.0);
        Ok(hits
            .into_iter()
            .filter(|h| h.score >= min)
            .filter_map(|h| {
                let label = in_scope.get(&h.addr)?;
                let (ns, key) = self.split_label(label)?;
                let id: String = h.addr.as_ref().to_string();
                Some(self.entry_from(id, ns, key, h.snippet, Some(h.score)))
            })
            .collect())
    }

    async fn recall_relevant_by_vector(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        min_vector_similarity: f64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        // Same scoping rule as `recall`: `search` scans the whole company, so a
        // hit is only relevant when its addr is one of ours. The label set is
        // read once, before the search, so a degrading search still filters.
        let in_scope: std::collections::HashSet<ChunkAddr> = self
            .context
            .list(&self.company, &self.namespace_prefix(namespace))
            .await
            .map_err(|e| anyhow::anyhow!("context list failed: {e}"))?
            .into_iter()
            .map(|meta| meta.addr)
            .collect();
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
            .filter(|h| in_scope.contains(&h.addr))
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

    #[test]
    fn redact_secrets_removes_the_value_but_keeps_the_prose() {
        // A one-time-secret link: the key is stripped, the surrounding sentence
        // (the context an agent needs) is kept.
        assert_eq!(
            redact_secrets("here it is https://ots.example/secret/AbCdEf123456 open it"),
            "here it is https://ots.example/secret/[REDACTED] open it"
        );
        // A bearer token in prose.
        assert_eq!(
            redact_secrets("auth with Bearer sk-verylongsecrettoken please"),
            "auth with Bearer [REDACTED] please"
        );
        // A JWT carries `.` between its base64url segments; the whole
        // credential is consumed, not just the header segment.
        assert_eq!(
            redact_secrets(
                "auth with Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.aSignature0123456789 please"
            ),
            "auth with Bearer [REDACTED] please"
        );
        // An opaque key with base64 punctuation (`+`, `/`, `~`, `=`) is also
        // consumed whole rather than leaking the part after the first such char.
        assert_eq!(
            redact_secrets("auth with Bearer aGVsbG8r/d29ybGQ=andtheRestOfTheKey please"),
            "auth with Bearer [REDACTED] please"
        );
        // A short bearer credential is still a secret: the MCP config accepts
        // any non-empty bearer value, so a token-shaped value like `s3cret`
        // (it contains a digit) is redacted despite being under the prose
        // length floor.
        assert_eq!(
            redact_secrets("auth with Bearer s3cret please"),
            "auth with Bearer [REDACTED] please"
        );
        // A digit-free credential is redacted from six characters on: a real
        // if weak value like `secret` must not persist, while prose words
        // after the marker stay short enough to survive.
        assert_eq!(
            redact_secrets("auth with Bearer secret please"),
            "auth with Bearer [REDACTED] please"
        );
        // The scheme is matched case-insensitively (RFC 9110's auth-scheme
        // ABNF), so lower- and upper-case `bearer` are credentials too.
        assert_eq!(
            redact_secrets("auth with bearer sk-longsecret please"),
            "auth with bearer [REDACTED] please"
        );
        assert_eq!(
            redact_secrets("auth with BEARER sk-longsecret please"),
            "auth with BEARER [REDACTED] please"
        );
        // Lower-case prose survives: `bond` (4) is under the digit-free floor.
        assert_eq!(
            redact_secrets("the bearer bond matures in June"),
            "the bearer bond matures in June"
        );
        // Lower-case "bearer" in its ordinary English sense: a plain word
        // after it is prose, not a credential, and must survive untouched.
        assert_eq!(
            redact_secrets("the standard bearer candidate won the race"),
            "the standard bearer candidate won the race"
        );
        assert_eq!(
            redact_secrets("the ring bearer walked down the aisle"),
            "the ring bearer walked down the aisle"
        );
        // A Markdown-backtick or quote wrapper around a credential (formatted
        // chat) must not defeat the scan.
        assert_eq!(
            redact_secrets("auth with Bearer `sk-verylongsecret` please"),
            "auth with Bearer `[REDACTED]` please"
        );
        assert_eq!(
            redact_secrets("auth with Bearer \"sk-verylongsecret\" please"),
            "auth with Bearer \"[REDACTED]\" please"
        );
        // The MCP config keeps the whole trimmed remainder as the bearer value,
        // so a credential can span space-separated fragments — every fragment
        // of the run is redacted, not just the first.
        assert_eq!(
            redact_secrets("auth with Bearer firstpart secondpart please"),
            "auth with Bearer [REDACTED] [REDACTED] please"
        );
        // Trailing prose after a single-token credential stays readable: a
        // short fragment like "please" (6) is under the continuation floor.
        assert_eq!(
            redact_secrets("auth with Bearer sk-abc123 please"),
            "auth with Bearer [REDACTED] please"
        );
        // No marker: borrowed through untouched, no allocation.
        assert!(matches!(
            redact_secrets("nothing secret here"),
            std::borrow::Cow::Borrowed(_)
        ));
        // Too short after the marker to be a secret: left alone, so ordinary
        // text like "Bearer or not" is not mangled.
        assert_eq!(redact_secrets("Bearer or not"), "Bearer or not");
        // The MCP config trims the value, so extra whitespace between the
        // marker and a credential is legal — and must not leave the credential
        // verbatim because the value scan stopped at the first space.
        assert_eq!(
            redact_secrets("auth with Bearer   sk-longsecret please"),
            "auth with Bearer   [REDACTED] please"
        );
        // Dots do not turn a short prose word into a secret: "key." is still
        // under the digit-free threshold, and plain "token" (no trailing dot)
        // is too.
        assert_eq!(redact_secrets("Bearer key. Please"), "Bearer key. Please");
        assert_eq!(redact_secrets("Bearer token please"), "Bearer token please");
    }

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
                    // The mock does not model store time; these tests exercise
                    // the adapter, not the Brain's freshness stat.
                    stored_at_millis: 0,
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

        async fn delete(&self, _id: &CompanyId, addr: &ChunkAddr) -> crate::Result<bool> {
            let mut guard = self.chunks.lock().unwrap();
            let before = guard.len();
            guard.retain(|(a, _)| a != addr);
            Ok(guard.len() < before)
        }

        async fn delete_label(
            &self,
            _id: &CompanyId,
            addr: &ChunkAddr,
            label: &str,
        ) -> crate::Result<bool> {
            let mut guard = self.chunks.lock().unwrap();
            let before = guard.len();
            guard.retain(|(a, c)| !(a == addr && c.label == label));
            Ok(guard.len() < before)
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
        cfo.store("global", "secret", "cfo-only", MemoryCategory::Core, None)
            .await
            .unwrap();

        // The CFO shares the company + ContextStore but must not see the CEO's
        // namespaced entry. Each agent reads back its *own* value — the CEO's
        // "ceo-only" content is isolated under the CEO's agent scope and never
        // surfaces for the CFO.
        let cfo_got = cfo
            .get("global", "secret")
            .await
            .unwrap()
            .expect("the CFO stored this key itself");
        assert_eq!(cfo_got.content, "cfo-only");
        let ceo_got = ceo
            .get("global", "secret")
            .await
            .unwrap()
            .expect("the CEO stored this key itself");
        assert_eq!(ceo_got.content, "ceo-only");
        assert_eq!(ceo.count().await.unwrap(), 1);
        assert_eq!(cfo.count().await.unwrap(), 1);

        // Both recall methods are scoped the same way: a query both entries
        // match surfaces only the caller's own chunk, and the recalled entry
        // carries the stored namespace and key rather than a substituted
        // default.
        let ceo_hits = ceo
            .recall(
                "only",
                5,
                RecallOpts {
                    namespace: Some("global"),
                    ..RecallOpts::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(ceo_hits.len(), 1);
        assert_eq!(ceo_hits[0].content, "ceo-only");
        assert_eq!(ceo_hits[0].namespace.as_deref(), Some("global"));
        assert_eq!(ceo_hits[0].key, "secret");

        let cfo_vec = cfo
            .recall_relevant_by_vector("global", "only", 5, 0.0)
            .await
            .unwrap();
        assert_eq!(cfo_vec.len(), 1);
        assert_eq!(cfo_vec[0].1, "cfo-only");
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
        // The recalled entry answers with the stored label's tail, not the
        // requested default: namespace "global" and the real key.
        assert_eq!(hits[0].namespace.as_deref(), Some("global"));
        assert_eq!(hits[0].key, "note");
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
