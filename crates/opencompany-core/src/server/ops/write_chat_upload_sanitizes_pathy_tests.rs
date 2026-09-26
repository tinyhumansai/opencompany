//! Integration tests for the `ops` write plane: tasks, memory, workspace,
//! skills, team, inbox-read, and desk chat — exercised end-to-end over the
//! router against a real fs-backed company.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use super::write_test_support::*;
use crate::ports::types::CompanyId;
use crate::server::router;

/// The history route for the default agent's DM, where a message with no
/// `chat` lands.
async fn default_history_uri(state: &crate::AppState) -> String {
    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let dm = runtime
        .default_agent_dm()
        .await
        .unwrap()
        .expect("the company has a default agent");
    format!("/api/v1/company/chat/history?desk={dm}")
}

/// A browser may send a full path as the filename; the route stores under the
/// last segment only, named by the workspace rule — the same sanitizer the
/// workspace upload applies, so no client string reaches a filesystem path.
#[tokio::test]
async fn chat_upload_sanitizes_pathy_filename() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let bytes: Vec<u8> = vec![0x00, 0x01, 0x02, 0xff];
    let (status, reference) = chat_upload(
        &state,
        "../../etc/Secret Report.bin",
        Some("application/octet-stream"),
        &bytes,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let name = reference["name"].as_str().unwrap();
    assert!(
        !name.contains('/') && !name.contains('\\'),
        "the stored name kept a path separator: {name}"
    );
    assert_eq!(
        name, "secret-report.bin",
        "stored under the sanitized last segment"
    );
}

/// Codex review finding on #1682: chat uploads all land at the workspace
/// root, so a second message attaching a file under an earlier one's exact
/// name — the common case of picking `image.png` twice — used to 409 rather
/// than attach, since the only way to free the name was deleting the first
/// upload and breaking its download. The route now retries once under a
/// disambiguated name instead of failing the attach.
#[tokio::test]
async fn chat_upload_disambiguates_a_repeated_filename() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let first: Vec<u8> = vec![0x01, 0x02, 0x03];
    let (status, first_ref) = chat_upload(&state, "image.png", Some("image/png"), &first).await;
    assert_eq!(status, StatusCode::OK, "{first_ref}");
    assert_eq!(first_ref["name"], "image.png");

    // A later message attaches a *different* file under the same filename.
    let second: Vec<u8> = vec![0x09, 0x08, 0x07, 0x06];
    let (status, second_ref) = chat_upload(&state, "image.png", Some("image/png"), &second).await;
    assert_eq!(status, StatusCode::OK, "{second_ref}");
    let second_name = second_ref["name"].as_str().expect("a stored name");
    assert_ne!(
        second_name, "image.png",
        "the second upload must not silently fail or overwrite the first"
    );
    assert!(
        second_name.starts_with("image-") && second_name.ends_with(".png"),
        "expected a disambiguated image-*.png name, got {second_name}"
    );

    // Both node ids are distinct, live in the tree, and stream back their own
    // (not each other's) bytes — no data was lost or aliased on the collision.
    let first_id = first_ref["nodeId"].as_str().unwrap();
    let second_id = second_ref["nodeId"].as_str().unwrap();
    assert_ne!(first_id, second_id);
    for (node_id, want) in [(first_id, &first), (second_id, &second)] {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/company/workspace/blob/{node_id}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let got = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&got.to_vec(), want);
    }
}

/// The headline of #1682 end-to-end: an operator attaches a file, the message
/// carries it, and a reload projects the attachment back with the **store's**
/// name / mime / size — never a client claim, because `/chat` was handed only
/// the node id. This is the reload proof the whole two-step design exists for.
#[tokio::test]
async fn chat_message_with_attachment_journals_and_hydrates() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x01];
    let (status, reference) = chat_upload(&state, "diagram.png", Some("image/png"), &png).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    // The send carries the id only — no name, mime or size the host could be
    // tricked into trusting.
    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "here is the diagram", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The reload: the operator's own message comes back with the attachment,
    // and every field is the store's.
    let (status, history) = send(&state, "GET", &default_history_uri(&state).await, None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "here is the diagram")
        .expect("the operator message survived the reload");
    let attachments = mine["attachments"]
        .as_array()
        .expect("the message carries its attachment on reload");
    assert_eq!(attachments.len(), 1, "exactly one attachment: {mine}");
    let attachment = &attachments[0];
    assert_eq!(attachment["nodeId"], node_id.as_str());
    assert_eq!(attachment["name"], "diagram.png", "name is the store's");
    assert_eq!(attachment["mime"], "image/png", "mime is the store's");
    assert_eq!(attachment["size"], png.len() as u64, "size is the store's");
}

/// Codex review finding on #1682, round 2: a bare node id told a hosted or
/// sidecar brain a file existed but gave it nothing to act on — no device
/// tool bridges a `context_*` call into the workspace's binary store. The
/// send route now extracts a readable attachment's text and journals it
/// alongside the reference, so `wire_event` (`brain::medulla::effects`) has
/// real content to put on the wire.
///
/// Reads the raw journal rather than `/chat/history` on purpose:
/// `extracted_text` is an internal server-to-brain channel, not operator-
/// facing data, so `ChatAttachmentDto` deliberately drops it — the console
/// never sees it and must not.
#[tokio::test]
async fn chat_attachment_text_is_extracted_and_journaled_for_the_brain() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let text = b"Q3 revenue grew 12% year over year.".to_vec();
    let (status, reference) = chat_upload(&state, "report.txt", Some("text/plain"), &text).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "summarize the attached report", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");

    assert_eq!(journaled.len(), 1);
    assert_eq!(
        journaled[0].extracted_text.as_deref(),
        Some("Q3 revenue grew 12% year over year."),
        "a plain-text attachment's content must reach the durable event, \
         not just its node id"
    );
}

/// A binary attachment nothing here parses (an image) journals with no
/// extracted text — the reference alone rides the wire, and honestly: no
/// content is fabricated for a format extraction cannot read.
#[tokio::test]
async fn chat_attachment_with_no_readable_text_journals_none() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x01];
    let (status, reference) = chat_upload(&state, "photo.png", Some("image/png"), &png).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "what's in this photo?", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");

    assert_eq!(journaled.len(), 1);
    assert_eq!(journaled[0].extracted_text, None);
}

/// The IDOR / phantom guard: a `node_id` that resolves to no binary node in
/// this company's workspace refuses the send with a `400`, on the same terms a
/// malformed thread `parent` does — so a stale or hostile client cannot attach
/// another company's file, or a file that does not exist.
#[tokio::test]
async fn chat_message_rejects_foreign_node() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "trust me", "attachments": ["01JZZZNOTAREALNODE00000000"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And nothing was journaled: the refusal is before the append, so the
    // transcript does not hold a message pointing at a file this company lacks.
    let (status, history) = send(&state, "GET", &default_history_uri(&state).await, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        history
            .as_array()
            .expect("history is a list")
            .iter()
            .all(|m| m["text"] != "trust me"),
        "a refused attachment message still reached the transcript: {history}"
    );
}
/// Codex review finding: an unbounded attachment list turns one `/chat` POST
/// into an attacker-controlled multiplier on `resolve_attachments`' tree scan
/// and extraction work. Refused with a `400` before any of that work runs.
#[tokio::test]
async fn chat_message_rejects_too_many_attachments() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let ids: Vec<String> = (0..21).map(|n| format!("node-{n}")).collect();
    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "too many", "attachments": ids })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, history) = send(&state, "GET", &default_history_uri(&state).await, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        history
            .as_array()
            .expect("history is a list")
            .iter()
            .all(|m| m["text"] != "too many"),
        "a refused attachment message still reached the transcript: {history}"
    );
}

/// Codex review finding: the same node id repeated in `attachments` used to
/// resolve — and extract — once per repetition. A message attaching the same
/// file three times over carries it exactly once.
#[tokio::test]
async fn chat_message_deduplicates_a_repeated_attachment_id() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let text = b"Q3 revenue grew 12%.".to_vec();
    let (status, reference) = chat_upload(&state, "report.txt", Some("text/plain"), &text).await;
    assert_eq!(status, StatusCode::OK, "{reference}");
    let node_id = reference["nodeId"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({
            "message": "attached three times",
            "attachments": [node_id.clone(), node_id.clone(), node_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, history) = send(&state, "GET", &default_history_uri(&state).await, None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "attached three times")
        .expect("the message survived");
    assert_eq!(
        mine["attachments"].as_array().map(Vec::len),
        Some(1),
        "a repeated id must resolve to one attachment, not three: {mine}"
    );
}

// ---------------------------------------------------------------------------
// Prose attachments (issue #2029)
// ---------------------------------------------------------------------------

/// A markdown file uploaded through the console's own workspace upload — the
/// route that stores a UTF-8 `text/*` payload as a prose note — attaches to a
/// chat message and hydrates with the store's own name, mime and size.
///
/// The issue's case (a) verbatim: the upload endpoint accepted the file and
/// returned its id, and the send route refused that id as "not a file in this
/// company's workspace".
#[tokio::test]
async fn chat_message_attaches_a_note_uploaded_to_the_workspace() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let markdown = b"# Q3 notes\n\nRevenue grew 12% year over year.\n".to_vec();
    let (status, uploaded) = upload_file(
        &state,
        "q3-notes.md",
        Some("text/markdown"),
        &markdown,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    assert!(
        uploaded["mime"].is_null(),
        "the upload route stores a UTF-8 text payload as a prose note: {uploaded}"
    );
    let node_id = uploaded["id"].as_str().expect("a node id").to_string();

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "notes attached", "attachments": [node_id.clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, history) = send(&state, "GET", &default_history_uri(&state).await, None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "notes attached")
        .expect("the message survived");
    let attachments = mine["attachments"]
        .as_array()
        .expect("the message carries its attachment on reload");
    assert_eq!(attachments.len(), 1, "exactly one attachment: {mine}");
    assert_eq!(attachments[0]["nodeId"], node_id.as_str());
    assert_eq!(attachments[0]["name"], "q3-notes.md", "name is the store's");
    assert_eq!(
        attachments[0]["mime"], "text/markdown",
        "a prose note's mime is guessed from the store's own name"
    );
    assert_eq!(
        attachments[0]["size"],
        markdown.len() as u64,
        "size is the note's content length"
    );
}

/// The issue's case (b): a note already sitting in the company workspace —
/// seeded, agent-authored or created from the console — attaches on the same
/// terms. Nothing about a chat attachment requires the file to have arrived
/// through an upload.
#[tokio::test]
async fn chat_message_attaches_a_note_already_in_the_workspace() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({ "name": "roadmap.md", "kind": "file", "content": "Ship the thing." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let node_id = created["id"].as_str().expect("a node id").to_string();

    let (status, body) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "roadmap attached", "attachments": [node_id.clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, history) = send(&state, "GET", &default_history_uri(&state).await, None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|m| m["text"] == "roadmap attached")
        .expect("the message survived");
    let attachments = mine["attachments"].as_array().expect("attachments");
    assert_eq!(attachments.len(), 1, "{mine}");
    assert_eq!(attachments[0]["nodeId"], node_id.as_str());
    assert_eq!(attachments[0]["name"], "roadmap.md");
    assert_eq!(attachments[0]["size"], "Ship the thing.".len() as u64);
}

/// A prose attachment's own words reach the durable event, the same as an
/// uploaded text blob's do — otherwise the brain is handed a filename and
/// nothing to read.
#[tokio::test]
async fn chat_attachment_note_text_is_extracted_and_journaled() {
    let home_dir = home();
    let home = home_dir.path().to_path_buf();
    let state = state_with_company(&home).await;

    let (status, created) = send(
        &state,
        "POST",
        "/api/v1/company/workspace",
        Some(json!({
            "name": "brief.md",
            "kind": "file",
            "content": "Q3 revenue grew 12% year over year.",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let node_id = created["id"].as_str().unwrap().to_string();

    let (status, _) = send(
        &state,
        "POST",
        "/api/v1/company/chat",
        Some(json!({ "message": "summarize the brief", "attachments": [node_id] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
    let journaled = runtime
        .events()
        .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .find_map(|s| match s.event {
            crate::ports::types::CompanyEvent::OperatorMessage { attachments, .. }
                if !attachments.is_empty() =>
            {
                Some(attachments)
            }
            _ => None,
        })
        .expect("the message with an attachment is in the journal");

    assert_eq!(journaled.len(), 1);
    assert_eq!(
        journaled[0].extracted_text.as_deref(),
        Some("Q3 revenue grew 12% year over year."),
        "a note's content must reach the durable event, not just its node id"
    );
}
