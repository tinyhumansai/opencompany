//! `resolve`'s referent-rule tests: the workspace-node fixtures and
//! the `ScriptedStore` fake used to exercise every referent outcome
//! (split out of `avatar_tests.rs`).

use super::tests_formats::{gif, png};
use super::*;

// ——— resolve's referent rule ———————————————————————————————

fn binary_node(
    id: &str,
    parent: Option<&str>,
    mime: &str,
    origin: WorkspaceOrigin,
) -> WorkspaceNode {
    WorkspaceNode {
        name: format!("{id}.png"),
        id: id.to_string(),
        kind: NodeKind::File,
        parent_id: parent.map(str::to_string),
        updated_at_millis: 0,
        created_by: origin.clone(),
        updated_by: origin,
        mime: Some(mime.to_string()),
        size: None,
        sha256: None,
        adopted: false,
    }
}

fn folder_node(id: &str, name: &str) -> WorkspaceNode {
    WorkspaceNode {
        name: name.to_string(),
        id: id.to_string(),
        kind: NodeKind::Folder,
        parent_id: None,
        updated_at_millis: 0,
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    }
}

/// A scripted [`WorkspaceStore`] for exercising [`resolve`]'s referent
/// rule.
///
/// It answers exactly the reads `resolve` performs — the referent node and
/// its bytes, and the parent folder behind an immutability read — and
/// records the writes it performs, so a test can assert what a validated
/// copy became. Every other trait method is `unreachable!`: a test that
/// reaches one is resolving outside the branches it means to cover.
struct ScriptedStore {
    /// Referent id → (node, payload), served by both `read` and `read_bytes`.
    nodes: std::collections::HashMap<String, (WorkspaceNode, Vec<u8>)>,
    /// Folder id → node, for the parent read behind `avatar_node_is_immutable`.
    folders: std::collections::HashMap<String, WorkspaceNode>,
    /// The validated copies `create_binary` was asked to store.
    copies: std::sync::Mutex<Vec<(WorkspaceNode, Vec<u8>)>>,
}

impl ScriptedStore {
    fn new() -> Self {
        Self {
            nodes: std::collections::HashMap::new(),
            folders: std::collections::HashMap::new(),
            copies: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn with_node(mut self, node: WorkspaceNode, bytes: Vec<u8>) -> Self {
        self.nodes.insert(node.id.clone(), (node, bytes));
        self
    }

    fn with_folder(mut self, node: WorkspaceNode) -> Self {
        self.folders.insert(node.id.clone(), node);
        self
    }
}

#[async_trait::async_trait]
impl crate::ports::WorkspaceStore for ScriptedStore {
    async fn tree(&self, _company: &crate::ports::types::CompanyId) -> Result<Vec<WorkspaceNode>> {
        unreachable!("resolve does not list the tree")
    }
    async fn read(
        &self,
        _company: &crate::ports::types::CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, String)>> {
        if let Some((node, _)) = self.nodes.get(id) {
            return Ok(Some((node.clone(), String::new())));
        }
        Ok(self
            .folders
            .get(id)
            .map(|node| (node.clone(), String::new())))
    }

    async fn read_capped(
        &self,
        company: &crate::ports::types::CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<(WorkspaceNode, String, u64)>> {
        crate::ports::workspace::read_capped_by_reading(self, company, id, max_bytes).await
    }
    async fn write_with_revision(
        &self,
        _company: &crate::ports::types::CompanyId,
        _id: &str,
        _content: &str,
        _author: WorkspaceOrigin,
        _expected_updated_at: Option<u64>,
    ) -> Result<WorkspaceNode> {
        unreachable!("resolve does not write prose")
    }
    async fn create(
        &self,
        _company: &crate::ports::types::CompanyId,
        _node: &WorkspaceNode,
        _content: Option<&str>,
    ) -> Result<()> {
        unreachable!("resolve does not create prose")
    }
    async fn adopt_or_create_folder(
        &self,
        _company: &crate::ports::types::CompanyId,
        _parent: Option<&str>,
        name: &str,
        _origin: WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::FolderClaim> {
        Ok(crate::ports::workspace::FolderClaim::Created(folder_node(
            &format!("folder-{name}"),
            name,
        )))
    }
    async fn create_binary(
        &self,
        _company: &crate::ports::types::CompanyId,
        node: &WorkspaceNode,
        bytes: &[u8],
    ) -> Result<WorkspaceNode> {
        self.copies
            .lock()
            .expect("test double not poisoned")
            .push((node.clone(), bytes.to_vec()));
        Ok(node.clone())
    }
    async fn write_binary(
        &self,
        _company: &crate::ports::types::CompanyId,
        _id: &str,
        _bytes: &[u8],
        _mime: Option<&str>,
        _author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        unreachable!("resolve does not rewrite bytes")
    }
    async fn read_bytes(
        &self,
        _company: &crate::ports::types::CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
        Ok(self.nodes.get(id).map(|(node, bytes)| {
            (
                node.clone(),
                crate::ports::workspace::one_chunk(bytes.clone()),
            )
        }))
    }
    async fn rename_move(
        &self,
        _company: &crate::ports::types::CompanyId,
        _id: &str,
        _name: Option<&str>,
        _parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode> {
        unreachable!("resolve does not move nodes")
    }
    async fn swap_files(
        &self,
        _company: &crate::ports::types::CompanyId,
        _expected_id: Option<&str>,
        _replacement_id: &str,
        _name: &str,
    ) -> Result<Option<WorkspaceNode>> {
        unreachable!("resolve does not swap files")
    }
    async fn delete(&self, _company: &crate::ports::types::CompanyId, _id: &str) -> Result<bool> {
        unreachable!("resolve does not delete")
    }
    async fn is_empty(&self, _company: &crate::ports::types::CompanyId) -> Result<bool> {
        unreachable!("resolve does not ask whether the tree is empty")
    }
}

#[tokio::test]
async fn resolve_refuses_a_missing_referent() {
    let store = ScriptedStore::new();
    let company = crate::ports::types::CompanyId::new("e2e");
    let err = resolve(&store, &company, "blob:nope")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("isn't here any more"), "{err}");
}

/// `Mascot`, like `Tiny`, is resolved with no workspace lookup at all —
/// `resolve`'s early return passes any non-`Blob` variant straight through.
/// An empty `ScriptedStore` (every method `unreachable!`) proves it: reaching
/// any store method here would panic the test.
#[tokio::test]
async fn resolve_passes_a_mascot_reference_through_untouched() {
    let store = ScriptedStore::new();
    let company = crate::ports::types::CompanyId::new("e2e");
    let stored = resolve(&store, &company, "mascot:animated").await.unwrap();
    assert_eq!(stored, "mascot:animated");
}

/// The store's own byte count is refused before any payload is buffered.
#[tokio::test]
async fn resolve_refuses_a_referent_the_store_counts_over_the_ceiling() {
    let company = crate::ports::types::CompanyId::new("e2e");
    let mut node = binary_node("big", None, "image/png", WorkspaceOrigin::Operator);
    node.size = Some(MAX_AVATAR_BYTES as u64 + 1);
    let store = ScriptedStore::new().with_node(node, png(16, 16));
    let err = resolve(&store, &company, "blob:big")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("can't be an avatar"), "{err}");
}

/// A store that leaves `size` unset is still bounded: the stream itself is
/// re-checked while it buffers.
#[tokio::test]
async fn resolve_refuses_a_referent_whose_stream_exceeds_the_byte_ceiling() {
    let company = crate::ports::types::CompanyId::new("e2e");
    let store = ScriptedStore::new().with_node(
        binary_node("huge", None, "image/png", WorkspaceOrigin::Operator),
        vec![0u8; MAX_AVATAR_BYTES + 1],
    );
    let err = resolve(&store, &company, "blob:huge")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("can't be an avatar"), "{err}");
}

/// A `blob:` can name any binary this host holds, and only the avatar route
/// sniffs before storing — so a node whose declared type disagrees with its
/// bytes would render as one face from this path and another from the Files
/// tab. A node with no declared type has nothing to agree with and is
/// refused too.
#[tokio::test]
async fn resolve_refuses_a_referent_whose_stored_type_disagrees() {
    let company = crate::ports::types::CompanyId::new("e2e");
    let store = ScriptedStore::new().with_node(
        binary_node("mislabeled", None, "image/png", WorkspaceOrigin::Operator),
        gif(16, 16),
    );
    let err = resolve(&store, &company, "blob:mislabeled")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("can't be an avatar"), "{err}");

    let mut bare = binary_node("bare", None, "image/png", WorkspaceOrigin::Operator);
    bare.mime = None;
    let store = ScriptedStore::new().with_node(bare, png(16, 16));
    let err = resolve(&store, &company, "blob:bare")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("can't be an avatar"), "{err}");
}

/// The upload route's own node — Operator origin under `avatars/` — is
/// already a validated copy that nothing rewrites, so resolve returns it as
/// the stored reference and mints nothing.
#[tokio::test]
async fn resolve_leaves_an_in_folders_node_untouched() {
    let company = crate::ports::types::CompanyId::new("e2e");
    let store = ScriptedStore::new()
        .with_folder(folder_node("folder-avatars", AVATARS_FOLDER))
        .with_node(
            binary_node(
                "avatar-1",
                Some("folder-avatars"),
                "image/png",
                WorkspaceOrigin::Operator,
            ),
            png(16, 16),
        );
    let stored = resolve(&store, &company, "blob:avatar-1")
        .await
        .expect("a stored reference");
    assert_eq!(stored, "blob:avatar-1");
    assert!(store.copies.lock().expect("not poisoned").is_empty());
}

/// The referent rule behind the provenance check: an artifact node that a
/// `PATCH …/workspace/{node}` moved beneath a folder named `avatars` still
/// carries its writer's origin, so it is not a face this host validated and
/// resolve must copy the bytes rather than store a reference to a node a
/// republish could rewrite.
#[tokio::test]
async fn resolve_copies_a_moved_artifact_node_before_storing() {
    let company = crate::ports::types::CompanyId::new("e2e");
    let store = ScriptedStore::new()
        .with_folder(folder_node("folder-avatars", AVATARS_FOLDER))
        .with_node(
            binary_node(
                "artifact-1",
                Some("folder-avatars"),
                "image/png",
                WorkspaceOrigin::Agent {
                    id: "image-bot".to_string(),
                },
            ),
            png(16, 16),
        );
    let stored = resolve(&store, &company, "blob:artifact-1")
        .await
        .expect("a validated copy");
    assert_ne!(
        stored, "blob:artifact-1",
        "a moved artifact node must not be stored by reference"
    );
    let copies = store.copies.lock().expect("not poisoned");
    assert_eq!(copies.len(), 1, "exactly one validated copy");
    let (node, bytes) = &copies[0];
    assert_eq!(node.parent_id.as_deref(), Some("folder-avatars"));
    assert_eq!(node.created_by, WorkspaceOrigin::Operator);
    assert_eq!(bytes, &png(16, 16));
}

/// A validated referent that lives outside the avatars folder is copied in
/// rather than stored by reference — the same immutable-copy rule as the
/// moved-artifact case, without the hostile parent.
#[tokio::test]
async fn resolve_copies_a_referent_that_lives_outside_the_avatars_folder() {
    let company = crate::ports::types::CompanyId::new("e2e");
    let store = ScriptedStore::new()
        .with_folder(folder_node("folder-files", "files"))
        .with_node(
            binary_node(
                "elsewhere",
                Some("folder-files"),
                "image/png",
                WorkspaceOrigin::Operator,
            ),
            png(16, 16),
        );
    let stored = resolve(&store, &company, "blob:elsewhere")
        .await
        .expect("a validated copy");
    assert_ne!(stored, "blob:elsewhere");
    let copies = store.copies.lock().expect("not poisoned");
    assert_eq!(copies.len(), 1);
    let (node, _) = &copies[0];
    assert_eq!(node.parent_id.as_deref(), Some("folder-avatars"));
    assert_eq!(node.created_by, WorkspaceOrigin::Operator);
}
