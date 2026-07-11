//! [`CompanyRegistry`]: the map from [`CompanyId`] to running
//! [`CompanyRuntime`].
//!
//! One type serves both deployment shapes: the prosumer single-company case
//! (use [`sole`](CompanyRegistry::sole)) and the multi-tenant platform case
//! (address companies by id). Registry operations are fast and synchronous, so
//! it is backed by a `std::sync::RwLock`, not a tokio lock.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyId;

/// A thread-safe registry of running companies.
#[derive(Clone, Default)]
pub struct CompanyRegistry {
    inner: Arc<RwLock<HashMap<CompanyId, Arc<CompanyRuntime>>>>,
}

impl CompanyRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the runtime registered under `id`, if any.
    pub fn get(&self, id: &CompanyId) -> Option<Arc<CompanyRuntime>> {
        self.inner
            .read()
            .expect("registry poisoned")
            .get(id)
            .cloned()
    }

    /// Registers (or replaces) the runtime for `id`.
    pub fn insert(&self, id: CompanyId, runtime: Arc<CompanyRuntime>) {
        self.inner
            .write()
            .expect("registry poisoned")
            .insert(id, runtime);
    }

    /// Removes and returns the runtime registered under `id`, if any.
    ///
    /// Used by the archive lifecycle transition to park a company: once removed,
    /// the company is no longer addressable and chatting it returns a 404.
    pub fn remove(&self, id: &CompanyId) -> Option<Arc<CompanyRuntime>> {
        self.inner.write().expect("registry poisoned").remove(id)
    }

    /// The ids of every registered company, sorted.
    pub fn list(&self) -> Vec<CompanyId> {
        let mut ids: Vec<CompanyId> = self
            .inner
            .read()
            .expect("registry poisoned")
            .keys()
            .cloned()
            .collect();
        ids.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
        ids
    }

    /// The number of registered companies.
    pub fn len(&self) -> usize {
        self.inner.read().expect("registry poisoned").len()
    }

    /// Whether no companies are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The sole registered runtime, when exactly one company is registered.
    ///
    /// Powers the single-company `/api/v1/company/...` aliases; returns `None`
    /// when zero or more than one company is registered.
    pub fn sole(&self) -> Option<Arc<CompanyRuntime>> {
        let map = self.inner.read().expect("registry poisoned");
        if map.len() == 1 {
            map.values().next().cloned()
        } else {
            None
        }
    }
}

impl std::fmt::Debug for CompanyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanyRegistry")
            .field("companies", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::company::CompanyManifest;
    use crate::runtime::RuntimeBuilder;

    fn manifest(name: &str) -> CompanyManifest {
        toml::from_str(&format!("[company]\nname = \"{name}\"\n")).unwrap()
    }

    async fn runtime(home: &std::path::Path, id: &str) -> Arc<CompanyRuntime> {
        Arc::new(
            RuntimeBuilder::new(home.to_path_buf(), manifest(id))
                .with_id(CompanyId::new(id))
                .build()
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn sole_returns_the_only_company() {
        let home =
            std::env::temp_dir().join(format!("opencompany-reg-{}", crate::ports::generate_id()));
        let registry = CompanyRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.sole().is_none());

        registry.insert(CompanyId::new("acme"), runtime(&home, "acme").await);
        assert!(registry.sole().is_some());
        assert_eq!(registry.list(), vec![CompanyId::new("acme")]);

        registry.insert(CompanyId::new("globex"), runtime(&home, "globex").await);
        // Two companies: no sole.
        assert!(registry.sole().is_none());
        assert_eq!(registry.len(), 2);
        assert!(registry.get(&CompanyId::new("globex")).is_some());
        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn remove_unregisters_the_company() {
        let home =
            std::env::temp_dir().join(format!("opencompany-reg-{}", crate::ports::generate_id()));
        let registry = CompanyRegistry::new();
        registry.insert(CompanyId::new("acme"), runtime(&home, "acme").await);
        assert!(registry.get(&CompanyId::new("acme")).is_some());

        let removed = registry.remove(&CompanyId::new("acme"));
        assert!(removed.is_some());
        assert!(registry.get(&CompanyId::new("acme")).is_none());
        assert!(registry.remove(&CompanyId::new("acme")).is_none());
        tokio::fs::remove_dir_all(&home).await.ok();
    }
}
