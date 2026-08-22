//! The module bus: a broker, a connection, and the loader — once per process.
//!
//! Ported from the shape openhuman proved out for its own module host
//! (`vendor/openhuman/src/openhuman/modules/host.rs`), because every property
//! that made that shape correct there holds here unchanged: tinybus's
//! `OnceBus` never hands out its broker, so modules run on a second private
//! broker; `dlopen` runs code before anything can inspect it, so admission
//! gates decide what is *admitted*, never what is *safe*; and tinybus never
//! unloads a library, so a failed module is failed until the process restarts
//! and there is nothing a shutdown path could reclaim.
//!
//! The broker lives on a dedicated process-lifetime tokio runtime on its own
//! named OS thread. That keeps a loaded module usable across short-lived
//! caller runtimes (notably `#[tokio::test]`) and keeps module lifetime out of
//! the hands of whichever application runtime happened to make the first call.
//!
//! # One host per process
//!
//! openhuman's own module runtime is gated on its `modules` feature, which
//! this crate deliberately does not forward (`openhuman_core` is taken with
//! `features = ["skills", "mcp", "hosting"]`), so no second broker can exist
//! in an opencompany process today. If that dependency line ever grows the
//! `modules` (or `documents`, which implies it) feature, the TinyMemory
//! module itself is the backstop: it refuses a second `claim_process_setup`
//! with `SETUP_FAILED` rather than serving two masters — a loud boot failure,
//! not a quiet split-brain.

use std::sync::OnceLock;

use tinybus::broker::Broker;
use tinybus::module::ModuleHost;
use tinybus::transport::memory::MemoryBus;
use tinybus::{Connection, Proxy};

/// The module bus, built once on first use.
static RUNTIME: OnceLock<ModuleRuntime> = OnceLock::new();

/// The broker, the host's own connection to it, and the module loader.
pub struct ModuleRuntime {
    /// The loader. Owns every admitted module for the process lifetime.
    host: ModuleHost,
    /// This process's client connection, used to call into loaded modules.
    connection: Connection,
    /// Handle for work that must run on this runtime rather than the
    /// caller's — module admission does blocking I/O and `dlopen`.
    handle: tokio::runtime::Handle,
}

impl ModuleRuntime {
    /// The loader.
    #[must_use]
    pub fn host(&self) -> &ModuleHost {
        &self.host
    }

    /// The connection modules are called over.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Run module admission work on this runtime's blocking pool.
    ///
    /// # Errors
    ///
    /// Returns the work's own error, or a message when the blocking task did
    /// not finish (the runtime is shutting down).
    pub async fn blocking<F>(&self, work: F) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        self.handle
            .spawn_blocking(work)
            .await
            .map_err(|error| format!("the module loader did not finish: {error}"))?
    }

    /// A proxy for one object on a loaded module.
    ///
    /// # Errors
    ///
    /// Returns an error if `bus_name` or `object_path` is not well formed,
    /// which for a name spelled from `tinymemory-bus` constants means the
    /// constants are wrong rather than the module.
    pub fn proxy(&self, bus_name: &str, object_path: &str) -> tinybus::Result<Proxy> {
        self.connection.proxy(bus_name, object_path, bus_name)
    }
}

/// The process-wide module runtime, standing it up on first use.
///
/// # Errors
///
/// Returns an error if the broker's in-memory transport cannot be connected,
/// which in practice means the tokio runtime is shutting down.
///
/// # Panics
///
/// Does not panic: a lost initialisation race reuses the winner's runtime.
pub async fn runtime() -> tinybus::Result<&'static ModuleRuntime> {
    if let Some(existing) = RUNTIME.get() {
        return Ok(existing);
    }

    static START: OnceLock<Result<(), String>> = OnceLock::new();
    let started = START.get_or_init(|| {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("opencompany-module-bus".to_string())
            .spawn(move || {
                let tokio_runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("opencompany-module-worker")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let result = tokio_runtime.block_on(build_runtime());
                match result {
                    Ok(runtime) => {
                        let _ = RUNTIME.set(runtime);
                        let _ = ready_tx.send(Ok(()));
                        tokio_runtime.block_on(std::future::pending::<()>());
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        ready_rx.recv().map_err(|error| error.to_string())?
    });
    started
        .as_ref()
        .map_err(|error| tinybus::Error::Transport(error.clone()))?;
    RUNTIME
        .get()
        .ok_or_else(|| tinybus::Error::Transport("module runtime did not start".to_string()))
}

async fn build_runtime() -> tinybus::Result<ModuleRuntime> {
    let transport = MemoryBus::new();
    let broker = Broker::new();
    // The broker task is deliberately not retained. It lives as long as the
    // process, and holding the handle would only offer an abort that must
    // never be called: a module whose transport disappears faults, and a
    // faulted module cannot be reloaded without a restart.
    broker.spawn(transport.clone());

    // Permissive admission — tinybus's default, chosen rather than inherited.
    //
    // Strict mode additionally refuses a module whose rustc version string
    // differs from the host's, and openhuman's host records that turning it
    // on refused the real published artifact outright: released artifacts are
    // built by CI on whatever toolchain that runner had, while this crate
    // pins its own, so mismatch is the normal case, and strict would mean the
    // feature never works in the field while every local build looks fine.
    // Everything protecting the address space is still enforced — ABI
    // revision, descriptor layout, target triple, pointer width, endianness,
    // feature bits, the refusal of a panic=abort module. Only the toolchain
    // string is relaxed, and it is reported rather than ignored.
    let host = ModuleHost::new(broker);
    let connection = Connection::connect(transport.connect().await?).await?;

    // Callback objects BEFORE anything can load: tinymemory installs its
    // embedder during module setup, before its store is constructed, so a
    // callback served after a load binds an inert provider silently.
    super::callbacks::install(&connection).await?;

    Ok(ModuleRuntime {
        host,
        connection,
        handle: tokio::runtime::Handle::current(),
    })
}

/// The runtime, when it has already been stood up — for callers already ON
/// the module runtime's own threads, where the async accessor cannot be
/// awaited.
#[must_use]
pub(super) fn try_runtime() -> Option<&'static ModuleRuntime> {
    RUNTIME.get()
}

/// Whether the module runtime has been stood up.
///
/// Lets status reporting answer without starting a broker as a side effect of
/// being asked a question.
#[must_use]
pub fn is_started() -> bool {
    RUNTIME.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::{is_started, runtime};

    /// The process-global runtime is stable within one caller runtime, and a
    /// proxy for an unloaded module fails rather than hangs — the property
    /// that makes an eager-load-at-boot design honest.
    #[tokio::test]
    async fn the_module_bus_is_a_singleton_and_serves_proxies() {
        let first = runtime().await.expect("runtime should start");
        assert!(is_started());
        let second = runtime().await.expect("runtime should be reused");
        assert!(
            std::ptr::eq(first, second),
            "runtime() handed out two different runtimes"
        );

        // Building a proxy is a local operation — it validates names and
        // nothing else. Nothing has claimed the name, so the call fails
        // rather than hanging.
        let proxy = first
            .proxy(
                "ai.tinyhumans.tinymemory.Memory",
                "/ai/tinyhumans/tinymemory/Memory",
            )
            .expect("bus names should be well formed");
        let result: tinybus::Result<serde_json::Value> = proxy.call("Health", ()).await;
        assert!(result.is_err(), "an unloaded module should not answer");

        // A name that cannot be a bus name is refused without reaching the
        // bus.
        assert!(first.proxy("not a bus name", "/nope").is_err());
    }
}
