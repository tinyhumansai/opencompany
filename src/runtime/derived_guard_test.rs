//! That the folder is closed to hand edits, from every direction, and that the
//! runtime's own derivation still gets through.

use serde_json::json;

use crate::company::ledgers;
use crate::company::runtime::CompanyRuntime;
use crate::ledger::LedgerAuthor;
use crate::ports::types::CompanyId;
use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};
use crate::ports::{generate_id, now_millis};

async fn runtime() -> (CompanyRuntime, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
        [company]
        name = "Acme"

        [[agent]]
        id = "ceo"
        role = "Chief"

        [policy]
        mode = "supervised"
        "#,
    )
    .expect("manifest");
    let runtime = crate::runtime::RuntimeBuilder::new(home.path().to_path_buf(), manifest)
        .with_id(CompanyId::new("acme"))
        .build()
        .await
        .expect("runtime");
    (runtime, home)
}

/// Declares a ledger, which publishes its file — the state every test here
/// starts from.
async fn with_a_ledger(runtime: &CompanyRuntime) -> (String, String) {
    ledgers::define(
        &ledgers::Ledgers::from(runtime),
        &json!({
            "slug": "hazards",
            "title": "Hazards",
            "derived": "derived/hazards.md",
            "fields": [
                { "name": "id", "role": "id" },
                { "name": "risk", "role": "title" },
                { "name": "status", "role": "status" }
            ],
            "statuses": [{ "name": "open" }]
        }),
    )
    .await
    .expect("declared");
    let tree = runtime.workspace().tree(runtime.id()).await.expect("tree");
    let folder = tree
        .iter()
        .find(|node| node.name == "derived")
        .expect("folder")
        .id
        .clone();
    let file = tree
        .iter()
        .find(|node| node.name == "hazards.md")
        .expect("file")
        .id
        .clone();
    (folder, file)
}

/// The failure this guard exists to prevent: an edit that lands, reads
/// correctly, and is erased by the next derivation with nothing saying so.
#[tokio::test]
async fn an_operator_cannot_overwrite_a_derived_file() {
    let (runtime, _home) = runtime().await;
    let (_folder, file) = with_a_ledger(&runtime).await;

    let error = runtime
        .workspace()
        .write(
            runtime.id(),
            &file,
            "my own notes",
            WorkspaceOrigin::Operator,
        )
        .await
        .expect_err("refused");
    let message = format!("{error}");
    assert!(message.contains("hazards"), "{message}");
    assert!(message.contains("record_entry"), "{message}");
}

#[tokio::test]
async fn an_agent_cannot_either() {
    let (runtime, _home) = runtime().await;
    let (_folder, file) = with_a_ledger(&runtime).await;
    assert!(
        runtime
            .workspace()
            .write(
                runtime.id(),
                &file,
                "notes",
                WorkspaceOrigin::Agent { id: "ceo".into() },
            )
            .await
            .is_err()
    );
}

/// The other direction: a hand-written file placed *into* the folder whose
/// whole meaning is that nothing in it is hand-written.
#[tokio::test]
async fn nothing_may_be_created_inside_the_folder_by_hand() {
    let (runtime, _home) = runtime().await;
    let (folder, _file) = with_a_ledger(&runtime).await;

    let node = WorkspaceNode {
        id: generate_id(),
        name: "MY-NOTES.md".to_string(),
        kind: NodeKind::File,
        parent_id: Some(folder.clone()),
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    let error = runtime
        .workspace()
        .create(runtime.id(), &node, Some("hello"))
        .await
        .expect_err("refused");
    // Nothing claims that file, so the message falls back to the folder's rule
    // and points at the catalogue.
    assert!(format!("{error}").contains("list_ledgers"), "{error}");
}

/// A second `derived` beside the real one would be a writable folder with the
/// authoritative name.
#[tokio::test]
async fn the_folder_name_itself_cannot_be_claimed_by_hand() {
    let (runtime, _home) = runtime().await;
    let node = WorkspaceNode {
        id: generate_id(),
        name: "derived".to_string(),
        kind: NodeKind::Folder,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    assert!(
        runtime
            .workspace()
            .create(runtime.id(), &node, None)
            .await
            .is_err()
    );
    assert!(
        runtime
            .workspace()
            .adopt_or_create_folder(runtime.id(), None, "derived", WorkspaceOrigin::Operator)
            .await
            .is_err()
    );
}

/// Moving a derived file out strands a file the next derivation recreates;
/// moving an ordinary note in puts a hand-written file in the folder.
#[tokio::test]
async fn a_derived_file_can_be_moved_neither_out_nor_in() {
    let (runtime, _home) = runtime().await;
    let (folder, file) = with_a_ledger(&runtime).await;

    assert!(
        runtime
            .workspace()
            .rename_move(runtime.id(), &file, Some("MINE.md"), Some(None))
            .await
            .is_err(),
        "a derived file must not be moved out"
    );

    let note = WorkspaceNode {
        id: generate_id(),
        name: "note.md".to_string(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    runtime
        .workspace()
        .create(runtime.id(), &note, Some("hello"))
        .await
        .expect("an ordinary note is fine");
    assert!(
        runtime
            .workspace()
            .rename_move(
                runtime.id(),
                &note.id,
                Some("note.md"),
                Some(Some(folder.as_str()))
            )
            .await
            .is_err(),
        "an ordinary note must not be moved in"
    );
}

/// The whole point of the guard is that the derivation still works. If this
/// fails, every ledger write silently stops publishing.
#[tokio::test]
async fn the_runtimes_own_derivation_still_writes() {
    let (runtime, _home) = runtime().await;
    let (_folder, file) = with_a_ledger(&runtime).await;
    let ctx = ledgers::Ledgers::from(&runtime);
    let registry = ledgers::registry(&ctx).await.expect("registry");
    let spec = registry.find("hazards").expect("declared");

    ledgers::record(
        &ctx,
        spec,
        &LedgerAuthor::agent("ceo"),
        "vendor-slip",
        [("risk".to_string(), Some("the vendor slips".to_string()))]
            .into_iter()
            .collect(),
    )
    .await
    .expect("recorded");

    let (_node, body) = runtime
        .workspace()
        .read(runtime.id(), &file)
        .await
        .expect("read")
        .expect("present");
    assert!(
        body.contains("vendor-slip"),
        "the derivation did not reach the file: {body}"
    );
}

/// Deliberately allowed. A delete is visible and recoverable — the next write
/// re-derives — and a retired ledger has to leave something somebody can clear.
#[tokio::test]
async fn a_derived_file_may_still_be_deleted() {
    let (runtime, _home) = runtime().await;
    let (_folder, file) = with_a_ledger(&runtime).await;
    assert!(
        runtime
            .workspace()
            .delete(runtime.id(), &file)
            .await
            .expect("deleted")
    );
}

/// The guard must cost nothing anywhere else in the tree.
#[tokio::test]
async fn ordinary_notes_are_untouched() {
    let (runtime, _home) = runtime().await;
    with_a_ledger(&runtime).await;
    let node = WorkspaceNode {
        id: generate_id(),
        name: "plan.md".to_string(),
        kind: NodeKind::File,
        parent_id: None,
        updated_at_millis: now_millis(),
        created_by: WorkspaceOrigin::Operator,
        updated_by: WorkspaceOrigin::Operator,
        mime: None,
        size: None,
        sha256: None,
        adopted: false,
    };
    runtime
        .workspace()
        .create(runtime.id(), &node, Some("hello"))
        .await
        .expect("created");
    runtime
        .workspace()
        .write(runtime.id(), &node.id, "goodbye", WorkspaceOrigin::Operator)
        .await
        .expect("written");
}
