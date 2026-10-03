use std::sync::Arc;

use tinymemory::{MemoryEngine, MemoryMeta, SourceKind, StoreItem};
use tinymemory_conformance::ReferenceEngine;

use super::*;
use crate::ports::{CompanyId, FactKind, FactRecord};
use crate::store::memory::BoundMemory;
use crate::store::{MemoryBackend, StorageSettings};

fn fact(id: &str) -> FactRecord {
    FactRecord {
        id: id.into(),
        kind: FactKind::Fact,
        title: format!("title {id}"),
        body: format!("body {id}"),
        source: "cto".into(),
        updated_at_millis: 1,
    }
}

async fn seeded() -> Arc<dyn MemoryEngine> {
    let engine: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    let bound = BoundMemory::bind(engine.clone());
    for company in ["acme", "globex"] {
        let id = CompanyId::new(company);
        for n in 0..3 {
            bound
                .facts()
                .upsert(&id, &fact(&format!("f{n}")))
                .await
                .unwrap();
        }
    }
    engine
}

#[tokio::test]
async fn every_host_record_moves_and_reads_back_through_the_ports() {
    let from = seeded().await;
    let to: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    let summary = migrate(&from, &to, 2, None, |_| {}).await.unwrap().unwrap();
    assert_eq!(summary.exported, 6);
    assert_eq!(summary.imported, 6);
    assert_eq!(summary.skipped, 0);
    let facts = BoundMemory::bind(to)
        .facts()
        .list(&CompanyId::new("acme"), None, None)
        .await
        .unwrap();
    assert_eq!(facts.len(), 3);
}

#[tokio::test]
async fn records_other_products_wrote_are_not_selected() {
    let from = seeded().await;
    from.store(StoreItem::document(
        "someone else's note",
        MemoryMeta::from_source(SourceKind::Agent, None),
    ))
    .await
    .unwrap();
    assert_eq!(count_records(&from, None, 10).await.unwrap(), 6);
}

#[tokio::test]
async fn a_rerun_is_idempotent() {
    let from = seeded().await;
    let to: Arc<dyn MemoryEngine> = Arc::new(ReferenceEngine::new());
    migrate(&from, &to, 4, None, |_| {}).await.unwrap().unwrap();
    let again = migrate(&from, &to, 4, None, |_| {}).await.unwrap().unwrap();
    assert_eq!(again.imported, 0);
    assert_eq!(again.skipped, 6);
    assert_eq!(count_records(&to, None, 4).await.unwrap(), 6);
}

fn remote_settings() -> StorageSettings {
    StorageSettings {
        memory_backend: MemoryBackend::Remote,
        memory_driver: Some("cortexdb".into()),
        memory_url: Some("https://from.example".into()),
        memory_api_key: Some("k".into()),
        ..StorageSettings::default()
    }
}

#[test]
fn resolve_refuses_store_null_and_tenant_mode_sources() {
    let mut store = remote_settings();
    store.memory_backend = MemoryBackend::Store;
    assert!(resolve_migrate_configs(&store, "cortexdb", Some("https://x".into()), None).is_err());

    let mut null = remote_settings();
    null.memory_backend = MemoryBackend::Null;
    assert!(resolve_migrate_configs(&null, "cortexdb", Some("https://x".into()), None).is_err());

    let mut tenant = remote_settings();
    tenant.tenant_id = Some("t1".into());
    assert!(resolve_migrate_configs(&tenant, "cortexdb", Some("https://x".into()), None).is_err());
}

#[test]
fn resolve_refuses_null_retired_and_same_targets() {
    let settings = remote_settings();
    assert!(resolve_migrate_configs(&settings, "null", None, None).is_err());
    assert!(resolve_migrate_configs(&settings, "mem0", Some("https://x".into()), None).is_err());
    assert!(
        resolve_migrate_configs(
            &settings,
            "cortexdb",
            Some("https://from.example/".into()),
            None
        )
        .is_err()
    );
}

#[test]
fn resolve_takes_the_engines_default_endpoint_when_none_is_given() {
    // Both registry engines carry a default endpoint, so `--to-url` is
    // optional; a given one is carried through for a self-run instance.
    let settings = remote_settings();
    let (_, to) = resolve_migrate_configs(&settings, "tinyhumans", None, Some("k".into())).unwrap();
    assert_eq!(to.driver_id.as_deref(), Some("tinyhumans"));
    assert_eq!(to.url, None);
    let (_, to) = resolve_migrate_configs(
        &settings,
        "cortexdb",
        Some("https://to.example".into()),
        Some("k".into()),
    )
    .unwrap();
    assert_eq!(to.url.as_deref(), Some("https://to.example"));
}
