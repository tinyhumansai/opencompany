//! Unit tests for [`crate::harness::publish`].
//!
//! These pin the tool's *own* behaviour — validation, kind inference, capture,
//! queue semantics, scan bounds and the nudge's wording. Whether the tool is
//! reachable from a real model-driven turn is a different question, and it is
//! answered by `publish_turn_test.rs`.

use super::*;
use serde_json::json;

/// A workspace with the given `path → contents` files written into it.
fn workspace(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }
    dir
}

async fn run(tool: &PublishArtifactTool, args: serde_json::Value) -> ToolResult {
    tool.execute(args).await.expect("the tool never propagates")
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .map(|c| match c {
            oh::skills::types::ToolContent::Text { text } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Path validation ───────────────────────────────────────────────────────

/// The headline: a file the agent wrote resolves, and its `source` is the
/// normalized workspace-relative path that becomes half the artifact's
/// identity.
#[test]
fn a_workspace_file_resolves_to_its_relative_source() {
    let dir = workspace(&[("specs/launch.md", b"# Spec")]);
    let (file, source) = resolve_in_workspace(dir.path(), "specs/launch.md").unwrap();
    assert!(file.is_file());
    assert_eq!(source, "specs/launch.md");
}

/// Identity must not depend on how the agent spelled the path, or a re-run that
/// wrote `./specs/launch.md` would open a second lineage for one file.
#[test]
fn an_equivalent_spelling_produces_the_same_identity() {
    let dir = workspace(&[("specs/launch.md", b"# Spec")]);
    let (_, direct) = resolve_in_workspace(dir.path(), "specs/launch.md").unwrap();
    let (_, roundabout) = resolve_in_workspace(dir.path(), "./specs/../specs/launch.md").unwrap();
    assert_eq!(direct, roundabout);
}

#[test]
fn traversal_and_absolute_paths_are_refused() {
    let dir = workspace(&[("specs/launch.md", b"# Spec")]);
    // Climbing out, in the obvious shape…
    assert_eq!(
        resolve_in_workspace(dir.path(), "../outside.md"),
        Err(PublishPathError::Missing),
        "nothing is there, and it would be outside if it were"
    );
    // …and where the target genuinely exists outside the workspace.
    let sibling = dir.path().parent().unwrap().join("outside.md");
    std::fs::write(&sibling, b"secret").unwrap();
    assert_eq!(
        resolve_in_workspace(dir.path(), "../outside.md"),
        Err(PublishPathError::Outside)
    );
    let _ = std::fs::remove_file(&sibling);

    assert_eq!(
        resolve_in_workspace(dir.path(), "/etc/hosts"),
        Err(PublishPathError::Outside)
    );
    assert_eq!(
        resolve_in_workspace(dir.path(), "  "),
        Err(PublishPathError::Empty)
    );
}

/// The reason containment is a canonicalize-then-prefix check and not a `..`
/// scan: a symlink inside the workspace has no `..` in it at all.
#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_workspace_is_refused() {
    let dir = workspace(&[("specs/launch.md", b"# Spec")]);
    let outside = dir.path().parent().unwrap().join("escape-target.md");
    std::fs::write(&outside, b"not yours").unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join("escape.md")).unwrap();

    assert_eq!(
        resolve_in_workspace(dir.path(), "escape.md"),
        Err(PublishPathError::Outside),
        "a symlink is a path that contains no `..` and still leaves the sandbox"
    );
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn a_missing_file_and_a_directory_are_different_mistakes() {
    let dir = workspace(&[("specs/launch.md", b"# Spec")]);
    assert_eq!(
        resolve_in_workspace(dir.path(), "specs/nope.md"),
        Err(PublishPathError::Missing)
    );
    assert_eq!(
        resolve_in_workspace(dir.path(), "specs"),
        Err(PublishPathError::NotAFile)
    );
}

/// Every refusal has to tell the agent what to do next — a tool error that only
/// says "no" costs a whole turn to recover from.
#[test]
fn every_path_error_names_a_next_step() {
    for err in [
        PublishPathError::Empty,
        PublishPathError::Outside,
        PublishPathError::Missing,
        PublishPathError::NotAFile,
    ] {
        let message = err.message("specs/launch.md");
        assert!(message.len() > 40, "{err:?}: {message}");
        assert!(
            message.contains("publish") || message.contains("Publish") || message.contains("path"),
            "{err:?}: {message}"
        );
    }
}

// ── Kind + capture ────────────────────────────────────────────────────────

#[test]
fn kind_is_inferred_from_the_extension() {
    use std::path::Path;
    assert_eq!(
        kind_for_extension(Path::new("a/launch.md")),
        ArtifactKind::Markdown
    );
    assert_eq!(
        kind_for_extension(Path::new("a/notes.txt")),
        ArtifactKind::Text
    );
    assert_eq!(
        kind_for_extension(Path::new("a/chart.png")),
        ArtifactKind::Image
    );
    assert_eq!(
        kind_for_extension(Path::new("a/data.parquet")),
        ArtifactKind::File
    );
    // No extension at all is a file, not a guess at prose.
    assert_eq!(
        kind_for_extension(Path::new("a/Makefile")),
        ArtifactKind::File
    );
    // Case does not decide anything.
    assert_eq!(
        kind_for_extension(Path::new("a/READ.MD")),
        ArtifactKind::Markdown
    );
}

#[test]
fn text_at_or_under_the_cap_is_stored_whole() {
    let body = "x".repeat(MAX_ARTIFACT_BODY_BYTES);
    let dir = workspace(&[("big.txt", body.as_bytes())]);
    let captured =
        capture_body(&dir.path().join("big.txt"), "big.txt", ArtifactKind::Text).unwrap();
    assert!(
        !captured.is_reference,
        "exactly at the cap must still inline"
    );
    assert_eq!(captured.body.len(), MAX_ARTIFACT_BODY_BYTES);
    assert_eq!(captured.forced_kind, None);
}

/// One byte over the cap flips to a reference. The boundary is asserted from
/// both sides because an off-by-one here silently truncates a deliverable.
#[test]
fn one_byte_over_the_cap_becomes_a_reference() {
    let body = "x".repeat(MAX_ARTIFACT_BODY_BYTES + 1);
    let dir = workspace(&[("big.txt", body.as_bytes())]);
    let captured =
        capture_body(&dir.path().join("big.txt"), "big.txt", ArtifactKind::Text).unwrap();
    assert!(captured.is_reference);
    assert_eq!(
        captured.forced_kind,
        Some(ArtifactKind::File),
        "a reference must not be filed under a kind the console renders as prose"
    );
    assert!(captured.body.contains("path: big.txt"), "{}", captured.body);
    assert!(
        captured
            .body
            .contains(&format!("bytes: {}", MAX_ARTIFACT_BODY_BYTES + 1)),
        "{}",
        captured.body
    );
    assert!(captured.body.contains("sha256: "), "{}", captured.body);
    // Never silently-truncated content presented as complete.
    assert!(
        !captured.body.contains(&"x".repeat(100)),
        "the reference must not carry a slice of the content"
    );
}

#[test]
fn a_non_utf8_file_becomes_a_reference_whatever_its_size() {
    let dir = workspace(&[("logo.png", &[0x89, 0x50, 0x4e, 0x47, 0xff, 0xfe])]);
    let captured = capture_body(
        &dir.path().join("logo.png"),
        "logo.png",
        ArtifactKind::Image,
    )
    .unwrap();
    assert!(captured.is_reference);
    assert_eq!(
        captured.forced_kind,
        Some(ArtifactKind::Image),
        "an image reference stays an image so the console picks the right renderer"
    );
    assert!(captured.body.contains("not UTF-8"), "{}", captured.body);
}

/// The digest is of the bytes, so two publishes of the same content agree and a
/// changed file does not.
#[test]
fn the_reference_digest_tracks_the_bytes() {
    let dir = workspace(&[("a.bin", &[0xff, 0x00]), ("b.bin", &[0xff, 0x00])]);
    let a = capture_body(&dir.path().join("a.bin"), "a.bin", ArtifactKind::File).unwrap();
    let b = capture_body(&dir.path().join("b.bin"), "b.bin", ArtifactKind::File).unwrap();
    let sha = |body: &str| {
        body.lines()
            .find_map(|l| l.strip_prefix("sha256: "))
            .unwrap()
            .to_string()
    };
    assert_eq!(sha(&a.body), sha(&b.body));

    std::fs::write(dir.path().join("b.bin"), [0xff, 0x01]).unwrap();
    let changed = capture_body(&dir.path().join("b.bin"), "b.bin", ArtifactKind::File).unwrap();
    assert_ne!(sha(&a.body), sha(&changed.body));
}

// ── The tool ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn publishing_stages_the_file_and_reports_what_was_captured() {
    let dir = workspace(&[("specs/launch.md", b"# Spec\nShip it.")]);
    let queue = PendingPublishQueue::default();
    let tool = PublishArtifactTool::new(dir.path(), queue.clone());

    let result = run(&tool, json!({ "path": "specs/launch.md" })).await;
    assert!(!result.is_error, "{}", text_of(&result));

    let staged = queue.drain();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].source, "specs/launch.md");
    assert_eq!(staged[0].kind, ArtifactKind::Markdown);
    assert_eq!(staged[0].body, "# Spec\nShip it.");
    // Title defaults to the file name, not the whole path.
    assert_eq!(staged[0].title, "launch.md");
    assert_eq!(staged[0].note, None);
}

#[tokio::test]
async fn an_explicit_title_kind_and_note_are_carried_through() {
    let dir = workspace(&[("out.dat", b"plain text really")]);
    let queue = PendingPublishQueue::default();
    let tool = PublishArtifactTool::new(dir.path(), queue.clone());

    run(
        &tool,
        json!({
            "path": "out.dat",
            "title": "Q3 export",
            "kind": "text",
            "note": "rewrote the pricing section"
        }),
    )
    .await;

    let staged = queue.drain();
    assert_eq!(staged[0].title, "Q3 export");
    assert_eq!(
        staged[0].kind,
        ArtifactKind::Text,
        "an explicit kind beats the extension"
    );
    assert_eq!(
        staged[0].note.as_deref(),
        Some("rewrote the pricing section")
    );
}

/// The body is read at publish time, so a later shell step cannot retroactively
/// change what the operator is told was published.
#[tokio::test]
async fn the_body_is_captured_at_publish_time_not_at_drain_time() {
    let dir = workspace(&[("spec.md", b"# The version I published")]);
    let queue = PendingPublishQueue::default();
    let tool = PublishArtifactTool::new(dir.path(), queue.clone());

    run(&tool, json!({ "path": "spec.md" })).await;
    // The agent's next step scribbles over the file.
    std::fs::write(dir.path().join("spec.md"), b"# clobbered afterwards").unwrap();

    let staged = queue.drain();
    assert_eq!(staged[0].body, "# The version I published");
}

#[tokio::test]
async fn a_bad_path_is_a_truthful_tool_error_and_stages_nothing() {
    let dir = workspace(&[("spec.md", b"# Spec")]);
    let queue = PendingPublishQueue::default();
    let tool = PublishArtifactTool::new(dir.path(), queue.clone());

    for path in ["../escape.md", "/etc/hosts", "nope.md", ""] {
        let result = run(&tool, json!({ "path": path })).await;
        assert!(result.is_error, "`{path}` was accepted");
    }
    // A missing `path` argument entirely.
    assert!(run(&tool, json!({})).await.is_error);
    assert_eq!(queue.queued(), 0, "a refused publish must stage nothing");
}

#[tokio::test]
async fn an_unknown_kind_is_refused_by_name() {
    let dir = workspace(&[("spec.md", b"# Spec")]);
    let queue = PendingPublishQueue::default();
    let tool = PublishArtifactTool::new(dir.path(), queue.clone());

    let result = run(&tool, json!({ "path": "spec.md", "kind": "spreadsheet" })).await;
    assert!(result.is_error);
    let message = text_of(&result);
    assert!(message.contains("markdown"), "{message}");
    assert_eq!(queue.queued(), 0);
}

// ── Queue semantics ───────────────────────────────────────────────────────

#[test]
fn the_queue_drains_fifo_and_empties() {
    let queue = PendingPublishQueue::default();
    let publish = |source: &str| PendingPublish {
        source: source.to_string(),
        title: source.to_string(),
        kind: ArtifactKind::Text,
        note: None,
        body: "b".to_string(),
    };
    queue.push(publish("a.md"));
    queue.push(publish("b.md"));
    assert_eq!(queue.sources(), ["a.md", "b.md"]);
    assert_eq!(queue.queued(), 2);

    let drained = queue.drain();
    assert_eq!(
        drained
            .iter()
            .map(|p| p.source.as_str())
            .collect::<Vec<_>>(),
        ["a.md", "b.md"]
    );
    assert_eq!(queue.queued(), 0, "drain empties");
    assert!(queue.drain().is_empty(), "a second drain yields nothing");
}

/// `clear` is what stops an operator chat turn earlier in the same cycle — or
/// an abandoned redirect re-run — from having its staged file attributed to
/// this card.
#[test]
fn clear_drops_what_a_prior_turn_staged() {
    let queue = PendingPublishQueue::default();
    queue.push(PendingPublish {
        source: "leftover.md".to_string(),
        title: "leftover".to_string(),
        kind: ArtifactKind::Text,
        note: None,
        body: "b".to_string(),
    });
    queue.clear();
    assert_eq!(queue.queued(), 0);
    assert!(queue.sources().is_empty());
}

/// The queue handle is shared, not copied — the tool built into the agent and
/// the brain that drains it must see one queue.
#[tokio::test]
async fn a_cloned_handle_sees_the_same_queue() {
    let dir = workspace(&[("spec.md", b"# Spec")]);
    let queue = PendingPublishQueue::default();
    let tool = PublishArtifactTool::new(dir.path(), queue.clone());
    run(&tool, json!({ "path": "spec.md" })).await;
    assert_eq!(queue.queued(), 1, "the brain's handle sees the tool's push");
}

// ── The scan ──────────────────────────────────────────────────────────────

#[test]
fn the_scan_sees_new_and_modified_files_but_not_deletions() {
    let dir = workspace(&[("keep.md", b"one"), ("gone.md", b"two")]);
    let before = WorkspaceSnapshot::take(dir.path());
    assert_eq!(before.len(), 2);

    std::fs::write(dir.path().join("keep.md"), b"one, revised").unwrap();
    std::fs::write(dir.path().join("fresh.md"), b"new").unwrap();
    std::fs::remove_file(dir.path().join("gone.md")).unwrap();

    let changed = before.changed_since(dir.path());
    assert_eq!(
        changed,
        ["fresh.md", "keep.md"],
        "a deleted file is not a deliverable somebody forgot to publish"
    );
}

/// A same-timestamp rewrite still counts, because size is compared too. Coarse
/// filesystem clocks are common enough that mtime alone would miss real edits.
#[test]
fn a_same_instant_rewrite_of_a_different_length_is_still_a_change() {
    let dir = workspace(&[("spec.md", b"short")]);
    let before = WorkspaceSnapshot::take(dir.path());
    let path = dir.path().join("spec.md");
    let stat = std::fs::metadata(&path).unwrap();
    std::fs::write(&path, b"a considerably longer body").unwrap();
    // Force the mtime back so only the size differs.
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(stat.modified().unwrap()).unwrap();
    drop(file);

    assert_eq!(before.changed_since(dir.path()), ["spec.md"]);
}

#[test]
fn the_scan_skips_the_directories_an_exec_sandbox_fills() {
    let dir = workspace(&[
        ("spec.md", b"one"),
        (".git/objects/ab/cdef", b"blob"),
        ("node_modules/left-pad/index.js", b"module"),
        ("target/debug/build.log", b"log"),
    ]);
    let snapshot = WorkspaceSnapshot::take(dir.path());
    assert_eq!(snapshot.len(), 1, "only the agent's own file");
    assert!(!snapshot.truncated());

    // …and they are skipped on the diff side too, so a build never nudges.
    let before = WorkspaceSnapshot::take(dir.path());
    std::fs::write(dir.path().join("target/debug/build.log"), b"rebuilt").unwrap();
    assert!(before.changed_since(dir.path()).is_empty());
}

/// **The false-positive test that matters most.** The agent's `workspace_dir`
/// is also where OpenHuman writes its own session transcripts, audit trail and
/// checkpoints — on *every* run, by the harness rather than the agent. If the
/// scan counted them, the nudge would fire after every single dispatch, asking
/// an agent whether its own transcript is a deliverable.
///
/// Found the hard way: before these exclusions, every existing dispatch test
/// grew a second model turn.
#[test]
fn the_scan_ignores_what_the_runtime_itself_writes() {
    let dir = workspace(&[("spec.md", b"one")]);
    let before = WorkspaceSnapshot::take(dir.path());

    // Exactly what a real run leaves behind beside the agent's own work.
    for path in [
        "sessions/2026_08_05/1785952277_chief.md",
        "session_raw/1785952277_chief.jsonl",
        "artifacts/some-id/content",
        "checkpoints/state.json",
        ".openhuman/subagent_checkpoints/a.json",
        ".runs/run-1.json",
        "audit.log",
        ".env",
    ] {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, b"runtime bookkeeping").unwrap();
    }

    assert!(
        before.changed_since(dir.path()).is_empty(),
        "the runtime's own files must never look like unpublished agent work"
    );

    // The agent's actual file is still seen, so the exclusions did not blind it.
    std::fs::write(dir.path().join("spec.md"), b"one, revised").unwrap();
    assert_eq!(before.changed_since(dir.path()), ["spec.md"]);
}

/// The entry cap. A truncated scan may only under-report — it feeds a warning,
/// never a promotion, so missing something is the acceptable failure.
#[test]
fn the_scan_stops_at_its_entry_cap() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..(MAX_SCAN_ENTRIES + 50) {
        std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
    }
    let snapshot = WorkspaceSnapshot::take(dir.path());
    assert!(snapshot.truncated());
    assert!(snapshot.len() <= MAX_SCAN_ENTRIES);
}

#[test]
fn a_workspace_that_does_not_exist_yet_has_changed_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let never = dir.path().join("no-such-agent/workspace");
    let snapshot = WorkspaceSnapshot::take(&never);
    assert!(snapshot.is_empty());
    assert!(snapshot.changed_since(&never).is_empty());
}

#[test]
fn unpublished_is_changed_minus_staged() {
    let changed = vec!["a.md".to_string(), "b.md".to_string(), "c.md".to_string()];
    assert_eq!(
        unpublished(&changed, &["b.md".to_string()]),
        ["a.md", "c.md"]
    );
    assert!(unpublished(&changed, &changed).is_empty(), "all published");
    assert!(unpublished(&[], &[]).is_empty(), "nothing written");
}

#[test]
fn a_long_file_list_is_bounded_and_says_so() {
    let many: Vec<String> = (0..MAX_NAMED_FILES + 7)
        .map(|i| format!("f{i}.txt"))
        .collect();
    let rendered = name_files(&many);
    assert!(rendered.contains("and 7 more"), "{rendered}");
    assert!(!rendered.contains(&format!("f{}.txt", MAX_NAMED_FILES + 1)));
    // Under the bound, nothing is added.
    assert_eq!(name_files(&["a.md".to_string()]), "a.md");
}

// ── The nudge's words ─────────────────────────────────────────────────────

/// The nudge has to stand alone: turns share no conversation context, so the
/// brief, the reply and the files all have to be inside it.
#[test]
fn the_nudge_carries_its_own_context() {
    let instruction = nudge_instruction(
        "Draft the launch spec.",
        "Done — I've written it up.",
        &["specs/launch.md".to_string(), "scratch.txt".to_string()],
    );
    assert!(
        instruction.contains("Draft the launch spec."),
        "{instruction}"
    );
    assert!(
        instruction.contains("Done — I've written it up."),
        "{instruction}"
    );
    assert!(instruction.contains("specs/launch.md"), "{instruction}");
    assert!(instruction.contains("scratch.txt"), "{instruction}");
    assert!(instruction.contains(PUBLISH_ARTIFACT_TOOL), "{instruction}");
}

/// **The non-coercion test.** A nudge that reads as an instruction produces
/// published build logs. It must offer the decline in the same breath, and must
/// never claim publishing is required.
#[test]
fn the_nudge_offers_the_decline_and_never_demands_a_publish() {
    let instruction = nudge_instruction("Draft it.", "Done.", &["scratch.txt".to_string()]);
    let lower = instruction.to_lowercase();

    assert!(
        lower.contains("declining is a normal answer"),
        "the decline must be affirmed, not merely permitted: {instruction}"
    );
    assert!(
        lower.contains("say briefly why not"),
        "there must be a stated way to decline: {instruction}"
    );
    assert!(
        lower.contains("scratch files"),
        "the legitimate reasons to decline must be named: {instruction}"
    );
    for coercion in [
        "you must",
        "you should",
        "required",
        "make sure you publish",
    ] {
        assert!(
            !lower.contains(coercion),
            "the nudge reads as a demand (`{coercion}`): {instruction}"
        );
    }
    // And it must be clear the already-sent answer is not at stake.
    assert!(
        lower.contains("already been sent"),
        "the agent must know its reply is safe: {instruction}"
    );
}

#[test]
fn a_decline_is_recorded_with_both_the_files_and_the_reason() {
    let note = declined_note(
        &["scratch.txt".to_string()],
        "  Those were intermediate notes, not the deliverable.  ",
    );
    assert_eq!(
        note,
        "unpublished: scratch.txt — agent: Those were intermediate notes, not the deliverable."
    );
}
