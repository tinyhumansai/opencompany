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
//! - runs → `runs.jsonl` (last-write-wins per id) + `run-steps.jsonl`
//!   (append-only trace, last-write-wins per `(run_id, step_seq)`)
//! - usage → `usage.jsonl` (append-only samples)
//! - skills → `skills.json` (operator deltas)
//! - workspace → real folders + Markdown files under `workspace/`, indexed by
//!   `.workspace-index.json` (ULID → node metadata; physical paths derive from
//!   the folder/name tree so a rename physically relocates the subtree). The
//!   filesystem store therefore refuses a write whose resolved path another
//!   node already holds: two ids may never alias one on-disk path (issue #666).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ledger::{LedgerEvent, LedgerSpec};
use crate::ports::artifacts::{ArtifactRecord, ArtifactStore};
use crate::ports::deep_trace::{DeepTraceStore, MAX_DEEP_RUNS_PER_COMPANY, RunStepDetailRecord};
use crate::ports::facts::{FactKind, FactRecord, FactStore};
use crate::ports::ledgers::LedgerStore;
use crate::ports::login_codes::{LoginCodeRecord, LoginCodeStore};
use crate::ports::now_millis;
use crate::ports::run_output::{
    MAX_RUN_OUTPUTS_PER_COMPANY, WorkflowRunOutputRecord, WorkflowRunOutputStore,
    sort_newest_first as sort_run_outputs_newest_first,
};
use crate::ports::runs::{
    NewRun, RunFilter, RunRecord, RunStatus, RunStepRecord, RunStore, sort_newest_first,
};
use crate::ports::sessions::{SessionRecord, SessionStore};
use crate::ports::skills_state::{SkillState, SkillStateStore};
use crate::ports::tasks::{TaskRecord, TaskStore};
use crate::ports::types::CompanyId;
use crate::ports::usage::{UsageMeter, UsageSample, retention_cutoff};
use crate::ports::users::{InviteRecord, UserRecord, UserStore};
use crate::ports::workflow_revisions::{
    MAX_WORKFLOW_REVISIONS, WorkflowRevisionRecord, WorkflowRevisionStore,
    sort_newest_first as sort_revisions_newest_first,
};
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};
use crate::store::fs::{
    append_line, io_err, path_lock, read_jsonl, read_optional, write_atomic, write_atomic_bytes,
};
use crate::store::paths::Bundle;

/// One filesystem store implementing every WS3 console port over a company
/// [`Bundle`]. A single `Arc<FsOps>` can be injected into each of the five
/// `RuntimeBuilder::with_*` setters.
#[derive(Clone)]
pub struct FsOps {
    root: PathBuf,
    /// Run ids known to live in each company's deep-trace file, so a compaction
    /// is triggered exactly when a new run pushes the count past the cap — not
    /// on every append (issue #1679). Seeded from disk on the first deep-trace
    /// write of a process and rebuilt after each compaction or purge.
    deep_runs: Arc<tokio::sync::Mutex<HashMap<PathBuf, HashSet<String>>>>,
}

impl FsOps {
    /// Creates an ops store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            deep_runs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
// LedgerStore
// ---------------------------------------------------------------------------

#[async_trait]
impl LedgerStore for FsOps {
    async fn list_specs(&self, company: &CompanyId) -> Result<Vec<LedgerSpec>> {
        load_json_vec::<LedgerSpec>(&self.bundle(company).ledgers_json()).await
    }

    async fn put_spec(&self, company: &CompanyId, spec: &LedgerSpec) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.ledgers_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut specs = load_json_vec::<LedgerSpec>(&path).await?;
        match specs.iter_mut().find(|held| held.slug == spec.slug) {
            Some(existing) => *existing = spec.clone(),
            None => specs.push(spec.clone()),
        }
        write_atomic(&path, &serde_json::to_string(&specs)?).await
    }

    async fn delete_spec(&self, company: &CompanyId, slug: &str) -> Result<bool> {
        let path = self.bundle(company).ledgers_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut specs = load_json_vec::<LedgerSpec>(&path).await?;
        let before = specs.len();
        specs.retain(|spec| spec.slug != slug);
        if specs.len() == before {
            return Ok(false);
        }
        // The event log is deliberately untouched. See `LedgerStore::delete_spec`.
        write_atomic(&path, &serde_json::to_string(&specs)?).await?;
        Ok(true)
    }

    async fn append(&self, company: &CompanyId, event: &LedgerEvent) -> Result<()> {
        let bundle = self.bundle(company);
        let dir = bundle.ledgers_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|source| io_err(&dir, source))?;
        // One `write_all` of one complete line under `O_APPEND`: concurrent
        // writers interleave whole lines and never halves of one, so no lock is
        // needed here at all.
        append_line(
            &bundle.ledger_events_jsonl(&event.ledger),
            &serde_json::to_string(event)?,
        )
        .await
    }

    async fn events(&self, company: &CompanyId, ledger: &str) -> Result<Vec<LedgerEvent>> {
        read_jsonl::<LedgerEvent>(&self.bundle(company).ledger_events_jsonl(ledger)).await
    }

    async fn purge_entry(&self, company: &CompanyId, ledger: &str, entry: &str) -> Result<bool> {
        let path = self.bundle(company).ledger_events_jsonl(ledger);
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let events = read_jsonl::<LedgerEvent>(&path).await?;
        let kept: Vec<&LedgerEvent> = events.iter().filter(|held| held.id != entry).collect();
        if kept.len() == events.len() {
            return Ok(false);
        }
        // Rewritten rather than tombstoned: this is the one operation that is
        // meant to leave nothing behind, and a tombstone that still carried the
        // row's text would make "deleted" mean "hidden from one renderer".
        let mut body = String::new();
        for event in kept {
            body.push_str(&serde_json::to_string(event)?);
            body.push('\n');
        }
        write_atomic(&path, &body).await?;
        Ok(true)
    }

    async fn purge_ledger(&self, company: &CompanyId, ledger: &str) -> Result<bool> {
        let path = self.bundle(company).ledger_events_jsonl(ledger);
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_err(&path, error)),
        }
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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

    async fn mark_invite_notified(
        &self,
        company: &CompanyId,
        id: &str,
        at_millis: u64,
    ) -> Result<bool> {
        let path = self.bundle(company).user_invites_json();
        // The same lock `delete_invite` takes, which is what makes the
        // read-modify-write atomic against a concurrent revocation rather than
        // merely unlikely to race with one.
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut invites = load_json_vec::<InviteRecord>(&path).await?;
        let Some(existing) = invites.iter_mut().find(|i| i.id == id) else {
            return Ok(false);
        };
        existing.notified_at_millis = Some(at_millis);
        write_atomic(&path, &serde_json::to_string(&invites)?).await?;
        Ok(true)
    }

    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).user_invites_json();
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
// ArtifactStore
// ---------------------------------------------------------------------------

#[async_trait]
impl ArtifactStore for FsOps {
    async fn list(
        &self,
        company: &CompanyId,
        task_id: Option<&str>,
    ) -> Result<Vec<ArtifactRecord>> {
        let mut artifacts = dedup_latest(
            read_jsonl::<ArtifactRecord>(&self.bundle(company).artifacts_jsonl()).await?,
        );
        if let Some(task_id) = task_id {
            artifacts.retain(|a| a.task_id == task_id);
        }
        artifacts.sort_by_key(|a| std::cmp::Reverse(a.updated_at_millis));
        Ok(artifacts)
    }

    async fn get(&self, company: &CompanyId, id: &str) -> Result<Option<ArtifactRecord>> {
        let artifacts = dedup_latest(
            read_jsonl::<ArtifactRecord>(&self.bundle(company).artifacts_jsonl()).await?,
        );
        Ok(artifacts.into_iter().find(|a| a.id == id))
    }

    async fn upsert(&self, company: &CompanyId, artifact: &ArtifactRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.artifacts_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut artifacts = dedup_latest(read_jsonl::<ArtifactRecord>(&path).await?);
        match artifacts.iter_mut().find(|a| a.id == artifact.id) {
            Some(existing) => *existing = artifact.clone(),
            None => artifacts.push(artifact.clone()),
        }
        rewrite_jsonl(&path, &artifacts).await
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).artifacts_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut artifacts = dedup_latest(read_jsonl::<ArtifactRecord>(&path).await?);
        let before = artifacts.len();
        artifacts.retain(|a| a.id != id);
        if artifacts.len() == before {
            return Ok(false);
        }
        rewrite_jsonl(&path, &artifacts).await?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// WorkflowRevisionStore
// ---------------------------------------------------------------------------

#[async_trait]
impl WorkflowRevisionStore for FsOps {
    async fn push_revision(
        &self,
        company: &CompanyId,
        revision: &WorkflowRevisionRecord,
    ) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.workflow_revisions_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut all = read_jsonl::<WorkflowRevisionRecord>(&path).await?;
        all.push(revision.clone());
        // Prune-to-cap for THIS workflow only, inside the lock so a reader never
        // sees a 21-deep ring. Other workflows' snapshots are untouched.
        prune_workflow_revisions(&mut all, &revision.workflow_id);
        rewrite_jsonl(&path, &all).await
    }

    async fn list_revisions(
        &self,
        company: &CompanyId,
        workflow_id: &str,
    ) -> Result<Vec<WorkflowRevisionRecord>> {
        let mut revs =
            read_jsonl::<WorkflowRevisionRecord>(&self.bundle(company).workflow_revisions_jsonl())
                .await?;
        revs.retain(|r| r.workflow_id == workflow_id);
        sort_revisions_newest_first(&mut revs);
        Ok(revs)
    }

    async fn get_revision(
        &self,
        company: &CompanyId,
        workflow_id: &str,
        revision_id: &str,
    ) -> Result<Option<WorkflowRevisionRecord>> {
        let revs =
            read_jsonl::<WorkflowRevisionRecord>(&self.bundle(company).workflow_revisions_jsonl())
                .await?;
        Ok(revs
            .into_iter()
            .find(|r| r.workflow_id == workflow_id && r.id == revision_id))
    }

    async fn delete_revisions(&self, company: &CompanyId, workflow_id: &str) -> Result<u64> {
        let path = self.bundle(company).workflow_revisions_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut revs = read_jsonl::<WorkflowRevisionRecord>(&path).await?;
        let before = revs.len();
        revs.retain(|r| r.workflow_id != workflow_id);
        let removed = (before - revs.len()) as u64;
        if removed > 0 {
            rewrite_jsonl(&path, &revs).await?;
        }
        Ok(removed)
    }
}

/// Trims `all` so the workflow named by `workflow_id` keeps at most
/// [`MAX_WORKFLOW_REVISIONS`] of its newest snapshots, leaving every other
/// workflow's rows in place and in their original file order.
fn prune_workflow_revisions(all: &mut Vec<WorkflowRevisionRecord>, workflow_id: &str) {
    let mut mine: Vec<WorkflowRevisionRecord> = all
        .iter()
        .filter(|r| r.workflow_id == workflow_id)
        .cloned()
        .collect();
    if mine.len() <= MAX_WORKFLOW_REVISIONS {
        return;
    }
    sort_revisions_newest_first(&mut mine);
    let keep: HashSet<String> = mine
        .into_iter()
        .take(MAX_WORKFLOW_REVISIONS)
        .map(|r| r.id)
        .collect();
    all.retain(|r| r.workflow_id != workflow_id || keep.contains(&r.id));
}

// ---------------------------------------------------------------------------
// WorkflowRunOutputStore
// ---------------------------------------------------------------------------

#[async_trait]
impl WorkflowRunOutputStore for FsOps {
    async fn put_run_output(
        &self,
        company: &CompanyId,
        record: &WorkflowRunOutputRecord,
    ) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.run_outputs_jsonl();
        // The per-path lock makes read-dedup-prune-write atomic against a
        // concurrent settle — process-local, the documented fs-backend
        // assumption everywhere in this file.
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut all = read_jsonl::<WorkflowRunOutputRecord>(&path).await?;
        // Last-write-wins per run_id: drop any prior snapshot for this run before
        // appending the new one, so a re-run's output overwrites rather than
        // stacks (and still counts once toward the cap).
        all.retain(|r| r.run_id != record.run_id);
        all.push(record.clone());
        prune_run_outputs(&mut all);
        rewrite_jsonl(&path, &all).await
    }

    async fn get_run_output(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Option<WorkflowRunOutputRecord>> {
        let all = read_jsonl::<WorkflowRunOutputRecord>(&self.bundle(company).run_outputs_jsonl())
            .await?;
        // Last-write-wins on read too: if two lines share a run_id (a crash
        // between append and prune), the later one is the truth.
        Ok(all.into_iter().rev().find(|r| r.run_id == run_id))
    }
}

/// Trims `all` to the newest [`MAX_RUN_OUTPUTS_PER_COMPANY`] run snapshots,
/// dropping the oldest. Kept as a free function so the cap lives in one place.
fn prune_run_outputs(all: &mut Vec<WorkflowRunOutputRecord>) {
    if all.len() <= MAX_RUN_OUTPUTS_PER_COMPANY {
        return;
    }
    sort_run_outputs_newest_first(all);
    all.truncate(MAX_RUN_OUTPUTS_PER_COMPANY);
}

// ---------------------------------------------------------------------------
// DeepTraceStore
// ---------------------------------------------------------------------------

#[async_trait]
impl DeepTraceStore for FsOps {
    async fn append_step_detail(
        &self,
        company: &CompanyId,
        record: &RunStepDetailRecord,
    ) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.deep_trace_jsonl();
        // The per-path lock makes append and compact atomic against a
        // concurrent step write — process-local, the documented fs-backend
        // assumption everywhere in this file.
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        // A genuine append, exactly like `run_steps`: the read side folds a
        // repeated `(run_id, step_seq)` to its last line (`list_step_details`),
        // so a flush that rewrites an existing ordinal converges rather than
        // stacks. The old whole-file read-prune-rewrite per step was quadratic
        // in a long company's history — every event rewrote everything.
        append_line(&path, &serde_json::to_string(record)?).await?;
        // Compaction is deferred until it is actually required. The known set
        // is seeded from disk on the first deep-trace write of a process, so a
        // file that already exceeded the cap is corrected immediately; after
        // that, a fresh run id pushing the count past the cap triggers one
        // read-prune-rewrite, and the set is rebuilt from the survivors.
        let mut by_path = self.deep_runs.lock().await;
        let runs = by_path.entry(path.clone()).or_default();
        if runs.is_empty() {
            let mut all = read_jsonl::<RunStepDetailRecord>(&path).await?;
            let before = all.len();
            prune_deep_trace(&mut all);
            *runs = all.iter().map(|r| r.run_id.clone()).collect();
            if all.len() < before {
                rewrite_jsonl(&path, &all).await?;
            }
            return Ok(());
        }
        if runs.insert(record.run_id.clone()) && runs.len() > MAX_DEEP_RUNS_PER_COMPANY {
            let mut all = read_jsonl::<RunStepDetailRecord>(&path).await?;
            prune_deep_trace(&mut all);
            rewrite_jsonl(&path, &all).await?;
            *runs = all.iter().map(|r| r.run_id.clone()).collect();
        }
        Ok(())
    }

    async fn list_step_details(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<RunStepDetailRecord>> {
        let all =
            read_jsonl::<RunStepDetailRecord>(&self.bundle(company).deep_trace_jsonl()).await?;
        let mut mine: Vec<RunStepDetailRecord> = Vec::new();
        for record in all {
            if record.run_id != run_id {
                continue;
            }
            // Last-write-wins on read too: two lines can share an ordinal after
            // a crash between append and rewrite, and the later one is the truth.
            match mine.iter_mut().find(|r| r.step_seq == record.step_seq) {
                Some(existing) => *existing = record,
                None => mine.push(record),
            }
        }
        mine.sort_by_key(|r| r.step_seq);
        Ok(mine)
    }

    async fn list_step_details_for_runs(
        &self,
        company: &CompanyId,
        run_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<RunStepDetailRecord>>> {
        // One scan of the company-wide file; the per-run alternative rescans it
        // once per listed run on the Observatory index.
        let all =
            read_jsonl::<RunStepDetailRecord>(&self.bundle(company).deep_trace_jsonl()).await?;
        let mut by_run: std::collections::HashMap<String, Vec<RunStepDetailRecord>> =
            run_ids.iter().map(|id| (id.clone(), Vec::new())).collect();
        for record in all {
            if let Some(mine) = by_run.get_mut(&record.run_id) {
                mine.push(record);
            }
        }
        for (run_id, mine) in by_run.iter_mut() {
            // Last-write-wins per step_seq, oldest first — the same settling the
            // single-run read applies (see `list_step_details`).
            let mut by_seq: std::collections::HashMap<u32, RunStepDetailRecord> =
                std::collections::HashMap::new();
            for record in std::mem::take(mine) {
                if &record.run_id != run_id {
                    continue;
                }
                by_seq.insert(record.step_seq, record);
            }
            let mut settled: Vec<RunStepDetailRecord> = by_seq.into_values().collect();
            settled.sort_by_key(|r| r.step_seq);
            *mine = settled;
        }
        Ok(by_run)
    }

    async fn purge_deep_trace(&self, company: &CompanyId, run_id: Option<&str>) -> Result<u64> {
        let path = self.bundle(company).deep_trace_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut all = read_jsonl::<RunStepDetailRecord>(&path).await?;
        // `removed` is the number of *records* the reader would have seen —
        // distinct `(run_id, step_seq)` pairs — not physical lines. A completion
        // row for an ordinal the start row already covered is one record, folded
        // exactly as the read side folds it, so the count agrees with a backend
        // that stores one row per ordinal.
        let mut doomed: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
        for record in all.iter() {
            match run_id {
                Some(id) if record.run_id == id => {
                    doomed.insert((record.run_id.clone(), record.step_seq));
                }
                None => {
                    doomed.insert((record.run_id.clone(), record.step_seq));
                }
                _ => {}
            }
        }
        let removed = doomed.len() as u64;
        if removed > 0 {
            match run_id {
                Some(id) => all.retain(|r| r.run_id != id),
                None => all.clear(),
            }
            rewrite_jsonl(&path, &all).await?;
        }
        // Keep the in-memory run set in step: a purged run is gone, so a later
        // append of that id must count as fresh again, and a company-wide purge
        // empties the file entirely.
        let mut by_path = self.deep_runs.lock().await;
        if let Some(runs) = by_path.get_mut(&path) {
            match run_id {
                Some(id) => {
                    runs.remove(id);
                }
                None => runs.clear(),
            }
        }
        Ok(removed)
    }
}

/// Trims `all` to the newest [`MAX_DEEP_RUNS_PER_COMPANY`] runs, dropping every
/// record belonging to an older one.
///
/// Prunes by **run**, not by record: dropping the oldest N rows would leave a
/// run holding a torn half of its own trace, which reads as "the agent stopped
/// thinking here" rather than "this run's bodies were pruned". Recency is the
/// newest `at_millis` any of a run's records carries, so a long run is ranked by
/// when it last wrote rather than when it started.
fn prune_deep_trace(all: &mut Vec<RunStepDetailRecord>) {
    let mut newest: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for record in all.iter() {
        let entry = newest.entry(record.run_id.as_str()).or_insert(0);
        *entry = (*entry).max(record.at_millis);
    }
    if newest.len() <= MAX_DEEP_RUNS_PER_COMPANY {
        return;
    }
    let mut ranked: Vec<(&str, u64)> = newest.into_iter().collect();
    // Newest first, with the run id as a tiebreaker so a prune is deterministic
    // when two runs share a millisecond.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let keep: std::collections::HashSet<String> = ranked
        .into_iter()
        .take(MAX_DEEP_RUNS_PER_COMPANY)
        .map(|(id, _)| id.to_string())
        .collect();
    all.retain(|r| keep.contains(&r.run_id));
}

// ---------------------------------------------------------------------------
// RunStore
// ---------------------------------------------------------------------------

#[async_trait]
impl RunStore for FsOps {
    async fn create_run(&self, company: &CompanyId, spec: NewRun) -> Result<RunRecord> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.runs_jsonl();
        // The per-path lock is what makes read-max-then-write atomic. It is
        // process-local — the documented fs-backend assumption everywhere in
        // this file — which is why the port's `create_run` contract calls the
        // filesystem ordinal best-effort rather than transactional.
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut runs = dedup_latest(read_jsonl::<RunRecord>(&path).await?);
        if runs.iter().any(|r| r.id == spec.id) {
            return Err(OpenCompanyError::Conflict(format!(
                "run '{}' already exists",
                spec.id
            )));
        }
        // A card-less run (issue #983) is always attempt 1: the ordinal counts
        // attempts *at a card*, and with no card there is nothing for a second
        // attempt to be the second of. Folding them all into one anonymous
        // bucket would make every chat turn the Nth attempt at nothing.
        let attempt = match &spec.task_id {
            Some(task_id) => runs
                .iter()
                .filter(|r| r.task_id.as_deref() == Some(task_id.as_str()))
                .map(|r| r.attempt)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
            None => 1,
        };
        let run = RunRecord {
            id: spec.id,
            company: company.clone(),
            task_id: spec.task_id,
            agent_id: spec.agent_id,
            chat_id: spec.chat_id,
            workflow_run_id: spec.workflow_run_id,
            node_id: spec.node_id,
            attempt,
            status: RunStatus::Pending,
            trigger_event_seq: None,
            thread_root: spec.thread_root,
            created_at_millis: now_millis(),
            started_at_millis: None,
            finished_at_millis: None,
            error: None,
            usage: Default::default(),
            step_count: 0,
        };
        runs.push(run.clone());
        rewrite_jsonl(&path, &runs).await?;
        Ok(run)
    }

    async fn get_run(&self, company: &CompanyId, id: &str) -> Result<Option<RunRecord>> {
        let runs = dedup_latest(read_jsonl::<RunRecord>(&self.bundle(company).runs_jsonl()).await?);
        Ok(runs.into_iter().find(|r| r.id == id))
    }

    async fn put_run(&self, company: &CompanyId, run: &RunRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.runs_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut runs = dedup_latest(read_jsonl::<RunRecord>(&path).await?);
        match runs.iter_mut().find(|r| r.id == run.id) {
            Some(existing) => *existing = run.clone(),
            None => runs.push(run.clone()),
        }
        rewrite_jsonl(&path, &runs).await
    }

    async fn list_runs(&self, company: &CompanyId, filter: &RunFilter) -> Result<Vec<RunRecord>> {
        let mut runs =
            dedup_latest(read_jsonl::<RunRecord>(&self.bundle(company).runs_jsonl()).await?);
        runs.retain(|r| filter.matches(r));
        sort_newest_first(&mut runs);
        if let Some(limit) = filter.limit {
            runs.truncate(limit);
        }
        Ok(runs)
    }

    async fn append_run_step(&self, company: &CompanyId, step: &RunStepRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.run_steps_jsonl();
        let line = serde_json::to_string(step)?;
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        // A genuine append: the trace only ever grows, and a replayed
        // `(run_id, step_seq)` is folded out at read time rather than by
        // rewriting the whole file per step.
        append_line(&path, &line).await
    }

    async fn list_run_steps(
        &self,
        company: &CompanyId,
        run_id: &str,
    ) -> Result<Vec<RunStepRecord>> {
        let steps = read_jsonl::<RunStepRecord>(&self.bundle(company).run_steps_jsonl()).await?;
        Ok(dedup_steps(steps, run_id))
    }

    async fn list_run_steps_for_runs(
        &self,
        company: &CompanyId,
        run_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<RunStepRecord>>> {
        // One scan of the company-wide file, then the same per-run dedup the
        // single-read applies — the Observatory index would otherwise rescan
        // the whole history once per listed run.
        let all = read_jsonl::<RunStepRecord>(&self.bundle(company).run_steps_jsonl()).await?;
        let mut by_run: std::collections::HashMap<String, Vec<RunStepRecord>> =
            run_ids.iter().map(|id| (id.clone(), Vec::new())).collect();
        for step in all {
            if let Some(mine) = by_run.get_mut(&step.run_id) {
                mine.push(step);
            }
        }
        let mut out = std::collections::HashMap::with_capacity(by_run.len());
        for (id, steps) in by_run {
            out.insert(id.clone(), dedup_steps(steps, &id));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// ScheduleFireStore (issue #241)
// ---------------------------------------------------------------------------
//
// Function-local imports keep this a pure append to a file edited concurrently
// on other branches (#274, #596).

/// The filesystem-safe directory component for `schedule_id`: its lowercase-hex
/// SHA-256. Hashing means an id the store did not mint (a `workflow-<id>` whose
/// `<id>` a console author chose) can never become a path component — the rule
/// [`Bundle::runs_jsonl`] documents for run ids.
fn hashed_schedule_component(schedule_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(schedule_id.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        // Infallible: writing to a String never fails.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[async_trait]
impl crate::ports::schedule_fires::ScheduleFireStore for FsOps {
    async fn claim_fire(
        &self,
        company: &CompanyId,
        schedule_id: &str,
        minute: u64,
    ) -> Result<bool> {
        let dir = self
            .bundle(company)
            .schedule_fires_dir()
            .join(hashed_schedule_component(schedule_id));
        let marker = dir.join(minute.to_string());
        // `create_new` is `O_EXCL`: the OS refuses to open the file if it
        // already exists, so the *first* caller to reach an unclaimed minute is
        // the only one whose open succeeds. That is the whole claim — no lock,
        // no read-then-write window. Single-node only: `O_EXCL` is not
        // trustworthy on NFS, which is why the hosted path runs mongodb.
        let claimed_at = now_millis();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return Err(io_err(&dir, e));
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
            {
                Ok(mut file) => {
                    // The claimed-at stamp is debug only, never part of the key;
                    // a write failure here does not un-claim the instant, so it
                    // is deliberately not propagated as a lost claim.
                    let _ = file.write_all(claimed_at.to_string().as_bytes());
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(io_err(&marker, e)),
            }
        })
        .await
        .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
    }

    async fn latest_fire(&self, company: &CompanyId, schedule_id: &str) -> Result<Option<u64>> {
        let dir = self
            .bundle(company)
            .schedule_fires_dir()
            .join(hashed_schedule_component(schedule_id));
        tokio::task::spawn_blocking(move || {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                // No directory means the schedule has never fired — the fresh
                // install case, which is "no anchor", not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io_err(&dir, e)),
            };
            let mut max: Option<u64> = None;
            for entry in entries {
                let entry = entry.map_err(|e| io_err(&dir, e))?;
                // A marker filename that does not parse as a minute is not one of
                // ours; skip it rather than failing the read.
                if let Some(minute) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u64>().ok())
                {
                    max = Some(max.map_or(minute, |m| m.max(minute)));
                }
            }
            Ok(max)
        })
        .await
        .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
    }

    async fn prune_fires_before(&self, company: &CompanyId, cutoff_minute: u64) -> Result<usize> {
        let root = self.bundle(company).schedule_fires_dir();
        tokio::task::spawn_blocking(move || {
            let schedules = match std::fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(e) => return Err(io_err(&root, e)),
            };
            let mut removed = 0usize;
            for schedule in schedules {
                let schedule_dir = schedule.map_err(|e| io_err(&root, e))?.path();
                let markers = match std::fs::read_dir(&schedule_dir) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_err(&schedule_dir, e)),
                };
                for marker in markers {
                    let marker = marker.map_err(|e| io_err(&schedule_dir, e))?;
                    let Some(minute) = marker
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse::<u64>().ok())
                    else {
                        continue;
                    };
                    if minute < cutoff_minute {
                        let path = marker.path();
                        match std::fs::remove_file(&path) {
                            Ok(()) => removed += 1,
                            // A racing prune already removed it — not our removal
                            // to count, but not an error either.
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(io_err(&path, e)),
                        }
                    }
                }
            }
            Ok(removed)
        })
        .await
        .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
    }

    async fn delete_schedule_fires(&self, company: &CompanyId, schedule_id: &str) -> Result<usize> {
        let dir = self
            .bundle(company)
            .schedule_fires_dir()
            .join(hashed_schedule_component(schedule_id));
        tokio::task::spawn_blocking(move || {
            let markers = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                // No directory means the schedule never fired — nothing to
                // purge, the same NotFound-is-empty rule the other verbs use.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(e) => return Err(io_err(&dir, e)),
            };
            let mut removed = 0usize;
            for marker in markers {
                let marker = marker.map_err(|e| io_err(&dir, e))?;
                // Only our minute markers count as claim rows; a filename that
                // does not parse is not one of ours, so it is left in place
                // (and will simply keep the best-effort `remove_dir` below from
                // succeeding, which is harmless).
                let Some(_minute) = marker
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u64>().ok())
                else {
                    continue;
                };
                let path = marker.path();
                match std::fs::remove_file(&path) {
                    Ok(()) => removed += 1,
                    // A racing prune already removed it — not our removal to
                    // count, but not an error either.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_err(&path, e)),
                }
            }
            // Best-effort: drop the now-empty schedule directory so a purged
            // schedule leaves no directory behind. Ignored if it is not empty
            // (a stray non-marker file) or already gone — the rows are what the
            // count and the contract are about, not the directory.
            let _ = std::fs::remove_dir(&dir);
            Ok(removed)
        })
        .await
        .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
        let lock = path_lock(&path);
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
// ReadStateStore
// ---------------------------------------------------------------------------

/// One stored marker. The user is part of the row rather than the file name, so
/// the whole company's markers stay one readable document.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredRead {
    user_id: String,
    channel_id: String,
    last_read_at: i64,
}

#[async_trait]
impl crate::ports::read_state::ReadStateStore for FsOps {
    async fn list(
        &self,
        company: &CompanyId,
        user: &str,
    ) -> Result<Vec<crate::ports::read_state::ChannelRead>> {
        let rows = load_json_vec::<StoredRead>(&self.bundle(company).read_state_json()).await?;
        let mut out: Vec<_> = rows
            .into_iter()
            .filter(|r| r.user_id == user)
            .map(|r| crate::ports::read_state::ChannelRead {
                channel_id: r.channel_id,
                last_read_at: r.last_read_at,
            })
            .collect();
        out.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        Ok(out)
    }

    async fn mark(
        &self,
        company: &CompanyId,
        user: &str,
        channel_id: &str,
        at: i64,
    ) -> Result<crate::ports::read_state::ChannelRead> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.read_state_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut rows = load_json_vec::<StoredRead>(&path).await?;
        // Monotonic, as the port promises — see `ReadStateStore::mark`.
        let settled = match rows
            .iter_mut()
            .find(|r| r.user_id == user && r.channel_id == channel_id)
        {
            Some(existing) => {
                existing.last_read_at = existing.last_read_at.max(at);
                existing.last_read_at
            }
            None => {
                rows.push(StoredRead {
                    user_id: user.to_string(),
                    channel_id: channel_id.to_string(),
                    last_read_at: at,
                });
                at
            }
        };
        write_atomic(&path, &serde_json::to_string(&rows)?).await?;
        Ok(crate::ports::read_state::ChannelRead {
            channel_id: channel_id.to_string(),
            last_read_at: settled,
        })
    }
}

// ---------------------------------------------------------------------------
// NotificationStore
// ---------------------------------------------------------------------------

/// One per-person read marker. Kept in its own document rather than as a field
/// on the notification, because read state is per person (#749) — a flag on the
/// shared record would mark it read for everyone the moment one admin opened it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredNotifRead {
    user_id: String,
    notification_id: String,
    read_at: u64,
}

#[async_trait]
impl crate::ports::notifications::NotificationStore for FsOps {
    async fn append(
        &self,
        company: &CompanyId,
        notification: &crate::ports::notifications::Notification,
    ) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.notifications_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut rows = load_json_vec::<crate::ports::notifications::Notification>(&path).await?;
        // Idempotent by id (first write wins): a retried or replayed append must
        // not duplicate the feed. Matches the sqlite primary key and the mongo
        // unique index, so all three backends agree.
        if !rows.iter().any(|n| n.id == notification.id) {
            rows.push(notification.clone());
            write_atomic(&path, &serde_json::to_string(&rows)?).await?;
        }
        Ok(())
    }

    async fn list(
        &self,
        company: &CompanyId,
        user: &str,
    ) -> Result<Vec<crate::ports::notifications::NotificationView>> {
        let bundle = self.bundle(company);
        let records = load_json_vec::<crate::ports::notifications::Notification>(
            &bundle.notifications_json(),
        )
        .await?;
        let reads = load_json_vec::<StoredNotifRead>(&bundle.notification_reads_json()).await?;
        // This person's markers, indexed once, so the join is linear rather than
        // O(records × markers) — mirrors the map the mongo backend builds.
        let mine: std::collections::HashMap<&str, u64> = reads
            .iter()
            .filter(|r| r.user_id == user)
            .map(|r| (r.notification_id.as_str(), r.read_at))
            .collect();
        let mut out: Vec<_> = records
            .into_iter()
            // Only what this person is addressed by. The rule lives on
            // `Notification::visible_to` so all three backends read it from one
            // place rather than each re-deriving it.
            .filter(|n| n.visible_to(user))
            .map(|n| {
                let read_at = mine.get(n.id.as_str()).copied();
                crate::ports::notifications::NotificationView {
                    notification: n,
                    read_at,
                }
            })
            .collect();
        // Newest first, ties broken by id descending — the order the trait
        // documents, not each backend's insertion order.
        out.sort_by(|a, b| {
            b.notification
                .created_at
                .cmp(&a.notification.created_at)
                .then_with(|| b.notification.id.cmp(&a.notification.id))
        });
        Ok(out)
    }

    async fn mark_read(
        &self,
        company: &CompanyId,
        user: &str,
        ids: Option<&[String]>,
    ) -> Result<u64> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let records = load_json_vec::<crate::ports::notifications::Notification>(
            &bundle.notifications_json(),
        )
        .await?;
        let path = bundle.notification_reads_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut reads = load_json_vec::<StoredNotifRead>(&path).await?;
        // Which notifications to mark: the named ids that actually exist, or
        // every notification when `None`.
        let targets: Vec<&str> = match ids {
            Some(ids) => records
                .iter()
                .filter(|n| ids.iter().any(|i| i == &n.id))
                .map(|n| n.id.as_str())
                .collect(),
            // Only what this person can see: a marker on a colleague's targeted
            // row is inert, but writing one makes "mark all read" mean
            // something different per backend.
            None => records
                .iter()
                .filter(|n| n.visible_to(user))
                .map(|n| n.id.as_str())
                .collect(),
        };
        let now = crate::ports::now_millis();
        let mut changed = false;
        for id in targets {
            // A latch: only stamp a marker that is not already there, so the
            // original `read_at` survives a re-mark.
            let already = reads
                .iter()
                .any(|r| r.user_id == user && r.notification_id == id);
            if !already {
                reads.push(StoredNotifRead {
                    user_id: user.to_string(),
                    notification_id: id.to_string(),
                    read_at: now,
                });
                changed = true;
            }
        }
        // Only rewrite the marker file when a marker was actually added, so a
        // repeated mark-all is a pure read rather than a rewrite of the whole
        // file for no change.
        if changed {
            write_atomic(&path, &serde_json::to_string(&reads)?).await?;
        }
        // Still-unread count for this person: records with no marker of theirs.
        let unread = records
            .iter()
            .filter(|n| n.visible_to(user))
            .filter(|n| {
                !reads
                    .iter()
                    .any(|r| r.user_id == user && r.notification_id == n.id)
            })
            .count() as u64;
        Ok(unread)
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
        // A binary node reads as an empty body, like a folder — see the port's
        // trait docs. Checked before touching the disk, so a 200 MiB video is
        // never loaded to be thrown away (and never attempted as UTF-8).
        let content = if node.kind == NodeKind::File && !node.is_binary() {
            let path = self.physical_path(company, &index, id)?;
            read_optional(&path).await?
        } else {
            String::new()
        };
        Ok(Some((node, content)))
    }

    async fn read_capped(
        &self,
        company: &CompanyId,
        id: &str,
        max_bytes: u64,
    ) -> Result<Option<(WorkspaceNode, String, u64)>> {
        let index = self.load_index(company).await?;
        let Some(node) = index.get(id).cloned() else {
            return Ok(None);
        };
        if node.kind != NodeKind::File || node.is_binary() {
            return Ok(Some((node, String::new(), 0)));
        }
        let path = self.physical_path(company, &index, id)?;
        // One open handle for both the length and the body. A concurrent
        // replacement publishes via rename (`write_atomic`), which repoints
        // the directory entry at a new inode rather than mutating this one,
        // so a handle opened before that rename keeps reading the file it
        // opened — `metadata` and the read below always agree.
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some((node, String::new(), 0)));
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        let len = file.metadata().await.map_err(|e| io_err(&path, e))?.len();
        if len > max_bytes {
            return Ok(Some((node, String::new(), len)));
        }
        let mut content = String::new();
        file.read_to_string(&mut content)
            .await
            .map_err(|e| io_err(&path, e))?;
        Ok(Some((node, content, len)))
    }

    async fn write(
        &self,
        company: &CompanyId,
        id: &str,
        content: &str,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        let path = self.bundle(company).workspace_index_json();
        let lock = path_lock(&path);
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
        if let Some(mime) = node.mime.clone() {
            return Err(OpenCompanyError::InvalidRequest(
                crate::ports::workspace::binary_write_refusal(&node.name, &mime),
            ));
        }
        node.updated_at_millis = now_millis();
        // Authorship rides the same stamp as the timestamp: "when the body last
        // changed" and "who changed it" are one fact and must never drift apart.
        node.updated_by = author;
        let node = node.clone();
        let file = self.physical_path(company, &index, id)?;
        write_atomic(&file, content).await?;
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
        let lock = path_lock(&path);
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
        reject_path_collision(&index, &node.id, &node.name, node.parent_id.as_deref())?;
        index.insert(node.id.clone(), node.clone());
        let physical = self.physical_path(company, &index, &node.id)?;
        match node.kind {
            NodeKind::Folder => {
                tokio::fs::create_dir_all(&physical)
                    .await
                    .map_err(|e| io_err(&physical, e))?;
            }
            NodeKind::File => {
                write_atomic(&physical, content.unwrap_or("")).await?;
            }
        }
        self.save_index(company, &index).await
    }

    /// The whole claim — look, then adopt or insert — under the one lock every
    /// other write to this index already takes (issue #759).
    ///
    /// That lock is what makes it atomic here, and it is enough: the `fs`
    /// backend is single-process per data directory by documented contract (see
    /// `docs/spec/runtime/storage.md`), so there is no second writer for it to
    /// miss. The other two backends have to reach for a transaction and an index
    /// respectively because their deployments have one.
    ///
    /// Before this, the same claim was a `tree()` read in the caller followed by
    /// a plain [`create`](WorkspaceStore::create). The read was honest about the
    /// instant it happened and the create acted on it later; on this backend
    /// `reject_path_collision` then refused the loser, so a concurrent publish
    /// failed spuriously instead of adopting the folder it wanted.
    async fn adopt_or_create_folder(
        &self,
        company: &CompanyId,
        parent: Option<&str>,
        name: &str,
        origin: WorkspaceOrigin,
    ) -> Result<crate::ports::workspace::FolderClaim> {
        use crate::ports::workspace::{FolderClaim, existing_folder_claim, new_folder};
        reject_unsafe_name(name)?;
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.workspace_index_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        if let Some(parent) = parent {
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
        if let Some(mut existing) = existing_folder_claim(index.values(), parent, name)? {
            // Stamp the adoption lease under the same index lock every other
            // writer takes (issue #1839), so it is durable before this returns
            // and a concurrent `delete_if_empty` cannot miss it. Authorship is
            // untouched — adoption still does not rewrite whose folder it is.
            if !existing.adopted {
                existing.adopted = true;
                index.insert(existing.id.clone(), existing.clone());
                self.save_index(company, &index).await?;
            }
            return Ok(FolderClaim::Adopted(existing));
        }
        let node = new_folder(name, parent, origin);
        // Still checked, and it is not the sibling check above repeated: this
        // backend renders a node's *path* from the chain of names above it, so a
        // legacy tree carrying duplicate-named ancestors can put two different
        // `(parent, name)` pairs on one directory. Refusing keeps the claim
        // fail-closed in exactly the case the sibling check cannot see.
        reject_path_collision(&index, &node.id, &node.name, parent)?;
        index.insert(node.id.clone(), node.clone());
        let physical = self.physical_path(company, &index, &node.id)?;
        tokio::fs::create_dir_all(&physical)
            .await
            .map_err(|e| io_err(&physical, e))?;
        self.save_index(company, &index).await?;
        Ok(FolderClaim::Created(node))
    }

    /// Writes the payload to its real path, then indexes it.
    ///
    /// **File first, index second — the same order the text path already uses.**
    /// A crash between the two leaves a file on disk that the index does not
    /// name: invisible to every reader, costing only disk, and overwritten by
    /// the next create at that path. The opposite order would leave the index
    /// naming a node whose bytes are absent — a download that 404s from a tree
    /// that says the file is there. Only one of those is survivable, so it is
    /// the one this picks; there is no sweep because there is nothing a sweep
    /// would protect a reader from.
    async fn create_binary(
        &self,
        company: &CompanyId,
        node: &WorkspaceNode,
        bytes: &[u8],
    ) -> Result<WorkspaceNode> {
        let node = crate::ports::workspace::stamped_binary(node, bytes)?;
        reject_unsafe_name(&node.name)?;
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.workspace_index_json();
        let lock = path_lock(&path);
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
        reject_path_collision(&index, &node.id, &node.name, node.parent_id.as_deref())?;
        index.insert(node.id.clone(), node.clone());
        // The same sanitized derivation the text path uses, so a binary node
        // cannot reach a path a note could not.
        let physical = self.physical_path(company, &index, &node.id)?;
        write_atomic_bytes(&physical, bytes).await?;
        self.save_index(company, &index).await?;
        // The stamped node, so the digest a caller records can only have come
        // from the store (issue #668).
        Ok(node)
    }

    async fn write_binary(
        &self,
        company: &CompanyId,
        id: &str,
        bytes: &[u8],
        mime: Option<&str>,
        author: WorkspaceOrigin,
    ) -> Result<WorkspaceNode> {
        let path = self.bundle(company).workspace_index_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        let node = index
            .get_mut(id)
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(format!("workspace node {id}")))?;
        crate::ports::workspace::rebind_binary(node, bytes, mime, author)?;
        let node = node.clone();
        let file = self.physical_path(company, &index, id)?;
        write_atomic_bytes(&file, bytes).await?;
        self.save_index(company, &index).await?;
        Ok(node)
    }

    /// Streams the file straight off disk — the payload is never resident.
    async fn read_bytes(
        &self,
        company: &CompanyId,
        id: &str,
    ) -> Result<Option<(WorkspaceNode, crate::ports::workspace::BlobStream)>> {
        let index = self.load_index(company).await?;
        let Some(node) = index.get(id).cloned() else {
            return Ok(None);
        };
        if !node.is_binary() {
            return Ok(None);
        }
        let path = self.physical_path(company, &index, id)?;
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            // The index names it but the bytes are gone — the benign half of the
            // write ordering above, seen from the read side. Reported as absent
            // rather than as an I/O error: there is no payload to serve, which
            // is exactly what `None` means here.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path, e)),
        };
        let stream = tokio_util::io::ReaderStream::new(file);
        Ok(Some((
            node,
            Box::pin(futures::StreamExt::map(stream, |chunk| {
                chunk.map_err(|e| {
                    OpenCompanyError::Store(format!("reading a workspace blob failed: {e}"))
                })
            })),
        )))
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
        let lock = path_lock(&path);
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
        let current = index.get(id).expect("node present");
        let target_name = name.unwrap_or(&current.name);
        let target_parent = parent.unwrap_or(current.parent_id.as_deref());
        reject_path_collision(&index, id, target_name, target_parent)?;
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

    async fn swap_files(
        &self,
        company: &CompanyId,
        expected_id: Option<&str>,
        replacement_id: &str,
        name: &str,
    ) -> Result<Option<WorkspaceNode>> {
        reject_unsafe_name(name)?;
        let path = self.bundle(company).workspace_index_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        let Some(replacement) = index.get(replacement_id).cloned() else {
            return Err(OpenCompanyError::CompanyNotFound(format!(
                "workspace node {replacement_id}"
            )));
        };
        if replacement.kind != NodeKind::File {
            return Err(OpenCompanyError::InvalidRequest(
                "only files can be promoted from a staging path".to_string(),
            ));
        }

        // The two readings of `expected_id`, decided under the same index lock
        // that the install below runs under — so a concurrent publisher cannot
        // slip between the question and the answer.
        let expected = expected_id.and_then(|id| index.get(id).cloned());
        let still_current = match expected_id {
            Some(_) => expected.as_ref().is_some_and(|node| {
                node.kind == NodeKind::File
                    && node.name == name
                    && node.parent_id == replacement.parent_id
            }),
            // Issue #697: `None` asserts the name is unoccupied. Any node
            // already holding it — of either kind, however it got there — loses
            // this caller the compare-and-swap. The staged node is excluded
            // because it is the thing being installed and still carries its
            // staging name.
            None => !index.values().any(|node| {
                node.id != replacement.id
                    && node.name == name
                    && node.parent_id == replacement.parent_id
            }),
        };
        if !still_current {
            // The compare-and-swap lost. The staging node is private to this
            // operation, so consume it while the same index lock is held.
            let physical = self.physical_path(company, &index, replacement_id)?;
            if tokio::fs::try_exists(&physical).await.unwrap_or(false) {
                tokio::fs::remove_file(&physical)
                    .await
                    .map_err(|e| io_err(&physical, e))?;
            }
            index.remove(replacement_id);
            self.save_index(company, &index).await?;
            return Ok(None);
        }

        let staged_physical = self.physical_path(company, &index, replacement_id)?;
        let mut promoted = replacement;
        promoted.name = name.to_string();
        promoted.updated_at_millis = now_millis();

        // Where the staged payload lands differs by mode. Replacing, it is the
        // superseded node's own path — that rename IS the swap boundary, which
        // is why the destination is computed before the index changes.
        // Creating, there is no such path yet, so the promoted node is named in
        // the index first and its final path derived from that.
        let destination = match expected.as_ref() {
            Some(node) => self.physical_path(company, &index, &node.id)?,
            None => {
                index.insert(promoted.id.clone(), promoted.clone());
                self.physical_path(company, &index, &promoted.id)?
            }
        };

        // `rename` is the filesystem compare-and-swap boundary: on the
        // supported Unix server platforms it replaces the destination in one
        // operation, so a payload reader sees either the old bytes or the new
        // bytes and never an absent final path. If it fails, the index is still
        // untouched on disk — nothing above here has been saved — and the old
        // deliverable, where there was one, remains authoritative.
        tokio::fs::rename(&staged_physical, &destination)
            .await
            .map_err(|e| io_err(&destination, e))?;
        if let Some(node) = expected {
            index.remove(&node.id);
        }
        index.insert(promoted.id.clone(), promoted.clone());
        self.save_index(company, &index).await?;
        Ok(Some(promoted))
    }

    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).workspace_index_json();
        let lock = path_lock(&path);
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

    /// Checked and removed under the single per-company index lock every
    /// writer here takes — `create`, `write`, `delete` and this method all
    /// serialize on the same `path_lock(workspace_index_json)` guard, so a
    /// concurrent `create` either lands its write entirely before this method
    /// takes the lock (and is seen by the fresh `load_index` below) or waits
    /// for this method to finish first. There is no window between the
    /// emptiness check and the removal for it to land in.
    async fn delete_if_empty(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let path = self.bundle(company).workspace_index_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut index = self.load_index(company).await?;
        let Some(node) = index.get(id) else {
            return Ok(false);
        };
        // An adopted folder has a second writer whose create has not landed yet
        // (issue #1839). The flag-write and this check both take the index lock
        // above, so they serialize: once an adoption has stamped `adopted`, this
        // refuses — closing Race 1 on this backend rather than narrowing it.
        if node.adopted {
            return Ok(false);
        }
        if index
            .values()
            .any(|node| node.parent_id.as_deref() == Some(id))
        {
            return Ok(false);
        }
        let physical = self.physical_path(company, &index, id)?;
        index.remove(id);
        if tokio::fs::try_exists(&physical).await.unwrap_or(false) {
            let meta = tokio::fs::symlink_metadata(&physical)
                .await
                .map_err(|e| io_err(&physical, e))?;
            if meta.is_dir() {
                // Non-recursive: the check above, taken under this same lock,
                // already proved nothing parents to `id`, so there is nothing
                // beneath it for a recursive sweep to find.
                tokio::fs::remove_dir(&physical)
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

/// The workspace-relative chain of names a node resolves to, or would.
///
/// This is [`FsOps::physical_path`]'s derivation with the constant
/// `workspace/` prefix left off, and it exists separately because the check
/// below needs the path of a node that is *not in the index yet*. Two nodes
/// alias exactly when their chains are equal, so comparing chains and
/// comparing the paths they build is the same question.
fn name_chain(
    index: &HashMap<String, WorkspaceNode>,
    name: &str,
    parent: Option<&str>,
) -> Result<Vec<String>> {
    let mut names = vec![name.to_string()];
    let mut cursor = parent.map(str::to_string);
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
    Ok(names)
}

/// Refuses a name/parent that would resolve to a physical path another node
/// already holds.
///
/// Node ids are the workspace's durable identity, but the filesystem backend
/// intentionally mirrors the operator-visible folder/name tree on disk. That
/// representation cannot safely carry two nodes on one path: the second create
/// would overwrite the first node's bytes while leaving both metadata rows
/// behind, and a later read would serve one payload under the other row's size
/// and digest (issue #666).
///
/// # Why the whole path, and not the sibling name
///
/// Matching on `(parent_id, name)` catches the ordinary case and misses the one
/// that is already in the field. A tree may hold **duplicate-named folders** —
/// [`crate::company::workspace_scaffold`] finds them, refuses to resolve them
/// and deliberately leaves them standing, and an index written before this
/// check existed can carry them. Two nodes under two roots both named `Desks`
/// are not siblings by `parent_id`, and their paths are nevertheless the same
/// string. Comparing the resolved chain closes that, and subsumes the sibling
/// case: equal parents plus equal names is equal chains.
///
/// # Why only the moved node's own path is checked
///
/// Renaming a folder moves its whole subtree, but a chain is determined by its
/// prefix: if a descendant's new chain equalled some other node's chain, their
/// prefixes would be equal too — which is a collision on the renamed node's own
/// chain, and is refused here. So a descendant cannot collide unless its
/// ancestor already does.
///
/// A node whose own chain cannot be derived (a dangling parent in a damaged
/// index) is skipped rather than propagated: it resolves to no path, so it can
/// alias nothing, and one unreachable row must not make every write fail.
///
/// The check runs while the workspace-index lock is held by every caller, so
/// two concurrent creates cannot both observe an available path. `self_id` is
/// excluded so a no-op rename remains valid.
fn reject_path_collision(
    index: &HashMap<String, WorkspaceNode>,
    self_id: &str,
    name: &str,
    parent: Option<&str>,
) -> Result<()> {
    let candidate = name_chain(index, name, parent)?;
    let taken = index
        .values()
        .filter(|node| node.id != self_id)
        .any(|node| {
            name_chain(index, &node.name, node.parent_id.as_deref())
                .is_ok_and(|chain| chain == candidate)
        });
    if taken {
        return Err(OpenCompanyError::Conflict(format!(
            "workspace already contains `{name}` in that folder"
        )));
    }
    Ok(())
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

/// Something a JSONL log keys its last-write-wins dedupe on.
///
/// Kept as a trait rather than a closure so [`dedup_latest`] reads identically
/// at every call site; the two implementors below are the only record types
/// stored in an id-keyed JSONL log.
trait HasId {
    fn record_id(&self) -> &str;
}

impl HasId for FactRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl HasId for ArtifactRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl HasId for RunRecord {
    fn record_id(&self) -> &str {
        &self.id
    }
}

/// Folds a raw `run-steps.jsonl` read down to one run's trace: the last record
/// per `step_seq`, oldest first.
///
/// Steps are appended, never rewritten, so a replayed append leaves two lines
/// with the same `(run_id, step_seq)`. Keeping the later one makes an append
/// idempotent — the same guarantee the sqlite and MongoDB backends get from
/// their composite primary key.
fn dedup_steps(steps: Vec<RunStepRecord>, run_id: &str) -> Vec<RunStepRecord> {
    let mut by_seq: HashMap<u32, RunStepRecord> = HashMap::new();
    for step in steps {
        if step.run_id != run_id {
            continue;
        }
        by_seq.insert(step.step_seq, step);
    }
    let mut out: Vec<RunStepRecord> = by_seq.into_values().collect();
    out.sort_by_key(|s| s.step_seq);
    out
}

/// Keeps the last record per id (last-write-wins), preserving first-seen order.
fn dedup_latest<T: HasId>(records: Vec<T>) -> Vec<T> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, T> = HashMap::new();
    for record in records {
        let id = record.record_id().to_string();
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.insert(id, record);
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// Rewrites a JSONL file from a slice of records (one JSON object per line).
pub(crate) async fn rewrite_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
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

    fn tmp_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-fsops-")
            .tempdir()
            .expect("tempdir")
    }

    fn workspace_node(id: &str, name: &str, kind: NodeKind, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            id: id.to_string(),
            name: name.to_string(),
            kind,
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

    /// A file node carrying the mime a binary write requires; `size` and
    /// `sha256` are the store's to compute.
    fn binary_node(id: &str, name: &str, parent: Option<&str>) -> WorkspaceNode {
        WorkspaceNode {
            mime: Some("application/pdf".to_string()),
            ..workspace_node(id, name, NodeKind::File, parent)
        }
    }

    async fn collect_stream(stream: crate::ports::workspace::BlobStream) -> Vec<u8> {
        use futures::StreamExt;
        let mut stream = stream;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        out
    }

    /// Two root folders under one name, written straight to the index.
    ///
    /// `create` refuses to *make* this shape now (issue #666), but it is
    /// reachable in the field: an index written before that refusal existed
    /// carries it, and `workspace_scaffold` finds such roots, declines to
    /// resolve them and deliberately leaves them standing. So the state is
    /// seeded rather than requested.
    async fn seed_duplicate_roots(ops: &FsOps, company: &CompanyId) {
        let mut index = HashMap::new();
        for id in ["root-a", "root-b"] {
            index.insert(
                id.to_string(),
                workspace_node(id, "Desks", NodeKind::Folder, None),
            );
        }
        ops.bundle(company)
            .ensure_dirs()
            .await
            .expect("bundle dirs");
        ops.save_index(company, &index).await.expect("seed index");
    }

    /// Issue #666, one level below the sibling case: nodes under two
    /// same-named folders are not siblings by `parent_id`, and still resolve to
    /// one path. A check keyed on `(parent_id, name)` admits the second and
    /// lets it overwrite the first node's bytes.
    #[tokio::test]
    async fn equal_names_under_duplicate_roots_cannot_claim_one_path() {
        let root_dir = tmp_root();
        let ops = FsOps::new(root_dir.path());
        let company = CompanyId::new("acme");
        seed_duplicate_roots(&ops, &company).await;

        ops.create_binary(
            &company,
            &binary_node("first", "report.pdf", Some("root-a")),
            b"the first payload",
        )
        .await
        .expect("the first child of the first root is fine");

        let err = ops
            .create_binary(
                &company,
                &binary_node("second", "report.pdf", Some("root-b")),
                b"a different payload entirely",
            )
            .await
            .expect_err("the second root's child resolves to the same path");
        assert!(
            matches!(err, OpenCompanyError::Conflict(_)),
            "a path already in use is a conflict, not a store error: {err:?}"
        );

        let (node, stream) = ops
            .read_bytes(&company, "first")
            .await
            .expect("read")
            .expect("the first node still exists");
        assert_eq!(
            collect_stream(stream).await,
            b"the first payload",
            "the refused create must not have overwritten the first payload"
        );
        assert_eq!(
            node.size,
            Some("the first payload".len() as u64),
            "nor left the surviving node describing bytes it no longer holds"
        );
        assert!(
            ops.tree(&company)
                .await
                .expect("tree")
                .iter()
                .all(|n| n.id != "second"),
            "a refused create must not leave a metadata row behind"
        );
    }

    /// The same path, reached by moving rather than creating.
    #[tokio::test]
    async fn a_move_cannot_claim_a_path_held_under_a_duplicate_root() {
        let root_dir = tmp_root();
        let ops = FsOps::new(root_dir.path());
        let company = CompanyId::new("acme");
        seed_duplicate_roots(&ops, &company).await;

        ops.create_binary(
            &company,
            &binary_node("first", "report.pdf", Some("root-a")),
            b"the first payload",
        )
        .await
        .expect("create under the first root");
        ops.create_binary(
            &company,
            &binary_node("mover", "draft.pdf", Some("root-b")),
            b"a different payload entirely",
        )
        .await
        .expect("a differently-named child of the second root is fine");

        let err = ops
            .rename_move(&company, "mover", Some("report.pdf"), None)
            .await
            .expect_err("renaming it onto the other root's path is refused");
        assert!(
            matches!(err, OpenCompanyError::Conflict(_)),
            "expected a conflict: {err:?}"
        );

        let (_, stream) = ops
            .read_bytes(&company, "first")
            .await
            .expect("read")
            .expect("the node whose path was targeted still exists");
        assert_eq!(
            collect_stream(stream).await,
            b"the first payload",
            "the refused move must not have overwritten it"
        );
        let (moved, stream) = ops
            .read_bytes(&company, "mover")
            .await
            .expect("read")
            .expect("and the mover still exists");
        assert_eq!(moved.name, "draft.pdf", "under its original name");
        assert_eq!(
            collect_stream(stream).await,
            b"a different payload entirely",
            "with its own bytes"
        );
    }

    #[tokio::test]
    async fn conformance_task_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_task_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_user_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_user_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_session_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_session_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_login_code_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_login_code_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_fact_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_fact_store(Arc::new(FsOps::new(&root))).await;
        conformance::assert_artifact_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_run_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_run_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_deep_trace_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_deep_trace_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_run_store_workflow_join() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_run_store_workflow_join(Arc::new(FsOps::new(&root))).await;
    }

    /// The prune drops whole runs, never a torn half of one.
    ///
    /// Backend-specific because it reaches past the cap, which would make the
    /// shared conformance suite write thousands of rows on every backend to
    /// assert a property one implementation owns.
    #[tokio::test]
    async fn deep_trace_prune_keeps_whole_runs() {
        use crate::ports::deep_trace::{
            DeepTraceStore, MAX_DEEP_RUNS_PER_COMPANY, RunStepDetailRecord, TurnStepDetail,
        };

        let root_dir = tmp_root();
        let ops = FsOps::new(root_dir.path());
        let company = CompanyId::new("alpha");

        // One run past the cap, two steps each, oldest first.
        let runs = MAX_DEEP_RUNS_PER_COMPANY + 1;
        for run in 0..runs {
            for seq in 0..2u32 {
                ops.append_step_detail(
                    &company,
                    &RunStepDetailRecord {
                        run_id: format!("run-{run:04}"),
                        step_seq: seq,
                        at_millis: (run as u64 + 1) * 1000,
                        detail: TurnStepDetail {
                            reasoning: Some(format!("run {run} step {seq}")),
                            ..TurnStepDetail::default()
                        },
                    },
                )
                .await
                .unwrap();
            }
        }

        // The oldest run went entirely...
        assert!(
            ops.list_step_details(&company, "run-0000")
                .await
                .unwrap()
                .is_empty(),
            "the oldest run's bodies were pruned"
        );
        // ...and every survivor kept BOTH of its steps. A prune that dropped the
        // oldest N *rows* would leave a run holding half its own trace, which
        // reads as the agent stopping mid-thought.
        for run in 1..runs {
            assert_eq!(
                ops.list_step_details(&company, &format!("run-{run:04}"))
                    .await
                    .unwrap()
                    .len(),
                2,
                "run {run} kept a torn half of its trace"
            );
        }
    }

    /// Appends are O(1): a flush that rewrites an existing ordinal appends a
    /// line and lets the read side fold it, rather than rewriting the whole
    /// company file per step (issue #1679). The raw file therefore carries both
    /// lines, and the read returns the later one.
    #[tokio::test]
    async fn deep_trace_append_folds_at_read_not_at_write() {
        use crate::ports::deep_trace::{DeepTraceStore, RunStepDetailRecord, TurnStepDetail};

        let root_dir = tmp_root();
        let ops = FsOps::new(root_dir.path());
        let company = CompanyId::new("alpha");
        let record = |reasoning: &str, at: u64| RunStepDetailRecord {
            run_id: "run-a".to_string(),
            step_seq: 1,
            at_millis: at,
            detail: TurnStepDetail {
                reasoning: Some(reasoning.to_string()),
                ..TurnStepDetail::default()
            },
        };

        ops.append_step_detail(&company, &record("first", 10))
            .await
            .unwrap();
        ops.append_step_detail(&company, &record("second", 20))
            .await
            .unwrap();

        // The read folds last-write-wins per ordinal...
        let got = ops.list_step_details(&company, "run-a").await.unwrap();
        assert_eq!(got.len(), 1, "a re-write replaces rather than stacking");
        assert_eq!(got[0].detail.reasoning.as_deref(), Some("second"));

        // ...because the file still physically holds both lines: no rewrite
        // happened between appends.
        let path = ops.bundle(&company).deep_trace_jsonl();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            raw.lines().count(),
            2,
            "a same-ordinal append must not rewrite the whole file"
        );
    }

    /// A run row written before `task_id` could be absent still loads
    /// (issue #983). Backend-independent — see the assertion's own docs — so it
    /// is driven from the one backend every lane builds.
    #[test]
    fn conformance_legacy_run_row_loads() {
        conformance::assert_legacy_run_row_loads();
    }

    #[tokio::test]
    async fn conformance_workflow_revision_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workflow_revision_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workflow_run_output_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workflow_run_output_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_run_reaper() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_run_reaper(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_schedule_fire_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_schedule_fire_store(Arc::new(FsOps::new(&root))).await;
    }

    /// A fresh `FsOps` over the same root sees a prior instance's claims (issue
    /// #241): the durable record is on disk, so the anchor survives the process
    /// restart that motivated the whole port. Proves the fs backend's marker
    /// files are read back, not just written.
    #[tokio::test]
    async fn schedule_fire_claim_survives_a_new_fsops_over_the_same_root() {
        use crate::ports::schedule_fires::ScheduleFireStore;
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let company = crate::ports::types::CompanyId::new("acme");

        let first = FsOps::new(&root);
        assert!(first.claim_fire(&company, "workflow-x", 42).await.unwrap());

        // A brand-new store over the same root — the shape a restart produces.
        let second = FsOps::new(&root);
        assert!(
            !second.claim_fire(&company, "workflow-x", 42).await.unwrap(),
            "a restart must see the earlier claim and lose the repeat"
        );
        assert_eq!(
            second.latest_fire(&company, "workflow-x").await.unwrap(),
            Some(42),
            "the anchor is durable across a new instance"
        );
    }

    #[tokio::test]
    async fn conformance_usage_meter() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_usage_meter(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_usage_retention() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_usage_retention(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_skill_state_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_skill_state_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_read_state_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_read_state_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_notification_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_notification_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workspace_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workspace_binary_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_binary_store(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workspace_read_capped() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_read_capped(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workspace_folder_claims() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_folder_claims(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workspace_sibling_names() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_sibling_names(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_workspace_adoption_lease() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_adoption_lease(Arc::new(FsOps::new(&root))).await;
    }

    /// Issue #887, and the backend the case was written against: this is the
    /// one that failed it. Node content was written with a bare
    /// `tokio::fs::write`, so a reader inside the `O_TRUNC` window saw a
    /// prefix — visibly when the cut split a codepoint, silently when it did
    /// not. Multi-threaded because the race is between two blocking-pool
    /// threads, which a current-thread runtime makes far rarer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_workspace_read_never_tears() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_read_never_tears(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_workspace_read_capped_race() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_workspace_read_capped_race(Arc::new(FsOps::new(&root))).await;
    }

    #[tokio::test]
    async fn workspace_files_land_on_disk_under_folders() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
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
                name: "brand".into(),
                kind: NodeKind::Folder,
                parent_id: None,
                updated_at_millis: now,
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
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
                created_by: WorkspaceOrigin::Operator,
                updated_by: WorkspaceOrigin::Operator,
                mime: None,
                size: None,
                sha256: None,
                adopted: false,
            },
            Some("# Voice"),
        )
        .await
        .unwrap();
        let disk = root.join("companies/acme/workspace/brand/voice.md");
        assert_eq!(tokio::fs::read_to_string(&disk).await.unwrap(), "# Voice");

        // A rename physically relocates the subtree.
        ops.rename_move(&company, "f1", Some("Branding"), None)
            .await
            .unwrap();
        let moved = root.join("companies/acme/workspace/Branding/voice.md");
        assert!(tokio::fs::try_exists(&moved).await.unwrap());
        assert!(!tokio::fs::try_exists(&disk).await.unwrap());

        let _ = (FactKind::Fact, SkillSource::Company, SampleKind::Inference);
    }
}
