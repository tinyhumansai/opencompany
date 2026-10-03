use tinymemory::{ListRequest, MemoryMeta, MetaFilter, SourceKind};

use super::*;

#[tokio::test]
async fn writes_are_accepted_and_reads_stay_empty() {
    let engine = NullEngine::new();
    let receipt = engine
        .store(StoreItem::document(
            "remember this",
            MemoryMeta::from_source(SourceKind::Agent, None),
        ))
        .await
        .unwrap();
    assert!(!receipt.replayed);
    let page = engine
        .list(ListRequest::new(MetaFilter::default(), 10))
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn malformed_writes_are_still_refused() {
    let engine = NullEngine::new();
    let refused = engine
        .store(StoreItem::document(
            "   ",
            MemoryMeta::from_source(SourceKind::Agent, None),
        ))
        .await;
    assert!(refused.is_err());
}

#[test]
fn it_reports_itself_as_null_with_nothing_to_rank() {
    let engine = NullEngine::new();
    assert_eq!(engine.descriptor().id, NULL_ENGINE_ID);
    assert!(engine.descriptor().fetch_modes.is_empty());
}
