//! Workflow reads: `Company.workflows` summaries (from the manifest's enabled
//! list) and `Company.workflow(id)` graphs.
//!
//! Graph bodies come from the union of the company's two sources — the seed
//! files at `{company}/workflows/<id>.toml` and the runtime-authored bodies on
//! the [`CompanyRecord`](crate::ports::types::CompanyRecord) overlay — via
//! [`load_workflow_union`]. A hosted tenant has no source directory, so all of
//! its graphs are overlay bodies; resolving only the seed side used to render
//! them as bare ids with no graph (issue #168).

use std::collections::HashSet;
use std::sync::Arc;

use async_graphql::{Context, ID, SimpleObject};

use crate::company::runtime::CompanyRuntime;
use crate::company::{
    WorkflowFile, WorkflowPostconditionDef, list_workflows_with_globals, load_workflow_with_globals,
};
use crate::ports::types::OverlayWorkflow;

/// A one-line workflow summary for the workflows list.
#[derive(SimpleObject)]
#[graphql(name = "WorkflowSummary")]
pub struct WorkflowSummaryGql {
    /// The workflow id.
    pub id: ID,
    /// The workflow display name.
    pub name: String,
    /// Whether the workflow id appears in the company manifest's
    /// `[workflows].enabled` list.
    ///
    /// This is manifest membership, **not** "does this workflow exist and can it
    /// run". A runtime-authored workflow is listed here whether or not it is
    /// manifest-enabled, and `Company.workflow(id)` / the run routes serve any
    /// saved graph regardless of this flag — nothing consults it to decide
    /// whether a workflow may run. Treat it as "declared by the blueprint".
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
    /// Free-form, kind-specific node config (P1), exposed as a JSON scalar so
    /// `Company.workflow(id)` does not drop model data.
    pub config: Option<async_graphql::Json<serde_json::Value>>,
    /// Per-node error policy: `stop` / `continue` / `route`.
    pub on_error: Option<String>,
    /// Per-node retry policy (attempts + backoff), as a JSON scalar. Keys are
    /// camelCase (`maxAttempts` / `backoffMs`) to match the REST read shape;
    /// see [`RetryGql`].
    pub retry: Option<async_graphql::Json<RetryGql>>,
    /// Whether the node pauses awaiting operator approval before it runs.
    pub requires_approval: Option<bool>,
    /// Where an `output` node's report is routed once the run finishes (issue
    /// #170), as a JSON scalar: `{"kind": "owner"|"email"|"channel",
    /// "target"?: "…"}`. Exposed for the same reason `config` is — so
    /// `Company.workflow(id)` does not drop model data. Both keys are single
    /// words, so the model shape needs no camelCase mirror the way `retry` does.
    pub destination: Option<async_graphql::Json<crate::company::WorkflowDestinationDef>>,
    /// A node's declared deterministic postcondition (issue #1866), as a JSON
    /// scalar for the same reason `destination` is — `require`/`field` are
    /// single words, so no camelCase mirror is needed.
    pub postcondition: Option<async_graphql::Json<WorkflowPostconditionDef>>,
}

/// The camelCase retry shape the console reads back over GraphQL, mirroring the
/// REST `WorkflowRetryOut` (`maxAttempts` / `backoffMs`). The model/TOML type
/// [`crate::company::WorkflowRetryDef`] stays snake_case; without this mirror
/// the GraphQL JSON scalar would leak snake_case keys and diverge from REST.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryGql {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff: Option<String>,
}

impl From<crate::company::WorkflowRetryDef> for RetryGql {
    fn from(r: crate::company::WorkflowRetryDef) -> Self {
        Self {
            max_attempts: r.max_attempts,
            backoff_ms: r.backoff_ms,
            backoff: r.backoff,
        }
    }
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
                    config: node.config.map(async_graphql::Json),
                    on_error: node.on_error,
                    retry: node.retry.map(|r| async_graphql::Json(RetryGql::from(r))),
                    requires_approval: node.requires_approval,
                    destination: node.destination.map(async_graphql::Json),
                    postcondition: node.postcondition.map(async_graphql::Json),
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

/// Best-effort load of one workflow graph from the seed ∪ overlay union.
/// Yields `None` when neither source has the id, or the body it finds is
/// invalid — a resolver never fails the whole query over one bad graph.
fn load_one(
    runtime: &Arc<CompanyRuntime>,
    overlays: &[OverlayWorkflow],
    disable: &[String],
    id: &str,
) -> Option<WorkflowFile> {
    load_workflow_with_globals(runtime.source_dir(), overlays, disable, id)
        .ok()
        .flatten()
}

/// The enabled workflow ids from the company manifest.
async fn enabled_ids(runtime: &Arc<CompanyRuntime>) -> async_graphql::Result<Vec<String>> {
    Ok(runtime.enabled_workflow_ids().await?)
}

/// The company's runtime-authored graph bodies and its `[globals].disable`,
/// read once per resolve from a single record load — the pair every union read
/// needs, exactly as on the REST side. A company with no persisted record
/// contributes neither.
async fn overlays_and_globals(
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<(Vec<OverlayWorkflow>, Vec<String>)> {
    Ok(runtime
        .store()
        .load(runtime.id())
        .await?
        .map(|record| (record.overlay_workflows, record.manifest.globals.disable))
        .unwrap_or_default())
}

/// Resolves `Company.workflows` — every workflow the company has saved.
///
/// The id set is built exactly the way the REST picker
/// (`GET …/workflows`) builds it, so the two read surfaces cannot disagree:
/// first every graph that has a body (seed ∪ overlay, deduped with the seed
/// winning), then any manifest-`enabled` id that has no body in either source,
/// named after itself.
///
/// Driving this off the manifest's enabled list alone — as it used to — made a
/// runtime-authored workflow invisible here while `Company.workflow(id)`
/// returned its full graph. That gap was not hypothetical: a boot rebuild used
/// to overwrite the persisted record's manifest from the seed
/// (`RuntimeBuilder`), so a runtime-added enabled id was gone after a restart
/// and the graph body was the only surviving evidence the workflow existed.
///
/// Issue #208 closed that hole at the source — a rebuild now merges surviving
/// overlay ids back into `[workflows].enabled`, so `enabled` reads `true` again
/// after a restart. Enumerating from bodies stays the right shape regardless:
/// it is what keeps this resolver and the REST picker agreeing on the id set
/// whatever put a record in a body-without-enabled-entry state.
pub(crate) async fn resolve_summaries(
    _ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Vec<WorkflowSummaryGql>> {
    let (overlays, globals_disable) = overlays_and_globals(runtime).await?;
    let enabled = enabled_ids(runtime).await?;
    let enabled_set: HashSet<&str> = enabled.iter().map(String::as_str).collect();

    let mut summaries = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for file in list_workflows_with_globals(runtime.source_dir(), &overlays, &globals_disable) {
        seen.insert(file.id.clone());
        summaries.push(WorkflowSummaryGql {
            // A global graph is armed by being in the baseline at all — it has
            // no `[workflows].enabled` entry to be named in, and the REST picker
            // reports it enabled for the same reason.
            enabled: file.global || enabled_set.contains(file.id.as_str()),
            id: ID(file.id),
            name: file.name,
        });
    }

    // Manifest-enabled ids with no loadable graph anywhere still list, named
    // after themselves — the same fallback the REST picker uses.
    for id in enabled {
        if !seen.insert(id.clone()) {
            continue;
        }
        summaries.push(WorkflowSummaryGql {
            id: ID(id.clone()),
            name: id,
            enabled: true,
        });
    }

    Ok(summaries)
}

/// Resolves `Company.workflow(id)`, returning null when the graph is unavailable.
pub(crate) async fn resolve_one(
    _ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
    id: &str,
) -> async_graphql::Result<Option<WorkflowGql>> {
    let (overlays, globals_disable) = overlays_and_globals(runtime).await?;
    Ok(load_one(runtime, &overlays, &globals_disable, id).map(WorkflowGql::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::parse_workflow;
    use serde_json::json;

    #[test]
    fn node_conversion_preserves_p1_fields_and_camelcases_retry() {
        let file = parse_workflow(
            r#"
            id = "wf"
            name = "Workflow"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            on_error = "continue"
            requires_approval = true
            [node.config]
            message = "hello"
            [node.retry]
            max_attempts = 3
            backoff_ms = 250
            backoff = "exponential"
            "#,
        )
        .expect("workflow parses");

        let gql = WorkflowGql::from(file);
        let node = &gql.nodes[0];
        assert_eq!(node.config.as_ref().unwrap().0, json!({"message": "hello"}));
        assert_eq!(node.on_error.as_deref(), Some("continue"));
        assert_eq!(node.requires_approval, Some(true));
        assert_eq!(
            serde_json::to_value(&node.retry.as_ref().unwrap().0).unwrap(),
            json!({
                "maxAttempts": 3,
                "backoffMs": 250,
                "backoff": "exponential"
            })
        );
        // A node with no destination carries none.
        assert!(node.destination.is_none());
    }

    /// An `output` node's destination reaches the GraphQL read shape too — the
    /// console's REST path is not the only reader, and a resolver that dropped
    /// it would report the graph as routing nowhere (issue #170).
    #[test]
    fn node_conversion_preserves_the_output_destination() {
        let file = parse_workflow(
            r#"
            id = "wf"
            name = "Workflow"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "done"
            kind = "output"
            name = "Report"
            [node.destination]
            kind = "email"
            target = "ada@example.com"
            [[edge]]
            from = "start"
            to = "done"
            "#,
        )
        .expect("workflow parses");

        let gql = WorkflowGql::from(file);
        let done = gql.nodes.iter().find(|n| n.id.as_str() == "done").unwrap();
        assert_eq!(
            serde_json::to_value(&done.destination.as_ref().unwrap().0).unwrap(),
            json!({ "kind": "email", "target": "ada@example.com" })
        );
        // `owner` carries no target, and the key stays absent rather than null.
        let owner = parse_workflow(
            r#"
            id = "wf2"
            name = "Workflow 2"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "done"
            kind = "output"
            name = "Report"
            [node.destination]
            kind = "owner"
            [[edge]]
            from = "start"
            to = "done"
            "#,
        )
        .expect("parses");
        let gql = WorkflowGql::from(owner);
        let done = gql.nodes.iter().find(|n| n.id.as_str() == "done").unwrap();
        assert_eq!(
            serde_json::to_value(&done.destination.as_ref().unwrap().0).unwrap(),
            json!({ "kind": "owner" })
        );
    }

    /// Bonus finding from the #1937 boundary sweep (issue #1866): a node's
    /// declared `postcondition` was never exposed over GraphQL at all — every
    /// sibling policy field (`config`/`on_error`/`retry`/`requires_approval`/
    /// `destination`) had a field on `WorkflowNodeGql` and this didn't, so
    /// `Company.workflow(id)` silently dropped a run-safety gate for any
    /// console surface reading over GraphQL instead of REST. Same shape as
    /// `node_conversion_preserves_the_output_destination` above.
    #[test]
    fn node_conversion_preserves_the_postcondition() {
        let file = parse_workflow(
            r#"
            id = "wf3"
            name = "Workflow 3"
            [[node]]
            id = "start"
            kind = "trigger"
            name = "Start"
            [[node]]
            id = "worker"
            kind = "agent"
            name = "Worker"
            agent = "ceo"
            [node.postcondition]
            require = "field_present"
            field = "json.items"
            [[edge]]
            from = "start"
            to = "worker"
            "#,
        )
        .expect("workflow parses");

        let gql = WorkflowGql::from(file);
        let worker = gql
            .nodes
            .iter()
            .find(|n| n.id.as_str() == "worker")
            .unwrap();
        assert_eq!(
            serde_json::to_value(&worker.postcondition.as_ref().unwrap().0).unwrap(),
            json!({ "require": "field_present", "field": "json.items" })
        );
    }
}
