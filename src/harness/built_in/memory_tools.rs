//! Deliberate agent memory: `memory_store` / `memory_recall` / `memory_forget`
//! (issue #1113, the G11 half).
//!
//! Until now the only thing persisting agent memory was the automatic
//! [`memory_loop`](super::memory_loop) — retrieve → inject → store around every
//! turn, unsteerable from inside the turn. These three tools add the
//! deliberate half: an agent that just learned the customer's fiscal year can
//! *choose* to keep it, find it again next week, and discard it when it stops
//! being true.
//!
//! # Why these are oc-authored rather than the vendored upstream tools
//!
//! Upstream's `MemoryStoreTool` / `MemoryRecallTool` resolve their store
//! through an ambient `CoreContext` (or a process-global fallback) that this
//! crate's multi-tenant-in-one-process model never scopes — wiring them would
//! be a cross-company memory leak, which is exactly why `build.rs` withheld
//! them (see `memory_tools`'s history there). These implementations follow the
//! `workspace_tools` pattern instead: the company and agent are **fields
//! captured at build time**, the port is the company's own [`ContextStore`],
//! and `execute()` has no ambient anything to reach. Upstream's schemas also
//! take `namespace` as a model-supplied parameter — handing the model the
//! tenant boundary as a free-text field. Here the namespace is derived from
//! the captured identity, always.
//!
//! # Where the memories live
//!
//! `ContextStore` rows labelled `agent-memory/<agent id>/<slug>` — the same
//! store and label family the automatic loop and the Brain console already
//! read, so a deliberate memory surfaces in ambient recall and in the
//! operator's Brain view like any other context row, and travels with the
//! export bundle. Writes are `Internal` taint by authorship (the agent's own
//! conclusion — same precedent as operator facts), which is why they go
//! through `deps.context`, not the inbound port.
//!
//! # What forget may touch
//!
//! Only rows under this agent's own `agent-memory/<agent id>/` prefix. Recall
//! surfaces everything — task outcomes, operator-fact mirrors, other agents'
//! memories — but delete is scoped to what this agent deliberately stored:
//! task outcomes are the loop's record, and operator facts are the operator's.
//! The guard is a list-membership check against the agent's own prefix, and
//! the delete itself is **label-scoped** (`ContextStore::delete_label`, issue
//! #1300): it removes exactly this agent's own claims, so a hallucinated or
//! copied address cannot reach anyone else's rows even when byte-identical
//! content put their label on the same content address — the other labels
//! keep the body, which is reaped only with its last claim.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core::openhuman as oh;

use crate::ports::ContextStore;
use crate::ports::types::{ChunkAddr, CompanyId, ContextChunk};

/// Tool names — must stay in lockstep with `policy/consequence.rs`,
/// `policy/judgement.rs` and `confine.rs`, none of which the compiler checks.
pub const MEMORY_STORE_TOOL: &str = "memory_store";
pub const MEMORY_RECALL_TOOL: &str = "memory_recall";
pub const MEMORY_FORGET_TOOL: &str = "memory_forget";

/// The label family deliberate memories live under; `<prefix>/<agent>/<slug>`.
pub const AGENT_MEMORY_LABEL_PREFIX: &str = "agent-memory";

/// Bodies above this are refused rather than truncated: a memory the agent
/// cannot read back whole is a memory that silently lies to it later.
const MAX_BODY_BYTES: usize = 16 * 1024;
/// Titles are labels; labels are index keys everywhere. Kept short.
const MAX_TITLE_BYTES: usize = 200;
/// Recall result-count ceiling (and its default).
const MAX_RECALL_LIMIT: usize = 10;
const DEFAULT_RECALL_LIMIT: usize = 5;

/// One company's, one agent's, deliberate-memory surface: the captured
/// identity every tool call is scoped by. The `workspace_tools` shape — no
/// ambient state, the port and ids are fields.
#[derive(Clone)]
struct AgentMemory {
    context: Arc<dyn ContextStore>,
    company: CompanyId,
    agent_id: String,
}

impl AgentMemory {
    /// This agent's own label prefix, with the trailing slash that makes it a
    /// namespace rather than a string prefix (`agent-memory/ann/` must not
    /// cover `agent-memory/anna/`).
    fn own_prefix(&self) -> String {
        format!("{AGENT_MEMORY_LABEL_PREFIX}/{}/", self.agent_id)
    }
}

/// Builds the three deliberate-memory tools for one agent of one company.
pub fn memory_tools(
    context: Arc<dyn ContextStore>,
    company: CompanyId,
    agent_id: String,
) -> Vec<Box<dyn Tool>> {
    let mem = AgentMemory {
        context,
        company,
        agent_id,
    };
    vec![
        Box::new(MemoryStoreTool { mem: mem.clone() }),
        Box::new(MemoryRecallTool { mem: mem.clone() }),
        Box::new(MemoryForgetTool { mem }),
    ]
}

/// Lowercases, keeps `[a-z0-9]` runs joined by `-`, bounds length. The slug is
/// only a label segment — uniqueness comes from the store's content address —
/// so collisions here are harmless (two memories may share a slug).
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "note".to_string()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// memory_store
// ---------------------------------------------------------------------------

struct MemoryStoreTool {
    mem: AgentMemory,
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        MEMORY_STORE_TOOL
    }

    fn description(&self) -> &str {
        "Deliberately remember one thing for future turns — a fact you learned, a decision and \
         its reason, a preference someone stated. It becomes part of this company's durable \
         memory: retrievable with `memory_recall`, visible to the operator in the Brain view, \
         and automatically surfaced to future turns when relevant. USE FOR conclusions worth \
         keeping. NOT for scratch working-out (use `file_write`) and NOT for secrets or \
         credentials — memory is not a vault."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short name for the memory (a few words)."
                },
                "body": {
                    "type": "string",
                    "description": "The thing to remember, self-contained — a future turn sees this text without today's conversation around it."
                }
            },
            "required": ["title", "body"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        // Writes durable company state. The upstream tools' unfixed ReadOnly
        // default is exactly the trap mod.rs warns about; declared explicitly.
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let body = args
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if title.is_empty() || body.is_empty() {
            return Ok(ToolResult::error(
                "Both `title` and `body` are required and must be non-empty.".to_string(),
            ));
        }
        if title.len() > MAX_TITLE_BYTES {
            return Ok(ToolResult::error(format!(
                "`title` is {} bytes; the cap is {MAX_TITLE_BYTES}. Give it a short name and put \
                 the detail in `body`.",
                title.len()
            )));
        }
        if body.len() > MAX_BODY_BYTES {
            return Ok(ToolResult::error(format!(
                "`body` is {} bytes; the cap is {MAX_BODY_BYTES}. A memory should be a \
                 conclusion, not a document — store the document with `file_write` and remember \
                 where it is.",
                body.len()
            )));
        }
        // Redact the title once, here, so the label, the stored body and the
        // success echo all carry the redacted form — a model-supplied title
        // like "Bearer sk-longsecret" must not persist verbatim in any of them.
        let title = super::memory::redact_secrets(title);
        let chunk = ContextChunk {
            label: format!("{}{}", self.mem.own_prefix(), slug(&title)),
            // Title on the first line so the body is self-describing wherever
            // it surfaces (recall snippet, Brain view, ambient injection).
            // Title and body pass through the same secret redaction as every
            // other memory write, so an agent that stores a credential in
            // either field does not persist it.
            body: format!("{title}\n\n{}", super::memory::redact_secrets(body)),
        };
        let addr = self.mem.context.put(&self.mem.company, chunk).await?;
        Ok(ToolResult::success(format!(
            "Remembered as `{title}` (addr {}). It will surface in future turns when relevant; \
             `memory_forget` with that addr discards it.",
            addr.as_ref()
        )))
    }
}

// ---------------------------------------------------------------------------
// memory_recall
// ---------------------------------------------------------------------------

struct MemoryRecallTool {
    mem: AgentMemory,
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        MEMORY_RECALL_TOOL
    }

    fn description(&self) -> &str {
        "Search this company's durable memory on purpose — deliberate memories, past task \
         outcomes, operator-curated facts. USE WHEN the answer might already be known from \
         earlier work: before re-deriving a decision, re-asking a preference, or redoing \
         research. Relevant memory is also injected automatically each turn; this tool is for \
         asking a *specific* question of it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look for, phrased as the fact you hope exists."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 5, cap 10)."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return Ok(ToolResult::error("`query` is required.".to_string()));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l as usize)
            .unwrap_or(DEFAULT_RECALL_LIMIT)
            .clamp(1, MAX_RECALL_LIMIT);
        let hits = self
            .mem
            .context
            .search(&self.mem.company, query, limit)
            .await?;
        if hits.is_empty() {
            return Ok(ToolResult::success(format!(
                "Nothing in memory matches \"{query}\". It may simply never have been stored — \
                 absence of memory is not evidence, it is absence."
            )));
        }
        let mut out = format!("{} match(es):\n", hits.len());
        for hit in &hits {
            out.push_str(&format!(
                "- [{}] {}\n",
                hit.addr.as_ref(),
                hit.snippet.replace('\n', " ")
            ));
        }
        out.push_str(
            "Addresses work with `memory_forget` (your own memories only) and are stable across \
             turns.",
        );
        Ok(ToolResult::success(out))
    }
}

// ---------------------------------------------------------------------------
// memory_forget
// ---------------------------------------------------------------------------

struct MemoryForgetTool {
    mem: AgentMemory,
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        MEMORY_FORGET_TOOL
    }

    fn description(&self) -> &str {
        "Discard one memory YOU deliberately stored with `memory_store`, by the addr \
         `memory_recall` reported — for something that stopped being true or was stored in \
         error. Cannot touch task outcomes, operator facts, or other agents' memories; if \
         someone else stored identical text, only your own copy is forgotten. Forgetting an \
         already-forgotten addr is a no-op, not an error."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "addr": {
                    "type": "string",
                    "description": "The memory's address, exactly as memory_recall or memory_store reported it."
                }
            },
            "required": ["addr"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let addr = args
            .get("addr")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if addr.is_empty() {
            return Ok(ToolResult::error("`addr` is required.".to_string()));
        }
        // The scope guard: the addr must carry a label under THIS agent's own
        // deliberate-memory prefix. Checked against the store's own index at
        // call time — not against anything the model asserted — so a copied or
        // hallucinated address outside the prefix is refused, never deleted.
        //
        // The delete below is label-scoped (`ContextStore::delete_label`,
        // issue #1300): it removes exactly this agent's own claims on the
        // address, so a shared address — another agent's byte-identical
        // memory, a task outcome with the same text — keeps every other row,
        // and the body is reaped only with its last claim, atomically inside
        // the port. That closes the two holes the old address-level delete
        // had: a shared address no longer forces a refusal (which let anyone
        // make an agent's memory un-forgettable by storing identical text),
        // and the list below is advisory rather than a racy snapshot guard (a
        // concurrent identical-content write can no longer lose its row).
        let all = self.mem.context.list(&self.mem.company, "").await?;
        let own_prefix = self.mem.own_prefix();
        let at_addr: Vec<&str> = all
            .iter()
            .filter(|m| m.addr.as_ref() == addr)
            .map(|m| m.label.as_str())
            .collect();
        // Nothing at the address at all: the promised no-op. Either the
        // memory was already forgotten (a retry, or a stale recall listing)
        // or the addr never existed — in both cases there is nothing to
        // delete and nothing to protect, so failing would only punish
        // idempotent retries. The ownership refusal below is for addresses
        // that DO exist and are not this agent's.
        if at_addr.is_empty() {
            return Ok(ToolResult::success(format!(
                "`{addr}` was already gone; nothing to forget."
            )));
        }
        let own: Vec<String> = at_addr
            .iter()
            .filter(|label| label.starts_with(&own_prefix))
            .map(|label| label.to_string())
            .collect();
        if own.is_empty() {
            return Ok(ToolResult::error(format!(
                "`{addr}` is not one of your own stored memories, so it cannot be forgotten from \
                 here. Task outcomes are the turn loop's record, and operator facts belong to \
                 the operator (Brain view). Use `memory_recall` to see addresses; yours are the \
                 ones under `{own_prefix}`."
            )));
        }
        let shared = at_addr.iter().any(|label| !label.starts_with(&own_prefix));
        let chunk_addr = ChunkAddr::new(addr.to_string());
        let mut removed = false;
        for label in &own {
            removed |= self
                .mem
                .context
                .delete_label(&self.mem.company, &chunk_addr, label)
                .await?;
        }
        Ok(ToolResult::success(if !removed {
            format!("`{addr}` was already gone; nothing to forget.")
        } else if shared {
            format!(
                "Forgotten: `{addr}`. Your memory is gone. The same text is also stored under \
                 other labels (a task record or another agent's memory), which keep their own \
                 copies — recall may still surface it from those rows."
            )
        } else {
            format!("Forgotten: `{addr}`. It will no longer surface in recall.")
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::store::FsContextStore;

    fn ctx(dir: &std::path::Path) -> Arc<dyn ContextStore> {
        Arc::new(FsContextStore::new(dir.to_path_buf()))
    }

    fn tools_for(
        dir: &std::path::Path,
        company: &str,
        agent: &str,
    ) -> (Box<dyn Tool>, Box<dyn Tool>, Box<dyn Tool>) {
        let mut v = memory_tools(ctx(dir), CompanyId::new(company), agent.to_string());
        let forget = v.pop().unwrap();
        let recall = v.pop().unwrap();
        let store = v.pop().unwrap();
        (store, recall, forget)
    }

    fn addr_from(reply: &str) -> String {
        // "... (addr <a>) ..."
        let i = reply.find("(addr ").unwrap() + 6;
        let j = reply[i..].find(')').unwrap();
        reply[i..i + j].to_string()
    }

    #[tokio::test]
    async fn store_recall_forget_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (store, recall, forget) = tools_for(dir.path(), "acme", "ceo");

        let stored = store
            .execute(
                json!({"title": "Fiscal year", "body": "Acme's fiscal year starts in February."}),
            )
            .await
            .unwrap();
        assert!(!stored.is_error, "{stored:?}");
        let addr = addr_from(&stored.text());

        let found = recall.execute(json!({"query": "fiscal"})).await.unwrap();
        assert!(
            found.text().contains(&addr),
            "recall must surface the stored addr"
        );
        assert!(found.text().contains("February"));

        let gone = forget.execute(json!({"addr": addr.clone()})).await.unwrap();
        assert!(!gone.is_error);
        assert!(gone.text().contains("Forgotten"));

        // Idempotent, as the tool description promises: nothing lives at the
        // address any more, so a repeated forget (a retry, a stale recall
        // listing) succeeds as a no-op instead of scolding the agent.
        let again = forget.execute(json!({"addr": addr})).await.unwrap();
        assert!(
            !again.is_error,
            "already-forgotten must be a no-op: {again:?}"
        );
        assert!(again.text().contains("already gone"));
    }

    #[tokio::test]
    async fn memory_store_redacts_credentials_in_the_title() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let context = ctx(dir.path());
        let (store, _, _) = tools_for(dir.path(), "acme", "ceo");

        // A credential-shaped title must not persist verbatim anywhere: not in
        // the stored body, not in the label, not in the success echo.
        let stored = store
            .execute(json!({"title": "Bearer sk-longsecret", "body": "the api key for staging"}))
            .await
            .unwrap();
        assert!(!stored.is_error, "{stored:?}");
        assert!(
            stored.text().contains("[REDACTED]"),
            "success echo must carry the redacted title: {}",
            stored.text()
        );
        assert!(
            !stored.text().contains("sk-longsecret"),
            "{}",
            stored.text()
        );

        let addr = addr_from(&stored.text());
        let peeked = context
            .peek(&company, &ChunkAddr::new(addr), None)
            .await
            .unwrap();
        assert!(
            peeked.contains("Bearer [REDACTED]"),
            "stored title must be redacted: {peeked}"
        );
        assert!(!peeked.contains("sk-longsecret"), "{peeked}");
    }

    #[tokio::test]
    async fn forget_cannot_touch_other_rows() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let context = ctx(dir.path());

        // A task-outcome row (the loop's) and another agent's memory.
        let outcome = context
            .put(
                &company,
                ContextChunk {
                    label: "task-outcome/ceo".into(),
                    body: "Task: x\nOutcome: y".into(),
                },
            )
            .await
            .unwrap();
        let theirs = context
            .put(
                &company,
                ContextChunk {
                    label: format!("{AGENT_MEMORY_LABEL_PREFIX}/researcher/their-note"),
                    body: "Their note\n\nbody".into(),
                },
            )
            .await
            .unwrap();

        let (_, _, forget) = tools_for(dir.path(), "acme", "ceo");
        for addr in [&outcome, &theirs] {
            let refused = forget
                .execute(json!({"addr": addr.as_ref()}))
                .await
                .unwrap();
            assert!(refused.is_error, "must refuse {addr:?}");
            // And the row must still be there.
            context.peek(&company, addr, None).await.unwrap();
        }
    }

    /// Content addressing means byte-identical bodies share ONE address. A
    /// forget of a shared address removes exactly the caller's own claim
    /// (label-scoped delete, #1300): the other agent's row and the body both
    /// survive, and the caller is told the text lives on under other labels.
    /// This replaces the old refusal, which let any agent make another's
    /// memory permanently un-forgettable by storing identical text.
    #[tokio::test]
    async fn forget_of_a_shared_address_removes_only_the_callers_claim() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let context = ctx(dir.path());

        let (ceo_store, _, ceo_forget) = tools_for(dir.path(), "acme", "ceo");
        let (them_store, _, _) = tools_for(dir.path(), "acme", "researcher");
        let mine = ceo_store
            .execute(json!({"title": "Fiscal year", "body": "Starts in February."}))
            .await
            .unwrap();
        let addr = addr_from(&mine.text());
        them_store
            .execute(json!({"title": "Fiscal year", "body": "Starts in February."}))
            .await
            .unwrap();

        let forgotten = ceo_forget
            .execute(json!({"addr": addr.clone()}))
            .await
            .unwrap();
        assert!(
            !forgotten.is_error,
            "a shared address must forget the caller's own claim: {forgotten:?}"
        );
        assert!(
            forgotten.text().contains("other labels"),
            "the reply must say the text lives on elsewhere: {forgotten:?}"
        );
        // Exactly the researcher's row remains, and the body with it.
        let rows = context
            .list(&company, AGENT_MEMORY_LABEL_PREFIX)
            .await
            .unwrap();
        let labels: Vec<&str> = rows
            .iter()
            .filter(|m| m.addr.as_ref() == addr)
            .map(|m| m.label.as_str())
            .collect();
        assert_eq!(labels.len(), 1, "only the ceo's claim may go: {labels:?}");
        assert!(
            labels[0].starts_with("agent-memory/researcher/"),
            "{labels:?}"
        );
        context
            .peek(&company, &ChunkAddr::new(addr), None)
            .await
            .expect("the body must survive under the researcher's claim");
    }

    /// The regression lock the #1290 review found missing: deleting the
    /// trailing slash from own_prefix() — the exact #936 Namespace class —
    /// survived every existing test. `ann` and `anna` are both legal
    /// snake_case agent ids; ann's guard must neither see nor delete anna's
    /// rows.
    #[tokio::test]
    async fn the_prefix_boundary_is_a_namespace_not_a_string_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let company = CompanyId::new("acme");
        let context = ctx(dir.path());
        let (anna_store, _, _) = tools_for(dir.path(), "acme", "anna");
        let stored = anna_store
            .execute(json!({"title": "Annas note", "body": "hers alone"}))
            .await
            .unwrap();
        let addr = addr_from(&stored.text());

        let (_, _, ann_forget) = tools_for(dir.path(), "acme", "ann");
        let refused = ann_forget
            .execute(json!({"addr": addr.clone()}))
            .await
            .unwrap();
        assert!(
            refused.is_error,
            "ann must not reach agent-memory/anna/ rows: {refused:?}"
        );
        context
            .peek(&company, &crate::ports::types::ChunkAddr::new(addr), None)
            .await
            .expect("anna's row must survive ann's attempt");
    }

    #[tokio::test]
    async fn tools_are_company_isolated() {
        // Two companies over one store root: what alpha stores, beta's tools
        // can neither recall nor forget — the CompanyId captured at build time
        // is the entire boundary, exactly as the port contract promises.
        let dir = tempfile::tempdir().unwrap();
        let (a_store, _, _) = tools_for(dir.path(), "alpha", "ceo");
        let (_, b_recall, b_forget) = tools_for(dir.path(), "beta", "ceo");

        let stored = a_store
            .execute(json!({"title": "Alpha secret plan", "body": "The plan is zig."}))
            .await
            .unwrap();
        let addr = addr_from(&stored.text());

        let seen = b_recall.execute(json!({"query": "zig"})).await.unwrap();
        // The no-match reply echoes the query word, so assert on what must be
        // absent: alpha's addr and alpha's body text ("The plan is zig." would
        // surface as a snippet), plus the no-match marker being present.
        assert!(
            !seen.text().contains(&addr) && !seen.text().contains("The plan is"),
            "beta recalled alpha's memory: {}",
            seen.text()
        );
        assert!(seen.text().contains("Nothing in memory matches"));
        // Beta's forget of alpha's addr: within beta's visibility nothing
        // lives at that address, so the answer is the idempotent no-op — the
        // same answer a never-existed addr gets, so the response is no
        // cross-company existence oracle. What MUST hold is that alpha's row
        // survives untouched.
        let noop = b_forget
            .execute(json!({"addr": addr.clone()}))
            .await
            .unwrap();
        assert!(
            !noop.is_error,
            "cross-company forget answers the no-op: {noop:?}"
        );
        assert!(noop.text().contains("already gone"));
        let (_, a_recall, _) = tools_for(dir.path(), "alpha", "ceo");
        let still = a_recall.execute(json!({"query": "zig"})).await.unwrap();
        assert!(
            still.text().contains(&addr),
            "alpha's memory must survive beta's forget: {}",
            still.text()
        );
    }

    #[tokio::test]
    async fn slug_is_bounded_and_never_empty() {
        assert_eq!(slug("Fiscal Year!!"), "fiscal-year");
        assert_eq!(slug("///"), "note");
        assert!(slug(&"x".repeat(500)).len() <= 64);
    }
}
