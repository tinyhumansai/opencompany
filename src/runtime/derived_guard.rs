//! The write guard on the `derived/` folder.
//!
//! Every ledger renders into `derived/<NAME>.md`, rewritten on the next write
//! to that ledger. An edit there therefore *lands and is then silently erased*,
//! which is the worst available outcome: the author believes it took, the file
//! reads correctly for a while, and nothing anywhere says what happened. So the
//! folder is refused at the port, and the refusal names the tool that actually
//! writes the row.
//!
//! # Why a decorator rather than a check in each route
//!
//! Because there are six ways into this tree — the console routes, the agent
//! file tools, the publish drain, the seeder, an import, a workflow node — and
//! a rule enforced in five of them is not enforced. The same argument
//! [`QuotaEnforcedWorkspace`](super::QuotaEnforcedWorkspace) and
//! [`WorkspaceAnnouncer`](super::WorkspaceAnnouncer) already make: wrap the
//! port once, at the single place the store is chosen, and every writer obeys
//! without knowing it does.
//!
//! # The one way through
//!
//! [`WorkspaceOrigin::Seed`] passes. That is not a hole — it is the runtime's
//! own derivation, which is the only thing that may write these files and is
//! the only caller that stamps `Seed` at this point in a company's life
//! (`derived::publish` sets it deliberately; the console stamps `Operator` and
//! an agent stamps `Agent`). An identity check against a caller would need a
//! principal the port does not have.
//!
//! # What is *not* refused, and why
//!
//! **Deleting** a derived file, and deleting the folder. A delete is not the
//! failure this guard exists to prevent: nothing is silently lost, the next
//! write to that ledger re-derives the file, and a ledger that was retired
//! leaves a stale file somebody has to be able to clear. Refusing it would
//! trade a visible, recoverable act for an unremovable one.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ledger::Registry;
use crate::ledger::spec::DERIVED_DIR;
use crate::ports::ledgers::LedgerStore;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{
    BlobStream, FolderClaim, NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore,
};

/// Wraps a [`WorkspaceStore`] and refuses hand-written edits under `derived/`.
pub struct DerivedGuardWorkspace {
    inner: Arc<dyn WorkspaceStore>,
    /// Read only to *compose the refusal*, and only once a write has already
    /// been found to target the folder. The guard itself is a path question and
    /// does not need it — but a refusal that cannot name the ledger's real
    /// write path sends its caller to a tool that refuses them a second time,
    /// which is barely better than refusing them with nothing.
    ledgers: Arc<dyn LedgerStore>,
}

impl DerivedGuardWorkspace {
    /// Wraps `inner`, naming refusals from `ledgers`.
    pub fn new(inner: Arc<dyn WorkspaceStore>, ledgers: Arc<dyn LedgerStore>) -> Self {
        Self { inner, ledgers }
    }

    /// The refusal for `path`, with the owning ledger named when one owns it.
    async fn refuse(&self, company: &CompanyId, path: &str) -> OpenCompanyError {
        let registry = match self.ledgers.list_specs(company).await {
            Ok(declared) => Registry::build(declared),
            // A store that cannot answer still gets a refusal — the guard is
            // the point and the ledger's name is the nicety.
            Err(_) => Registry::build([]),
        };
        OpenCompanyError::InvalidRequest(crate::ledger::derived::refusal(&registry, path))
    }

    /// The id of the `derived/` folder, if this company has one.
    async fn derived_folder(&self, company: &CompanyId) -> Result<Option<String>> {
        Ok(self
            .inner
            .tree(company)
            .await?
            .into_iter()
            .find(|node| {
                node.kind == NodeKind::Folder
                    && node.parent_id.is_none()
                    && node.name.eq_ignore_ascii_case(DERIVED_DIR)
            })
            .map(|node| node.id))
    }

    /// Whether `id` is the derived folder or a node inside it, and the path to
    /// name in a refusal.
    async fn guarded_node(&self, company: &CompanyId, id: &str) -> Result<Option<String>> {
        let Some(folder) = self.derived_folder(company).await? else {
            return Ok(None);
        };
        if id == folder {
            return Ok(Some(DERIVED_DIR.to_string()));
        }
        let tree = self.inner.tree(company).await?;
        let Some(node) = tree.iter().find(|node| node.id == id) else {
            return Ok(None);
        };
        if node.parent_id.as_deref() == Some(folder.as_str()) {
            return Ok(Some(format!("{DERIVED_DIR}/{}", node.name)));
        }
        Ok(None)
    }

    /// Whether a new node under `parent` named `name` lands in the folder.
    async fn guarded_target(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
    ) -> Result<Option<String>> {
        // A root-level node called `derived` is the folder itself: claiming it
        // by hand is how somebody would otherwise get a writable one beside the
        // real one.
        if parent.is_none() && name.eq_ignore_ascii_case(DERIVED_DIR) {
            return Ok(Some(DERIVED_DIR.to_string()));
        }
        let Some(folder) = self.derived_folder(company).await? else {
            return Ok(None);
        };
        if parent == Some(folder.as_str()) {
            return Ok(Some(format!("{DERIVED_DIR}/{name}")));
        }
        Ok(None)
    }
}

#[async_trait]
impl WorkspaceStore for DerivedGuardWorkspace {
    async fn admit_upload(&self, company: &CompanyId, name: &str, len: u64) -> Result<()> {
        self.inner.admit_upload(company, name, len).await
    }

    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
        self.inner.tree(company).await
    }

    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>> {
        self.inner.read(company, id).await
    }

    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<(WorkspaceNode, String, u64)>> {
        self.inner.read_capped(company, id, max_bytes).await
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        if author != WorkspaceOrigin::Seed
            && let Some(path) = self.guarded_node(company, id).await?
        {
            return Err(self.refuse(company, &path).await);
        }
        self.inner.write(company, id, content, author).await
    }

    async fn create(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()> {
        if node.created_by != WorkspaceOrigin::Seed
            && let Some(path) = self
                .guarded_target(company, node.parent_id.as_deref(), &node.name)
                .await?
        {
            return Err(self.refuse(company, &path).await);
        }
        self.inner.create(company, node, content).await
    }

    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: WorkspaceOrigin,
    ) -> Result<FolderClaim> {
        if origin != WorkspaceOrigin::Seed
            && let Some(path) = self.guarded_target(company, parent, name).await?
        {
            return Err(self.refuse(company, &path).await);
        }
        self.inner
            .adopt_or_create_folder(company, parent, name, origin)
            .await
    }

    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        bytes: &[u8],
    ) -> Result<WorkspaceNode> {
        if let Some(path) = self
            .guarded_target(company, node.parent_id.as_deref(), &node.name)
            .await?
        {
            // No `Seed` exemption: a derived file is Markdown a renderer wrote,
            // and nothing in the runtime puts bytes here.
            return Err(self.refuse(company, &path).await);
        }
        self.inner.create_binary(company, node, bytes).await
    }

    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        if let Some(path) = self.guarded_node(company, id).await? {
            return Err(self.refuse(company, &path).await);
        }
        self.inner
            .write_binary(company, id, bytes, mime, author)
            .await
    }

    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, BlobStream)>> {
        self.inner.read_bytes(company, id).await
    }

    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode> {
        // Both ends. Moving a derived file out would strand a file the next
        // derivation immediately recreates; moving an ordinary note *in* would
        // put a hand-written file in the folder whose whole meaning is that
        // nothing in it is hand-written.
        if let Some(path) = self.guarded_node(company, id).await? {
            return Err(self.refuse(company, &path).await);
        }
        if let Some(target) = parent
            && let Some(path) = self
                .guarded_target(company, target, name.unwrap_or_default())
                .await?
        {
            return Err(self.refuse(company, &path).await);
        }
        self.inner.rename_move(company, id, name, parent).await
    }

    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> Result<Option<WorkspaceNode>> {
        if let Some(expected) = expected_id
            && let Some(path) = self.guarded_node(company, expected).await?
        {
            return Err(self.refuse(company, &path).await);
        }
        self.inner
            .swap_files(company, expected_id, replacement_id, name)
            .await
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        // Deliberately unguarded. See the module docs.
        self.inner.delete(company, id).await
    }

    /// Forwards to `self.inner.delete_if_empty`, deliberately unguarded like
    /// `delete` above, and deliberately NOT the default trait method — that
    /// default would resolve `tree()`/`delete()` back through this decorator
    /// as two separate calls and lose whatever tighter guarantee the wrapped
    /// store provides. See the port doc.
    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        self.inner.delete_if_empty(company, id).await
    }

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        self.inner.is_empty(company).await
    }
}

#[cfg(test)]
#[path = "derived_guard_test.rs"]
mod test;
