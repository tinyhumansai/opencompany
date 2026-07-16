//! The workspace tree read: `Company.workspaceTree` / `workspaceFile` over the
//! [`WorkspaceStore`] port, with `[[wikilink]]` backlinks computed at read.

use std::sync::Arc;

use async_graphql::{ID, SimpleObject};

use super::iso8601;
use crate::company::extract_wikilinks;
use crate::company::runtime::CompanyRuntime;
use crate::ports::workspace::{NodeKind, WorkspaceNode};

/// One node (folder or file) in the workspace tree. Mirrors [`WorkspaceNode`].
#[derive(SimpleObject, Clone)]
#[graphql(name = "FsNode")]
pub struct FsNodeGql {
    /// The node id (stable ULID).
    pub id: ID,
    /// The node name.
    pub name: String,
    /// `folder` or `file`.
    pub kind: String,
    /// The parent node id, or null at the root.
    pub parent_id: Option<ID>,
    /// When it was last updated, ISO-8601 UTC.
    pub updated_at: String,
}

impl From<WorkspaceNode> for FsNodeGql {
    fn from(node: WorkspaceNode) -> Self {
        let kind = match node.kind {
            NodeKind::Folder => "folder",
            NodeKind::File => "file",
        };
        Self {
            id: ID(node.id),
            name: node.name,
            kind: kind.to_string(),
            parent_id: node.parent_id.map(ID),
            updated_at: iso8601(node.updated_at_millis),
        }
    }
}

/// A single workspace file with its content and inbound `[[wikilink]]` backlinks.
#[derive(SimpleObject)]
#[graphql(name = "WorkspaceFile")]
pub struct WorkspaceFileGql {
    /// The file id.
    pub id: ID,
    /// The file name.
    pub name: String,
    /// The file content.
    pub content: String,
    /// When it was last updated, ISO-8601 UTC.
    pub updated_at: String,
    /// Other files whose content links to this one via `[[name]]`.
    pub backlinks: Vec<FsNodeGql>,
}

/// The link target a file's name presents: its base name minus a `.md` suffix.
fn link_target(name: &str) -> &str {
    name.strip_suffix(".md").unwrap_or(name)
}

/// Resolves `Company.workspaceTree`.
pub(crate) async fn resolve_tree(
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Vec<FsNodeGql>> {
    let nodes = runtime.workspace().tree(runtime.id()).await?;
    Ok(nodes.into_iter().map(FsNodeGql::from).collect())
}

/// Resolves `Company.workspaceFile(id)`, returning null when absent.
pub(crate) async fn resolve_file(
    runtime: &Arc<CompanyRuntime>,
    id: &str,
) -> async_graphql::Result<Option<WorkspaceFileGql>> {
    let Some((node, content)) = runtime.workspace().read(runtime.id(), id).await? else {
        return Ok(None);
    };
    let target = link_target(&node.name).to_string();

    // Backlinks: scan every other file node's content for a `[[target]]` link.
    let mut backlinks = Vec::new();
    for other in runtime.workspace().tree(runtime.id()).await? {
        if other.id == node.id || !matches!(other.kind, NodeKind::File) {
            continue;
        }
        if let Some((other_node, other_content)) =
            runtime.workspace().read(runtime.id(), &other.id).await?
            && extract_wikilinks(&other_content)
                .iter()
                .any(|link| link == &target)
        {
            backlinks.push(FsNodeGql::from(other_node));
        }
    }

    Ok(Some(WorkspaceFileGql {
        id: ID(node.id),
        name: node.name,
        content,
        updated_at: iso8601(node.updated_at_millis),
        backlinks,
    }))
}
