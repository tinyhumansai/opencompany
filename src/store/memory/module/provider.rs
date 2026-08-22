//! The host half of `ai.tinyhumans.tinymemory.Memory` (issue #1524).
//!
//! [`ModuleMemoryProvider`] implements the mandatory provider families by
//! forwarding each method to the loaded module over the bus. The wire surface
//! mirrors the trait one member for one member, so there is no translation
//! layer — only the bus call, the shared error table, and the decisions below.
//!
//! # Construction is synchronous and does no I/O
//!
//! `open_driver` and `open_memory_overlay` are synchronous and reached by
//! thousands of test callers with no tokio runtime. So [`ModuleMemoryProvider::new`]
//! stores its configuration and resolves everything — the bus, the load, the
//! proxies — lazily on first use.
//!
//! # `capabilities()` is answered statically, and narrowly
//!
//! The advertisement is exactly `Core | Recall | Portability`, with every
//! optional accessor left on the trait's `None` default. This makes the
//! bind-time audit pass **by construction**, keeps `/spec`'s capability list
//! at three names, and makes the advertisement independent of which artifact
//! version loaded — deleting the pin-the-artifact-capability-list mechanism
//! (and its stale-pin bug class) rather than maintaining it. The module's
//! engine serves more families; this host deliberately does not reach them.
//!
//! # Errors round-trip through the shared table
//!
//! [`tinymemory_bus::wire::from_wire`] maps a `(name, message)` pair back to a
//! [`MemoryError`], and **both ends use it** — reimplementing the mapping here
//! is what would let a `PathEscape` arrive as an `Invalid`, reclassifying a
//! sandbox escape as a caller mistake. The tinybus transport variants that
//! never carry a wire name get explicit arms instead: a deadline is a
//! [`MemoryError::Timeout`], and a missing owner or dead transport is
//! [`MemoryError::Unreachable`].
//!
//! # Missing taint is refused, not defaulted
//!
//! `MemoryEntry.taint` is `#[serde(default)]` and the default is `Internal` —
//! the trusted end of the scale. A module answer that omits the field (a
//! version skew, a serialization bug) would therefore *upgrade* externally
//! sourced content to internal trust on decode, failing open. Entry-bearing
//! answers are decoded through [`entries_with_taint`], which refuses any
//! entry object lacking an explicit `taint` before the typed decode runs.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tinybus::Proxy;
use tinymemory_api::capabilities::{Capabilities, Capability};
use tinymemory_api::error::MemoryError;
use tinymemory_api::health::MemoryHealth;
use tinymemory_api::provider::types::{ExportPage, ExportRecord, ImportOutcome, SourceScope};
use tinymemory_api::provider::{MemoryCore, MemoryPortability, MemoryProvider, MemoryRecall};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{MemoryCategory, MemoryEntry, MemoryTaint};
use tinymemory_bus::names::{BUS_NAME, OBJECT_PATH, methods};
use tinymemory_bus::wire;

use super::{host, ops};

/// The reserved driver id this provider binds as.
pub const MODULE_DRIVER_ID: &str = "module";

/// Deadline for the interactive members (Store/Get/Forget/List/Namespaces/
/// Recall/Health). These sit on cycle paths where the caller is an agent turn:
/// a store that has not answered in 30 seconds is not about to, and surfacing
/// a `Timeout` inside a turn's patience beats a hung turn. opencompany applied
/// no ceiling at all before this driver, so any value is new — 30s is the
/// widest that still fails inside one turn.
const SHORT_DEADLINE: Duration = Duration::from_secs(30);

/// Deadline for the bulk members (ExportPage/ImportRecords). A migrate page
/// is up to 500 records against a possibly cold store; 300s matches the
/// per-page patience `memory migrate` already assumes, and a bus timeout does
/// not cancel the remote work — the migrate runbook carries that caveat.
const LONG_DEADLINE: Duration = Duration::from_secs(300);

/// The mandatory provider over the loaded TinyMemory module.
pub struct ModuleMemoryProvider {
    /// The instance data root; the module's store lives under
    /// `<data_dir>/memory-module` (`ops::module_workspace_dir`).
    data_dir: PathBuf,
    /// The interactive-deadline proxy, resolved on first use.
    short: tokio::sync::OnceCell<Proxy>,
    /// The bulk-deadline proxy, resolved on first use.
    long: tokio::sync::OnceCell<Proxy>,
}

impl ModuleMemoryProvider {
    /// A provider bound to `data_dir`. Synchronous, no I/O.
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            short: tokio::sync::OnceCell::new(),
            long: tokio::sync::OnceCell::new(),
        }
    }

    /// Ensure the module is loaded, and hand back the proxy for `deadline`.
    ///
    /// `operation` names the forwarded member for the diagnostic — never a
    /// namespace, key, or content value: those are user memory.
    async fn proxy(&self, operation: &str, long: bool) -> Result<Proxy, MemoryError> {
        tracing::debug!(%operation, "[memory-module] resolving module proxy");
        ops::ensure_loaded(&self.data_dir)
            .await
            .map_err(|reason| MemoryError::Other(anyhow::anyhow!(reason)))?;
        let runtime = host::runtime().await.map_err(|error| {
            MemoryError::Other(anyhow::anyhow!("the module bus is not running: {error}"))
        })?;
        let cell = if long { &self.long } else { &self.short };
        cell.get_or_try_init(|| async {
            let deadline = if long { LONG_DEADLINE } else { SHORT_DEADLINE };
            runtime
                .proxy(BUS_NAME, OBJECT_PATH)
                .map(|proxy| proxy.with_timeout(deadline))
                .map_err(|error| {
                    MemoryError::Other(anyhow::anyhow!(
                        "the module proxy could not be built: {error}"
                    ))
                })
        })
        .await
        .cloned()
    }
}

/// Map a bus failure back onto a [`MemoryError`].
///
/// [`MethodFailed`](tinybus::Error::MethodFailed) rides the shared wire
/// table, so the host and the module cannot disagree about what a name means.
/// The transport-level variants never carry a wire name and get explicit
/// arms; everything else degrades to `Other`, never `Invalid`.
/// Whether a bus error is the frame-size cap refusing an oversized message.
///
/// Matched on the message because tinybus reports it as a protocol error
/// carrying the byte counts rather than a distinct variant. **One marker,
/// deliberately**: both sites that raise it phrase it with the cap —
/// `encode`'s "frame of N bytes exceeds the {MAX}-byte cap" and
/// `decode_length`'s "peer announced an N-byte frame, over the {MAX}-byte
/// cap" — so `-byte cap` alone covers the pair. An extra alternative like
/// "over the" would add nothing and would match any protocol error that
/// happened to contain the phrase, and `Error::Protocol` carries arbitrary
/// text. That is not a cosmetic risk: a false positive here halves the page
/// and retries, so an unrelated transport fault would turn into a quiet
/// retry loop ending in a wrong "this record is too large" verdict.
fn is_frame_cap_refusal(error: &tinybus::Error) -> bool {
    error.to_string().contains("-byte cap")
}

fn from_bus(error: &tinybus::Error) -> MemoryError {
    match error {
        tinybus::Error::MethodFailed { .. } => {
            wire::from_wire(error.wire_name(), &error.to_string())
        }
        tinybus::Error::Timeout { .. } => MemoryError::Timeout(error.to_string()),
        tinybus::Error::NameHasNoOwner(_)
        | tinybus::Error::Transport(_)
        | tinybus::Error::Io(_)
        | tinybus::Error::ConnectionClosed => MemoryError::Unreachable(error.to_string()),
        other => MemoryError::Other(anyhow::anyhow!(other.to_string())),
    }
}

/// Decode a JSON answer whose entries must carry an explicit `taint`.
///
/// Accepts the raw bus value BEFORE the typed decode: `MemoryEntry.taint` is
/// `#[serde(default)]`, so by the time serde has run, a missing field and an
/// explicit `internal` are indistinguishable — and the default is the trusted
/// class, so the skew fails open. Refusing here is the fail-closed half.
fn refuse_missing_taint(value: &serde_json::Value) -> Result<(), MemoryError> {
    let missing = |entry: &serde_json::Value| entry.is_object() && entry.get("taint").is_none();
    let violated = match value {
        serde_json::Value::Array(entries) => entries.iter().any(missing),
        serde_json::Value::Object(_) => missing(value),
        _ => false,
    };
    if violated {
        return Err(MemoryError::Other(anyhow::anyhow!(
            "the module answered a memory entry with no taint field; refusing rather than \
             defaulting external content to internal trust (version skew between the host \
             and the loaded artifact)"
        )));
    }
    Ok(())
}

/// `call` + taint guard + typed decode, for entry-bearing answers.
async fn call_guarded<T: serde::de::DeserializeOwned>(
    proxy: &Proxy,
    method: &str,
    body: impl serde::Serialize + Send,
) -> Result<T, MemoryError> {
    let raw: serde_json::Value = proxy
        .call(method, body)
        .await
        .map_err(|error| from_bus(&error))?;
    refuse_missing_taint(&raw)?;
    serde_json::from_value(raw).map_err(|error| {
        MemoryError::Other(anyhow::anyhow!("the module answer did not decode: {error}"))
    })
}

#[async_trait]
impl MemoryProvider for ModuleMemoryProvider {
    fn driver_id(&self) -> &str {
        MODULE_DRIVER_ID
    }

    fn capabilities(&self) -> Capabilities {
        [
            Capability::Core,
            Capability::Recall,
            Capability::Portability,
        ]
        .into_iter()
        .collect()
    }

    async fn health(&self) -> MemoryHealth {
        match self.proxy("health", false).await {
            Ok(proxy) => match proxy.call::<MemoryHealth>(methods::HEALTH, ()).await {
                Ok(health) => health,
                Err(error) => MemoryHealth::down(from_bus(&error).to_string()),
            },
            Err(error) => MemoryHealth::down(error.to_string()),
        }
    }

    async fn shutdown(&self) -> Result<(), MemoryError> {
        // Best-effort BY CONTRACT: shutdown failure never blocks exit, and a
        // module that was never loaded has nothing to shut down — including
        // the case where a prior load STARTED the runtime and then failed
        // admission, where resolving a proxy here would only re-surface the
        // cached load error on the exit path. Log, never propagate.
        if !host::is_started() {
            return Ok(());
        }
        let outcome = async {
            self.proxy("shutdown", false)
                .await?
                .call::<()>(methods::SHUTDOWN, ())
                .await
                .map_err(|error| from_bus(&error))
        }
        .await;
        if let Err(error) = outcome {
            tracing::debug!(%error, "[memory-module] shutdown call failed; continuing exit");
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryCore for ModuleMemoryProvider {
    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> Result<(), MemoryError> {
        // No log line carries `namespace`, `key` or `content` — user memory.
        self.proxy("store", false)
            .await?
            .call::<()>(
                methods::STORE,
                (
                    namespace,
                    key,
                    content,
                    category,
                    session_id.map(str::to_string),
                    taint,
                ),
            )
            .await
            .map_err(|error| from_bus(&error))
    }

    async fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        let proxy = self.proxy("get", false).await?;
        call_guarded(&proxy, methods::GET, (namespace, key)).await
    }

    async fn forget(&self, namespace: &str, key: &str) -> Result<bool, MemoryError> {
        self.proxy("forget", false)
            .await?
            .call(methods::FORGET, (namespace, key))
            .await
            .map_err(|error| from_bus(&error))
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let proxy = self.proxy("list", false).await?;
        call_guarded(
            &proxy,
            methods::LIST,
            (
                namespace.map(str::to_string),
                category.cloned(),
                session_id.map(str::to_string),
            ),
        )
        .await
    }

    async fn namespaces(
        &self,
    ) -> Result<Vec<tinymemory_api::types::NamespaceSummary>, MemoryError> {
        self.proxy("namespaces", false)
            .await?
            .call(methods::NAMESPACES, ())
            .await
            .map_err(|error| from_bus(&error))
    }
}

#[async_trait]
impl MemoryRecall for ModuleMemoryProvider {
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: &OwnedRecallOpts,
        scope: Option<&SourceScope>,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        // `scope` crosses as a value: the module must apply it as a query
        // predicate internally — narrowing the answer here instead would let
        // it spend `limit` on entries the caller may not see.
        let proxy = self.proxy("recall", false).await?;
        call_guarded(
            &proxy,
            methods::RECALL,
            (query, limit, opts, scope.cloned()),
        )
        .await
    }
}

#[async_trait]
impl MemoryPortability for ModuleMemoryProvider {
    /// Exports one page, **halving the page on a frame-cap refusal** until it
    /// fits or a single record is left.
    ///
    /// A page crosses the bus as one reply, and a reply cannot stream: tinybus
    /// hands a served object no caller connection, so unlike an inbound
    /// payload it cannot be split into chunks. `MAX_FRAME_LEN` is 16 MiB, and
    /// the page size is a count of records, not of bytes — `memory migrate`
    /// defaults to 500 and lets an operator raise it — so a company whose
    /// chunks came from document ingest can ask for a page that cannot be
    /// framed. Without this the migration stops on a protocol error naming a
    /// byte count, which tells an operator nothing about which knob to turn.
    ///
    /// Halving is the honest response: the caller asked for a page, the page
    /// is a batching detail, and returning a smaller one is a complete answer
    /// (`ExportPage` carries its own cursor, so the walk simply takes more
    /// steps). It ends at `1`, where a refusal means one record alone exceeds
    /// the frame — a real limit rather than a tuning problem, and reported as
    /// such rather than retried forever.
    async fn export_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ExportPage, MemoryError> {
        let proxy = self.proxy("export_page", true).await?;
        let mut limit = limit.max(1);
        loop {
            let attempt = proxy
                .call(methods::EXPORT_PAGE, (cursor.map(str::to_string), limit))
                .await;
            return match attempt {
                Ok(page) => Ok(page),
                Err(error) if limit > 1 && is_frame_cap_refusal(&error) => {
                    let smaller = limit / 2;
                    tracing::warn!(
                        limit,
                        smaller,
                        "memory module: an export page did not fit the bus frame; retrying with \
                         a smaller page"
                    );
                    limit = smaller;
                    continue;
                }
                Err(error) if is_frame_cap_refusal(&error) => Err(MemoryError::Invalid(format!(
                    "a single memory record does not fit the module bus frame ({error}); \
                         export cannot carry it, and the record needs to be shortened or \
                         removed before this company can migrate"
                ))),
                Err(error) => Err(from_bus(&error)),
            };
        }
    }

    async fn import_records(
        &self,
        records: Vec<ExportRecord>,
    ) -> Result<ImportOutcome, MemoryError> {
        self.proxy("import_records", true)
            .await?
            .call(methods::IMPORT_RECORDS, (records,))
            .await
            .map_err(|error| from_bus(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction is synchronous with no runtime — the property `open_driver`
    /// and thousands of pre-boot test callers rely on.
    #[test]
    fn construction_is_synchronous_and_does_no_io() {
        let provider = ModuleMemoryProvider::new(std::path::PathBuf::from("/nowhere"));
        assert_eq!(provider.driver_id(), MODULE_DRIVER_ID);
        let capabilities = provider.capabilities();
        assert!(capabilities.contains(Capability::Core));
        assert!(capabilities.contains(Capability::Recall));
        assert!(capabilities.contains(Capability::Portability));
    }

    /// The narrow advertisement is honest by the audit's own rule: every
    /// advertised family reachable, every unadvertised accessor `None`.
    #[test]
    fn the_narrow_advertisement_passes_the_capability_audit() {
        let provider = ModuleMemoryProvider::new(std::path::PathBuf::from("/nowhere"));
        tinymemory_api::provider::audit_provider(&provider)
            .expect("Core|Recall|Portability with every accessor None must audit clean");
    }

    /// Transport-level failures map to the taxonomy's transient classes, and
    /// wire-named failures ride the shared table — including the round trip
    /// that must never reclassify.
    /// The frame-cap detector must fire on what tinybus actually says, and
    /// NOT on unrelated protocol faults — a broad match would turn a real
    /// transport problem into a quiet halving loop that ends in a wrong
    /// "record too large" verdict.
    #[test]
    fn the_frame_cap_detector_is_narrow() {
        // Both phrasings tinybus uses: the encoder's and the length-prefix
        // reader's, quoted from `message::codec`.
        let encoded = tinybus::Error::protocol(
            "frame of 20000000 bytes exceeds the 16777216-byte cap".to_string(),
        );
        let announced = tinybus::Error::protocol(
            "peer announced a 20000000-byte frame, over the 16777216-byte cap".to_string(),
        );
        assert!(is_frame_cap_refusal(&encoded), "{encoded}");
        assert!(is_frame_cap_refusal(&announced), "{announced}");

        // Everything else must not look like the cap — including a protocol
        // error that merely shares wording with the reader's phrasing. The
        // predicate keyed on "over the" as well until review pointed out that
        // `Error::Protocol` carries arbitrary text; both real messages carry
        // "-byte cap", so the extra alternative bought nothing and could have
        // halved pages for an unrelated fault.
        for other in [
            tinybus::Error::ConnectionClosed,
            tinybus::Error::Transport("connection reset".into()),
            tinybus::Error::protocol("unknown method".to_string()),
            tinybus::Error::protocol("peer skipped over the handshake and sent a body".to_string()),
        ] {
            assert!(
                !is_frame_cap_refusal(&other),
                "must not read as the frame cap: {other}"
            );
        }
    }

    #[test]
    fn bus_failures_map_onto_the_taxonomy() {
        let timeout = tinybus::Error::MethodFailed {
            name: wire::wire_name(&MemoryError::Timeout(String::new())).to_string(),
            message: "deadline".to_string(),
        };
        assert!(matches!(from_bus(&timeout), MemoryError::Timeout(_)));

        let invalid = tinybus::Error::MethodFailed {
            name: wire::wire_name(&MemoryError::Invalid("x".into())).to_string(),
            message: "bad".to_string(),
        };
        assert!(matches!(from_bus(&invalid), MemoryError::Invalid(_)));

        assert!(matches!(
            from_bus(&tinybus::Error::Transport("gone".into())),
            MemoryError::Unreachable(_)
        ));
        assert!(matches!(
            from_bus(&tinybus::Error::ConnectionClosed),
            MemoryError::Unreachable(_)
        ));
    }

    /// A module answer omitting `taint` is refused before the typed decode
    /// can default it to internal trust; explicit taint passes, and the
    /// external class survives the guard untouched.
    #[test]
    fn missing_taint_is_refused_rather_than_defaulted() {
        // Built from a REAL serialized entry, so the fixture cannot drift
        // from the contract's schema: the missing-taint case is the full
        // wire shape minus exactly the one field.
        let entry = MemoryEntry {
            id: "e-1".into(),
            key: "k".into(),
            content: "c".into(),
            namespace: Some("ns".into()),
            category: MemoryCategory::Core,
            timestamp: String::new(),
            session_id: None,
            score: None,
            taint: MemoryTaint::ExternalSync,
        };
        let external = serde_json::to_value(vec![entry]).expect("serialize");
        let mut missing = external.clone();
        missing[0]
            .as_object_mut()
            .expect("entry object")
            .remove("taint")
            .expect("the serialized entry carries taint");
        assert!(refuse_missing_taint(&missing).is_err());

        refuse_missing_taint(&external).expect("explicit taint passes");
        let decoded: Vec<MemoryEntry> = serde_json::from_value(external).expect("decodes");
        assert_eq!(decoded[0].taint, MemoryTaint::ExternalSync);

        // Non-entry answers (booleans, counts) pass through untouched.
        refuse_missing_taint(&serde_json::json!(true)).expect("scalars are not entries");
    }
}
