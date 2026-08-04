//! The [`WorkflowResolver`] that backs `sub_workflow`-by-id for a company run.
//!
//! tinyflows is persistence-free: when a `sub_workflow` node references a child
//! by `workflow_id`, the engine asks the host to resolve that id to a runnable
//! [`WorkflowGraph`](tinyflows::model::WorkflowGraph). [`StoreWorkflowResolver`]
//! serves that from the union of the company's two graph sources — the seed
//! files (`companies/<name>/workflows/<id>.toml`) and the runtime-authored
//! bodies on the [`CompanyRecord`](crate::ports::types::CompanyRecord) overlay —
//! running the child through the SAME full
//! [`parse_workflow`](crate::company::parse_workflow) validation a
//! hand-authored or console-created workflow gets, then translating it.
//!
//! Reading the overlay matters for more than availability: a hosted tenant's
//! children live *only* there, so a resolver that saw the seed side alone would
//! both fail to resolve them and — worse — miss them in the cycle scan below.
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
//! The resolver is stateless per call — each `resolve` re-loads the record and
//! re-reads the source directory, so a workflow edited between steps is picked
//! up.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tinyflows::caps::WorkflowResolver;
use tinyflows::error::{EngineError, Result as TfResult};
use tinyflows::model::WorkflowGraph;

use crate::company::{WorkflowFile, WorkflowNodeKind, load_workflow_union};
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, OverlayWorkflow};

/// Hard bound on how many workflows the static cycle scan visits before giving
/// up. A store this deep is either pathological or adversarial; refusing to run
/// is safer than an unbounded walk.
const MAX_STATIC_RESOLVE_NODES: usize = 64;

/// A [`WorkflowResolver`] serving `sub_workflow`-by-id from a company's on-disk
/// `workflows/` directory, with a static transitive-closure cycle guard.
pub struct StoreWorkflowResolver {
    /// The company source directory (`companies/<name>`); seed children live
    /// under its `workflows/<id>.toml`. `None` on a hosted tenant, whose
    /// children are all overlay bodies.
    source_dir: Option<PathBuf>,
    /// The company store, read per resolve for the runtime-authored graph
    /// bodies (the overlay half of the union).
    store: Arc<dyn CompanyStore>,
    /// The company whose record carries those bodies.
    company: CompanyId,
    /// The id of the top-level workflow the current run started from — a child
    /// whose closure reaches back to it would loop the whole run.
    root_id: String,
}

impl StoreWorkflowResolver {
    /// Builds a resolver serving children from `source_dir` ∪ `company`'s
    /// overlay bodies, for a run rooted at `root_id`.
    pub fn new(
        source_dir: Option<PathBuf>,
        store: Arc<dyn CompanyStore>,
        company: CompanyId,
        root_id: String,
    ) -> Self {
        Self {
            source_dir,
            store,
            company,
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
        source_dir: Option<PathBuf>,
        overlays: Vec<OverlayWorkflow>,
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
            let Ok(Some(file)) = load_workflow_union(source_dir.as_deref(), &overlays, &current)
            else {
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

        // (b) The company's runtime-authored bodies, read once and reused by the
        // load and the cycle scan below so both see the same snapshot.
        let overlays = self
            .store
            .load(&self.company)
            .await
            .map_err(|err| {
                EngineError::Capability(format!(
                    "sub_workflow '{workflow_id}': could not read saved workflows: {err}"
                ))
            })?
            .map(|record| record.overlay_workflows)
            .unwrap_or_default();

        // (c) Load the child from the seed ∪ overlay union, re-running full
        // OpenCompany parse + validation on it (the same rules a hand-authored
        // or console-created graph passes).
        let file = load_workflow_union(self.source_dir.as_deref(), &overlays, workflow_id)
            .map_err(|err| EngineError::Capability(format!("sub_workflow '{workflow_id}': {err}")))?
            .ok_or_else(|| {
                EngineError::Capability(format!(
                    "sub_workflow '{workflow_id}' is not a saved workflow on this company"
                ))
            })?;

        // (d) Static cycle guard over the same union, before the child is handed
        // back to the engine to compile + run.
        let source_dir = self.source_dir.clone();
        let root_id = self.root_id.clone();
        let start_id = workflow_id.to_string();
        let start_file = file.clone();
        tokio::task::spawn_blocking(move || {
            Self::guard_cycle(source_dir, overlays, root_id, start_id, start_file)
        })
        .await
        .map_err(|err| {
            EngineError::Capability(format!(
                "sub_workflow '{workflow_id}' cycle scan failed: {err}"
            ))
        })??;

        // (e) Translate to a runnable tinyflows graph.
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

    use crate::company::CompanyManifest;
    use crate::error::Result as OcResult;
    use crate::ports::types::{CompanyRecord, CompanySummary, LedgerEntry};

    /// An in-memory `CompanyStore` holding one record, so the resolver's overlay
    /// half can be seeded without a real backend.
    struct MemStore(std::sync::Mutex<Option<CompanyRecord>>);

    #[async_trait]
    impl CompanyStore for MemStore {
        async fn load(&self, _id: &CompanyId) -> OcResult<Option<CompanyRecord>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn save(&self, record: &CompanyRecord) -> OcResult<()> {
            *self.0.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        async fn list(&self) -> OcResult<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> OcResult<()> {
            Ok(())
        }
    }

    /// A store whose record carries `overlays` as its runtime-authored graphs.
    fn store_with(overlays: Vec<OverlayWorkflow>) -> Arc<dyn CompanyStore> {
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n").expect("valid manifest");
        Arc::new(MemStore(std::sync::Mutex::new(Some(CompanyRecord {
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: overlays,
            template_provenance: None,
        }))))
    }

    /// A resolver over a seed directory only (no runtime-authored graphs).
    fn seed_resolver(dir: &std::path::Path, root_id: &str) -> StoreWorkflowResolver {
        StoreWorkflowResolver::new(
            Some(dir.to_path_buf()),
            store_with(Vec::new()),
            CompanyId::new("acme"),
            root_id.to_string(),
        )
    }

    /// A resolver with NO seed directory, serving only overlay bodies — the
    /// hosted shape (issue #168).
    fn overlay_resolver(overlays: Vec<OverlayWorkflow>, root_id: &str) -> StoreWorkflowResolver {
        StoreWorkflowResolver::new(
            None,
            store_with(overlays),
            CompanyId::new("acme"),
            root_id.to_string(),
        )
    }

    fn overlay(id: &str, toml: String) -> OverlayWorkflow {
        OverlayWorkflow {
            id: id.to_string(),
            toml,
        }
    }

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
        let resolver = seed_resolver(dir.path(), "root");

        let graph = resolver.resolve("child").await.expect("resolves");
        assert_eq!(graph.id.as_deref(), Some("child"));
        // The resolved child is a graph the engine accepts.
        tinyflows::compiler::compile(&graph).expect("resolved child compiles");
    }

    #[tokio::test]
    async fn unknown_child_is_a_capability_error() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = seed_resolver(dir.path(), "root");
        let err = resolver.resolve("ghost").await.unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[tokio::test]
    async fn traversal_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = seed_resolver(dir.path(), "root");
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
        let resolver = seed_resolver(dir.path(), "a");
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
        let resolver = seed_resolver(dir.path(), "flow_a");
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
        let resolver = seed_resolver(dir.path(), "root");
        resolver.resolve("b").await.expect("b resolves");
        resolver.resolve("c").await.expect("c resolves");
        resolver.resolve("d").await.expect("d resolves");
    }

    // --- #168: overlay-only (hosted) children --------------------------------

    /// A `sub_workflow` child that exists ONLY as a runtime-authored body — the
    /// hosted case — resolves into a compilable graph.
    #[tokio::test]
    async fn resolves_an_overlay_child_with_no_source_dir() {
        let resolver = overlay_resolver(vec![overlay("child", leaf("child"))], "root");
        let graph = resolver.resolve("child").await.expect("resolves");
        assert_eq!(graph.id.as_deref(), Some("child"));
        tinyflows::compiler::compile(&graph).expect("resolved overlay child compiles");
    }

    /// The static cycle scan must walk overlay children too — otherwise a cycle
    /// formed entirely from console-created workflows would only be caught by the
    /// engine's depth backstop, deep into the run.
    #[tokio::test]
    async fn cycle_through_overlay_children_is_rejected() {
        let resolver = overlay_resolver(
            vec![
                overlay("flow_a", parent_of("flow_a", "flow_b")),
                overlay("flow_b", parent_of("flow_b", "flow_a")),
            ],
            "flow_a",
        );
        let err = resolver.resolve("flow_b").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cycle"), "{msg}");
        assert!(
            !msg.contains("depth"),
            "should be the static cycle msg: {msg}"
        );
    }

    /// A seed file and an overlay body sharing one id: the seed wins, matching
    /// `load_workflow_union`'s documented precedence.
    #[tokio::test]
    async fn a_seed_file_shadows_an_overlay_of_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        write_wf(dir.path(), "child", &leaf("child"));
        // The overlay body of the same id is a *parent* graph — if it won, the
        // resolved graph would carry a sub_workflow node.
        let resolver = StoreWorkflowResolver::new(
            Some(dir.path().to_path_buf()),
            store_with(vec![overlay("child", parent_of("child", "other"))]),
            CompanyId::new("acme"),
            "root".to_string(),
        );
        let graph = resolver.resolve("child").await.expect("resolves");
        assert_eq!(graph.id.as_deref(), Some("child"));
        assert_eq!(
            graph.nodes.len(),
            2,
            "the seed leaf must win over the overlay parent"
        );
    }
}
