//! Workflow reads: `Company.workflows` summaries (from the manifest's enabled
//! list) and `Company.workflow(id)` graphs (parsed from
//! `{company}/workflows/<id>.toml` via WS1's `workflow_file`).

use std::path::Path;
use std::sync::Arc;

use async_graphql::{Context, ID, SimpleObject};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::company::{WorkflowFile, parse_workflow};
use crate::store::Bundle;

/// A one-line workflow summary for the workflows list.
#[derive(SimpleObject)]
#[graphql(name = "WorkflowSummary")]
pub struct WorkflowSummaryGql {
    /// The workflow id.
    pub id: ID,
    /// The workflow display name.
    pub name: String,
    /// Whether the workflow is enabled in the manifest.
    pub enabled: bool,
}

/// A full workflow graph.
#[derive(SimpleObject)]
#[graphql(name = "Workflow")]
pub struct WorkflowGql {
    /// The workflow id.
    pub id: ID,
    /// The workflow display name.
    pub name: String,
    /// The graph nodes.
    pub nodes: Vec<WorkflowNodeGql>,
    /// The graph edges.
    pub edges: Vec<WorkflowEdgeGql>,
}

/// One node in a workflow graph.
#[derive(SimpleObject)]
#[graphql(name = "WorkflowNode")]
pub struct WorkflowNodeGql {
    /// The node id.
    pub id: ID,
    /// The node kind (`trigger`, `agent`, `toolCall`, ...).
    pub kind: String,
    /// The node display name.
    pub name: String,
    /// An optional one-line summary.
    pub summary: Option<String>,
}

/// One directed edge in a workflow graph.
#[derive(SimpleObject)]
#[graphql(name = "WorkflowEdge")]
pub struct WorkflowEdgeGql {
    /// The source node id.
    pub from: ID,
    /// The target node id.
    pub to: ID,
    /// An optional edge label.
    pub label: Option<String>,
}

impl From<WorkflowFile> for WorkflowGql {
    fn from(file: WorkflowFile) -> Self {
        Self {
            id: ID(file.id),
            name: file.name,
            nodes: file
                .nodes
                .into_iter()
                .map(|node| WorkflowNodeGql {
                    id: ID(node.id),
                    kind: node.kind.as_str().to_string(),
                    name: node.name,
                    summary: node.summary,
                })
                .collect(),
            edges: file
                .edges
                .into_iter()
                .map(|edge| WorkflowEdgeGql {
                    from: ID(edge.from),
                    to: ID(edge.to),
                    label: edge.label,
                })
                .collect(),
        }
    }
}

/// Best-effort parse of one workflow graph from a company directory.
fn load_one(dir: &Path, id: &str) -> Option<WorkflowFile> {
    let path = dir.join("workflows").join(format!("{id}.toml"));
    let text = std::fs::read_to_string(path).ok()?;
    parse_workflow(&text).ok()
}

/// The enabled workflow ids from the company manifest.
async fn enabled_ids(runtime: &Arc<CompanyRuntime>) -> async_graphql::Result<Vec<String>> {
    let record = runtime.store().load(runtime.id()).await?;
    Ok(record
        .map(|record| record.manifest.workflows.enabled)
        .unwrap_or_default())
}

/// Resolves `Company.workflows`.
pub(crate) async fn resolve_summaries(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Vec<WorkflowSummaryGql>> {
    let state = ctx.data::<AppState>()?;
    let dir = Bundle::new(state.home(), runtime.id()).dir().to_path_buf();
    let ids = enabled_ids(runtime).await?;
    Ok(ids
        .into_iter()
        .map(|id| {
            let name = load_one(&dir, &id)
                .map(|file| file.name)
                .unwrap_or_else(|| id.clone());
            WorkflowSummaryGql {
                id: ID(id),
                name,
                enabled: true,
            }
        })
        .collect())
}

/// Resolves `Company.workflow(id)`, returning null when the graph is unavailable.
pub(crate) async fn resolve_one(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
    id: &str,
) -> async_graphql::Result<Option<WorkflowGql>> {
    let state = ctx.data::<AppState>()?;
    let dir = Bundle::new(state.home(), runtime.id()).dir().to_path_buf();
    Ok(load_one(&dir, id).map(WorkflowGql::from))
}
