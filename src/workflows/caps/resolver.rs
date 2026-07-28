//! The [`WorkflowResolver`] that backs `sub_workflow`-by-id for a company run.
//!
//! tinyflows is persistence-free: when a `sub_workflow` node references a child
//! by `workflow_id`, the engine asks the host to resolve that id to a runnable
//! [`WorkflowGraph`](tinyflows::model::WorkflowGraph). [`StoreWorkflowResolver`]
//! serves that from the company's on-disk source directory
//! (`companies/<name>/workflows/<id>.toml`), running the child through the SAME
//! full [`parse_workflow`](crate::company::parse_workflow) validation a
//! hand-authored or console-created workflow gets, then translating it.
//!
//! ## Cycle safety
//!
//! Two independent guards stop a `sub_workflow` chain from looping forever:
//!
//! * **This resolver's static guard** — before a child is loaded/translated, a
//!   bounded breadth-first scan walks the *static* `workflow_id` references in the
//!   store starting from the requested id. If the requested id itself, or the
//!   run's `root_id`, appears in that transitive closure, the chain would loop and
//!   the resolve is refused with a named error. This catches the common
//!   one-and-two-level cycles (A→B→A) eagerly, before any child runs.
//! * **The engine's depth backstop** — a *dynamic* id (an `=expr` resolved at run
//!   time) can't be scanned statically, so the engine's
//!   [`MAX_SUB_WORKFLOW_DEPTH`](tinyflows::engine::MAX_SUB_WORKFLOW_DEPTH) bound
//!   still terminates any cycle formed through expression-computed ids.
//!
//! The resolver is stateless per call — each `resolve` re-derives everything from
//! the source directory, so a workflow edited on disk between steps is picked up.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use async_trait::async_trait;
use tinyflows::caps::WorkflowResolver;
use tinyflows::error::{EngineError, Result as TfResult};
use tinyflows::model::WorkflowGraph;

use crate::company::{WorkflowFile, WorkflowNodeKind, load_company_workflows};

/// Hard bound on how many workflows the static cycle scan visits before giving
/// up. A store this deep is either pathological or adversarial; refusing to run
/// is safer than an unbounded walk.
const MAX_STATIC_RESOLVE_NODES: usize = 64;

/// A [`WorkflowResolver`] serving `sub_workflow`-by-id from a company's on-disk
/// `workflows/` directory, with a static transitive-closure cycle guard.
pub struct StoreWorkflowResolver {
    /// The company source directory (`companies/<name>`); children live under
    /// its `workflows/<id>.toml`.
    source_dir: PathBuf,
    /// The id of the top-level workflow the current run started from — a child
    /// whose closure reaches back to it would loop the whole run.
    root_id: String,
}

impl StoreWorkflowResolver {
    /// Builds a resolver serving children from `source_dir` for a run rooted at
    /// `root_id`.
    pub fn new(source_dir: PathBuf, root_id: String) -> Self {
        Self {
            source_dir,
            root_id,
        }
    }

    /// The **static** `workflow_id` references a graph makes — literal ids only.
    /// A dynamic `=expr` id is resolved by the engine at run time (and cannot be
    /// scanned statically), so it is skipped here and left to the engine's depth
    /// backstop.
    fn static_refs(file: &WorkflowFile) -> Vec<String> {
        file.nodes
            .iter()
            .filter(|n| n.kind == WorkflowNodeKind::SubWorkflow)
            .filter_map(|n| {
                n.config
                    .as_ref()
                    .and_then(|config| config.get("workflow_id"))
                    .and_then(|value| value.as_str())
                    .filter(|id| !id.starts_with('='))
                    .map(str::to_string)
            })
            .collect()
    }

    /// Rejects a cycle reachable by static references from `start_id`: if the
    /// requested id or the run's `root_id` appears in `start_id`'s transitive
    /// closure, the `sub_workflow` chain would loop. Bounded by a visited set and
    /// [`MAX_STATIC_RESOLVE_NODES`]; an unresolvable child is not a cycle (it
    /// fails loudly at its own resolve) so it is skipped here.
    fn guard_cycle(
        source_dir: PathBuf,
        root_id: String,
        start_id: String,
        start_file: WorkflowFile,
    ) -> TfResult<()> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        visited.insert(start_id.clone());
        let mut budget = 1usize;
        for referenced in Self::static_refs(&start_file) {
            if referenced == start_id || referenced == root_id {
                return Err(EngineError::Capability(format!(
                    "sub_workflow cycle detected: '{start_id}' references '{referenced}', which loops back into the running chain (root '{root_id}', resolving '{start_id}')"
                )));
            }
            queue.push_back(referenced);
        }

        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            budget += 1;
            if budget > MAX_STATIC_RESOLVE_NODES {
                return Err(EngineError::Capability(format!(
                    "sub_workflow chain from '{start_id}' spans more than {MAX_STATIC_RESOLVE_NODES} workflows; refusing to run"
                )));
            }
            // An unresolvable / invalid child is not itself a cycle — it will
            // fail loudly when the engine resolves it. Skip it in the scan.
            let Ok(mut loaded) =
                load_company_workflows(&source_dir, std::slice::from_ref(&current))
            else {
                continue;
            };
            let Some(file) = loaded.pop() else {
                continue;
            };
            for referenced in Self::static_refs(&file) {
                if referenced == start_id || referenced == root_id {
                    return Err(EngineError::Capability(format!(
                        "sub_workflow cycle detected: '{current}' references '{referenced}', which loops back into the running chain (root '{}', resolving '{start_id}')",
                        root_id
                    )));
                }
                queue.push_back(referenced);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl WorkflowResolver for StoreWorkflowResolver {
    async fn resolve(&self, workflow_id: &str) -> TfResult<WorkflowGraph> {
        // (a) The id becomes a path segment — reject anything that could escape
        // the `workflows/` directory before it touches the filesystem.
        if !is_safe_workflow_id(workflow_id) {
            return Err(EngineError::Capability(format!(
                "sub_workflow id '{workflow_id}' is not a valid workflow id"
            )));
        }

        // (b) Load the child, re-running full OpenCompany parse + validation on
        // it (the same rules a hand-authored or console-created graph passes).
        let id = workflow_id.to_string();
        let file = load_company_workflows(&self.source_dir, std::slice::from_ref(&id))
            .map_err(|err| EngineError::Capability(format!("sub_workflow '{workflow_id}': {err}")))?
            .into_iter()
            .next()
            .ok_or_else(|| {
                EngineError::Capability(format!(
                    "sub_workflow '{workflow_id}' is not a saved workflow on this company"
                ))
            })?;

        // (c) Static cycle guard over the store, before the child is handed back
        // to the engine to compile + run.
        let source_dir = self.source_dir.clone();
        let root_id = self.root_id.clone();
        let start_id = workflow_id.to_string();
        let start_file = file.clone();
        tokio::task::spawn_blocking(move || {
            Self::guard_cycle(source_dir, root_id, start_id, start_file)
        })
        .await
        .map_err(|err| {
            EngineError::Capability(format!(
                "sub_workflow '{workflow_id}' cycle scan failed: {err}"
            ))
        })??;

        // (d) Translate to a runnable tinyflows graph.
        Ok(crate::workflows::translate::translate(&file))
    }
}

/// Whether `id` is a single safe on-disk filename stem — no path separators, no
/// `..`, not empty — so it cannot escape the `workflows/` directory. Mirrors the
/// `safe_wid` check the REST workflow routes use.
fn is_safe_workflow_id(id: &str) -> bool {
    use std::path::{Component, Path};
    let mut comps = Path::new(id).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `src` to `<dir>/workflows/<id>.toml`.
    fn write_wf(dir: &std::path::Path, id: &str, src: &str) {
        let workflows = dir.join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join(format!("{id}.toml")), src).unwrap();
    }

    /// A minimal valid child graph body (trigger → output).
    fn leaf(id: &str) -> String {
        format!(
            r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "done"
kind = "output"
name = "Done"
[[edge]]
from = "start"
to = "done"
"#
        )
    }

    /// A graph that runs `child_id` as a sub_workflow.
    fn parent_of(id: &str, child_id: &str) -> String {
        format!(
            r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "{child_id}"
[[edge]]
from = "start"
to = "sub"
"#
        )
    }

    #[tokio::test]
    async fn resolves_a_saved_child_into_a_compilable_graph() {
        let dir = tempfile::tempdir().unwrap();
        write_wf(dir.path(), "child", &leaf("child"));
        let resolver = StoreWorkflowResolver::new(dir.path().to_path_buf(), "root".to_string());

        let graph = resolver.resolve("child").await.expect("resolves");
        assert_eq!(graph.id.as_deref(), Some("child"));
        // The resolved child is a graph the engine accepts.
        tinyflows::compiler::compile(&graph).expect("resolved child compiles");
    }

    #[tokio::test]
    async fn unknown_child_is_a_capability_error() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = StoreWorkflowResolver::new(dir.path().to_path_buf(), "root".into());
        let err = resolver.resolve("ghost").await.unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn traversal_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = StoreWorkflowResolver::new(dir.path().to_path_buf(), "root".into());
        let err = resolver.resolve("../secrets").await.unwrap_err();
        assert!(err.to_string().contains("not a valid workflow id"), "{err}");
    }

    /// A→A: the child directly references the run root, closing a one-level loop.
    #[tokio::test]
    async fn root_self_loop_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // `a` runs `b`; `b` runs `a` (the root). Resolving `b` (root = `a`) must
        // reject because `a` is in `b`'s closure.
        write_wf(dir.path(), "a", &parent_of("a", "b"));
        write_wf(dir.path(), "b", &parent_of("b", "a"));
        let resolver = StoreWorkflowResolver::new(dir.path().to_path_buf(), "a".into());
        let err = resolver.resolve("b").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle"), "{msg}");
        // The cycle message names the chain, not the depth backstop.
        assert!(
            !msg.contains("depth"),
            "should be the static cycle msg: {msg}"
        );
    }

    /// A→B→A: two on-disk workflows referencing each other hard-reject at the
    /// first resolve of the second.
    #[tokio::test]
    async fn mutual_reference_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_wf(dir.path(), "flow_a", &parent_of("flow_a", "flow_b"));
        write_wf(dir.path(), "flow_b", &parent_of("flow_b", "flow_a"));
        // Root is flow_a; the engine resolves flow_b first.
        let resolver = StoreWorkflowResolver::new(dir.path().to_path_buf(), "flow_a".into());
        let err = resolver.resolve("flow_b").await.unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    /// A diamond (root → B and → C, both → D) is NOT a cycle: D is reached twice
    /// but never loops back, so every resolve succeeds.
    #[tokio::test]
    async fn diamond_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        write_wf(dir.path(), "b", &parent_of("b", "d"));
        write_wf(dir.path(), "c", &parent_of("c", "d"));
        write_wf(dir.path(), "d", &leaf("d"));
        let resolver = StoreWorkflowResolver::new(dir.path().to_path_buf(), "root".into());
        resolver.resolve("b").await.expect("b resolves");
        resolver.resolve("c").await.expect("c resolves");
        resolver.resolve("d").await.expect("d resolves");
    }
}
