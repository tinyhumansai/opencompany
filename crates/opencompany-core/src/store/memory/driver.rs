//! Memory-engine selection: from `OPENCOMPANY_MEMORY*` to a built engine.
//!
//! The engines themselves, and the rule that a credentialed endpoint must be
//! `https` unless it is loopback, live upstream in `tinymemory::build_engine`.
//! What stays here is host policy: which env vars name what, which modes exist,
//! and the operator-facing refusals when a deployment still names an engine
//! TinyMemory v2 no longer ships.

use std::path::PathBuf;
use std::sync::Arc;

use tinymemory::{EngineCredential, EngineSettings, MemoryEngine, build_engine, list_engines};

use super::null::NullEngine;
use crate::Result;
use crate::error::OpenCompanyError;

/// Which kind of engine the selected mode binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryMode {
    /// A hosted engine behind a URL and a credential.
    Remote,
    /// The explicit "no memory" engine ([`NullEngine`]).
    Null,
}

/// Everything needed to open one engine, independent of where it came from.
#[derive(Clone)]
pub struct MemoryDriverConfig {
    /// The selected mode.
    pub mode: MemoryMode,
    /// The engine id for [`MemoryMode::Remote`] (`OPENCOMPANY_MEMORY_DRIVER`).
    pub driver_id: Option<String>,
    /// The engine endpoint (`OPENCOMPANY_MEMORY_URL`); engines with a default
    /// endpoint may leave it unset.
    pub url: Option<String>,
    /// The engine credential (`OPENCOMPANY_MEMORY_API_KEY`).
    pub api_key: Option<String>,
    /// Retained for the migration command's common configuration shape.
    pub data_dir: Option<PathBuf>,
}

impl std::fmt::Debug for MemoryDriverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDriverConfig")
            .field("mode", &self.mode)
            .field("driver_id", &self.driver_id)
            .field("url", &self.url.as_ref().map(|_| "<set>"))
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// A configuration refusal, surfaced as [`OpenCompanyError::Config`].
#[derive(Debug)]
pub struct MemoryDriverError(String);

impl std::fmt::Display for MemoryDriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<MemoryDriverError> for OpenCompanyError {
    fn from(error: MemoryDriverError) -> Self {
        Self::Config(error.0)
    }
}

/// Engine ids TinyMemory v1 shipped and v2 dropped. Named so a deployment that
/// still selects one is told why it stopped working, instead of reading
/// "unknown memory engine" as a typo.
const RETIRED_ENGINES: [&str; 3] = ["supermemory", "mem0", "cognee"];

/// The v1 alias for the CortexDB dialect. Both reached the same service, so the
/// alias keeps binding rather than refusing a working deployment.
const CORTEX_ALIAS: &str = "cortex";

/// The remote engine ids this build can bind, in catalog order.
pub fn supported_remote_engines() -> Vec<&'static str> {
    list_engines().into_iter().map(|engine| engine.id).collect()
}

/// Opens the configured engine.
pub fn open_driver(config: &MemoryDriverConfig) -> Result<Arc<dyn MemoryEngine>> {
    match config.mode {
        MemoryMode::Null => Ok(Arc::new(NullEngine::new())),
        MemoryMode::Remote => {
            let requested = require(
                config.driver_id.as_deref(),
                "OPENCOMPANY_MEMORY=remote requires OPENCOMPANY_MEMORY_DRIVER naming a hosted engine",
            )?;
            let id = canonical_engine_id(requested)?;
            let key = require(
                config.api_key.as_deref(),
                "OPENCOMPANY_MEMORY=remote requires OPENCOMPANY_MEMORY_API_KEY",
            )?;
            let settings = EngineSettings {
                endpoint: config
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                    .map(str::to_string),
            };
            build_engine(id, &settings, EngineCredential::Static(key.to_string())).map_err(
                |error| {
                    OpenCompanyError::Config(format!(
                        "could not open memory engine `{id}`: {error}. Check \
                         OPENCOMPANY_MEMORY_URL and OPENCOMPANY_MEMORY_API_KEY."
                    ))
                },
            )
        }
    }
}

/// Maps an operator-supplied engine id onto one `build_engine` knows, or a
/// refusal that names the cause.
pub fn canonical_engine_id(requested: &str) -> Result<&'static str> {
    let normalized = requested.trim().to_ascii_lowercase();
    let normalized = if normalized == CORTEX_ALIAS {
        "cortexdb".to_string()
    } else {
        normalized
    };
    if let Some(id) = supported_remote_engines()
        .into_iter()
        .find(|id| *id == normalized)
    {
        return Ok(id);
    }
    if RETIRED_ENGINES.contains(&normalized.as_str()) {
        return Err(MemoryDriverError(format!(
            "memory engine `{normalized}` is no longer supported: TinyMemory v2 ships only {}. \
             Migrate with `opencompany memory migrate` from a build that still has it, or select \
             one of those.",
            supported_remote_engines().join(" and ")
        ))
        .into());
    }
    Err(MemoryDriverError(format!(
        "unknown memory engine `{normalized}`; this build can bind {}",
        supported_remote_engines().join(", ")
    ))
    .into())
}

fn require<'a>(value: Option<&'a str>, refusal: &str) -> Result<&'a str> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MemoryDriverError(refusal.to_string()).into())
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
