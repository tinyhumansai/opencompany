//! An in-memory [`WorkspaceStore`] that permits the broken state issue #759
//! repairs — test support only.
//!
//! # Why a double, when there is a real backend right there
//!
//! The `fs` backend cannot hold the tree this module exists to fix. Issue #665
//! made `FsOps::create` and `FsOps::rename_move` refuse a node whose rendered
//! path is already taken (`reject_path_collision`), so two sibling folders
//! sharing a name are unreachable through it — the repair, and every test of the
//! repair, would be exercising a shape the backend forbids at the door.
//!
//! The backends hosted tenants actually run do not forbid it. sqlite and mongodb
//! key on node ids and accept two siblings with one name, which is precisely why
//! a publish race can leave that state behind there and not on a laptop. Both
//! are behind Cargo features that the default `cargo test` lane does not enable,
//! so pinning the repair against one of them would put the whole suite in a lane
//! that mostly does not run (issue #770).
//!
//! This double is therefore modelled on the *permissive* backends: it holds
//! nodes by id, and it does not second-guess names. It is deliberately not a
//! conformance-suite participant — it exists to make the damaged tree
//! constructible, nothing more.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{
    BlobStream, FolderClaim, NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore,
    existing_folder_claim, new_folder, one_chunk, rebind_binary, stamped_binary,
};

/// A permissive, company-scoped, in-memory workspace tree.
#[derive(Default)]
pub(crate) struct LooseWorkspace {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Every node, keyed by company — company A's nodes must stay invisible to
    /// company B here too, or a test could pass for the wrong reason.
    nodes: HashMap<String, Vec<WorkspaceNode>>,
    text: HashMap<String, String>,
    bytes: HashMap<String, Vec<u8>>,
    /// Fired once, immediately after the next successful `rename_move`. The
    /// hook edits the tree directly rather than calling back into the store,
    /// which is what lets a test open the race window between the repair's two
    /// reads without an `Arc` pointing at itself.
    #[allow(clippy::type_complexity)]
    after_move: Option<Box<dyn FnOnce(&mut Vec<WorkspaceNode>) + Send>>,
}

impl LooseWorkspace {
    /// Runs `hook` once, right after the next successful move.
    pub(crate) fn on_next_move(&self, hook: impl FnOnce(&mut Vec<WorkspaceNode>) + Send + 'static) {
        self.state.lock().expect("lock").after_move = Some(Box::new(hook));
    }

    /// Pushes nodes straight into `company`'s tree, bypassing the parent-exists
    /// check [`create`](Self::create) applies.
    ///
    /// A lawful create or move refuses a node whose parent is absent, so a
    /// **dangling-parent** node — the Race-2 orphan issue #1839's reaper is aimed
    /// at — is otherwise unconstructible through this double, exactly as a
    /// duplicate sibling name is (which is why the double exists at all). Test
    /// support only.
    pub(crate) fn inject(&self, company: &CompanyId, nodes: Vec<WorkspaceNode>) {
        self.with(company, |state, key| state.tree(&key).extend(nodes));
    }

    fn with<T>(&self, company: &CompanyId, f: impl FnOnce(&mut State, String) -> T) -> T {
        let key = company.to_string();
        let mut state = self.state.lock().expect("lock");
        f(&mut state, key)
    }
}

impl State {
    fn tree(&mut self, company: &str) -> &mut Vec<WorkspaceNode> {
        self.nodes.entry(company.to_string()).or_default()
    }
}

/// Every node beneath `id`, plus `id` itself.
fn subtree(nodes: &[WorkspaceNode], id: &str) -> Vec<String> {
    let mut out = vec![id.to_string()];
    let mut frontier = vec![id.to_string()];
    // Bounded by the node count: a parent cycle must not spin here either.
    for _ in 0..nodes.len() {
        let mut next = Vec::new();
        for node in nodes {
            if let Some(parent) = node.parent_id.as_deref()
                && frontier.iter().any(|id| id == parent)
                && !out.contains(&node.id)
            {
                out.push(node.id.clone());
                next.push(node.id.clone());
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

#[async_trait]
impl WorkspaceStore for LooseWorkspace {
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
        Ok(self.with(company, |state, key| state.tree(&key).clone()))
    }

    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>> {
        Ok(self.with(company, |state, key| {
            let node = state.tree(&key).iter().find(|n| n.id == id).cloned()?;
            let body = state.text.get(id).cloned().unwrap_or_default();
            Some((node, body))
        }))
    }

    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<(WorkspaceNode, String, u64)>> {
        crate::ports::workspace::read_capped_by_reading(self, company, id, max_bytes).await
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        self.with(company, |state, key| {
            let node = state
                .tree(&key)
                .iter_mut()
                .find(|n| n.id == id)
                .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("workspace node {id}")))?;
            if node.kind != NodeKind::File || node.is_binary() {
                return Err(OpenCompanyError::InvalidRequest(
                    "only a prose file can be written as text".to_string(),
                ));
            }
            node.updated_at_millis = now_millis();
            node.updated_by = author;
            let node = node.clone();
            state.text.insert(id.to_string(), content.to_string());
            Ok(node)
        })
    }

    async fn create(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()> {
        self.with(company, |state, key| {
            let tree = state.tree(&key);
            if tree.iter().any(|n| n.id == node.id) {
                return Err(OpenCompanyError::Conflict(format!(
                    "workspace node {} already exists",
                    node.id
                )));
            }
            if let Some(parent) = node.parent_id.as_deref()
                && tree
                    .iter()
                    .find(|n| n.id == parent)
                    .map(|n| n.kind)
                    .is_none_or(|kind| kind != NodeKind::Folder)
            {
                return Err(OpenCompanyError::InvalidRequest(
                    "target parent is not a folder".to_string(),
                ));
            }
            // No name check, on purpose — see the module docs.
            tree.push(node.clone());
            if let Some(content) = content {
                state.text.insert(node.id.clone(), content.to_string());
            }
            Ok(())
        })
    }

    /// Atomic here for the same reason it is atomic on the real backends: the
    /// whole read-then-write sits inside the one lock that guards this tree, so
    /// no other caller can slip a folder in between the look and the mint.
    ///
    /// This double is permissive about names on [`create`](Self::create) — that
    /// is the point of it — but the claim path is **not**, and deliberately so.
    /// A duplicate a test built by hand must still refuse a claim, because a
    /// tree that lost the race before the guard existed is exactly the tree this
    /// module repairs, and gaining a third node on it would be the guard making
    /// the damage worse. The refusals come from `existing_folder_claim`, so they
    /// read identically to fs, sqlite and mongodb.
    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: WorkspaceOrigin,
    ) -> Result<FolderClaim> {
        self.with(company, |state, key| {
            let tree = state.tree(&key);
            if let Some(existing) = existing_folder_claim(tree.iter(), parent, name)? {
                return Ok(FolderClaim::Adopted(existing));
            }
            let folder = new_folder(name, parent, origin);
            tree.push(folder.clone());
            Ok(FolderClaim::Created(folder))
        })
    }

    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        bytes: &[u8],
    ) -> Result<WorkspaceNode> {
        let stamped = stamped_binary(node, bytes)?;
        self.create(company, &stamped, None).await?;
        self.with(company, |state, _| {
            state.bytes.insert(stamped.id.clone(), bytes.to_vec());
        });
        Ok(stamped)
    }

    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        self.with(company, |state, key| {
            let node = state
                .tree(&key)
                .iter_mut()
                .find(|n| n.id == id)
                .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("workspace node {id}")))?;
            rebind_binary(node, bytes, mime, author)?;
            let node = node.clone();
            state.bytes.insert(id.to_string(), bytes.to_vec());
            Ok(node)
        })
    }

    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, BlobStream)>> {
        Ok(self.with(company, |state, key| {
            let node = state.tree(&key).iter().find(|n| n.id == id).cloned()?;
            if !node.is_binary() {
                return None;
            }
            let bytes = state.bytes.get(id).cloned().unwrap_or_default();
            Some((node, one_chunk(bytes)))
        }))
    }

    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode> {
        self.with(company, |state, key| {
            let tree = state.tree(&key);
            if !tree.iter().any(|n| n.id == id) {
                return Err(OpenCompanyError::CompanyNotFound(format!(
                    "workspace node {id}"
                )));
            }
            if let Some(Some(parent)) = parent {
                if parent == id || subtree(tree, id).iter().any(|inside| inside == parent) {
                    return Err(OpenCompanyError::InvalidRequest(
                        "cannot move a folder into its own subtree".to_string(),
                    ));
                }
                if tree
                    .iter()
                    .find(|n| n.id == parent)
                    .map(|n| n.kind)
                    .is_none_or(|kind| kind != NodeKind::Folder)
                {
                    return Err(OpenCompanyError::InvalidRequest(
                        "target parent is not a folder".to_string(),
                    ));
                }
            }
            let node = tree.iter_mut().find(|n| n.id == id).expect("node present");
            if let Some(name) = name {
                node.name = name.to_string();
            }
            if let Some(parent) = parent {
                node.parent_id = parent.map(str::to_string);
            }
            node.updated_at_millis = now_millis();
            let node = node.clone();
            if let Some(hook) = state.after_move.take() {
                hook(state.tree(&key));
            }
            Ok(node)
        })
    }

    async fn swap_files(
        &self,
        _company: &CompanyId,
        _expected_id: Option<&str>,
        _replacement_id: &str,
        _name: &str,
    ) -> Result<Option<WorkspaceNode>> {
        Err(OpenCompanyError::InvalidRequest(
            "the loose workspace double does not implement the publish compare-and-swap"
                .to_string(),
        ))
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        Ok(self.with(company, |state, key| {
            let tree = state.tree(&key);
            if !tree.iter().any(|n| n.id == id) {
                return false;
            }
            let doomed = subtree(tree, id);
            tree.retain(|node| !doomed.contains(&node.id));
            for id in doomed {
                state.text.remove(&id);
                state.bytes.remove(&id);
            }
            true
        }))
    }

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        Ok(self.with(company, |state, key| state.tree(&key).is_empty()))
    }
}
