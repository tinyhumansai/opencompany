//! Driver selection: configuration in, a bound [`MemoryProvider`] out.
//!
//! # Class is decided here, not by the driver
//!
//! [`DriverClass`] comes from the host's configuration and is cross-checked
//! against the reserved id table before anything opens. The contract crate
//! excludes class on purpose — a driver that self-reported it could claim to be
//! embedded and skip the egress and trust checks the class gates — so this
//! module never asks a provider what it is.
//!
//! # How `embedded` goes through this seam — and what still does not
//!
//! [`MemoryMode::Embedded`] has two shapes, told apart by whether the operator
//! named a driver:
//!
//! - **No driver named** (`OPENCOMPANY_MEMORY=embedded` alone): the incumbent
//!   `EngineCortex` overlay, exactly as it has always been. It does not pass
//!   through this module at all — [`open_driver`] answers `Ok(None)` and the
//!   caller keeps the engine path. Today's companies have their data in those
//!   tables, and swapping the default out from under them would strand it.
//! - **`OPENCOMPANY_MEMORY_DRIVER=namespace`**: the contract's own durable
//!   store, bound *through* this seam. `tinymemory-core`'s `UnifiedMemory` is
//!   a per-deployment SQLite store that implements the contract's `Memory`
//!   trait (`core/src/store/memory_trait.rs`, reporting `name() ==
//!   "namespace"`); it is composed into a driver with
//!   `tinymemory::mandatory::MemoryTraitProvider`, admitted under the
//!   host-reserved `namespace` id, audited at bind time, and handed to the
//!   same tenant-namespace facades the hosted engines use. No network call;
//!   persists under `<data_dir>/memory-namespace/`.
//!
//! An earlier version of this note listed four obstacles to the second shape.
//! Three are resolved by the shape itself and one is a real cost, taken
//! knowingly — recorded here so the decisions stay visible:
//!
//! 1. **Composition.** `MemoryTraitProvider` advertises the mandatory three
//!    families and nothing else. That was an argument against *replacing*
//!    `EngineCortex` with it, and it still is; as a mode **beside** the
//!    incumbent it is simply the truth about this driver, and the bind-time
//!    audit holds because the advertisement is derived.
//! 2. **The id.** The registry reserves `null`, `tinycortex`, `supermemory`,
//!    `mem0` and `cognee`; `namespace` is not among them. The registry's own
//!    `with_reserved` exists for exactly this — "a host that bundles an
//!    adapter this crate does not know about" — so [`admit`] reserves it at
//!    [`DriverClass::Embedded`] host-side.
//! 3. **A different store.** True, and answered by being additive: the
//!    incumbent keeps `<data_dir>/memory/`, this driver keeps
//!    `<data_dir>/memory-namespace/`, and nothing migrates. An operator who
//!    switches modes starts that store empty, which the mode's own docs say
//!    out loud.
//! 4. **The dependency.** `tinymemory-core` pulls `tinycortex` (with
//!    `obsidian`, `persona`, `people`, `sync`) and `tinyagents/sqlite` — the
//!    bundled-SQLite weight the manifest keeps out of hosted-memory tenants
//!    (tinymemory#18 §D). This is the cost that cannot be reasoned away, so it
//!    is opt-in: the `tinymemory-embedded` feature, separate from `tinymemory`,
//!    which `remote` and `null` still serve alone.
//!
//! Recall honesty: this build injects no embedding backend
//! (`NoopEmbedding`), so `UnifiedMemory` stores every chunk vector-less and
//! recall runs on its graph and keyword tiers. That is the same degraded-mode
//! contract `EngineCortex` ships under, and it is announced loudly at bind —
//! never mistaken for semantic recall.
//!
use std::path::PathBuf;
use std::sync::Arc;

use tinymemory::registry::{
    COGNEE_DRIVER_ID, ConfigLabels, DriverClass, DriverEntry, DriverRegistry, MEM0_DRIVER_ID,
    NULL_DRIVER_ID, SUPERMEMORY_DRIVER_ID, TRUSTED,
};
use tinymemory_api::null::NullMemoryProvider;
use tinymemory_api::provider::MemoryProvider;

use crate::Result;
use crate::error::OpenCompanyError;

/// Which engine backs memory, as an operator selects it.
///
/// The wire values of `OPENCOMPANY_MEMORY`, minus the legacy spellings that
/// `crate::store::select::MemoryBackend` maps onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryMode {
    /// The engine runs in-pod against `OPENCOMPANY_DATA_DIR`. No network call,
    /// works with a read-only root filesystem.
    ///
    /// Two shapes: with no driver named, the incumbent `EngineCortex` overlay
    /// (this module answers `Ok(None)`); with
    /// `OPENCOMPANY_MEMORY_DRIVER=namespace`, the contract's durable
    /// `UnifiedMemory` store bound through this seam. See the module docs.
    Embedded,
    /// A hosted memory service behind a URL and a credential.
    Remote,
    /// Writes accepted and discarded, reads empty.
    ///
    /// `/dev/null` semantics, for a deployment that wants the ports wired and
    /// nothing retained. Never selected as a fallback when a configured driver
    /// fails to bind — a company that believes it is remembering and is not is
    /// the failure this whole surface exists to prevent.
    Null,
}

/// Everything needed to open a driver, already resolved from env + manifest.
///
/// Holds the credential, so it is not `Debug` — see the manual impl below.
#[derive(Clone)]
pub struct MemoryDriverConfig {
    /// The selected mode.
    pub mode: MemoryMode,
    /// The driver id (`supermemory`, `mem0`, `cognee`, `namespace`, `null`, …).
    ///
    /// `None` takes the mode's default: `null` for [`MemoryMode::Null`], the
    /// incumbent engine overlay for [`MemoryMode::Embedded`], and for
    /// [`MemoryMode::Remote`] there is no default — an unnamed remote engine is
    /// a refusal, because guessing which hosted service an operator meant is
    /// not a recoverable mistake.
    pub driver_id: Option<String>,
    /// Base URL of the hosted service. Required for [`MemoryMode::Remote`].
    pub url: Option<String>,
    /// The outbound credential. Required for [`MemoryMode::Remote`].
    pub api_key: Option<String>,
    /// The durable data root, for the in-pod contract driver
    /// ([`MemoryMode::Embedded`] naming `namespace`). The store opens under
    /// `<data_dir>/memory-namespace/` — deliberately beside, never inside, the
    /// incumbent engine's `<data_dir>/memory/`.
    pub data_dir: Option<PathBuf>,
}

impl std::fmt::Debug for MemoryDriverConfig {
    /// Renders the mode and driver id; never the URL, never the credential.
    ///
    /// The URL is withheld alongside the key rather than treated as harmless:
    /// a self-hosted memory endpoint is internal topology, and this type is
    /// reachable from boot logging and error paths where a bare `{:?}` is one
    /// keystroke away.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDriverConfig")
            .field("mode", &self.mode)
            .field("driver_id", &self.driver_id)
            .field("url", &self.url.as_ref().map(|_| "<set>"))
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

/// A refusal to bind, phrased for the operator who has to fix it.
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

/// The config-section names the registry echoes in its refusals.
fn labels() -> ConfigLabels<'static> {
    ConfigLabels {
        section: "OPENCOMPANY_MEMORY",
        drivers: "OPENCOMPANY_MEMORY_DRIVER",
        driver_entry: "OPENCOMPANY_MEMORY_DRIVER",
    }
}

/// Opens the configured driver, returning it with the class the *host* assigned.
///
/// `Ok(None)` means [`MemoryMode::Embedded`] with no driver named: the caller
/// keeps the existing `EngineCortex` overlay rather than binding a provider
/// here. Embedded *with* `OPENCOMPANY_MEMORY_DRIVER=namespace` binds the
/// contract's durable in-pod store through this seam — see the module docs.
///
/// # Errors
///
/// Every failure names the knob to change. A missing credential is a refusal,
/// never a silent downgrade to the embedded engine: a company that thinks it is
/// writing to its hosted memory and is not is worse off than one that fails to
/// start, because the first failure is invisible until the memory is needed.
pub fn open_driver(
    config: &MemoryDriverConfig,
) -> Result<Option<(Arc<dyn MemoryProvider>, DriverClass)>> {
    let bound: (Arc<dyn MemoryProvider>, DriverClass) = match config.mode {
        MemoryMode::Embedded => {
            let Some(driver_id) = config
                .driver_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                // The incumbent engine overlay, untouched. Deliberately not a
                // bind through this seam — see the module docs.
                return Ok(None);
            };
            if driver_id != NAMESPACE_DRIVER_ID {
                return Err(MemoryDriverError(format!(
                    "OPENCOMPANY_MEMORY=embedded with OPENCOMPANY_MEMORY_DRIVER=\
                     {driver_id} names a driver this mode cannot bind. The only \
                     embedded contract driver is `{NAMESPACE_DRIVER_ID}`; unset \
                     OPENCOMPANY_MEMORY_DRIVER to keep the incumbent engine \
                     overlay, or switch OPENCOMPANY_MEMORY=remote for a hosted \
                     engine."
                ))
                .into());
            }
            let class = admit(NAMESPACE_DRIVER_ID, DriverClass::Embedded)?;
            (namespace_provider(config)?, class)
        }
        MemoryMode::Null => {
            let admission = admit(NULL_DRIVER_ID, DriverClass::Null)?;
            (
                Arc::new(NullMemoryProvider::new()) as Arc<dyn MemoryProvider>,
                admission,
            )
        }
        MemoryMode::Remote => {
            let driver_id = config
                .driver_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    MemoryDriverError(format!(
                        "OPENCOMPANY_MEMORY=remote requires OPENCOMPANY_MEMORY_DRIVER \
                         naming the hosted engine — one of {}. There is no \
                         default: binding the wrong hosted engine writes a company's memory \
                         somewhere it cannot be read back from.",
                        SUPPORTED_REMOTE_DRIVERS.join(", ")
                    ))
                })?;
            let url = require(
                config.url.as_deref(),
                "OPENCOMPANY_MEMORY=remote requires OPENCOMPANY_MEMORY_URL \
                 naming the hosted engine's endpoint",
            )?;
            let key = require(
                config.api_key.as_deref(),
                "OPENCOMPANY_MEMORY=remote requires a credential: set OPENCOMPANY_MEMORY_API_KEY, \
                — the key is a secret and env is its only channel",
            )?;
            let class = admit(driver_id, DriverClass::External)?;
            (remote_provider(driver_id, url, key)?, class)
        }
    };
    audit_capabilities(bound.0.as_ref())?;
    Ok(Some(bound))
}

/// Refuses a driver that over-claims what it implements, and reports one that
/// under-claims.
///
/// `capabilities()` is a hand-written claim; `provides()` is derived from the
/// accessors and cannot drift. Comparing them is the contract's own honesty
/// check, and `tinymemory_api::provider::audit` says explicitly to run it "at
/// bind time" — which nothing here was doing.
///
/// The two directions are not the same failure, so they are not treated the
/// same:
///
/// - **Advertised but absent** refuses the bind. The host registers RPC methods
///   and assembles agent tools from the *claim* and never re-checks, so an
///   over-claim becomes a surface that exists, is offered to an agent, and fails
///   on first call — inside a tenant, at the moment the memory is needed.
/// - **Present but unadvertised** only warns. The family works; nothing routes
///   to it, because routing follows the claim. Upstream calls that dead surface
///   from a forgotten `capabilities()` entry. Refusing a boot over it would turn
///   an upstream oversight into a tenant outage, which is a worse trade than
///   running with one family unreachable.
///
/// Structurally neither should fire: every adapter reachable from here is
/// composed through `MemoryTraitProvider`, which derives the advertisement from
/// the accessors. It runs anyway because that guarantee lives upstream, in a
/// submodule this repo pins by gitlink, and a gitlink bump is exactly when it
/// would quietly stop holding.
fn audit_capabilities(provider: &dyn MemoryProvider) -> Result<()> {
    let Err(mismatch) = tinymemory_api::provider::audit_provider(provider) else {
        return Ok(());
    };
    if !mismatch.present_but_unadvertised.is_empty() {
        tracing::warn!(
            driver_id = provider.driver_id(),
            families = %families(&mismatch.present_but_unadvertised),
            "the bound memory driver implements capability families it does not advertise; they \
             are unreachable, because the host routes from the advertised set",
        );
    }
    if !mismatch.advertised_but_absent.is_empty() {
        return Err(MemoryDriverError(format!(
            "the memory driver `{}` advertises capability families it does not implement: {}. \
             Every one of those becomes an agent tool that fails on first call, so the bind is \
             refused here rather than left to surface mid-cycle. This is an adapter bug rather \
             than a configuration mistake — no environment variable lifts it.",
            provider.driver_id(),
            families(&mismatch.advertised_but_absent)
        ))
        .into());
    }
    Ok(())
}

/// Formats capability families for an operator-facing message.
fn families(families: &[tinymemory_api::capabilities::Capability]) -> String {
    families
        .iter()
        .map(|family| family.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns `value`, or the refusal text when it is absent or blank.
fn require<'a>(value: Option<&'a str>, refusal: &str) -> Result<&'a str> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MemoryDriverError(refusal.to_string()).into())
}

/// Runs the driver id through the registry and pins the class host-side.
///
/// Two checks, both of which have to pass:
///
/// 1. The registry's own admission, which is what refuses an external driver
///    whose trust has not been explicitly raised.
/// 2. That the class the registry resolved matches the class this *mode*
///    implies. `OPENCOMPANY_MEMORY=remote` naming `tinycortex` is a
///    configuration mistake with a security shape — it would run an engine
///    under the checks meant for the other class — so it is refused rather than
///    quietly resolved in the registry's favour.
fn admit(driver_id: &str, expected: DriverClass) -> Result<DriverClass> {
    // `namespace` is `tinymemory-core`'s own durable store. Since the v1.1.0
    // pin the vendored registry reserves the id itself
    // (`registry/mod.rs:182`), so this `with_reserved` restates the same
    // class the builtin table already carries — kept deliberately: repeating
    // a reserved id's class is the one thing the registry's own rule permits
    // ("may repeat, never override"), and the host-side line keeps the
    // reservation visible where the driver is routed rather than only inside
    // the vendor. Reserved unconditionally: with `tinymemory-embedded`
    // disabled this function still runs for the id — admission comes first —
    // and the feature-off `namespace_provider` fallback then rejects the
    // bind, which is the fail-closed half of the pair.
    let registry =
        DriverRegistry::builtin().with_reserved(NAMESPACE_DRIVER_ID, DriverClass::Embedded);
    // Trust is asserted by the host for a driver the host itself selected from
    // its own configuration: reaching this line already means the operator named
    // the engine and supplied its endpoint and credential. The registry's
    // fail-closed trust gate exists for config files that name a driver without
    // meaning to enable it, which is not a state this path can be in.
    let entry = DriverEntry {
        class: Some(expected.as_str()),
        trust_state: TRUSTED,
    };
    let admission = registry
        .admit(driver_id, Some(entry), labels())
        .map_err(|reason| {
            MemoryDriverError(format!(
                "memory driver `{}` was refused: {}",
                reason.configured_driver, reason.reason
            ))
        })?;
    if admission.class != expected {
        return Err(MemoryDriverError(format!(
            "driver `{driver_id}` is class `{}`, but OPENCOMPANY_MEMORY selected a mode that \
             requires class `{}`. Pick a driver matching the mode, or change the mode.",
            admission.class.as_str(),
            expected.as_str()
        ))
        .into());
    }
    Ok(admission.class)
}

/// Builds the HTTP provider for one hosted engine.
///
/// Capability honesty is the adapters' own: each is composed through
/// `MemoryTraitProvider`, which advertises exactly Core + Recall + Portability
/// and leaves every optional accessor `None`. That is the truth about a hosted
/// service — no summary tree, no graph, no taint tier — and it is what makes
/// `audit_provider` pass at bind rather than a call fail later.
fn remote_provider(driver_id: &str, url: &str, key: &str) -> Result<Arc<dyn MemoryProvider>> {
    let provider: Arc<dyn MemoryProvider> = match driver_id {
        SUPERMEMORY_DRIVER_ID => Arc::new(tinymemory_remote::supermemory_provider(
            tinymemory_remote::SupermemoryMemory::new(url, Some(key)).map_err(open_failed)?,
        )),
        MEM0_DRIVER_ID => Arc::new(tinymemory_remote::mem0_provider(
            tinymemory_remote::Mem0Memory::new(url, Some(key)).map_err(open_failed)?,
        )),
        COGNEE_DRIVER_ID => Arc::new(tinymemory_remote::cognee_provider(
            tinymemory_remote::CogneeMemory::new(url, Some(key)).map_err(open_failed)?,
        )),
        // Unreachable in practice: `admit` has already rejected any id the
        // registry does not reserve as External. Kept as a refusal rather than
        // an `unreachable!` so adding a reserved id upstream surfaces here as a
        // clear boot message instead of a panic in a tenant container.
        other => {
            return Err(MemoryDriverError(format!(
                "no HTTP adapter is compiled in for memory driver `{other}`"
            ))
            .into());
        }
    };
    Ok(provider)
}

/// Renders an adapter construction failure without echoing the endpoint.
///
/// The adapters validate the URL at construction, so this is usually a
/// malformed `OPENCOMPANY_MEMORY_URL`. The error text is the adapter's own and
/// is documented not to carry the credential; the endpoint is withheld here for
/// the same reason [`MemoryDriverConfig`]'s `Debug` withholds it.
fn open_failed(error: anyhow::Error) -> OpenCompanyError {
    OpenCompanyError::Config(format!(
        "could not open the configured memory engine: {error}. \
         Check OPENCOMPANY_MEMORY_URL."
    ))
}

/// The driver ids this build can actually construct, for error text and docs.
pub const SUPPORTED_REMOTE_DRIVERS: [&str; 3] =
    [SUPERMEMORY_DRIVER_ID, MEM0_DRIVER_ID, COGNEE_DRIVER_ID];

/// The contract's own durable store, as a driver id.
///
/// The value matches what the driver reports: `UnifiedMemory`'s `Memory`
/// implementation answers `name() == "namespace"`, and a bound driver whose
/// configured id disagreed with its reported id would make status output lie.
pub const NAMESPACE_DRIVER_ID: &str = "namespace";

/// The subdirectory of the data root holding the contract driver's store.
///
/// Beside — never inside — the incumbent engine's `memory/`: `UnifiedMemory`
/// lays out `namespaces/`, `vectors/` and `memory.db` under its root, and
/// `EngineCortex` mints a subdirectory per company under its own, so sharing
/// one directory would interleave the two schemas and put a company named
/// anything that sanitises to `namespaces` on top of the store's own layout.
// Gated with its only consumer (`namespace_provider`): the new strict
// `acp,runner,tinymemory` clippy lane compiles this file without
// `tinymemory-embedded`, where an ungated const is dead code.
#[cfg(feature = "tinymemory-embedded")]
const NAMESPACE_STORE_SUBDIR: &str = "memory-namespace";

/// Builds the in-pod contract driver over `tinymemory-core`'s durable store.
///
/// Same composition honesty as [`remote_provider`]: `MemoryTraitProvider`
/// advertises exactly Core + Recall + Portability and leaves every optional
/// accessor `None`, which is the truth about this driver and what makes the
/// bind-time audit pass by construction.
#[cfg(feature = "tinymemory-embedded")]
fn namespace_provider(config: &MemoryDriverConfig) -> Result<Arc<dyn MemoryProvider>> {
    let Some(data_dir) = config.data_dir.as_deref() else {
        return Err(MemoryDriverError(format!(
            "OPENCOMPANY_MEMORY_DRIVER={NAMESPACE_DRIVER_ID} persists to \
             <data_dir>/{NAMESPACE_STORE_SUBDIR}/ and no data dir is configured. \
             Set OPENCOMPANY_DATA_DIR to a durable path. There is deliberately \
             no in-memory fallback: a store that answers every read and \
             remembers nothing is the failure this surface exists to prevent."
        ))
        .into());
    };
    // No embedding backend is injected: every chunk is stored vector-less and
    // recall runs on the store's graph and keyword tiers. The same loud
    // degraded-mode contract the incumbent engine ships under.
    tracing::warn!(
        data_dir = %data_dir.display(),
        "OPENCOMPANY_MEMORY_DRIVER=namespace is running in DEGRADED lexical/graph recall mode: \
         no embeddings backend is injected, so recall is keyword/graph ranking, NOT \
         vector/semantic recall.",
    );
    let memory = tinymemory_core::store::UnifiedMemory::new_with_memory_dir(
        data_dir,
        NAMESPACE_STORE_SUBDIR,
        Arc::new(tinymemory_api::host::NoopEmbedding),
        None,
    )
    .map_err(|error| {
        MemoryDriverError(format!(
            "could not open the embedded contract store under the data dir: {error}. \
             Check that OPENCOMPANY_DATA_DIR exists and is writable."
        ))
    })?;
    // `UnifiedMemory` speaks the engine-side `Memory` trait; `TinycortexMemory`
    // is the adapter that converts its vocabulary to the contract's. The id is
    // this driver's own, not the adapter's `tinycortex` — that name belongs to
    // the incumbent engine overlay, and the store here reports `namespace`.
    Ok(Arc::new(tinymemory::mandatory::MemoryTraitProvider::new(
        Arc::new(tinymemory_tinycortex::TinycortexMemory::new(Arc::new(
            memory,
        ))),
        NAMESPACE_DRIVER_ID,
    )))
}

/// Without the `tinymemory-embedded` feature the in-pod contract driver cannot
/// be served, so it refuses rather than silently resolving to something else —
/// the same contract as `open_provider`'s feature refusal in `store::select`.
#[cfg(not(feature = "tinymemory-embedded"))]
fn namespace_provider(_config: &MemoryDriverConfig) -> Result<Arc<dyn MemoryProvider>> {
    Err(MemoryDriverError(format!(
        "OPENCOMPANY_MEMORY_DRIVER={NAMESPACE_DRIVER_ID} requires a build with the \
         `tinymemory-embedded` feature"
    ))
    .into())
}

#[cfg(test)]
mod test {
    use super::*;
    use tinymemory::registry::TINYCORTEX_DRIVER_ID;

    fn config(mode: MemoryMode) -> MemoryDriverConfig {
        MemoryDriverConfig {
            mode,
            driver_id: None,
            url: None,
            api_key: None,
            data_dir: None,
        }
    }

    /// A driver that claims a family it does not implement.
    ///
    /// Delegates every mandatory method to the null driver and changes exactly
    /// one thing: `capabilities()` adds `Graph`, while `as_graph()` keeps the
    /// contract's `None` default. That is the shape `audit_capabilities` exists
    /// to catch — an adapter whose hand-written claim outran its accessors.
    struct OverClaimer(NullMemoryProvider);

    #[async_trait::async_trait]
    impl tinymemory_api::provider::MemoryCore for OverClaimer {
        async fn store(
            &self,
            namespace: &str,
            key: &str,
            content: &str,
            category: tinymemory_api::types::MemoryCategory,
            session_id: Option<&str>,
            taint: tinymemory_api::types::MemoryTaint,
        ) -> std::result::Result<(), tinymemory_api::error::MemoryError> {
            self.0
                .store(namespace, key, content, category, session_id, taint)
                .await
        }
        async fn get(
            &self,
            namespace: &str,
            key: &str,
        ) -> std::result::Result<
            Option<tinymemory_api::types::MemoryEntry>,
            tinymemory_api::error::MemoryError,
        > {
            self.0.get(namespace, key).await
        }
        async fn forget(
            &self,
            namespace: &str,
            key: &str,
        ) -> std::result::Result<bool, tinymemory_api::error::MemoryError> {
            self.0.forget(namespace, key).await
        }
        async fn list(
            &self,
            namespace: Option<&str>,
            category: Option<&tinymemory_api::types::MemoryCategory>,
            session_id: Option<&str>,
        ) -> std::result::Result<
            Vec<tinymemory_api::types::MemoryEntry>,
            tinymemory_api::error::MemoryError,
        > {
            self.0.list(namespace, category, session_id).await
        }
        async fn namespaces(
            &self,
        ) -> std::result::Result<
            Vec<tinymemory_api::types::NamespaceSummary>,
            tinymemory_api::error::MemoryError,
        > {
            self.0.namespaces().await
        }
    }

    #[async_trait::async_trait]
    impl tinymemory_api::provider::MemoryRecall for OverClaimer {
        async fn recall(
            &self,
            query: &str,
            limit: usize,
            opts: &tinymemory_api::recall::OwnedRecallOpts,
            scope: Option<&tinymemory_api::provider::SourceScope>,
        ) -> std::result::Result<
            Vec<tinymemory_api::types::MemoryEntry>,
            tinymemory_api::error::MemoryError,
        > {
            self.0.recall(query, limit, opts, scope).await
        }
    }

    #[async_trait::async_trait]
    impl tinymemory_api::provider::MemoryPortability for OverClaimer {
        async fn export_page(
            &self,
            cursor: Option<&str>,
            limit: usize,
        ) -> std::result::Result<
            tinymemory_api::provider::ExportPage,
            tinymemory_api::error::MemoryError,
        > {
            self.0.export_page(cursor, limit).await
        }
        async fn import_records(
            &self,
            records: Vec<tinymemory_api::provider::ExportRecord>,
        ) -> std::result::Result<
            tinymemory_api::provider::ImportOutcome,
            tinymemory_api::error::MemoryError,
        > {
            self.0.import_records(records).await
        }
    }

    #[async_trait::async_trait]
    impl MemoryProvider for OverClaimer {
        fn driver_id(&self) -> &str {
            "over-claimer"
        }
        fn capabilities(&self) -> tinymemory_api::capabilities::Capabilities {
            let mut claimed = self.0.capabilities();
            claimed.insert(tinymemory_api::capabilities::Capability::Graph);
            claimed
        }
        async fn health(&self) -> tinymemory_api::health::MemoryHealth {
            self.0.health().await
        }
    }

    #[test]
    fn an_over_claiming_driver_is_refused_and_names_the_family() {
        let error = audit_capabilities(&OverClaimer(NullMemoryProvider::new()))
            .expect_err("a driver advertising Graph without implementing it must be refused")
            .to_string();
        assert!(error.contains("over-claimer"), "{error}");
        assert!(error.contains("graph"), "{error}");
    }

    #[test]
    fn an_honest_driver_passes_the_audit() {
        // The other half of the gate: it must not refuse the drivers we ship.
        audit_capabilities(&NullMemoryProvider::new()).expect("the null driver is honest");
    }

    #[test]
    fn embedded_keeps_the_existing_overlay() {
        assert!(
            open_driver(&config(MemoryMode::Embedded))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn embedded_refuses_a_driver_id_that_is_not_namespace() {
        // Never a silent fallback to the engine the operator did not name: an
        // unknown id under the embedded mode is a refusal that names the one
        // id this mode can bind.
        let mut cfg = config(MemoryMode::Embedded);
        cfg.driver_id = Some(SUPERMEMORY_DRIVER_ID.into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains(NAMESPACE_DRIVER_ID), "{error}");
        assert!(error.contains("OPENCOMPANY_MEMORY_DRIVER"), "{error}");
    }

    #[test]
    fn a_blank_embedded_driver_id_keeps_the_existing_overlay() {
        // An env var set to the empty string is the shape a broken deployment
        // template produces; it must mean "not set", exactly as it does for the
        // remote credential.
        let mut cfg = config(MemoryMode::Embedded);
        cfg.driver_id = Some("  ".into());
        assert!(open_driver(&cfg).unwrap().is_none());
    }

    #[cfg(feature = "tinymemory-embedded")]
    #[test]
    fn the_namespace_driver_without_a_data_dir_refuses_and_names_the_knob() {
        // No in-memory fallback: a store that answers every read and remembers
        // nothing is the exact failure this surface exists to prevent.
        let mut cfg = config(MemoryMode::Embedded);
        cfg.driver_id = Some(NAMESPACE_DRIVER_ID.into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains("OPENCOMPANY_DATA_DIR"), "{error}");
    }

    #[cfg(not(feature = "tinymemory-embedded"))]
    #[test]
    fn the_namespace_driver_without_the_feature_names_the_feature() {
        let mut cfg = config(MemoryMode::Embedded);
        cfg.driver_id = Some(NAMESPACE_DRIVER_ID.into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains("tinymemory-embedded"), "{error}");
    }

    #[cfg(feature = "tinymemory-embedded")]
    #[test]
    fn the_namespace_driver_binds_durable_survives_reopen_and_passes_the_audit() {
        // The full claim, end to end: it binds as class Embedded under the
        // reserved id, its advertisement survives the same audit every other
        // driver faces, a stored entry recalls through the contract, and — the
        // property the incumbent was kept for — the store is still there after
        // the driver is dropped and reopened.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(MemoryMode::Embedded);
        cfg.driver_id = Some(NAMESPACE_DRIVER_ID.into());
        cfg.data_dir = Some(dir.path().to_path_buf());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        {
            let (provider, class) = open_driver(&cfg).unwrap().unwrap();
            assert_eq!(class, DriverClass::Embedded);
            assert_eq!(provider.driver_id(), NAMESPACE_DRIVER_ID);
            assert!(
                tinymemory_api::provider::audit_provider(provider.as_ref()).is_ok(),
                "the namespace driver failed its capability audit"
            );
            runtime
                .block_on(provider.store(
                    "oc/test",
                    "fact-1",
                    "the build is green",
                    tinymemory_api::types::MemoryCategory::Core,
                    None,
                    tinymemory_api::types::MemoryTaint::Internal,
                ))
                .unwrap();
        }

        // A second bind against the same data dir must read the first bind's
        // write back — this is the durability the module docs promise, and the
        // property that separates this driver from the in-memory engine the
        // old module docs warned about.
        let (reopened, _) = open_driver(&cfg).unwrap().unwrap();
        let entry = runtime
            .block_on(reopened.get("oc/test", "fact-1"))
            .unwrap()
            .expect("the entry stored before the reopen must still be there");
        assert_eq!(entry.content, "the build is green");
        // And it landed under the driver's own subdirectory, beside — not
        // inside — the incumbent engine's `memory/`.
        assert!(
            dir.path()
                .join("memory-namespace")
                .join("memory.db")
                .exists()
        );
    }

    #[test]
    fn null_binds_and_is_class_null() {
        let (provider, class) = open_driver(&config(MemoryMode::Null)).unwrap().unwrap();
        assert_eq!(class, DriverClass::Null);
        assert_eq!(provider.driver_id(), NULL_DRIVER_ID);
    }

    #[test]
    fn the_null_driver_passes_its_capability_audit() {
        let (provider, _) = open_driver(&config(MemoryMode::Null)).unwrap().unwrap();
        assert!(tinymemory_api::provider::audit_provider(provider.as_ref()).is_ok());
    }

    #[test]
    fn remote_without_a_driver_id_refuses_and_names_the_knob() {
        let mut cfg = config(MemoryMode::Remote);
        cfg.url = Some("https://memory.example".into());
        cfg.api_key = Some("k".into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains("OPENCOMPANY_MEMORY_DRIVER"), "{error}");
    }

    #[test]
    fn remote_without_a_url_refuses_and_names_the_knob() {
        let mut cfg = config(MemoryMode::Remote);
        cfg.driver_id = Some(SUPERMEMORY_DRIVER_ID.into());
        cfg.api_key = Some("k".into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains("OPENCOMPANY_MEMORY_URL"), "{error}");
    }

    #[test]
    fn remote_without_a_credential_refuses_and_names_the_knob() {
        let mut cfg = config(MemoryMode::Remote);
        cfg.driver_id = Some(SUPERMEMORY_DRIVER_ID.into());
        cfg.url = Some("https://memory.example".into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains("OPENCOMPANY_MEMORY_API_KEY"), "{error}");
        // The refusal must not resurrect the phantom manifest knob (#1113):
        // env is the credential's only channel, and the message says so.
        assert!(!error.contains("api_key_secret"), "{error}");
        assert!(error.contains("only channel"), "{error}");
    }

    #[test]
    fn a_blank_credential_is_treated_as_missing() {
        // An env var set to the empty string is the shape a broken deployment
        // template produces, and it must not read as "configured".
        let mut cfg = config(MemoryMode::Remote);
        cfg.driver_id = Some(SUPERMEMORY_DRIVER_ID.into());
        cfg.url = Some("https://memory.example".into());
        cfg.api_key = Some("   ".into());
        assert!(open_driver(&cfg).is_err());
    }

    #[test]
    fn remote_refuses_an_embedded_driver_id() {
        // Class is host-side: naming the embedded engine under the remote mode
        // would run it under the wrong checks.
        let mut cfg = config(MemoryMode::Remote);
        cfg.driver_id = Some(TINYCORTEX_DRIVER_ID.into());
        cfg.url = Some("https://memory.example".into());
        cfg.api_key = Some("k".into());
        let error = open_driver(&cfg).err().unwrap().to_string();
        assert!(error.contains("class"), "{error}");
    }

    #[test]
    fn remote_refuses_an_unknown_driver_id() {
        let mut cfg = config(MemoryMode::Remote);
        cfg.driver_id = Some("definitely-not-an-engine".into());
        cfg.url = Some("https://memory.example".into());
        cfg.api_key = Some("k".into());
        assert!(open_driver(&cfg).is_err());
    }

    #[test]
    fn every_supported_driver_id_binds() {
        for id in SUPPORTED_REMOTE_DRIVERS {
            let mut cfg = config(MemoryMode::Remote);
            cfg.driver_id = Some(id.to_string());
            cfg.url = Some("https://memory.example".into());
            cfg.api_key = Some("k".into());
            let (provider, class) = open_driver(&cfg)
                .unwrap_or_else(|error| panic!("{id} did not bind: {error}"))
                .unwrap();
            assert_eq!(class, DriverClass::External, "{id}");
            assert_eq!(provider.driver_id(), id);
            assert!(
                tinymemory_api::provider::audit_provider(provider.as_ref()).is_ok(),
                "{id} failed its capability audit"
            );
        }
    }

    #[test]
    fn debug_never_renders_the_credential_or_the_endpoint() {
        let cfg = MemoryDriverConfig {
            mode: MemoryMode::Remote,
            driver_id: Some(SUPERMEMORY_DRIVER_ID.into()),
            url: Some("https://memory.internal.example".into()),
            api_key: Some("sk-super-secret-value".into()),
            data_dir: None,
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("sk-super-secret-value"), "{rendered}");
        assert!(!rendered.contains("memory.internal.example"), "{rendered}");
    }
}
