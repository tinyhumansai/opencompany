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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use async_trait::async_trait;
use tinyflows::caps::WorkflowResolver;
use tinyflows::error::{EngineError, Result as TfResult};
use tinyflows::model::{NodeKind, WorkflowGraph};

use crate::company::{WorkflowFile, WorkflowNodeKind, load_workflow_with_globals};
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
    /// The live per-run policy inputs used to gate child graphs (issue #617).
    /// `None` for a dry run, whose effect slots are all inert.
    gates: Option<ChildPolicyGates>,
}

/// The policy inputs the top-level runner selected for this run.
///
/// Grouped so [`StoreWorkflowResolver::new`] does not grow more parameters for
/// one concern. A dry run passes `None`, preserving its no-gate semantics.
pub struct ChildPolicyGates {
    /// Production sets this false while policy-generated HITL is disabled.
    pub policy_hitl_enabled: bool,
    /// The effective company policy, including any console override.
    pub policy: crate::company::Policy,
    /// The parent run resolving this child.
    pub run_id: String,
    /// The company's live standing permissions.
    pub grants: crate::runtime::grants::GrantSet,
    /// Per-run record of the gates the resolver marked, keyed by child id.
    ///
    /// The parent's parking path reads this when a child pauses, so the card
    /// can name the child's tool and reason the way a top-level gate's card
    /// does (issue #617).
    pub registry: Arc<ChildGateRegistry>,
}

/// The namespace that separates a child gate's id from the `sub_workflow` node
/// that ran it, as tinyflows builds it (`namespaced_gate` in
/// `vendor/openhuman/vendor/tinyflows/src/nodes/integration/sub_workflow.rs`).
///
/// A parent gate `approve` and a child's gate `approve` are different gates in
/// different id spaces, so tinyflows reports the child's as `<node>::<gate>`.
/// Both consumers of that shape live on this side of the seam: the resolver's
/// [`child_gate_call`] and the runner's unreplayable-call report.
pub(crate) const GATE_NAMESPACE: &str = "::";

/// What the resolver recorded about one resolved child: the gated graph as
/// tinyflows runs it, and the calls the policy raised on it.
#[derive(Clone)]
pub(crate) struct ChildGateRecord {
    /// The child graph the engine is running, post-gate-pass.
    pub graph: WorkflowGraph,
    /// The calls the policy stopped on it (the ones marked `requires_approval`).
    pub gated: Vec<crate::workflows::gate::GatedCall>,
}

/// A per-run record of every child graph the resolver gated, keyed by child id.
///
/// The resolver is invoked by the engine mid-run; the parent's parking path
/// runs after the engine returns. This registry is the one channel between the
/// two, so a child that pauses can be described from what the gate pass
/// actually classified instead of being re-read from the store (which may have
/// moved on, and the graph the engine ran is what the card must describe).
#[derive(Default)]
pub(crate) struct ChildGateRegistry {
    inner: std::sync::Mutex<HashMap<String, ChildGateRecord>>,
}

impl ChildGateRegistry {
    /// Records the gate pass for one resolved child.
    pub(crate) fn record(&self, child_id: &str, record: ChildGateRecord) {
        self.inner
            .lock()
            .unwrap()
            .insert(child_id.to_string(), record);
    }

    /// The record for `child_id`, cloned out so the caller need not hold the
    /// lock (the graphs are small and lookups happen at pause time, not per
    /// node).
    pub(crate) fn get(&self, child_id: &str) -> Option<ChildGateRecord> {
        self.inner.lock().unwrap().get(child_id).cloned()
    }
}

impl StoreWorkflowResolver {
    /// Builds a resolver serving children from `source_dir` ∪ `company`'s
    /// overlay bodies, for a run rooted at `root_id`.
    pub fn new(
        source_dir: Option<PathBuf>,
        store: Arc<dyn CompanyStore>,
        company: CompanyId,
        root_id: String,
        gates: Option<ChildPolicyGates>,
    ) -> Self {
        Self {
            source_dir,
            store,
            company,
            root_id,
            gates,
        }
    }

    /// Applies the same policy pass the top-level runner uses to a child graph.
    ///
    /// tinyflows now surfaces a child pause at the parent's `sub_workflow` node
    /// using a namespaced id and forwards that approval when the parent re-runs.
    /// Marking the child here therefore reaches the ordinary card and resume
    /// path instead of silently executing an effect beneath the parent graph.
    ///
    /// The gate pass runs against the run's **root** workflow id, not the
    /// child's own: `policy_gates` binds its standing-permission subject to the
    /// workflow it is gating, and the card the parent parks is minted with the
    /// root's id (issue #617). Binding the child to its own id would make a
    /// permission the operator granted the top-level workflow invisible to the
    /// child's checks, so a child call would park again under a grant that
    /// should have admitted it.
    ///
    /// The resulting gates — and the gated graph itself — are recorded per child
    /// id so the parent's parking path can name them after the run pauses (see
    /// [`ChildGateRegistry`]).
    async fn apply_policy_gates(&self, child_id: &str, graph: &mut WorkflowGraph) {
        let Some(gates) = self.gates.as_ref() else {
            return;
        };
        let gated = if gates.policy_hitl_enabled {
            crate::workflows::gate::apply_policy_gates_with_policy(
                graph,
                &gates.policy,
                &self.company,
                &self.root_id,
                &gates.run_id,
                &gates.grants,
            )
            .await
        } else {
            crate::workflows::gate::policy_hitl_disabled(graph)
        };
        gates.registry.record(
            child_id,
            ChildGateRecord {
                graph: graph.clone(),
                gated,
            },
        );
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
        globals_disable: Vec<String>,
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
            let Ok(Some(file)) = load_workflow_with_globals(
                source_dir.as_deref(),
                &overlays,
                &globals_disable,
                &current,
            ) else {
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
            .map(|record| (record.overlay_workflows, record.manifest.globals.disable))
            .unwrap_or_default();
        let (overlays, globals_disable) = overlays;

        // (c) Load the child from the seed ∪ overlay union, re-running full
        // OpenCompany parse + validation on it (the same rules a hand-authored
        // or console-created graph passes).
        let file = load_workflow_with_globals(
            self.source_dir.as_deref(),
            &overlays,
            &globals_disable,
            workflow_id,
        )
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
            Self::guard_cycle(
                source_dir,
                overlays,
                globals_disable,
                root_id,
                start_id,
                start_file,
            )
        })
        .await
        .map_err(|err| {
            EngineError::Capability(format!(
                "sub_workflow '{workflow_id}' cycle scan failed: {err}"
            ))
        })??;

        // (e) Translate to a runnable tinyflows graph.
        let mut graph = crate::workflows::translate::translate(&file);
        // (f) A child is translated inside tinyflows, after the top-level
        // runner's gate pass. Apply that same pass before giving it back.
        self.apply_policy_gates(workflow_id, &mut graph).await;
        Ok(graph)
    }
}

/// The child workflow id a `sub_workflow` node resolves to — the key the
/// registry records the gate pass under.
///
/// A static `workflow_id` names it directly. A `=`-expression names it through
/// the engine, which resolves it against the node's run scope at run time; for
/// a `once` node — the only shape whose id this lookup can reconstruct — that
/// scope's `item` is the whole trigger input, so the same resolution is
/// repeated here. Best-effort: an expression touching a key a parked run no
/// longer carries (`run`, `nodes`), or a `per_item` fan-out whose per-element
/// scope needs the item index the paused id does not carry, yields `None` and
/// the caller falls back rather than failing the pause.
pub(crate) fn child_id_of(
    graph: &WorkflowGraph,
    node: &str,
    trigger_input: Option<&Value>,
) -> Option<String> {
    let node = graph.nodes.iter().find(|n| n.id == node)?;
    if !matches!(node.kind, NodeKind::SubWorkflow) {
        return None;
    }
    let config = node.config.get("workflow_id")?;
    let id = config.as_str()?;
    if !id.starts_with('=') {
        return Some(id.to_string());
    }
    // A `per_item` fan-out resolves `workflow_id` against each element's own
    // scope, and the paused id carries no item index to say which element that
    // was. Resolving against the whole-input scope could describe the wrong
    // child, so fall back rather than guess.
    if node.config.get("execution").and_then(Value::as_str) == Some("per_item") {
        return None;
    }
    let input = trigger_input?;
    tinyflows::expr::resolve(
        config,
        &serde_json::json!({ "item": input, "items": [input] }),
    )
    .as_str()
    .map(str::to_string)
}

/// Walks a namespaced pending id (`sub::work`, `sub::nested::work`) down through
/// the registry's per-child records, to the gate the deepest child paused on.
///
/// Each segment before the last names a `sub_workflow` node in the graph one
/// level up; [`child_id_of`] resolves the child it runs, and the registry entry
/// for that child carries the next graph down. The last segment names the gate
/// *inside* the deepest child. Returns that deepest record and the gate id, so
/// the caller can read the gate's own classification ([`ChildGateRecord::gated`])
/// or walk its upstream calls.
///
/// A segment that names no `sub_workflow` node, a child id the registry did not
/// record (its gate pass never ran — a child the policy let through), or an
/// expression-bound id this side cannot reconstruct yields `None`, and the
/// caller falls back to its own default rather than failing the pause.
pub(crate) fn descend(
    registry: &ChildGateRegistry,
    parent: &WorkflowGraph,
    node_id: &str,
    trigger_input: Option<&Value>,
) -> Option<(ChildGateRecord, String)> {
    let mut segments: Vec<&str> = node_id.split(GATE_NAMESPACE).collect();
    let gate = segments.pop()?;
    let mut record: Option<ChildGateRecord> = None;
    for node in segments {
        let graph = match &record {
            None => parent,
            Some(record) => &record.graph,
        };
        let child_id = child_id_of(graph, node, trigger_input)?;
        record = Some(registry.get(&child_id)?);
    }
    Some((record?, gate.to_string()))
}

/// The policy classification behind a namespaced child gate (`sub::work`,
/// `sub::nested::work`), if the resolver recorded it this run.
///
/// The parent's parking path resolves a namespaced id by descending the
/// registry's per-child records — resolving each intermediate `sub_workflow`
/// node's `workflow_id` (static, or a `=expr` best-effort against
/// `trigger_input`) to the child the engine actually ran — and reading the
/// deepest child's own classification. This is the only route by which the
/// child's policy gate (tool, reason, arguments) reaches the card: the parent
/// graph does not contain the child's nodes, and the child's partial output
/// does not travel up when it pauses (tinyflows drops it on
/// `ChildOutcome::Paused`).
pub(crate) fn child_gate_call(
    registry: &ChildGateRegistry,
    parent: &WorkflowGraph,
    node_id: &str,
    trigger_input: Option<&Value>,
) -> Option<crate::workflows::gate::GatedCall> {
    let (record, gate) = descend(registry, parent, node_id, trigger_input)?;
    record.gated.iter().find(|g| g.node_id == gate).cloned()
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

    /// A store whose record carries `overlays` and a `[globals].disable` list —
    /// [`store_with`] with the disable list always empty.
    fn store_with_globals_disable(
        overlays: Vec<OverlayWorkflow>,
        disable: Vec<String>,
    ) -> Arc<dyn CompanyStore> {
        let entries = disable
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest: CompanyManifest = toml::from_str(&format!(
            "[company]\nname = \"Acme\"\n\n[globals]\ndisable = [{entries}]\n"
        ))
        .expect("valid manifest");
        Arc::new(MemStore(std::sync::Mutex::new(Some(CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: overlays,
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }))))
    }

    /// A store whose record carries `overlays` as its runtime-authored graphs.
    fn store_with(overlays: Vec<OverlayWorkflow>) -> Arc<dyn CompanyStore> {
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n").expect("valid manifest");
        Arc::new(MemStore(std::sync::Mutex::new(Some(CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: overlays,
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }))))
    }

    /// A resolver over a seed directory only (no runtime-authored graphs).
    fn seed_resolver(dir: &std::path::Path, root_id: &str) -> StoreWorkflowResolver {
        StoreWorkflowResolver::new(
            Some(dir.to_path_buf()),
            store_with(Vec::new()),
            CompanyId::new("acme"),
            root_id.to_string(),
            None,
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
            None,
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

    /// A global workflow the company disabled via `[globals].disable` must
    /// fail resolution as a `sub_workflow` child, exactly like an unknown id —
    /// the same contract `crate::globals::test`'s
    /// `a_disabled_global_workflow_neither_lists_nor_loads` pins at the
    /// `load_workflow_with_globals` layer this resolver calls into.
    #[tokio::test]
    async fn a_company_disabled_global_child_fails_resolution() {
        let dropped = crate::globals::workflows()[0].id.clone();
        let store = store_with_globals_disable(Vec::new(), vec![format!("workflow:{dropped}")]);
        let resolver = StoreWorkflowResolver::new(
            None,
            store,
            CompanyId::new("acme"),
            "root".to_string(),
            None,
        );

        let err = resolver.resolve(&dropped).await.unwrap_err();
        assert!(
            err.to_string().contains(&dropped),
            "the error names the disabled child: {err}"
        );
        assert!(
            err.to_string().contains("not a saved workflow"),
            "a disabled global reads the same as an unknown id, not a cycle or a parse error: {err}"
        );
    }

    /// A disabled global is excluded from the cycle scan the same way an
    /// unresolvable child is (per `guard_cycle`'s own doc comment): `middle`
    /// runs the disabled global as a `sub_workflow`, and if the scan tried to
    /// load it the same way it loads a live child, it would hit the same
    /// company-disabled refusal `a_company_disabled_global_child_fails_resolution`
    /// pins — instead the disabled id is skipped as unresolvable, so resolving
    /// `middle` itself succeeds rather than failing with an unrelated
    /// "workflow not found" surfaced through the cycle guard.
    #[tokio::test]
    async fn a_disabled_global_in_the_closure_is_skipped_not_treated_as_a_cycle() {
        let dropped = crate::globals::workflows()[0].id.clone();
        let dir = tempfile::tempdir().unwrap();
        write_wf(dir.path(), "middle", &parent_of("middle", &dropped));
        let store = store_with_globals_disable(Vec::new(), vec![format!("workflow:{dropped}")]);
        let resolver = StoreWorkflowResolver::new(
            Some(dir.path().to_path_buf()),
            store,
            CompanyId::new("acme"),
            "root".to_string(),
            None,
        );

        let graph = resolver
            .resolve("middle")
            .await
            .expect("the disabled global in the closure is skipped, not fatal");
        assert_eq!(graph.id.as_deref(), Some("middle"));
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
            None,
        );
        let graph = resolver.resolve("child").await.expect("resolves");
        assert_eq!(graph.id.as_deref(), Some("child"));
        assert_eq!(
            graph.nodes.len(),
            2,
            "the seed leaf must win over the overlay parent"
        );
    }

    // ---- Issue #617: gating child calls -----------------------------------

    /// A child graph whose one working node is a `tool_call` the policy parks.
    fn child_with_shell(id: &str) -> String {
        format!(
            r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "run"
kind = "tool_call"
name = "Run"
[node.config]
slug = "shell"
[node.config.args]
# An ACTING command. Since issue #875 `shell` is classified by what it was
# handed, so a read would be a call the policy does not park — and the
# gate this fixture exists to exercise only happens for one that would.
command = "rm -rf ."
[[edge]]
from = "start"
to = "run"
"#
        )
    }

    fn gated_resolver(overlays: Vec<OverlayWorkflow>, mode: &str) -> StoreWorkflowResolver {
        let policy: crate::company::Policy =
            toml::from_str(&format!("mode = \"{mode}\"\nalways_approve = []\n"))
                .expect("valid [policy]");
        StoreWorkflowResolver::new(
            None,
            store_with(overlays),
            CompanyId::new("acme"),
            "root".to_string(),
            Some(ChildPolicyGates {
                policy_hitl_enabled: true,
                policy,
                run_id: "run-1".to_string(),
                grants: crate::runtime::grants::GrantSet::default(),
                registry: Arc::new(ChildGateRegistry::default()),
            }),
        )
    }

    /// Issue #617. The child's `shell` call is one the policy parks at the top
    /// level, so the resolver must mark it before tinyflows runs the child.
    #[tokio::test]
    async fn a_policy_gated_child_call_is_marked_before_the_engine_runs_it() {
        let resolver = gated_resolver(
            vec![overlay("child", child_with_shell("child"))],
            "supervised",
        );

        let graph = resolver.resolve("child").await.expect("child resolves");

        let run = graph
            .nodes
            .iter()
            .find(|n| n.id == "run")
            .expect("the child's node survives");
        assert!(
            run.config["requires_approval"].as_bool().unwrap_or(false),
            "the child call must be gated: {:?}",
            run.config
        );
    }

    /// A company whose policy does not park the call leaves the child runnable.
    #[tokio::test]
    async fn a_child_the_policy_would_not_park_is_not_marked() {
        let resolver = gated_resolver(vec![overlay("child", child_with_shell("child"))], "full");

        let graph = resolver.resolve("child").await.expect("child resolves");
        let run = graph
            .nodes
            .iter()
            .find(|n| n.id == "run")
            .expect("the child's node survives");

        assert!(
            run.config.get("requires_approval").is_none(),
            "an ungated child call remains runnable: {:?}",
            run.config
        );
    }

    /// A dry run executes nothing, so its resolver receives no gate context.
    #[tokio::test]
    async fn a_resolver_without_policy_gates_leaves_the_child_unmarked() {
        let resolver = overlay_resolver(vec![overlay("child", child_with_shell("child"))], "root");
        let graph = resolver.resolve("child").await.expect("child resolves");
        let run = graph
            .nodes
            .iter()
            .find(|n| n.id == "run")
            .expect("the child's node survives");
        assert!(run.config.get("requires_approval").is_none());
    }

    // ---- Issue #617: routing the child's gate pass back to the parent -------

    /// A child graph whose one working node is a `web_fetch` — the grantable
    /// call the standing-permission tests below exercise (`shell` is
    /// `Standing::PerCall`, so no grant can ever admit it).
    fn child_with_web_fetch(id: &str) -> String {
        format!(
            r#"
id = "{id}"
name = "{id}"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "fetch"
kind = "tool_call"
name = "Fetch"
[node.config]
slug = "web_fetch"
[node.config.args]
url = "https://docs.rs/jaq"
[[edge]]
from = "start"
to = "fetch"
"#
        )
    }

    /// A live standing permission for `web_fetch` held by one workflow.
    fn web_fetch_grant(workflow: &str) -> crate::runtime::grants::GrantSet {
        let grants = crate::runtime::grants::GrantSet::default();
        grants.grant_standing(crate::runtime::grants::StandingGrant {
            id: crate::runtime::grants::GrantId::new("g-web"),
            agent: String::new(),
            workflow: Some(workflow.to_string()),
            tool: "web_fetch".to_string(),
            verdict: crate::ports::types::Verdict::Approve,
            granted_by: crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "user-1".to_string(),
            },
            approval_id: crate::ports::types::ApprovalId::new("approval-1"),
            at_millis: 1_000,
            expires_at_millis: crate::ports::now_millis() + 60 * 60 * 1000,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
            scope: Some("https://docs.rs".to_string()),
        });
        grants
    }

    /// Like [`gated_resolver`], but hands back the registry too so a test can
    /// assert what the resolver recorded, and accepts the live grant set.
    fn gated_resolver_with_grants(
        overlays: Vec<OverlayWorkflow>,
        mode: &str,
        grants: crate::runtime::grants::GrantSet,
    ) -> (StoreWorkflowResolver, Arc<ChildGateRegistry>) {
        let policy: crate::company::Policy =
            toml::from_str(&format!("mode = \"{mode}\"\nalways_approve = []\n"))
                .expect("valid [policy]");
        let registry = Arc::new(ChildGateRegistry::default());
        let resolver = StoreWorkflowResolver::new(
            None,
            store_with(overlays),
            CompanyId::new("acme"),
            "root".to_string(),
            Some(ChildPolicyGates {
                policy_hitl_enabled: true,
                policy,
                run_id: "run-1".to_string(),
                grants,
                registry: registry.clone(),
            }),
        );
        (resolver, registry)
    }

    /// Issue #617. The child's policy check is bound to the run's **root**
    /// workflow id — the id the parked card is minted under — so a permission
    /// the operator granted the top-level workflow is honoured inside the
    /// child. Bound to the child's own id instead, the grant would not match
    /// and the child call would park again under a permission that should have
    /// admitted it.
    #[tokio::test]
    async fn a_standing_grant_for_the_root_workflow_admits_a_child_call() {
        let (resolver, _) = gated_resolver_with_grants(
            vec![overlay("child", child_with_web_fetch("child"))],
            "supervised",
            web_fetch_grant("root"),
        );

        let graph = resolver.resolve("child").await.expect("child resolves");
        let fetch = graph
            .nodes
            .iter()
            .find(|n| n.id == "fetch")
            .expect("the child's node survives");
        assert!(
            fetch.config.get("requires_approval").is_none(),
            "a grant for the root workflow must admit the child's call: {:?}",
            fetch.config
        );
    }

    /// The other direction of the subject decision: a grant bound to the
    /// child's own id does **not** admit it, because the child's checks run
    /// under the root workflow. No card path mints a child-bound grant — cards
    /// are minted with the root — so this pins the decision rather than a
    /// reachable state, and keeps the two ids from being confused again.
    #[tokio::test]
    async fn a_grant_bound_to_the_child_id_does_not_admit_the_child_call() {
        let (resolver, _) = gated_resolver_with_grants(
            vec![overlay("child", child_with_web_fetch("child"))],
            "supervised",
            web_fetch_grant("child"),
        );

        let graph = resolver.resolve("child").await.expect("child resolves");
        let fetch = graph
            .nodes
            .iter()
            .find(|n| n.id == "fetch")
            .expect("the child's node survives");
        assert!(
            fetch.config["requires_approval"].as_bool().unwrap_or(false),
            "a grant bound to the child's own id must not admit it: {:?}",
            fetch.config
        );
    }

    /// Issue #617. The resolver records each gated child — the graph the engine
    /// is actually running and the calls the policy raised on it — so the
    /// parent's parking path can name a child pause after the run settles.
    /// A namespaced pending id (`sub::work`) resolves through the parent graph
    /// and the registry back to the child's own classification.
    #[tokio::test]
    async fn a_policy_gated_child_is_recorded_for_the_parents_parking_path() {
        let (resolver, registry) = gated_resolver_with_grants(
            vec![overlay("child", child_with_shell("child"))],
            "supervised",
            crate::runtime::grants::GrantSet::default(),
        );
        resolver.resolve("child").await.expect("child resolves");

        let record = registry
            .get("child")
            .expect("the resolver recorded the gated child");
        // The recorded graph is the one the engine runs — post-gate-pass.
        assert!(
            record.graph.nodes.iter().any(|n| n.id == "run"),
            "the recorded graph is the gated child graph"
        );
        // The gated list names the child's OWN node ids (un-namespaced), so the
        // parent can match them against the stripped namespace.
        let gate = record
            .gated
            .iter()
            .find(|g| g.node_id == "run")
            .expect("the shell call was gated");
        assert_eq!(gate.slug, "shell");
        assert!(
            gate.reason.contains("shell"),
            "the reason names the call: {}",
            gate.reason
        );

        // The test graph is the post-gate graph. Populate a registry record
        // manually so this unit test exercises the namespace lookup itself,
        // rather than depending on a second engine resolve to reach a nested
        // child.
        let registry = Arc::new(ChildGateRegistry::default());
        let child = crate::workflows::translate::translate(
            &crate::company::parse_workflow(&child_with_shell("child")).expect("child parses"),
        );
        let gated = child
            .nodes
            .iter()
            .find(|node| node.id == "run")
            .map(|node| crate::workflows::gate::GatedCall {
                node_id: node.id.clone(),
                slug: "shell".to_string(),
                reason: "shell requires approval".to_string(),
                args: node.config.get("args").cloned().unwrap_or(Value::Null),
                target: None,
            })
            .into_iter()
            .collect();
        registry.record(
            "child",
            ChildGateRecord {
                graph: child,
                gated,
            },
        );

        let parent = crate::workflows::translate::translate(
            &crate::company::parse_workflow(&parent_of("parent", "child")).expect("parent parses"),
        );
        let described = child_gate_call(&registry, &parent, "sub::run", None)
            .expect("a namespaced child gate resolves through the registry");
        assert_eq!(described.node_id, "run");
        assert_eq!(described.slug, "shell");
    }

    /// Issue #617, the nested half. A gate two levels down is reported as
    /// `sub::nested::work`; the parent graph resolves only the first hop, so
    /// the parking path must descend the registry — resolving each intermediate
    /// `sub_workflow` node's `workflow_id` to the child that actually ran it —
    /// to reach the grandchild's own classification.
    #[tokio::test]
    async fn a_two_level_child_gate_resolves_through_the_registry() {
        let (_resolver, _unused_registry) = gated_resolver_with_grants(
            vec![
                // `a` runs `b` from a node named `nested`.
                overlay("a", parent_of("a", "b")),
                overlay("b", child_with_shell("b")),
            ],
            "supervised",
            crate::runtime::grants::GrantSet::default(),
        );
        // `a`'s child record is the graph that contains `nested`; `b`'s record
        // is the graph that contains the gated `work` node. Populate both
        // explicitly so the lookup test mirrors the engine's call order.
        let registry = Arc::new(ChildGateRegistry::default());
        let a = crate::workflows::translate::translate(
            &crate::company::parse_workflow(
                r#"
id = "a"
name = "a"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "nested"
kind = "sub_workflow"
name = "Nested"
[node.config]
workflow_id = "b"
[[edge]]
from = "start"
to = "nested"
"#,
            )
            .expect("a parses"),
        );
        let b = crate::workflows::translate::translate(
            &crate::company::parse_workflow(&child_with_shell("b")).expect("b parses"),
        );
        let gated = b
            .nodes
            .iter()
            .find(|node| node.id == "run")
            .map(|node| crate::workflows::gate::GatedCall {
                node_id: node.id.clone(),
                slug: "shell".to_string(),
                reason: "shell requires approval".to_string(),
                args: node.config.get("args").cloned().unwrap_or(Value::Null),
                target: None,
            })
            .into_iter()
            .collect();
        registry.record(
            "a",
            ChildGateRecord {
                graph: a,
                gated: Vec::new(),
            },
        );
        registry.record("b", ChildGateRecord { graph: b, gated });
        let parent = crate::workflows::translate::translate(
            &crate::company::parse_workflow(&parent_of("parent", "a")).expect("parent parses"),
        );
        let described = child_gate_call(&registry, &parent, "sub::nested::run", None)
            .expect("a two-level namespaced child gate resolves through the registry");
        assert_eq!(described.node_id, "run");
        assert_eq!(described.slug, "shell");
    }

    /// Issue #617, the dynamic half. A `workflow_id = "=item.target"` child is
    /// resolved by the engine at run time, so the registry is keyed by the
    /// RESOLVED id (`child`), not the authored expression. The parking path
    /// must resolve the same expression against the trigger input to find the
    /// record and describe the gate.
    #[tokio::test]
    async fn an_expr_bound_child_gate_resolves_through_the_registry() {
        let registry = Arc::new(ChildGateRegistry::default());
        let child = crate::workflows::translate::translate(
            &crate::company::parse_workflow(&child_with_shell("child")).expect("child parses"),
        );
        let gated = child
            .nodes
            .iter()
            .find(|node| node.id == "run")
            .map(|node| crate::workflows::gate::GatedCall {
                node_id: node.id.clone(),
                slug: "shell".to_string(),
                reason: "shell requires approval".to_string(),
                args: node.config.get("args").cloned().unwrap_or(Value::Null),
                target: None,
            })
            .into_iter()
            .collect();
        registry.record(
            "child",
            ChildGateRecord {
                graph: child,
                gated,
            },
        );

        let parent = r#"
id = "parent"
name = "Parent"
[[node]]
id = "start"
kind = "trigger"
name = "Start"
[[node]]
id = "sub"
kind = "sub_workflow"
name = "Sub"
[node.config]
workflow_id = "=item.target"
[[edge]]
from = "start"
to = "sub"
"#;
        let file = crate::company::parse_workflow(parent).expect("parent parses");
        let parent = crate::workflows::translate::translate(&file);
        let described = child_gate_call(
            &registry,
            &parent,
            "sub::run",
            Some(&serde_json::json!({ "target": "child" })),
        )
        .expect("an expression-bound child gate resolves through the registry");
        assert_eq!(described.node_id, "run");
        assert_eq!(described.slug, "shell");
    }
}
