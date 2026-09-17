use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::Notify;

static GATES: LazyLock<Mutex<HashMap<PathBuf, Arc<Notify>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn key(dir: &Path) -> PathBuf {
    std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// One armed stall, owned by the test that armed it. Its `Notify`
/// belongs to the gate rather than to the probe so that a sibling test
/// stalling a cleanup in its own directory cannot wake this waiter —
/// the harness runs every test in this binary as a parallel thread.
pub(crate) struct Gate {
    blocked: Arc<Notify>,
}

impl Gate {
    /// Waits until this gate's armed cleanup has reached its stall
    /// point. `notify_one` stores its permit if called before this is
    /// polled, so arming, spawning and awaiting cannot race.
    pub(crate) async fn wait(&self) {
        self.blocked.notified().await;
    }
}

/// Arms a one-shot stall for the next `remove_staged` of a temp file
/// living directly in `dir`.
pub(crate) fn arm(dir: &Path) -> Gate {
    let blocked = Arc::new(Notify::new());
    GATES
        .lock()
        .expect("cleanup-probe poisoned")
        .insert(key(dir), Arc::clone(&blocked));
    Gate { blocked }
}

/// No-op unless this temp's directory was armed. Wakes that directory's
/// gate, then parks forever — the test aborts the task rather than
/// releasing it, which is the scenario under test.
pub(crate) async fn maybe_block(tmp: &Path) {
    let armed = tmp.parent().and_then(|dir| {
        GATES
            .lock()
            .expect("cleanup-probe poisoned")
            .remove(&key(dir))
    });
    if let Some(blocked) = armed {
        blocked.notify_one();
        std::future::pending::<()>().await;
    }
}
