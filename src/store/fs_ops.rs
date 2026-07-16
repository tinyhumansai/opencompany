//! Filesystem backends for the WS3 console ports: tasks, facts, usage,
//! skill-state, and the workspace file tree.
//!
//! Each store owns a small file (or subtree) inside the company [`Bundle`]:
//!
//! - tasks → `tasks.json` (the whole board as a JSON array)
//! - facts → `facts.jsonl` (last-write-wins per id, rewritten on mutate)
//! - usage → `usage.jsonl` (append-only samples)
//! - skills → `skills.json` (operator deltas)
//! - workspace → real folders + Markdown files under `workspace/`, indexed by
//!   `.workspace-index.json` (ULID → node metadata; physical paths derive from
//!   the folder/name tree so a rename physically relocates the subtree)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::facts::{FactKind, FactRecord, FactStore};
use crate::ports::now_millis;
use crate::ports::skills_state::{SkillState, SkillStateStore};
use crate::ports::tasks::{TaskRecord, TaskStore};
use crate::ports::types::CompanyId;
use crate::ports::usage::{UsageMeter, UsageSample, retention_cutoff};
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceStore};
use crate::store::fs::{PathLocks, append_line, io_err, read_jsonl, read_optional, write_atomic};
use crate::store::paths::Bundle;

/// One filesystem store implementing every WS3 console port over a company
/// [`Bundle`]. A single `Arc<FsOps>` can be injected into each of the five
/// `RuntimeBuilder::with_*` setters.
#[derive(Clone)]
pub struct FsOps {
    root: PathBuf,
    locks: PathLocks,
}

impl FsOps {
    /// Creates an ops store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: PathLocks::default(),
        }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

// ---------------------------------------------------------------------------
// TaskStore
// ---------------------------------------------------------------------------

#[async_trait]
impl TaskStore for FsOps {
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>> {
        let mut tasks = load_json_vec::<TaskRecord>(&self.bundle(company).tasks_json()).await?;
        tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at_millis));
        Ok(tasks)
    }

    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.tasks_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut tasks = load_json_vec::<TaskRecord>(&path).await?;
        match tasks.iter_mut().find(|t| t.id == task.id) {
            Some(existing) => *existing = task.clone(),
            None => tasks.push(task.clone()),
        }
        write_atomic(&path, &serde_json::to_string(&tasks)?).await
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).tasks_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut tasks = load_json_vec::<TaskRecord>(&path).await?;
        let before = tasks.len();
        tasks.retain(|t| t.id != id);
        if tasks.len() == before {
            return Ok(false);
        }
        write_atomic(&path, &serde_json::to_string(&tasks)?).await?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// FactStore
// ---------------------------------------------------------------------------

#[async_trait]
impl FactStore for FsOps {
    async fn list(
        &self,
        company: &CompanyId,
        query: Option<&str>,
        kind: Option<FactKind>,
    ) -> Result<Vec<FactRecord>> {
        let mut facts =
            dedup_latest(read_jsonl::<FactRecord>(&self.bundle(company).facts_jsonl()).await?);
        if let Some(kind) = kind {
            facts.retain(|f| f.kind == kind);
        }
        if let Some(q) = query.map(str::to_lowercase).filter(|q| !q.is_empty()) {
            facts.retain(|f| {
                f.title.to_lowercase().contains(&q) || f.body.to_lowercase().contains(&q)
            });
        }
        facts.sort_by_key(|f| std::cmp::Reverse(f.updated_at_millis));
        Ok(facts)
    }

    async fn upsert(&self, company: &CompanyId, fact: &FactRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.facts_jsonl();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut facts = dedup_latest(read_jsonl::<FactRecord>(&path).await?);
        match facts.iter_mut().find(|f| f.id == fact.id) {
            Some(existing) => *existing = fact.clone(),
            None => facts.push(fact.clone()),
        }
        rewrite_jsonl(&path, &facts).await
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).facts_jsonl();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut facts = dedup_latest(read_jsonl::<FactRecord>(&path).await?);
        let before = facts.len();
        facts.retain(|f| f.id != id);
        if facts.len() == before {
            return Ok(false);
        }
        rewrite_jsonl(&path, &facts).await?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// UsageMeter
// ---------------------------------------------------------------------------

#[async_trait]
impl UsageMeter for FsOps {
    async fn record(&self, company: &CompanyId, sample: &UsageSample) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.usage_jsonl();
        let line = serde_json::to_string(sample)?;
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        append_line(&path, &line).await?;
        // Retention: compact `usage.jsonl` in place when it holds samples older
        // than the 90-day window. The cutoff anchors to the newest sample seen,
        // so a fresh write past the boundary evicts stale rows; a quiet company
        // (or small timestamps in tests) rewrites nothing.
        let samples = read_jsonl::<UsageSample>(&path).await?;
        let Some(newest) = samples.iter().map(|s| s.at_millis).max() else {
            return Ok(());
        };
        let cutoff = retention_cutoff(newest);
        if samples.iter().any(|s| s.at_millis < cutoff) {
            let kept: Vec<UsageSample> = samples
                .into_iter()
                .filter(|s| s.at_millis >= cutoff)
                .collect();
            rewrite_jsonl(&path, &kept).await?;
        }
        Ok(())
    }

    async fn query(&self, company: &CompanyId, since_millis: u64) -> Result<Vec<UsageSample>> {
        let mut samples = read_jsonl::<UsageSample>(&self.bundle(company).usage_jsonl()).await?;
        samples.retain(|s| s.at_millis >= since_millis);
        samples.sort_by_key(|s| s.at_millis);
        Ok(samples)
    }
}

// ---------------------------------------------------------------------------
// SkillStateStore
// ---------------------------------------------------------------------------

#[async_trait]
impl SkillStateStore for FsOps {
    async fn list(&self, company: &CompanyId) -> Result<Vec<SkillState>> {
        load_json_vec::<SkillState>(&self.bundle(company).skills_json()).await
    }

    async fn set(&self, company: &CompanyId, state: &SkillState) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.skills_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut states = load_json_vec::<SkillState>(&path).await?;
        match states.iter_mut().find(|s| s.slug == state.slug) {
            Some(existing) => *existing = state.clone(),
            None => states.push(state.clone()),
        }
        write_atomic(&path, &serde_json::to_string(&states)?).await
    }

    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool> {
        let path = self.bundle(company).skills_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut states = load_json_vec::<SkillState>(&path).await?;
        let before = states.len();
        states.retain(|s| s.slug != slug);
        if states.len() == before {
            return Ok(false);
        }
        write_atomic(&path, &serde_json::to_string(&states)?).await?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// WorkspaceStore
// ---------------------------------------------------------------------------

#[async_trait]
impl WorkspaceStore for FsOps {
    async fn tree(&self, company: &CompanyId) -> Result<Vec<WorkspaceNode>> {
        let index = self.load_index(company).await?;
        Ok(index.into_values().collect())
    }

    async fn read(&self, company: &CompanyId, id: &str) -> Result<Option<(WorkspaceNode, String)>> {
        let index = self.load_index(company).await?;
        let Some(node) = index.get(id).cloned() else {
            return Ok(None);
        };
        let content = if node.kind == NodeKind::File {
            let path = self.physical_path(company, &index, id)?;
            read_optional(&path).await?
        } else {
            String::new()
        };
        Ok(Some((node, content)))
    }

    async fn write(&self, company: &CompanyId, id: &str, content: &str) -> Result<WorkspaceNode> {
        let path = self.bundle(company).workspace_index_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        let node = index
            .get_mut(id)
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("workspace node {id}")))?;
        if node.kind != NodeKind::File {
            return Err(OpenCompanyError::InvalidRequest(
                "cannot write content to a folder".to_string(),
            ));
        }
        node.updated_at_millis = now_millis();
        let node = node.clone();
        let file = self.physical_path(company, &index, id)?;
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_err(parent, e))?;
        }
        tokio::fs::write(&file, content)
            .await
            .map_err(|e| io_err(&file, e))?;
        self.save_index(company, &index).await?;
        Ok(node)
    }

    async fn create(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        content: Option<&str>,
    ) -> Result<()> {
        reject_unsafe_name(&node.name)?;
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.workspace_index_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        if index.contains_key(&node.id) {
            return Err(OpenCompanyError::Conflict(format!(
                "workspace node {} already exists",
                node.id
            )));
        }
        if let Some(parent) = &node.parent_id {
            match index.get(parent) {
                Some(p) if p.kind == NodeKind::Folder => {}
                Some(_) => {
                    return Err(OpenCompanyError::InvalidRequest(
                        "parent is not a folder".to_string(),
                    ));
                }
                None => {
                    return Err(OpenCompanyError::InvalidRequest(
                        "parent folder does not exist".to_string(),
                    ));
                }
            }
        }
        index.insert(node.id.clone(), node.clone());
        let physical = self.physical_path(company, &index, &node.id)?;
        match node.kind {
            NodeKind::Folder => {
                tokio::fs::create_dir_all(&physical)
                    .await
                    .map_err(|e| io_err(&physical, e))?;
            }
            NodeKind::File => {
                if let Some(parent) = physical.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| io_err(parent, e))?;
                }
                tokio::fs::write(&physical, content.unwrap_or(""))
                    .await
                    .map_err(|e| io_err(&physical, e))?;
            }
        }
        self.save_index(company, &index).await
    }

    async fn rename_move(
        &self,
        company: &CompanyId,
        id: &str,
        name: Option<&str>,
        parent: Option<Option<&str>>,
    ) -> Result<WorkspaceNode> {
        if let Some(name) = name {
            reject_unsafe_name(name)?;
        }
        let path = self.bundle(company).workspace_index_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        if !index.contains_key(id) {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {id}"
            )));
        }
        // Reject cycles: a node cannot be reparented under itself or a descendant.
        // A move to root (`Some(None)`) never forms a cycle.
        if let Some(Some(parent)) = parent {
            if parent == id || descendants(&index, id).contains(parent) {
                return Err(OpenCompanyError::InvalidRequest(
                    "cannot move a folder into its own subtree".to_string(),
                ));
            }
            if index.get(parent).map(|p| p.kind) != Some(NodeKind::Folder) {
                return Err(OpenCompanyError::InvalidRequest(
                    "target parent is not a folder".to_string(),
                ));
            }
        }
        let old_physical = self.physical_path(company, &index, id)?;
        {
            let node = index.get_mut(id).expect("node present");
            if let Some(name) = name {
                node.name = name.to_string();
            }
            if let Some(parent) = parent {
                node.parent_id = parent.map(str::to_string);
            }
            node.updated_at_millis = now_millis();
        }
        let node = index.get(id).cloned().expect("node present");
        let new_physical = self.physical_path(company, &index, id)?;
        if old_physical != new_physical {
            if let Some(parent) = new_physical.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| io_err(parent, e))?;
            }
            if tokio::fs::try_exists(&old_physical).await.unwrap_or(false) {
                tokio::fs::rename(&old_physical, &new_physical)
                    .await
                    .map_err(|e| io_err(&new_physical, e))?;
            }
        }
        self.save_index(company, &index).await?;
        Ok(node)
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).workspace_index_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        if !index.contains_key(id) {
            return Ok(false);
        }
        let physical = self.physical_path(company, &index, id)?;
        let mut to_remove = descendants(&index, id);
        to_remove.insert(id.to_string());
        for node_id in &to_remove {
            index.remove(node_id);
        }
        if tokio::fs::try_exists(&physical).await.unwrap_or(false) {
            let meta = tokio::fs::symlink_metadata(&physical)
                .await
                .map_err(|e| io_err(&physical, e))?;
            if meta.is_dir() {
                tokio::fs::remove_dir_all(&physical)
                    .await
                    .map_err(|e| io_err(&physical, e))?;
            } else {
                tokio::fs::remove_file(&physical)
                    .await
                    .map_err(|e| io_err(&physical, e))?;
            }
        }
        self.save_index(company, &index).await?;
        Ok(true)
    }

    async fn is_empty(&self, company: &CompanyId) -> Result<bool> {
        Ok(self.load_index(company).await?.is_empty())
    }
}

impl FsOps {
    /// Loads the workspace index (`id` → node metadata).
    async fn load_index(&self, company: &CompanyId) -> Result<HashMap<String, WorkspaceNode>> {
        let path = self.bundle(company).workspace_index_json();
        let contents = read_optional(&path).await?;
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&contents)?)
    }

    /// Persists the workspace index.
    async fn save_index(
        &self,
        company: &CompanyId,
        index: &HashMap<String, WorkspaceNode>,
    ) -> Result<()> {
        let path = self.bundle(company).workspace_index_json();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_err(parent, e))?;
        }
        write_atomic(&path, &serde_json::to_string(index)?).await
    }

    /// The on-disk path of a node, derived from its ancestor folder names.
    fn physical_path(
        &self,
        company: &CompanyId,
        index: &HashMap<String, WorkspaceNode>,
        id: &str,
    ) -> Result<PathBuf> {
        let mut names = Vec::new();
        let mut cursor = Some(id.to_string());
        let mut guard = 0;
        while let Some(node_id) = cursor {
            let node = index.get(&node_id).ok_or_else(|| {
                OpenCompanyError::Store(format!("dangling workspace parent {node_id}"))
            })?;
            names.push(node.name.clone());
            cursor = node.parent_id.clone();
            guard += 1;
            if guard > 10_000 {
                return Err(OpenCompanyError::Store(
                    "workspace cycle detected".to_string(),
                ));
            }
        }
        names.reverse();
        let mut path = self.bundle(company).workspace_dir();
        for name in names {
            path.push(name);
        }
        Ok(path)
    }
}

/// Collects the ids of every descendant of `id` (excluding `id` itself).
fn descendants(index: &HashMap<String, WorkspaceNode>, id: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut frontier = vec![id.to_string()];
    while let Some(current) = frontier.pop() {
        for (child_id, node) in index {
            if node.parent_id.as_deref() == Some(current.as_str()) && out.insert(child_id.clone()) {
                frontier.push(child_id.clone());
            }
        }
    }
    out
}

/// Rejects a node name that contains a path separator or a parent-dir hop, so a
/// workspace write can never escape the `workspace/` root.
fn reject_unsafe_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." || name == "." {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "invalid workspace node name: {name:?}"
        )));
    }
    Ok(())
}

/// Reads a JSON array file into a `Vec<T>`, treating an absent/empty file as `[]`.
async fn load_json_vec<T>(path: &Path) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let contents = read_optional(path).await?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&contents)?)
}

/// Keeps the last record per id (last-write-wins), preserving first-seen order.
fn dedup_latest(records: Vec<FactRecord>) -> Vec<FactRecord> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, FactRecord> = HashMap::new();
    for record in records {
        if !by_id.contains_key(&record.id) {
            order.push(record.id.clone());
        }
        by_id.insert(record.id.clone(), record);
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// Rewrites a JSONL file from a slice of records (one JSON object per line).
async fn rewrite_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
where
    T: serde::Serialize,
{
    let mut body = String::new();
    for record in records {
        body.push_str(&serde_json::to_string(record)?);
        body.push('\n');
    }
    write_atomic(path, &body).await
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::facts::FactKind;
    use crate::ports::skills_state::SkillSource;
    use crate::ports::usage::SampleKind;
    use crate::store::conformance;
    use std::sync::Arc;

    fn tmp_root() -> PathBuf {
        std::env::temp_dir().join(format!("opencompany-fsops-{}", crate::ports::generate_id()))
    }

    #[tokio::test]
    async fn conformance_task_store() {
        let root = tmp_root();
        conformance::assert_task_store(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_fact_store() {
        let root = tmp_root();
        conformance::assert_fact_store(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_usage_meter() {
        let root = tmp_root();
        conformance::assert_usage_meter(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_usage_retention() {
        let root = tmp_root();
        conformance::assert_usage_retention(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_skill_state_store() {
        let root = tmp_root();
        conformance::assert_skill_state_store(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_workspace_store() {
        let root = tmp_root();
        conformance::assert_workspace_store(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn workspace_files_land_on_disk_under_folders() {
        let root = tmp_root();
        let ops = FsOps::new(&root);
        let company = CompanyId::new("acme");
        let now = now_millis();
        ops.create(
            &company,
            &WorkspaceNode {
                id: "f1".into(),
                name: "Brand".into(),
                kind: NodeKind::Folder,
                parent_id: None,
                updated_at_millis: now,
            },
            None,
        )
        .await
        .unwrap();
        ops.create(
            &company,
            &WorkspaceNode {
                id: "n1".into(),
                name: "voice.md".into(),
                kind: NodeKind::File,
                parent_id: Some("f1".into()),
                updated_at_millis: now,
            },
            Some("# Voice"),
        )
        .await
        .unwrap();
        let disk = root.join("companies/acme/workspace/Brand/voice.md");
        assert_eq!(tokio::fs::read_to_string(&disk).await.unwrap(), "# Voice");

        // A rename physically relocates the subtree.
        ops.rename_move(&company, "f1", Some("Branding"), None)
            .await
            .unwrap();
        let moved = root.join("companies/acme/workspace/Branding/voice.md");
        assert!(tokio::fs::try_exists(&moved).await.unwrap());
        assert!(!tokio::fs::try_exists(&disk).await.unwrap());

        let _ = (FactKind::Fact, SkillSource::Company, SampleKind::Inference);
        tokio::fs::remove_dir_all(&root).await.ok();
    }
}
