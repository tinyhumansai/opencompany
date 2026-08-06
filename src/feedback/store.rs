//! The [`FeedbackStore`]: durable per-company persistence for feedback items.
//!
//! Mirrors the append-only JSONL pattern the runtime journal and memory store
//! use. Items live under the company bundle's `feedback/items.jsonl` and persist
//! whether or not they are ever filed. Status updates (issue URL + status) are
//! applied by rewriting the log atomically, so the closing-the-loop poller can
//! record where a filed issue stands.

use std::path::PathBuf;

use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::feedback::types::FeedbackItem;
use crate::ports::generate_id;
use crate::store::paths::Bundle;

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

    /// Records a filed issue's URL and status against an item, rewriting the log
    /// atomically. Closing-the-loop uses this to track status changes.
    pub async fn update_status(&self, id: &str, url: &str, status: &str) -> Result<()> {
        let lock = self.write_lock();
        let _guard = lock.lock().await;
        let mut items = self.list_unlocked().await?;
        for item in &mut items {
            if item.id == id {
                item.filed_issue_url = Some(url.to_string());
                item.issue_status = Some(status.to_string());
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

    async fn write_atomic(&self, contents: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| self.io_err(parent.to_path_buf(), e))?;
        }
        let tmp = self.path.with_extension(format!("tmp-{}", generate_id()));
        tokio::fs::write(&tmp, contents)
            .await
            .map_err(|e| self.io_err(tmp.clone(), e))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| self.io_err(self.path.clone(), e))
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
mod test {
    use super::*;
    use crate::feedback::types::{ConsentMode, FeedbackCategory, FeedbackInput, FeedbackItem};
    use crate::ports::types::CompanyId;

    /// The caller must hold the returned handle: it owns the bundle's root and
    /// removes it on drop.
    fn tmp_bundle() -> (tempfile::TempDir, Bundle) {
        let root = tempfile::Builder::new()
            .prefix("oc-feedback-")
            .tempdir()
            .expect("tempdir");
        let bundle = Bundle::new(root.path().to_path_buf(), &CompanyId::new("acme"));
        (root, bundle)
    }

    fn item(note: &str) -> FeedbackItem {
        FeedbackItem::capture(
            FeedbackInput {
                category: FeedbackCategory::Bug,
                note: note.into(),
                work_ref: None,
                template_name: None,
                template_version: None,
            },
            "0.1.0",
            ConsentMode::Manual,
        )
    }

    #[tokio::test]
    async fn append_and_list_round_trips() {
        let (_root, bundle) = tmp_bundle();
        let store = FeedbackStore::new(&bundle);
        assert!(store.list().await.unwrap().is_empty());

        store.append(&item("first")).await.unwrap();
        store.append(&item("second")).await.unwrap();
        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].operator_words, "first");
        assert_eq!(all[1].operator_words, "second");
        tokio::fs::remove_dir_all(bundle.dir()).await.ok();
    }

    #[tokio::test]
    async fn update_status_records_url_without_losing_others() {
        let (_root, bundle) = tmp_bundle();
        let store = FeedbackStore::new(&bundle);
        let a = item("a");
        let b = item("b");
        store.append(&a).await.unwrap();
        store.append(&b).await.unwrap();

        store
            .update_status(&b.id, "https://example/issues/7", "open")
            .await
            .unwrap();

        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 2);
        let updated = all.iter().find(|i| i.id == b.id).unwrap();
        assert_eq!(
            updated.filed_issue_url.as_deref(),
            Some("https://example/issues/7")
        );
        assert_eq!(updated.issue_status.as_deref(), Some("open"));
        // The other item is untouched.
        let other = all.iter().find(|i| i.id == a.id).unwrap();
        assert!(other.filed_issue_url.is_none());
        tokio::fs::remove_dir_all(bundle.dir()).await.ok();
    }

    /// Two feedback stores over one bundle must not erase each other's writes
    /// (issue #388).
    ///
    /// `update_status` is a read-modify-write: it reads the whole log, edits one
    /// item in memory, and rewrites the file atomically. An append that lands
    /// between that read and the rename is **gone** — the rewrite replaces the
    /// file with a snapshot taken before it existed.
    ///
    /// The lock that was supposed to prevent this was a bare
    /// `TokioMutex<()>` **field** on `FeedbackStore`, so it only ever serialised
    /// one instance against itself. `FeedbackStore::new(&bundle)` takes a bundle
    /// and builds a fresh mutex every time, so two call sites over one company —
    /// a console request handler and the closing-the-loop poller, say — hold two
    /// unrelated locks over one file and serialise against nothing. Keying the
    /// lock on the *path* in a process-wide registry is what makes the two
    /// instances meet.
    #[tokio::test]
    async fn two_feedback_stores_over_one_bundle_do_not_erase_each_others_writes() {
        let (_root, bundle) = tmp_bundle();
        // Two independently-constructed stores over the same bundle.
        let appender = std::sync::Arc::new(FeedbackStore::new(&bundle));
        let updater = std::sync::Arc::new(FeedbackStore::new(&bundle));

        // Seed one item so the updater has something to rewrite around.
        let seed = item("seed");
        updater.append(&seed).await.unwrap();

        const APPENDS: usize = 32;
        const UPDATES: usize = 8;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..APPENDS {
            let store = appender.clone();
            set.spawn(async move { store.append(&item(&format!("note-{i}"))).await });
        }
        for i in 0..UPDATES {
            let store = updater.clone();
            let id = seed.id.clone();
            set.spawn(async move {
                store
                    .update_status(&id, &format!("https://example/issues/{i}"), "open")
                    .await
            });
        }
        while let Some(res) = set.join_next().await {
            res.expect("task joins").expect("write succeeds");
        }

        let all = appender.list().await.expect("no corrupt lines");
        let mut notes: Vec<String> = all.iter().map(|i| i.operator_words.clone()).collect();
        notes.sort();
        let mut want: Vec<String> = (0..APPENDS).map(|i| format!("note-{i}")).collect();
        want.push("seed".to_string());
        want.sort();
        assert_eq!(
            notes, want,
            "a whole-file rewrite must not erase an append that raced it — every \
             item written by either instance has to survive"
        );

        // The update itself still landed, so serialising did not cost the write.
        let updated = all.iter().find(|i| i.id == seed.id).expect("seed survives");
        assert!(
            updated.filed_issue_url.is_some(),
            "the status update must be recorded"
        );
        assert_eq!(updated.issue_status.as_deref(), Some("open"));

        tokio::fs::remove_dir_all(bundle.dir()).await.ok();
    }

    /// Every appended item must occupy its own physical line.
    ///
    /// `append` used to write the record and its `\n` as two `write_all` calls on
    /// a `tokio::fs::File`, which buffers and can return before the kernel write
    /// lands — so a newline could be reordered or lost and two records shared one
    /// line, which `list` then rejects with a `serde_json` "trailing characters"
    /// error. That is what intermittently failed
    /// `update_status_records_url_without_losing_others` in CI. Mirrors
    /// `store::fs::test::concurrent_appends_stay_one_record_per_line` (PR #43).
    #[tokio::test]
    async fn concurrent_appends_stay_one_record_per_line() {
        let (_root, bundle) = tmp_bundle();
        let store = std::sync::Arc::new(FeedbackStore::new(&bundle));

        const N: usize = 32;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..N {
            let store = store.clone();
            set.spawn(async move { store.append(&item(&format!("note-{i}"))).await });
        }
        while let Some(res) = set.join_next().await {
            res.unwrap().expect("append succeeds");
        }

        // `list` parses every line: a merged record would fail here rather than
        // silently vanish.
        let all = store.list().await.expect("no corrupt lines");
        assert_eq!(all.len(), N, "every append is its own line");
        let mut notes: Vec<String> = all.into_iter().map(|i| i.operator_words).collect();
        notes.sort();
        let mut want: Vec<String> = (0..N).map(|i| format!("note-{i}")).collect();
        want.sort();
        assert_eq!(notes, want, "all records intact");

        tokio::fs::remove_dir_all(bundle.dir()).await.ok();
    }
}
