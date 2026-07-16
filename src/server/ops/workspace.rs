//! Workspace writes: create a node, overwrite a file, rename/move a node, and
//! delete (folders recursive) — under both scope forms.
//!
//! Bodies mirror the console's `FsNode` (`frontend/src/lib/workspace.ts`).
//! Writes land in the [`WorkspaceStore`](crate::ports::WorkspaceStore); node
//! ids are stable ULIDs so a rename/move never breaks a reference.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::generate_id;
use crate::ports::workspace::{NodeKind, WorkspaceNode};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the workspace route fragment.
pub fn router() -> Router<AppState> {
    scoped("/workspace", post(create_node))
        .merge(scoped("/workspace/file/{node_id}", put(write_file)))
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
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameMove {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    parent_id: Option<String>,
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
            body.parent_id.as_deref(),
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
