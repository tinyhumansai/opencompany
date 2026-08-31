//! Filesystem-backed implementations of the persistence ports.
//!
//! All state lives in per-company [`Bundle`] directories (TOML for the
//! manifest, JSONL for append-only logs, content-addressed blobs for context).
//! Appends are the hot path and never rewrite the whole file; per-path
//! `tokio::sync::Mutex` locks serialize concurrent writers within a process.
//! Those locks live in one process-wide registry (`path_lock`) rather than on
//! each store, so two instances over one bundle actually meet (issue #388).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex as TokioMutex, broadcast};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::events::{EventLog, EventStreamItem, PruneReport, RetentionPolicy, plan_prune};
use crate::ports::inbox::{EmailRecord, InboxMeta, InboxStore};
use crate::ports::memory::MemoryStore;
use crate::ports::secrets::SecretStore;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanyEvent, CompanyId, CompanyRecord, CompanySummary,
    CompressedTrace, ContextChunk, EventSeq, EvictionPolicy, LedgerEntry, SecretValue, StoredEvent,
    TaskResult,
};
use crate::ports::{generate_id, now_millis};
use crate::store::content_address;
use crate::store::paths::Bundle;
use crate::store::text::slice_on_char_boundaries;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn io_err(path: &Path, source: std::io::Error) -> OpenCompanyError {
    OpenCompanyError::StoreIo {
        path: path.to_path_buf(),
        source,
    }
}

/// A registry of per-path async locks, so appends to the same file serialize
/// while distinct files stay concurrent.
#[derive(Clone, Default)]
pub(crate) struct PathLocks {
    inner: Arc<StdMutex<HashMap<PathBuf, Arc<TokioMutex<()>>>>>,
}

impl PathLocks {
    pub(crate) fn get(&self, path: &Path) -> Arc<TokioMutex<()>> {
        let mut map = self.inner.lock().expect("path-lock map poisoned");
        map.entry(path.to_path_buf()).or_default().clone()
    }
}

/// Every filesystem store's write locks, shared by every instance in the
/// process (issue #388).
///
/// The locks these replaced were **fields** on `FsCompanyStore`, `FsEventLog`,
/// `FsMemoryStore`, `FsContextStore`, `FsInboxStore` and `FsOps` — so two stores
/// over one bundle serialised against nothing, which is the state those types
/// have always been in and which nothing stopped a caller reaching: each
/// constructor takes a root and builds a fresh registry. A `static` is the only
/// thing two independently-constructed instances can share.
///
/// The damage that let through was not theoretical. `FsEventLog::append`
/// derives the next sequence from the current line count and then appends, so
/// two unsynchronised instances hand out the **same** `seq` — breaking every
/// consumer that treats it as an identity. And the read-modify-write sites
/// (`FsMemoryStore::evict`, `FsInboxStore::mark_read`, and the whole-file
/// rewrites in [`FsOps`](crate::store::fs_ops::FsOps)) replace the file with a
/// snapshot, so an append that raced one of them was simply erased.
///
/// Mirrors the precedent
/// [`JOURNAL_WRITE_LOCKS`](crate::runtime::journal) set for the runtime journal
/// in issue #386, deliberately including its caveats — see [`path_lock`].
static FS_WRITE_LOCKS: std::sync::LazyLock<PathLocks> =
    std::sync::LazyLock::new(PathLocks::default);

/// The process-wide write lock for `path`.
///
/// Keyed on the **absolutised** path, so a relative and an absolute spelling of
/// one file meet on the same lock instead of racing.
///
/// Two limits, both by construction rather than oversight:
///
/// * **Absolutising is not canonicalising.** `std::path::absolute` is purely
///   lexical: it never touches the filesystem, so it does not resolve symlinks
///   and it does not collapse `..` across one. Two spellings that differ by a
///   symlinked ancestor therefore land on two different locks. Resolving that
///   would mean a `canonicalize` syscall on every append — on the hot path, for
///   a case no caller in this crate produces (every path is built by
///   [`Bundle`] from one configured root). What keeps the missed case from
///   tearing is [`append_line`]'s single `O_APPEND` write, not this lock.
/// * **In-process only.** A second *process* over the same
///   `OPENCOMPANY_DATA_DIR` is outside any in-process lock's reach, and no
///   amount of `static` fixes that. Write atomicity is what keeps that case
///   safe: appends are one `O_APPEND` write and rewrites go through
///   [`write_atomic`]'s temp-file-plus-rename. That bounds the damage to a lost
///   update, never a torn file. Real cross-process exclusion would need file
///   locking, which is a separate change with a separate portability argument.
pub(crate) fn path_lock(path: &Path) -> Arc<TokioMutex<()>> {
    FS_WRITE_LOCKS.get(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Appends one line (a `\n` is added) to `path`, creating the file if absent.
///
/// The line and its terminating newline are written in a **single** blocking
/// `write_all` inside `spawn_blocking` (via `std::fs::OpenOptions` with
/// `O_APPEND`), so the whole record lands as one atomic OS-level write.
/// Tokio's async `File` buffers internally and can return before the kernel
/// write completes, which makes concurrent-appends tests unreliable; this
/// version always waits for the write syscall to finish before returning.
///
/// **Process-crash durable, not host-crash durable**: the bytes are in the
/// kernel's page cache when this returns, so killing the process cannot lose
/// them but losing the machine can. A caller that must have a record on stable
/// storage before it proceeds uses [`append_line_durable`] instead. Which
/// records those are is decided per record kind by the runtime journal (issue
/// #392) — not here, and deliberately not for every append: this function is
/// the hot path for the event log and the ledger, whose own durability is a
/// separate decision that #392 does not make.
pub(crate) async fn append_line(path: &Path, line: &str) -> Result<()> {
    append_line_inner(path, line, false).await
}

/// Appends one line exactly as [`append_line`] does, and does not return until
/// the bytes are on **stable storage** (issue #392).
///
/// The write itself is unchanged — one `write_all` under `O_APPEND`, so the
/// atomicity argument in [`append_line`] carries over untouched — and is
/// followed by `File::sync_data`. When this append is the one that **creates**
/// the file, the parent directory is opened and `sync_all`ed as well: on a
/// create it is the directory entry that names the new file, and that entry is a
/// separate write which a host crash can lose on its own, leaving a flushed file
/// nothing can find. Whether it created the file is decided by the open itself
/// rather than by a prior stat ([`open_for_append`]), so a concurrent deleter
/// cannot make the append skip that flush. Creating the file's *parent chain*
/// durably is [`create_dir_all_durable`], and is the caller's to ask for.
///
/// **A failed flush fails the append.** For the caller this exists for — the
/// journal's `EffectExecuted` commit, written immediately before the side effect
/// runs — that is the safe direction: no record means `execute_effect_once`
/// aborts before `perform_effect`, so nothing external fires and nothing can
/// duplicate. It is also the only correct handling of an `fsync` error on Linux,
/// where a failed flush may already have dropped the dirty pages and a *retry*
/// would cheerfully report success over lost data.
pub(crate) async fn append_line_durable(path: &Path, line: &str) -> Result<()> {
    append_line_inner(path, line, true).await
}

/// The one append implementation, with the flush as a parameter.
///
/// Shared rather than duplicated so the two entry points can never drift on the
/// part that matters to both of them: the single whole-record `write_all` under
/// `O_APPEND`.
async fn append_line_inner(path: &Path, line: &str, sync: bool) -> Result<()> {
    let owned_path = path.to_path_buf();
    let mut record = String::with_capacity(line.len() + 1);
    record.push_str(line);
    record.push('\n');
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        // Whether this append is the one that creates the file, which is the
        // only append whose directory entry needs flushing.
        let (mut file, creating) = open_for_append(&owned_path, sync)?;
        file.write_all(record.as_bytes())
            .map_err(|e| io_err(&owned_path, e))?;
        if sync {
            file.sync_data().map_err(|e| io_err(&owned_path, e))?;
            if creating {
                sync_parent_dir(&owned_path)?;
            }
        }
        #[cfg(test)]
        append_probe::record(&owned_path, sync);
        Ok::<_, OpenCompanyError>(())
    })
    .await
    .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
}

/// Opens `path` for appending, reporting whether **this open** created it.
///
/// The plain path does not need the answer and takes the single-syscall route.
/// The durable path does need it — it decides whether the directory entry naming
/// the file is flushed — and needs it to be *true*, which is why it is not asked
/// with a `try_exists` before the open. That would be a time-of-check window: a
/// concurrent deleter landing between the stat and the open answers "already
/// there" for a file this open then re-creates, and the append skips the
/// directory flush it exists to guarantee, leaving a synced record under a name
/// that was never written down. An in-process [`path_lock`] does not help,
/// because the deleter that matters is another process on the same data
/// directory.
///
/// So the answer comes from the open itself, where it cannot be stale:
/// `create_new` succeeding **is** the creation, and an append-open succeeding is
/// proof the file was already there. Neither is cheaper than the stat it
/// replaces — one open when the file exists, two when it does not, against the
/// stat-plus-open it cost before.
///
/// The two opens can lose to each other repeatedly (created, then deleted, then
/// absent again), so the retry is bounded and gives up in the safe direction:
/// assume created, pay one needless directory flush, never skip a needed one.
fn open_for_append(path: &Path, sync: bool) -> Result<(std::fs::File, bool)> {
    /// Enough to absorb a racing deleter; past this the path is being churned by
    /// something whose behaviour no answer here would survive anyway.
    const ATTEMPTS: usize = 3;

    let create_or_open = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| io_err(path, e))
    };
    if !sync {
        // `creating` is meaningless on the plain path — nothing flushes.
        return Ok((create_or_open()?, false));
    }
    for _ in 0..ATTEMPTS {
        match std::fs::OpenOptions::new().append(true).open(path) {
            Ok(file) => return Ok((file, false)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(path, e)),
        }
        match std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)
        {
            Ok(file) => return Ok((file, true)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(path, e)),
        }
    }
    Ok((create_or_open()?, true))
}

/// Flushes the directory entry naming `path`, so a newly created file is still
/// *findable* after a host crash.
///
/// `sync_data` on the file covers the file's own data and the metadata needed to
/// read it back. It does not cover the parent directory's block, which is where
/// a create records the new name — flush only the file and a crash can leave the
/// data durable under a name that was never written down.
///
/// POSIX-only by construction: Windows has no directory handle to flush (the
/// nearest equivalent flushes a whole volume), and `File::open` on a directory
/// fails there outright, so a non-unix build must not turn that into a failed
/// append. The deployed target is Linux containers and the development target is
/// macOS; both are covered.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| io_err(parent, e))?;
    #[cfg(test)]
    append_probe::record_dir_sync(parent);
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// Creates `dir` and every missing ancestor, and does not return until each
/// directory entry it created is on **stable storage** (issue #392).
///
/// A create is durable only once the block that holds its *name* is, and that
/// block belongs to the parent. [`sync_parent_dir`] covers the entry naming the
/// file, which is the whole story for a journal whose directory already existed.
/// It is not the whole story for the first append into a fresh company data
/// directory: that append creates a *chain*, and flushing only its innermost
/// link leaves the outer ones as unflushed writes a host crash can lose
/// independently. Losing one takes the entire subtree with it — synced record
/// included — which is precisely the loss the flush was bought to prevent, with
/// the flush's cost already paid.
///
/// Each created directory is made durable by syncing **its** parent, walking
/// outermost-first so every `mkdir` lands in a directory that exists. The
/// innermost created directory is deliberately not synced here; it is the
/// parent of the file the caller is about to create, and [`sync_parent_dir`]
/// flushes it as part of that create.
///
/// POSIX-only on the same terms as [`sync_parent_dir`] — a non-unix build
/// creates the chain and skips the flushes.
pub(crate) async fn create_dir_all_durable(dir: &Path) -> Result<()> {
    // Mirrors `std::fs::create_dir_all`, which treats the empty path as a no-op
    // rather than an error.
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    let owned = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // Innermost-first while walking up, so the creation loop below reverses
        // it. A readable ancestor that already exists ends the walk; anything
        // else is treated as missing and left for `create_dir` to report against
        // the path that actually failed.
        let mut missing: Vec<&Path> = Vec::new();
        let mut cursor = Some(owned.as_path());
        while let Some(path) = cursor {
            if matches!(path.try_exists(), Ok(true)) {
                break;
            }
            missing.push(path);
            cursor = path.parent().filter(|p| !p.as_os_str().is_empty());
        }
        for path in missing.iter().rev() {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                // Lost the race to another creator. The entry exists either way,
                // and the winner owns flushing it.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io_err(path, e)),
            }
            sync_parent_dir(path)?;
        }
        Ok::<_, OpenCompanyError>(())
    })
    .await
    .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))?
}

/// A test-only tally of how each append to a path was performed (issue #392).
///
/// The seam a per-kind durability policy needs proved is "this record asked for
/// the flush", and no inspection of the file can answer it: a synced line and an
/// unsynced line are byte-identical on disk. Counting the request where it is
/// made is the honest check. What it proves is the plumbing, not the platter —
/// see the journal's `host_records_route_through_the_durable_append` for the
/// full statement of what a unit test can and cannot establish here.
#[cfg(test)]
pub(crate) mod append_probe {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    /// `(plain, durable)` append counts, keyed like [`super::path_lock`] on the
    /// absolutised path so a test and the code under test always meet.
    static COUNTS: LazyLock<Mutex<HashMap<PathBuf, (usize, usize)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn key(path: &Path) -> PathBuf {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }

    pub(crate) fn record(path: &Path, synced: bool) {
        let mut counts = COUNTS.lock().expect("append-probe poisoned");
        let entry = counts.entry(key(path)).or_insert((0, 0));
        if synced {
            entry.1 += 1;
        } else {
            entry.0 += 1;
        }
    }

    /// The `(plain, durable)` appends observed for `path`. Tests use their own
    /// temp paths, so no two of them share a tally.
    pub(crate) fn counts(path: &Path) -> (usize, usize) {
        COUNTS
            .lock()
            .expect("append-probe poisoned")
            .get(&key(path))
            .copied()
            .unwrap_or((0, 0))
    }

    /// How many times [`super::sync_parent_dir`] has flushed each directory.
    ///
    /// The same argument as the append tally: a flushed directory and an
    /// unflushed one are identical on disk, so the honest check is to count the
    /// request where it is made. A count rather than a set, because *how often*
    /// is the question for the directory flush — it is meant to be paid by the
    /// append that creates a file and by no other. Never cleared: tests own
    /// unique temp paths and ask about their own.
    static DIR_SYNCS: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(crate) fn record_dir_sync(path: &Path) {
        *DIR_SYNCS
            .lock()
            .expect("append-probe poisoned")
            .entry(key(path))
            .or_insert(0) += 1;
    }

    /// How many times `path`'s directory entry block was flushed.
    pub(crate) fn dir_syncs(path: &Path) -> usize {
        DIR_SYNCS
            .lock()
            .expect("append-probe poisoned")
            .get(&key(path))
            .copied()
            .unwrap_or(0)
    }

    /// How many times [`super::write_atomic_bytes`] flushed a temp file's data
    /// before publishing it, keyed on the **final** path rather than the temp
    /// one — the temp name carries a fresh id per call, so a test could never
    /// ask about it.
    ///
    /// Counted here for the reason [`dir_syncs`] is: a flushed file and an
    /// unflushed one are identical on disk, so the honest check is to count the
    /// request at the point it is made. This proves the call happens; it does
    /// not — and cannot — prove what a power cut would leave behind.
    static ATOMIC_SYNCS: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(crate) fn record_atomic_sync(path: &Path) {
        *ATOMIC_SYNCS
            .lock()
            .expect("append-probe poisoned")
            .entry(key(path))
            .or_insert(0) += 1;
    }

    /// How many times `path` was flushed before being published by a rename.
    pub(crate) fn atomic_syncs(path: &Path) -> usize {
        ATOMIC_SYNCS
            .lock()
            .expect("append-probe poisoned")
            .get(&key(path))
            .copied()
            .unwrap_or(0)
    }

    /// The order in which [`super::write_atomic_bytes`] publish renames have
    /// landed, globally, since the process started.
    ///
    /// A multi-file save (`FsCompanyStore::save_gated` writes `company.toml`
    /// then `meta.json`) has a crash-ordering property neither
    /// [`counts`] nor [`atomic_syncs`] can answer: *which file's publish is
    /// observable first* if the process dies between the two. Each is
    /// individually atomic+durable (that is what those two probes prove), but
    /// nothing about a single path's own counters says anything about a
    /// **different** path's write landing before or after it. This log does:
    /// it is one global, append-only sequence of every publish, in the order
    /// `write_atomic_bytes` actually completed them.
    static WRITE_ORDER: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

    pub(crate) fn record_write_order(path: &Path) {
        WRITE_ORDER
            .lock()
            .expect("append-probe poisoned")
            .push(key(path));
    }

    /// The subsequence of the global publish order restricted to `paths`,
    /// in the order they actually landed. Tests use their own unique temp
    /// paths, so restricting to the paths under test is enough to make this
    /// deterministic even though the log itself is never cleared.
    pub(crate) fn write_order_for(paths: &[&Path]) -> Vec<PathBuf> {
        let keys: Vec<PathBuf> = paths.iter().map(|p| key(p)).collect();
        WRITE_ORDER
            .lock()
            .expect("append-probe poisoned")
            .iter()
            .filter(|p| keys.contains(p))
            .cloned()
            .collect()
    }
}

/// Test-only fault injection for [`write_atomic_bytes`].
///
/// Simulates the disk-full / transient-IO failure a real deployment can hit
/// mid-`save` (issue #1828 review on `provision.rs`'s durable collision
/// check): a targeted path's *next* atomic write fails before it touches the
/// filesystem, exactly like a real early I/O error — no temp file, no partial
/// bytes, nothing under the target name. That lets a test prove which of the
/// two files in `FsCompanyStore::save` a real crash leaves behind, without
/// needing actual disk-full conditions.
#[cfg(test)]
pub(crate) mod fault_probe {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    static FAIL_NEXT: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    fn key(path: &Path) -> PathBuf {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Arms a one-shot failure for the next [`write_atomic_bytes`] call
    /// targeting `path`.
    pub(crate) fn fail_next_write(path: &Path) {
        FAIL_NEXT
            .lock()
            .expect("fault-probe poisoned")
            .insert(key(path));
    }

    /// Consumes the armed failure for `path`, if any. One-shot so a retry
    /// after the injected failure exercises the real write.
    pub(crate) fn should_fail(path: &Path) -> bool {
        FAIL_NEXT
            .lock()
            .expect("fault-probe poisoned")
            .remove(&key(path))
    }

    static FAIL_MID_WRITE: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Arms a one-shot failure for the next [`stage_atomic_bytes`]'s
    /// `File::create` to *succeed* and the write that follows it to fail
    /// (issue #1828 review, seventh round). Unlike [`fail_next_write`],
    /// which fails before any filesystem call, this simulates the failure
    /// mode that actually leaves a temp file behind: the create succeeded,
    /// so a `.tmp-*` file already exists, and only the subsequent
    /// `write_all`/`sync_data` fails.
    pub(crate) fn fail_next_mid_write(path: &Path) {
        FAIL_MID_WRITE
            .lock()
            .expect("fault-probe poisoned")
            .insert(key(path));
    }

    /// Consumes the armed mid-write failure for `path`, if any.
    pub(crate) fn should_fail_mid_write(path: &Path) -> bool {
        FAIL_MID_WRITE
            .lock()
            .expect("fault-probe poisoned")
            .remove(&key(path))
    }

    static FAIL_NEXT_DIR_SYNC: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Arms a one-shot failure for the parent-directory fsync that follows a
    /// successful rename in [`commit_staged`] (issue #1828 review, tenth
    /// round). Unlike [`fail_next_commit`], which fails before the rename,
    /// this reaches the state where the destination is *already replaced* on
    /// disk and only the durability step failed.
    pub(crate) fn fail_next_dir_sync(path: &Path) {
        FAIL_NEXT_DIR_SYNC
            .lock()
            .expect("fault-probe poisoned")
            .insert(key(path));
    }

    /// Consumes the armed post-rename dir-sync failure for `path`, if any.
    pub(crate) fn should_fail_dir_sync(path: &Path) -> bool {
        FAIL_NEXT_DIR_SYNC
            .lock()
            .expect("fault-probe poisoned")
            .remove(&key(path))
    }

    static FAIL_NEXT_COMMIT: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Arms a one-shot failure for the next [`commit_staged`] of `path`,
    /// i.e. the rename/`sync_parent_dir` step rather than the staging write
    /// (issue #1828 review, ninth round). Lets a test drive the case where
    /// the *first* file of a two-file save is already published and the
    /// second commit then fails.
    pub(crate) fn fail_next_commit(path: &Path) {
        FAIL_NEXT_COMMIT
            .lock()
            .expect("fault-probe poisoned")
            .insert(key(path));
    }

    /// Consumes the armed commit failure for `path`, if any.
    pub(crate) fn should_fail_commit(path: &Path) -> bool {
        FAIL_NEXT_COMMIT
            .lock()
            .expect("fault-probe poisoned")
            .remove(&key(path))
    }

    static FAIL_NEXT_EXISTS_CHECK: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Arms a one-shot failure for the next `try_exists` probe of `path`
    /// (issue #1828 review, third round: an existence check can fail for
    /// reasons other than "not found" — a transient I/O error or an ACL
    /// denial on the bundle directory — and that failure must not be
    /// silently read as "does not exist").
    pub(crate) fn fail_next_exists_check(path: &Path) {
        FAIL_NEXT_EXISTS_CHECK
            .lock()
            .expect("fault-probe poisoned")
            .insert(key(path));
    }

    /// Consumes the armed existence-check failure for `path`, if any.
    pub(crate) fn should_fail_exists_check(path: &Path) -> bool {
        FAIL_NEXT_EXISTS_CHECK
            .lock()
            .expect("fault-probe poisoned")
            .remove(&key(path))
    }
}

/// Test-only cancellation harness for [`stage_atomic_bytes`].
///
/// Issue #1828 review, sixth round: proving the cancellation hazard (and the
/// fix for it) requires reliably cancelling the caller of `stage_atomic_bytes`
/// *while* its blocking write is still in flight — a plain `sleep`-based race
/// would be flaky. `arm` registers a gate for a target path; the blocking
/// write closure parks on that gate (from a blocking-pool thread, so it never
/// blocks the async runtime) until `maybe_block` is called and a test signals
/// `wait_blocked()`, giving the test a deterministic window to `abort()` the
/// caller before releasing the write to actually run.
#[cfg(test)]
pub(crate) mod stall_probe {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{LazyLock, Mutex};
    use tokio::sync::Notify;

    static GATES: LazyLock<Mutex<HashMap<PathBuf, Receiver<()>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static BLOCKED: LazyLock<Notify> = LazyLock::new(Notify::new);

    fn key(path: &Path) -> PathBuf {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Arms a one-shot stall for the next [`stage_atomic_bytes`] write
    /// targeting `path`. Returns the sender a test uses to release it.
    pub(crate) fn arm(path: &Path) -> Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        GATES
            .lock()
            .expect("stall-probe poisoned")
            .insert(key(path), rx);
        tx
    }

    /// Called from inside the blocking write closure. No-op unless `path`
    /// was armed. Notifies [`wait_blocked`], then parks this blocking-pool
    /// thread until the test's sender releases it.
    pub(crate) fn maybe_block(path: &Path) {
        let gate = GATES
            .lock()
            .expect("stall-probe poisoned")
            .remove(&key(path));
        if let Some(gate) = gate {
            BLOCKED.notify_one();
            let _ = gate.recv();
        }
    }

    /// Waits until an armed write has reached its stall point. `notify_one`
    /// stores its permit if called before this is polled, so there is no
    /// race between arming, spawning the write, and awaiting this.
    pub(crate) async fn wait_blocked() {
        BLOCKED.notified().await;
    }

    static COMMIT_GATES: LazyLock<Mutex<HashMap<PathBuf, Receiver<()>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static COMMIT_BLOCKED: LazyLock<Notify> = LazyLock::new(Notify::new);

    /// Same idea as [`arm`]/[`maybe_block`]/[`wait_blocked`] above, but for
    /// [`commit_staged`]'s blocking closure instead of [`stage_atomic_bytes`]'s
    /// (issue #1828 review, twelfth round follow-up). A separate gate set
    /// because the two stall on the *same* destination path at different
    /// points in the same `save` call — arming one must not be consumed by
    /// the other.
    pub(crate) fn arm_commit(path: &Path) -> Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        COMMIT_GATES
            .lock()
            .expect("stall-probe poisoned")
            .insert(key(path), rx);
        tx
    }

    /// Called from inside `commit_staged`'s blocking closure, before the
    /// rename. No-op unless `path` was armed.
    pub(crate) fn maybe_block_commit(path: &Path) {
        let gate = COMMIT_GATES
            .lock()
            .expect("stall-probe poisoned")
            .remove(&key(path));
        if let Some(gate) = gate {
            COMMIT_BLOCKED.notify_one();
            let _ = gate.recv();
        }
    }

    /// Waits until an armed commit has reached its stall point, i.e. the
    /// rename is genuinely about to run, not merely staged.
    pub(crate) async fn wait_blocked_commit() {
        COMMIT_BLOCKED.notified().await;
    }
}

/// Test-only rendezvous for the seventeenth-round regression test below
/// (finding on comment 3878696002). `stall_probe` above proves the window
/// *before* `tx.send` — rx already dropped, send fails, the detached task's
/// own cleanup runs. This proves the other window: `tx.send` *succeeds*
/// (the `Receiver` was still alive at that instant) but the future awaiting
/// it is torn down before it is ever polled again to actually consume the
/// value. That window can't be reached by aborting a spawned task and
/// hoping the timing lines up — `send` completing and the awaiting task
/// resuming are two independently-scheduled events. Instead this notifies a
/// test the instant `send` returns `Ok`, so the test can race a `select!`
/// biased toward this notification against the awaited future itself and
/// drop the future without ever polling it again once notified — a
/// deterministic reproduction of "sent successfully, never consumed."
#[cfg(test)]
pub(crate) mod send_probe {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};
    use tokio::sync::Notify;

    static GATES: LazyLock<Mutex<HashMap<PathBuf, Arc<Notify>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn key(path: &Path) -> PathBuf {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// Arms a one-shot "the send just succeeded" signal for `path`. Returns
    /// the `Notify` a test awaits.
    pub(crate) fn arm(path: &Path) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        GATES
            .lock()
            .expect("send-probe poisoned")
            .insert(key(path), notify.clone());
        notify
    }

    /// Called right after a successful `tx.send` in `stage_atomic_bytes`'s
    /// detached task. No-op unless `path` was armed.
    pub(crate) fn notify_sent(path: &Path) {
        if let Some(notify) = GATES
            .lock()
            .expect("send-probe poisoned")
            .remove(&key(path))
        {
            notify.notify_one();
        }
    }
}

/// Reads a file to a string, returning an empty string if it does not exist.
pub(crate) async fn read_optional(path: &Path) -> Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io_err(path, e)),
    }
}

/// Parses every non-empty JSONL line of `path` into `T`, skipping absent files.
///
/// **Strict**: one unparseable line fails the whole read. That is the right
/// default for everything that still uses it, and it is a decision rather than
/// an oversight (issue #387):
///
/// * **Read-modify-write callers must stay strict.** `evict` and `mark_read`
///   read a file here and then [`write_atomic`] it back. A tolerant read would
///   drop the damaged line from the in-memory vector, and the rewrite would then
///   erase it from disk — turning recoverable damage into permanent deletion,
///   silently. See [`read_jsonl_lenient`] for the same boundary stated from the
///   other side.
/// * **Request-time read-only callers stay strict too.** A failed inbox or
///   context read surfaces as one failed request the operator can retry and
///   report; it does not cost a company its boot. Making those tolerant is a
///   separate judgement about each surface's error budget, and it is not made
///   here.
///
/// Only the *boot* path — the ledger read in [`FsCompanyStore::load`] — is
/// tolerant, because there the failure is not one request but the whole
/// company, and the console that would repair the file is behind the boot.
pub(crate) async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let contents = read_optional(path).await?;
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

/// A JSONL line [`read_jsonl_lenient`] could not parse: quarantined in place,
/// never deleted.
///
/// Carries where the line is, how big it is, and what the parser rejected —
/// and **never the line's contents**. A ledger memo is free text a person or an
/// agent wrote; echoing it into a log to explain a parse failure would put
/// arbitrary tenant prose into the container log. The 1-based line number and
/// the byte length are enough for an operator to find the line by hand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkippedLine {
    /// The line's 1-based number in the file.
    pub(crate) line: usize,
    /// The line's length in bytes, as it sits on disk.
    pub(crate) bytes: usize,
    /// What the parse rejected. `serde_json`'s message is positional
    /// ("EOF while parsing a string at line 1 column 74") and quotes no input.
    pub(crate) message: String,
}

/// Parses every non-empty JSONL line of `path` into `T`, **skipping** the lines
/// that will not parse and reporting them rather than failing the read.
///
/// Returns the values that did parse, oldest first, plus one [`SkippedLine`]
/// per line that did not. An absent file is an empty read, as in [`read_jsonl`].
///
/// Bytes are read and decoded per line, not through `read_to_string`. A torn
/// write can split a multi-byte codepoint, and a whole-file UTF-8 decode fails
/// on that one bad byte — losing the entire file to damage confined to a single
/// line. Decoding lossily per line keeps the damage where it is. This mirrors
/// [`RuntimeJournal::load`](crate::runtime::journal::RuntimeJournal::load),
/// which reached the same conclusion for the journal (issue #386).
///
/// # Tolerance boundary — do not widen it
///
/// **A read-modify-write consumer must never call this.** Skipping is only safe
/// because the bytes stay on disk: the caller drops the line from its in-memory
/// view and a person can still repair the file. A caller that reads here and
/// then rewrites the file atomically would write back exactly the lines that
/// parsed, deleting the damaged one for good — converting a recoverable fault
/// into silent, permanent data loss, which is strictly worse than the failed
/// boot this function exists to prevent. Rewriters ([`FsMemoryStore::evict`],
/// [`FsInboxStore::mark_read`]) stay on strict [`read_jsonl`], where a damaged
/// line aborts the rewrite instead of laundering it.
///
/// The one caller today is the ledger read in [`FsCompanyStore::load`]. A ledger
/// entry is descriptive accounting: dropping one from a boot-time view
/// misreports spend until the file is repaired, which is recoverable and
/// visible. It is not an at-most-once safety key — the journal's
/// `EffectExecuted` records are, and they are handled separately and more
/// carefully for exactly that reason.
pub(crate) async fn read_jsonl_lenient<T>(path: &Path) -> Result<(Vec<T>, Vec<SkippedLine>)>
where
    T: serde::de::DeserializeOwned,
{
    let contents = match tokio::fs::read(path).await {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(io_err(path, e)),
    };

    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for (index, raw) in contents.split(|b| *b == b'\n').enumerate() {
        // Lossy on purpose: an invalid byte becomes U+FFFD, the line then fails
        // to parse as JSON, and it lands on the same skip-and-report path as any
        // other unreadable line instead of aborting the whole read.
        let line = String::from_utf8_lossy(raw);
        let line = line.as_ref();
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(value) => out.push(value),
            Err(error) => skipped.push(SkippedLine {
                line: index + 1,
                // The on-disk length, not the lossily-decoded one: U+FFFD is
                // three bytes and would inflate the count for exactly the lines
                // an operator is trying to locate.
                bytes: raw.len(),
                message: error.to_string(),
            }),
        }
    }
    Ok((out, skipped))
}

/// Splits `path` into lines, decoding each **lossily and separately**. An absent
/// file reads as no lines.
///
/// Bytes, not a `String`, for the reason [`read_jsonl_lenient`] states above: a
/// torn write can split a multi-byte codepoint, and a whole-file UTF-8 decode
/// fails on that one bad byte. Decoding per line turns whole-file loss into one
/// mangled line the caller can quarantine.
///
/// Deliberately returns **every** segment the split produced, blank ones
/// included, and parses nothing. Both are what the runtime journal needs: it
/// numbers corrupt lines by position, so dropping blanks here would shift every
/// report after one, and it owns the decision about what a line means (see
/// [`JournalStore`](crate::ports::journal::JournalStore)).
pub(crate) async fn read_lines_lossy(path: &Path) -> Result<Vec<String>> {
    let contents = match tokio::fs::read(path).await {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(path, e)),
    };
    Ok(contents
        .split(|b| *b == b'\n')
        .map(|raw| String::from_utf8_lossy(raw).into_owned())
        .collect())
}

/// Atomically writes `contents` to `path` via a temp file + rename.
pub(crate) async fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    write_atomic_bytes(path, contents.as_bytes()).await
}

/// Atomically writes `bytes` to `path` via a temp file + rename.
///
/// The byte-taking half of [`write_atomic`], which delegates here so the
/// tmp-then-rename dance has exactly one implementation: a second copy is how
/// one of the two paths ends up missing the `create_dir_all`, or renaming
/// before the write is flushed, with nothing to say the two ever disagreed.
///
/// What the rename buys is that **no reader ever sees a partial file** (issue
/// #887). A plain `tokio::fs::write` opens with `O_TRUNC` and then streams: for
/// the whole of that window the file on disk is short, and a concurrent reader
/// gets whatever had landed. On a workspace note that surfaces two ways, and
/// the quieter one is worse — `read_to_string` fails with `InvalidData` when the
/// cut lands mid-codepoint, which at least produces a red step, but when the cut
/// lands *on* a codepoint boundary the read **succeeds** and the agent grounds
/// an answer in half a document with nothing anywhere saying so. A `rename(2)`
/// over the same directory is atomic, so the reader sees the old file or the new
/// one and never a prefix of either.
///
/// ## Durable, not only atomic (issue #1049)
///
/// The rename orders the *publish*; it does not make the bytes **survive**. A
/// `rename(2)` returning is a promise about what a concurrent reader sees, not
/// about what is on the platter — power loss between the rename and the
/// kernel's writeback can leave the new name pointing at an inode whose data
/// never landed, so the file comes back empty or holding the previous contents.
/// No corruption and no error: a save the caller was told succeeded, silently
/// gone.
///
/// So the write is flushed in the order the recipe requires:
///
/// 1. `sync_data` on the temp file, **before** the rename — the bytes have to be
///    durable before the name that publishes them exists, or the crash window
///    just moves.
/// 2. the `rename`.
/// 3. [`sync_parent_dir`] **after** it.
///
/// ### Why step 3 is unconditional, unlike the append path
///
/// `append_line_inner` flushes the parent directory only when *this* append
/// created the file (`if creating`), because an append to an existing file
/// changes no directory entry. **That guard does not transfer here, and copying
/// it would leave the fix half done.** A rename repoints an existing name at a
/// different inode, so *every* call through here changes the parent directory's
/// block — flush the file but not the directory and a crash can leave the name
/// still resolving to the old inode, with the new data durable under a name
/// nothing refers to. This is the half that is easy to forget, precisely because
/// the file-level sync looks like it finished the job.
///
/// ### Cost, and why every caller pays it
///
/// Two device flushes per save, and they are the expensive syscalls in a
/// function whose others are cheap — not a rounding error on the existing
/// `create_dir_all` + write + rename.
///
/// Paid unconditionally anyway, because every caller of this function is a
/// **state save whose caller was told it succeeded**: the task list, agent
/// specs, users, invites, sessions, auth codes, skill states, notification read
/// markers, workspace notes and their index. Not one is a cache or a
/// re-derivable artifact, so there is no clean line to cut along — and the cost
/// of drawing that line wrong is a silent lost update, which is the bug this
/// fixes. `append_line` / [`append_line_durable`] split because *there* the line
/// is clean (routine journal chatter versus `EffectExecuted`, which records real
/// money); no equivalent split exists among these callers.
///
/// The exposure is bounded too: the fs backend is the desktop and self-hosted
/// deployment. Hosted tenants run mongodb and never reach this function, so the
/// write volume behind these flushes is one machine's own activity. If a hot
/// caller is ever *measured* to hurt, the `sync: bool` shape `append_line_inner`
/// already uses is the precedent to copy — with evidence, rather than guessed at
/// now.
pub(crate) async fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = stage_atomic_bytes(path, bytes).await?;
    commit_staged(path, tmp).await.map_err(|f| f.error)
}

/// The write-and-fsync half of [`write_atomic_bytes`], split out so a caller
/// that must publish more than one file "at once" (`FsCompanyStore::save`,
/// issue #1828 review, fourth round) can durably stage every file's bytes
/// *before* any of them is published.
///
/// That ordering matters because staging — serialize, open, write, fsync —
/// is where a save actually fails in practice: a transient I/O error, a full
/// disk, a denied ACL. None of those touch `path`; they only touch the temp
/// file this returns. So if `save` stages both `company.toml` and
/// `meta.json` before calling [`commit_staged`] on either, a failure here
/// leaves **neither** live file touched, regardless of which file failed or
/// which order the two are eventually committed in. Only [`commit_staged`]'s
/// rename can leave one file updated and not the other, and a bare rename
/// over an already-fsynced temp file is a far smaller, cheaper failure
/// window than the write this function does — small enough that the commit
/// order `save` already picks (first-publish vs. update) is left to police
/// it, rather than needing a corresponding "stage order."
///
/// Every other caller of [`write_atomic_bytes`] gets the same fault-injection
/// point and the same durability recipe as before — this only splits *when*
/// the temp file is published from *when* it is written.
///
/// ### Cancellation safety (issue #1828 review, sixth round)
///
/// `tokio::task::spawn_blocking` cannot be cancelled: dropping the
/// `JoinHandle` future stops nothing, it only discards the result. If the
/// future *calling* this function is itself dropped while on the `.await`
/// below — a task `abort()`ed, or an axum handler cancelled by a client
/// disconnect mid-`save` — the old direct `spawn_blocking(...).await` lost
/// the only reference to `tmp`: the write still finishes on the blocking
/// pool regardless, leaving a fully written, fsynced temp file that neither
/// [`commit_staged`] nor [`remove_staged`] will ever run against, because
/// the caller that would call either one never got the path back.
///
/// The write now runs inside a *detached* task, with the result routed back
/// through a [`tokio::sync::oneshot`] channel instead of directly through
/// the `spawn_blocking` `JoinHandle`. On the non-cancelled path this changes
/// nothing observable — the write still happens on `spawn_blocking` exactly
/// as before, and `rx.await` yields the same `Ok(tmp)` / `Err(_)` the direct
/// call did. What it adds is the one case that matters: `tx.send` failing
/// because `rx` — and the caller awaiting it — was dropped is now the
/// detached task's own signal to remove the orphan it just wrote, rather
/// than depending on downstream code that was cancelled before it could run.
async fn stage_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    // Test-only: see `fault_probe`. Fails before any filesystem call, like a
    // real early I/O error — no temp file, no partial write under `path`.
    #[cfg(test)]
    if fault_probe::should_fail(path) {
        return Err(io_err(
            path,
            std::io::Error::other("injected test failure (fault_probe)"),
        ));
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io_err(parent, e))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", generate_id()));
    let owned_tmp = tmp.clone();
    // Only read back under `#[cfg(test)]` below now that staging no longer
    // renames — a non-test build would otherwise warn this unused.
    #[cfg_attr(not(test), allow(unused_variables))]
    let owned_path = path.to_path_buf();
    let bytes = bytes.to_vec();
    let cleanup_tmp = tmp.clone();
    // Separate from `owned_path` above: that one is moved into the
    // `spawn_blocking` closure, so the outer detached task needs its own
    // clone to key the post-send probe hook below by the original `path`.
    #[cfg_attr(not(test), allow(unused_variables))]
    let notify_path = path.to_path_buf();

    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            // Test-only: see `stall_probe`. Parks this blocking-pool thread
            // until the test releases it, giving it a window to cancel the
            // caller below while the write has not happened yet.
            #[cfg(test)]
            stall_probe::maybe_block(&owned_path);
            let mut file = std::fs::File::create(&owned_tmp).map_err(|e| io_err(&owned_tmp, e))?;
            // Test-only: see `fault_probe::fail_next_mid_write`. Simulates a
            // write/fsync failure *after* the temp file already exists on
            // disk, which `fault_probe::should_fail` above cannot — it only
            // fires before any filesystem call.
            #[cfg(test)]
            if fault_probe::should_fail_mid_write(&owned_path) {
                return Err(io_err(
                    &owned_tmp,
                    std::io::Error::other("injected test failure (fault_probe mid-write)"),
                ));
            }
            file.write_all(&bytes).map_err(|e| io_err(&owned_tmp, e))?;
            // Before the rename, deliberately — see the recipe on
            // `write_atomic_bytes`'s doc comment.
            file.sync_data().map_err(|e| io_err(&owned_tmp, e))?;
            // Recorded here rather than at the end of the block, and that placement
            // is the whole value of the probe: tallying on the way out would count
            // the *function running*, so deleting the `sync_data` above would leave
            // every assertion still passing. Tied to the call, removing the call
            // fails the test.
            #[cfg(test)]
            append_probe::record_atomic_sync(&owned_path);
            Ok::<_, OpenCompanyError>(())
        })
        .await
        .map_err(|e| OpenCompanyError::Store(format!("spawn_blocking failed: {e}")))
        .and_then(|inner| inner);

        // Issue #1828 review, seventh round: a write failure is not only
        // reachable via a dropped `rx`. If `File::create` above succeeded
        // and `write_all`/`sync_data` then failed, `result` is `Err` even
        // though the caller is still there awaiting it — `tx.send` below
        // succeeds, so the old `is_err()`-only check never ran cleanup, and
        // the caller only ever receives the `Err`, never `tmp`, so nothing
        // downstream (`commit_staged` / `remove_staged`) can reclaim it
        // either. Reclaim it here, unconditionally, whenever the write
        // itself failed — before even trying to send, since a failed send
        // still needs the same cleanup and this makes it happen exactly
        // once either way.
        if result.is_err() {
            remove_staged(&cleanup_tmp).await;
            let _ = tx.send(result);
            return;
        }

        // `rx` (and the caller awaiting it) is gone: the write already
        // landed on disk, so reclaim it here instead of leaving it for a
        // `commit_staged` / `remove_staged` call nobody is left to make.
        if tx.send(result).is_err() {
            remove_staged(&cleanup_tmp).await;
        } else {
            // Test-only: see `send_probe`. Fires the instant a successful
            // send lands, before this task does anything else, so a test
            // can race dropping the awaiting future against it.
            #[cfg(test)]
            send_probe::notify_sent(&notify_path);
        }
    });

    // Issue #1828 review, seventeenth round (finding on 3878696002): the
    // detached task above only reclaims `tmp` when `tx.send` itself fails,
    // i.e. when `rx` was already dropped *before* the send. `send` returning
    // `Ok` only means the `Receiver` existed at that instant — it does not
    // mean this function's own future ever gets polled again to retrieve
    // it. A oneshot value that is sent successfully but whose receiver is
    // dropped before actually consuming it (this future aborted or
    // otherwise torn down between the send and this call's next poll) is
    // simply discarded: no error, no signal, nothing downstream ever runs.
    // That window is real — `send` completing and this task resuming are
    // two independently-scheduled events — and the caller loses the only
    // other reference to `tmp` right along with it, exactly like the
    // sixth-round hazard this function already exists to close.
    //
    // A local guard closes it without needing to know which of those two
    // events happened first: it watches `tmp` from before the handoff and
    // is disarmed only once `rx.await` has actually completed, so any drop
    // of *this* future in between — regardless of whether the detached
    // task's `send` raced ahead of it — reclaims the file in `Drop`, same
    // as `StagedGuard` already does for `save`'s own cancellation window.
    let mut guard = StagedGuard::new();
    guard.watch(&tmp);

    rx.await
        .map_err(|_| OpenCompanyError::Store("stage task dropped before completing".into()))??;

    // The value was both sent and actually consumed here — ownership of
    // `tmp` now passes to our caller (`commit_staged`, `remove_staged`, or
    // `save`'s own `StagedGuard`), so stop guarding it ourselves.
    guard.disarm();
    Ok(tmp)
}

/// The rename-and-fsync half of [`write_atomic_bytes`]: publishes a temp
/// file previously staged by [`stage_atomic_bytes`] over `path`, then
/// flushes the parent directory. See that function's doc comment for why
/// the two are split.
/// Why a commit failed — and, crucially, whether the rename already landed.
///
/// `rename` and the parent-directory fsync are separate steps, so a plain
/// `Err` conflates two very different states: nothing was published, versus
/// the destination is already replaced and only its durability step failed
/// (issue #1828 review, tenth round). Callers that compensate on failure —
/// rolling a sibling file back, or treating a bundle as never created — need
/// to tell them apart, because compensating for a publish that *did* happen
/// is how a half-updated record is made.
#[derive(Debug)]
struct CommitFailure {
    /// `true` when the rename succeeded and only `sync_parent_dir` failed:
    /// readers already see the new contents, they are just not yet durable
    /// against a host crash.
    published: bool,
    error: OpenCompanyError,
}

async fn commit_staged(path: &Path, tmp: PathBuf) -> std::result::Result<(), CommitFailure> {
    #[cfg(test)]
    if fault_probe::should_fail_commit(path) {
        return Err(CommitFailure {
            published: false,
            error: io_err(
                path,
                std::io::Error::other("injected test failure (fault_probe commit)"),
            ),
        });
    }
    let owned_path = path.to_path_buf();

    // One `spawn_blocking` for the rename-then-sync pair rather than two
    // `tokio::fs` calls, mirroring `stage_atomic_bytes` above.
    let joined = tokio::task::spawn_blocking(move || {
        // Test-only: see `stall_probe::arm_commit`. Parks this blocking-pool
        // thread just before the rename, giving a test a deterministic
        // window to cancel the caller while the rename has not happened yet
        // but is genuinely about to.
        #[cfg(test)]
        stall_probe::maybe_block_commit(&owned_path);
        if let Err(e) = std::fs::rename(&tmp, &owned_path) {
            // Ownership of `tmp` passed to this job, so nothing upstream is
            // still guarding it — reclaim it here when the rename did not
            // land (issue #1828 review, eleventh round).
            let _ = std::fs::remove_file(&tmp);
            return Err(CommitFailure {
                published: false,
                error: io_err(&owned_path, e),
            });
        }
        // Test-only: see `fault_probe::fail_next_dir_sync`. Reaches the state
        // the plain fault probe cannot — rename landed, durability step did
        // not.
        #[cfg(test)]
        if fault_probe::should_fail_dir_sync(&owned_path) {
            return Err(CommitFailure {
                published: true,
                error: io_err(
                    &owned_path,
                    std::io::Error::other("injected test failure (fault_probe dir sync)"),
                ),
            });
        }
        // Unconditional: the rename changed this directory whether or not the
        // destination already existed.
        sync_parent_dir(&owned_path).map_err(|error| CommitFailure {
            published: true,
            error,
        })?;
        // Records the order this publish landed in, relative to any other
        // `write_atomic` / `commit_staged` call — see
        // `append_probe::write_order_for`'s doc comment for why a multi-file
        // save (`FsCompanyStore::save_gated`) needs this and a per-path
        // counter alone cannot answer it.
        #[cfg(test)]
        append_probe::record_write_order(&owned_path);
        Ok::<_, CommitFailure>(())
    })
    .await;
    match joined {
        Ok(result) => result,
        Err(e) => Err(CommitFailure {
            // The join itself failed, so whether the rename ran is unknown;
            // the conservative answer for a caller deciding whether to
            // compensate is "assume it may have landed".
            published: true,
            error: OpenCompanyError::Store(format!("spawn_blocking failed: {e}")),
        }),
    }
}

/// Best-effort cleanup of a temp file previously returned by
/// [`stage_atomic_bytes`], called when a multi-file save (`FsCompanyStore::save`,
/// issue #1828 review, fifth round) aborts after staging it but before — or
/// instead of — committing it.
///
/// Safe to call on a path [`commit_staged`] already renamed away: that is a
/// `NotFound` on `remove_file`, silently ignored, not a fault in either
/// direction. What it exists to stop is the other outcome — a staged temp
/// file (already written and fsynced, so it holds real bytes on disk) that
/// `save` returns `Err` without ever renaming into place. Left alone, each
/// failed attempt at the same save stages a fresh uniquely-named temp file
/// and abandons it, so a caller that retries into a persistent fault (a full
/// disk being the case this matters most: [`stage_atomic_bytes`] already
/// wrote and fsynced the file before the failure this cleans up after)
/// consumes more of the already-constrained filesystem on every retry,
/// working against the very recovery the retry is trying to achieve.
///
/// Errors other than `NotFound` are logged, not propagated: this runs on an
/// error path that is already returning the real failure to the caller, and
/// a second failure here must not shadow it or abort the cleanup of any
/// other temp file still owed.
/// Owns every temp path a multi-step save has staged but not yet committed,
/// so that **dropping** the save future cannot strand them.
///
/// Issue #1828 review, seventh round. Every explicit error path in
/// [`FsCompanyStore::save`] already calls [`remove_staged`], but none of them
/// runs when the future is simply dropped mid-flight — an aborted task, or an
/// axum handler cancelled by a client disconnect. `save` stages `meta.json`
/// and then awaits the staging of `company.toml`; cancelled in that window,
/// the first temp file is fsynced on disk and its only handle goes out of
/// scope with nothing left to reclaim it.
///
/// Cleanup runs in `Drop`, so it cannot await: it uses the blocking
/// `std::fs::remove_file` deliberately. That is a single `unlink` on a path
/// this process just created, and it must happen before the owning frame
/// disappears — deferring it to a spawned task would reintroduce the same
/// "nobody is left to run it" hole on a runtime that is shutting down.
///
/// Committing a staged file renames it away, so a later `Drop` sweep of that
/// path is a no-op; the guard stays armed to the end rather than trying to
/// track which renames have landed.
struct StagedGuard {
    tmps: Vec<PathBuf>,
}

impl StagedGuard {
    fn new() -> Self {
        Self { tmps: Vec::new() }
    }

    /// Start guarding `tmp` until this guard is dropped or disarmed.
    fn watch(&mut self, tmp: &Path) {
        self.tmps.push(tmp.to_path_buf());
    }

    /// Stop guarding one path without removing it. Used when ownership
    /// passes to `commit_staged`: its `spawn_blocking` job renames the temp
    /// and keeps running even if the caller's future is dropped, so a `Drop`
    /// sweep here would delete the file out from under a rename that is
    /// still going to happen — leaving the first file published against the
    /// second's `NotFound` (issue #1828 review, eleventh round).
    fn forget(&mut self, tmp: &Path) {
        self.tmps.retain(|held| held != tmp);
    }

    /// Release every guarded path without removing it — the save reached a
    /// point where the explicit paths own the outcome.
    fn disarm(&mut self) {
        self.tmps.clear();
    }
}

impl Drop for StagedGuard {
    fn drop(&mut self) {
        for tmp in self.tmps.drain(..) {
            match std::fs::remove_file(&tmp) {
                Ok(()) => {
                    tracing::warn!(
                        path = %tmp.display(),
                        "[store] reclaimed a staged temp file whose save was dropped mid-flight"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        path = %tmp.display(),
                        error = %e,
                        "[store] failed to reclaim a staged temp file after its save was dropped"
                    );
                }
            }
        }
    }
}

/// Test-only: parks [`remove_staged`] so a test can abort the task *while*
/// the error-path cleanup is awaiting, proving the guard stays armed across
/// it (issue #1828 review, eighth round). Keyed by the temp file's parent
/// directory, since the temp's own name is randomly generated — each test
/// owns its own bundle directory, so tests stay independent under the
/// default parallel harness.
#[cfg(test)]
pub(crate) mod cleanup_probe {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};
    use tokio::sync::Notify;

    static GATES: LazyLock<Mutex<HashMap<PathBuf, ()>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static BLOCKED: LazyLock<Notify> = LazyLock::new(Notify::new);

    fn key(dir: &Path) -> PathBuf {
        std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf())
    }

    /// Arms a one-shot stall for the next `remove_staged` of a temp file
    /// living directly in `dir`.
    pub(crate) fn arm(dir: &Path) {
        GATES
            .lock()
            .expect("cleanup-probe poisoned")
            .insert(key(dir), ());
    }

    /// No-op unless this temp's directory was armed. Notifies
    /// [`wait_blocked`], then parks forever — the test aborts the task
    /// rather than releasing it, which is the scenario under test.
    pub(crate) async fn maybe_block(tmp: &Path) {
        let armed = tmp
            .parent()
            .map(|dir| {
                GATES
                    .lock()
                    .expect("cleanup-probe poisoned")
                    .remove(&key(dir))
                    .is_some()
            })
            .unwrap_or(false);
        if armed {
            BLOCKED.notify_one();
            std::future::pending::<()>().await;
        }
    }

    /// Waits until an armed cleanup has reached its stall point.
    pub(crate) async fn wait_blocked() {
        BLOCKED.notified().await;
    }
}

async fn remove_staged(tmp: &Path) {
    #[cfg(test)]
    cleanup_probe::maybe_block(tmp).await;
    match tokio::fs::remove_file(tmp).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(
                path = %tmp.display(),
                error = %e,
                "[store] failed to remove orphaned staged temp file after a save error"
            );
        }
    }
}

/// The commit half of [`FsCompanyStore::save`]: forgets both staged temps
/// from `guard` in commit order, renames each into place, and runs the
/// manifest-rollback branch if the second commit fails outright.
///
/// Spawned as a detached task from `save` (issue #1828 review, twelfth
/// round follow-up, findings on 3878400729 / 3878400724) — see that call
/// site for why: nothing below this point may be torn down by `save`'s own
/// cancellation, because `commit_staged`'s rename cannot be stopped once
/// dispatched (issue #1828 review, sixth/eleventh rounds), and each
/// commit's failure-handling assumes the *other* file's temp is still
/// exactly where `guard` left it.
async fn commit_bundle_writes(
    bundle: Bundle,
    updating_existing_bundle: bool,
    meta_tmp: PathBuf,
    toml_tmp: PathBuf,
    mut guard: StagedGuard,
) -> Result<()> {
    if updating_existing_bundle {
        // The two commits are independent renames, so publishing
        // `company.toml` and then failing to publish `meta.json` would
        // return `Err` with the manifest change already durable — a save
        // that reports failure while a logo or name edit persists, and a
        // mixed record when the call changed both files (issue #1828
        // review, ninth round). Snapshot the manifest we are about to
        // overwrite so that failure can put it back. The file is a small
        // TOML document, so holding it in memory beats managing another
        // temp path.
        let previous_toml = match tokio::fs::read(&bundle.company_toml()).await {
            Ok(bytes) => Some(bytes),
            // Nothing to roll back to; `updating_existing_bundle` was
            // decided by a probe that can race a concurrent delete.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                remove_staged(&toml_tmp).await;
                remove_staged(&meta_tmp).await;
                return Err(io_err(&bundle.company_toml(), e));
            }
        };
        guard.forget(&toml_tmp);
        // Issue #1828 review, twelfth round follow-up (finding on
        // 3878400724): a post-rename directory-sync failure here used to be
        // treated exactly like a total failure — `meta_tmp` was discarded
        // and never even attempted, even though the manifest rename had
        // already landed. Readers then saw the new manifest paired with the
        // old metadata: the same mixed record the *second* commit's
        // `published` branch below (tenth round) already guards against,
        // just on the other commit. When `published` is true, keep going
        // instead of abandoning the metadata write, and only surface this
        // sync failure if nothing worse happens after it.
        let toml_sync_warning = match commit_staged(&bundle.company_toml(), toml_tmp.clone()).await
        {
            Ok(()) => None,
            Err(f) if f.published => {
                tracing::warn!(
                    path = %bundle.company_toml().display(),
                    error = %f.error,
                    "[store] company.toml was renamed into place but its directory sync \
                     failed; continuing to commit meta.json rather than abandoning it"
                );
                Some(f.error)
            }
            Err(f) => {
                remove_staged(&toml_tmp).await;
                remove_staged(&meta_tmp).await;
                return Err(f.error);
            }
        };
        guard.forget(&meta_tmp);
        if let Err(f) = commit_staged(&bundle.meta_json(), meta_tmp.clone()).await {
            remove_staged(&meta_tmp).await;
            // Only roll the manifest back when `meta.json` genuinely did
            // not publish. If its rename landed and only the durability
            // step failed, readers already see the new metadata —
            // restoring the old manifest would pair it with new metadata
            // and manufacture the mixed record this rollback exists to
            // prevent (issue #1828 review, tenth round).
            if f.published {
                tracing::warn!(
                    path = %bundle.meta_json().display(),
                    error = %f.error,
                    "[store] meta.json was renamed into place but its directory sync failed; \
                     leaving both new files published rather than rolling the manifest back \
                     onto newer metadata"
                );
                return Err(f.error);
            }
            if let Some(previous) = previous_toml {
                // Best-effort: the caller is already being told the save
                // failed, so a failed rollback must not mask the original
                // error — but it must be loud, because it is the one case
                // that leaves the record genuinely mixed.
                match stage_atomic_bytes(&bundle.company_toml(), &previous).await {
                    Ok(rollback_tmp) => {
                        // Ownership passes straight to the commit below;
                        // never guarded, for the same reason as above.
                        if let Err(rollback_failure) =
                            commit_staged(&bundle.company_toml(), rollback_tmp.clone()).await
                        {
                            let rollback_err = rollback_failure.error;
                            remove_staged(&rollback_tmp).await;
                            tracing::error!(
                                path = %bundle.company_toml().display(),
                                error = %rollback_err,
                                "[store] could not restore the previous manifest after a \
                                 failed meta.json commit — the bundle is left with the new \
                                 manifest and the old meta.json"
                            );
                        }
                    }
                    Err(rollback_err) => {
                        tracing::error!(
                            path = %bundle.company_toml().display(),
                            error = %rollback_err,
                            "[store] could not stage the previous manifest to roll back after \
                             a failed meta.json commit — the bundle is left with the new \
                             manifest and the old meta.json"
                        );
                    }
                }
            }
            return Err(f.error);
        }
        if let Some(warning) = toml_sync_warning {
            return Err(warning);
        }
    } else {
        guard.forget(&meta_tmp);
        if let Err(f) = commit_staged(&bundle.meta_json(), meta_tmp.clone()).await {
            remove_staged(&meta_tmp).await;
            remove_staged(&toml_tmp).await;
            return Err(f.error);
        }
        guard.forget(&toml_tmp);
        if let Err(f) = commit_staged(&bundle.company_toml(), toml_tmp.clone()).await {
            remove_staged(&toml_tmp).await;
            return Err(f.error);
        }
    }
    // Both files are committed: the renames moved each temp away, so the
    // guard has nothing left to reclaim.
    guard.disarm();
    Ok(())
}

/// Bundle metadata persisted alongside the manifest.
#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    lifecycle: String,
    /// The operator team overlay (teammates added outside the manifest).
    #[serde(default)]
    overlay_agents: Vec<crate::ports::types::OverlayAgent>,
    /// The operator desk-membership overlay (agents added to desks at runtime).
    #[serde(default)]
    overlay_desk_members: Vec<crate::ports::types::OverlayDeskMember>,
    /// The operator per-desk member-ordering overlay (desk hierarchy).
    #[serde(default)]
    overlay_desk_order: Vec<crate::ports::types::OverlayDeskOrder>,
    /// The operator desk-creation overlay (desks created at runtime).
    #[serde(default)]
    overlay_desks: Vec<crate::ports::types::OverlayDesk>,
    /// The operator workflow-authoring overlay (graphs created at runtime).
    /// Absent on meta files written before runtime workflow bodies persisted
    /// through the store, so `#[serde(default)]` keeps those loading.
    #[serde(default)]
    overlay_workflows: Vec<crate::ports::types::OverlayWorkflow>,
    /// The operator-set per-teammate daily spend caps (issue #343). Absent on
    /// meta files written before the console could write a budget, so
    /// `#[serde(default)]` keeps those loading with the manifest in charge.
    #[serde(default)]
    overlay_budgets: Vec<crate::ports::types::BudgetOverride>,
    /// The operator's edits of manifest-declared teammates. Absent on meta files
    /// written before the console could edit a blueprint teammate, and
    /// `#[serde(default)]` reads that absence as "nothing is overridden" —
    /// which leaves the manifest in charge, exactly as those companies ran.
    #[serde(default)]
    overlay_agent_edits: Vec<crate::ports::types::AgentOverride>,
    /// The ids of manifest teammates the operator has removed. Absent on meta
    /// files written before a blueprint teammate could be removed, which
    /// `#[serde(default)]` reads as "nobody was removed" — exactly how those
    /// companies ran.
    #[serde(default)]
    overlay_retired_agents: Vec<String>,
    /// The operator's `[policy]` override (issue #562). Absent on meta files
    /// written before the console could write a tier, so `#[serde(default)]`
    /// keeps those loading with the manifest's `[policy]` in charge.
    #[serde(default)]
    overlay_policy: Option<crate::ports::types::PolicyOverride>,
    /// The operator's console-added `[tools].allow` grants (issue #1796).
    /// Absent on meta files written before a connect surface could grant a
    /// namespace, and `#[serde(default)]` reads that absence as "the manifest's
    /// `[tools]` still decides" — exactly how those companies ran.
    #[serde(default)]
    overlay_tool_grants: Option<crate::ports::types::ToolGrantsOverride>,
    /// The operator-set per-desk tool ceilings. Absent on meta files written
    /// before desks could scope tools, and `#[serde(default)]` reads that
    /// absence as "no desk overrides a ceiling" — which leaves the manifest in
    /// charge, exactly as those companies ran.
    #[serde(default)]
    overlay_desk_tools: std::collections::BTreeMap<String, Vec<String>>,
    /// The workflow ids the operator has switched off (issue #276). Absent on
    /// meta files written before the pause switch existed, and
    /// `#[serde(default)]` reads that absence as "nothing is paused" — which is
    /// exactly what those companies meant.
    #[serde(default)]
    disabled_workflows: Vec<String>,
    /// The source-template provenance stamped at launch. `None` for companies
    /// provisioned from a raw manifest and for legacy meta files written before
    /// provenance existed (the `#[serde(default)]` keeps those loading).
    #[serde(default)]
    template_provenance: Option<crate::ports::types::TemplateProvenance>,
    /// What the operator told first-run setup about their business. Absent on
    /// meta files written before setup existed, and for any company whose
    /// operator never answered; `#[serde(default)]` keeps those loading.
    #[serde(default)]
    setup: Option<crate::company::setup::SetupAnswers>,
    /// Whether the operator has confirmed the company's display name
    /// (issue #1843). See [`crate::ports::types::CompanyRecord::name_confirmed`].
    #[serde(default)]
    name_confirmed: bool,
    /// Epoch-millis the activation funnel completed (issue #1843). See
    /// [`crate::ports::types::CompanyRecord::activation_completed_at`].
    #[serde(default)]
    activation_completed_at: Option<u64>,
    /// Epoch-millis this record was first created. See
    /// [`crate::ports::types::CompanyRecord::created_at_millis`].
    #[serde(default)]
    created_at_millis: Option<u64>,
    /// Whether this bundle has ever been saved by activation-aware code — the
    /// on-disk marker behind [`CompanyStore::activation_gate_seen`]
    /// (PR #1875 review finding).
    ///
    /// `#[serde(default)]` reads a meta.json written before this field
    /// existed as `false`: a genuinely pre-#1843 record. `save` below always
    /// writes `true`, since every save from this build understands the
    /// activation funnel — which is what makes a *second* save of the same
    /// bundle (e.g. a restart before the operator finishes onboarding)
    /// distinguishable from a bundle that predates activation tracking
    /// entirely, even though both can otherwise have the identical
    /// `lifecycle == "running"`, `activation_completed_at: None` shape.
    ///
    /// [`CompanyStore::activation_gate_seen`]: crate::ports::store::CompanyStore::activation_gate_seen
    #[serde(default)]
    activation_gate_seen: bool,
}

impl Default for Meta {
    /// A bundle whose meta file has not been written yet: a running company
    /// with no operator overlays and no recorded provenance. `lifecycle` is
    /// `"running"` rather than `String::default()` — an empty lifecycle is not
    /// a state this runtime has.
    fn default() -> Self {
        Self {
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_retired_agents: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
            activation_gate_seen: false,
        }
    }
}

// ---------------------------------------------------------------------------
// CompanyStore
// ---------------------------------------------------------------------------

/// Filesystem [`CompanyStore`]: the manifest as TOML, lifecycle as JSON, and an
/// append-only ledger.
#[derive(Clone)]
pub struct FsCompanyStore {
    root: PathBuf,
}

impl FsCompanyStore {
    /// Creates a store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }

    /// The shared body of `save` and `save_importing`: writes the meta file
    /// and manifest, stamping `activation_gate_seen` with whatever the
    /// caller passes rather than always `true`. See
    /// `CompanyStore::save_importing`'s doc comment for why the two callers
    /// need different values.
    ///
    /// ## Write order depends on whether the bundle already exists (PR #1875
    /// review finding)
    ///
    /// `FsCompanyStore::load` decides whether a bundle exists **at all** by
    /// whether `company.toml` is present — `meta.json` is read only once that
    /// check has already passed. That makes `company.toml` the file whose
    /// existence *publishes* the bundle. A crash between the two writes
    /// always leaves one of them stale, and which file needs to be the
    /// stale one flips depending on whether this call is a first-time
    /// publish or an update to a bundle that is already live:
    ///
    /// - **First publish** (no `company.toml` yet): `meta.json` first. A
    ///   crash after `company.toml`'s rename but before `meta.json`'s leaves
    ///   a bundle `load` reports as existing, with `lifecycle == "running"`
    ///   and — because `meta.json` is missing — `activation_gate_seen`
    ///   defaulting to `false`. That is byte-for-byte the shape
    ///   `RuntimeBuilder::build`'s grandfather migration matches on, so a
    ///   fresh company's interrupted first boot gets silently auto-activated
    ///   (issue #1843). Meta-first fails toward the *safe* state instead: a
    ///   crash after `meta.json`'s rename but before `company.toml`'s leaves
    ///   `company.toml` still absent, so `load` reports the bundle as not
    ///   existing yet — worst case a retried save, never an unseen
    ///   activation.
    ///
    /// - **Update to an existing bundle**: `company.toml` first — the
    ///   opposite order, and load-bearing for `PATCH {scope}`'s name-confirm
    ///   write (`company_profile::patch_company`, issue #1844), which flips
    ///   `name_confirmed` and the manifest name in the same save. Meta-first
    ///   here would durably land `name_confirmed: true` while `company.toml`
    ///   still carried the pre-rename placeholder name if the process died
    ///   between the two writes — and because `name_confirmed` is what hides
    ///   the console's only rename control, that mismatch has no way back
    ///   through the UI; the next rebuild would carry the *wrong* name
    ///   forward forever, confirmed. Manifest-first fails toward the
    ///   recoverable state instead: a crash after `company.toml`'s rename but
    ///   before `meta.json`'s leaves `load` reading the OLD `meta.json` —
    ///   `name_confirmed` still `false` — so the console simply re-shows the
    ///   rename step, and resubmitting it is idempotent by design (see
    ///   `patch_company`'s own doc comment).
    ///
    ///   This does not reopen the first-publish hazard above: that hazard
    ///   needs a **missing** `meta.json` to make `load` default
    ///   `activation_gate_seen` to `false` via `Meta::default()`, which can
    ///   only happen on a bundle's very first save. An update's `meta.json`
    ///   already exists from a prior save, so if this save's rewrite of it
    ///   never lands, `load` falls back to that OLD `meta.json` — never a
    ///   default — and its gate marker is untouched either way.
    async fn save_gated(&self, record: &CompanyRecord, activation_gate_seen: bool) -> Result<()> {
        let bundle = self.bundle(&record.id);
        bundle.ensure_dirs().await?;

        let toml_src = toml::to_string(&record.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;

        let meta = Meta {
            lifecycle: record.lifecycle.clone(),
            overlay_agents: record.overlay_agents.clone(),
            overlay_desk_members: record.overlay_desk_members.clone(),
            overlay_desk_order: record.overlay_desk_order.clone(),
            overlay_desks: record.overlay_desks.clone(),
            overlay_workflows: record.overlay_workflows.clone(),
            overlay_budgets: record.overlay_budgets.clone(),
            overlay_agent_edits: record.overlay_agent_edits.clone(),
            overlay_retired_agents: record.overlay_retired_agents.clone(),
            overlay_policy: record.overlay_policy.clone(),
            overlay_tool_grants: record.overlay_tool_grants.clone(),
            overlay_desk_tools: record.overlay_desk_tools.clone(),
            disabled_workflows: record.disabled_workflows.clone(),
            template_provenance: record.template_provenance.clone(),
            setup: record.setup.clone(),
            name_confirmed: record.name_confirmed,
            activation_completed_at: record.activation_completed_at,
            created_at_millis: record.created_at_millis,
            activation_gate_seen,
        };
        // Write order depends on whether the bundle already exists (issue
        // #1828 review, second round).
        //
        // `load` treats a missing `company.toml` as "no such company"
        // (`Ok(None)`) but a missing `meta.json` as an *existing* company with
        // no overlays yet (`Meta::default()` — see its doc comment, load-
        // bearing for real bundles written before `meta.json` existed at all).
        // That asymmetry means the two writes below can never be made to look
        // atomic from the read side by ordering alone — but they can be made
        // to fail safe. Which order is safe flips depending on whether this
        // call is a first-time publish or an update to a bundle that is
        // already live:
        //
        // - **First publish** (no `company.toml` yet): `meta.json` first.
        //   Whichever file this function writes *last* is the one a crash or
        //   a transient write failure between the two calls leaves absent.
        //   Writing `company.toml` last means that failure window always
        //   reads back as `Ok(None)`, so an interrupted `save` (mid-create or
        //   mid-reset) looks like it never started rather than like a
        //   successfully provisioned company — the provision-path pre-check
        //   that calls `load` before every create/reset can then retry
        //   instead of returning `company_exists` forever over an orphaned
        //   bundle it can never load a usable record from.
        //
        // - **Update to an existing bundle**: `company.toml` first — the
        //   opposite order. `meta.json` carries the lifecycle and every
        //   overlay (workflows, budgets, policy, tool grants, …), so
        //   meta-first would let a save whose *second* write fails still
        //   durably commit those changes even though `save` returns `Err` to
        //   the caller: a resume can land `lifecycle == "running"` and then
        //   report a 500 and skip appending its audit event, and the same
        //   ambiguity reaches workflow and policy updates. `company.toml`
        //   first avoids that: if the second write (`meta.json`) then fails,
        //   `meta.json` is simply left as it was — old lifecycle, old
        //   overlays — so the update fails toward "nothing changed" instead
        //   of "changed anyway, but the caller was told it didn't." This does
        //   not reopen the first-publish hazard above: that hazard needs a
        //   *missing* `meta.json` to make `load` fall back to
        //   `Meta::default()`, which only happens on a bundle's very first
        //   save. An update's `meta.json` already exists from a prior save,
        //   so if this save's rewrite of it never lands, `load` falls back to
        //   that OLD `meta.json` — never a default.
        // `unwrap_or(false)` would misclassify a probe failure (e.g. a
        // transient I/O error or an ACL denial on the bundle directory) as
        // "no such bundle", steering an update onto the first-publish branch
        // below and reopening the exact partial-write hazard that branch
        // exists to avoid (issue #1828 review, third round). Propagate it
        // instead.
        #[cfg(test)]
        if fault_probe::should_fail_exists_check(&bundle.company_toml()) {
            return Err(io_err(
                &bundle.company_toml(),
                std::io::Error::other("injected test failure (fault_probe)"),
            ));
        }
        let updating_existing_bundle = tokio::fs::try_exists(&bundle.company_toml())
            .await
            .map_err(|e| io_err(&bundle.company_toml(), e))?;
        let meta_src = serde_json::to_string(&meta)?;

        // Invariant this function must hold, independent of the commit
        // order above: an `Err` from `save` must mean *neither* file
        // changed, not "one of the two changed, but I'm not telling you
        // which." The order alone can't deliver that — it only controls
        // which file survives *loss of the rename*, not which file
        // survives a failed *write*, and a write is the far more likely
        // failure (I/O error, a full disk, a denied ACL all land here,
        // never on a bare `rename(2)`). Issue #1828 review, fourth round:
        // the update order above protects `meta.json` (so a failed second
        // write never durably commits a lifecycle/overlay change) but, on
        // its own, does nothing to stop the *first* write — `company.toml`,
        // e.g. a manifest-only change like the logo endpoint's
        // `logo_url` — from landing durably even though the caller is
        // told the save failed when the second write then fails.
        //
        // So both files are staged (written + fsynced to a temp name, never
        // touching the live path) before either is committed (renamed into
        // place). A failure during staging — the likely case — now leaves
        // both live files exactly as they were, regardless of which file
        // failed or what order they're staged in; staging order is
        // deliberately not made to match commit order, to keep it visible
        // that the two are independent. Only a failure *between* the two
        // commits below (an already-fsynced rename failing) still lands
        // asymmetrically, and that residual, much smaller window is what
        // the create/update commit order was chosen to make safe.
        // A failure anywhere below this point must not just fail safe for
        // the *live* files (the invariant above) — it must not strand the
        // temp file(s) already staged either (issue #1828 review, fifth
        // round). Each is a fully written, fsynced file sitting on disk
        // under a name nothing will ever rename into place once `save`
        // returns `Err`; on a full disk, exactly the failure mode most
        // likely to land here, leaving it behind means every retry stages
        // another one and consumes more of the already-constrained
        // filesystem instead of recovering it. `remove_staged` is best-
        // effort and safe to call on a path that a later `commit_staged`
        // already renamed away, so every early return below cleans up
        // whichever staged temp file(s) are still sitting unrenamed.
        // Dropping this future — an aborted task, or a handler cancelled by a
        // client disconnect — runs none of the explicit `remove_staged` calls
        // below. `guard` covers exactly that: it owns each staged path from
        // the moment it exists until the save is committed or has failed
        // explicitly (issue #1828 review, seventh round).
        let mut guard = StagedGuard::new();
        let meta_tmp = stage_atomic_bytes(&bundle.meta_json(), meta_src.as_bytes()).await?;
        guard.watch(&meta_tmp);
        let toml_tmp = match stage_atomic_bytes(&bundle.company_toml(), toml_src.as_bytes()).await {
            Ok(tmp) => tmp,
            Err(e) => {
                // Deliberately NOT disarmed before this await: aborting the
                // task *during* the cleanup would otherwise leave `meta_tmp`
                // with nothing to reclaim it, which is the same hole this
                // guard exists to close (issue #1828 review, eighth round).
                // Once `remove_staged` has run, the guard's own `Drop` sweep
                // of that path is a harmless no-op.
                remove_staged(&meta_tmp).await;
                return Err(e);
            }
        };
        guard.watch(&toml_tmp);

        // Issue #1828 review, twelfth round follow-up (finding on
        // 3878400729): everything from here through the end of
        // `commit_bundle_writes` must run to completion as one unit even if
        // this `save` future is dropped mid-flight — a client disconnect or
        // a request timeout cancels the *caller*, not the process, and
        // `commit_staged`'s rename is deliberately uncancellable (sixth /
        // eleventh rounds) so it keeps running regardless. `guard` used to
        // live in this frame: cancelling while the *first* commit below was
        // in flight dropped `guard` synchronously, reclaiming the *other*,
        // not-yet-committed temp file — while the in-flight rename could
        // still land moments later, publishing one file against the
        // other's now-deleted staging. Detaching the whole decision into an
        // uncancellable task closes that window the same way
        // `stage_atomic_bytes` already closes it for staging: `save` can be
        // cancelled freely, but the unit it is waiting on cannot be.
        //
        // Detaching alone reopens a *different* hole (issue #1828 review,
        // finding on 3878896036): `company_write_lock` — the per-company
        // serialization every load-mutate-save caller (policy, team,
        // workflow, …) relies on — is held on the *caller's* frame, not on
        // this detached task. Cancel the caller while it is awaiting
        // `commit_rx` below and that guard drops immediately, even though
        // the commit it was meant to be guarding is still renaming files in
        // the background. A fresh caller can then acquire the now-free
        // `company_write_lock` and start its own save for the same bundle
        // while the orphaned commit is still in flight — nothing stops the
        // two detached commits from interleaving their renames, so the
        // newer save's files can be overwritten by the older, cancelled one
        // landing last.
        //
        // `commit_lock` closes that at the layer that actually owns the
        // race: a second, internal lock keyed on the bundle directory (so
        // both files' commits share one unit, same as `guard` above),
        // acquired here — before the caller's own lock can be dropped out
        // from under this call — and moved into the detached task so it
        // stays held for the commit's entire lifetime regardless of what
        // happens to the caller. Same ownership-transfer shape
        // `StagedGuard`/`forget` already uses for the staged temp files. No
        // `.await` sits between acquiring it and the `spawn` below, so a
        // caller cancelled in that gap either never acquired the lock
        // (nothing to release) or has already handed it to the spawned task
        // (nothing left on this frame to drop either way).
        let commit_lock = path_lock(bundle.dir()).lock_owned().await;
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel();
        let commit_bundle = bundle.clone();
        tokio::task::spawn(async move {
            let _commit_lock = commit_lock;
            let result = commit_bundle_writes(
                commit_bundle,
                updating_existing_bundle,
                meta_tmp,
                toml_tmp,
                guard,
            )
            .await;
            let _ = commit_tx.send(result);
        });
        commit_rx
            .await
            .map_err(|_| OpenCompanyError::Store("commit task dropped before completing".into()))?
    }
}

#[async_trait]
impl CompanyStore for FsCompanyStore {
    async fn load(&self, id: &CompanyId) -> Result<Option<CompanyRecord>> {
        let bundle = self.bundle(id);

        // Issue #1828 review, finding on comment 3879048530: `commit_lock` in
        // `save` (above) only serializes the *commit* phase across a
        // cancelled caller's orphaned detached task — it says nothing about
        // `load`, which used to read `company.toml`/`meta.json` straight off
        // disk with no lock at all. A fresh caller's `company_write_lock` is
        // freed the instant a cancelled caller's frame drops, so a fresh
        // load-mutate-save cycle (every real call site — `policy.rs`,
        // `team.rs`, …) could call `load` immediately, while the previous
        // caller's orphaned commit was still parked mid-rename, and read the
        // pre-commit record. That fresh caller's own `save` still blocks on
        // `commit_lock` until the orphaned commit finishes, so the two
        // commits never interleave on disk — but the fresh caller had
        // already merged its change onto stale data, so its save durably
        // *reverted* the orphaned commit's already-landed change the instant
        // it finally landed. A lost update, not a file-corruption race:
        // `commit_lock` alone cannot see it, because both commits still
        // succeed, in the correct order, on well-formed files.
        //
        // Acquiring and releasing the same `path_lock(bundle.dir())` the
        // commit phase holds, before reading either file, is a barrier:
        // `load` cannot return until any commit already in flight for this
        // bundle has fully finished renaming. It is safe to release
        // immediately rather than hold it across both reads below — the
        // per-company `company_write_lock` every load-mutate-save caller
        // acquires before `load` and keeps through `save` rules out any
        // *other* legitimate writer starting a new commit before this same
        // caller's own `save` reaches `commit_lock`, so the only commit this
        // barrier ever needs to wait out is the one already in flight when
        // `load` was called.
        {
            let _commit_barrier = path_lock(bundle.dir()).lock_owned().await;
        }

        let toml_path = bundle.company_toml();
        let toml_src = match tokio::fs::read_to_string(&toml_path).await {
            Ok(src) => src,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&toml_path, e)),
        };
        // `from_stored_toml`, not `toml::from_str`: a stored company is read far
        // more often than it is provisioned, and the global baseline has to
        // reach the companies that already exist — see
        // `CompanyManifest::apply_globals`.
        let manifest = crate::company::CompanyManifest::from_stored_toml(&toml_src)
            .map_err(|e| OpenCompanyError::Store(format!("invalid company.toml: {e}")))?;

        let meta_src = read_optional(&bundle.meta_json()).await?;
        // A bundle with no meta file yet is a running company with no overlays —
        // which is exactly `Meta::default()`. Carrying that through one value
        // rather than a positional tuple keeps adding an overlay collection a
        // one-line change here instead of a four-place one.
        let meta: Meta = if meta_src.trim().is_empty() {
            Meta::default()
        } else {
            serde_json::from_str(&meta_src)?
        };

        // The ledger is read leniently, and this is the only place in the fs
        // backend that is (issue #387). Every other read here is either a
        // rewriter — where skipping would delete the damaged line on write-back
        // — or a request-time reader, where a failure costs one request. This
        // one is neither: it is on the boot path, so a single malformed
        // accounting line used to take the whole company down, and the repair
        // console an operator would use sits behind the boot it killed.
        //
        // The skipped lines stay on disk untouched. Nothing in `load` writes,
        // and `append_ledger` only appends, so the file after a tolerated boot
        // is byte-identical to the file before it.
        let ledger_path = bundle.ledger_jsonl();
        let (ledger, skipped) = read_jsonl_lenient::<LedgerEntry>(&ledger_path).await?;
        if let Some(first) = skipped.first() {
            // `error!`, not `warn!`: the company is running on an incomplete
            // ledger, so its reported spend is wrong until someone repairs the
            // file. Loud once per load, naming the file and the first bad line —
            // never the line's contents, which are operator/agent free text.
            tracing::error!(
                company = %id,
                ledger = %ledger_path.display(),
                skipped = skipped.len(),
                first_line = first.line,
                first_bytes = first.bytes,
                error = %first.message,
                "[store] ledger lines could not be parsed; they were skipped so the company can still boot, and left on disk for repair — reported spend is incomplete until they are fixed"
            );
        }

        Ok(Some(CompanyRecord {
            overlay_agent_edits: meta.overlay_agent_edits,
            overlay_retired_agents: meta.overlay_retired_agents,
            id: id.clone(),
            manifest,
            ledger,
            lifecycle: meta.lifecycle,
            overlay_agents: meta.overlay_agents,
            overlay_desk_members: meta.overlay_desk_members,
            overlay_desk_order: meta.overlay_desk_order,
            overlay_desks: meta.overlay_desks,
            overlay_workflows: meta.overlay_workflows,
            overlay_budgets: meta.overlay_budgets,
            overlay_policy: meta.overlay_policy,
            overlay_tool_grants: meta.overlay_tool_grants,
            overlay_desk_tools: meta.overlay_desk_tools,
            disabled_workflows: meta.disabled_workflows,
            template_provenance: meta.template_provenance,
            setup: meta.setup,
            name_confirmed: meta.name_confirmed,
            activation_completed_at: meta.activation_completed_at,
            created_at_millis: meta.created_at_millis,
        }))
    }

    async fn save(&self, record: &CompanyRecord) -> Result<()> {
        // Every ordinary `save` call against a `running` company is, by
        // definition, made by code that understands the activation funnel —
        // see `CompanyStore::activation_gate_seen`'s doc comment. That
        // reasoning does NOT extend to a `paused`/`archived` record still
        // waiting on its own first `running` boot to decide the marker (PR
        // #1875 review finding, third round — mongodb.rs/sqlite.rs already
        // carry this): `RuntimeBuilder::build`'s "existing but not running"
        // arm carries the marker forward untouched for exactly this reason,
        // but a write that reaches this method directly — bypassing `build`
        // entirely, e.g. `company_logo::put_logo`'s plain load-modify-save,
        // which never checks lifecycle — would stamp `true` regardless and
        // poison the grandfather arm's `!gate_already_seen` guard before the
        // record's own migration boot ever runs. So: stamp `true` only once
        // the record itself says `running`; otherwise preserve whatever is
        // already on file, same as `build`'s own "not running" arm does.
        if record.lifecycle == "running" {
            self.save_gated(record, true).await
        } else {
            let gate_seen = self.activation_gate_seen(&record.id).await?;
            self.save_gated(record, gate_seen).await
        }
    }

    async fn save_importing(&self, record: &CompanyRecord, gate_seen: bool) -> Result<()> {
        self.save_gated(record, gate_seen).await
    }

    async fn activation_gate_seen(&self, id: &CompanyId) -> Result<bool> {
        let bundle = self.bundle(id);
        let meta_src = read_optional(&bundle.meta_json()).await?;
        if meta_src.trim().is_empty() {
            // No meta.json at all: this bundle has never been saved by any
            // code, activation-aware or not.
            return Ok(false);
        }
        let meta: Meta = serde_json::from_str(&meta_src)?;
        Ok(meta.activation_gate_seen)
    }

    async fn list(&self) -> Result<Vec<CompanySummary>> {
        let companies_dir = self.root.join("companies");
        let mut entries = match tokio::fs::read_dir(&companies_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&companies_dir, e)),
        };

        let mut out = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| io_err(&companies_dir, e))?
        {
            let dir = entry.path();
            let toml_path = dir.join("company.toml");
            let Ok(toml_src) = tokio::fs::read_to_string(&toml_path).await else {
                continue;
            };
            let manifest: crate::company::CompanyManifest = match toml::from_str(&toml_src) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let meta_src = read_optional(&dir.join("meta.json")).await?;
            let lifecycle = if meta_src.trim().is_empty() {
                "running".to_string()
            } else {
                serde_json::from_str::<Meta>(&meta_src)?.lifecycle
            };
            let id = entry.file_name().to_string_lossy().into_owned();
            out.push(CompanySummary {
                id: CompanyId::new(id),
                name: manifest.company.name,
                lifecycle,
            });
        }
        out.sort_by(|a, b| a.id.as_ref().cmp(b.id.as_ref()));
        Ok(out)
    }

    async fn append_ledger(&self, id: &CompanyId, entry: LedgerEntry) -> Result<()> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.ledger_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&entry)?).await
    }
}

// ---------------------------------------------------------------------------
// EventLog
// ---------------------------------------------------------------------------

/// Filesystem [`EventLog`]: append-only JSONL with a live broadcast fan-out for
/// subscribers.
#[derive(Clone)]
pub struct FsEventLog {
    root: PathBuf,
    /// Live subscribers, keyed by company.
    ///
    /// **Deliberately per-instance, and not part of issue #388's fix.** The
    /// write locks moved to a process-wide registry because two instances over
    /// one file must exclude each other. This map is the opposite kind of state:
    /// it is a fan-out to the subscribers *this* instance handed streams to, and
    /// nothing about durability or ordering depends on two instances sharing it.
    /// A subscriber that misses an event because it subscribed through the other
    /// instance is a delivery gap in a best-effort live feed, recoverable by
    /// [`read_from`](EventLog::read_from) against the durable log — not the
    /// silent write loss the lock change fixes. Making it global would give one
    /// company's stream process-global lifetime for a much weaker reason.
    senders: Arc<StdMutex<HashMap<CompanyId, broadcast::Sender<StoredEvent>>>>,
}

impl FsEventLog {
    /// Creates an event log rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            senders: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }

    fn sender_for(&self, id: &CompanyId) -> broadcast::Sender<StoredEvent> {
        let mut map = self.senders.lock().expect("sender map poisoned");
        map.entry(id.clone())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }
}

#[async_trait]
impl EventLog for FsEventLog {
    async fn append(&self, id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.events_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;

        // The next sequence is one past the highest already present; held under
        // the lock so concurrent appends never collide on a seq.
        //
        // Reading the *maximum* rather than the line count is what makes
        // `prune` safe (issue #275): a count-derived sequence silently reuses
        // the numbers of any removed lines, and sequences are stable ids —
        // thread parents, reaction targets and #358's redaction tombstone all
        // address a message by one. For a log that has never been pruned the
        // two agree exactly (n lines numbered 0..n-1 both yield n), so this
        // changes no existing behaviour.
        let existing = read_optional(&path).await?;
        let seq = existing
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<StoredEvent>(l).ok())
            .map(|ev| ev.seq.value() + 1)
            .max()
            .unwrap_or(0);

        let stored = StoredEvent {
            seq: EventSeq::new(seq),
            company: id.clone(),
            event,
            at_millis: now_millis(),
        };
        append_line(&path, &serde_json::to_string(&stored)?).await?;

        // Best-effort fan-out; a send error only means there are no live
        // subscribers, which is fine.
        let _ = self.sender_for(id).send(stored);
        Ok(EventSeq::new(seq))
    }

    async fn read_from(
        &self,
        id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let all = read_jsonl::<StoredEvent>(&self.bundle(id).events_jsonl()).await?;
        Ok(all
            .into_iter()
            .filter(|ev| ev.seq >= seq)
            .take(limit)
            .collect())
    }

    async fn read_before(
        &self,
        id: &CompanyId,
        before: Option<EventSeq>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let path = self.bundle(id).events_jsonl();
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_err(&path, error)),
        };
        let mut lines = BufReader::new(file).lines();
        // `usize::MAX` means an unlimited read for the EventLog port. Do not
        // treat that sentinel as an allocation request before streaming lines.
        let mut tail = VecDeque::new();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|error| io_err(&path, error))?
        {
            if line.trim().is_empty() {
                continue;
            }
            let event: StoredEvent = serde_json::from_str(&line)?;
            // Event logs are ordered by sequence. Once the cursor is reached,
            // no later line belongs to this page, so do not scan the tail.
            if before.is_some_and(|cursor| event.seq >= cursor) {
                break;
            }
            if tail.len() == limit {
                tail.pop_front();
            }
            tail.push_back(event);
        }
        Ok(tail.into_iter().rev().collect())
    }

    fn subscribe(&self, id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
        let rx = self.sender_for(id).subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            // Each call to this closure produces exactly one item and hands the
            // receiver back as continuation state, so there is no loop here.
            match rx.recv().await {
                Ok(event) => Some((EventStreamItem::Event(event), rx)),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    Some((EventStreamItem::Gap { missed }, rx))
                }
                Err(broadcast::error::RecvError::Closed) => None,
            }
        });
        Box::pin(stream)
    }

    async fn prune(&self, id: &CompanyId, policy: &RetentionPolicy) -> Result<PruneReport> {
        let path = self.bundle(id).events_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;

        // Strict parsing on purpose: `read_jsonl` fails the whole pass on a
        // line it cannot read, where `read_jsonl_lenient` would skip it and
        // then the rewrite below would drop it for good. A file we cannot
        // fully understand is a file we must not rewrite.
        let all = read_jsonl::<StoredEvent>(&path).await?;
        let doomed = plan_prune(&all, policy);

        let mut report = PruneReport {
            scanned: all.len(),
            removed: 0,
            oldest_retained: all.iter().map(|e| e.seq).min(),
        };
        if doomed.is_empty() {
            return Ok(report);
        }

        let kept: Vec<StoredEvent> = all
            .into_iter()
            .filter(|ev| doomed.binary_search(&ev.seq).is_err())
            .collect();
        report.removed = doomed.len();
        report.oldest_retained = kept.iter().map(|e| e.seq).min();

        let mut body = String::new();
        for record in &kept {
            body.push_str(&serde_json::to_string(record)?);
            body.push('\n');
        }
        write_atomic(&path, &body).await?;
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// Filesystem [`MemoryStore`]: compressed traces and task results as JSONL.
#[derive(Clone)]
pub struct FsMemoryStore {
    root: PathBuf,
}

impl FsMemoryStore {
    /// Creates a memory store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

#[async_trait]
impl MemoryStore for FsMemoryStore {
    async fn save_trace(&self, id: &CompanyId, trace: CompressedTrace) -> Result<()> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.traces_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&trace)?).await
    }

    async fn recent_traces(&self, id: &CompanyId, limit: usize) -> Result<Vec<CompressedTrace>> {
        let mut all = read_jsonl::<CompressedTrace>(&self.bundle(id).traces_jsonl()).await?;
        if all.len() > limit {
            all.drain(0..all.len() - limit);
        }
        Ok(all)
    }

    async fn save_task_result(&self, id: &CompanyId, result: TaskResult) -> Result<()> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let path = bundle.tasks_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &serde_json::to_string(&result)?).await
    }

    async fn evict(&self, id: &CompanyId, policy: EvictionPolicy) -> Result<u64> {
        let bundle = self.bundle(id);
        let path = bundle.traces_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;

        let all = read_jsonl::<CompressedTrace>(&path).await?;
        let before = all.len();
        let kept: Vec<CompressedTrace> = match policy {
            EvictionPolicy::KeepRecent { n } => {
                if all.len() > n {
                    all[all.len() - n..].to_vec()
                } else {
                    all
                }
            }
            EvictionPolicy::OlderThan { before_millis } => all
                .into_iter()
                .filter(|t| t.at_millis >= before_millis)
                .collect(),
        };
        let removed = (before - kept.len()) as u64;
        if removed > 0 {
            let body: String = kept
                .iter()
                .map(|t| serde_json::to_string(t).map(|s| s + "\n"))
                .collect::<std::result::Result<String, _>>()?;
            write_atomic(&path, &body).await?;
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// ContextStore
// ---------------------------------------------------------------------------

/// A context index line pairing an address with its label, length, and the
/// epoch-millis it was first stored.
///
/// `stored_at_millis` defaults to `0` so index lines written before the field
/// existed still deserialize — they simply report an unknown store time.
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    addr: String,
    label: String,
    len: usize,
    #[serde(default)]
    stored_at_millis: u64,
}

/// Filesystem [`ContextStore`]: content-addressed blobs plus a JSONL index.
///
/// Phase 1 uses a non-cryptographic [`DefaultHasher`] content id; a real
/// content hash (sha-256) is a documented follow-up.
#[derive(Clone)]
pub struct FsContextStore {
    root: PathBuf,
}

impl FsContextStore {
    /// Creates a context store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

/// Removes the unreferenced blob for `addr`, best-effort: the index rows are
/// already gone, so a blob that will not delete is orphaned and invisible
/// (list and search are index-driven) rather than turned into an error that
/// would tell the caller nothing was deleted after the index half already was.
async fn reap_blob(bundle: &Bundle, addr: &str) {
    let blob_path = bundle.context_blob(addr);
    match tokio::fs::remove_file(&blob_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            addr = %addr,
            path = %blob_path.display(),
            error = %e,
            "context index rows removed but the blob would not delete; \
             leaving an orphaned, unreferenced blob"
        ),
    }
}

#[async_trait]
impl ContextStore for FsContextStore {
    async fn put(&self, id: &CompanyId, chunk: ContextChunk) -> Result<ChunkAddr> {
        let bundle = self.bundle(id);
        bundle.ensure_dirs().await?;
        let addr = content_address(&chunk.body);

        // Blob write INSIDE the index lock: `delete` removes blob and index
        // rows under this lock, and a same-address put racing it from another
        // task (fact mirroring runs outside the cycle serial) could otherwise
        // interleave as write-blob / delete-both / append-index — an index
        // row pointing at a missing blob, where peek fails while list still
        // answers.
        let index_path = bundle.context_index_jsonl();
        let lock = path_lock(&index_path);
        let _guard = lock.lock().await;
        let blob_path = bundle.context_blob(&addr);
        // A re-`put` of an identical body rewrites an already-indexed blob
        // while a concurrent peek/search may be mid-read; only the tmp-then-
        // rename publish guarantees the reader full old bytes or full new
        // bytes, never a truncated file.
        write_atomic(&blob_path, &chunk.body).await?;
        // A plain append, deliberately: the (addr, label) set semantics #1300
        // pins are applied when the index is READ (see `list`), not by
        // checking membership here. Checking here would read and parse the
        // whole index on every write — and the ingest path writes one chunk
        // per document fragment, so a single folder drop would turn into a
        // quadratic scan. Appending keeps a write O(1), as it has always
        // been; a duplicate line costs one row and reads back as one claim.
        let entry = IndexEntry {
            addr: addr.clone(),
            label: chunk.label,
            len: chunk.body.len(),
            stored_at_millis: now_millis(),
        };
        append_line(&index_path, &serde_json::to_string(&entry)?).await?;
        Ok(ChunkAddr::new(addr))
    }

    async fn list(&self, id: &CompanyId, prefix: &str) -> Result<Vec<ChunkMeta>> {
        let index = read_jsonl::<IndexEntry>(&self.bundle(id).context_index_jsonl()).await?;
        // One claim per (addr, label) — the set semantics #1300 pins on every
        // backend — applied here rather than in `put`, which stays an O(1)
        // append (see its comment). The FIRST row for a pair wins, so the
        // stamp reported is the first write's, matching the other backends'
        // first-write-wins; later duplicate rows are a re-`put` of content
        // already claimed under that label and carry nothing new.
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut out = Vec::new();
        for entry in index {
            if !entry.label.starts_with(prefix) {
                continue;
            }
            if !seen.insert((entry.addr.clone(), entry.label.clone())) {
                continue;
            }
            out.push(ChunkMeta {
                addr: ChunkAddr::new(entry.addr),
                label: entry.label,
                len: entry.len,
                stored_at_millis: entry.stored_at_millis,
            });
        }
        Ok(out)
    }

    async fn peek(
        &self,
        id: &CompanyId,
        addr: &ChunkAddr,
        range: Option<Range<usize>>,
    ) -> Result<String> {
        let path = self.bundle(id).context_blob(addr.as_ref());
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| io_err(&path, e))?;
        match range {
            None => Ok(body),
            // Byte offsets from the caller can land mid-codepoint; widen to
            // the boundary rather than panic the slice.
            Some(r) => Ok(slice_on_char_boundaries(&body, r)),
        }
    }

    async fn delete(&self, id: &CompanyId, addr: &ChunkAddr) -> Result<bool> {
        let bundle = self.bundle(id);
        let index_path = bundle.context_index_jsonl();
        let lock = path_lock(&index_path);
        let _guard = lock.lock().await;
        // Strict read on purpose: this is a read-modify-write, and
        // `read_jsonl_lenient` is forbidden to rewriters — a damaged line must
        // abort the rewrite, not be laundered out of the file for good.
        let index = read_jsonl::<IndexEntry>(&index_path).await?;
        let before = index.len();
        let kept: Vec<IndexEntry> = index
            .into_iter()
            .filter(|e| e.addr != addr.as_ref())
            .collect();
        if kept.len() == before {
            return Ok(false);
        }
        crate::store::fs_ops::rewrite_jsonl(&index_path, &kept).await?;
        // The blob is shared by every index entry bearing this address (put
        // appends an entry per label, all pointing at one content-addressed
        // file). The filter above removed all of them, so the blob is
        // unreferenced and goes too — best-effort, because the index is the
        // source of truth and its rows are already gone: an orphaned blob is
        // invisible to list and search (both index-driven), and while a
        // direct `peek` of the exact addr can still read it until the file is
        // reclaimed (peek is blob-path-driven), a caller holding that addr
        // already held the body — nothing new is reachable. An `Err` here
        // would instead tell the caller nothing was deleted after the index
        // half already was.
        reap_blob(&bundle, addr.as_ref()).await;
        Ok(true)
    }

    async fn delete_label(&self, id: &CompanyId, addr: &ChunkAddr, label: &str) -> Result<bool> {
        let bundle = self.bundle(id);
        let index_path = bundle.context_index_jsonl();
        let lock = path_lock(&index_path);
        let _guard = lock.lock().await;
        // Strict read, same rule as `delete`: this is a read-modify-write, and
        // a damaged line must abort the rewrite, not be laundered out.
        let index = read_jsonl::<IndexEntry>(&index_path).await?;
        let before = index.len();
        let kept: Vec<IndexEntry> = index
            .into_iter()
            .filter(|e| !(e.addr == addr.as_ref() && e.label == label))
            .collect();
        if kept.len() == before {
            return Ok(false);
        }
        crate::store::fs_ops::rewrite_jsonl(&index_path, &kept).await?;
        // Label-scoped (#1300): only this label's row went. The blob is reaped
        // exactly when no row references the address any more — decided under
        // the same lock every put and delete holds, so a concurrent put of
        // identical content under another label either lands its row before
        // this read (and keeps the blob) or after this call completes (and
        // rewrites the blob it needs). Best-effort, per `delete`'s reasoning.
        if !kept.iter().any(|e| e.addr == addr.as_ref()) {
            reap_blob(&bundle, addr.as_ref()).await;
        }
        Ok(true)
    }

    /// Weighted token overlap rather than `body.find(query)` — see
    /// [`crate::store::lexical`]. One implementation for every backend, because
    /// these three stood here three times over with the same defect, and that is
    /// exactly the kind of thing that drifts apart again.
    ///
    /// Blobs are read one at a time and weighed immediately; only the snippets
    /// of candidates *with* overlap are kept.
    async fn search(&self, id: &CompanyId, query: &str, limit: usize) -> Result<Vec<ChunkHit>> {
        let mut ranker = crate::store::lexical::Ranker::new(query);
        if ranker.matches_nothing() {
            return Ok(Vec::new());
        }
        let bundle = self.bundle(id);
        let index = read_jsonl::<IndexEntry>(&bundle.context_index_jsonl()).await?;
        // One hit per ADDRESS, not per index row: a hit carries no label, and
        // one address can be claimed by several labels (#1300) — or repeated
        // by a duplicate row, since `put` appends without reading. Without
        // this, recall would report the same body once per claim, where every
        // other backend (which scans bodies, not claims) reports it once.
        let mut seen: HashSet<String> = HashSet::new();
        for entry in index {
            if !seen.insert(entry.addr.clone()) {
                continue;
            }
            let blob_path = bundle.context_blob(&entry.addr);
            let Ok(body) = tokio::fs::read_to_string(&blob_path).await else {
                continue;
            };
            ranker.offer(&entry.addr, &body);
        }
        Ok(ranker.best(limit))
    }
}

// ---------------------------------------------------------------------------
// SecretStore
// ---------------------------------------------------------------------------

/// Filesystem [`SecretStore`]: one file per key under the company's isolated
/// `secrets/` directory.
///
/// Isolation is structural: a secret path is always under the requesting
/// company's bundle, so company B cannot address company A's directory.
/// Encryption-at-rest is a documented follow-up; Phase 1 stores plaintext with
/// `0700` directory permissions on unix.
#[derive(Clone)]
pub struct FsSecretStore {
    root: PathBuf,
}

impl FsSecretStore {
    /// Creates a secret store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

#[async_trait]
impl SecretStore for FsSecretStore {
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
        let bundle = self.bundle(company);
        let path = bundle.secret(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(value) => Ok(Some(SecretValue(value))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let legacy_path = bundle.legacy_secret(key);
                match tokio::fs::read_to_string(&legacy_path).await {
                    Ok(value) => Ok(Some(SecretValue(value))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(io_err(&legacy_path, e)),
                }
            }
            Err(e) => Err(io_err(&path, e)),
        }
    }

    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.secret(key);
        tokio::fs::write(&path, value.expose())
            .await
            .map_err(|e| io_err(&path, e))?;

        // A clear is a revocation, not a migration. Keeping the legacy bytes
        // would let a colliding, not-yet-migrated alias resurrect the revoked
        // credential through the fallback. Remove the shared legacy file in
        // that case; the conservative result is that every alias loses the
        // ambiguous credential rather than any alias retaining a secret the
        // operator explicitly cleared.
        if value.expose().is_empty() {
            let legacy_path = bundle.legacy_secret(key);
            match tokio::fs::remove_file(&legacy_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&legacy_path, e)),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InboxStore
// ---------------------------------------------------------------------------

/// Filesystem [`InboxStore`]: one append-only `inbox.jsonl` per company holding
/// every inbox's mail interleaved. Reads filter by inbox in memory; the volumes
/// (a teammate's mail) stay well within a single-file scan.
#[derive(Clone)]
pub struct FsInboxStore {
    root: PathBuf,
}

impl FsInboxStore {
    /// Creates an inbox store rooted at `root` (the OpenCompany home).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn bundle(&self, id: &CompanyId) -> Bundle {
        Bundle::new(self.root.clone(), id)
    }
}

impl FsInboxStore {
    /// Loads the `key` → [`InboxMeta`] map, defaulting to empty.
    async fn load_meta(&self, company: &CompanyId) -> Result<HashMap<String, InboxMeta>> {
        let path = self.bundle(company).inbox_meta_json();
        let contents = read_optional(&path).await?;
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&contents)?)
    }
}

#[async_trait]
impl InboxStore for FsInboxStore {
    async fn inboxes(&self, company: &CompanyId) -> Result<Vec<InboxMeta>> {
        let meta = self.load_meta(company).await?;
        let all = read_jsonl::<EmailRecord>(&self.bundle(company).inbox_jsonl()).await?;
        // Start from explicit metadata, then synthesize a default enabled meta
        // for any inbox that only has messages.
        let mut out: HashMap<String, InboxMeta> = meta;
        for record in all {
            out.entry(record.inbox.clone())
                .or_insert_with(|| InboxMeta {
                    key: record.inbox.clone(),
                    name: record.inbox.clone(),
                    address: String::new(),
                    enabled: true,
                });
        }
        let mut list: Vec<InboxMeta> = out.into_values().collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(list)
    }

    async fn set_enabled(&self, company: &CompanyId, key: &str, meta: &InboxMeta) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.inbox_meta_json();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut map = self.load_meta(company).await?;
        map.insert(key.to_string(), meta.clone());
        write_atomic(&path, &serde_json::to_string(&map)?).await
    }

    async fn messages(
        &self,
        company: &CompanyId,
        key: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<EmailRecord>> {
        let all = read_jsonl::<EmailRecord>(&self.bundle(company).inbox_jsonl()).await?;
        Ok(all
            .into_iter()
            .filter(|r| r.inbox == key)
            .skip(offset)
            .take(limit)
            .collect())
    }

    async fn append(&self, company: &CompanyId, msg: &EmailRecord) -> Result<()> {
        let bundle = self.bundle(company);
        bundle.ensure_dirs().await?;
        let path = bundle.inbox_jsonl();
        let line = serde_json::to_string(msg)?;
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        append_line(&path, &line).await
    }

    async fn mark_read(
        &self,
        company: &CompanyId,
        key: &str,
        ids: Option<&[String]>,
    ) -> Result<u64> {
        let path = self.bundle(company).inbox_jsonl();
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        let mut all = read_jsonl::<EmailRecord>(&path).await?;
        for record in all.iter_mut() {
            if record.inbox != key {
                continue;
            }
            let hit = match ids {
                Some(ids) => ids.iter().any(|id| id == &record.id),
                None => true,
            };
            if hit {
                record.read = true;
            }
        }
        let unread = all.iter().filter(|r| r.inbox == key && !r.read).count() as u64;
        let body: String = all
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        let body = if body.is_empty() {
            String::new()
        } else {
            format!("{body}\n")
        };
        write_atomic(&path, &body).await?;
        Ok(unread)
    }
}

// ---------------------------------------------------------------------------
// JournalStore
// ---------------------------------------------------------------------------

/// The filesystem [`JournalStore`]: the `journal.jsonl` inside each company's
/// [`Bundle`], which is exactly where the runtime journal has always lived
/// (issue #726).
///
/// So the default backend migrates nothing — same path, same bytes, same
/// per-path locking. This type is the port surface around behaviour that already
/// existed, not new behaviour.
pub struct FsJournalStore {
    root: JournalRoot,
}

/// How an [`FsJournalStore`] resolves a company's journal file.
enum JournalRoot {
    /// A company-scoped store over an OpenCompany home:
    /// `<home>/companies/<slug>/journal.jsonl`. The production shape, and the
    /// only one that upholds the port's per-company isolation contract.
    Home(PathBuf),
    /// One fixed file, whatever company is asked for.
    ///
    /// Backs [`RuntimeJournal::new`](crate::runtime::journal::RuntimeJournal::new),
    /// the path-taking convenience constructor the test suite builds journals
    /// with. Single-company by construction — the caller named the file — so the
    /// id it is handed is not consulted, and the conformance suite's isolation
    /// assertions run against [`Home`](JournalRoot::Home) instead.
    File(PathBuf),
}

impl FsJournalStore {
    /// A store over every company bundle under the OpenCompany home `home`.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            root: JournalRoot::Home(home.into()),
        }
    }

    /// A store pinned to one journal file — see [`JournalRoot::File`].
    pub(crate) fn at_file(path: impl Into<PathBuf>) -> Self {
        Self {
            root: JournalRoot::File(path.into()),
        }
    }

    fn path(&self, id: &CompanyId) -> PathBuf {
        match &self.root {
            JournalRoot::Home(home) => Bundle::new(home, id).journal_jsonl(),
            JournalRoot::File(path) => path.clone(),
        }
    }
}

#[async_trait]
impl crate::ports::journal::JournalStore for FsJournalStore {
    /// One whole-line `O_APPEND` write, serialised on the process-wide per-path
    /// lock (issue #386, now reached through [`path_lock`]).
    ///
    /// The journal used to keep a `JOURNAL_WRITE_LOCKS` registry of its own,
    /// which was a second `PathLocks` holding the same kind of key for the same
    /// reason as [`FS_WRITE_LOCKS`]. One registry keyed on the absolutised path
    /// is what two independently-constructed stores over one file actually
    /// share, and there is no file both registries would have contended for —
    /// `journal.jsonl` is written here and nowhere else.
    ///
    /// **Both branches of issue #392 live here now**, moved from
    /// `RuntimeJournal::append` when the sink became a port (#726), and the
    /// directory half is the one that is easy to lose in the move:
    /// [`Durability::Host`] creates the parent chain through
    /// [`create_dir_all_durable`], which flushes each created directory's own
    /// parent. Plain `create_dir_all` here would leave a flushed record under
    /// directory entries that were never written down — lost with them on
    /// exactly the crash the flush was bought for, with the record's own
    /// `sync_data` still passing its test.
    async fn append_journal(
        &self,
        id: &CompanyId,
        line: &str,
        durability: crate::ports::journal::Durability,
    ) -> Result<()> {
        use crate::ports::journal::Durability;

        let path = self.path(id);
        let lock = path_lock(&path);
        let _guard = lock.lock().await;
        if let Some(parent) = path.parent() {
            match durability {
                Durability::Host => create_dir_all_durable(parent).await?,
                Durability::Process => tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| io_err(parent, e))?,
            }
        }
        match durability {
            Durability::Host => append_line_durable(&path, line).await,
            Durability::Process => append_line(&path, line).await,
        }
    }

    /// Every `\n`-separated segment of the file, minus the empty one the final
    /// terminator produces.
    ///
    /// Splitting `"a\nb\n"` yields a third, empty segment that is an artefact of
    /// the terminator rather than a record. Dropping exactly that one — the last,
    /// and only when it is empty — is what makes this backend's read agree
    /// element-for-element with a database backend's, which is what
    /// `assert_journal_store` holds every backend to. It shifts no line number:
    /// the discarded segment is past every record in the file, and a genuinely
    /// blank line in the middle is still returned so a corruption report counts
    /// it.
    async fn read_journal(&self, id: &CompanyId) -> Result<Vec<String>> {
        let mut lines = read_lines_lossy(&self.path(id)).await?;
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Ok(lines)
    }

    /// Always imported: this store *is* the file an import would copy from.
    async fn journal_imported(&self, _id: &CompanyId) -> Result<bool> {
        Ok(true)
    }

    /// Unreachable, and a no-op if reached.
    ///
    /// [`journal_imported`](Self::journal_imported) never opens the gate, so the
    /// builder never calls this. If some future caller does, copying the file's
    /// own lines back over itself is the identity — so doing nothing is the
    /// correct answer rather than a swallowed write.
    async fn complete_import(&self, _id: &CompanyId, _lines: Vec<String>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::store::conformance;
    use futures::StreamExt;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("opencompany-test-")
            .tempdir()
            .expect("tempdir")
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

    // The fs backend runs the identical port-conformance suite the sqlite
    // backend runs under `--features sqlite`. Each test gets a fresh root so the
    // stores start empty.
    #[tokio::test]
    async fn conformance_isolation_by_company() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_isolation_by_company(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
            Arc::new(FsMemoryStore::new(&root)),
            Arc::new(FsContextStore::new(&root)),
        )
        .await;
    }

    #[tokio::test]
    async fn conformance_paused_ordinary_save_preserves_activation_gate() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_paused_ordinary_save_preserves_activation_gate(Arc::new(
            FsCompanyStore::new(&root),
        ))
        .await;
    }

    #[tokio::test]
    async fn conformance_append_only_event_and_ledger() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_append_only_event_and_ledger(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
        )
        .await;
    }

    #[tokio::test]
    async fn conformance_monotonic_event_seq() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_monotonic_event_seq(Arc::new(FsEventLog::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_event_subscription_surfaces_gap() {
        let root_dir = tmp_root();
        conformance::assert_event_subscription_surfaces_gap(Arc::new(FsEventLog::new(
            root_dir.path(),
        )))
        .await;
    }

    #[tokio::test]
    async fn conformance_event_read_before() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_event_read_before(Arc::new(FsEventLog::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_event_retention() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_event_retention(Arc::new(FsEventLog::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_inbox_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_inbox_store(Arc::new(FsInboxStore::new(&root))).await;
    }

    /// Issue #1505. The port holds this company's inference credential, its MCP
    /// OAuth tokens and its SMTP password, and had no conformance case on any
    /// backend until this one.
    #[tokio::test]
    async fn conformance_secret_store() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_secret_store(Arc::new(FsSecretStore::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_context_chunk_stamps() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_context_chunk_stamps(Arc::new(FsContextStore::new(&root))).await;
    }

    // The fs backend keeps the port's default `peek_many` (per-file reads are
    // its floor), so this run is also the default implementation's own proof.
    #[tokio::test]
    async fn conformance_context_peek_many() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_context_peek_many_answers_positionally(Arc::new(FsContextStore::new(
            &root,
        )))
        .await;
    }

    #[tokio::test]
    async fn conformance_context_multibyte_bodies() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_multibyte_bodies_survive_search_and_ranged_peek(Arc::new(
            FsContextStore::new(&root),
        ))
        .await;
    }

    #[tokio::test]
    async fn conformance_context_identical_body_two_labels() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_identical_body_two_labels(Arc::new(FsContextStore::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_scoped() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_delete_label_scoped(Arc::new(FsContextStore::new(&root))).await;
    }

    #[tokio::test]
    async fn conformance_context_delete_label_survives_a_concurrent_identical_put() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_delete_label_survives_a_concurrent_identical_put(Arc::new(
            FsContextStore::new(&root),
        ))
        .await;
    }

    /// This backend keeps `put` an O(1) append and applies the (addr, label)
    /// set semantics on the read side (#1300), so the two halves need pinning
    /// together: a duplicate row on disk, exactly one claim through `list`,
    /// one hit through `search`, and a `delete_label` that takes every
    /// duplicate row with it — a survivor would resurrect a forgotten claim.
    #[tokio::test]
    async fn a_duplicate_index_row_reads_back_as_one_claim_and_deletes_whole() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let context = FsContextStore::new(&root);
        let id = CompanyId::new("acme");
        let chunk = || ContextChunk {
            label: "notes/one".to_string(),
            body: "remembered twice".to_string(),
        };

        let addr = context.put(&id, chunk()).await.unwrap();
        assert_eq!(context.put(&id, chunk()).await.unwrap(), addr);

        // The append really did write a second row — this is the cost the
        // read-side dedupe exists to absorb, so assert it rather than assume.
        let index_path = Bundle::new(root.clone(), &id).context_index_jsonl();
        let rows = read_jsonl::<IndexEntry>(&index_path).await.unwrap();
        assert_eq!(rows.len(), 2, "put appends without reading the index");

        let metas = context.list(&id, "").await.unwrap();
        assert_eq!(metas.len(), 1, "the duplicate row is one claim: {metas:?}");
        assert_eq!(metas[0].stored_at_millis, rows[0].stored_at_millis);
        assert_eq!(
            context.search(&id, "remembered", 10).await.unwrap().len(),
            1,
            "recall reports the body once, not once per row"
        );

        assert!(context.delete_label(&id, &addr, "notes/one").await.unwrap());
        assert!(context.list(&id, "").await.unwrap().is_empty());
        assert!(
            context.peek(&id, &addr, None).await.is_err(),
            "every duplicate row went, so the body is unreferenced and reaped"
        );
    }

    /// The deterministic half of the atomicity guarantee: a re-`put` publishes
    /// a *new* file over the old name, so the blob's inode changes. A plain
    /// truncating write — the shape that let a reader see a prefix — keeps the
    /// same inode and fails this every run, where the racing test below only
    /// reddens when the reader happens to land inside the truncate window.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_re_put_publishes_a_new_blob_instead_of_truncating_in_place() {
        use std::os::unix::fs::MetadataExt;

        let root_dir = tmp_root();
        let store = FsContextStore::new(root_dir.path().to_path_buf());
        let id = CompanyId::new("alpha");
        let chunk = ContextChunk {
            label: "agent/atomic".to_string(),
            body: "the same body, published twice".to_string(),
        };
        let addr = store.put(&id, chunk.clone()).await.unwrap();
        let blob_path = Bundle::new(root_dir.path().to_path_buf(), &id).context_blob(addr.as_ref());
        let before = tokio::fs::metadata(&blob_path).await.unwrap().ino();

        store.put(&id, chunk).await.unwrap();

        let after = tokio::fs::metadata(&blob_path).await.unwrap().ino();
        assert_ne!(
            before, after,
            "the blob was rewritten in place; a concurrent reader can see the \
             truncate window"
        );
    }

    /// A re-`put` of an already-indexed blob must never expose a torn body: the
    /// blob is republished via tmp-then-rename, so a racing `peek` sees the
    /// old bytes or the new bytes in full — with a plain truncating write it
    /// could read an empty or partial file for the whole write window.
    ///
    /// Racing, so its redness is probabilistic (the reader must land inside the
    /// write window); the inode test above is the every-run proof. This one
    /// guards what the inode cannot: that the bytes a racing reader *does* get
    /// are always a whole body.
    #[tokio::test]
    async fn a_concurrent_peek_never_sees_a_torn_blob_rewrite() {
        let root_dir = tmp_root();
        let store = Arc::new(FsContextStore::new(root_dir.path().to_path_buf()));
        let id = CompanyId::new("alpha");
        // Large enough that a truncate-then-stream write has a visible window.
        let body = "x".repeat(64 * 1024);
        let chunk = ContextChunk {
            label: "agent/atomic".to_string(),
            body: body.clone(),
        };
        let addr = store.put(&id, chunk.clone()).await.unwrap();

        for _ in 0..50 {
            let writer = {
                let store = store.clone();
                let id = id.clone();
                let chunk = chunk.clone();
                tokio::spawn(async move { store.put(&id, chunk).await.unwrap() })
            };
            let reader = {
                let store = store.clone();
                let id = id.clone();
                let addr = addr.clone();
                tokio::spawn(async move { store.peek(&id, &addr, None).await.unwrap() })
            };
            let read = reader.await.unwrap();
            assert_eq!(
                read.len(),
                body.len(),
                "a concurrent peek saw a torn blob rewrite"
            );
            assert_eq!(read, body);
            writer.await.unwrap();
        }
    }

    /// Once the index rows are gone the delete HAS happened; a blob that will
    /// not remove is an unreferenced orphan, and reporting an error for it
    /// would tell the caller nothing was deleted after half of it was.
    #[tokio::test]
    async fn delete_reports_the_index_result_even_when_the_blob_will_not_remove() {
        let root_dir = tmp_root();
        let store = FsContextStore::new(root_dir.path().to_path_buf());
        let id = CompanyId::new("alpha");
        let addr = store
            .put(
                &id,
                ContextChunk {
                    label: "agent/orphan".to_string(),
                    body: "orphan me".to_string(),
                },
            )
            .await
            .unwrap();

        // Swap the blob for a non-empty directory so `remove_file` must fail
        // with something other than NotFound, on every platform.
        let blob_path = Bundle::new(root_dir.path().to_path_buf(), &id).context_blob(addr.as_ref());
        tokio::fs::remove_file(&blob_path).await.unwrap();
        tokio::fs::create_dir(&blob_path).await.unwrap();
        tokio::fs::write(blob_path.join("occupant"), b"x")
            .await
            .unwrap();

        assert!(
            store.delete(&id, &addr).await.unwrap(),
            "the index rows were removed, so the delete happened"
        );
        assert!(
            store.list(&id, "").await.unwrap().is_empty(),
            "the index is the source of truth and it is empty"
        );
        assert!(
            store.search(&id, "orphan", 8).await.unwrap().is_empty(),
            "search is index-driven, so the orphan does not surface"
        );
        // Documented residual: peek is blob-path-driven, so the exact addr
        // can still read the orphan until the file is reclaimed. Here the
        // blob was replaced by a directory, so the read fails — the point
        // pinned is that delete's answer did not depend on it either way.
        let _ = store.peek(&id, &addr, None).await;
    }

    /// The same search semantics as every other backend.
    #[tokio::test]
    async fn conformance_context_search_ranking() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_context_search_ranking(Arc::new(FsContextStore::new(&root))).await;
    }

    /// Two event logs over one data root must not hand out the same sequence
    /// number (issue #388).
    ///
    /// `EventLog::append` computes the next `seq` by counting the lines already
    /// in the file, then appends. That read-then-append is only atomic under a
    /// lock, and the lock used to be a **field** on `FsEventLog` — so two
    /// instances over one bundle serialised against nothing, both read the same
    /// count, and both wrote the same `seq`. A duplicate sequence number breaks
    /// every consumer that treats it as an identity: `read_from`'s `seq >=`
    /// cursor silently replays, and the console's resume-from-seq skips.
    ///
    /// Nothing stops a second instance being constructed — `FsEventLog::new`
    /// takes a root and is called wherever one is needed — so this is reachable
    /// without any exotic setup, which is exactly what makes it worth a lock in
    /// the registry rather than a convention.
    #[tokio::test]
    async fn two_event_logs_over_one_root_never_reuse_a_sequence() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        // Two independently-constructed logs over the same data root — the shape
        // a second runtime, a maintenance task, or an export job produces.
        let first = Arc::new(FsEventLog::new(&root));
        let second = Arc::new(FsEventLog::new(&root));
        let id = CompanyId::new("acme");

        const N: u64 = 32;
        let mut set = tokio::task::JoinSet::new();
        for i in 0..N {
            let log = if i % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            let id = id.clone();
            set.spawn(async move {
                log.append(
                    &id,
                    CompanyEvent::OperatorMessage {
                        mentions: Vec::new(),
                        parent: None,
                        text: format!("event {i}"),
                        by: None,
                        chat: None,
                        deliverable: None,
                        attachments: Vec::new(),
                    },
                )
                .await
                .expect("append succeeds")
            });
        }
        let mut handed_out = Vec::new();
        while let Some(res) = set.join_next().await {
            handed_out.push(res.expect("task joins").value());
        }

        handed_out.sort_unstable();
        assert_eq!(
            handed_out,
            (0..N).collect::<Vec<_>>(),
            "the sequences handed to callers must be unique and dense — a repeat \
             means two instances read the same line count before either appended"
        );

        // And the same must hold for what actually landed on disk.
        let stored = first.read_from(&id, EventSeq::new(0), 1024).await.unwrap();
        assert_eq!(stored.len() as u64, N, "every append is on disk");
        let mut persisted: Vec<u64> = stored.iter().map(|e| e.seq.value()).collect();
        persisted.sort_unstable();
        assert_eq!(
            persisted,
            (0..N).collect::<Vec<_>>(),
            "the persisted sequences must be unique and dense too"
        );
    }

    /// The fs backend's migration path: index lines written before
    /// `stored_at_millis` existed carry no such field, and must still
    /// deserialize — reporting an unknown (`0`) store time rather than failing
    /// the read and blanking the whole Brain list.
    #[test]
    fn legacy_context_index_line_without_a_stamp_still_parses() {
        let legacy = r#"{"addr":"abc123","label":"agent/ceo","len":24}"#;
        let entry: IndexEntry = serde_json::from_str(legacy).expect("legacy index line parses");
        assert_eq!(entry.addr, "abc123");
        assert_eq!(entry.label, "agent/ceo");
        assert_eq!(entry.len, 24);
        assert_eq!(entry.stored_at_millis, 0);
    }

    #[tokio::test]
    async fn conformance_export_totality() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        conformance::assert_export_totality(
            Arc::new(FsCompanyStore::new(&root)),
            Arc::new(FsEventLog::new(&root)),
            Arc::new(FsMemoryStore::new(&root)),
            Arc::new(FsContextStore::new(&root)),
        )
        .await;
    }

    fn sample_manifest() -> crate::company::CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
        "#;
        toml::from_str(toml_src).expect("parse manifest")
    }

    #[tokio::test]
    async fn company_store_saves_and_loads() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
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
        };
        store.save(&record).await.unwrap();

        let loaded = store.load(&id).await.unwrap().expect("record exists");
        assert_eq!(loaded.manifest.company.name, "Acme");
        assert_eq!(loaded.lifecycle, "running");
        // One authored teammate, plus the global baseline the load path merges
        // into every stored manifest.
        assert_eq!(
            loaded
                .manifest
                .agents
                .iter()
                .filter(|agent| !agent.global)
                .count(),
            1
        );
        assert_eq!(
            loaded.manifest.agents.len(),
            1 + crate::globals::agents().len()
        );

        let summaries = store.list().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "Acme");

        assert!(
            store
                .load(&CompanyId::new("ghost"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn save_publishes_the_gate_marker_before_the_manifest() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: sample_manifest(),
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

        let order = append_probe::write_order_for(&[&bundle.meta_json(), &bundle.company_toml()]);
        assert_eq!(
            order,
            vec![bundle.meta_json(), bundle.company_toml()],
            "meta.json (the gate marker) must land before company.toml (what \
             `load` treats as the bundle's existence) — reversed, an \
             interrupted save can auto-activate a fresh company as a legacy \
             one on its next boot"
        );
    }

    /// **PR #1875 review finding**: the opposite ordering rule from the test
    /// above, for the opposite case — updating a bundle that is already live
    /// must publish `company.toml` before `meta.json`.
    ///
    /// Reversed (the first-publish order applied to an update too), a crash
    /// between the two writes during `PATCH {scope}`'s name-confirm save
    /// (`company_profile::patch_company`) can durably land `name_confirmed:
    /// true` while `company.toml` still carries the pre-rename placeholder
    /// name — and since `name_confirmed` is what hides the console's only
    /// rename control, that mismatched pair has no way back through the UI.
    /// Same reasoning as the sibling test: only the *order* the publishes
    /// land in distinguishes the two outcomes, which is what
    /// `append_probe::write_order_for` records.
    #[tokio::test]
    async fn updating_an_existing_bundle_publishes_the_manifest_before_the_gate_marker() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let first_save = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
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
        };
        // Publish the bundle for the first time — the create-path branch,
        // unaffected by this test's assertion.
        store.save(&first_save).await.unwrap();

        // The exact write `patch_company` performs: an existing bundle,
        // `name_confirmed` flips to `true` alongside the manifest name.
        let mut second_save = first_save;
        second_save.manifest.company.name = "Operator Chosen Name".to_string();
        second_save.name_confirmed = true;
        store.save(&second_save).await.unwrap();

        let order = append_probe::write_order_for(&[&bundle.meta_json(), &bundle.company_toml()]);
        // The log is global across both saves; the update's pair is the last
        // two entries.
        let update_order = &order[order.len() - 2..];
        assert_eq!(
            update_order,
            vec![bundle.company_toml(), bundle.meta_json()],
            "an update to an existing bundle must publish company.toml (the \
             name) before meta.json (name_confirmed) — reversed, an \
             interrupted rename save can durably confirm the wrong name with \
             no way back through the console"
        );
    }

    #[tokio::test]
    async fn a_save_interrupted_after_the_first_write_still_reads_back_as_absent() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        fault_probe::fail_next_write(&bundle.meta_json());

        let record = || CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
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
        };

        let err = store.save(&record()).await;
        assert!(
            err.is_err(),
            "the injected failure must propagate out of save"
        );

        // The one combination that would make `load` lie about the company
        // existing: `company.toml` must not have landed.
        assert!(
            !tokio::fs::try_exists(&bundle.company_toml()).await.unwrap(),
            "company.toml must not exist after the first write of save failed"
        );

        let loaded = store.load(&id).await.unwrap();
        assert!(
            loaded.is_none(),
            "a save interrupted before company.toml landed must read back as \
             absent, not as an existing company — retrying create/reset for \
             this id must be possible"
        );

        // `fail_next_write` is one-shot, so the retry hits the real write path
        // and must succeed.
        store
            .save(&record())
            .await
            .expect("retry succeeds once the fault is no longer armed");
        assert!(store.load(&id).await.unwrap().is_some());
    }

    /// **Issue #1828 review, second round**: the sibling window the test
    /// above never exercised. `meta.json` carries the lifecycle and every
    /// overlay; the original fix (this file's prior revision) wrote it
    /// *first*, unconditionally, for every save — first publish and update
    /// alike. That order is safe for a first publish (see the test above),
    /// but for an update it means a fault on `company.toml`'s write — the
    /// *second* write under that old, unconditional order — still leaves the
    /// lifecycle change durably on disk in `meta.json` even though `save`
    /// returns `Err` to the caller: a resume that reports a 500 while
    /// `lifecycle` already reads back `"running"`, with the audit event the
    /// caller only appends after a successful `save` never written.
    ///
    /// The fix makes the order conditional: an update to an already-existing
    /// bundle writes `company.toml` *first*, so the same fault — on
    /// `company.toml`'s write — now fails the update's *first* write instead,
    /// before `meta.json` is touched at all. This proves that directly:
    /// publish a bundle, then arm the fault on `company.toml` for an update
    /// save, and assert the lifecycle read back afterward is still the *old*
    /// value, never the new one — the update fails toward "nothing changed,"
    /// not "changed anyway, but the caller was told it didn't." It then
    /// retries without the fault and asserts the new lifecycle *does* land.
    #[tokio::test]
    async fn an_update_interrupted_on_the_second_write_does_not_persist_the_lifecycle_change() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |lifecycle: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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
        };

        // Publish the bundle for the first time — the create-path branch,
        // unaffected by this test's assertion.
        store
            .save(&record("provisioning"))
            .await
            .expect("first publish succeeds");

        // The update save this test exercises: a resume, flipping lifecycle
        // to "running". Under the old, unconditional order this fault landed
        // on the *second* write, after `meta.json` (the new lifecycle) had
        // already committed — the untested window the review flagged. Under
        // the fix, `save` writes `company.toml` first for an update, so this
        // same fault now fails the update's *first* write, before
        // `meta.json` is ever touched.
        fault_probe::fail_next_write(&bundle.company_toml());

        let err = store.save(&record("running")).await;
        assert!(
            err.is_err(),
            "the injected failure must propagate out of save"
        );

        let loaded = store
            .load(&id)
            .await
            .unwrap()
            .expect("the bundle still exists — only the update failed");
        assert_eq!(
            loaded.lifecycle, "provisioning",
            "a failed company.toml write during an update must leave \
             meta.json — and the lifecycle it carries — exactly as it was \
             before the update; the caller was told the resume failed, so \
             nothing about it may have taken effect"
        );

        // `fail_next_write` is one-shot, so the retry hits the real write
        // path and the lifecycle change lands normally.
        store
            .save(&record("running"))
            .await
            .expect("retry succeeds once the fault is no longer armed");
        assert_eq!(store.load(&id).await.unwrap().unwrap().lifecycle, "running");
    }

    /// **Issue #1828 review, fourth round**: the mirror image of the
    /// second-round hazard above, on the *other* file. For an update, `save`
    /// commits `company.toml` (the manifest — name, output, logo, …)
    /// *before* `meta.json`, so that a failure on `meta.json` never lands a
    /// lifecycle/overlay change the caller was told failed. But ordering the
    /// two *writes* that way means a fault on the *second* write —
    /// `meta.json` — used to let the *first* write land durably first:
    /// exactly the same shape of bug as the second-round hazard, just with
    /// which file survives and which is protected swapped. A real-world
    /// instance: `PUT …/logo` changes `record.manifest.company.logo_url`
    /// and calls `save`; a transient failure writing `meta.json` right after
    /// would report the request failed while the new logo was already on
    /// disk and would reappear on reload.
    ///
    /// The fix is not a fifth reorder — reordering again would just swap the
    /// hazard back. It durably stages both files (write + fsync to a temp
    /// name) *before* committing (renaming) either one, so a write failure —
    /// the likely failure mode a transient I/O error or a full disk actually
    /// produces — never touches a live file no matter which of the two temp
    /// writes fails or in what order they're attempted. This proves it:
    /// publish a bundle, change the manifest, arm the fault on `meta.json`'s
    /// write for the update `save`, and assert the manifest read back
    /// afterward is still the *old* value — never the new one — even though
    /// `meta.json` is committed second and untouched-on-disk is normally
    /// where a change would be expected to survive a same-call failure.
    #[tokio::test]
    async fn an_update_interrupted_on_the_second_write_does_not_persist_the_manifest_change() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |company_name: &str| {
            let mut manifest = sample_manifest();
            manifest.company.name = company_name.to_string();
            CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest,
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
            }
        };

        // Publish the bundle for the first time — the create-path branch,
        // unaffected by this test's assertion.
        store
            .save(&record("Acme"))
            .await
            .expect("first publish succeeds");

        // The update save this test exercises: a manifest-only change (the
        // shape of the logo endpoint's `PUT …/logo`), landing via a
        // `company.toml`-first commit order. Under the pre-fix code the
        // fault below fires on the *second* write, after `company.toml`
        // (the new name) had already been written and committed — the
        // untested window the review flagged. Under the fix, both files are
        // staged before either is committed, so this same fault fails
        // before `company.toml` is ever published.
        fault_probe::fail_next_write(&bundle.meta_json());

        let err = store.save(&record("Acme Renamed")).await;
        assert!(
            err.is_err(),
            "the injected failure must propagate out of save"
        );

        let loaded = store
            .load(&id)
            .await
            .unwrap()
            .expect("the bundle still exists — only the update failed");
        assert_eq!(
            loaded.manifest.company.name, "Acme",
            "a failed meta.json write during an update must leave \
             company.toml — and the manifest fields it carries — exactly as \
             it was before the update; the caller was told the save \
             failed, so nothing about it may have taken effect"
        );

        // `fail_next_write` is one-shot, so the retry hits the real write
        // path and the manifest change lands normally.
        store
            .save(&record("Acme Renamed"))
            .await
            .expect("retry succeeds once the fault is no longer armed");
        assert_eq!(
            store
                .load(&id)
                .await
                .unwrap()
                .unwrap()
                .manifest
                .company
                .name,
            "Acme Renamed"
        );
    }

    /// **Issue #1828 review, fifth round**: the structural fix above (fourth
    /// round) stages both files — write + fsync to a temp name — before
    /// committing either, so a write failure never touches a live file. It
    /// says nothing about the temp file that staging *did* manage to write
    /// before the failure. If staging `meta.json` succeeds and staging
    /// `company.toml` then fails, the old code let `?` return straight out
    /// of `save`, leaving the fully written, fsynced `meta.tmp-*` file
    /// behind — nothing ever renames it into place and nothing ever deletes
    /// it. On the disk-full failure this matters most for, that is doubly
    /// bad: the leaked temp file's bytes are exactly what the disk is
    /// already short of, so every retry into a persistent fault stages
    /// another orphan and shrinks the room available to recover in, rather
    /// than the retry loop converging on either success or a clean failure.
    ///
    /// This proves it: arm the write fault on `company.toml` for a
    /// first-time publish — the same fault as the very first round's test
    /// above, which already proves the *live* files stay safe — and, in
    /// addition to that, assert no `*.tmp-*` file is left anywhere under the
    /// **Issue #1828 review, eleventh round**: `commit_staged`'s
    /// `spawn_blocking` job keeps running after the caller's future is
    /// dropped, exactly like `stage_atomic_bytes`. So a save cancelled while
    /// the *second* commit was in flight had `StagedGuard::drop` delete
    /// `meta_tmp` out from under a rename that was still going to happen:
    /// the rename then failed `NotFound`, the manifest was already
    /// published, and cancellation skipped the rollback branch — leaving a
    /// new manifest paired with old metadata.
    ///
    /// Ownership of each temp now passes to `commit_staged` before the call,
    /// so the guard no longer races it. Drives the case with `stall_probe`
    /// on the metadata staging write and an abort while it is parked.
    #[tokio::test]
    async fn cancelling_a_save_does_not_delete_a_temp_a_commit_still_owns() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record_named = |name: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
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
        };

        store
            .save(&record_named("Before"))
            .await
            .expect("first publish");

        // Park the metadata staging write, then abort the save while it is
        // held there — the update path stages meta.json first.
        let release = stall_probe::arm(&bundle.meta_json());
        let after = record_named("After");
        let reader = FsCompanyStore::new(&root);
        let handle = tokio::spawn(async move { store.save(&after).await });
        stall_probe::wait_blocked().await;
        handle.abort();
        let joined = handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the save task must actually have been cancelled for this test \
             to mean anything, got {joined:?}"
        );
        release.send(()).expect("stall gate still open");

        // Whatever the cancellation left behind, the bundle must never be a
        // new manifest paired with stale metadata, and must not accumulate
        // orphaned temps.
        let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let orphans = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count();
                if orphans == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "a cancelled save must not strand a staged temp in {}",
            bundle_dir.display()
        );

        // The record must still load — the whole hazard is a manifest whose
        // paired metadata never landed.
        let loaded = reader.load(&id).await.expect("load must not error");
        assert!(
            loaded.is_some(),
            "the bundle must remain loadable after a cancelled update"
        );
    }

    /// **Issue #1828 review, tenth round**: `commit_staged` returned a plain
    /// `Err` for two very different states — the rename never happened, or
    /// the rename landed and only `sync_parent_dir` failed. The rollback
    /// added in the ninth round compensated identically for both, so a
    /// post-rename sync failure on `meta.json` restored the *old* manifest
    /// while readers already saw the *new* metadata: precisely the mixed
    /// record the rollback exists to prevent.
    ///
    /// `fault_probe::fail_next_dir_sync` reaches that state, which
    /// `fail_next_commit` cannot — it fires before the rename. The manifest
    /// must stay as the save left it, not be rolled back.
    #[tokio::test]
    async fn a_post_rename_sync_failure_does_not_roll_the_manifest_back() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record_named = |name: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
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
        };

        store
            .save(&record_named("Before"))
            .await
            .expect("first publish");

        // Fail only the durability step of the *second* commit, after its
        // rename has already replaced meta.json.
        fault_probe::fail_next_dir_sync(&bundle.meta_json());
        let err = store.save(&record_named("After")).await;
        assert!(
            err.is_err(),
            "a post-rename sync failure must still be reported to the caller"
        );

        let manifest = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
        assert!(
            manifest.contains("After") && !manifest.contains("Before"),
            "meta.json was already published, so the manifest must NOT be \
             rolled back onto it — found {manifest}"
        );
    }

    /// **Issue #1828 review, twelfth round follow-up** (finding on
    /// 3878400724): a post-rename directory-sync failure on the *first*
    /// commit (`company.toml`, update path) used to be treated exactly like
    /// a total failure — the staged `meta.json` was discarded and never
    /// even attempted, even though the manifest rename had already landed.
    /// Readers then saw the new manifest paired with the old metadata: the
    /// same mixed record the *second* commit's `published` branch (tenth
    /// round, the test above) already guards against, just on the other
    /// commit.
    ///
    /// `fault_probe::fail_next_dir_sync` reaches the "rename landed, sync
    /// alone failed" state on the *first* commit specifically. The metadata
    /// must still land.
    #[tokio::test]
    async fn a_first_commit_sync_failure_still_lands_the_paired_metadata() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |name: &str, lifecycle: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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
        };

        store
            .save(&record("Before", "running"))
            .await
            .expect("first publish");

        // Fail only the durability step of the *first* commit — the rename
        // that replaces company.toml has already landed by the time this
        // fires.
        fault_probe::fail_next_dir_sync(&bundle.company_toml());
        let err = store.save(&record("After", "paused")).await;
        assert!(
            err.is_err(),
            "a post-rename sync failure must still be reported to the caller"
        );

        let manifest = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
        assert!(
            manifest.contains("After") && !manifest.contains("Before"),
            "the manifest rename already landed and must not be rolled back — found {manifest}"
        );

        let meta = std::fs::read_to_string(bundle.meta_json()).expect("meta.json on disk");
        assert!(
            meta.contains("paused"),
            "the metadata write must not be abandoned just because the manifest's own \
             directory sync failed after its rename already landed — found {meta}"
        );
    }

    /// **Issue #1828 review, twelfth round follow-up** (finding on
    /// 3878400729): before this round, `guard` still watched `meta_tmp`
    /// while the *first* commit (`company.toml`, update path) was in
    /// flight. `commit_staged`'s rename is deliberately uncancellable
    /// (sixth/eleventh rounds), so cancelling `save` there dropped `guard`
    /// synchronously — reclaiming `meta_tmp` — while the detached rename
    /// could still land moments later, publishing the new manifest against
    /// metadata that never got a chance to commit.
    ///
    /// `stall_probe::arm_commit` parks `commit_staged`'s blocking closure
    /// just before the rename — so it is genuinely about to run, not merely
    /// staged — giving the test a deterministic window to abort while it is
    /// parked, then release it. The manifest and metadata must land
    /// together or not at all.
    #[tokio::test]
    async fn cancelling_a_save_during_the_first_commit_does_not_orphan_the_second() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |name: &str, lifecycle: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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
        };

        store
            .save(&record("Before", "running"))
            .await
            .expect("first publish");

        // The update path commits company.toml first — park that rename in
        // flight, then abort the save while it is held there.
        let release = stall_probe::arm_commit(&bundle.company_toml());
        let after = record("After", "paused");
        let reader = FsCompanyStore::new(&root);
        let handle = tokio::spawn(async move { store.save(&after).await });
        stall_probe::wait_blocked_commit().await;
        handle.abort();
        let joined = handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the save task must actually have been cancelled for this test \
             to mean anything, got {joined:?}"
        );
        release.send(()).expect("stall gate still open");

        // The detached commit unit keeps running after cancellation — give
        // it a moment to finish landing (or fully bailing on) both files.
        let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let orphans = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count();
                if orphans == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "a cancelled save must not strand a staged temp in {}",
            bundle_dir.display()
        );

        let loaded = reader
            .load(&id)
            .await
            .expect("load must not error")
            .expect("the bundle must remain loadable after a cancelled update");
        let manifest_updated = loaded.manifest.company.name == "After";
        let meta_updated = loaded.lifecycle == "paused";
        assert_eq!(
            manifest_updated, meta_updated,
            "the manifest and metadata must land together or not at all after a \
             cancelled save (manifest updated = {manifest_updated}, metadata updated \
             = {meta_updated})"
        );
    }

    /// **Issue #1828 review, finding on 3878896036**: `company_write_lock` —
    /// the per-company serialization every load-mutate-save caller relies on
    /// — lives on the *caller's* frame, not on the detached commit task the
    /// twelfth round introduced. Cancel a caller while it awaits `commit_rx`
    /// and that guard drops immediately even though the commit it was
    /// guarding is still renaming files in the background; nothing used to
    /// stop a fresh caller from acquiring the now-free lock and starting its
    /// own save for the same bundle while the orphaned commit was still in
    /// flight.
    ///
    /// Reuses `stall_probe::arm_commit` to park the cancelled call's first
    /// rename in flight, aborts its caller, then proves a concurrent second
    /// save for the same bundle cannot complete while that orphaned commit
    /// is still parked — and that once it is released, the second save's
    /// content is what survives, not a mix with (or an overwrite by) the
    /// cancelled one.
    #[tokio::test]
    async fn abort_then_concurrent_update_does_not_race_the_orphaned_commit() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |name: &str, lifecycle: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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
        };

        store
            .save(&record("Before", "running"))
            .await
            .expect("first publish");

        // Park the stale call's first commit (company.toml, update path) in
        // flight, then abort its caller while it is held there — the same
        // setup as the sibling test above, but this time a second, live
        // caller shows up while the first is still orphaned mid-commit.
        let release = stall_probe::arm_commit(&bundle.company_toml());
        let stale = record("Stale", "paused");
        let stale_store = FsCompanyStore::new(&root);
        let stale_handle = tokio::spawn(async move { stale_store.save(&stale).await });
        stall_probe::wait_blocked_commit().await;
        stale_handle.abort();
        let joined = stale_handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the stale save's caller must actually have been cancelled for this \
             test to mean anything, got {joined:?}"
        );

        // The stale call's detached commit is still parked on the rename it
        // acquired `commit_lock` for. A fresh caller's own save must not be
        // able to start committing until that lock is released, even though
        // `company_write_lock` (held by whatever calls `save` in production)
        // was already dropped when the stale caller was aborted above.
        let fresh_store = FsCompanyStore::new(&root);
        let fresh = record("Fresh", "archived");
        let mut fresh_handle = tokio::spawn(async move { fresh_store.save(&fresh).await });
        let raced_ahead =
            tokio::time::timeout(std::time::Duration::from_millis(300), &mut fresh_handle).await;
        assert!(
            raced_ahead.is_err(),
            "a concurrent save must not complete while an orphaned commit for \
             the same bundle is still in flight — got {raced_ahead:?}"
        );

        // Release the stale commit and let it settle, then the fresh save
        // must be free to finish.
        release.send(()).expect("stall gate still open");
        let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let orphans = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count();
                if orphans == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "the stale, orphaned commit must not strand a staged temp in {}",
            bundle_dir.display()
        );

        let joined_fresh = tokio::time::timeout(std::time::Duration::from_secs(5), fresh_handle)
            .await
            .expect("the fresh save must complete once the orphaned commit clears")
            .expect("the fresh save's task must not panic");
        joined_fresh.expect("the fresh save must succeed");

        let reader = FsCompanyStore::new(&root);
        let loaded = reader
            .load(&id)
            .await
            .expect("load must not error")
            .expect("the bundle must remain loadable");
        assert_eq!(
            loaded.manifest.company.name, "Fresh",
            "the live, awaited save must win — not the cancelled one whose \
             detached commit was merely still in flight"
        );
        assert_eq!(
            loaded.lifecycle, "archived",
            "the live, awaited save must win — not the cancelled one whose \
             detached commit was merely still in flight"
        );
    }

    /// **Issue #1828 review, finding on comment 3879048530**: `commit_lock`
    /// (above) only serializes the *commit* phase — it says nothing about
    /// `load`, which reads `company.toml`/`meta.json` straight off disk with
    /// no lock at all. A fresh caller's `company_write_lock` is freed the
    /// instant a cancelled caller's frame drops, so a fresh load-mutate-save
    /// cycle (every real call site — `policy.rs`, `team.rs`, …) can call
    /// `load` immediately, while the previous caller's orphaned commit is
    /// still parked mid-rename, and read the pre-commit record. That fresh
    /// caller's own `save` then blocks on `commit_lock` until the orphaned
    /// commit finishes, so the two commits never interleave — but the fresh
    /// caller already merged its change onto stale data, so its save durably
    /// *reverts* the orphaned commit's already-landed change the instant it
    /// finally lands. This is a lost update, not a file-corruption race, so
    /// `commit_lock` alone cannot see it: both commits still succeed, in the
    /// correct order, on well-formed files.
    ///
    /// Reuses `stall_probe::arm_commit` exactly as the sibling test above to
    /// park an aborted caller's commit mid-rename, but this time drives a
    /// real `load()` → mutate one field → `save()` cycle for the "fresh"
    /// caller — the production shape the prior round's test skipped by
    /// constructing its second `CompanyRecord` directly — so a `load` that
    /// races ahead of the still-parked commit is actually exercised.
    #[tokio::test]
    async fn a_racing_load_does_not_lose_an_orphaned_commits_update() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |name: &str, lifecycle: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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
        };

        store
            .save(&record("Before", "running"))
            .await
            .expect("first publish");

        // Park the stale call's first commit (company.toml, update path) in
        // flight, exactly as the sibling test above, then abort its caller
        // while it is held there.
        let release = stall_probe::arm_commit(&bundle.company_toml());
        let stale = record("Before", "paused");
        let stale_store = FsCompanyStore::new(&root);
        let stale_handle = tokio::spawn(async move { stale_store.save(&stale).await });
        stall_probe::wait_blocked_commit().await;
        stale_handle.abort();
        let joined = stale_handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the stale save's caller must actually have been cancelled for this \
             test to mean anything, got {joined:?}"
        );

        // The stale call's detached commit is still parked mid-rename,
        // holding `commit_lock`, with `lifecycle: "paused"` not yet on disk.
        // A fresh caller now runs the real production sequence: `load`,
        // touch one unrelated field, `save` back the merged record — the
        // same shape `policy.rs`/`team.rs` use. Spawned rather than awaited
        // inline so the still-parked commit above cannot block this task
        // from being scheduled at all.
        let fresh_store = FsCompanyStore::new(&root);
        let fresh_id = id.clone();
        let mut fresh_handle = tokio::spawn(async move {
            let mut loaded = fresh_store
                .load(&fresh_id)
                .await
                .expect("load must not error")
                .expect("bundle must exist");
            loaded.manifest.company.name = "Fresh".to_string();
            // Deliberately NOT touching `lifecycle` — mirrors a real caller
            // that only mutates the field it owns and carries the rest of
            // the loaded record through untouched.
            fresh_store.save(&loaded).await
        });

        // Give the fresh task a real window to run while the orphaned commit
        // is still parked. If `load` is unguarded, it finishes almost
        // immediately (well inside this window) and reads the pre-commit
        // "running" lifecycle; if `load` waits on the same lock the commit
        // holds, this whole task is still blocked when the window ends.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Release the stale commit and let it land.
        release.send(()).expect("stall gate still open");
        let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let orphans = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count();
                if orphans == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "the stale, orphaned commit must not strand a staged temp in {}",
            bundle_dir.display()
        );

        let joined_fresh =
            tokio::time::timeout(std::time::Duration::from_secs(5), &mut fresh_handle)
                .await
                .expect("the fresh save must complete once the orphaned commit clears")
                .expect("the fresh save's task must not panic");
        joined_fresh.expect("the fresh save must succeed");

        let reader = FsCompanyStore::new(&root);
        let loaded = reader
            .load(&id)
            .await
            .expect("load must not error")
            .expect("the bundle must remain loadable");
        assert_eq!(
            loaded.manifest.company.name, "Fresh",
            "the fresh caller's own change must land"
        );
        assert_eq!(
            loaded.lifecycle, "paused",
            "the orphaned commit's `lifecycle: \"paused\"` update landed on disk \
             before the fresh save committed, so a fresh save that carries \
             forward whatever it loaded must not silently revert it back to \
             \"running\" — that is a lost update, even though both commits \
             individually succeeded on well-formed files"
        );
    }

    /// **Issue #1828 review, ninth round**: on the update path the two files
    /// are published by two independent renames, `company.toml` first. If the
    /// `meta.json` commit then fails, the manifest edit is already durable
    /// while `save` returns `Err` — so a logo or name change persists even
    /// though the caller was told the save failed, and a call that changed
    /// both files leaves a mixed record.
    ///
    /// Drives it with `fault_probe::fail_next_commit`, which fails the
    /// rename step rather than the staging write, and asserts the manifest
    /// on disk is the one from before the failed save.
    #[tokio::test]
    async fn a_failed_meta_commit_rolls_the_published_manifest_back() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record_named = |name: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: {
                let mut m = sample_manifest();
                m.company.name = name.to_string();
                m
            },
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
        };

        // First publish, so the next save takes the update path.
        store
            .save(&record_named("Before"))
            .await
            .expect("first publish");
        let published = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
        assert!(
            published.contains("Before"),
            "precondition: the first save must have published the original name"
        );

        // Now fail only the *second* commit of the update.
        fault_probe::fail_next_commit(&bundle.meta_json());
        let err = store.save(&record_named("After")).await;
        assert!(
            err.is_err(),
            "the injected meta.json commit failure must propagate out of save"
        );

        let after = std::fs::read_to_string(bundle.company_toml()).expect("manifest on disk");
        assert!(
            after.contains("Before") && !after.contains("After"),
            "a save that reported failure must not leave its manifest edit \
             published — found {after}"
        );

        let orphans = std::fs::read_dir(bundle.company_toml().parent().unwrap())
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(
            orphans, 0,
            "the rollback must not leave its own temp behind"
        );
    }

    /// **Issue #1828 review, eighth round**: the guard closed the "dropped
    /// mid-stage" hole, but its first version disarmed *before* awaiting the
    /// error path's `remove_staged`. Aborting the task inside that await
    /// therefore reopened the very hole the guard exists to close: the
    /// cleanup never finished, and `Drop` no longer had `meta_tmp` to
    /// reclaim.
    ///
    /// Fault plus cancellation, as the review asked for: fail the second
    /// stage so the error path runs, park `remove_staged` with
    /// `cleanup_probe`, then abort. Pre-fix the `meta.tmp-<id>` survives;
    /// post-fix the still-armed guard reclaims it as the frame unwinds.
    #[tokio::test]
    async fn aborting_during_the_error_path_cleanup_still_reclaims_the_staged_temp_file() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: "provisioning".to_string(),
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
        };

        let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&bundle_dir).expect("bundle dir");

        // Fail the *second* stage so `save` takes its error path, and park
        // the cleanup that path awaits.
        fault_probe::fail_next_write(&bundle.company_toml());
        cleanup_probe::arm(&bundle_dir);

        let handle = tokio::spawn(async move { store.save(&record).await });

        cleanup_probe::wait_blocked().await;
        handle.abort();
        let joined = handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the save task must actually have been cancelled inside the \
             cleanup await for this test to mean anything, got {joined:?}"
        );

        let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let orphans = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count();
                if orphans == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;

        assert!(
            cleaned_up.is_ok(),
            "aborting inside the error path's remove_staged must still \
             reclaim the staged temp — an orphan sat in {} for the whole \
             timeout",
            bundle_dir.display()
        );
    }

    /// **Issue #1828 review, seventh round**: the sibling test below covers a
    /// second-stage write that *fails*. This covers the second-stage write
    /// that never returns at all because the caller went away — an aborted
    /// task, or an axum handler cancelled by a client disconnect, which is a
    /// reachable path since `save` is called from the operator routes.
    ///
    /// `save` stages `meta.json`, then awaits the staging of `company.toml`.
    /// Dropped in that window, none of its explicit `remove_staged` branches
    /// runs, and the already-fsynced `meta.tmp-<id>` loses its only handle.
    /// The second stage cleans up after itself (sixth round), so pre-fix the
    /// bundle is left with exactly one orphan; post-fix `StagedGuard`'s
    /// `Drop` reclaims it as the frame unwinds.
    #[tokio::test]
    async fn dropping_save_between_its_two_stages_does_not_strand_the_first_temp_file() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: "provisioning".to_string(),
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
        };

        // Park the *second* stage, so the abort below lands squarely in the
        // window where `meta.json` is staged and `company.toml` is not.
        let release = stall_probe::arm(&bundle.company_toml());

        let handle = tokio::spawn(async move { store.save(&record).await });

        stall_probe::wait_blocked().await;
        handle.abort();
        let joined = handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the task running save must actually have been cancelled for \
             this test to mean anything, got {joined:?}"
        );

        // Let the parked second stage finish; it reclaims its own temp.
        release.send(()).expect("stall gate still open");

        let bundle_dir = bundle.company_toml().parent().unwrap().to_path_buf();
        let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let orphans = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count();
                if orphans == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;

        assert!(
            cleaned_up.is_ok(),
            "dropping save between its two stages must not strand the \
             meta.json temp it had already staged — an orphan sat in {} for \
             the whole timeout",
            bundle_dir.display()
        );
    }

    /// bundle directory afterward. Pre-fix, `meta.tmp-<id>` survives the
    /// failed `save` call; post-fix, `save`'s error path removes it before
    /// returning.
    #[tokio::test]
    async fn a_failed_second_stage_write_does_not_strand_the_first_staged_temp_file() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = || CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: "provisioning".to_string(),
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
        };

        fn staged_tmp_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.contains(".tmp-"))
                })
                .collect()
        }

        // Staging order is fixed regardless of the commit-order branch:
        // `save` stages `meta.json` first, then `company.toml`. Arming the
        // fault on `company.toml`'s write fails the *second* stage call,
        // after `meta.json`'s temp file has already landed on disk.
        fault_probe::fail_next_write(&bundle.company_toml());

        let err = store.save(&record()).await;
        assert!(
            err.is_err(),
            "the injected staging failure must propagate out of save"
        );

        assert!(
            !tokio::fs::try_exists(&bundle.company_toml()).await.unwrap(),
            "company.toml must not exist after a staging failure"
        );
        assert!(
            !tokio::fs::try_exists(&bundle.meta_json()).await.unwrap(),
            "meta.json must not exist after a staging failure"
        );
        assert_eq!(
            staged_tmp_files(bundle.dir()),
            Vec::<std::path::PathBuf>::new(),
            "a failed second stage write must not strand the temp file the \
             first stage write already committed to disk — every retry into \
             a persistent fault (e.g. a full disk) would otherwise leak \
             another one"
        );

        // `fail_next_write` is one-shot, so the retry hits the real write
        // path and publishes normally, with no temp files left behind
        // either.
        store
            .save(&record())
            .await
            .expect("retry succeeds once the fault is no longer armed");
        assert!(store.load(&id).await.unwrap().is_some());
        assert_eq!(
            staged_tmp_files(bundle.dir()),
            Vec::<std::path::PathBuf>::new(),
            "a successful save must not leave any staged temp file behind \
             either"
        );
    }

    /// **Issue #1828 review, sixth round**: the fifth round's `remove_staged`
    /// calls only run on `save`'s own error path — code that never executes
    /// if `save`'s caller is cancelled before that path is reached.
    /// `spawn_blocking` cannot be cancelled: dropping its `JoinHandle` future
    /// stops nothing, it only discards the result. So if the future calling
    /// `stage_atomic_bytes` is itself dropped while parked on that await —
    /// exactly what happens when an axum handler is cancelled by a client
    /// disconnect mid-`save`, or a task is `abort()`ed — the write finishes
    /// on the blocking pool regardless, but the only reference to the temp
    /// path it wrote is gone with the dropped future. Neither `commit_staged`
    /// nor `remove_staged` ever runs, and the fully written, fsynced temp
    /// file is orphaned for good.
    ///
    /// This proves it with `stall_probe`: park the write mid-flight, `abort`
    /// the task awaiting `stage_atomic_bytes` (simulating the cancellation),
    /// then release the write and confirm it still lands on disk (proving
    /// cancellation didn't stop it — that's the hazard, not the fix). Pre-fix
    /// the temp file then sits there forever; post-fix the detached task
    /// notices nobody claimed the result and removes it itself.
    #[tokio::test]
    async fn cancelling_the_caller_does_not_strand_the_staged_temp_file() {
        let root_dir = tmp_root();
        let target = root_dir.path().join("bundle").join("company.toml");

        let release = stall_probe::arm(&target);

        let awaited_target = target.clone();
        let handle =
            tokio::spawn(async move { stage_atomic_bytes(&awaited_target, b"hello").await });

        // Deterministic rendezvous: the write closure has reached the gate
        // and parked, so aborting now is guaranteed to land on the await
        // this test is exercising, not before or after it.
        stall_probe::wait_blocked().await;

        handle.abort();
        let joined = handle.await;
        assert!(
            joined.as_ref().is_err_and(|e| e.is_cancelled()),
            "the task awaiting stage_atomic_bytes must actually have been \
             cancelled for this test to mean anything, got {joined:?}"
        );

        // Let the parked write proceed. Nothing above the blocking pool is
        // watching it anymore — this is the crux of the hazard: the write
        // was never cancellable, only the caller's ability to hear about it.
        release.send(()).expect("stall gate still open");

        // Poll for the temp file's fate instead of a fixed sleep: the
        // detached cleanup task needs a moment to resume after the blocking
        // write returns. Pre-fix there is nothing to wait for — the file
        // sits there for the lifetime of the test (and the process) — so
        // this loop only terminates via the timeout, which is the failing
        // signal on unpatched code.
        let bundle_dir = target.parent().unwrap().to_path_buf();
        let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let has_orphan = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
                if !has_orphan {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;

        assert!(
            cleaned_up.is_ok(),
            "cancelling the caller must not strand the temp file \
             stage_atomic_bytes already wrote and fsynced — it sat in {} \
             for the whole timeout",
            bundle_dir.display()
        );
    }

    /// **Issue #1828 review, seventeenth round** (finding on comment
    /// 3878696002): the sixth round's fix reclaims `tmp` when `tx.send`
    /// *fails* — i.e. when `rx` was already dropped before the send was
    /// attempted. It does nothing for the other order: `send` succeeds
    /// (the `Receiver` was still alive at that instant) but the future
    /// awaiting it is dropped before it is ever polled again to actually
    /// consume the value. `send` completing and the awaiting task resuming
    /// are two independently-scheduled events, so that gap is real — a
    /// successfully-sent, never-consumed oneshot value is silently dropped,
    /// and with it the only other reference to `tmp`.
    ///
    /// This can't be proven by aborting a spawned task and hoping the
    /// timing lines up, the way the sixth round's test does — that races
    /// two independently-scheduled tasks against wall-clock time. Instead,
    /// `send_probe` fires the instant `tx.send` returns `Ok`, and this test
    /// races that notification against the awaited future itself in a
    /// `biased` `select!`: whichever branch is checked first and found
    /// ready wins outright within a single poll, so once the notification
    /// fires, the awaited future is provably *not yet re-polled* to
    /// retrieve the value — dropping it right there reproduces "sent
    /// successfully, never consumed" deterministically, no sleeps involved.
    ///
    /// Pre-fix (before the local `StagedGuard` in `stage_atomic_bytes`) the
    /// temp file is orphaned forever: the detached task saw `send` succeed
    /// and did no cleanup, and the caller never got `tmp` back to hand to
    /// `commit_staged` / `remove_staged` / `save`'s own guard. Post-fix the
    /// local guard's `Drop` reclaims it as soon as this test's `drop(fut)`
    /// runs.
    #[tokio::test]
    async fn dropping_the_caller_after_a_successful_send_does_not_strand_the_temp_file() {
        let root_dir = tmp_root();
        let target = root_dir.path().join("bundle").join("company.toml");

        let notify = send_probe::arm(&target);

        // `Box::pin`, not `tokio::pin!`: the latter shadows `fut` with a
        // `Pin<&mut F>` into a hidden stack slot that outlives this
        // function's scope, so a later `drop(fut)` would only drop that
        // reference — the real future (and the `StagedGuard` living inside
        // it) would stay alive, silently defeating this whole test. `Box`
        // makes `fut` the actual owner, so dropping it drops the future for
        // real.
        let mut fut = Box::pin(stage_atomic_bytes(&target, b"hello"));

        tokio::select! {
            biased;
            _ = notify.notified() => {
                // The detached task's `tx.send` has already returned `Ok`
                // — `rx` was alive at that instant, and the value is now
                // buffered inside it. `biased` guarantees `&mut fut` was
                // NOT polled again in the same event that delivered this
                // notification (this arm is checked first and wins
                // outright), so `fut` is still parked on `rx.await`,
                // having never retrieved that buffered value. Falling
                // through to `drop(fut)` below discards it unconsumed —
                // exactly the race this test targets.
            }
            _ = &mut fut => {
                panic!(
                    "stage_atomic_bytes resolved before send_probe's \
                     notification fired — the race window this test \
                     targets (send succeeds, caller never consumes it) \
                     was never actually exercised"
                );
            }
        }
        drop(fut);

        let bundle_dir = target.parent().unwrap().to_path_buf();
        let cleaned_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let has_orphan = std::fs::read_dir(&bundle_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
                if !has_orphan {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;

        assert!(
            cleaned_up.is_ok(),
            "dropping the awaiting future after a successful but \
             never-consumed send must not strand the temp file \
             stage_atomic_bytes already wrote and fsynced — it sat in {} \
             for the whole timeout",
            bundle_dir.display()
        );
    }

    /// **Issue #1828 review, seventh round**: the sixth round's cleanup only
    /// fires when `tx.send` fails, i.e. when the caller is gone. It does
    /// nothing for the other way `result` can be `Err`: `File::create`
    /// succeeds (the temp file now exists on disk) and `write_all` or
    /// `sync_data` then fails. The caller is still there and gets the
    /// `Err`, but never gets `tmp` back, so nothing downstream can call
    /// `remove_staged` for it either — the fully-created, partially-written
    /// temp file is orphaned even though nothing was cancelled.
    ///
    /// `fault_probe::fail_next_mid_write` proves it: unlike
    /// `fail_next_write`, which fails before any filesystem call, this lets
    /// `File::create` succeed and fails right after, so a `.tmp-*` file is
    /// on disk when the injected failure hits. Pre-fix that file survives
    /// the failed call; post-fix `stage_atomic_bytes` removes it itself
    /// before returning `Err`.
    #[tokio::test]
    async fn a_write_failure_after_create_does_not_strand_the_temp_file() {
        let root_dir = tmp_root();
        let target = root_dir.path().join("bundle").join("company.toml");

        fault_probe::fail_next_mid_write(&target);

        let err = stage_atomic_bytes(&target, b"hello").await;
        assert!(
            err.is_err(),
            "the injected mid-write failure must propagate out of stage_atomic_bytes"
        );

        let bundle_dir = target.parent().unwrap();
        let has_orphan = std::fs::read_dir(bundle_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
        assert!(
            !has_orphan,
            "a write failure after File::create succeeded must not strand \
             the temp file it already created in {}",
            bundle_dir.display()
        );
    }

    /// **Issue #1828 review, third round**: `save` picks its write order by
    /// probing `try_exists(company.toml)` and treating a failed probe the
    /// same as `Ok(false)`. For an *update* — the bundle already exists —
    /// that misclassification steers the save onto the first-publish branch,
    /// which writes `meta.json` (the new lifecycle) *first*. If the
    /// subsequent `company.toml` write then also fails, or even if it
    /// doesn't, the probe failure alone means the safety property the
    /// second-round fix established (an update's first write is
    /// `company.toml`, so a failure never lands the lifecycle change) no
    /// longer holds — the wrong branch was taken before either file write
    /// was attempted.
    ///
    /// This proves the probe failure itself is propagated as an error from
    /// `save`, rather than silently steering the branch choice: publish a
    /// bundle, arm a fault on the *existence check* (not a write) for the
    /// next update save, and assert `save` returns `Err` — never that it
    /// silently took the first-publish branch and left `meta.json`
    /// rewritten with the new lifecycle. A retry without the fault must
    /// still land the update normally.
    #[tokio::test]
    async fn a_failed_existence_probe_during_an_update_does_not_misfire_the_first_publish_order() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        let bundle = Bundle::new(root.clone(), &id);

        let record = |lifecycle: &str| CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: sample_manifest(),
            ledger: Vec::new(),
            lifecycle: lifecycle.to_string(),
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
        };

        // Publish the bundle for the first time — unaffected by the fault
        // this test arms below.
        store
            .save(&record("provisioning"))
            .await
            .expect("first publish succeeds");

        // Simulate the existence probe itself failing (a transient I/O error
        // or an ACL denial), not either write. Under the pre-fix
        // `unwrap_or(false)`, this reads as "does not exist" and `save`
        // proceeds to write `meta.json` first — the first-publish order —
        // even though the bundle is live.
        fault_probe::fail_next_exists_check(&bundle.company_toml());

        let err = store.save(&record("running")).await;
        assert!(
            err.is_err(),
            "a failed existence probe must propagate out of save, not be \
             silently read as \"bundle does not exist\""
        );

        let loaded = store
            .load(&id)
            .await
            .unwrap()
            .expect("the bundle still exists — only the probe failed");
        assert_eq!(
            loaded.lifecycle, "provisioning",
            "a failed existence probe must not let save fall through to the \
             first-publish write order and rewrite meta.json's lifecycle \
             before company.toml is even considered"
        );

        // The fault is one-shot, so the retry hits the real probe and lands
        // the update normally.
        store
            .save(&record("running"))
            .await
            .expect("retry succeeds once the fault is no longer armed");
        assert_eq!(store.load(&id).await.unwrap().unwrap().lifecycle, "running");
    }

    #[tokio::test]
    async fn append_ledger_grows_without_rewrite() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: sample_manifest(),
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

        for i in 0..3 {
            store
                .append_ledger(
                    &id,
                    LedgerEntry {
                        at_millis: now_millis(),
                        kind: "inference.spend".to_string(),
                        amount_usd: i as f64,
                        memo: format!("entry {i}"),
                    },
                )
                .await
                .unwrap();
        }
        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.ledger.len(), 3);
        assert_eq!(loaded.ledger[2].memo, "entry 2");
    }

    /// A company with a damaged ledger still boots, and the damage is
    /// quarantined rather than deleted (issue #387).
    ///
    /// Before this, `read_jsonl` parsed inside its loop, so the first bad line
    /// returned `Err` from `FsCompanyStore::load`, the builder propagated it,
    /// and one torn accounting line made the company unbootable — with the
    /// console that would repair it sitting behind the boot it killed.
    ///
    /// Four lines, two of them damaged in the two ways a torn write actually
    /// produces: JSON that stops mid-value, and a byte sequence that is not
    /// UTF-8 at all. The second is the reason this reads bytes rather than a
    /// `String`: a whole-file decode would fail on that one byte and lose all
    /// four lines to damage confined to one.
    #[tokio::test]
    async fn a_damaged_ledger_line_is_skipped_and_left_on_disk() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let store = FsCompanyStore::new(&root);
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: sample_manifest(),
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

        // The memo text the report must never echo. Distinctive enough that a
        // substring search cannot pass by accident.
        const MEMO_TWO: &str = "acquire-northwind-holdings";
        const MEMO_THREE: &str = "settle-quarterly-invoice";

        let mut bytes: Vec<u8> = Vec::new();
        // 1. Intact.
        bytes.extend_from_slice(
            br#"{"at_millis":1,"kind":"inference.spend","amount_usd":1.0,"memo":"first entry"}"#,
        );
        bytes.push(b'\n');
        // 2. A torn write: the JSON stops in the middle of the memo string.
        bytes.extend_from_slice(
            format!(
                r#"{{"at_millis":2,"kind":"inference.spend","amount_usd":2.0,"memo":"{MEMO_TWO}"#
            )
            .as_bytes(),
        );
        bytes.push(b'\n');
        // 3. Invalid UTF-8 in a structural position, so the lossy U+FFFD lands
        //    where a key is expected and the line cannot parse. (Damage *inside*
        //    a string would decode to a valid — if mangled — record, which is
        //    the recoverable case and deliberately not skipped.)
        bytes.extend_from_slice(br#"{"at_millis":3,"#);
        bytes.push(0xFF);
        bytes.extend_from_slice(
            format!(r#""kind":"inference.spend","amount_usd":3.0,"memo":"{MEMO_THREE}"}}"#)
                .as_bytes(),
        );
        bytes.push(b'\n');
        // 4. Intact.
        bytes.extend_from_slice(
            br#"{"at_millis":4,"kind":"inference.spend","amount_usd":4.0,"memo":"fourth entry"}"#,
        );
        bytes.push(b'\n');

        let path = Bundle::new(root.clone(), &id).ledger_jsonl();
        tokio::fs::write(&path, &bytes).await.unwrap();

        // The company boots, carrying the entries that survived.
        let loaded = store
            .load(&id)
            .await
            .expect("a damaged ledger line must not fail the load")
            .expect("the company exists");
        assert_eq!(
            loaded.ledger.len(),
            2,
            "the two intact entries load; the two damaged ones are skipped"
        );
        assert_eq!(loaded.ledger[0].memo, "first entry");
        assert_eq!(loaded.ledger[1].memo, "fourth entry");

        // The report locates the damage without quoting it.
        let (entries, skipped) = read_jsonl_lenient::<LedgerEntry>(&path).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
            vec![2, 3],
            "the report names the 1-based line numbers of the damaged lines"
        );
        for entry in &skipped {
            assert!(
                entry.bytes > 0,
                "the report carries the on-disk line length"
            );
            assert!(
                !entry.message.is_empty(),
                "the report says what was rejected"
            );
            for memo in [MEMO_TWO, MEMO_THREE] {
                assert!(
                    !entry.message.contains(memo),
                    "a memo is free text and must never reach the report: {:?}",
                    entry.message
                );
            }
        }

        // Quarantine, not repair: the file is untouched, so an operator can
        // still recover the damaged lines by hand.
        let after = tokio::fs::read(&path).await.unwrap();
        assert_eq!(
            after, bytes,
            "loading must not rewrite the ledger — skipping a line the reader \
             could not parse must never become deleting it"
        );
    }

    #[tokio::test]
    async fn event_log_assigns_monotonic_seqs_and_resumes() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let log = FsEventLog::new(&root);
        let id = CompanyId::new("acme");

        let s0 = log
            .append(
                &id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "a".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .unwrap();
        let s1 = log
            .append(
                &id,
                CompanyEvent::OperatorMessage {
                    mentions: Vec::new(),
                    parent: None,
                    text: "b".into(),
                    by: None,
                    chat: None,
                    deliverable: None,
                    attachments: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(s0, EventSeq::new(0));
        assert_eq!(s1, EventSeq::new(1));

        let from_start = log.read_from(&id, EventSeq::new(0), 10).await.unwrap();
        assert_eq!(from_start.len(), 2);
        let from_one = log.read_from(&id, EventSeq::new(1), 10).await.unwrap();
        assert_eq!(from_one.len(), 1);
        assert_eq!(from_one[0].seq, EventSeq::new(1));
    }

    #[tokio::test]
    async fn event_log_subscribe_delivers_new_event() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let log = FsEventLog::new(&root);
        let id = CompanyId::new("acme");
        let mut stream = log.subscribe(&id);

        log.append(
            &id,
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();
        let received = stream.next().await.expect("event delivered");
        let EventStreamItem::Event(received) = received else {
            panic!("subscription unexpectedly reported a gap");
        };
        assert_eq!(
            received.event,
            CompanyEvent::OperatorMessage {
                mentions: Vec::new(),
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn memory_store_traces_tail_and_evict() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let mem = FsMemoryStore::new(&root);
        let id = CompanyId::new("acme");
        for i in 0..5 {
            mem.save_trace(&id, CompressedTrace::now(format!("c{i}"), format!("s{i}")))
                .await
                .unwrap();
        }
        let recent = mem.recent_traces(&id, 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].cycle_id, "c4");

        let removed = mem
            .evict(&id, EvictionPolicy::KeepRecent { n: 1 })
            .await
            .unwrap();
        assert_eq!(removed, 4);
        assert_eq!(mem.recent_traces(&id, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn context_store_put_peek_search() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let ctx = FsContextStore::new(&root);
        let id = CompanyId::new("acme");
        let addr = ctx
            .put(
                &id,
                ContextChunk {
                    label: "notes/intro".into(),
                    body: "the quick brown fox jumps".into(),
                },
            )
            .await
            .unwrap();

        let full = ctx.peek(&id, &addr, None).await.unwrap();
        assert_eq!(full, "the quick brown fox jumps");
        let ranged = ctx.peek(&id, &addr, Some(4..9)).await.unwrap();
        assert_eq!(ranged, "quick");

        let listed = ctx.list(&id, "notes/").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "notes/intro");

        let hits = ctx.search(&id, "brown", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("brown"));
    }

    #[tokio::test]
    async fn secret_store_isolates_companies() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let a = CompanyId::new("company-a");
        let b = CompanyId::new("company-b");

        secrets
            .set(&a, "github_token", SecretValue("ghp_secret".into()))
            .await
            .unwrap();
        assert_eq!(
            secrets.get(&a, "github_token").await.unwrap(),
            Some(SecretValue("ghp_secret".into()))
        );
        // Company B cannot see company A's secret.
        assert_eq!(secrets.get(&b, "github_token").await.unwrap(), None);
    }

    #[tokio::test]
    async fn secret_store_reads_legacy_file_and_keeps_it_after_rotation() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let company = CompanyId::new("company-a");
        let key = "mcp/acme prod/auth";
        let bundle = Bundle::new(root, &company);
        bundle.ensure_dirs().await.unwrap();
        let legacy_path = bundle.legacy_secret(key);
        tokio::fs::write(&legacy_path, "old-not-a-real-token")
            .await
            .unwrap();

        assert_eq!(
            secrets.get(&company, key).await.unwrap(),
            Some(SecretValue("old-not-a-real-token".into()))
        );

        secrets
            .set(
                &company,
                key,
                SecretValue("rotated-not-a-real-token".into()),
            )
            .await
            .unwrap();

        // The legacy file is kept for a non-empty rotation: a slug may be shared
        // by several keys, so it may still hold a colliding alias's value. The
        // canonical file shadows it for this key, so `get` returns the rotated
        // value.
        assert!(tokio::fs::metadata(&legacy_path).await.is_ok());
        assert_eq!(
            secrets.get(&company, key).await.unwrap(),
            Some(SecretValue("rotated-not-a-real-token".into()))
        );
    }

    #[tokio::test]
    async fn rotating_one_colliding_key_keeps_the_other_alias_readable() {
        // Issue #1510 migration hazard: two distinct keys can share one legacy
        // slug (`mcp/acme prod/auth` and `mcp/acme_prod/auth` both slug to
        // `mcp_acme_prod_auth`). Rotating one of them used to delete the shared
        // legacy file, so the other alias's next `get` fell through to `None`
        // even though it had been reading its own value before the upgrade.
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let company = CompanyId::new("company-a");
        let bundle = Bundle::new(root, &company);
        bundle.ensure_dirs().await.unwrap();

        // The shared file, exactly as a pre-injective install would have left
        // it: one value for both keys, last write wins.
        let key_a = "mcp/acme prod/auth";
        let key_b = "mcp/acme_prod/auth";
        let shared = bundle.legacy_secret(key_a);
        assert_eq!(shared, bundle.legacy_secret(key_b));
        tokio::fs::write(&shared, "token-for-underscore-name")
            .await
            .unwrap();

        // Rotate only A. B's value must survive in the kept legacy file.
        secrets
            .set(&company, key_a, SecretValue("rotated-token-a".into()))
            .await
            .unwrap();
        assert_eq!(
            secrets.get(&company, key_a).await.unwrap(),
            Some(SecretValue("rotated-token-a".into()))
        );
        assert_eq!(
            secrets.get(&company, key_b).await.unwrap(),
            Some(SecretValue("token-for-underscore-name".into()))
        );
    }

    #[tokio::test]
    async fn clearing_one_colliding_key_revokes_the_ambiguous_legacy_value() {
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let company = CompanyId::new("company-a");
        let bundle = Bundle::new(root, &company);
        bundle.ensure_dirs().await.unwrap();

        let key_a = "mcp/acme prod/auth";
        let key_b = "mcp/acme_prod/auth";
        let shared = bundle.legacy_secret(key_a);
        assert_eq!(shared, bundle.legacy_secret(key_b));
        tokio::fs::write(&shared, "legacy-token-must-not-return")
            .await
            .unwrap();

        // Clearing A must not leave the old shared credential available to B.
        secrets
            .set(&company, key_a, SecretValue(String::new()))
            .await
            .unwrap();
        assert!(!tokio::fs::try_exists(&shared).await.unwrap());
        assert_eq!(secrets.get(&company, key_b).await.unwrap(), None);
    }
    #[tokio::test]
    async fn canonical_namespace_does_not_bleed_into_legacy_fallback() {
        // Issue #1510's follow-up: `key-` was itself a valid legacy slug, so
        // the old canonical file for `foo` (`key-foo`) was returned when
        // reading `key-foo` through the legacy fallback, and writing `key-foo`
        // deleted `foo`. The `%` canonical prefix makes the two namespaces
        // disjoint.
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let company = CompanyId::new("company-a");

        secrets
            .set(&company, "foo", SecretValue("value-for-foo".into()))
            .await
            .unwrap();
        // `key-foo` was never set, and the legacy fallback must not reach the
        // canonical file of `foo`.
        assert_eq!(
            secrets.get(&company, "key-foo").await.unwrap(),
            None,
            "legacy fallback reached a canonical file of a different key"
        );

        // Writing `key-foo` must not disturb `foo`'s value.
        secrets
            .set(&company, "key-foo", SecretValue("value-for-key-foo".into()))
            .await
            .unwrap();
        assert_eq!(
            secrets.get(&company, "foo").await.unwrap(),
            Some(SecretValue("value-for-foo".into())),
            "writing `key-foo` deleted `foo`"
        );
        assert_eq!(
            secrets.get(&company, "key-foo").await.unwrap(),
            Some(SecretValue("value-for-key-foo".into()))
        );
    }

    #[tokio::test]
    async fn secret_set_succeeds_for_encoding_heavy_keys() {
        // A 20-emoji MCP server name used to exceed the filesystem component
        // limit once percent-encoded; the filename must stay bounded so `set`
        // does not fail with ENAMETOOLONG.
        let root_dir = tmp_root();
        let root = root_dir.path().to_path_buf();
        let secrets = FsSecretStore::new(&root);
        let company = CompanyId::new("company-a");
        let key = format!("mcp/{}/auth", "🎯".repeat(20));
        let value = SecretValue("not-a-real-token".into());

        secrets.set(&company, &key, value.clone()).await.unwrap();
        assert_eq!(secrets.get(&company, &key).await.unwrap(), Some(value));
    }
    /// The put/delete race the index lock exists for: a same-address write
    /// and delete interleaving as write-blob / delete-both / append-index
    /// would leave an index row whose blob is gone — list answers, peek
    /// fails. With the blob write under the lock, every surviving index row
    /// must have a readable blob, whichever order the race resolved.
    #[tokio::test]
    async fn concurrent_same_address_put_and_delete_stay_coherent() {
        use crate::ports::ContextStore;
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(FsContextStore::new(dir.path().to_path_buf()));
        let id = CompanyId::new("race-co");
        let chunk = || ContextChunk {
            label: "race/probe".into(),
            body: "identical body".into(),
        };
        let addr = store.put(&id, chunk()).await.unwrap();

        for _ in 0..20 {
            let s1 = store.clone();
            let s2 = store.clone();
            let id1 = id.clone();
            let id2 = id.clone();
            let a = addr.clone();
            let put = tokio::spawn(async move { s1.put(&id1, chunk()).await });
            let del = tokio::spawn(async move { s2.delete(&id2, &a).await });
            put.await.unwrap().unwrap();
            del.await.unwrap().unwrap();

            // Whatever interleaving happened: every listed row peeks.
            for meta in store.list(&id, "").await.unwrap() {
                store.peek(&id, &meta.addr, None).await.unwrap_or_else(|e| {
                    panic!("index row {} has no readable blob: {e}", meta.label)
                });
            }
            // Reset to a known present state for the next round.
            store.put(&id, chunk()).await.unwrap();
        }
    }
}
