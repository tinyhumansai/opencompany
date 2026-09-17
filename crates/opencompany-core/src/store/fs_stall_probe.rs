use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SendError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::Notify;

/// One armed stall, owned by the test that armed it. Carries both halves
/// of the rendezvous: the `Notify` the parked write signals when it
/// reaches the gate, and the sender that releases it again.
///
/// The `Notify` belongs to this gate rather than to the probe, because
/// the harness runs every test in this binary as a parallel thread: a
/// process-wide signal lets a sibling test's write, stalling on its own
/// path, wake this gate's waiter, which then aborts its save before the
/// save it meant to catch has reached anything.
pub(crate) struct Gate {
    release: Sender<()>,
    blocked: Arc<Notify>,
}

impl Gate {
    /// Waits until this gate's armed write has reached its stall point.
    /// `notify_one` stores its permit if called before this is polled,
    /// so there is no race between arming, spawning the write, and
    /// awaiting this.
    pub(crate) async fn wait(&self) {
        self.blocked.notified().await;
    }

    /// Releases the parked write. Errs once the write is no longer
    /// parked, which is how a test asserts the gate really held it.
    pub(crate) fn release(&self) -> Result<(), SendError<()>> {
        self.release.send(())
    }
}

type Armed = (Receiver<()>, Arc<Notify>);

static GATES: LazyLock<Mutex<HashMap<PathBuf, Armed>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn key(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn arm_in(gates: &Mutex<HashMap<PathBuf, Armed>>, path: &Path) -> Gate {
    let (release, receiver) = std::sync::mpsc::channel();
    let blocked = Arc::new(Notify::new());
    gates
        .lock()
        .expect("stall-probe poisoned")
        .insert(key(path), (receiver, Arc::clone(&blocked)));
    Gate { release, blocked }
}

fn block_in(gates: &Mutex<HashMap<PathBuf, Armed>>, path: &Path) {
    let armed = gates
        .lock()
        .expect("stall-probe poisoned")
        .remove(&key(path));
    if let Some((receiver, blocked)) = armed {
        blocked.notify_one();
        let _ = receiver.recv();
    }
}

/// Arms a one-shot stall for the next [`stage_atomic_bytes`] write
/// targeting `path`. Returns the gate the test waits on and releases.
pub(crate) fn arm(path: &Path) -> Gate {
    arm_in(&GATES, path)
}

/// Called from inside the blocking write closure. No-op unless `path`
/// was armed. Wakes that path's gate, then parks this blocking-pool
/// thread until the test releases it.
pub(crate) fn maybe_block(path: &Path) {
    block_in(&GATES, path);
}

static COMMIT_GATES: LazyLock<Mutex<HashMap<PathBuf, Armed>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Same idea as [`arm`]/[`maybe_block`] above, but for
/// [`commit_staged`]'s blocking closure instead of [`stage_atomic_bytes`]'s
/// (issue #1828 review, twelfth round follow-up). A separate gate set
/// because the two stall on the *same* destination path at different
/// points in the same `save` call — arming one must not be consumed by
/// the other.
pub(crate) fn arm_commit(path: &Path) -> Gate {
    arm_in(&COMMIT_GATES, path)
}

/// Called from inside `commit_staged`'s blocking closure, before the
/// rename. No-op unless `path` was armed. A gate armed here is reached
/// only when the rename is genuinely about to run, not merely staged.
pub(crate) fn maybe_block_commit(path: &Path) {
    block_in(&COMMIT_GATES, path);
}
