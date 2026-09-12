//! Turn-scoped collection of addressable files produced by agent tools.
//!
//! Workspace tools are built once per cached agent while a chat reply varies
//! per turn. A company-wide vector therefore cannot answer which reply wrote a
//! node: concurrent turns can interleave, and even sequential turns in one
//! cycle would let a forgotten drain cross-attribute the first turn's output
//! to the second. The task-local scope below gives every turn its own bucket on
//! one cheap shared handle.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ports::types::{ChatOutput, ChatOutputKind};
use crate::runtime::approval_display;

tokio::task_local! {
    /// The collector bucket owned by the agent turn executing in this task.
    static CURRENT_OUTPUT_SCOPE: u64;
}

/// A shared handle whose writes are partitioned by the current turn.
#[derive(Clone, Default)]
pub struct TurnOutputCollector {
    inner: Arc<Mutex<BTreeMap<u64, Vec<ChatOutput>>>>,
    next_scope: Arc<AtomicU64>,
}

impl TurnOutputCollector {
    /// Opens an isolated output bucket for one turn.
    #[must_use = "the claim removes its bucket on drop"]
    pub fn claim(&self) -> TurnOutputClaim {
        // Zero means "unscoped" in the overflow fallback below, so real
        // claims start at one.
        let scope = self
            .next_scope
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.clear(scope);
        TurnOutputClaim {
            collector: self.clone(),
            scope,
        }
    }

    /// Registers one successful workspace create/write against the active
    /// turn. Calls outside a claim are ignored: background and console writes
    /// must never leak onto the next chat reply.
    pub fn workspace_node(&self, node_id: impl Into<String>, title: &str) {
        self.push(ChatOutput {
            kind: ChatOutputKind::WorkspaceNode,
            target_id: node_id.into(),
            title: redacted_text("path", title),
            task_id: None,
            version: None,
        });
    }

    /// Registers one artifact revision recorded for the active chat turn.
    /// Calls from dispatched cards and other non-chat contexts are unscoped
    /// and intentionally ignored.
    pub fn artifact(
        &self,
        artifact_id: impl Into<String>,
        task_id: impl Into<String>,
        version: u32,
        title: &str,
    ) {
        self.push(ChatOutput {
            kind: ChatOutputKind::Artifact,
            target_id: artifact_id.into(),
            title: redacted_text("title", title),
            task_id: Some(task_id.into()),
            version: Some(version),
        });
    }

    fn push(&self, output: ChatOutput) {
        let Ok(scope) = CURRENT_OUTPUT_SCOPE.try_with(|scope| *scope) else {
            return;
        };
        let mut buckets = self.inner.lock().expect("turn output collector");
        let outputs = buckets.entry(scope).or_default();
        // Target identity is kind + id. Replacing in place retains stable
        // button order while keeping the final label/state from a repeated
        // write in the same turn.
        if let Some(existing) = outputs
            .iter_mut()
            .find(|item| item.kind == output.kind && item.target_id == output.target_id)
        {
            *existing = output;
        } else {
            outputs.push(output);
        }
    }

    fn take(&self, scope: u64) -> Vec<ChatOutput> {
        self.inner
            .lock()
            .expect("turn output collector")
            .remove(&scope)
            .unwrap_or_default()
    }

    fn clear(&self, scope: u64) {
        self.inner
            .lock()
            .expect("turn output collector")
            .remove(&scope);
    }
}

/// One turn's live claim on the shared collector.
pub struct TurnOutputClaim {
    collector: TurnOutputCollector,
    scope: u64,
}

impl TurnOutputClaim {
    /// Runs `future` with this claim as the ambient tool-write destination.
    pub async fn scoped<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T>,
    {
        CURRENT_OUTPUT_SCOPE.scope(self.scope, future).await
    }

    /// Drains only this turn's outputs.
    pub fn drain(&self) -> Vec<ChatOutput> {
        self.collector.take(self.scope)
    }
}

impl Drop for TurnOutputClaim {
    fn drop(&mut self) {
        self.collector.clear(self.scope);
    }
}

/// Applies the same redaction and string bound used by turn-step details.
pub fn redacted_text(key: &str, text: &str) -> String {
    approval_display::redact(&serde_json::json!({ (key): text }))
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(approval_display::UNRENDERABLE)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collects_create_and_write_and_dedupes_the_final_target() {
        let collector = TurnOutputCollector::default();
        let claim = collector.claim();
        claim
            .scoped(async {
                collector.workspace_node("n-1", "agents/writer/draft.md");
                collector.workspace_node("n-2", "agents/writer/other.md");
                collector.workspace_node("n-1", "agents/writer/final.md");
            })
            .await;

        let outputs = claim.drain();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].target_id, "n-1");
        assert_eq!(outputs[0].title, "agents/writer/final.md");
        assert_eq!(outputs[1].target_id, "n-2");
    }

    #[tokio::test]
    async fn two_turns_never_cross_attribute_outputs() {
        let collector = TurnOutputCollector::default();
        let first = collector.claim();
        let second = collector.claim();

        first
            .scoped(collector_write(&collector, "first", "first.md"))
            .await;
        second
            .scoped(collector_write(&collector, "second", "second.md"))
            .await;

        assert_eq!(ids(first.drain()), vec!["first"]);
        assert_eq!(ids(second.drain()), vec!["second"]);
    }

    #[tokio::test]
    async fn collects_the_exact_published_artifact_revision() {
        let collector = TurnOutputCollector::default();
        let claim = collector.claim();
        claim
            .scoped(async {
                collector.artifact("a-1", "t-1", 3, "Launch brief");
            })
            .await;

        let outputs = claim.drain();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, ChatOutputKind::Artifact);
        assert_eq!(outputs[0].target_id, "a-1");
        assert_eq!(outputs[0].task_id.as_deref(), Some("t-1"));
        assert_eq!(outputs[0].version, Some(3));
    }

    async fn collector_write(collector: &TurnOutputCollector, id: &str, title: &str) {
        collector.workspace_node(id, title);
    }

    fn ids(outputs: Vec<ChatOutput>) -> Vec<String> {
        outputs.into_iter().map(|output| output.target_id).collect()
    }

    #[tokio::test]
    async fn reads_and_deletes_register_nothing_without_a_write_call() {
        let collector = TurnOutputCollector::default();
        let claim = collector.claim();
        claim.scoped(async {}).await;
        assert!(claim.drain().is_empty());
    }
}
