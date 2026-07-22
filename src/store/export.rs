//! Store-agnostic bundle export and import.
//!
//! Export reads *everything* for a company through the four durable storage
//! ports ([`CompanyStore`], [`EventLog`], [`MemoryStore`], [`ContextStore`]) and
//! writes the canonical filesystem [`Bundle`](crate::store::paths::Bundle)
//! layout. Because it drives the ports rather than a backend's private files, an
//! export is *total by construction* for any backend — the fs and sqlite stores
//! produce identical bundles. Import is the exact inverse: it reads a bundle
//! directory and replays every record through the ports, so it materializes into
//! whichever backend the target ports are wired to.
//!
//! The dep-free core operates on an *unpacked bundle directory*. A single-file
//! `.tar` wrapper ([`pack_tar`]/[`unpack_tar`]) is gated behind the `export`
//! feature so the default build links no archive crate.
//!
//! `secrets/` and `keys/` are fs-only artifacts (the builder keeps them on the
//! filesystem even under a non-fs store) with no enumeration port, so they are
//! excluded from an export unless [`ExportOpts::include_secrets`] is set and a
//! source bundle directory is supplied.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::company::CompanyManifest;
use crate::error::OpenCompanyError;
use crate::ports::context::ContextStore;
use crate::ports::events::EventLog;
use crate::ports::memory::MemoryStore;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    CompanyId, CompanyRecord, CompressedTrace, ContextChunk, EventSeq, LedgerEntry, StoredEvent,
};

/// Canonical bundle file and directory names, matching the fs
/// [`Bundle`](crate::store::paths::Bundle) layout.
const COMPANY_TOML: &str = "company.toml";
const META_JSON: &str = "meta.json";
const EVENTS_JSONL: &str = "events.jsonl";
const LEDGER_JSONL: &str = "ledger.jsonl";
const MEMORY_DIR: &str = "memory";
const TRACES_JSONL: &str = "traces.jsonl";
const CONTEXT_DIR: &str = "context";
const CONTEXT_INDEX_JSONL: &str = "index.jsonl";
const CONTEXT_BLOBS_DIR: &str = "blobs";
const SECRETS_DIR: &str = "secrets";
const KEYS_DIR: &str = "keys";

/// The four durable storage ports as trait objects, in export/import order
/// (`CompanyStore`, `EventLog`, `MemoryStore`, `ContextStore`).
pub type Ports = (
    Arc<dyn CompanyStore>,
    Arc<dyn EventLog>,
    Arc<dyn MemoryStore>,
    Arc<dyn ContextStore>,
);

/// Options controlling what an export includes.
#[derive(Clone, Debug, Default)]
pub struct ExportOpts {
    /// Include the fs-only `secrets/` and `keys/` directories. Off by default so
    /// a shared bundle never leaks the company's signing key or secrets.
    pub include_secrets: bool,
    /// The source fs bundle directory to copy `secrets/`/`keys/` from when
    /// [`Self::include_secrets`] is set. Left `None` for a non-fs source (which
    /// has no such artifacts to copy).
    pub fs_bundle: Option<PathBuf>,
}

fn io_err(path: &Path, source: std::io::Error) -> OpenCompanyError {
    OpenCompanyError::StoreIo {
        path: path.to_path_buf(),
        source,
    }
}

/// Bundle metadata persisted alongside the manifest. Carries the company id so an
/// import can restore the original id even when it diverges from the manifest
/// slug. The fs [`CompanyStore`] reads only `lifecycle`; the extra field is
/// ignored there (serde skips unknown fields).
#[derive(Serialize, Deserialize)]
struct BundleMeta {
    lifecycle: String,
    id: String,
}

/// One exported context chunk: its content address, label, and body.
struct ExportedChunk {
    addr: String,
    label: String,
    body: String,
}

/// A context-index line pairing an address with its label and length. Matches the
/// fs [`ContextStore`] index shape.
#[derive(Serialize, Deserialize)]
struct IndexEntry {
    addr: String,
    label: String,
    len: usize,
}

/// Everything an export carries for one company, read through the ports.
struct BundleContents {
    id: CompanyId,
    manifest: CompanyManifest,
    lifecycle: String,
    ledger: Vec<LedgerEntry>,
    events: Vec<StoredEvent>,
    traces: Vec<CompressedTrace>,
    context: Vec<ExportedChunk>,
}

impl BundleContents {
    /// Reads the complete company state through the four durable ports.
    async fn read_via_ports(
        id: &CompanyId,
        store: Arc<dyn CompanyStore>,
        events: Arc<dyn EventLog>,
        memory: Arc<dyn MemoryStore>,
        context: Arc<dyn ContextStore>,
    ) -> Result<Self> {
        let record = store
            .load(id)
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(id.to_string()))?;

        let events = events.read_from(id, EventSeq::new(0), usize::MAX).await?;
        let traces = memory.recent_traces(id, usize::MAX).await?;

        let metas = context.list(id, "").await?;
        let mut chunks = Vec::with_capacity(metas.len());
        for meta in metas {
            let body = context.peek(id, &meta.addr, None).await?;
            chunks.push(ExportedChunk {
                addr: meta.addr.as_ref().to_string(),
                label: meta.label,
                body,
            });
        }

        Ok(Self {
            id: id.clone(),
            manifest: record.manifest,
            lifecycle: record.lifecycle,
            ledger: record.ledger,
            events,
            traces,
            context: chunks,
        })
    }

    /// Replays the complete company state through the four durable ports. Events
    /// are appended in order, so a fresh target log reproduces the original
    /// 0-based sequence numbers; context chunks re-derive their original content
    /// address from the body.
    async fn write_via_ports(
        &self,
        store: Arc<dyn CompanyStore>,
        events: Arc<dyn EventLog>,
        memory: Arc<dyn MemoryStore>,
        context: Arc<dyn ContextStore>,
    ) -> Result<()> {
        // The manifest + lifecycle; ledger is appended separately so the store's
        // append-only ledger stays authoritative.
        store
            .save(&CompanyRecord {
                id: self.id.clone(),
                manifest: self.manifest.clone(),
                ledger: Vec::new(),
                lifecycle: self.lifecycle.clone(),
                overlay_agents: Vec::new(),
            })
            .await?;
        for entry in &self.ledger {
            store.append_ledger(&self.id, entry.clone()).await?;
        }
        for stored in &self.events {
            events.append(&self.id, stored.event.clone()).await?;
        }
        for trace in &self.traces {
            memory.save_trace(&self.id, trace.clone()).await?;
        }
        for chunk in &self.context {
            context
                .put(
                    &self.id,
                    ContextChunk {
                        label: chunk.label.clone(),
                        body: chunk.body.clone(),
                    },
                )
                .await?;
        }
        Ok(())
    }

    /// Writes the canonical fs bundle layout under `dest`.
    async fn write_to_dir(&self, dest: &Path) -> Result<()> {
        create_dir(dest).await?;

        let toml_src = toml::to_string(&self.manifest)
            .map_err(|e| OpenCompanyError::Store(format!("cannot serialize manifest: {e}")))?;
        write_file(&dest.join(COMPANY_TOML), toml_src.as_bytes()).await?;

        let meta = BundleMeta {
            lifecycle: self.lifecycle.clone(),
            id: self.id.as_ref().to_string(),
        };
        write_file(
            &dest.join(META_JSON),
            serde_json::to_string(&meta)?.as_bytes(),
        )
        .await?;

        write_file(&dest.join(LEDGER_JSONL), jsonl(&self.ledger)?.as_bytes()).await?;
        write_file(&dest.join(EVENTS_JSONL), jsonl(&self.events)?.as_bytes()).await?;

        let memory_dir = dest.join(MEMORY_DIR);
        create_dir(&memory_dir).await?;
        write_file(
            &memory_dir.join(TRACES_JSONL),
            jsonl(&self.traces)?.as_bytes(),
        )
        .await?;

        let context_dir = dest.join(CONTEXT_DIR);
        let blobs_dir = context_dir.join(CONTEXT_BLOBS_DIR);
        create_dir(&blobs_dir).await?;
        let index: Vec<IndexEntry> = self
            .context
            .iter()
            .map(|c| IndexEntry {
                addr: c.addr.clone(),
                label: c.label.clone(),
                len: c.body.len(),
            })
            .collect();
        write_file(
            &context_dir.join(CONTEXT_INDEX_JSONL),
            jsonl(&index)?.as_bytes(),
        )
        .await?;
        for chunk in &self.context {
            write_file(&blobs_dir.join(&chunk.addr), chunk.body.as_bytes()).await?;
        }
        Ok(())
    }

    /// Reads a bundle directory (the inverse of [`Self::write_to_dir`]).
    async fn read_from_dir(src: &Path) -> Result<Self> {
        let toml_path = src.join(COMPANY_TOML);
        let toml_src = read_to_string(&toml_path).await?;
        let manifest: CompanyManifest = toml::from_str(&toml_src)
            .map_err(|e| OpenCompanyError::Store(format!("invalid {COMPANY_TOML}: {e}")))?;

        let meta: BundleMeta = serde_json::from_str(&read_to_string(&src.join(META_JSON)).await?)?;

        let ledger = read_jsonl::<LedgerEntry>(&src.join(LEDGER_JSONL)).await?;
        let events = read_jsonl::<StoredEvent>(&src.join(EVENTS_JSONL)).await?;
        let traces =
            read_jsonl::<CompressedTrace>(&src.join(MEMORY_DIR).join(TRACES_JSONL)).await?;

        let context_dir = src.join(CONTEXT_DIR);
        let index = read_jsonl::<IndexEntry>(&context_dir.join(CONTEXT_INDEX_JSONL)).await?;
        let blobs_dir = context_dir.join(CONTEXT_BLOBS_DIR);
        let mut context = Vec::with_capacity(index.len());
        for entry in index {
            let body = read_to_string(&blobs_dir.join(&entry.addr)).await?;
            context.push(ExportedChunk {
                addr: entry.addr,
                label: entry.label,
                body,
            });
        }

        Ok(Self {
            id: CompanyId::new(meta.id),
            manifest,
            lifecycle: meta.lifecycle,
            ledger,
            events,
            traces,
            context,
        })
    }
}

/// Exports `id`'s complete state through the ports into an unpacked bundle
/// directory at `dest`.
///
/// Total by construction: every port is drained (`read_from(0, MAX)`,
/// `recent_traces(MAX)`, `list("")` + `peek`), so an export never depends on a
/// backend's private on-disk shape. When [`ExportOpts::include_secrets`] is set
/// and [`ExportOpts::fs_bundle`] points at the source fs bundle, the fs-only
/// `secrets/` and `keys/` directories are copied verbatim.
pub async fn export_bundle(
    id: &CompanyId,
    dest: &Path,
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
    opts: ExportOpts,
) -> Result<()> {
    let contents = BundleContents::read_via_ports(id, store, events, memory, context).await?;
    contents.write_to_dir(dest).await?;

    if opts.include_secrets
        && let Some(src_bundle) = &opts.fs_bundle
    {
        for sub in [SECRETS_DIR, KEYS_DIR] {
            copy_dir(&src_bundle.join(sub), &dest.join(sub)).await?;
        }
    }
    Ok(())
}

/// Imports a bundle directory at `src` through the target ports, returning the
/// restored company id.
///
/// The inverse of [`export_bundle`] for the port-driven records: the manifest,
/// lifecycle, ledger, events, traces, and context are replayed through the
/// supplied ports. `secrets/`/`keys/` are fs artifacts restored separately via
/// [`restore_fs_artifacts`].
pub async fn import_bundle(
    src: &Path,
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
) -> Result<CompanyId> {
    let contents = BundleContents::read_from_dir(src).await?;
    let id = contents.id.clone();
    contents
        .write_via_ports(store, events, memory, context)
        .await?;
    Ok(id)
}

/// Copies the fs-only `secrets/` and `keys/` directories from an imported bundle
/// at `src` into the live fs bundle directory `dest_bundle_dir`, if present.
///
/// A no-op for subdirectories the bundle did not carry (the common case, since
/// they are excluded from exports by default).
pub async fn restore_fs_artifacts(src: &Path, dest_bundle_dir: &Path) -> Result<()> {
    for sub in [SECRETS_DIR, KEYS_DIR] {
        let from = src.join(sub);
        if tokio::fs::metadata(&from).await.is_ok() {
            copy_dir(&from, &dest_bundle_dir.join(sub)).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory helpers
// ---------------------------------------------------------------------------

async fn create_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| io_err(dir, e))
}

async fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| io_err(path, e))
}

async fn read_to_string(path: &Path) -> Result<String> {
    tokio::fs::read_to_string(path)
        .await
        .map_err(|e| io_err(path, e))
}

/// Serializes a slice as newline-delimited JSON (one value per line).
fn jsonl<T: Serialize>(items: &[T]) -> Result<String> {
    let mut out = String::new();
    for item in items {
        out.push_str(&serde_json::to_string(item)?);
        out.push('\n');
    }
    Ok(out)
}

/// Parses every non-empty JSONL line of `path`, skipping an absent file.
async fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(path, e)),
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

/// Recursively copies `from` into `to`. A no-op when `from` does not exist.
async fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    if tokio::fs::metadata(from).await.is_err() {
        return Ok(());
    }
    create_dir(to).await?;
    let mut entries = tokio::fs::read_dir(from)
        .await
        .map_err(|e| io_err(from, e))?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| io_err(from, e))? {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type().await.map_err(|e| io_err(&path, e))?;
        if file_type.is_dir() {
            Box::pin(copy_dir(&path, &dest)).await?;
        } else {
            tokio::fs::copy(&path, &dest)
                .await
                .map_err(|e| io_err(&path, e))?;
        }
    }
    Ok(())
}

/// Locates the bundle root under `dir`: `dir` itself when it holds a
/// `company.toml`, else the single immediate subdirectory that does (as produced
/// by [`pack_tar`], which nests the bundle under a top-level slug directory).
pub fn find_bundle_root(dir: &Path) -> Result<PathBuf> {
    if dir.join(COMPANY_TOML).is_file() {
        return Ok(dir.to_path_buf());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| io_err(dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let path = entry.path();
        if path.join(COMPANY_TOML).is_file() {
            return Ok(path);
        }
    }
    Err(OpenCompanyError::Store(format!(
        "no {COMPANY_TOML} found under {}",
        dir.display()
    )))
}

// ---------------------------------------------------------------------------
// Tar wrapper (feature `export`)
// ---------------------------------------------------------------------------

/// Packs an unpacked bundle directory into a single `.tar` at `out`.
///
/// The bundle is nested under a top-level directory named after `bundle_dir`, so
/// [`unpack_tar`] followed by [`find_bundle_root`] recovers it unambiguously.
#[cfg(feature = "export")]
pub fn pack_tar(bundle_dir: &Path, out: &Path) -> Result<()> {
    let file = std::fs::File::create(out).map_err(|e| io_err(out, e))?;
    let mut builder = tar::Builder::new(file);
    let top = bundle_dir
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from("bundle"));
    builder
        .append_dir_all(&top, bundle_dir)
        .map_err(|e| io_err(bundle_dir, e))?;
    builder.finish().map_err(|e| io_err(out, e))?;
    Ok(())
}

/// Unpacks a `.tar` produced by [`pack_tar`] into `dest`.
#[cfg(feature = "export")]
pub fn unpack_tar(tar_path: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(|e| io_err(dest, e))?;
    let file = std::fs::File::open(tar_path).map_err(|e| io_err(tar_path, e))?;
    let mut archive = tar::Archive::new(file);
    archive.unpack(dest).map_err(|e| io_err(dest, e))?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::SecretStore;
    use crate::ports::types::{Actor, ActorKind, CompanyEvent};
    use crate::runtime::RuntimeBuilder;
    use crate::store::paths::Bundle;
    use crate::store::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore, FsSecretStore};

    fn tmp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "opencompany-export-{tag}-{}-{}",
            std::process::id(),
            crate::ports::now_millis()
        ))
    }

    fn manifest() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Export Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [policy]
            mode = "supervised"
        "#;
        toml::from_str(toml_src).expect("parse manifest")
    }

    fn fs_ports(root: &Path) -> Ports {
        (
            Arc::new(FsCompanyStore::new(root.to_path_buf())),
            Arc::new(FsEventLog::new(root.to_path_buf())),
            Arc::new(FsMemoryStore::new(root.to_path_buf())),
            Arc::new(FsContextStore::new(root.to_path_buf())),
        )
    }

    /// The mandatory end-to-end round-trip: build a company, run a cycle to
    /// populate events/traces/ledger, seed a ledger entry and context chunk,
    /// export to a bundle directory, import into a *fresh* home through the fs
    /// ports, and assert the charter + event log + ledger survive intact.
    #[tokio::test]
    async fn export_import_roundtrip_fs() {
        let home1 = tmp_root("src");
        let home2 = tmp_root("dst");
        let dest = tmp_root("bundle");

        // Build + populate the source company.
        let runtime = RuntimeBuilder::fs_defaults(home1.clone(), manifest())
            .await
            .expect("build");
        let id = runtime.id().clone();
        runtime
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "kick off".into(),
                by: None,
                chat: None,
            }])
            .await
            .expect("cycle");

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.append_ledger(
            &id,
            LedgerEntry {
                at_millis: 42,
                kind: "inference.spend".into(),
                amount_usd: 1.25,
                memo: "seed".into(),
            },
        )
        .await
        .unwrap();
        c1.put(
            &id,
            ContextChunk {
                label: "notes/intro".into(),
                body: "the quick brown fox".into(),
            },
        )
        .await
        .unwrap();

        // Snapshot the source state through the ports for later comparison.
        let src_record = s1.load(&id).await.unwrap().unwrap();
        let src_events = e1
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap();
        assert!(!src_events.is_empty(), "cycle should log the input event");

        // Export → import into a fresh home.
        export_bundle(
            &id,
            &dest,
            s1.clone(),
            e1.clone(),
            m1.clone(),
            c1.clone(),
            ExportOpts::default(),
        )
        .await
        .expect("export");

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let imported_id = import_bundle(&dest, s2.clone(), e2.clone(), m2.clone(), c2.clone())
            .await
            .expect("import");
        assert_eq!(imported_id, id, "id preserved through the bundle");

        // Charter + lifecycle identical.
        let dst_record = s2.load(&id).await.unwrap().expect("imported record");
        assert_eq!(
            dst_record.manifest.company.name,
            src_record.manifest.company.name
        );
        assert_eq!(dst_record.manifest.company.name, "Export Co");
        assert_eq!(dst_record.lifecycle, src_record.lifecycle);

        // Ledger byte-identical (entries carry their original timestamps).
        assert_eq!(dst_record.ledger, src_record.ledger);
        assert!(dst_record.ledger.iter().any(|e| e.memo == "seed"));

        // Event log identical: same seqs and payloads (timestamps are re-stamped
        // on append, so compare seq + event only).
        let dst_events = e2
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap();
        assert_eq!(dst_events.len(), src_events.len());
        for (a, b) in src_events.iter().zip(dst_events.iter()) {
            assert_eq!(a.seq, b.seq);
            assert_eq!(a.event, b.event);
        }

        // Traces + context round-trip through the ports.
        let src_traces = m1.recent_traces(&id, usize::MAX).await.unwrap();
        let dst_traces = m2.recent_traces(&id, usize::MAX).await.unwrap();
        assert_eq!(src_traces, dst_traces);
        let chunk = c2.list(&id, "notes/").await.unwrap();
        assert_eq!(chunk.len(), 1);
        assert_eq!(
            c2.peek(&id, &chunk[0].addr, None).await.unwrap(),
            "the quick brown fox"
        );

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// Secrets and keys are excluded from an export by default and only appear
    /// when `include_secrets` is set with a source bundle.
    #[tokio::test]
    async fn secrets_excluded_by_default() {
        let home = tmp_root("sec-home");
        let runtime = RuntimeBuilder::fs_defaults(home.clone(), manifest())
            .await
            .expect("build");
        let id = runtime.id().clone();

        // Seed a secret and a key file in the source fs bundle.
        let secrets = FsSecretStore::new(home.clone());
        secrets
            .set(
                &id,
                "github_token",
                crate::ports::SecretValue("ghp_x".into()),
            )
            .await
            .unwrap();
        let bundle = Bundle::new(home.clone(), &id);
        tokio::fs::write(bundle.agent_key(), b"seed-bytes")
            .await
            .unwrap();

        let (s, e, m, c) = fs_ports(&home);

        // Default: no secrets/ or keys/ in the export.
        let plain = tmp_root("sec-plain");
        export_bundle(
            &id,
            &plain,
            s.clone(),
            e.clone(),
            m.clone(),
            c.clone(),
            ExportOpts::default(),
        )
        .await
        .unwrap();
        assert!(
            !plain.join(SECRETS_DIR).exists(),
            "secrets leaked by default"
        );
        assert!(!plain.join(KEYS_DIR).exists(), "keys leaked by default");

        // With include_secrets + a source bundle: both are copied.
        let withsec = tmp_root("sec-with");
        export_bundle(
            &id,
            &withsec,
            s,
            e,
            m,
            c,
            ExportOpts {
                include_secrets: true,
                fs_bundle: Some(bundle.dir().to_path_buf()),
            },
        )
        .await
        .unwrap();
        assert!(withsec.join(SECRETS_DIR).exists(), "secrets not included");
        assert!(
            withsec.join(KEYS_DIR).join("agent.ed25519").exists(),
            "key not included"
        );

        for dir in [home, plain, withsec] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// A `LifecycleChanged` event survives an export/import round-trip, proving
    /// the closed event enum tunnels through the bundle intact.
    #[tokio::test]
    async fn lifecycle_event_survives_roundtrip() {
        let home1 = tmp_root("lc-src");
        let home2 = tmp_root("lc-dst");
        let dest = tmp_root("lc-bundle");
        let id = CompanyId::new("lc-co");

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "paused".into(),
            overlay_agents: Vec::new(),
        })
        .await
        .unwrap();
        e1.append(
            &id,
            CompanyEvent::LifecycleChanged {
                from: "running".into(),
                to: "paused".into(),
                by: Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                },
            },
        )
        .await
        .unwrap();

        export_bundle(&id, &dest, s1, e1, m1, c1, ExportOpts::default())
            .await
            .unwrap();
        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2.clone(), e2.clone(), m2, c2)
            .await
            .unwrap();

        let rec = s2.load(&id).await.unwrap().unwrap();
        assert_eq!(rec.lifecycle, "paused");
        let events = e2
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap();
        assert!(matches!(
            events[0].event,
            CompanyEvent::LifecycleChanged { .. }
        ));

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    #[cfg(feature = "export")]
    #[tokio::test]
    async fn tar_pack_unpack_roundtrip() {
        let home = tmp_root("tar-home");
        let runtime = RuntimeBuilder::fs_defaults(home.clone(), manifest())
            .await
            .expect("build");
        let id = runtime.id().clone();
        runtime
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                text: "hi".into(),
                by: None,
                chat: None,
            }])
            .await
            .unwrap();

        let (s, e, m, c) = fs_ports(&home);
        let bundle_dir = tmp_root("tar-bundle").join(id.as_ref());
        export_bundle(&id, &bundle_dir, s, e, m, c, ExportOpts::default())
            .await
            .unwrap();

        let tar_path = tmp_root("tar-out").join("company.tar");
        tokio::fs::create_dir_all(tar_path.parent().unwrap())
            .await
            .unwrap();
        pack_tar(&bundle_dir, &tar_path).unwrap();
        assert!(tar_path.is_file());

        let unpacked = tmp_root("tar-unpacked");
        unpack_tar(&tar_path, &unpacked).unwrap();
        let root = find_bundle_root(&unpacked).unwrap();

        // Import the unpacked bundle into a fresh home.
        let home2 = tmp_root("tar-dst");
        let (s2, e2, m2, c2) = fs_ports(&home2);
        let imported = import_bundle(&root, s2.clone(), e2, m2, c2).await.unwrap();
        assert_eq!(imported, id);
        let rec = s2.load(&id).await.unwrap().unwrap();
        assert_eq!(rec.manifest.company.name, "Export Co");

        for dir in [
            home,
            home2,
            bundle_dir.parent().unwrap().to_path_buf(),
            tar_path.parent().unwrap().to_path_buf(),
            unpacked,
        ] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }
}
