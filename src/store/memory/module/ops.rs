//! Loading the TinyMemory module artifact, once per process.
//!
//! tinybus never unloads a library and every terminal module state is
//! terminal for the process, so this seam caches its outcome: a load that
//! failed once answers the same failure instantly rather than paying a
//! `dlopen` (or a filesystem walk) per call to reach the same error. Recovery
//! is always a process restart — which is why the boot path loads eagerly and
//! aborts with a named reason instead of letting a tenant discover the
//! failure a day later, one memory call at a time.
//!
//! The artifact is named by `OPENCOMPANY_MEMORY_MODULE_PATH`. Baked-image
//! resolution (platform buckets, digests, `modules.toml` beside the library)
//! rides the delivery commit; this seam deliberately takes exactly one path
//! and refuses when it is absent, because a driver that silently degrades to
//! a different engine is the failure mode #1524 spends a whole section on.

use std::path::{Path, PathBuf};

use tinymemory_api::host::MemoryConfig;

use super::host;

/// The env var naming the module artifact (`libtinymemory_module.so` /
/// `.dylib`). Set by the baked image; a developer points it at a local build.
pub const MODULE_PATH_ENV: &str = "OPENCOMPANY_MEMORY_MODULE_PATH";

/// The subdirectory of the data dir the module's store lives in.
///
/// Deliberately NOT `memory` — tinymemory-core lays out
/// `<workspace_dir>/memory/{namespaces/,vectors/,memory.db}` with the middle
/// segment hardcoded, and `<data_dir>/memory` is where the incumbent
/// EngineCortex overlay already writes. Sharing the directory would
/// interleave two schemas silently; [`module_workspace_dir`] refuses it.
pub const MODULE_STORE_SUBDIR: &str = "memory-module";

/// The load outcome, cached for the process. A tokio `OnceCell` rather than
/// a bare check-then-set: two concurrent FIRST calls must not both reach
/// artifact admission — the second `claim_process_setup` fails with
/// `SETUP_FAILED`, and whichever outcome lands last would cache a failure
/// beside a loaded module. The cell serialises the first load; every later
/// caller reads the one recorded outcome.
static LOADED: tokio::sync::OnceCell<Result<(), String>> = tokio::sync::OnceCell::const_new();

/// The workspace root handed to the module for `data_dir`.
///
/// # Errors
///
/// Refuses a `data_dir` whose module store would land inside the incumbent
/// engine's `memory/` directory.
pub fn module_workspace_dir(data_dir: &Path) -> Result<PathBuf, String> {
    // A data root that IS the incumbent engine's directory nests the module
    // store inside it: `data_dir=/volume/memory` puts the workspace at
    // `/volume/memory/memory-module`, inside the tree `UnifiedMemory` and
    // the EngineCortex overlay treat as theirs. Refuse the misconfiguration
    // by name rather than interleave two engines' trees.
    if data_dir.file_name().and_then(|name| name.to_str()) == Some("memory") {
        return Err(
            "the data root itself is a `memory` directory — the incumbent engine's own tree. \
             Point OPENCOMPANY_DATA_DIR at the instance data root, not at the engine store \
             inside it"
                .to_string(),
        );
    }
    let dir = data_dir.join(MODULE_STORE_SUBDIR);
    // Defence in depth: the constant makes this unreachable, but the property
    // is load-bearing enough (risk #2 in the issue's register) that a future
    // edit to the constant should trip a named refusal, not a data mix.
    if dir.file_name().and_then(|name| name.to_str()) == Some("memory") {
        return Err(
            "the module store may not live in `memory` — that is the incumbent engine's \
             directory, and interleaving the two schemas corrupts both"
                .to_string(),
        );
    }
    Ok(dir)
}

/// The typed load-time config crossing into the module.
///
/// Built through [`MemoryConfig`] rather than a raw `json!` block: the
/// module's own config struct defaults every missing key quietly, so a typo
/// here would degrade silently — round-tripping through the typed struct
/// makes an unknown or misspelled key a deserialization error instead.
///
/// # Errors
///
/// Returns an error when the typed round-trip fails, which means the override
/// block below disagrees with the contract's `MemoryConfig`.
fn load_config(workspace_dir: &Path) -> Result<serde_json::Value, String> {
    // Zero dimensions: the host's EmbeddingHost answers empty vectors
    // (`callbacks.rs`), keeping the module's recall on the lexical footing
    // the incumbent namespace driver documents. Gate Q2 owns revisiting this.
    let memory: MemoryConfig = serde_json::from_value(serde_json::json!({
        "embedding_dimensions": 0,
    }))
    .map_err(|error| format!("the module memory config did not typecheck: {error}"))?;
    Ok(serde_json::json!({
        "workspace_dir": workspace_dir,
        "memory": memory,
        "driver_id": "tinymemory",
    }))
}

/// Loads the module named by [`MODULE_PATH_ENV`], once.
///
/// # Errors
///
/// The env var being unset, the artifact failing admission, or any earlier
/// failure this process already cached. Every message is stable and names the
/// variable, because the boot path prints it as the abort reason.
pub async fn ensure_loaded(data_dir: &Path) -> Result<(), String> {
    LOADED
        .get_or_init(|| async { load(data_dir).await })
        .await
        .clone()
}

async fn load(data_dir: &Path) -> Result<(), String> {
    let path = std::env::var(MODULE_PATH_ENV)
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            format!(
                "the TinyMemory module driver needs {MODULE_PATH_ENV} to name the module \
                 artifact; refusing to bind rather than degrade to another engine"
            )
        })?;
    let workspace = module_workspace_dir(data_dir)?;
    let config = load_config(&workspace)?;

    let runtime = host::runtime()
        .await
        .map_err(|error| format!("the module bus is not running: {error}"))?;
    // Admission does blocking I/O and `dlopen`; it runs on the module
    // runtime's blocking pool, never an executor thread.
    runtime
        .blocking(move || {
            runtime_load(&path, config)?;
            Ok(())
        })
        .await
}

fn runtime_load(path: &Path, config: serde_json::Value) -> Result<(), String> {
    let runtime = host::try_runtime()
        .ok_or_else(|| "the module bus stopped before the load ran".to_string())?;
    runtime
        .host()
        .load_file_with_config(path, config)
        .map(|_| {
            tracing::info!(path = %path.display(), "[memory-module] module loaded");
        })
        .map_err(|error| {
            format!(
                "the TinyMemory module at {MODULE_PATH_ENV} was refused: {error}. This is \
                 terminal for the running process; fix the artifact and restart"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::module_workspace_dir;

    /// The module's store is beside — never inside — the incumbent engine's
    /// `memory/` directory (issue #1524 risk #2).
    #[test]
    fn the_module_store_is_disjoint_from_the_incumbent_engines() {
        let dir = module_workspace_dir(std::path::Path::new("/data")).expect("allowed");
        assert_eq!(dir, std::path::PathBuf::from("/data/memory-module"));
        assert_ne!(dir.file_name().unwrap(), "memory");
    }
}
