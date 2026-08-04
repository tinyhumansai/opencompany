//! Workspace reads + writes: list the tree, read one file with its backlinks,
//! create a node, overwrite a file, rename/move a node, and delete (folders
//! recursive) — under both scope forms.
//!
//! Bodies mirror the console's `FsNode` (`frontend/src/api/workspace.ts`).
//! Writes land in the [`WorkspaceStore`](crate::ports::WorkspaceStore); node
//! ids are stable ULIDs so a rename/move never breaks a reference.
//!
//! ```text
//! GET    …/workspace                  the whole tree (metadata; no bodies)
//! GET    …/workspace/file/{nodeId}    one file: content + inbound backlinks
//! POST   …/workspace                  create a folder/file (or upload)
//! PUT    …/workspace/file/{nodeId}    overwrite file content
//! PATCH  …/workspace/{nodeId}         rename / move
//! DELETE …/workspace/{nodeId}         delete a node (folders recursive)
//! ```
//!
//! ## Why the two `GET`s are REST and not GraphQL
//!
//! Every other console read goes through GraphQL, and `Company.workspaceTree` /
//! `workspaceFile` have existed since the read plane landed — but the operator
//! console ships **no GraphQL client**. Reaching them from the Workspace tab
//! would mean a second wire protocol, a second auth path, a second error
//! envelope and ISO-8601 string timestamps in a view whose siblings all use
//! epoch millis. These twins keep the console on one client; the backlink scan
//! itself is shared with the resolver
//! ([`file_with_backlinks`](crate::company::workspace_links::file_with_backlinks))
//! so the two surfaces cannot answer differently.
//!
//! ## Known limits (issue #177 — documented, not worked around)
//!
//! * **No authorship.** [`WorkspaceNode`] carries no author/origin field, so a
//!   note an agent wrote is indistinguishable from one the operator typed.
//!   Tracked by issue #326.
//! * **No live push.** A write that lands while the tab is open is only visible
//!   on a refetch (refresh button / window focus). Tracked by issue #327.
//! * **No CAS on the console write path.** Agent writes require an
//!   `expected_updated_at` compare-and-swap token; the console `PUT` does not,
//!   so a concurrent agent write can be overwritten by the operator's save. That
//!   is the store's stated design — the operator is the dominant editor, and the
//!   agent's *next* CAS write fails and re-reads, so the agent side self-heals.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::workspace_links::file_with_backlinks;
use crate::error::OpenCompanyError;
use crate::ports::generate_id;
use crate::ports::workspace::{NodeKind, WorkspaceNode};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workspace route fragment.
pub fn router() -> Router<AppState> {
    scoped("/workspace", post(create_node).get(list_tree))
        .merge(scoped(
            "/workspace/file/{node_id}",
            put(write_file).get(read_file),
        ))
        .merge(scoped(
            "/workspace/{node_id}",
            patch(rename_move).delete(delete_node),
        ))
}

/// A workspace node as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FsNode {
    id: String,
    name: String,
    kind: NodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    updated_at: u64,
}

impl FsNode {
    fn from_node(node: WorkspaceNode, content: Option<String>) -> Self {
        Self {
            id: node.id,
            name: node.name,
            kind: node.kind,
            parent_id: node.parent_id,
            content,
            updated_at: node.updated_at_millis,
        }
    }
}

/// One workspace file with its body and the notes that link to it.
///
/// The REST twin of the GraphQL `WorkspaceFile`, differing only in timestamp
/// shape (epoch millis, like every other console read) — the backlinks come
/// from the same shared scan.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceFileBody {
    id: String,
    name: String,
    content: String,
    updated_at: u64,
    /// Other files whose content links to this one via `[[name]]`.
    backlinks: Vec<FsNode>,
}

/// The create-node body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateNode {
    name: String,
    kind: NodeKind,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// The overwrite-file body.
#[derive(Debug, Deserialize)]
struct WriteFile {
    content: String,
}

/// The rename/move body.
///
/// `parent_id` uses a double option so an omitted `parentId` (leave the parent
/// unchanged) is distinguished from an explicit `"parentId": null` (move the
/// node back to the workspace root).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameMove {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    parent_id: Option<Option<String>>,
}

/// Deserializes into `Some(inner)` when the field is present (so an explicit
/// `null` becomes `Some(None)`); the `#[serde(default)]` leaves an omitted field
/// as `None`.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

/// The overwrite-file response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteAck {
    updated_at: u64,
}

/// The sub-resource path (`node_id`).
#[derive(Debug, Deserialize)]
struct NodePath {
    node_id: String,
}

/// `GET …/workspace` — every node in the tree, metadata only.
///
/// Bodies are deliberately omitted: a tree read is the console's *navigation*
/// call and happens on every mount, focus and refresh, so shipping every note's
/// content would make it grow without bound with the workspace. The console
/// fetches a body when a note is opened ([`read_file`]) — the same
/// index-then-fetch split the agent-facing `workspace_list` / `workspace_read`
/// tools make.
async fn list_tree(company: ScopedCompany) -> Result<Json<Vec<FsNode>>, ApiError> {
    let nodes = company.runtime.workspace().tree(company.id()).await?;
    Ok(Json(
        nodes
            .into_iter()
            .map(|node| FsNode::from_node(node, None))
            .collect(),
    ))
}

/// `GET …/workspace/file/{node_id}` — one file's content plus the notes that
/// link to it.
///
/// A folder id 404s rather than answering with an empty body: the console only
/// ever opens files, so a folder id here is a caller bug, and reporting it as an
/// empty note would hide it.
async fn read_file(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
) -> Result<Json<WorkspaceFileBody>, ApiError> {
    let found =
        file_with_backlinks(company.runtime.workspace().as_ref(), company.id(), &node_id).await?;
    let Some((node, content, backlinks)) = found.filter(|(node, _, _)| node.kind == NodeKind::File)
    else {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workspace file {node_id}"
        ))));
    };
    Ok(Json(WorkspaceFileBody {
        id: node.id,
        name: node.name,
        content,
        updated_at: node.updated_at_millis,
        backlinks: backlinks
            .into_iter()
            .map(|node| FsNode::from_node(node, None))
            .collect(),
    }))
}

async fn create_node(
    company: ScopedCompany,
    Json(body): Json<CreateNode>,
) -> Result<Json<FsNode>, ApiError> {
    let node = WorkspaceNode {
        id: generate_id(),
        name: body.name,
        kind: body.kind,
        parent_id: body.parent_id,
        updated_at_millis: crate::ports::now_millis(),
    };
    company
        .runtime
        .workspace()
        .create(company.id(), &node, body.content.as_deref())
        .await?;
    let content = match node.kind {
        NodeKind::File => Some(body.content.unwrap_or_default()),
        NodeKind::Folder => None,
    };
    Ok(Json(FsNode::from_node(node, content)))
}

async fn write_file(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
    Json(body): Json<WriteFile>,
) -> Result<Json<WriteAck>, ApiError> {
    let node = company
        .runtime
        .workspace()
        .write(company.id(), &node_id, &body.content)
        .await?;
    Ok(Json(WriteAck {
        updated_at: node.updated_at_millis,
    }))
}

async fn rename_move(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
    Json(body): Json<RenameMove>,
) -> Result<Json<FsNode>, ApiError> {
    let node = company
        .runtime
        .workspace()
        .rename_move(
            company.id(),
            &node_id,
            body.name.as_deref(),
            body.parent_id.as_ref().map(Option::as_deref),
        )
        .await?;
    let content = match node.kind {
        NodeKind::File => company
            .runtime
            .workspace()
            .read(company.id(), &node_id)
            .await?
            .map(|(_, body)| body),
        NodeKind::Folder => None,
    };
    Ok(Json(FsNode::from_node(node, content)))
}

async fn delete_node(
    company: ScopedCompany,
    Path(NodePath { node_id }): Path<NodePath>,
) -> Result<StatusCode, ApiError> {
    if company
        .runtime
        .workspace()
        .delete(company.id(), &node_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "workspace node {node_id}"
        ))))
    }
}
