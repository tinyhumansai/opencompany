//! Memory-fact reads + writes: `GET /memory`, `GET /memory/stats`,
//! `POST /memory`, `DELETE /memory/{fact_id}` under both scope forms.
//!
//! Bodies mirror the console's `MemoryEntry` (`frontend/src/api/memory.ts`).
//! Facts land in the [`FactStore`](crate::ports::FactStore) — the console's
//! durable Memory/Brain view. A create *also* mirrors the fact into the
//! [`ContextStore`](crate::ports::ContextStore) the embedded agents recall from,
//! so an operator note is agent-recallable on the next turn (see
//! [`create_fact`]). A delete journals a [`CompanyEvent::MemoryFactDeleted`] to
//! the `EventLog` per the Operator-rights section of
//! `docs/spec/company-brain/memory.md`.
//!
//! ## The delete → reap seam
//!
//! Deleting a fact removes it from the `FactStore` AND reaps its mirrored
//! `operator-fact/{id}` context chunk — the delete port this comment once
//! said was missing landed with #1290, and leaving the reap unwired would
//! have kept showing the operator "deleted" while agents still recalled it.
//! The reap is label-scoped since #1300: chunks are content-addressed, so a
//! mirror whose byte-identical body is indexed under any OTHER label loses
//! exactly the mirror's own claim — the other label keeps the body, and the
//! body goes only with its last claim, atomically inside the port.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::facts::{FactKind, FactRecord};
use crate::ports::types::{ChunkAddr, ChunkMeta, CompanyEvent, ContextChunk};
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// The deliberate-memory label family's prefix, with its separator.
///
/// A LOCAL copy: the authoring module (`crate::harness::built_in::memory_tools`,
/// `AGENT_MEMORY_LABEL_PREFIX`) is `openhuman`-gated and this file is not, so
/// the constant cannot be imported on every shape. The gated test below pins
/// the two spellings together, which makes the lockstep compiler-checked on
/// exactly the lanes that build both.
fn const_format_prefix() -> &'static str {
    "agent-memory/"
}

#[cfg(all(test, feature = "openhuman"))]
mod label_lockstep_test {
    /// A rename of the tool module's prefix must break here, not silently
    /// mis-attribute every deliberate memory in the Brain view.
    #[test]
    fn the_brain_view_parses_the_prefix_the_tools_write() {
        assert_eq!(
            super::const_format_prefix(),
            format!(
                "{}/",
                crate::harness::built_in::memory_tools::AGENT_MEMORY_LABEL_PREFIX
            )
        );
    }
}

/// Label prefix for the [`ContextStore`](crate::ports::ContextStore) mirror of
/// an operator-authored fact. Keyed by fact id so [`reap_fact_mirror`] can
/// find and remove the mirror's claim when the fact is deleted.
const OPERATOR_FACT_PREFIX: &str = "operator-fact";

/// Label prefix under which the harness stores completed task outcomes.
///
/// Mirrors `harness::memory_loop::OUTCOME_LABEL_PREFIX`, duplicated here because
/// that module is `openhuman`-gated while this route is always compiled. Kept in
/// sync by the `outcome_prefix_matches_harness` test under the feature.
const OUTCOME_LABEL_PREFIX: &str = "task-outcome";

/// Builds the memory route fragment.
pub fn router() -> Router<AppState> {
    scoped("/memory", post(create_fact).get(list_facts))
        .merge(scoped("/memory/stats", get(memory_stats)))
        .merge(scoped("/memory/{fact_id}", delete(delete_fact)))
}

/// Upper bound on context-store entries materialised into the list, so a company
/// with a very large learned-context store can't force an unbounded number of
/// chunk-body reads on a single `GET /memory`. The stats endpoint only counts
/// (no per-chunk read), so it stays unbounded; the list caps its reads here.
const MAX_CONTEXT_ENTRIES: usize = 500;

/// Max characters kept for a context entry's synthesised title (its first line).
const CONTEXT_TITLE_MAX: usize = 120;

/// Where a rendered [`MemoryEntry`] came from. The console keys "editable vs
/// read-only" and the source label off this: only [`Fact`](MemoryOrigin::Fact)
/// rows are operator-authored and therefore deletable; the two context-derived
/// origins are the agents' own runtime memory and are read-only.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum MemoryOrigin {
    /// A durable operator-authored fact (FactStore). Editable + deletable.
    Fact,
    /// A learned-context chunk the agents recall from (ContextStore). Read-only.
    AgentMemory,
    /// A stored task outcome the harness wrote (ContextStore). Read-only.
    TaskOutcome,
    /// A chunk of a document or link an operator dropped on the Brain page
    /// (`crate::server::ops::memory_ingest`). Its own origin rather than
    /// folded into [`AgentMemory`](MemoryOrigin::AgentMemory): an operator has
    /// to be able to see what their upload became, and rendering it as
    /// something a teammate learned would say the wrong thing about where the
    /// knowledge came from.
    Document,
}

/// A durable memory entry as the console renders it.
///
/// Carries entries from two backends: operator facts (FactStore) and the agents'
/// runtime context chunks (ContextStore). `origin` + `editable` let the console
/// tell them apart — facts are editable/deletable, context rows are read-only.
/// `kind` is only meaningful for facts, so it is omitted for context rows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryEntry {
    id: String,
    /// The fact taxonomy — present only on `Fact` rows (omitted for context).
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<FactKind>,
    /// Which backend the row came from; drives editable-vs-read-only rendering.
    origin: MemoryOrigin,
    /// Whether the operator may delete this row: facts, and the documents
    /// they dropped on the Brain page. Never the agents' own memory.
    editable: bool,
    title: String,
    body: String,
    source: String,
    updated_at: u64,
}

impl From<FactRecord> for MemoryEntry {
    fn from(f: FactRecord) -> Self {
        Self {
            id: f.id,
            kind: Some(f.kind),
            origin: MemoryOrigin::Fact,
            editable: true,
            title: f.title,
            body: f.body,
            source: f.source,
            updated_at: f.updated_at_millis,
        }
    }
}

/// A context chunk pulled from the [`ContextStore`](crate::ports::ContextStore),
/// carrying its content address (for a stable row id), logical label (for origin
/// classification), and body (peeked). The input to [`context_entries`].
struct RawChunk {
    addr: String,
    label: String,
    body: String,
    /// Epoch-millis the chunk was first stored (`0` when the backend has no
    /// stamp for it — chunks written before backends began recording one).
    stored_at_millis: u64,
}

/// Truncates `s` to at most `max` characters on a char boundary, appending an
/// ellipsis when anything was dropped. Char-based (never byte-slices) so a
/// multibyte body can't panic mid-codepoint.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// Splits a chunk body into a short title (its first non-empty line, truncated)
/// and the remaining body. Used to render a context chunk as a titled card.
fn split_title_body(body: &str) -> (String, String) {
    let trimmed = body.trim();
    let mut parts = trimmed.splitn(2, '\n');
    let first = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    (truncate_chars(first, CONTEXT_TITLE_MAX), rest.to_string())
}

/// Turns peeked context chunks into read-only [`MemoryEntry`]s, newest first
/// across both origins. Drops operator-fact mirrors (they are already
/// represented by their FactStore row — never double-list them) and applies
/// `query` (case-insensitive substring over the chunk body) so the list's
/// free-text search reaches context rows too, matching fact search.
///
/// Ordering is by stamp, never by origin. Bucketing agent memories ahead of
/// task outcomes put *every* outcome behind *every* memory whatever their
/// stamps, so the newest memory in the company could render last — the #1488
/// symptom reached by a second route, and past the cap the sort in
/// [`capped_newest_first`] was the only thing keeping it out of the list at
/// all. The console filters by origin client-side, so the grouping bought
/// nothing. The `id` tie-break carries the addr AND the label, so chunks
/// sharing a millisecond — including two labels claiming one address (#1300)
/// — keep a total, call-stable order, as the cap's sort does.
fn context_entries(chunks: Vec<RawChunk>, query: Option<&str>) -> Vec<MemoryEntry> {
    let needle = query.map(|q| q.to_lowercase());
    let mirror_prefix = format!("{OPERATOR_FACT_PREFIX}/");
    let outcome_prefix = format!("{OUTCOME_LABEL_PREFIX}/");
    let document_prefix = format!("{}/", crate::ingest::DOCUMENT_LABEL_PREFIX);

    let mut entries: Vec<MemoryEntry> = Vec::new();

    for chunk in chunks {
        // The operator-fact mirror is the same knowledge as its FactStore row;
        // surfacing it here would double-list the operator's note.
        if chunk.label.starts_with(&mirror_prefix) {
            continue;
        }
        if let Some(ref q) = needle
            && !chunk.body.to_lowercase().contains(q)
        {
            continue;
        }

        let (origin, source): (MemoryOrigin, String) = if chunk.label.starts_with(&document_prefix)
        {
            // The document's own name, which `ingest::chunk_document`
            // writes as the chunk's first line for exactly this reason: a
            // label is slugged and truncated, so it cannot be rendered
            // back as the file the operator dropped.
            let named = chunk
                .body
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .unwrap_or("a document");
            (MemoryOrigin::Document, named.to_string())
        } else if let Some(agent) = chunk.label.strip_prefix(&outcome_prefix) {
            let who = if agent.is_empty() { "an agent" } else { agent };
            (MemoryOrigin::TaskOutcome, who.to_string())
        } else {
            // Deliberate memories live one segment deeper —
            // `agent-memory/<agent>/<slug>` — so the naive first-segment
            // parse attributed every one of them to the literal
            // "agent-memory" (the #1290 review's M2).
            let who = match chunk.label.strip_prefix(const_format_prefix()) {
                Some(rest) => rest.split('/').next().filter(|s| !s.is_empty()),
                None => chunk.label.split('/').next().filter(|s| !s.is_empty()),
            };
            (
                MemoryOrigin::AgentMemory,
                who.unwrap_or("an agent").to_string(),
            )
        };

        let (mut title, body) = split_title_body(&chunk.body);
        if title.is_empty() {
            title = match origin {
                MemoryOrigin::TaskOutcome => "Task outcome".to_string(),
                MemoryOrigin::Document => "Document".to_string(),
                _ => "Agent memory".to_string(),
            };
        }

        entries.push(MemoryEntry {
            // Prefix so a context row's id can never collide with a fact id
            // (delete targets fact ids only; this keeps React keys unique too).
            // The LABEL is part of the id, not just the address: chunks are
            // content-addressed and one address carries one row per label
            // claiming it (#1300), so two rows here can share an address —
            // byte-identical text two agents both remembered. Keyed by address
            // alone they would collide, and the console renders these by id.
            id: format!("ctx:{}:{}", chunk.addr, chunk.label),
            kind: None,
            origin,
            // A document is material the operator supplied, so they may take
            // it back — through `…/memory/document/{slug}`, which forgets the
            // whole document rather than this one chunk of it. The two agent
            // origins stay read-only: they are the record of what the company
            // did, not something anybody typed.
            editable: matches!(origin, MemoryOrigin::Document),
            title,
            body,
            source,
            // A chunk stored before backends began stamping reports `0`, which
            // the console still renders as `—`.
            updated_at: chunk.stored_at_millis,
        });
    }

    // The cap upstream sorted metas; re-sort here because the query filter and
    // the mirror drop above both reshape the set, and because a caller that
    // hands us chunks in any other order still gets newest-first.
    entries.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    entries
}

/// The create-fact body.
#[derive(Debug, Deserialize)]
struct CreateFact {
    kind: FactKind,
    title: String,
    body: String,
    #[serde(default)]
    source: Option<String>,
}

/// Query params for `GET /memory`: an optional free-text `query` and `kind`
/// filter, both applied by the [`FactStore`](crate::ports::FactStore).
#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<FactKind>,
}

/// The sub-resource path (`fact_id`).
#[derive(Debug, Deserialize)]
struct FactPath {
    fact_id: String,
}

/// The Brain-tab health snapshot: how much the company remembers, across the
/// operator's durable facts and the agents' runtime context chunks. Lets the
/// console prove the store is live (non-fake) at a glance.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStats {
    /// Number of durable operator facts.
    facts: usize,
    /// The newest fact's last-updated epoch-millis (`0` when there are none).
    facts_updated_at_millis: u64,
    /// The newest epoch-millis across *every* memory source — operator facts
    /// and the agents' context chunks alike (`0` when the company remembers
    /// nothing yet).
    ///
    /// This, not [`Self::facts_updated_at_millis`], is what the console's
    /// "Last updated" stat renders. Agents only ever write to the
    /// `ContextStore`, so a facts-only figure sat at `0` — and the stat at "—"
    /// — for any company whose operator had not hand-authored a fact, however
    /// much memory the agents had accumulated.
    last_updated_at_millis: u64,
    /// Total agent-accessible context chunks — learned context, task outcomes,
    /// and the operator-fact mirrors together.
    agent_chunks: usize,
    /// Of those chunks, how many are stored task outcomes.
    task_outcomes: usize,
}

/// `GET /memory` — everything the company remembers, so the console lists what
/// the Brain header counts. Two sources, in this order:
///
/// 1. **Operator facts** (FactStore) — newest-first, editable/deletable.
/// 2. **Context rows** (ContextStore chunks that are not operator-fact
///    mirrors) — read-only, newest-first, agent memories and task outcomes
///    interleaved by stamp rather than grouped by origin, so the newest
///    memory in the company always heads the context half. The console's
///    type filter separates the two origins client-side.
///
/// `?query=` (case-insensitive substring over title + body) filters all three.
/// `?kind=` is a *fact* taxonomy filter, so when it is set the context sources
/// are omitted (they have no `FactKind`) and only matching facts are returned —
/// preserving the type-filter's original facts-only meaning while the console's
/// wider "agent memory / task outcome" filters run client-side.
async fn list_facts(
    company: ScopedCompany,
    Query(ListQuery { query, kind }): Query<ListQuery>,
) -> Result<Json<Vec<MemoryEntry>>, ApiError> {
    let rows = company
        .runtime
        .facts()
        .list(company.id(), query.as_deref(), kind)
        .await?;
    let mut entries: Vec<MemoryEntry> = rows.into_iter().map(MemoryEntry::from).collect();

    // A fact-kind filter is inherently facts-only — context chunks carry no
    // `FactKind`, so skip them (and the reads) when one is set.
    if kind.is_none() {
        // `""` lists every chunk; drop the operator-fact mirrors before peeking
        // so we neither double-list them nor pay to read their bodies, then cap
        // the reads so a huge context store can't unbound this request.
        let mirror_prefix = format!("{OPERATOR_FACT_PREFIX}/");
        let metas = company.runtime.context.list(company.id(), "").await?;
        let metas = capped_newest_first(metas, &mirror_prefix, MAX_CONTEXT_ENTRIES);
        // One batched read for every surviving body — how few round trips
        // that really is, is the backend's business (see `peek_many`); what
        // matters here is that the route no longer peeks per chunk. A body
        // that cannot be read
        // degrades that row to empty rather than failing the whole list, but
        // log it so a real storage fault surfaces instead of silently
        // rendering blank cards.
        let addrs: Vec<ChunkAddr> = metas.iter().map(|m| m.addr.clone()).collect();
        let bodies = match company
            .runtime
            .context
            .peek_many(company.id(), &addrs)
            .await
        {
            Ok(bodies) => bodies,
            Err(err) => {
                tracing::warn!(
                    company = %company.id(),
                    error = %err,
                    "bulk context read failed; rendering empty bodies"
                );
                vec![None; metas.len()]
            }
        };
        let chunks = metas
            .into_iter()
            .zip(bodies)
            .map(|(meta, body)| {
                let body = body.unwrap_or_else(|| {
                    tracing::warn!(
                        company = %company.id(),
                        addr = %meta.addr,
                        "failed to peek context chunk; rendering empty body"
                    );
                    String::new()
                });
                RawChunk {
                    addr: meta.addr.to_string(),
                    label: meta.label,
                    body,
                    stored_at_millis: meta.stored_at_millis,
                }
            })
            .collect();
        entries.extend(context_entries(chunks, query.as_deref()));
    }

    Ok(Json(entries))
}

/// Drops the operator-fact mirrors, then keeps the newest `cap` chunks.
///
/// Every backend lists oldest-first, so capping the head would pin the Brain
/// view to the oldest `cap` chunks forever — once a company crossed the cap, a
/// new memory could never appear again. Sort newest-first BEFORE capping; the
/// (addr, label) tie-break keeps the order total, so chunks stamped in the
/// same millisecond cannot swap places between calls. The label is part of
/// that tie-break because one address carries one row per label claiming it
/// (#1300), so the addr alone no longer separates two rows.
fn capped_newest_first(metas: Vec<ChunkMeta>, mirror_prefix: &str, cap: usize) -> Vec<ChunkMeta> {
    let mut metas: Vec<ChunkMeta> = metas
        .into_iter()
        .filter(|m| !m.label.starts_with(mirror_prefix))
        .collect();
    metas.sort_by(|a, b| {
        b.stored_at_millis
            .cmp(&a.stored_at_millis)
            .then_with(|| a.addr.as_ref().cmp(b.addr.as_ref()))
            .then_with(|| a.label.cmp(&b.label))
    });
    metas.truncate(cap);
    metas
}

/// `GET /memory/stats` — counts across the fact store and the agents' context
/// store, so the console's Brain health strip reflects the real backend.
async fn memory_stats(company: ScopedCompany) -> Result<Json<MemoryStats>, ApiError> {
    let facts = company
        .runtime
        .facts()
        .list(company.id(), None, None)
        .await?;
    // `list` is newest-first, so the head carries the freshest timestamp.
    let facts_updated_at_millis = facts.first().map(|f| f.updated_at_millis).unwrap_or(0);
    // Prefix `""` lists every chunk; the task-outcome prefix narrows to stored
    // outcomes (a subset of the total).
    let chunks = company.runtime.context.list(company.id(), "").await?;
    let agent_chunks = chunks.len();
    // Chunks list in insertion order, not freshness order, and a backend that
    // predates the stamp reports `0` — so take the max rather than the head.
    let chunks_stored_at_millis = chunks.iter().map(|m| m.stored_at_millis).max().unwrap_or(0);
    let task_outcomes = company
        .runtime
        .context
        .list(company.id(), OUTCOME_LABEL_PREFIX)
        .await?
        .len();
    Ok(Json(MemoryStats {
        facts: facts.len(),
        facts_updated_at_millis,
        last_updated_at_millis: facts_updated_at_millis.max(chunks_stored_at_millis),
        agent_chunks,
        task_outcomes,
    }))
}

async fn create_fact(
    company: ScopedCompany,
    Json(body): Json<CreateFact>,
) -> Result<Json<MemoryEntry>, ApiError> {
    let record = FactRecord {
        id: generate_id(),
        kind: body.kind,
        title: body.title,
        body: body.body,
        source: body.source.unwrap_or_else(|| "You".to_string()),
        updated_at_millis: now_millis(),
    };
    company
        .runtime
        .facts()
        .upsert(company.id(), &record)
        .await?;

    // Mirror the fact into the agents' ContextStore so an operator note becomes
    // recallable on the agent's next turn. The harness retrieve→inject step
    // searches the ContextStore (not the FactStore), so without this mirror an
    // operator-added fact would land in the console but never reach an agent —
    // the manual-ingest loop would stay open. Best-effort: the fact is already
    // durable, so a mirror failure degrades recall (logged) rather than failing
    // the operator's write. See the module doc for the append-only-delete seam.
    let chunk = ContextChunk {
        label: format!("{OPERATOR_FACT_PREFIX}/{}", record.id),
        body: format!("{}\n{}", record.title, record.body),
    };
    if let Err(err) = company.runtime.context.put(company.id(), chunk).await {
        tracing::warn!(
            company = %company.id(),
            fact = %record.id,
            error = %err,
            "operator-fact context mirror failed; fact saved but not agent-recallable"
        );
    }

    Ok(Json(record.into()))
}

async fn delete_fact(
    company: ScopedCompany,
    Path(FactPath { fact_id }): Path<FactPath>,
) -> Result<StatusCode, ApiError> {
    if company
        .runtime
        .facts()
        .delete(company.id(), &fact_id)
        .await?
    {
        // Reap the mirrored context chunk so recall stops serving a fact the
        // operator just deleted. Best-effort: the fact row is already gone,
        // and a reap failure leaves only the pre-#1290 status quo (a stale
        // mirror), which must not turn the successful delete into an error —
        // but it must be visible, because agents keep recalling the survivor
        // while the Brain view shows the fact gone.
        if let Err(err) =
            reap_fact_mirror(company.runtime.context.as_ref(), company.id(), &fact_id).await
        {
            tracing::warn!(
                fact_id = %fact_id,
                error = %err,
                "fact deleted but its context mirror could not be reaped; \
                 recall may keep serving it until a retry"
            );
        }
        // Journal the operator deletion to the event log (audit trail).
        company
            .runtime
            .events()
            .append(
                company.id(),
                CompanyEvent::MemoryFactDeleted {
                    fact_id: fact_id.clone(),
                },
            )
            .await?;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "fact {fact_id}"
        ))))
    }
}

/// Removes the `operator-fact/{fact_id}` mirror's claim on its chunk(s),
/// label-scoped (`ContextStore::delete_label`, issue #1300): exactly the
/// mirror's own index entry goes, and the body is reaped only when no other
/// label claims it — decided atomically inside the port, so a write of
/// byte-identical content landing mid-reap keeps its row by construction
/// (this function used to snapshot-check for shared labels and then delete
/// the whole address, which both raced that write and left a shared mirror's
/// row behind forever; now the shared case removes the mirror's claim and
/// the other label keeps the body).
pub(crate) async fn reap_fact_mirror(
    context: &dyn crate::ports::ContextStore,
    company: &crate::ports::CompanyId,
    fact_id: &str,
) -> crate::Result<()> {
    let mirror_label = format!("{OPERATOR_FACT_PREFIX}/{fact_id}");
    let all = context.list(company, "").await?;
    for meta in all.iter().filter(|m| m.label == mirror_label) {
        context
            .delete_label(company, &meta.addr, &mirror_label)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod reap_test {
    use super::*;
    use crate::ports::ContextStore;
    use crate::ports::types::{CompanyId, ContextChunk};
    use crate::store::FsContextStore;
    use std::sync::Arc;

    /// Deleting a fact reaps its mirror. A mirror whose content is shared
    /// with another label loses exactly the mirror's own claim (label-scoped
    /// delete, #1300): the other label keeps the body, and — unlike the old
    /// shared-address skip — the mirror row itself no longer lingers in the
    /// index serving a deleted fact.
    #[tokio::test]
    async fn the_mirror_is_reaped_and_a_shared_body_survives_under_its_other_label() {
        let dir = tempfile::tempdir().unwrap();
        let context: Arc<dyn ContextStore> =
            Arc::new(FsContextStore::new(dir.path().to_path_buf()));
        let company = CompanyId::new("acme");

        let lone = context
            .put(
                &company,
                ContextChunk {
                    label: "operator-fact/f1".into(),
                    body: "fact one".into(),
                },
            )
            .await
            .unwrap();
        reap_fact_mirror(context.as_ref(), &company, "f1")
            .await
            .unwrap();
        assert!(
            context.peek(&company, &lone, None).await.is_err(),
            "the unshared mirror must be reaped"
        );

        // Identical bodies share one address: the reap removes the mirror's
        // claim, and the agent's row keeps the body.
        let shared = context
            .put(
                &company,
                ContextChunk {
                    label: "operator-fact/f2".into(),
                    body: "shared text".into(),
                },
            )
            .await
            .unwrap();
        context
            .put(
                &company,
                ContextChunk {
                    label: "agent-memory/ceo/note".into(),
                    body: "shared text".into(),
                },
            )
            .await
            .unwrap();
        reap_fact_mirror(context.as_ref(), &company, "f2")
            .await
            .unwrap();
        context
            .peek(&company, &shared, None)
            .await
            .expect("the body must survive under the agent's label");
        let labels: Vec<String> = context
            .list(&company, "")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.addr == shared)
            .map(|m| m.label)
            .collect();
        assert_eq!(
            labels,
            ["agent-memory/ceo/note"],
            "the mirror's claim must be gone; only the agent's remains"
        );
    }
}

#[cfg(test)]
mod combined_list_tests {
    use super::*;

    fn chunk(label: &str, body: &str) -> RawChunk {
        chunk_at(label, body, 0)
    }

    fn chunk_at(label: &str, body: &str, stored_at_millis: u64) -> RawChunk {
        RawChunk {
            addr: format!("addr-{label}"),
            label: label.to_string(),
            body: body.to_string(),
            stored_at_millis,
        }
    }

    /// A chunk whose address is set explicitly, for the stamp-tie tie-break
    /// (`chunk_at` derives the addr from the label, so two chunks with
    /// different labels can never share one).
    fn chunk_at_addr(addr: &str, label: &str, body: &str, stored_at_millis: u64) -> RawChunk {
        RawChunk {
            addr: addr.to_string(),
            label: label.to_string(),
            body: body.to_string(),
            stored_at_millis,
        }
    }

    fn fact(id: &str, kind: FactKind, title: &str) -> FactRecord {
        FactRecord {
            id: id.to_string(),
            kind,
            title: title.to_string(),
            body: format!("{title} body"),
            source: "You".to_string(),
            updated_at_millis: 100,
        }
    }

    #[test]
    fn zero_facts_plus_context_yields_readonly_nonempty_list() {
        // The reported bug: header counts N context chunks but the list is empty
        // because it only read the (empty) FactStore. Prove context now surfaces.
        let chunks = vec![
            chunk("agent-1/notes", "Learned a thing\nmore detail"),
            chunk("task-outcome/agent-1", "Task: ship it\nOutcome: done"),
        ];
        let entries = context_entries(chunks, None);

        assert_eq!(entries.len(), 2, "both context chunks surface as entries");
        assert!(
            entries.iter().all(|e| !e.editable),
            "context rows are read-only"
        );
        assert!(
            entries.iter().all(|e| e.kind.is_none()),
            "context rows carry no fact kind"
        );
        // Ordering: agent memory before task outcomes.
        assert!(matches!(entries[0].origin, MemoryOrigin::AgentMemory));
        assert!(matches!(entries[1].origin, MemoryOrigin::TaskOutcome));
        assert_eq!(entries[1].source, "agent-1", "outcome source = agent id");
        // Title/body split off the first line.
        assert_eq!(entries[0].title, "Learned a thing");
        assert_eq!(entries[0].body, "more detail");
    }

    /// The #1290 review's M2 red-proof, now green: a deliberate memory's
    /// `agent-memory/<agent>/<slug>` label attributes to the AGENT, not to
    /// the literal first segment "agent-memory".
    #[test]
    fn deliberate_memories_are_attributed_to_the_storing_agent() {
        let chunks = vec![chunk(
            "agent-memory/ceo/fiscal-year",
            "Fiscal year\n\nFebruary",
        )];
        let entries = context_entries(chunks, None);
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].origin, MemoryOrigin::AgentMemory));
        assert_eq!(
            entries[0].source, "ceo",
            "the Brain view must name the agent that stored the memory"
        );
        assert_eq!(entries[0].title, "Fiscal year");
    }

    #[test]
    fn operator_fact_mirror_is_not_double_listed() {
        // The `operator-fact/{id}` chunk mirrors a FactStore row; it must NOT
        // appear as a second read-only row duplicating that fact.
        let chunks = vec![
            chunk("operator-fact/fact-123", "Client prefers Friday\nreviews"),
            chunk("agent-2/x", "genuine agent memory"),
        ];
        let entries = context_entries(chunks, None);

        assert_eq!(
            entries.len(),
            1,
            "mirror dropped; only agent memory remains"
        );
        assert!(matches!(entries[0].origin, MemoryOrigin::AgentMemory));
    }

    #[test]
    fn facts_editable_context_readonly() {
        // Facts get the delete affordance; read-only rows never do.
        let fact_entry = MemoryEntry::from(fact("f1", FactKind::Person, "Ada"));
        assert!(fact_entry.editable, "operator facts are deletable");
        assert!(matches!(fact_entry.origin, MemoryOrigin::Fact));
        assert_eq!(fact_entry.kind, Some(FactKind::Person));

        let ctx = context_entries(vec![chunk("task-outcome/a", "Task: t\nOutcome: o")], None);
        assert!(
            !ctx[0].editable,
            "read-only rows expose no edit/delete affordance"
        );
    }

    #[test]
    fn context_rows_carry_their_stored_at_stamp() {
        // Context rows used to hardcode `updated_at: 0`, so every agent-written
        // memory rendered "—" no matter how recently it landed.
        let entries = context_entries(
            vec![
                chunk_at("agent-1/notes", "recent", 2_000),
                chunk_at("task-outcome/agent-1", "Task: t\nOutcome: o", 3_000),
            ],
            None,
        );
        // Newest first, so the fresher task outcome heads the pair.
        assert_eq!(entries[0].updated_at, 3_000);
        assert_eq!(entries[1].updated_at, 2_000);
    }

    /// The route used to bucket rows by origin and concatenate — agent
    /// memories, then task outcomes — which pushed EVERY outcome behind EVERY
    /// memory whatever their stamps. A company whose newest memory is a task
    /// outcome saw it render last: the #1488 symptom by a second route.
    #[test]
    fn context_rows_interleave_the_two_origins_by_stamp() {
        let entries = context_entries(
            vec![
                chunk_at("task-outcome/agent-1", "newest outcome", 900),
                chunk_at("agent-memory/agent-1/five", "middle memory", 500),
                chunk_at("agent-memory/agent-1/one", "oldest memory", 100),
            ],
            None,
        );
        let stamps: Vec<u64> = entries.iter().map(|e| e.updated_at).collect();
        assert_eq!(
            stamps,
            vec![900, 500, 100],
            "context rows order by stamp across origins, not by origin"
        );
        assert!(matches!(entries[0].origin, MemoryOrigin::TaskOutcome));
    }

    #[test]
    fn context_rows_break_stamp_ties_by_addr_like_the_cap() {
        // Same tie-break as `capped_newest_first`, so chunks sharing a
        // millisecond cannot swap places between two calls.
        let entries = context_entries(
            vec![
                chunk_at_addr("z", "agent-1/z", "zulu", 500),
                chunk_at_addr("a", "task-outcome/agent-1", "alpha", 500),
            ],
            None,
        );
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["ctx:a:task-outcome/agent-1", "ctx:z:agent-1/z"],
            "the id carries addr then label, so the tie-break stays addr-first"
        );
    }

    /// One address, two labels — the #1300 shape — renders as two rows with
    /// DISTINCT ids. Keyed by address alone they collided, and the console
    /// renders these rows by id (React keys, row identity).
    #[test]
    fn two_labels_on_one_address_render_as_two_distinct_rows() {
        let entries = context_entries(
            vec![
                chunk_at_addr("shared", "agent-memory/ann/note", "same text", 500),
                chunk_at_addr("shared", "agent-memory/bob/note", "same text", 500),
            ],
            None,
        );
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "ctx:shared:agent-memory/ann/note",
                "ctx:shared:agent-memory/bob/note"
            ],
        );
        let sources: Vec<&str> = entries.iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, vec!["ann", "bob"], "each row keeps its own author");
    }

    #[test]
    fn unstamped_context_rows_stay_dashed() {
        // A chunk written before backends stamped has no store time; reporting
        // `0` keeps the console's "—" rather than inventing the epoch.
        let entries = context_entries(vec![chunk("agent-1/notes", "legacy")], None);
        assert_eq!(entries[0].updated_at, 0);
    }

    #[test]
    fn query_matches_across_context_rows() {
        let chunks = vec![
            chunk("agent-1/a", "alpha content"),
            chunk("agent-1/b", "beta content"),
        ];
        // Case-insensitive substring, same as fact search.
        let entries = context_entries(chunks, Some("BETA"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "beta content");
    }

    #[test]
    fn json_shape_facts_stable_context_omits_kind() {
        // Fact serialization is unchanged (kind present); context omits kind and
        // carries the read-only discriminator the console keys off.
        let fact_json =
            serde_json::to_value(MemoryEntry::from(fact("f", FactKind::Fact, "t"))).unwrap();
        assert_eq!(fact_json["kind"], "fact");
        assert_eq!(fact_json["origin"], "fact");
        assert_eq!(fact_json["editable"], true);

        let ctx = &context_entries(vec![chunk("agent-1/a", "hello world")], None)[0];
        let ctx_json = serde_json::to_value(ctx).unwrap();
        assert!(ctx_json.get("kind").is_none(), "context row omits kind");
        assert_eq!(ctx_json["origin"], "agent-memory");
        assert_eq!(ctx_json["editable"], false);
    }

    #[test]
    fn blank_body_falls_back_to_origin_label() {
        // A whitespace-only chunk body has no title line, so the row shows the
        // origin label rather than an empty heading.
        let ctx = context_entries(vec![chunk("task-outcome/a", "   ")], None);
        assert_eq!(ctx[0].title, "Task outcome");
        assert_eq!(ctx[0].body, "");
    }

    fn meta(addr: &str, label: &str, stored_at_millis: u64) -> ChunkMeta {
        ChunkMeta {
            addr: ChunkAddr::new(addr),
            label: label.to_string(),
            len: 0,
            stored_at_millis,
        }
    }

    /// #1488: backends list oldest-first, so capping before sorting pinned the
    /// Brain view to the oldest chunks forever — the cap must keep the newest.
    #[test]
    fn the_cap_keeps_the_newest_chunks_not_the_oldest() {
        let metas = vec![
            meta("a1", "agent-1/one", 100),
            meta("a2", "agent-1/two", 200),
            meta("a3", "agent-1/three", 300),
            meta("a4", "agent-1/four", 400),
        ];
        let capped = capped_newest_first(metas, "operator-fact/", 2);
        let stamps: Vec<u64> = capped.iter().map(|m| m.stored_at_millis).collect();
        assert_eq!(
            stamps,
            vec![400, 300],
            "the newest chunks survive the cap, newest first; the oldest fall off"
        );
    }

    #[test]
    fn the_cap_drops_mirrors_first_and_breaks_stamp_ties_by_addr() {
        // The mirror must not consume a cap slot even as the newest chunk, and
        // equal stamps must order deterministically (by addr) between calls.
        let metas = vec![
            meta("b", "agent-1/two", 500),
            meta("m", "operator-fact/f1", 900),
            meta("a", "agent-1/one", 500),
        ];
        let capped = capped_newest_first(metas, "operator-fact/", 2);
        let addrs: Vec<&str> = capped.iter().map(|m| m.addr.as_ref()).collect();
        assert_eq!(addrs, vec!["a", "b"]);
    }
}

#[cfg(test)]
mod route_tests {
    use std::collections::HashMap;
    use std::ops::Range;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::MAX_CONTEXT_ENTRIES;
    use crate::company::CompanyManifest;
    use crate::ports::context::ContextStore;
    use crate::ports::types::{
        ChunkAddr, ChunkHit, ChunkMeta, CompanyId, CompanyRecord, ContextChunk,
    };
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    /// A scripted [`ContextStore`]: `list` answers a fixed meta set (oldest
    /// first, as every real backend lists), and reads are counted so a test
    /// can pin HOW the route fetched bodies, not only what it rendered.
    struct ScriptedContext {
        metas: Vec<ChunkMeta>,
        bodies: HashMap<String, String>,
        single_peeks: AtomicUsize,
        bulk_peeks: AtomicUsize,
    }

    impl ScriptedContext {
        /// `total` chunks stamped `1..=total`, listed oldest-first, with the
        /// two context origins interleaved: even stamps are task outcomes,
        /// odd ones agent memories. The mix is load-bearing — with a
        /// single-origin fixture the route's own bucketing hid a cross-origin
        /// ordering bug from every newest-first assertion below (the newest
        /// chunk here, `total`, is deliberately a task outcome).
        fn with_chunks(total: usize) -> Arc<Self> {
            let mut metas = Vec::new();
            let mut bodies = HashMap::new();
            for i in 1..=total {
                let addr = format!("addr-{i:04}");
                let label = if i % 2 == 0 {
                    "task-outcome/agent-1".to_string()
                } else {
                    format!("agent-1/note-{i}")
                };
                metas.push(ChunkMeta {
                    addr: ChunkAddr::new(addr.clone()),
                    label,
                    len: 0,
                    stored_at_millis: i as u64,
                });
                bodies.insert(addr, format!("note {i}"));
            }
            Arc::new(Self {
                metas,
                bodies,
                single_peeks: AtomicUsize::new(0),
                bulk_peeks: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl ContextStore for ScriptedContext {
        async fn put(&self, _id: &CompanyId, chunk: ContextChunk) -> crate::Result<ChunkAddr> {
            // Nothing in these read-only tests writes context; answer with a
            // derived addr rather than failing an incidental write.
            Ok(ChunkAddr::new(format!("scripted/{}", chunk.label)))
        }

        async fn list(&self, _id: &CompanyId, prefix: &str) -> crate::Result<Vec<ChunkMeta>> {
            Ok(self
                .metas
                .iter()
                .filter(|m| m.label.starts_with(prefix))
                .cloned()
                .collect())
        }

        async fn peek(
            &self,
            _id: &CompanyId,
            addr: &ChunkAddr,
            _range: Option<Range<usize>>,
        ) -> crate::Result<String> {
            self.single_peeks.fetch_add(1, Ordering::SeqCst);
            self.bodies.get(addr.as_ref()).cloned().ok_or_else(|| {
                crate::error::OpenCompanyError::Store(format!(
                    "context chunk not found: {}",
                    addr.as_ref()
                ))
            })
        }

        async fn peek_many(
            &self,
            _id: &CompanyId,
            addrs: &[ChunkAddr],
        ) -> crate::Result<Vec<Option<String>>> {
            self.bulk_peeks.fetch_add(1, Ordering::SeqCst);
            Ok(addrs
                .iter()
                .map(|addr| self.bodies.get(addr.as_ref()).cloned())
                .collect())
        }

        async fn search(
            &self,
            _id: &CompanyId,
            _query: &str,
            _limit: usize,
        ) -> crate::Result<Vec<ChunkHit>> {
            Ok(Vec::new())
        }

        async fn delete(&self, _id: &CompanyId, _addr: &ChunkAddr) -> crate::Result<bool> {
            Ok(false)
        }

        async fn delete_label(
            &self,
            _id: &CompanyId,
            _addr: &ChunkAddr,
            _label: &str,
        ) -> crate::Result<bool> {
            Ok(false)
        }
    }

    /// An [`AppState`] whose company runtime reads context from `context`,
    /// with everything else on fresh fs stores under `home`.
    async fn state_over(home: &std::path::Path, context: Arc<ScriptedContext>) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .with_context(context)
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn get_memory(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/memory")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// #1488: with more chunks than the cap, the list must keep the NEWEST
    /// `MAX_CONTEXT_ENTRIES`, newest first. Before the fix the route took the
    /// head of the backend's oldest-first list, so a company past the cap
    /// never saw a new memory again.
    #[tokio::test]
    async fn the_brain_list_keeps_the_newest_chunks_once_over_the_cap() {
        let home = tempfile::tempdir().unwrap();
        let total = MAX_CONTEXT_ENTRIES + 2;
        let state = state_over(home.path(), ScriptedContext::with_chunks(total)).await;

        let (status, rows) = get_memory(&state).await;
        assert_eq!(status, StatusCode::OK);
        let rows = rows.as_array().expect("a JSON array");
        assert_eq!(rows.len(), MAX_CONTEXT_ENTRIES);
        let stamps: Vec<u64> = rows
            .iter()
            .map(|row| row["updatedAt"].as_u64().unwrap())
            .collect();
        assert_eq!(stamps[0], total as u64, "the newest chunk heads the list");
        assert_eq!(
            rows[0]["origin"], "task-outcome",
            "the newest chunk heads the list whatever its origin — grouping \
             outcomes behind memories put it last"
        );
        assert!(
            stamps.windows(2).all(|pair| pair[0] >= pair[1]),
            "context rows render newest-first"
        );
        assert!(
            stamps.iter().all(|stamp| *stamp > 2),
            "the oldest chunks fell off the cap, not the newest"
        );
    }

    /// The other half of #1488: bodies arrive through ONE bulk `peek_many`,
    /// not a peek-per-chunk loop.
    #[tokio::test]
    async fn the_brain_list_reads_bodies_in_one_bulk_peek() {
        let home = tempfile::tempdir().unwrap();
        let context = ScriptedContext::with_chunks(3);
        let state = state_over(home.path(), context.clone()).await;

        let (status, rows) = get_memory(&state).await;
        assert_eq!(status, StatusCode::OK);
        let rows = rows.as_array().expect("a JSON array");
        assert_eq!(rows.len(), 3);
        // The bodies really flowed through the bulk read (newest first).
        assert_eq!(rows[0]["title"], "note 3");
        assert_eq!(
            context.bulk_peeks.load(Ordering::SeqCst),
            1,
            "one bulk read"
        );
        assert_eq!(
            context.single_peeks.load(Ordering::SeqCst),
            0,
            "the per-chunk peek loop is gone"
        );
    }
}

#[cfg(all(test, feature = "openhuman"))]
mod tests {
    /// The local prefix constant must track the harness's, since the two label
    /// the same chunks from opposite sides of the `openhuman` feature gate.
    #[test]
    fn outcome_prefix_matches_harness() {
        assert_eq!(
            super::OUTCOME_LABEL_PREFIX,
            crate::harness::memory_loop::OUTCOME_LABEL_PREFIX
        );
    }
}
