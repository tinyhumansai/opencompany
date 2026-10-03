use super::*;
use crate::store::conformance;

pub(super) fn tmp_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("opencompany-test-")
        .tempdir()
        .expect("tempdir")
}

/* ---- issue #1890 G: the tail is read from the end ---- */

/// Collects a file's lines newest-first, for the cases below.
async fn backwards(path: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    read_lines_backwards(path, |line| {
        out.push(line.to_string());
        Ok(true)
    })
    .await
    .expect("read");
    out
}

async fn write_file(root: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = root.path().join(name);
    tokio::fs::write(&path, body).await.expect("write");
    path
}

#[tokio::test]
async fn a_tail_walk_yields_lines_newest_first() {
    let root = tmp_root();
    let path = write_file(&root, "a.jsonl", "one\ntwo\nthree\n").await;
    assert_eq!(backwards(&path).await, vec!["three", "two", "one"]);
}

/// A file that does not end in a newline reads the same as one that does —
/// the last line is a whole line, not a fragment to be dropped.
#[tokio::test]
async fn a_missing_trailing_newline_does_not_lose_the_last_line() {
    let root = tmp_root();
    let path = write_file(&root, "b.jsonl", "one\ntwo\nthree").await;
    assert_eq!(backwards(&path).await, vec!["three", "two", "one"]);
}

/// Blank lines are skipped, exactly as the forward walk skipped them.
#[tokio::test]
async fn blank_lines_are_skipped() {
    let root = tmp_root();
    let path = write_file(&root, "c.jsonl", "one\n\n\ntwo\n\n").await;
    assert_eq!(backwards(&path).await, vec!["two", "one"]);
}

#[tokio::test]
async fn an_empty_or_absent_file_yields_nothing() {
    let root = tmp_root();
    let path = write_file(&root, "d.jsonl", "").await;
    assert!(backwards(&path).await.is_empty());
    assert!(backwards(&root.path().join("nope.jsonl")).await.is_empty());
}

/// **The chunk-boundary case, and the reason this helper is tested
/// directly.** A line longer than one read, and lines straddling a read
/// boundary, are where a backwards walk silently truncates or duplicates —
/// and a transcript that lost one line in the middle would still render.
#[tokio::test]
async fn lines_spanning_and_straddling_chunk_boundaries_survive_whole() {
    let root = tmp_root();
    // Far larger than TAIL_CHUNK_BYTES, so the walk crosses many reads and
    // one single line is itself longer than a whole chunk.
    let long = "x".repeat(TAIL_CHUNK_BYTES as usize * 2 + 7);
    let mut body = String::new();
    for n in 0..400 {
        body.push_str(&format!("line-{n}\n"));
    }
    body.push_str(&long);
    body.push('\n');
    for n in 400..800 {
        body.push_str(&format!("line-{n}\n"));
    }
    let path = write_file(&root, "e.jsonl", &body).await;

    let read = backwards(&path).await;
    assert_eq!(read.len(), 801, "every line, once");
    assert_eq!(read[0], "line-799");
    assert_eq!(read[400], long, "the over-long line is whole");
    assert_eq!(read[800], "line-0");
}

/// A record spanning **many** reads, not just two.
///
/// The carry is a list of fragments joined once at the read that completes
/// the line, so the order they are collected in is what a wrong
/// front-insert would scramble — and a scrambled join is still a valid
/// string of the right length, which the two-chunk case above is too short
/// to expose. Forty chunks also puts a floor under the copying: the
/// splice-per-read version this replaced moved ~40x more bytes for this one
/// record than the file contains (coderabbit on #1972).
#[tokio::test]
async fn a_record_spanning_many_reads_is_joined_in_order() {
    let root = tmp_root();
    // Distinguishable content, so a mis-ordered join fails rather than
    // matching a repeated byte by luck.
    let mut long = String::new();
    let mut n = 0u64;
    while (long.len() as u64) < TAIL_CHUNK_BYTES * 40 {
        long.push_str(&format!("{n:012} "));
        n += 1;
    }
    let body = format!("before\n{long}\nafter\n");
    let path = write_file(&root, "e.jsonl", &body).await;

    let read = backwards(&path).await;
    assert_eq!(read.len(), 3, "three lines, however many reads they took");
    assert_eq!(read[0], "after");
    assert_eq!(read[1], long.trim(), "the record is whole and in order");
    assert_eq!(read[2], "before");
}

/// The walk stops the moment the caller has what it wants — the whole
/// point of reading from the end. Without this the first page still costs
/// the file.
#[tokio::test]
async fn the_walk_stops_as_soon_as_the_caller_is_done() {
    let root = tmp_root();
    let mut body = String::new();
    for n in 0..5_000 {
        body.push_str(&format!("line-{n}\n"));
    }
    let path = write_file(&root, "f.jsonl", &body).await;

    let mut seen = 0usize;
    read_lines_backwards(&path, |_| {
        seen += 1;
        Ok(seen < 3)
    })
    .await
    .expect("read");
    assert_eq!(seen, 3, "three lines read out of five thousand");
}

#[tokio::test]
async fn conformance_journal_store() {
    let root = tmp_root();
    conformance::assert_journal_store(Arc::new(FsJournalStore::new(root.path()))).await;
}

/// The fs backend reports itself permanently imported, so
/// `assert_journal_import` does not apply to it: its store IS the file an
/// import would copy from, and the builder therefore never imports on this
/// backend. Asserted here rather than left implicit — a backend that
/// answered `false` would have the builder wipe and re-copy a company's
/// journal on every single boot.
#[tokio::test]
async fn the_filesystem_backend_never_needs_an_import() {
    use crate::ports::journal::JournalStore;
    let root = tmp_root();
    let store = FsJournalStore::new(root.path());
    let id = CompanyId::new("alpha");
    assert!(store.journal_imported(&id).await.unwrap());
    store
        .append_journal(&id, "kept", crate::ports::journal::Durability::Host)
        .await
        .unwrap();
    // And the unreachable import is the identity, not a wipe.
    store.complete_import(&id, Vec::new()).await.unwrap();
    assert_eq!(store.read_journal(&id).await.unwrap(), vec!["kept"]);
}

#[tokio::test]
async fn concurrent_appends_stay_one_record_per_line() {
    // Many tasks appending to the same JSONL file must never interleave a
    // record with another's newline (the `{a}{b}\n\n` corruption that
    // `read_jsonl` reports as a "trailing characters" parse error). The
    // single-write `append_line` makes each record one atomic O_APPEND
    // write, so this holds deterministically.
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    tokio::fs::create_dir_all(&root).await.unwrap();
    let path = root.join("log.jsonl");

    const N: u64 = 64;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..N {
        let path = path.clone();
        set.spawn(async move {
            let line = serde_json::to_string(&serde_json::json!({ "i": i })).unwrap();
            append_line(&path, &line).await.unwrap();
        });
    }
    while let Some(res) = set.join_next().await {
        res.unwrap();
    }

    // Every record parses (no merged lines) and all N are present once.
    let rows: Vec<serde_json::Value> = read_jsonl(&path).await.expect("no corrupt lines");
    assert_eq!(rows.len() as u64, N, "every append is its own line");
    let mut seen: Vec<u64> = rows.iter().map(|r| r["i"].as_u64().unwrap()).collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..N).collect::<Vec<_>>(), "all records intact");
}

/// **Issue #392**: the durable append must behave exactly like the plain one
/// from the file's point of view — same bytes, same one-record-per-line
/// framing, appending rather than truncating — and must report the flush it
/// performed.
///
/// It cannot assert the bytes are on the platter; nothing in a unit test
/// can, because a synced and an unsynced line are identical on disk. It
/// asserts the two things that *are* observable: the flush was requested and
/// its syscall returned `Ok` (the append would have failed otherwise), and
/// the content is intact.
#[tokio::test]
async fn durable_append_writes_the_same_bytes_and_reports_the_flush() {
    let root_dir = tmp_root();
    // A nested directory so the create path — and with it the parent
    // directory flush — is exercised on a directory this test owns.
    let root = root_dir.path().join("nested");
    tokio::fs::create_dir_all(&root).await.unwrap();
    let path = root.join("durable.jsonl");

    // The first append creates the file; the second finds it already there.
    for i in 0..2u64 {
        let line = serde_json::to_string(&serde_json::json!({ "i": i })).unwrap();
        append_line_durable(&path, &line).await.unwrap();
    }
    // A plain append to the same file must still take the unsynced path.
    append_line(
        &path,
        &serde_json::to_string(&serde_json::json!({ "i": 2 })).unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(
        append_probe::counts(&path),
        (1, 2),
        "two durable appends and one plain one, each on its own path"
    );

    let rows: Vec<serde_json::Value> = read_jsonl(&path).await.expect("no corrupt lines");
    let seen: Vec<u64> = rows.iter().map(|r| r["i"].as_u64().unwrap()).collect();
    assert_eq!(
        seen,
        vec![0, 1, 2],
        "durable appends append, never truncate"
    );
}

/// **Issue #392**: creating the journal's parent chain durably flushes
/// **every** directory it creates, not only the innermost one.
///
/// A create records the new name in its parent's block, so a chain of fresh
/// directories is a chain of independent writes and a host crash can lose
/// any of them on its own. Flushing only the file's own parent would leave a
/// synced record under ancestors that were never written down — unreachable
/// after exactly the crash the flush is bought for, with the flush's cost
/// already paid.
///
/// Starts with the complete nested parent path absent, and asserts the flush
/// was requested for each link. As everywhere else in this module, what a
/// unit test can prove is that the request was made and its syscall returned
/// `Ok`; the platter is the OS contract's half.
#[cfg(unix)]
#[tokio::test]
async fn the_durable_path_flushes_every_directory_it_creates() {
    let root_dir = tmp_root();
    let root = root_dir.path().to_path_buf();
    let companies = root.join("companies");
    let acme = companies.join("acme");
    let journal_dir = acme.join("journal");
    let path = journal_dir.join("journal.jsonl");

    assert!(
        !companies.try_exists().unwrap(),
        "the whole chain below the temp root must be absent to start"
    );

    create_dir_all_durable(&journal_dir).await.unwrap();
    append_line_durable(&path, "{\"i\":0}").await.unwrap();

    // Each created directory's entry lives in its parent, so the parent is
    // what has to be flushed. `journal_dir` is flushed by the file create.
    for (dir, why) in [
        (&root, "holds the entry naming `companies`"),
        (&companies, "holds the entry naming `acme`"),
        (&acme, "holds the entry naming `journal`"),
        (&journal_dir, "holds the entry naming the journal file"),
    ] {
        assert!(
            append_probe::dir_syncs(dir) > 0,
            "{} — unflushed, a host crash can lose the subtree under it",
            why
        );
    }

    let rows: Vec<serde_json::Value> = read_jsonl(&path).await.expect("no corrupt lines");
    assert_eq!(rows.len(), 1, "the record itself still landed");
}

/// **Issue #392**: the directory flush is paid by the append that *creates*
/// the file, and by no other — decided by the open rather than by a stat
/// taken before it.
///
/// The race the decision-by-open closes (a deleter landing between a
/// `try_exists` and the open, so a re-created file skips the flush that
/// makes it findable) cannot be reproduced deterministically in a unit test;
/// it needs a second process interleaved between two syscalls. What is
/// pinned here is the contract that race would break, across all three
/// answers: create flushes, append-to-existing does not, and a re-create
/// after a delete flushes again.
#[cfg(unix)]
#[tokio::test]
async fn the_directory_flush_is_paid_only_by_the_append_that_creates() {
    let root_dir = tmp_root();
    let dir = root_dir.path().join("journal");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("journal.jsonl");
    // The `create_dir_all` above is not durable, so the tally starts here.
    let before = append_probe::dir_syncs(&dir);

    append_line_durable(&path, "{\"i\":0}").await.unwrap();
    assert_eq!(
        append_probe::dir_syncs(&dir) - before,
        1,
        "the creating append flushes the entry naming the new file"
    );

    append_line_durable(&path, "{\"i\":1}").await.unwrap();
    assert_eq!(
        append_probe::dir_syncs(&dir) - before,
        1,
        "an append to a file that is already there writes no new entry, \
             so it must not pay for a directory flush"
    );

    tokio::fs::remove_file(&path).await.unwrap();
    append_line_durable(&path, "{\"i\":2}").await.unwrap();
    assert_eq!(
        append_probe::dir_syncs(&dir) - before,
        2,
        "re-creating the file writes a new entry, which must be flushed"
    );
}

// ── write_atomic durability (issue #1049) ──────────────────────────────

/// An atomic write flushes the bytes **and** the directory entry that
/// publishes them.
///
/// ## What this proves, and what it does not
///
/// It pins the **calls**, not the physics. A flushed file and an unflushed
/// one are byte-identical on disk, and CI cannot pull the power, so there is
/// no test that observes a lost update. What is asserted is that
/// `sync_data` and the parent-directory `sync_all` are requested at the
/// points the recipe requires — which is the part a refactor can silently
/// drop. The claim that those calls make a save survive a power cut rests on
/// the filesystem contract, not on this test. Same stance, and the same
/// reason, as `the_directory_flush_is_paid_only_by_the_append_that_creates`
/// above.
#[cfg(unix)]
#[tokio::test]
async fn an_atomic_write_flushes_the_file_and_the_directory_entry() {
    let root_dir = tmp_root();
    let dir = root_dir.path().join("state");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("tasks.json");
    // `create_dir_all` above is not durable, so both tallies start here.
    let dirs_before = append_probe::dir_syncs(&dir);

    write_atomic(&path, "{\"v\":1}").await.unwrap();

    assert_eq!(
        append_probe::atomic_syncs(&path),
        1,
        "the temp file's bytes must be flushed before the rename publishes them"
    );
    assert_eq!(
        append_probe::dir_syncs(&dir) - dirs_before,
        1,
        "the rename changed the directory, so the directory must be flushed too"
    );
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "{\"v\":1}");
}

/// **The half people forget.** A rename repoints an existing name at a new
/// inode, so the parent directory changes on *every* call — not only on the
/// one that creates the file.
///
/// This is the assertion that separates a real fix from a naive copy of the
/// append path, whose `if creating` guard is correct there and wrong here.
/// An overwrite is the common case for every caller of `write_atomic`: the
/// task list is rewritten whole on each change, and its file exists after
/// the first save.
#[cfg(unix)]
#[tokio::test]
async fn overwriting_an_existing_file_still_flushes_the_directory() {
    let root_dir = tmp_root();
    let dir = root_dir.path().join("state");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("tasks.json");
    let dirs_before = append_probe::dir_syncs(&dir);

    write_atomic(&path, "first").await.unwrap();
    write_atomic(&path, "second").await.unwrap();
    write_atomic(&path, "third").await.unwrap();

    assert_eq!(
        append_probe::atomic_syncs(&path),
        3,
        "every save flushes its own bytes"
    );
    assert_eq!(
        append_probe::dir_syncs(&dir) - dirs_before,
        3,
        "every rename rewrites the directory entry, so no save may skip the \
             directory flush — an `if creating` guard here would lose the last \
             two updates to a power cut"
    );
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "third");
}

/// The byte-taking half goes through the same sequence — it is the one
/// implementation, and a second path that skipped a flush is exactly what
/// the shared body exists to prevent.
#[cfg(unix)]
#[tokio::test]
async fn the_bytes_entry_point_is_durable_too() {
    let root_dir = tmp_root();
    let dir = root_dir.path().join("blobs");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("note.bin");
    let dirs_before = append_probe::dir_syncs(&dir);

    write_atomic_bytes(&path, &[0xDE, 0xAD, 0xBE, 0xEF])
        .await
        .unwrap();

    assert_eq!(append_probe::atomic_syncs(&path), 1);
    assert_eq!(append_probe::dir_syncs(&dir) - dirs_before, 1);
    assert_eq!(
        tokio::fs::read(&path).await.unwrap(),
        vec![0xDE, 0xAD, 0xBE, 0xEF]
    );
}

/// The behaviour the durability work must not have cost: a reader still sees
/// the old file or the new one, never a prefix. Pinned alongside the flushes
/// because the rewrite moved the write off `tokio::fs::write` and onto an
/// explicit create/write/rename, and the temp-then-rename shape is the whole
/// reason issue #887 was closed.
#[tokio::test]
async fn an_atomic_write_leaves_no_temp_file_behind() {
    let root_dir = tmp_root();
    let dir = root_dir.path().join("state");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("tasks.json");

    write_atomic(&path, "{}").await.unwrap();

    let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    assert_eq!(
        names,
        vec!["tasks.json".to_string()],
        "the temp file must be renamed away, not left as litter: {names:?}"
    );
}

/// **Cross-process safety, simulated in-process.** [`path_lock`] is
/// keyed on a process-wide `static`, so it serializes every writer inside
/// *this* process regardless of what they call through. To prove the claim
/// that matters for a second `opencompany` process over the same bundle —
/// that losing that in-process lock bounds the damage to a lost update and
/// never a torn file — this drives many concurrent [`write_atomic_bytes`]
/// calls directly, bypassing [`path_lock`] entirely, exactly as two
/// unsynchronised processes would.
///
/// Each writer's payload is large and distinct, so a write that is not
/// truly atomic (a naive truncate-then-stream, or two renames' bytes
/// interleaving) would leave the file holding neither candidate in full —
/// short, mixed, or holding a length that names no writer. The assertion
/// is deliberately narrow: not "the last writer wins" (unordered
/// concurrent tasks have no defined last), only that whichever bytes land
/// are exactly one full, uncorrupted candidate.
#[tokio::test]
async fn concurrent_writers_without_the_lock_never_leave_a_torn_file() {
    let root_dir = tmp_root();
    let dir = root_dir.path().join("state");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("tasks.json");

    const WRITERS: u8 = 12;
    // Each candidate is a distinct byte repeated many times, so a torn or
    // interleaved result is detectable from content alone: any byte in
    // the final file that is not the *one* value every position holds
    // proves a mix, and any length that is not exactly `BYTES` proves a
    // truncation.
    const BYTES: usize = 200 * 1024;
    let candidates: Vec<Vec<u8>> = (0..WRITERS)
        .map(|writer| vec![b'A' + writer; BYTES])
        .collect();

    // `spawn` alone permits the runtime to finish one writer before the
    // next begins, and a serial schedule passes this test without ever
    // reaching the contended path. The barrier holds every task at the
    // instant before the write so they are released together.
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(WRITERS as usize));
    let mut set = tokio::task::JoinSet::new();
    for candidate in candidates.clone() {
        let path = path.clone();
        let gate = std::sync::Arc::clone(&gate);
        set.spawn(async move {
            gate.wait().await;
            write_atomic_bytes(&path, &candidate).await
        });
    }
    while let Some(res) = set.join_next().await {
        res.unwrap().expect("no writer observes an I/O error");
    }

    let landed = tokio::fs::read(&path).await.unwrap();
    assert_eq!(
        landed.len(),
        BYTES,
        "a torn write left a length matching no candidate: {} bytes",
        landed.len()
    );
    assert!(
        candidates.iter().any(|c| c == &landed),
        "the file's bytes were not a single writer's payload in full — a torn \
             or interleaved write slipped through the lock-free path"
    );
}

/// A failed write still surfaces as an error rather than half-succeeding —
/// the same direction `durable_append_reports_an_unwritable_path` pins for
/// the append path. Here the temp create fails because the parent is a
/// *file*, so `create_dir_all` cannot make it a directory.
#[tokio::test]
async fn an_atomic_write_onto_an_unusable_parent_reports_the_error() {
    let root_dir = tmp_root();
    let blocker = root_dir.path().join("not-a-dir");
    tokio::fs::write(&blocker, "occupied").await.unwrap();

    let err = write_atomic(&blocker.join("tasks.json"), "{}")
        .await
        .expect_err("a write under a non-directory cannot succeed");
    assert!(
        matches!(err, OpenCompanyError::StoreIo { .. }),
        "the caller must learn the save did not happen: {err:?}"
    );
}

/// A durable append into a directory that does not exist must surface the
/// error rather than half-succeed. This is the direction the journal's
/// at-most-once guarantee depends on: a failed commit stops the effect.
#[tokio::test]
async fn durable_append_reports_an_unwritable_path() {
    let root_dir = tmp_root();
    let path = root_dir.path().join("absent-dir").join("durable.jsonl");
    let err = append_line_durable(&path, "{}").await.unwrap_err();
    assert!(
        matches!(err, OpenCompanyError::StoreIo { .. }),
        "an unwritable durable append must report a store IO error, got {err:?}"
    );
}

/* ---- retired event kinds (tiny.place removal) ---- */

/// An `events.jsonl` written before tiny.place was removed holds
/// `A2aTaskReceived` rows. The log must still read — the row surfaces as
/// [`CompanyEvent::Unknown`] — and the next append must not reuse its
/// sequence number, which it would if the row were skipped as unparseable.
#[tokio::test]
async fn an_events_log_with_a_retired_a2a_row_still_reads() {
    let root = tmp_root();
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.path().to_path_buf(), &id);
    bundle.ensure_dirs().await.expect("dirs");
    let legacy = concat!(
        r#"{"seq":0,"company":"acme","event":{"kind":"ScheduleFired","cron":"0 9 * * *","prompt":"standup"},"at_millis":1}"#,
        "\n",
        r#"{"seq":1,"company":"acme","event":{"kind":"A2aTaskReceived","from":"@peer","task":{"skill":"seo.audit"}},"at_millis":2}"#,
        "\n",
    );
    tokio::fs::write(bundle.events_jsonl(), legacy)
        .await
        .expect("write");

    let log = FsEventLog::new(root.path());
    let events = log
        .read_from(&id, EventSeq::new(0), usize::MAX)
        .await
        .expect("a log holding a retired kind still reads");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event, CompanyEvent::Unknown);

    let tail = log
        .read_before(&id, None, 10)
        .await
        .expect("the tail walk reads it too");
    assert_eq!(tail.len(), 2);

    let next = log
        .append(
            &id,
            CompanyEvent::FeedbackFiled {
                note: "after".into(),
            },
        )
        .await
        .expect("append");
    assert_eq!(
        next,
        EventSeq::new(2),
        "the retired row's seq is not reused"
    );
}

/// A retention pass rewrites `events.jsonl`, and a retired row reads back as a
/// bare [`CompanyEvent::Unknown`]. Re-serializing that would replace the row's
/// body with `{"kind":"Unknown"}` — silently destroying a record this build
/// merely cannot read. Kept rows are written back as the bytes they were.
#[tokio::test]
async fn pruning_keeps_a_retired_row_byte_for_byte() {
    let root = tmp_root();
    let id = CompanyId::new("acme");
    let bundle = Bundle::new(root.path().to_path_buf(), &id);
    bundle.ensure_dirs().await.expect("dirs");
    let legacy = r#"{"seq":0,"company":"acme","event":{"kind":"A2aTaskReceived","from":"@peer","task":{"skill":"seo.audit"}},"at_millis":1}"#;
    tokio::fs::write(bundle.events_jsonl(), format!("{legacy}\n"))
        .await
        .expect("write");

    let log = FsEventLog::new(root.path());
    for n in 0..3 {
        log.append(
            &id,
            CompanyEvent::WorkflowRunStarted {
                workflow_id: "wf".into(),
                run_id: format!("run-{n}"),
                scheduled: false,
                started_by: None,
                resume_semantic: None,
            },
        )
        .await
        .expect("append");
    }

    let report = log
        .prune(
            &id,
            &RetentionPolicy {
                max_age_millis: None,
                max_entries_per_kind: Some(1),
            },
        )
        .await
        .expect("prune");
    assert!(
        report.removed > 0,
        "the pass must actually rewrite the file"
    );

    let body = tokio::fs::read_to_string(bundle.events_jsonl())
        .await
        .expect("read");
    assert_eq!(
        body.lines().next(),
        Some(legacy),
        "the retired row survives the rewrite verbatim"
    );
}
