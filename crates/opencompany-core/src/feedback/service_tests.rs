use std::sync::Arc;

use super::*;
use crate::feedback::github::MockGitHubClient;
use crate::feedback::store::FeedbackStore;
use crate::feedback::triage::Severity;
use crate::feedback::types::{ConsentMode, FeedbackCategory, FeedbackInput};
use crate::ports::types::SecretValue;
use crate::store::FsSecretStore;
use crate::store::paths::Bundle;

/// A manifest whose roster lists `dana_roe` only when `with_roster` is set —
/// the lever a test uses to simulate config changing between preview and
/// confirm.
fn manifest(with_roster: bool) -> CompanyManifest {
    let roster = if with_roster {
        "[[agent]]\nid = \"dana_roe\"\nrole = \"Analyst\"\n"
    } else {
        ""
    };
    toml::from_str(&format!(
        r#"
        [company]
        name = "Acme"
        {roster}
        [policy]
        mode = "full"
        "#
    ))
    .unwrap()
}

fn harness() -> (tempfile::TempDir, FeedbackStore, FsSecretStore, CompanyId) {
    let root = tempfile::Builder::new()
        .prefix("oc-feedback-")
        .tempdir()
        .expect("tempdir");
    let company = CompanyId::new("acme");
    let bundle = Bundle::new(root.path().to_path_buf(), &company);
    let secrets = FsSecretStore::new(root.path().to_path_buf());
    (root, FeedbackStore::new(&bundle), secrets, company)
}

fn filer(github: Arc<MockGitHubClient>) -> FeedbackFiler {
    FeedbackFiler {
        client: Some(github),
        consent: ConsentMode::Auto,
        ..FeedbackFiler::default()
    }
}

fn item(note: &str) -> FeedbackItem {
    FeedbackItem::capture(
        FeedbackInput {
            category: FeedbackCategory::Bug,
            note: note.into(),
            work_ref: None,
            template_name: None,
            template_version: None,
        },
        crate::VERSION,
        ConsentMode::Auto,
    )
}

/// The confirm of a previewed item posts the persisted preview body
/// byte-for-byte, even when the config that produced the preview has since
/// changed. A re-derivation under the new (smaller) roster would post the
/// raw agent name the operator never saw unredacted; the confirm must not.
#[tokio::test]
async fn confirm_posts_exact_previewed_body_after_roster_shrinks() {
    let (_root, store, secrets, company) = harness();
    let github = Arc::new(MockGitHubClient::new());
    let filer = filer(github.clone());
    let it = item("dana_roe produced the wrong invoice");
    store.append(&it).await.unwrap();

    let preview = finalize(
        &store,
        &secrets,
        &filer,
        &company,
        Some(&manifest(true)),
        &it,
        Severity::Annoyance,
        FeedbackSource::Operator,
        true,
    )
    .await
    .unwrap();
    let preview_body = preview.preview_body.expect("preview body");
    assert!(
        preview_body.contains("⟨redacted:name⟩"),
        "got {preview_body}"
    );
    assert!(!preview_body.contains("dana_roe"));
    // The preview persisted its body on the item; the confirm finalizes the
    // STORED item, exactly as the runtime's confirm path loads it.
    let stored = store.get(&it.id).await.unwrap().expect("item exists");

    // Between preview and confirm the roster loses the agent. The confirm
    // must still post exactly what the operator inspected.
    let confirm = finalize(
        &store,
        &secrets,
        &filer,
        &company,
        Some(&manifest(false)),
        &stored,
        Severity::Annoyance,
        FeedbackSource::Operator,
        false,
    )
    .await
    .unwrap();
    assert!(confirm.filed, "confirm should file");
    let created = github.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].body, preview_body, "byte-exact previewed body");
    assert!(!created[0].body.contains("dana_roe"));
}

/// A value that became a secret after the preview still aborts the confirm
/// (fail closed): the persisted body is re-gated, and a now-present secret
/// value is refused rather than posted.
#[tokio::test]
async fn confirm_aborts_when_a_secret_appears_after_the_preview() {
    let (_root, store, secrets, company) = harness();
    let github = Arc::new(MockGitHubClient::new());
    let filer = filer(github.clone());
    // The token is short, so it is not flagged by the entropy scan at
    // preview time, and it is not yet a stored secret.
    let it = item("the ghp_newtoken broke the run");
    store.append(&it).await.unwrap();

    let preview = finalize(
        &store,
        &secrets,
        &filer,
        &company,
        Some(&manifest(false)),
        &it,
        Severity::Annoyance,
        FeedbackSource::Operator,
        true,
    )
    .await
    .unwrap();
    let preview_body = preview.preview_body.expect("preview body");
    assert!(preview_body.contains("ghp_newtoken"));
    let stored = store.get(&it.id).await.unwrap().expect("item exists");

    // The value is now a stored secret: the confirm must refuse to post it.
    secrets
        .set(&company, "github_token", SecretValue("ghp_newtoken".into()))
        .await
        .unwrap();
    let confirm = finalize(
        &store,
        &secrets,
        &filer,
        &company,
        Some(&manifest(false)),
        &stored,
        Severity::Annoyance,
        FeedbackSource::Operator,
        false,
    )
    .await
    .unwrap();
    assert!(
        confirm.blocked,
        "a new secret value aborts, got {confirm:?}"
    );
    assert!(github.created().is_empty());
}

/// When the config would now redact more than the approved preview showed,
/// the confirm refuses to post either the approved bytes or the re-gated
/// ones, and asks for a fresh preview instead.
#[tokio::test]
async fn confirm_asks_for_repreview_when_config_would_redact_more() {
    let (_root, store, secrets, company) = harness();
    let github = Arc::new(MockGitHubClient::new());
    let filer = filer(github.clone());
    // Previewed under a manifest whose roster does NOT list dana_roe, so the
    // approved body shows the raw agent name.
    let it = item("dana_roe produced the wrong invoice");
    store.append(&it).await.unwrap();

    let preview = finalize(
        &store,
        &secrets,
        &filer,
        &company,
        Some(&manifest(false)),
        &it,
        Severity::Annoyance,
        FeedbackSource::Operator,
        true,
    )
    .await
    .unwrap();
    assert!(preview.preview_body.unwrap().contains("dana_roe"));
    let stored = store.get(&it.id).await.unwrap().expect("item exists");

    // The roster now lists dana_roe: re-gating the approved body would alter
    // it, so the confirm blocks with a re-preview reason and posts nothing.
    let confirm = finalize(
        &store,
        &secrets,
        &filer,
        &company,
        Some(&manifest(true)),
        &stored,
        Severity::Annoyance,
        FeedbackSource::Operator,
        false,
    )
    .await
    .unwrap();
    assert!(confirm.blocked, "config change must block, got {confirm:?}");
    let reason = confirm.reason.expect("a reason");
    assert!(reason.contains("preview again"), "got {reason}");
    assert!(github.created().is_empty());
}
