//! Issue #327: the workspace announces its own writes.
//!
//! A note can change on several paths — the console's create / `PUT` /
//! rename-move / delete routes, an agent's `workspace_create` and
//! `workspace_write` tools (issue #551), the publish drain materializing a
//! deliverable (issue #552), the boot seeder — and before this nothing on the
//! company feed said so. The Workspace tab's own module docs named the gap: "a
//! write that lands while the tab is open is only visible on a refetch (refresh
//! button / window focus)".
//!
//! That was tolerable when the operator was the only writer. It stopped being
//! tolerable the moment agents could create notes and published deliverables
//! started landing in the tree, because then most of what appears in the
//! workspace appears while nobody is touching it.
//!
//! ## Why the store, and not the callers
//!
//! [`WorkspaceAnnouncer`] is a decorator over the company's [`WorkspaceStore`],
//! wrapped in once by [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) — the
//! same shape [`BoardAnnouncer`](crate::runtime::BoardAnnouncer) uses, and for
//! the same reason its module docs give: every writer already goes through this
//! port, so the announcement is emitted **once, where notes are actually
//! written**, rather than once per call site that happens to write one.
//!
//! That is the fix, not an implementation detail. An emit at each write site is
//! the same shape as the bug: correct only for the paths somebody remembered,
//! and silent for the next one added. Here a new writer cannot forget, because
//! it does not get a say.
//!
//! ## What it is not
//!
//! * **Not a stimulus.** The event is appended after the write and never fed
//!   into a cycle. A note that started work merely by existing would re-enter
//!   this store and announce again.
//! * **Not the tree's truth.** The store is. The frame says *something in the
//!   workspace changed*; a console reacts by re-reading the tree it already
//!   knows how to read. That is why a lost frame degrades to "one stale render"
//!   and never to wrong state — and why the frame carries no node name and no
//!   body.
//! * **Not a failure path.** Record-keeping never fails the work it records: an
//!   event log that refuses is logged and swallowed. A note that was written
//!   stays written.
//!
//! ## The boot burst, named rather than discovered
//!
//! Seeding a new company's workspace from its bundle calls [`create`] once per
//! node, and the system-root scaffold adds two more — so first boot emits a
//! burst of `opened` frames. Nothing is listening (the SSE stream is opened by
//! a console, which cannot be attached to a company that is still being built),
//! and each frame is [`Prunable`](crate::ports::events::RetentionClass), so the
//! burst costs a bounded number of journal lines once in a company's life. It
//! is left alone deliberately: suppressing it would need the seeder to reach
//! past the port it shares with every other writer, which is exactly the
//! per-caller special-casing this decorator exists to remove.
//!
//! [`create`]: WorkspaceStore::create

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId};
use crate::ports::workspace::{WorkspaceNode, WorkspaceStore};
use crate::runtime::board_events::{CHANGE_OPENED, CHANGE_REMOVED, CHANGE_UPDATED};

/// A [`WorkspaceStore`] that appends a
/// [`WorkspaceChanged`](CompanyEvent::WorkspaceChanged) to the company's event
/// log after any write that actually changed the tree.
///
/// Reads pass straight through. See the module docs for why this lives at the
/// store rather than at the callers.
pub struct WorkspaceAnnouncer {
    inner: Arc<dyn WorkspaceStore>,
    events: Arc<dyn EventLog>,
}

impl WorkspaceAnnouncer {
    /// Wraps `inner`, announcing onto `events`.
    pub fn new(inner: Arc<dyn WorkspaceStore>, events: Arc<dyn EventLog>) -> Self {
        Self { inner, events }
    }

    /// Appends one announcement, best-effort.
    ///
    /// A refusing event log is warned about and swallowed: the workspace write
    /// it describes has already succeeded and must not be undone (or reported
    /// as failed) because the *record* of it could not be filed.
    async fn announce(&self, company: &CompanyId, node_id: &str, change: &str) {
        let event = CompanyEvent::WorkspaceChanged {
            node_id: node_id.to_string(),
            change: change.to_string(),
        };
        if let Err(err) = self.events.append(company, event).await {
            tracing::warn!(
                company = %company,
                node = %node_id,
                change = %change,
                error = %err,
                "could not announce a workspace write"
            );
        }
    }

    /// The node with `id` as the tree holds it now, or `None`.
    ///
    /// Reads the tree rather than [`WorkspaceStore::read`] on purpose: `read`
    /// returns the note's whole body, and this only ever needs the name and the
    /// parent. A read that fails yields `None` rather than propagating — the
    /// caller uses it only to decide whether to speak, and refusing a write
    /// because the *prior* state could not be read would fail workspace writes
    /// for a diagnostic.
    async fn current(&self, company: &CompanyId, id: &str) -> Option<WorkspaceNode> {
        self.inner
            .tree(company)
            .await
            .ok()?
            .into_iter()
            .find(|node| node.id == id)
    }
}

#[async_trait]
impl WorkspaceStore for WorkspaceAnnouncer {
    /// Delegated, not defaulted (issue #665). This wrapper sits *outside* the
    /// quota decorator, so taking the trait's permissive default here would
    /// answer "yes" for every upload and hide the only implementation that knows
    /// the company's cap.
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

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        self.inner.is_empty(company).await
    }

    /// Writes through, then announces `updated`.
    ///
    /// Unconditional, unlike [`BoardAnnouncer`](crate::runtime::BoardAnnouncer)'s
    /// identical-re-save suppression: a `write` always advances the node's
    /// `updated_at_millis`, which is the revision token the agent tools' CAS
    /// compares against, so a write that stores the same bytes has still
    /// changed the node in the way that matters. Detecting "same body" would
    /// also cost a full read of every note on every save, to save a refetch.
    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        let node = self.inner.write(company, id, content, author).await?;
        self.announce(company, id, CHANGE_UPDATED).await;
        Ok(node)
    }

    /// Creates through, then announces `opened`.
    async fn create(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()> {
        self.inner.create(company, node, content).await?;
        self.announce(company, &node.id, CHANGE_OPENED).await;
        Ok(())
    }

    /// Claims through, and announces `opened` **only when the folder was
    /// actually minted** (issue #759).
    ///
    /// An adoption changed nothing: the folder was already standing, already in
    /// the tree the console last read, already announced when whoever created it
    /// created it. Announcing it again would tell an open Workspace tab that
    /// something appeared, on a publish that only walked past it — and the
    /// publish walk adopts on nearly every publish a company ever makes, so the
    /// noise would be the common case rather than an edge one.
    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::FolderClaim> {
        let claim = self
            .inner
            .adopt_or_create_folder(company, parent, name, origin)
            .await?;
        if let crate::ports::workspace::FolderClaim::Created(node) = &claim {
            self.announce(company, &node.id, CHANGE_OPENED).await;
        }
        Ok(claim)
    }

    /// A binary node appearing in the tree is the same event as a note
    /// appearing: something the operator did not type is now there to look at.
    /// An uploaded image or a published chart therefore reaches an open
    /// Workspace tab exactly like a note does, which is the whole point of
    /// announcing at the store instead of at the callers.
    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        bytes: &[u8],
    ) -> Result<WorkspaceNode> {
        let stamped = self.inner.create_binary(company, node, bytes).await?;
        self.announce(company, &node.id, CHANGE_OPENED).await;
        Ok(stamped)
    }

    /// Replaces a payload through, then announces `updated` — unconditionally,
    /// for the reason [`write`](Self::write) gives.
    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: crate::ports::workspace::WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        let node = self
            .inner
            .write_binary(company, id, bytes, mime, author)
            .await?;
        self.announce(company, id, CHANGE_UPDATED).await;
        Ok(node)
    }

    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
        self.inner.read_bytes(company, id).await
    }

    /// Renames/moves through, then announces `updated` — unless nothing moved.
    ///
    /// **A rename that changes nothing says nothing.** The port takes `None` for
    /// "leave this alone", so a call naming neither a new name nor a new parent
    /// cannot have changed the tree; and a rename to the name a node already
    /// carries is an ordinary console outcome (an operator opening the rename
    /// dialog and pressing save). Announcing either would put a frame on the
    /// feed for a tree that did not move — noise a console cannot distinguish
    /// from a real change.
    ///
    /// The prior state is read before the call, so the comparison is against
    /// what the tree actually held rather than against what the caller asked
    /// for.
    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode> {
        let previous = self.current(company, id).await;
        let node = self.inner.rename_move(company, id, name, parent).await?;
        let moved = previous
            .is_none_or(|before| before.name != node.name || before.parent_id != node.parent_id);
        if moved {
            self.announce(company, id, CHANGE_UPDATED).await;
        }
        Ok(node)
    }

    /// Promotes a staged file and emits the same logical changes the former
    /// delete-plus-rename sequence emitted, but only after the atomic store
    /// operation has decided its winner.
    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> Result<Option<WorkspaceNode>> {
        let promoted = self
            .inner
            .swap_files(company, expected_id, replacement_id, name)
            .await?;
        match &promoted {
            Some(node) => {
                // Only a republish retires a node. A first publish (issue #697)
                // supersedes nothing, so announcing a removal would name an id
                // that never existed and close a watcher's handle on a file it
                // never had open.
                if let Some(superseded) = expected_id {
                    self.announce(company, superseded, CHANGE_REMOVED).await;
                }
                self.announce(company, &node.id, CHANGE_UPDATED).await;
            }
            None => {
                // `create(_binary)` announced the private staging node as
                // opened; the losing swap consumed it, so close that loop.
                self.announce(company, replacement_id, CHANGE_REMOVED).await;
            }
        }
        Ok(promoted)
    }

    /// Deletes through, and announces only when a node was actually removed —
    /// a delete of an id the tree never held changed nothing.
    ///
    /// One frame for the node named, even when it was a folder and the store
    /// removed a whole subtree with it. The frame says *the tree moved*, and
    /// the console's answer to that is to re-read the tree, which discovers
    /// every descendant that went with it. A frame per descendant would be a
    /// burst saying the same thing N times.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let removed = self.inner.delete(company, id).await?;
        if removed {
            self.announce(company, id, CHANGE_REMOVED).await;
        }
        Ok(removed)
    }

    /// Forwards to `self.inner.delete_if_empty`, not the default trait method.
    /// The default re-derives the check from `self.tree()` + `self.delete()`,
    /// which would call back through this announcer as two separate steps and
    /// never reach whatever tighter guarantee the wrapped store actually
    /// implements — see the port doc. Same removed-only announce as `delete`.
    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let removed = self.inner.delete_if_empty(company, id).await?;
        if removed {
            self.announce(company, id, CHANGE_REMOVED).await;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::{EventSeq, StoredEvent};
    use crate::ports::workspace::{NodeKind, WorkspaceOrigin};
    use crate::store::FsOps;
    use futures::stream::BoxStream;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemLog {
        appended: Mutex<Vec<CompanyEvent>>,
    }

    #[async_trait]
    impl EventLog for MemLog {
        async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
            let mut got = self.appended.lock().unwrap();
            got.push(event);
            Ok(EventSeq::new(got.len() as u64))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    /// An event log that refuses every append, so the "record-keeping never
    /// fails the work it records" contract is tested rather than asserted.
    struct BrokenLog;

    #[async_trait]
    impl EventLog for BrokenLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> Result<EventSeq> {
            Err(crate::error::OpenCompanyError::Store(
                "log is down".to_string(),
            ))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    /// A real fs-backed tree behind the decorator, so it is exercised against a
    /// genuine `WorkspaceStore` rather than a stub that cannot tell a create
    /// from an overwrite.
    fn wired() -> (
        tempfile::TempDir,
        WorkspaceAnnouncer,
        Arc<MemLog>,
        CompanyId,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(MemLog::default());
        let store = WorkspaceAnnouncer::new(Arc::new(FsOps::new(dir.path())), log.clone());
        (dir, store, log, CompanyId::new("announcer-co"))
    }

    fn note(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind: NodeKind::File,
            parent_id: parent.map(str::to_string),
            updated_at_millis: 1,
            created_by: WorkspaceOrigin::Operator,
            updated_by: WorkspaceOrigin::Operator,
            mime: None,
            size: None,
            sha256: None,
            adopted: false,
        }
    }

    fn changes(log: &MemLog) -> Vec<(String, String)> {
        log.appended
            .lock()
            .unwrap()
            .iter()
            .map(|event| match event {
                CompanyEvent::WorkspaceChanged { node_id, change } => {
                    (node_id.clone(), change.clone())
                }
                other => panic!("expected a workspace announcement, got {other:?}"),
            })
            .collect()
    }

    /// The headline of #327: a note written by *anything* announces itself, and
    /// the first write of an id is an `opened` rather than an `updated`.
    #[tokio::test]
    async fn a_new_note_announces_that_it_opened() {
        let (_dir, store, log, co) = wired();
        store
            .create(&co, &note("n-1", "brief.md", None), Some("body"))
            .await
            .unwrap();

        assert_eq!(
            changes(&log),
            vec![("n-1".to_string(), "opened".to_string())]
        );
    }

    /// Issue #759: a folder claim announces `opened` when it **mints** the
    /// folder and says nothing when it adopts one.
    ///
    /// Both halves are load-bearing. Without the first, a published deliverable
    /// would appear in a tab that never learned its folder existed. Without the
    /// second, every publish into an established folder — which is nearly every
    /// publish a company ever makes — would put an `opened` frame on the feed
    /// for a node that has been sitting there for weeks.
    #[tokio::test]
    async fn a_folder_claim_announces_only_when_it_mints_the_folder() {
        let (_dir, store, log, co) = wired();

        let claim = store
            .adopt_or_create_folder(&co, None, "Agents", WorkspaceOrigin::Seed)
            .await
            .unwrap();
        assert!(claim.was_created());
        assert_eq!(
            changes(&log),
            vec![(claim.node().id.clone(), "opened".to_string())]
        );
        log.appended.lock().unwrap().clear();

        let adopted = store
            .adopt_or_create_folder(&co, None, "Agents", WorkspaceOrigin::Seed)
            .await
            .unwrap();
        assert!(!adopted.was_created());
        assert_eq!(adopted.node().id, claim.node().id);
        assert!(
            changes(&log).is_empty(),
            "adopting a folder that was already standing changed nothing"
        );
    }

    /// An overwrite announces an update — the frame an agent's `workspace_write`
    /// and the console's `PUT` both need, and the one the Workspace tab had no
    /// way to learn about at all.
    #[tokio::test]
    async fn overwriting_a_note_announces_an_update() {
        let (_dir, store, log, co) = wired();
        store
            .create(&co, &note("n-1", "brief.md", None), Some("body"))
            .await
            .unwrap();
        store
            .write(&co, "n-1", "new body", WorkspaceOrigin::Operator)
            .await
            .unwrap();

        assert_eq!(
            changes(&log),
            vec![
                ("n-1".to_string(), "opened".to_string()),
                ("n-1".to_string(), "updated".to_string()),
            ]
        );
    }

    /// A rename that renames announces; one that names no change is silent. A
    /// frame for a tree that did not move is noise a console cannot tell from a
    /// real change.
    #[tokio::test]
    async fn a_rename_announces_only_when_something_actually_moved() {
        let (_dir, store, log, co) = wired();
        store
            .create(&co, &note("n-1", "brief.md", None), Some("body"))
            .await
            .unwrap();
        log.appended.lock().unwrap().clear();

        // A no-op: the port reads `None` as "leave this alone".
        store.rename_move(&co, "n-1", None, None).await.unwrap();
        assert!(changes(&log).is_empty(), "a no-op rename must be silent");

        // A rename to the name it already carries is the same non-event — an
        // operator opening the dialog and pressing save.
        store
            .rename_move(&co, "n-1", Some("brief.md"), None)
            .await
            .unwrap();
        assert!(
            changes(&log).is_empty(),
            "renaming to the current name changed nothing"
        );

        store
            .rename_move(&co, "n-1", Some("launch.md"), None)
            .await
            .unwrap();
        assert_eq!(
            changes(&log),
            vec![("n-1".to_string(), "updated".to_string())]
        );
    }

    /// A removed note announces its removal, and deleting an id the tree never
    /// held announces nothing — it changed nothing.
    #[tokio::test]
    async fn a_deleted_note_announces_removal_and_an_absent_one_is_silent() {
        let (_dir, store, log, co) = wired();
        store
            .create(&co, &note("n-1", "brief.md", None), Some("body"))
            .await
            .unwrap();
        log.appended.lock().unwrap().clear();

        assert!(!store.delete(&co, "never-existed").await.unwrap());
        assert!(changes(&log).is_empty());

        assert!(store.delete(&co, "n-1").await.unwrap());
        assert_eq!(
            changes(&log),
            vec![("n-1".to_string(), "removed".to_string())]
        );
    }

    /// Deleting a folder is one frame for the folder, not one per descendant.
    /// The frame says the tree moved; the console re-reads the tree and
    /// discovers everything that went with it.
    #[tokio::test]
    async fn deleting_a_folder_announces_once_not_once_per_descendant() {
        let (_dir, store, log, co) = wired();
        let folder = WorkspaceNode {
            kind: NodeKind::Folder,
            ..note("f-1", "Specs", None)
        };
        store.create(&co, &folder, None).await.unwrap();
        store
            .create(&co, &note("n-1", "a.md", Some("f-1")), Some("a"))
            .await
            .unwrap();
        store
            .create(&co, &note("n-2", "b.md", Some("f-1")), Some("b"))
            .await
            .unwrap();
        log.appended.lock().unwrap().clear();

        assert!(store.delete(&co, "f-1").await.unwrap());
        assert_eq!(
            changes(&log),
            vec![("f-1".to_string(), "removed".to_string())],
            "a subtree removal is one frame, not a burst saying the same thing"
        );
    }

    /// Reads pass straight through and announce nothing — this decorator is
    /// about writes, and a tab that refetched on every read would loop.
    #[tokio::test]
    async fn reads_announce_nothing() {
        let (_dir, store, log, co) = wired();
        store
            .create(&co, &note("n-1", "brief.md", None), Some("body"))
            .await
            .unwrap();
        log.appended.lock().unwrap().clear();

        store.tree(&co).await.unwrap();
        store.read(&co, "n-1").await.unwrap();
        store.is_empty(&co).await.unwrap();
        assert!(changes(&log).is_empty());
    }

    /// Record-keeping never fails the work it records: a refusing event log
    /// leaves the write successful and the note in the tree.
    #[tokio::test]
    async fn a_refusing_event_log_does_not_fail_the_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceAnnouncer::new(Arc::new(FsOps::new(dir.path())), Arc::new(BrokenLog));
        let co = CompanyId::new("announcer-broken");

        store
            .create(&co, &note("n-1", "brief.md", None), Some("body"))
            .await
            .expect("the write succeeds even when the announcement cannot be filed");
        store
            .write(&co, "n-1", "revised", WorkspaceOrigin::Operator)
            .await
            .expect("and so does the overwrite");
        assert_eq!(store.tree(&co).await.unwrap().len(), 1);
        assert_eq!(store.read(&co, "n-1").await.unwrap().unwrap().1, "revised");
    }
}
