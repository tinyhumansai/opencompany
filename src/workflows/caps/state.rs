//! The workflow [`StateStore`]: durable per-run key/value over the per-company
//! [`SecretStore`](crate::ports::SecretStore) seam.
//!
//! OpenCompany has no dedicated workflow-state store; the secret store is the
//! per-company durable key/value seam (the same one `DomainStatus` rides on at
//! `__domain`, see [`crate::company::dns`]). Keys are namespaced
//! `__wf_state:{workflow_id_len}:{workflow_id}:{key_len}:{key}` so one
//! workflow's state can never read or clobber another's, and values are JSON.
//!
//! **Honest note**: no tinyflows node OpenCompany emits reads or writes
//! `caps.state` in P1 — this is deliberate contract-plumbing that a later phase
//! (P3, stateful/resumable workflows) consumes. It is wired now so the seam
//! exists and is tested, not because a current node exercises it.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tinyflows::caps::StateStore;
use tinyflows::error::{EngineError, Result as TfResult};

use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

/// A [`StateStore`] over the per-company [`SecretStore`], namespaced by workflow
/// id so runs of different workflows share no keys.
pub struct CompanyStateStore {
    secrets: Arc<dyn SecretStore>,
    company: CompanyId,
    workflow_id: String,
}

impl CompanyStateStore {
    /// Builds a store scoped to `company`'s `workflow_id`.
    pub fn new(secrets: Arc<dyn SecretStore>, company: CompanyId, workflow_id: String) -> Self {
        Self {
            secrets,
            company,
            workflow_id,
        }
    }

    /// The namespaced secret key backing run state `key`.
    fn namespaced(&self, key: &str) -> String {
        format!(
            "__wf_state:{}:{}:{}:{}",
            self.workflow_id.len(),
            self.workflow_id,
            key.len(),
            key
        )
    }
}

#[async_trait]
impl StateStore for CompanyStateStore {
    async fn load(&self, key: &str) -> TfResult<Option<Value>> {
        let full = self.namespaced(key);
        match self.secrets.get(&self.company, &full).await {
            Ok(Some(value)) => serde_json::from_str(value.expose())
                .map(Some)
                .map_err(|err| {
                    EngineError::Capability(format!(
                        "workflow state '{key}' is not valid JSON: {err}"
                    ))
                }),
            Ok(None) => Ok(None),
            Err(err) => Err(EngineError::Capability(format!(
                "workflow state load '{key}' failed: {err}"
            ))),
        }
    }

    async fn store(&self, key: &str, value: Value) -> TfResult<()> {
        let full = self.namespaced(key);
        let serialized = serde_json::to_string(&value).map_err(|err| {
            EngineError::Capability(format!("workflow state '{key}' is not serializable: {err}"))
        })?;
        self.secrets
            .set(&self.company, &full, SecretValue(serialized))
            .await
            .map_err(|err| {
                EngineError::Capability(format!("workflow state store '{key}' failed: {err}"))
            })
    }
}

/// The inert no-op used when no secret store is wired: a miss reads as `None`, a
/// store is dropped. Keeps a run from failing just because durable state is
/// unavailable (no P1 node consumes it).
pub struct NoopState;

#[async_trait]
impl StateStore for NoopState {
    async fn load(&self, _key: &str) -> TfResult<Option<Value>> {
        Ok(None)
    }
    async fn store(&self, _key: &str, _value: Value) -> TfResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FsSecretStore;
    use serde_json::json;

    #[tokio::test]
    async fn roundtrips_and_isolates_across_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(FsSecretStore::new(dir.path().to_path_buf()));
        let company = CompanyId::new("acme");

        let wf_a = CompanyStateStore::new(secrets.clone(), company.clone(), "a".to_string());
        let wf_b = CompanyStateStore::new(secrets.clone(), company.clone(), "b".to_string());

        wf_a.store("cursor", json!({ "page": 2 })).await.unwrap();
        // Same key, different workflow → independent slot.
        assert_eq!(
            wf_a.load("cursor").await.unwrap(),
            Some(json!({ "page": 2 }))
        );
        assert_eq!(wf_b.load("cursor").await.unwrap(), None);

        wf_b.store("cursor", json!({ "page": 9 })).await.unwrap();
        assert_eq!(
            wf_a.load("cursor").await.unwrap(),
            Some(json!({ "page": 2 }))
        );
        assert_eq!(
            wf_b.load("cursor").await.unwrap(),
            Some(json!({ "page": 9 }))
        );
    }

    #[test]
    fn namespace_is_unambiguous_when_segments_contain_colons() {
        let dir = tempfile::tempdir().unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(FsSecretStore::new(dir.path().to_path_buf()));
        let company = CompanyId::new("acme");
        let left =
            CompanyStateStore::new(secrets.clone(), company.clone(), "workflow:a".to_string());
        let right = CompanyStateStore::new(secrets, company, "workflow".to_string());

        assert_ne!(left.namespaced("cursor"), right.namespaced("a:cursor"));
    }

    #[tokio::test]
    async fn noop_state_reads_none_and_drops_writes() {
        NoopState.store("k", json!(1)).await.unwrap();
        assert_eq!(NoopState.load("k").await.unwrap(), None);
    }
}
