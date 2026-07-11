//! The [`FeedbackStore`]: durable per-company persistence for feedback items.
//!
//! Mirrors the append-only JSONL pattern the runtime journal and memory store
//! use. Items live under the company bundle's `feedback/items.jsonl` and persist
//! whether or not they are ever filed. Status updates (issue URL + status) are
//! applied by rewriting the log atomically, so the closing-the-loop poller can
//! record where a filed issue stands.

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::feedback::types::FeedbackItem;
use crate::ports::generate_id;
use crate::store::paths::Bundle;

/// A per-company append-only store of [`FeedbackItem`]s.
pub struct FeedbackStore {
    path: PathBuf,
    write_lock: TokioMutex<()>,
}

impl FeedbackStore {
    /// Creates a store writing to `<bundle>/feedback/items.jsonl`.
    pub fn new(bundle: &Bundle) -> Self {
        Self {
            path: bundle.feedback_items_jsonl(),
            write_lock: TokioMutex::new(()),
        }
    }

    /// Appends a feedback item to the log.
    pub async fn append(&self, item: &FeedbackItem) -> Result<()> {
        let line = serde_json::to_string(item)?;
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| self.io_err(parent.to_path_buf(), e))?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| self.io_err(self.path.clone(), e))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| self.io_err(self.path.clone(), e))?;
        file.write_all(b"\n")
            .await
            .map_err(|e| self.io_err(self.path.clone(), e))?;
        Ok(())
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
        let _guard = self.write_lock.lock().await;
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

    fn tmp_bundle() -> Bundle {
        let root = std::env::temp_dir().join(format!("oc-feedback-{}", generate_id()));
        Bundle::new(root, &CompanyId::new("acme"))
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
        let bundle = tmp_bundle();
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
        let bundle = tmp_bundle();
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
}
