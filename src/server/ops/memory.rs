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
//! [`ContextStore`] is append-only (no delete port), so deleting a fact removes
//! it from the `FactStore` and the console but its mirrored chunk stays
//! agent-recallable until a delete port lands. The `operator-fact/{id}` label
//! makes that future reap trivial. We deliberately do NOT fork the retrieval
//! path to work around this — the seam is documented, not hidden.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::facts::{FactKind, FactRecord};
use crate::ports::types::{CompanyEvent, ContextChunk};
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

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
                let who = chunk.label.split('/').next().filter(|s| !s.is_empty());
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
            // Context chunks carry no timestamp; the console renders `—`.
            updated_at: 0,
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
        // `""` lists every chunk; drop the operator-fact mirrors before peeking
        // so we neither double-list them nor pay to read their bodies, then cap
        // the reads so a huge context store can't unbound this request.
        let mirror_prefix = format!("{OPERATOR_FACT_PREFIX}/");
        let metas = company.runtime.context.list(company.id(), "").await?;
        let mut chunks = Vec::new();
        for meta in metas
            .into_iter()
            .filter(|m| !m.label.starts_with(&mirror_prefix))
            .take(MAX_CONTEXT_ENTRIES)
        {
            // A peek failure degrades one row to an empty body rather than
            // failing the whole list, but log it so a real storage fault
            // surfaces instead of silently rendering blank cards.
            let body = match company
                .runtime
                .context
                .peek(company.id(), &meta.addr, None)
                .await
            {
                Ok(body) => body,
                Err(err) => {
                    tracing::warn!(
                        company = %company.id(),
                        addr = %meta.addr,
                        error = %err,
                        "failed to peek context chunk; rendering empty body"
                    );
                    String::new()
                }
            };
            chunks.push(RawChunk {
                addr: meta.addr.to_string(),
                label: meta.label,
                body,
            });
        }
        entries.extend(context_entries(chunks, query.as_deref()));
    }

    Ok(Json(entries))
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
    let agent_chunks = company.runtime.context.list(company.id(), "").await?.len();
    let task_outcomes = company
        .runtime
        .context
        .list(company.id(), OUTCOME_LABEL_PREFIX)
        .await?
        .len();
    Ok(Json(MemoryStats {
        facts: facts.len(),
        facts_updated_at_millis,
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
        // Journal the operator deletion to the event log (audit trail). Note the
        // mirrored `operator-fact/{fact_id}` chunk is NOT removed here: the
        // ContextStore has no delete port, so it stays agent-recallable until
        // one lands (documented seam, module doc).
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

#[cfg(test)]
mod combined_list_tests {
    use super::*;

    fn chunk(label: &str, body: &str) -> RawChunk {
        RawChunk {
            addr: format!("addr-{label}"),
            label: label.to_string(),
            body: body.to_string(),
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
