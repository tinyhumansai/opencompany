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
//! ## Known limitation (flagged seam)
//!
//! Deleting a fact removes it from the `FactStore` AND reaps its mirrored
//! `operator-fact/{id}` context chunk — the delete port this comment once
//! said was missing landed with #1290, and leaving the reap unwired would
//! have kept showing the operator "deleted" while agents still recalled it.
//! The reap honors the shared-address rule: chunks are content-addressed, so
//! a mirror whose byte-identical body is indexed under any OTHER label is
//! left in place rather than deleting someone else's row.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::facts::{FactKind, FactRecord};
use crate::ports::types::{ChunkWithBody, CompanyEvent, ContextChunk};
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
/// an operator-authored fact. Keyed by fact id so a future delete port can reap
/// the mirror when the fact is deleted (today it lingers — see the module doc).
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
    /// Whether the operator may edit/delete this row (true only for facts).
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

/// Turns peeked context chunks into read-only [`MemoryEntry`]s, ordered agent
/// memory first then task outcomes. Drops operator-fact mirrors (they are
/// already represented by their FactStore row — never double-list them) and
/// applies `query` (case-insensitive substring over the chunk body) so the
/// list's free-text search reaches context rows too, matching fact search.
fn context_entries(chunks: Vec<RawChunk>, query: Option<&str>) -> Vec<MemoryEntry> {
    let needle = query.map(|q| q.to_lowercase());
    let mirror_prefix = format!("{OPERATOR_FACT_PREFIX}/");
    let outcome_prefix = format!("{OUTCOME_LABEL_PREFIX}/");

    let mut agent_memory = Vec::new();
    let mut task_outcomes = Vec::new();

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

        let (origin, source, bucket): (MemoryOrigin, String, &mut Vec<MemoryEntry>) =
            if let Some(agent) = chunk.label.strip_prefix(&outcome_prefix) {
                let who = if agent.is_empty() { "an agent" } else { agent };
                (
                    MemoryOrigin::TaskOutcome,
                    who.to_string(),
                    &mut task_outcomes,
                )
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
                    &mut agent_memory,
                )
            };

        let (mut title, body) = split_title_body(&chunk.body);
        if title.is_empty() {
            title = match origin {
                MemoryOrigin::TaskOutcome => "Task outcome".to_string(),
                _ => "Agent memory".to_string(),
            };
        }

        bucket.push(MemoryEntry {
            // Prefix so a context row's id can never collide with a fact id
            // (delete targets fact ids only; this keeps React keys unique too).
            id: format!("ctx:{}", chunk.addr),
            kind: None,
            origin,
            editable: false,
            title,
            body,
            source,
            // A chunk stored before backends began stamping reports `0`, which
            // the console still renders as `—`.
            updated_at: chunk.stored_at_millis,
        });
    }

    agent_memory.extend(task_outcomes);
    agent_memory
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
/// the Brain header counts. Three sources, in this order:
///
/// 1. **Operator facts** (FactStore) — newest-first, editable/deletable.
/// 2. **Agent memory** (ContextStore chunks that are neither task outcomes nor
///    operator-fact mirrors) — read-only.
/// 3. **Task outcomes** (ContextStore `task-outcome/*`) — read-only.
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
        // One enumeration carries every body, so the whole page costs a single
        // read on the durable backends (and one account read on the provider
        // overlay) rather than a `list` followed by `MAX_CONTEXT_ENTRIES`
        // per-row `peek`s — each a fresh account enumeration on that overlay.
        let with_bodies = company
            .runtime
            .context
            .list_with_bodies(company.id(), "")
            .await?;
        entries.extend(context_entries(
            newest_context_chunks(with_bodies),
            query.as_deref(),
        ));
    }

    Ok(Json(entries))
}

/// Selects the page of context chunks the Brain list renders: drops the
/// operator-fact mirrors, keeps the newest [`MAX_CONTEXT_ENTRIES`], and hands
/// them to [`context_entries`] as [`RawChunk`]s carrying the body already read.
///
/// Backends list oldest-first, so an unsorted `take` would freeze the list on
/// the first `MAX_CONTEXT_ENTRIES` chunks ever written and silently drop every
/// newer memory while `/memory/stats` kept counting up (issue #1488). Sorting
/// newest-first before the cap keeps the most recent memories, not the stalest.
/// An unstamped chunk (`0`) sorts last, behind anything with a real stamp.
fn newest_context_chunks(with_bodies: Vec<ChunkWithBody>) -> Vec<RawChunk> {
    let mirror_prefix = format!("{OPERATOR_FACT_PREFIX}/");
    let mut with_bodies = with_bodies;
    // Drop the operator-fact mirrors: they double-list their FactStore row.
    with_bodies.retain(|c| !c.meta.label.starts_with(&mirror_prefix));
    with_bodies.sort_by_key(|c| std::cmp::Reverse(c.meta.stored_at_millis));
    with_bodies
        .into_iter()
        .take(MAX_CONTEXT_ENTRIES)
        .map(|c| RawChunk {
            addr: c.meta.addr.to_string(),
            label: c.meta.label,
            body: c.body,
            stored_at_millis: c.meta.stored_at_millis,
        })
        .collect()
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
                 recall may keep serving it until a retry or #1300's reaper"
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

/// Removes the `operator-fact/{fact_id}` mirror chunk(s), shared-address
/// aware: an address also carrying any label OUTSIDE this mirror's own is
/// skipped — deleting it would delete that other row too (content
/// addressing; the same rule `memory_forget` enforces).
///
/// KNOWN RACE (#1300): the shared-label check is a snapshot; a write of
/// byte-identical content landing between the check and the delete loses its
/// row. The port has no conditional delete to close this — that is exactly
/// the label-scoped-delete work item in #1300, which fixes it by
/// construction. Until then the window is one operator HTTP call wide and
/// requires an adversarially-timed identical-content write.
pub(crate) async fn reap_fact_mirror(
    context: &dyn crate::ports::ContextStore,
    company: &crate::ports::CompanyId,
    fact_id: &str,
) -> crate::Result<()> {
    let mirror_label = format!("{OPERATOR_FACT_PREFIX}/{fact_id}");
    let all = context.list(company, "").await?;
    for meta in all.iter().filter(|m| m.label == mirror_label) {
        let shared = all
            .iter()
            .any(|m| m.addr == meta.addr && m.label != mirror_label);
        if !shared {
            context.delete(company, &meta.addr).await?;
        }
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

    /// Deleting a fact reaps its mirror; a mirror whose content is shared
    /// with another label survives (the other row must not vanish).
    #[tokio::test]
    async fn the_mirror_is_reaped_unless_its_address_is_shared() {
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

        // Identical bodies share one address: the reap must leave it.
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
            .expect("a shared-address mirror must survive the reap");
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

    fn with_body(label: &str, body: &str, stored_at_millis: u64) -> ChunkWithBody {
        ChunkWithBody {
            meta: crate::ports::types::ChunkMeta {
                addr: crate::ports::types::ChunkAddr::new(format!("addr-{label}")),
                label: label.to_string(),
                len: body.len(),
                stored_at_millis,
            },
            body: body.to_string(),
        }
    }

    /// Major #1 (issue #1488): the Brain list froze on the oldest 500 chunks
    /// because it capped an oldest-first enumeration with no sort — so past 500
    /// chunks, every newer memory was silently dropped. Prove the cap now keeps
    /// the NEWEST `MAX_CONTEXT_ENTRIES` and drops the stalest.
    #[test]
    fn the_cap_keeps_the_newest_chunks_not_the_oldest() {
        // More than the cap, handed in oldest-first as the backends list them:
        // stamp `i` for the i-th chunk, so higher stamp == newer.
        let total = MAX_CONTEXT_ENTRIES + 50;
        let input: Vec<ChunkWithBody> = (0..total)
            .map(|i| with_body(&format!("agent-1/m{i}"), &format!("memory {i}"), i as u64))
            .collect();

        let kept = newest_context_chunks(input);
        assert_eq!(kept.len(), MAX_CONTEXT_ENTRIES, "the page is capped");

        let kept_stamps: std::collections::HashSet<u64> =
            kept.iter().map(|c| c.stored_at_millis).collect();
        // The 50 oldest (stamps 0..50) must be the ones dropped.
        for old in 0..50u64 {
            assert!(
                !kept_stamps.contains(&old),
                "the stalest chunk (stamp {old}) must be dropped, not frozen in"
            );
        }
        // The newest chunk ever written must survive — the exact memory the old
        // unsorted `take` dropped once the store passed the cap.
        assert!(
            kept_stamps.contains(&((total - 1) as u64)),
            "the newest memory must be kept"
        );
    }

    /// An unstamped legacy chunk (`0`) sorts last, so it never displaces a
    /// real, freshly-stamped memory out of the capped page.
    #[test]
    fn unstamped_chunks_sort_behind_stamped_ones() {
        let mut input = vec![with_body("agent-1/legacy", "legacy", 0)];
        input.extend(
            (1..=MAX_CONTEXT_ENTRIES)
                .map(|i| with_body(&format!("agent-1/m{i}"), &format!("memory {i}"), i as u64)),
        );

        let kept = newest_context_chunks(input);
        assert_eq!(kept.len(), MAX_CONTEXT_ENTRIES);
        assert!(
            !kept.iter().any(|c| c.stored_at_millis == 0),
            "the unstamped chunk is evicted first, behind every stamped one"
        );
    }

    /// The mirror-drop happens before the cap, and the bodies enumerated up
    /// front reach the rendered rows without a second read.
    #[test]
    fn mirrors_drop_and_bodies_carry_through() {
        let kept = newest_context_chunks(vec![
            with_body("operator-fact/f1", "mirror body", 3),
            with_body("agent-1/note", "real memory body", 2),
        ]);
        assert_eq!(kept.len(), 1, "the operator-fact mirror is dropped");
        assert_eq!(kept[0].label, "agent-1/note");
        assert_eq!(
            kept[0].body, "real memory body",
            "the body carried by the enumeration renders without a re-read"
        );
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
        assert_eq!(entries[0].updated_at, 2_000);
        assert_eq!(entries[1].updated_at, 3_000);
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
