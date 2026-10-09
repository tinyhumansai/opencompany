use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use serde_json::json;

use super::team_agent_test_support::*;
use crate::AppState;
use crate::runtime::RuntimeBuilder;

struct CountingRebuilder {
    home: std::path::PathBuf,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::runtime::RuntimeRebuilder for CountingRebuilder {
    async fn rebuild(
        &self,
        _state: &AppState,
        request: crate::runtime::RebuildRequest,
    ) -> crate::Result<crate::company::runtime::CompanyRuntime> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        RuntimeBuilder::new(self.home.clone(), request.manifest)
            .with_id(request.id)
            .with_handover(request.handover)
            .build()
            .await
    }
}

async fn counting_state(home: &std::path::Path) -> (AppState, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let state = state_with_manifest(home, ROSTER)
        .await
        .with_rebuilder(Arc::new(CountingRebuilder {
            home: home.to_path_buf(),
            calls: Arc::clone(&calls),
        }));
    (state, calls)
}

#[tokio::test]
async fn a_tools_edit_rebuilds_a_company_off_the_harness_path() {
    let home_dir = home();
    let (state, calls) = counting_state(home_dir.path()).await;

    let (status, body) = patch_agent(&state, "ceo", json!({"tools": ["workspace.read"]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (status, body) = patch_agent(&state, "ceo", json!({"tools": null})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_name_edit_does_not_rebuild() {
    let home_dir = home();
    let (state, calls) = counting_state(home_dir.path()).await;

    let (status, body) = patch_agent(&state, "ceo", json!({"name": "Boss"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
