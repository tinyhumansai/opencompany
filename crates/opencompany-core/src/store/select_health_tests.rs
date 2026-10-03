//! The boot/console engine probe: `health()` beside one read-only `list`,
//! under one deadline each, and how its answer lands on the descriptor.

use std::sync::Arc;

use tinymemory::{
    EngineDescriptor, EngineHealth, Error as EngineError, FetchPage, FetchRequest, ForgetReport,
    ForgetTarget, ListPage, ListRequest, MemoryEngine, RecallAnswer, RecallRequest, StoreItem,
    StoreReceipt, async_trait,
};
use tinymemory_conformance::ReferenceEngine;

use super::*;

/// An engine whose health and list answers are scripted, and which can be
/// made to sleep past the probe budget.
struct Scripted {
    inner: ReferenceEngine,
    health: EngineHealth,
    list_fails: bool,
    sleep: Option<std::time::Duration>,
}

impl Scripted {
    fn new(health: EngineHealth, list_fails: bool) -> Self {
        Self {
            inner: ReferenceEngine::new(),
            health,
            list_fails,
            sleep: None,
        }
    }
}

#[async_trait]
impl MemoryEngine for Scripted {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }

    async fn health(&self) -> EngineHealth {
        if let Some(sleep) = self.sleep {
            tokio::time::sleep(sleep).await;
        }
        self.health.clone()
    }

    async fn recall(&self, req: RecallRequest) -> tinymemory::Result<RecallAnswer> {
        self.inner.recall(req).await
    }

    async fn fetch(&self, req: FetchRequest) -> tinymemory::Result<FetchPage> {
        self.inner.fetch(req).await
    }

    async fn store(&self, item: StoreItem) -> tinymemory::Result<StoreReceipt> {
        self.inner.store(item).await
    }

    async fn forget(&self, target: ForgetTarget) -> tinymemory::Result<ForgetReport> {
        self.inner.forget(target).await
    }

    async fn list(&self, req: ListRequest) -> tinymemory::Result<ListPage> {
        if let Some(sleep) = self.sleep {
            tokio::time::sleep(sleep).await;
        }
        if self.list_fails {
            return Err(EngineError::Unauthorized("revoked key".into()));
        }
        self.inner.list(req).await
    }
}

const BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

#[tokio::test]
async fn a_healthy_engine_with_a_working_read_is_clean() {
    let outcome = probe_engine(&ReferenceEngine::new(), BUDGET).await;
    assert!(outcome.healthy);
    assert!(outcome.unreachable.is_empty());
    assert!(outcome.degraded.is_empty());
    assert!(outcome.slow.is_empty());
}

#[tokio::test]
async fn an_engine_that_says_ok_but_refuses_a_read_is_unreachable() {
    // The self-report is exactly what a revoked credential can get wrong;
    // the live read is what catches it.
    let outcome = probe_engine(&Scripted::new(EngineHealth::Ok, true), BUDGET).await;
    assert!(outcome.healthy);
    assert_eq!(outcome.unreachable, vec!["list"]);
}

#[tokio::test]
async fn degraded_is_still_healthy_and_carries_its_reason() {
    let engine = Scripted::new(EngineHealth::Degraded("index rebuilding".into()), false);
    let outcome = probe_engine(&engine, BUDGET).await;
    assert!(outcome.healthy);
    assert_eq!(outcome.degraded, vec!["index rebuilding"]);
}

#[tokio::test]
async fn down_is_unhealthy() {
    let engine = Scripted::new(EngineHealth::Down("connection refused".into()), false);
    assert!(!probe_engine(&engine, BUDGET).await.healthy);
}

#[tokio::test(start_paused = true)]
async fn a_slow_engine_is_slow_not_refused() {
    let mut engine = Scripted::new(EngineHealth::Ok, false);
    engine.sleep = Some(std::time::Duration::from_secs(60));
    let outcome = probe_engine(&engine, std::time::Duration::from_millis(50)).await;
    assert!(
        !outcome.healthy,
        "a timed-out health check is not a healthy one"
    );
    assert_eq!(outcome.slow, vec!["health", "list"]);
    assert!(outcome.unreachable.is_empty());
}

#[tokio::test]
async fn refresh_health_records_the_answer_on_the_descriptor() {
    let mut overlay = MemoryOverlay::test_with_ports(
        Arc::new(crate::store::FsMemoryStore::new(std::env::temp_dir())),
        Arc::new(crate::store::FsContextStore::new(std::env::temp_dir())),
        None,
    );
    overlay.probe = Some(Arc::new(Scripted::new(EngineHealth::Ok, true)));
    overlay.refresh_health(BUDGET).await;
    assert_eq!(overlay.descriptor.healthy, Some(true));
    assert_eq!(
        overlay.descriptor.unreachable_families.as_deref(),
        Some(&["list".to_string()][..])
    );
}
