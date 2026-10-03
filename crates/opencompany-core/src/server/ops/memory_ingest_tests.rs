//! Tests for the Brain drop zone.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::company::CompanyManifest;
use crate::ports::store::CompanyStore;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::RuntimeBuilder;
use crate::server::router;
use crate::store::FsCompanyStore;
use crate::{AppConfig, AppState};

const BOUNDARY: &str = "----ingesttestboundary";

/// A one-company host on a temporary home.
async fn state_at(dir: &std::path::Path) -> AppState {
    let manifest: CompanyManifest =
        toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
    let store = FsCompanyStore::new(dir.to_path_buf());
    let id = CompanyId::new("acme");
    store
        .save(&CompanyRecord {
            general_channel: Default::default(),
            overlay_desk_hive: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_retired_agents: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new(dir.to_path_buf(), manifest)
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
    let state = AppState::new(AppConfig::default()).with_home(dir.to_path_buf());
    state.registry().insert(id, Arc::new(runtime));
    crate::server::test_support::seed_fixed_admin(&state, "acme").await;
    state
}

/// Builds a `multipart/form-data` body with one part per `(filename, type, bytes)`.
fn multipart(files: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, mime, bytes) in files {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
                 Content-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Drops files on `…/memory/ingest`.
async fn drop_files(state: &AppState, files: &[(&str, &str, &[u8])]) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/company/memory/ingest")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart(files)))
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Reads the company's Brain list back, which is where an operator will look
/// for what they just dropped.
async fn brain_rows(state: &AppState) -> Vec<Value> {
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/company/memory")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The whole point: a dropped document is in memory afterwards, and the Brain
/// list shows it as a document rather than as something a teammate learned.
#[tokio::test]
async fn a_dropped_document_becomes_recallable_memory() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;

    let (status, body) = drop_files(
        &state,
        &[(
            "handbook.md",
            "text/markdown",
            b"# Expenses\n\nReceipts go to finance within 30 days.",
        )],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stored"], 1);
    assert_eq!(body["items"][0]["status"], "stored");
    assert_eq!(body["items"][0]["chunks"], 1);

    let rows = brain_rows(&state).await;
    let document = rows
        .iter()
        .find(|row| row["origin"] == "document")
        .unwrap_or_else(|| panic!("no document row in {rows:#?}"));
    assert!(
        document["body"]
            .as_str()
            .unwrap()
            .contains("Receipts go to finance"),
        "{document}"
    );
    assert_eq!(
        document["source"], "handbook.md",
        "the row names the document it came from"
    );
}

/// A folder drop is many files at once, and its relative paths are what
/// distinguishes four `README.md`s from each other.
#[tokio::test]
async fn a_folder_drop_keeps_each_file_apart_by_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;

    let (status, body) = drop_files(
        &state,
        &[
            (
                "docs/alpha/README.md",
                "text/markdown",
                b"Alpha service notes.",
            ),
            (
                "docs/beta/README.md",
                "text/markdown",
                b"Beta service notes.",
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stored"], 2);
    let sources: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, vec!["docs/alpha/README.md", "docs/beta/README.md"]);

    let rows = brain_rows(&state).await;
    let documents: Vec<&Value> = rows.iter().filter(|r| r["origin"] == "document").collect();
    assert_eq!(documents.len(), 2, "both files are their own memory");
}

/// One unreadable file in a folder must not fail the drop — a real folder
/// always contains a `.DS_Store` or an image — but it must be *reported*,
/// because silently skipping it leaves the operator believing the whole folder
/// is in memory.
#[tokio::test]
async fn an_unreadable_file_is_reported_without_failing_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;

    let (status, body) = drop_files(
        &state,
        &[
            ("notes.txt", "text/plain", b"Keep this."),
            (
                "logo.png",
                "image/png",
                &[0x89, b'P', b'N', b'G', 0x00, 0x01],
            ),
            ("blank.txt", "text/plain", b"   "),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stored"], 1);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["status"], "stored");
    assert_eq!(items[1]["status"], "unsupported");
    assert!(items[1]["detail"].is_string(), "the refusal says why");
    assert_eq!(
        items[2]["status"], "empty",
        "a file that was read and said nothing is not a failure"
    );
}

/// A drop with no file parts is a bad request rather than an empty success:
/// reporting "0 stored" for a request that carried nothing hides a console bug.
#[tokio::test]
async fn a_drop_with_no_files_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;
    let (status, _) = drop_files(&state, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A body of `prefix` + `payload` zero bytes + `suffix`, streamed in 1 MiB
/// frames with a yield before each — so the *test* never holds the whole
/// payload in memory even when it is well past the route's limit. Modeled on
/// `server::ops::write_test`'s `streamed_multipart`, which exists for the same
/// reason: a contiguous body would make proving a body-limit rejection cost as
/// much memory as the rejection is supposed to save.
fn streamed_multipart(prefix: Vec<u8>, payload: usize, suffix: Vec<u8>) -> Body {
    const FRAME: usize = 1024 * 1024;
    let prefix = Arc::new(prefix);
    let suffix = Arc::new(suffix);
    let filler = bytes::Bytes::from(vec![0u8; FRAME]);
    let frames = payload.div_ceil(FRAME);

    let stream = futures::stream::unfold(0usize, move |step| {
        let prefix = prefix.clone();
        let suffix = suffix.clone();
        let filler = filler.clone();
        async move {
            tokio::task::yield_now().await;
            let frame = if step == 0 {
                bytes::Bytes::from(prefix.as_ref().clone())
            } else if step <= frames {
                filler.slice(..(payload - (step - 1) * FRAME).min(FRAME))
            } else if step == frames + 1 {
                bytes::Bytes::from(suffix.as_ref().clone())
            } else {
                return None;
            };
            Some((Ok::<_, std::io::Error>(frame), step + 1))
        }
    });
    Body::from_stream(stream)
}

/// A whole request over [`INGEST_BODY_LIMIT`](super::INGEST_BODY_LIMIT) is cut
/// off before anything is read: `multipart_error` names this specifically as a
/// 413 with a "drop it in smaller batches" remedy, distinct from the plain
/// malformed-request 400 every other multipart failure gets. Only the per-file
/// cap (`an_unreadable_file_is_reported_without_failing_the_batch`'s sibling
/// tests) and the empty-drop 400 were covered before this; the whole-request
/// ceiling itself never had a request built to trip it.
#[tokio::test]
async fn a_request_over_the_body_limit_is_refused_as_413_not_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;

    let prefix = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"huge.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    let suffix = format!("\r\n--{BOUNDARY}--\r\n").into_bytes();
    // One byte past the 200 MiB ceiling (8 * MAX_DOCUMENT_BYTES).
    let oversize = 8 * 25 * 1024 * 1024 + 1;

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/company/memory/ingest")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(streamed_multipart(prefix, oversize, suffix))
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(
        body["code"], "workspace_quota_exceeded",
        "the body-limit refusal shares the platform's one \"too big\" code, not the generic \
         invalid_request one: {body}"
    );
    let message = body["error"].as_str().expect("an error message");
    assert!(
        message.contains("smaller batches"),
        "the body-limit refusal must name its own remedy, not the generic \
         malformed-request message: {message}"
    );
    assert!(
        !message.contains("Error parsing"),
        "an overrun body must not be reported as malformed: {message}"
    );
}

/// The early, spelling-only refusals: schemes, literal internal addresses and
/// internal names. What a name *resolves* to is judged by TinyMemory's pinned
/// resolver at connect time, and is tested there — as is the IPv4-compatible
/// `::127.0.0.1` form (tinyhumansai/tinymemory#190).
#[cfg(feature = "documents")]
#[test]
fn link_ingestion_refuses_this_deployments_own_network() {
    for refused in [
        "http://localhost:8080/admin",
        "http://127.0.0.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5/",
        "http://192.168.1.1/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://metadata.internal/",
        "file:///etc/passwd",
        "ftp://example.com/x",
        "not a url",
    ] {
        assert!(
            super::link_refusal(refused).is_err(),
            "{refused} must be refused"
        );
    }
    assert!(super::link_refusal("https://93.184.216.34/pricing").is_ok());
    assert!(super::link_refusal("https://example.com/pricing").is_ok());
}

/// Dropping the wrong folder is a mistake an operator makes once; without a
/// forget, the only remedy would be the company's whole memory.
#[tokio::test]
async fn a_dropped_document_can_be_forgotten_again() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;
    drop_files(
        &state,
        &[
            ("private/salaries.csv", "text/csv", b"name,amount\nada,100"),
            ("keep.md", "text/markdown", b"Keep this one."),
        ],
    )
    .await;

    let request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/api/v1/company/memory/document/{}",
            crate::ingest::label_for("private/salaries.csv")
                .strip_prefix("document/")
                .unwrap()
        ))
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let rows = brain_rows(&state).await;
    let documents: Vec<&str> = rows
        .iter()
        .filter(|r| r["origin"] == "document")
        .map(|r| r["source"].as_str().unwrap())
        .collect();
    assert_eq!(
        documents,
        vec!["keep.md"],
        "only the named document is forgotten"
    );
}

/// Forgetting reaches document memory and nothing else: an agent's own memory
/// is a record of what happened, not material an operator supplied.
#[tokio::test]
async fn forgetting_an_unknown_document_is_a_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_at(dir.path()).await;
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/company/memory/document/never-uploaded")
        .header("cookie", crate::server::test_support::fixed_cookie("acme"))
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
