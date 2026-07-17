//! Filesystem backends for the WS3 console ports: tasks, facts, usage,
//! skill-state, the workspace file tree, and the human user directory.
//!
//! Each store owns a small file (or subtree) inside the company [`Bundle`]:
//!
//! - tasks → `tasks.json` (the whole board as a JSON array)
//! - users → `users.json`, invites → `user-invites.json`
//! - sessions → `user-sessions.json`, login codes → `login-codes.json`
//!   (credential material: token/code *hashes* only, never plaintext)
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
use crate::ports::login_codes::{LoginCodeRecord, LoginCodeStore};
use crate::ports::now_millis;
use crate::ports::sessions::{SessionRecord, SessionStore};
use crate::ports::skills_state::{SkillState, SkillStateStore};
use crate::ports::tasks::{TaskRecord, TaskStore};
use crate::ports::types::CompanyId;
use crate::ports::usage::{UsageMeter, UsageSample, retention_cutoff};
use crate::ports::users::{InviteRecord, UserRecord, UserStore};
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
// UserStore
// ---------------------------------------------------------------------------

#[async_trait]
impl UserStore for FsOps {
    async fn list_users(&self, company: &CompanyId) -> Result<Vec<UserRecord>> {
        let mut users = load_json_vec::<UserRecord>(&self.bundle(company).users_json()).await?;
        users.sort_by_key(|u| std::cmp::Reverse(u.created_at_millis));
        Ok(users)
    }

    async fn get_user(&self, company: &CompanyId, id: &str) -> Result<Option<UserRecord>> {
        let users = load_json_vec::<UserRecord>(&self.bundle(company).users_json()).await?;
        Ok(users.into_iter().find(|u| u.id == id))
    }

    async fn find_user_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<UserRecord>> {
        let users = load_json_vec::<UserRecord>(&self.bundle(company).users_json()).await?;
        // Exact match: normalization is the caller's job, so that a store never
        // silently matches an address the caller did not ask for.
        Ok(users.into_iter().find(|u| u.email == email))
    }

    async fn upsert_user(&self, company: &CompanyId, user: &UserRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.users_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut users = load_json_vec::<UserRecord>(&path).await?;
        // Email is unique per company: a second id holding one address would
        // make find_user_by_email ambiguous and let one mailbox own two
        // accounts. The lock makes this check-and-write a single step.
        if users
            .iter()
            .any(|u| u.email == user.email && u.id != user.id)
        {
            return Err(OpenCompanyError::Conflict(format!(
                "another user already has the email {}",
                user.email
            )));
        }
        match users.iter_mut().find(|u| u.id == user.id) {
            Some(existing) => *existing = user.clone(),
            None => users.push(user.clone()),
        }
        write_atomic(&path, &serde_json::to_string(&users)?).await
    }

    async fn delete_user(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).users_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut users = load_json_vec::<UserRecord>(&path).await?;
        let before = users.len();
        users.retain(|u| u.id != id);
        if users.len() == before {
            return Ok(false);
        }
        write_atomic(&path, &serde_json::to_string(&users)?).await?;
        Ok(true)
    }

    async fn list_invites(&self, company: &CompanyId) -> Result<Vec<InviteRecord>> {
        let mut invites =
            load_json_vec::<InviteRecord>(&self.bundle(company).user_invites_json()).await?;
        invites.sort_by_key(|i| std::cmp::Reverse(i.created_at_millis));
        Ok(invites)
    }

    async fn find_invite_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<InviteRecord>> {
        let invites =
            load_json_vec::<InviteRecord>(&self.bundle(company).user_invites_json()).await?;
        Ok(invites.into_iter().find(|i| i.email == email))
    }

    async fn upsert_invite(&self, company: &CompanyId, invite: &InviteRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.user_invites_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut invites = load_json_vec::<InviteRecord>(&path).await?;
        if invites
            .iter()
            .any(|i| i.email == invite.email && i.id != invite.id)
        {
            return Err(OpenCompanyError::Conflict(format!(
                "{} is already invited",
                invite.email
            )));
        }
        match invites.iter_mut().find(|i| i.id == invite.id) {
            Some(existing) => *existing = invite.clone(),
            None => invites.push(invite.clone()),
        }
        write_atomic(&path, &serde_json::to_string(&invites)?).await
    }

    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).user_invites_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut invites = load_json_vec::<InviteRecord>(&path).await?;
        let before = invites.len();
        invites.retain(|i| i.id != id);
        if invites.len() == before {
            return Ok(false);
        }
        write_atomic(&path, &serde_json::to_string(&invites)?).await?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

#[async_trait]
impl SessionStore for FsOps {
    async fn create(&self, company: &CompanyId, session: &SessionRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.user_sessions_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut sessions = load_json_vec::<SessionRecord>(&path).await?;
        // A repeated token hash would mean the CSPRNG repeated (or a caller
        // reused a token). Refuse rather than overwrite a live session.
        if sessions.iter().any(|s| s.token_hash == session.token_hash) {
            return Err(OpenCompanyError::Conflict(
                "that session token already exists".to_string(),
            ));
        }
        sessions.push(session.clone());
        write_atomic(&path, &serde_json::to_string(&sessions)?).await
    }

    async fn find_by_token_hash(
        &self,
        company: &CompanyId,
        token_hash: &str,
    ) -> Result<Option<SessionRecord>> {
        let sessions =
            load_json_vec::<SessionRecord>(&self.bundle(company).user_sessions_json()).await?;
        Ok(sessions.into_iter().find(|s| s.token_hash == token_hash))
    }

    async fn list_for_user(
        &self,
        company: &CompanyId,
        user_id: &str,
    ) -> Result<Vec<SessionRecord>> {
        let mut sessions =
            load_json_vec::<SessionRecord>(&self.bundle(company).user_sessions_json()).await?;
        sessions.retain(|s| s.user_id == user_id);
        sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at_millis));
        Ok(sessions)
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).user_sessions_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut sessions = load_json_vec::<SessionRecord>(&path).await?;
        let before = sessions.len();
        sessions.retain(|s| s.id != id);
        if sessions.len() == before {
            return Ok(false);
        }
        write_atomic(&path, &serde_json::to_string(&sessions)?).await?;
        Ok(true)
    }

    async fn delete_for_user(&self, company: &CompanyId, user_id: &str) -> Result<u64> {
        let path = self.bundle(company).user_sessions_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut sessions = load_json_vec::<SessionRecord>(&path).await?;
        let before = sessions.len();
        sessions.retain(|s| s.user_id != user_id);
        let removed = (before - sessions.len()) as u64;
        if removed > 0 {
            write_atomic(&path, &serde_json::to_string(&sessions)?).await?;
        }
        Ok(removed)
    }

    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64> {
        let path = self.bundle(company).user_sessions_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut sessions = load_json_vec::<SessionRecord>(&path).await?;
        let before = sessions.len();
        sessions.retain(|s| s.is_live(now_millis));
        let removed = (before - sessions.len()) as u64;
        if removed > 0 {
            write_atomic(&path, &serde_json::to_string(&sessions)?).await?;
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// LoginCodeStore
// ---------------------------------------------------------------------------

#[async_trait]
impl LoginCodeStore for FsOps {
    async fn create(&self, company: &CompanyId, code: &LoginCodeRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.login_codes_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut codes = load_json_vec::<LoginCodeRecord>(&path).await?;
        codes.push(code.clone());
        write_atomic(&path, &serde_json::to_string(&codes)?).await
    }

    async fn latest_for_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<LoginCodeRecord>> {
        let codes =
            load_json_vec::<LoginCodeRecord>(&self.bundle(company).login_codes_json()).await?;
        Ok(codes
            .into_iter()
            .filter(|c| c.email == email)
            .max_by_key(|c| c.created_at_millis))
    }

    async fn consume(
        &self,
        company: &CompanyId,
        code_hash: &str,
        now_millis: u64,
    ) -> Result<Option<LoginCodeRecord>> {
        let path = self.bundle(company).login_codes_json();
        // The lock is what makes check-and-mark atomic, so two requests racing
        // on one code cannot both mint a session. This holds within a process;
        // the fs backend is single-process by construction (one bundle, one
        // host), which is the same assumption every other fs store makes.
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut codes = load_json_vec::<LoginCodeRecord>(&path).await?;
        let Some(code) = codes
            .iter_mut()
            .find(|c| c.code_hash == code_hash && c.is_redeemable(now_millis))
        else {
            return Ok(None);
        };
        code.consumed_at_millis = Some(now_millis);
        let consumed = code.clone();
        write_atomic(&path, &serde_json::to_string(&codes)?).await?;
        Ok(Some(consumed))
    }

    async fn delete_for_email(&self, company: &CompanyId, email: &str) -> Result<u64> {
        let path = self.bundle(company).login_codes_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut codes = load_json_vec::<LoginCodeRecord>(&path).await?;
        let before = codes.len();
        codes.retain(|c| c.email != email);
        let removed = (before - codes.len()) as u64;
        if removed > 0 {
            write_atomic(&path, &serde_json::to_string(&codes)?).await?;
        }
        Ok(removed)
    }

    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64> {
        let path = self.bundle(company).login_codes_json();
        let lock = self.locks.get(&path);
        let _guard = lock.lock().await;
        let mut codes = load_json_vec::<LoginCodeRecord>(&path).await?;
        let before = codes.len();
        codes.retain(|c| now_millis < c.expires_at_millis);
        let removed = (before - codes.len()) as u64;
        if removed > 0 {
            write_atomic(&path, &serde_json::to_string(&codes)?).await?;
        }
        Ok(removed)
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
    async fn conformance_user_store() {
        let root = tmp_root();
        conformance::assert_user_store(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_session_store() {
        let root = tmp_root();
        conformance::assert_session_store(Arc::new(FsOps::new(&root))).await;
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn conformance_login_code_store() {
        let root = tmp_root();
        conformance::assert_login_code_store(Arc::new(FsOps::new(&root))).await;
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
        // Qualified: `FsOps` implements `create` for the workspace, session, and
        // login-code ports, so the concrete receiver needs the trait named.
        WorkspaceStore::create(
            &ops,
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
        WorkspaceStore::create(
            &ops,
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
