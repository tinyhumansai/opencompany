//! The [`WorkspaceStore`] port: the company's durable file tree.
//!
//! The workspace is an Obsidian-style tree of folders and Markdown notes the
//! operator organizes, edits, and links with `[[wiki links]]`. Node ids are
//! stable ULIDs, **not** paths, so a rename or move never breaks a reference.
//! The tree is seeded once from `companies/<name>/workspace/**` (WS1 walker)
//! and thereafter owned by the operator — deletions stick.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// Whether a workspace node is a folder or a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// A directory that may contain other nodes.
    Folder,
    /// A file with Markdown content.
    File,
}

/// One node in the workspace tree. `id` is a stable ULID; `parent_id` is `None`
/// at the workspace root.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceNode {
    /// Stable ULID id.
    pub id: String,
    /// Display name (including any extension).
    pub name: String,
    /// Whether this node is a folder or a file.
    pub kind: NodeKind,
    /// The parent folder's id, or `None` at the root.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Epoch-millis timestamp of the last update.
    pub updated_at_millis: u64,
}

/// Durable per-company workspace tree. Company A's files MUST be invisible to
/// company B.
#[async_trait]
pub trait WorkspaceStore: Send + Sync {
    /// Returns every node in the tree (order unspecified; callers build the
    /// tree from `parent_id`).
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>>;
    /// Reads one node and, for files, its content. Folders yield an empty body.
    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>>;
    /// Overwrites a file's content, returning the updated node. A folder id is
    /// an [`OpenCompanyError::InvalidRequest`](crate::error::OpenCompanyError).
    async fn write(&self, company: &CompanyId, id: &str, content: &str) -> Result<WorkspaceNode>;
    /// Creates a node (folder or file). The node's `id` must be fresh; the
    /// `parent_id`, when set, must name an existing folder. `content` seeds a
    /// file body.
    async fn create(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()>;
    /// Renames and/or reparents a node, returning the updated node. Moving a
    /// folder under its own descendant (a cycle) is rejected.
    ///
    /// `parent` distinguishes three intents: `None` leaves the parent
    /// unchanged, `Some(None)` moves the node to the workspace root, and
    /// `Some(Some(id))` reparents it under folder `id`.
    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode>;
    /// Deletes a node; folders are removed recursively. Returns whether a node
    /// was removed.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    /// Whether the workspace has no nodes — the gate the seeder checks so a
    /// seeded-then-emptied workspace is never re-seeded.
    async fn is_empty(&self, company: &CompanyId) -> Result<bool>;
}
