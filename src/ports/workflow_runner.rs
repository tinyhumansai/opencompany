//! The [`WorkflowRunner`] port: execute a company's workflow graph.
//!
//! A company's workflows are data-only
//! [`WorkflowFile`](crate::company::workflow_file::WorkflowFile) graphs. Running
//! one is dependency-inverted behind this port so the kernel and the HTTP layer
//! depend only on the trait: the concrete engine-backed implementation
//! (`crate::workflows::HarnessWorkflowRunner`, which drives the graph on the
//! embedded `tinyflows` engine with agent nodes on the harness pool) is compiled
//! only under `feature = "openhuman"`. The default build compiles this trait and
//! its result type but wires no implementation — a runtime with no runner leaves
//! the run route reporting "not wired", exactly like the other networked seams.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::company::WorkflowFile;
use crate::ports::types::CompanyId;

/// The outcome of running one workflow to completion.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// The final run state after the terminal node(s) completed. Its shape is
    /// the engine's `{ "run": …, "nodes": { "<id>": { "items": [ … ] } } }` map.
    pub output: Value,
    /// Node ids that paused the run awaiting human approval. Empty for a run
    /// that reached its terminal node(s) without gating.
    pub pending_approvals: Vec<String>,
}

/// Runs a company's workflow graph to completion.
///
/// `company` names the tenant whose roster the run's agent nodes execute on;
/// `workflow` is the parsed graph; `input` is the trigger payload (an arbitrary
/// JSON value seeded as the trigger node's item).
#[async_trait]
pub trait WorkflowRunner: Send + Sync {
    /// Runs `workflow` for `company` with the trigger `input`, returning the
    /// final state and any nodes left pending approval.
    async fn run(
        &self,
        company: &CompanyId,
        workflow: &WorkflowFile,
        input: Value,
    ) -> Result<WorkflowRun>;
}
