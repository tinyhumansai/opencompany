//! The [`FeedbackStore`]: durable per-company persistence for feedback items.
//!
//! Mirrors the append-only JSONL pattern the runtime journal and memory store
//! use. Items live under the company bundle's `feedback/items.jsonl` and persist
//! whether or not they are ever filed. Status updates (issue URL + status) are
//! applied by rewriting the log atomically, so the closing-the-loop poller can
//! record where a filed issue stands.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};

use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::feedback::types::FeedbackItem;
use crate::store::paths::Bundle;

/// Process-wide, per-item confirm locks.
///
/// Confirming one feedback item (Send after Preview) serialises on its id so
/// two concurrent confirms of the same item cannot both observe
/// `issue_status = None` and both file or forward: the loser blocks until the
/// winner records its status, then re-reads the item and returns the recorded
/// result. Keyed on the item id — not the store path — because the confirm
/// surface holds the lock across the whole send; the path-keyed registry in
/// `store::fs` (issue #388) is what the send's own status rewrite serialises
/// on, and the two must not collide. The key being the item id also means two
/// independently-constructed [`FeedbackStore`]s over one company meet here.
struct ConfirmLocks {
    inner: Arc<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
}

impl ConfirmLocks {
    fn get(&self, id: &str) -> Arc<TokioMutex<()>> {
        let mut map = self.inner.lock().expect("confirm-lock map poisoned");
        map.entry(id.to_string()).or_default().clone()
    }
}

static CONFIRM_LOCKS: LazyLock<ConfirmLocks> = LazyLock::new(|| ConfirmLocks {
    inner: Arc::new(StdMutex::new(HashMap::new())),
});

/// The process-wide serialisation point for confirming feedback item `id`.
pub(crate) fn confirm_lock(id: &str) -> Arc<TokioMutex<()>> {
    CONFIRM_LOCKS.get(id)
}

/// Whether the confirm-lock registry holds an entry for `id`. Test-only: used
/// to assert that a nonexistent confirm id never mints an un-evictable entry.
#[cfg(test)]
pub(crate) fn confirm_lock_holds(id: &str) -> bool {
    CONFIRM_LOCKS
        .inner
        .lock()
        .expect("confirm-lock map poisoned")
        .contains_key(id)
}

/// A per-company append-only store of [`FeedbackItem`]s.
///
/// Holds no lock of its own. Writes serialise on the process-wide, **path-keyed**
/// registry in `store::fs` (issue #388) — see `store::fs::path_lock` for the two
/// limits that registry accepts by construction (absolutising is not
/// canonicalising; a second process is outside any in-process lock's reach, and
/// write atomicity is what keeps that case safe).
///
/// It used to hold a bare `TokioMutex<()>` field, which is a weaker thing than
/// it looks: [`new`](Self::new) mints a fresh mutex per call, so two stores over
/// one bundle each held their own and serialised against nothing. That mattered
/// because [`update_status`](Self::update_status) is a read-modify-write — an
/// append landing between its read and its rename was erased outright.
pub struct FeedbackStore {
    path: PathBuf,
}

impl FeedbackStore {
    /// Creates a store writing to `<bundle>/feedback/items.jsonl`.
    pub fn new(bundle: &Bundle) -> Self {
        Self {
            path: bundle.feedback_items_jsonl(),
        }
    }

    /// The process-wide write lock for this store's log.
    fn write_lock(&self) -> std::sync::Arc<TokioMutex<()>> {
        crate::store::fs::path_lock(&self.path)
    }

    /// Appends a feedback item to the log.
    ///
    /// Delegates to [`append_line`](crate::store::fs::append_line), which writes
    /// the record **and** its newline in a single blocking `write_all` under
    /// `O_APPEND`. Writing them as two `write_all` calls on a `tokio::fs::File`
    /// — as this did — can surface as a `serde_json` "trailing characters" error
    /// from [`Self::list`]: tokio's async `File` buffers internally and may
    /// return before the kernel write lands, so the newline can be reordered or
    /// lost against a concurrent append and two records end up on one physical
    /// line. Identical to the corruption PR #43 removed from
    /// `store::fs::append_line`; the feedback store was the remaining twin.
    pub async fn append(&self, item: &FeedbackItem) -> Result<()> {
        let line = serde_json::to_string(item)?;
        let lock = self.write_lock();
        let _guard = lock.lock().await;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| self.io_err(parent.to_path_buf(), e))?;
        }
        crate::store::fs::append_line(&self.path, &line).await
    }

    /// Lists every stored feedback item, oldest first.
    pub async fn list(&self) -> Result<Vec<FeedbackItem>> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.io_err(self.path.clone(), e)),
        };
        let mut out = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line)?);
        }
        Ok(out)
    }

    /// Loads one stored feedback item by id.
    pub async fn get(&self, id: &str) -> Result<Option<FeedbackItem>> {
        Ok(self.list().await?.into_iter().find(|item| item.id == id))
    }

    /// Records a filed issue's URL and status against an item, rewriting the log
    /// atomically. Closing-the-loop uses this to track status changes.
    pub async fn update_status(&self, id: &str, url: &str, status: &str) -> Result<()> {
        self.update(id, |item| {
            item.filed_issue_url = Some(url.to_string());
            item.issue_status = Some(status.to_string());
        })
        .await
    }

    /// Freezes the byte-exact final body a preview produced on an item, so a
    /// later confirm of the same item posts exactly the bytes the operator
    /// approved (see [`super::service::finalize`]).
    pub async fn record_preview(&self, id: &str, body: &str) -> Result<()> {
        self.update(id, |item| item.scrubbed_body = Some(body.to_string()))
            .await
    }

    /// Applies `edit` to the stored item with `id` and rewrites the log
    /// atomically, serialising on the path key's write lock so an append cannot
    /// be erased by the read-modify-write (issue #388).
    async fn update<F>(&self, id: &str, mut edit: F) -> Result<()>
    where
        F: FnMut(&mut FeedbackItem),
    {
        let lock = self.write_lock();
        let _guard = lock.lock().await;
        let mut items = self.list_unlocked().await?;
        for item in &mut items {
            if item.id == id {
                edit(item);
            }
        }
        let mut body = String::new();
        for item in &items {
            body.push_str(&serde_json::to_string(item)?);
            body.push('\n');
        }
        self.write_atomic(&body).await
    }

    /// Reads the log without taking the write lock (callers already hold it).
    async fn list_unlocked(&self) -> Result<Vec<FeedbackItem>> {
        let contents = match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.io_err(self.path.clone(), e)),
        };
        let mut out = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line)?);
        }
        Ok(out)
    }

    /// The shared fs-backend write (temp file, fsync, rename, cancellation
    /// safe), rather than a weaker copy without the fsync.
    async fn write_atomic(&self, contents: &str) -> Result<()> {
        crate::store::fs::write_atomic(&self.path, contents).await
    }

    fn io_err(&self, path: PathBuf, source: std::io::Error) -> OpenCompanyError {
        OpenCompanyError::StoreIo { path, source }
    }
}

impl std::fmt::Debug for FeedbackStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedbackStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
