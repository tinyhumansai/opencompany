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
use crate::ports::facts::{FactRecord, FactStore};
use crate::ports::memory::MemoryStore;
use crate::ports::store::CompanyStore;
use crate::ports::types::{
    AgentOverride, BudgetOverride, CompanyEvent, CompanyId, CompanyRecord, CompressedTrace,
    ContextChunk, EventSeq, LedgerEntry, OverlayAgent, OverlayDesk, OverlayDeskMember,
    OverlayDeskOrder, OverlayWorkflow, PolicyOverride, StoredEvent, TemplateProvenance,
    ToolGrantsOverride,
};
use crate::store::select::MemoryScopes;

/// Canonical bundle file and directory names, matching the fs
/// [`Bundle`](crate::store::paths::Bundle) layout.
const COMPANY_TOML: &str = "company.toml";
const META_JSON: &str = "meta.json";
const EVENTS_JSONL: &str = "events.jsonl";
const LEDGER_JSONL: &str = "ledger.jsonl";
const MEMORY_DIR: &str = "memory";
const TRACES_JSONL: &str = "traces.jsonl";
const ARCHIVES_JSONL: &str = "archives.jsonl";
/// Operator facts, at the bundle ROOT — the same place the live fs bundle
/// keeps them (`paths::Bundle::facts_jsonl`), so an export stays diffable
/// against a live home and a direct reader finds them where the canonical
/// layout says. Absent from bundles written before facts joined the export;
/// `read_jsonl` treats an absent file as empty, so both directions stay
/// compatible — an old importer ignores the new file, a new importer accepts
/// an old bundle.
const FACTS_JSONL: &str = "facts.jsonl";
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
/// slug, plus the source-template provenance so a template-launched company keeps
/// its provenance across an export/import round-trip. The fs [`CompanyStore`]
/// reads only `lifecycle`; the extra fields are ignored there (serde skips
/// unknown fields).
#[derive(Serialize, Deserialize)]
struct BundleMeta {
    lifecycle: String,
    id: String,
    /// The operator team overlay — teammates the operator added that the
    /// version-controlled manifest does not know about. Preserved across the
    /// bundle round-trip so an export→import keeps the operator-added roster.
    /// `#[serde(default)]` loads bundles written before this field existed as an
    /// empty overlay.
    #[serde(default)]
    overlay_agents: Vec<OverlayAgent>,
    /// The operator desk-membership overlay. Preserved so operator-added desk
    /// memberships survive an export→import. `#[serde(default)]` for back-compat
    /// with older bundles.
    #[serde(default)]
    overlay_desk_members: Vec<OverlayDeskMember>,
    /// The operator per-desk member-ordering overlay. Preserved across the bundle
    /// round-trip so an export→import keeps the operator-defined desk hierarchy
    /// (and therefore the routing lead). `#[serde(default)]` loads bundles written
    /// before this field existed as an empty order.
    #[serde(default)]
    overlay_desk_order: Vec<OverlayDeskOrder>,
    /// The operator-created desk overlay. Preserved so operator-created desks
    /// survive an export→import. `#[serde(default)]` for back-compat with older
    /// bundles.
    #[serde(default)]
    overlay_desks: Vec<OverlayDesk>,
    /// The operator workflow-authoring overlay — graph bodies created from the
    /// console or the orchestrator tool, which live on the record (never in the
    /// read-only source tree). Preserved so console-created workflows survive an
    /// export→import instead of being silently dropped. `#[serde(default)]` for
    /// back-compat with older bundles.
    #[serde(default)]
    overlay_workflows: Vec<OverlayWorkflow>,
    /// The operator-set per-teammate daily spend caps (issue #343). Preserved so
    /// an export→import keeps the caps an operator set from the console, rather
    /// than silently reverting every teammate to its manifest default.
    /// `#[serde(default)]` for back-compat with older bundles.
    #[serde(default)]
    overlay_budgets: Vec<BudgetOverride>,
    /// The operator's edits of manifest-declared teammates at export time.
    /// Preserved so an export→import keeps the roster the operator shaped from
    /// the console, rather than silently reverting every blueprint teammate to
    /// the name, role, instructions and scope `company.toml` declared.
    /// `#[serde(default)]` for back-compat with older bundles.
    #[serde(default)]
    overlay_agent_edits: Vec<AgentOverride>,
    /// The ids of manifest teammates removed from the console at export time.
    /// Preserved so an import does not silently restore a teammate the operator
    /// retired — the blueprint still declares it, so without the tombstone it
    /// comes straight back. `#[serde(default)]` for back-compat with older
    /// bundles.
    #[serde(default)]
    overlay_retired_agents: Vec<String>,
    /// The operator's `[policy]` override at export time (issue #562).
    /// `#[serde(default)]` for back-compat with older bundles, which read as
    /// `None` — the manifest's `[policy]` decides, exactly as before.
    #[serde(default)]
    overlay_policy: Option<PolicyOverride>,
    /// The operator's console-added `[tools].allow` grants at export time
    /// (issue #1796). Preserved so an export→import does not silently revoke an
    /// integration the operator granted from a connect surface, leaving the
    /// restored company "Connected" and reaching nobody. `#[serde(default)]`
    /// for back-compat with older bundles, which read as `None`: the manifest's
    /// `[tools]` decides, exactly as before.
    ///
    /// Carries the **seed's** list beside it, not the record's materialised one
    /// — see `read_via_ports` for why the bundle's `company.toml` must not name
    /// what the console added.
    #[serde(default)]
    overlay_tool_grants: Option<ToolGrantsOverride>,
    /// The operator-set per-desk tool ceilings at export time. Preserved so an
    /// export→import does not silently widen a desk back to the company's full
    /// grant — the same class of loss `overlay_policy` above is carried to
    /// prevent, on the axis that decides capability rather than autonomy.
    /// `#[serde(default)]` for back-compat with older bundles, which read as
    /// empty: the manifest's ceilings decide, exactly as before.
    #[serde(default)]
    overlay_desk_tools: std::collections::BTreeMap<String, Vec<String>>,
    /// The workflow ids switched off at export time (issue #276). Preserved so
    /// an export→import does not silently re-arm a schedule the operator had
    /// paused — which is the one direction this bundle must never move on its
    /// own. `#[serde(default)]` for back-compat with older bundles.
    #[serde(default)]
    disabled_workflows: Vec<String>,
    /// The source-template provenance, when the exported company carried one.
    /// `#[serde(default)]` keeps older bundles written before provenance existed
    /// importing cleanly (they decode to `None` — no migration).
    #[serde(default)]
    template_provenance: Option<TemplateProvenance>,
    /// What the operator told first-run setup about their business, carried
    /// through the bundle so an export→import keeps it — Phase 2 builds
    /// workflows from these answers, and a company that lost them on a restore
    /// would be asked to describe itself twice.
    /// `#[serde(default)]` keeps older bundles importing cleanly.
    #[serde(default)]
    setup: Option<crate::company::setup::SetupAnswers>,
    /// Whether the operator had confirmed the company's display name at export
    /// time (issue #1843). Preserved so an export→import does not silently
    /// re-open a confirmation step the operator already cleared.
    /// `#[serde(default)]` keeps older bundles importing cleanly (they decode
    /// to `false`, the pre-#1843 behaviour every such bundle already had).
    #[serde(default)]
    name_confirmed: bool,
    /// Epoch-millis the activation funnel completed at export time
    /// (issue #1843). Preserved for the same reason `overlay_policy` and
    /// `disabled_workflows` above are: without this, an export→import would
    /// silently re-gate an already-activated company behind onboarding.
    /// `#[serde(default)]` keeps older bundles importing cleanly (`None`).
    #[serde(default)]
    activation_completed_at: Option<u64>,
    /// Whether the source company had ever been saved by activation-aware
    /// code at export time (PR #1875 review finding). Preserved so import
    /// does not silently stamp a legacy pre-#1843 company — one whose gate
    /// was never seen — as activation-aware, which would block
    /// `RuntimeBuilder::build`'s grandfather back-fill on the very next boot
    /// and show an established operator the fresh-company onboarding gate.
    /// `#[serde(default)]` reads a bundle written before this field existed
    /// as `false`: exactly the legacy state such a bundle actually has.
    #[serde(default)]
    activation_gate_seen: bool,
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
    template_provenance: Option<TemplateProvenance>,
    setup: Option<crate::company::setup::SetupAnswers>,
    ledger: Vec<LedgerEntry>,
    events: Vec<StoredEvent>,
    traces: Vec<CompressedTrace>,
    /// Traces retained in a provider's archive tier. Empty for base stores and
    /// bundles written before archive export was introduced.
    archived_traces: Vec<CompressedTrace>,
    /// Operator facts. Empty when the source served no fact port (an old
    /// bundle, or an export run without one) — never a failure.
    facts: Vec<FactRecord>,
    context: Vec<ExportedChunk>,
    /// The operator team overlay (operator-added teammates), carried through the
    /// bundle so export→import preserves the operator roster.
    overlay_agents: Vec<OverlayAgent>,
    /// The operator desk-membership overlay, carried through the bundle so
    /// export→import preserves operator-added desk memberships.
    overlay_desk_members: Vec<OverlayDeskMember>,
    /// The operator per-desk member-ordering overlay, carried through the bundle
    /// so export→import preserves the desk hierarchy (and routing lead).
    overlay_desk_order: Vec<OverlayDeskOrder>,
    /// The operator-created desk overlay, carried through the bundle so
    /// export→import preserves operator-created desks.
    overlay_desks: Vec<OverlayDesk>,
    /// The operator workflow-authoring overlay, carried through the bundle so
    /// export→import preserves console-created workflow graphs.
    overlay_workflows: Vec<OverlayWorkflow>,
    /// The operator-set per-teammate daily spend caps, carried through the
    /// bundle so export→import preserves console-set budgets (issue #343).
    overlay_budgets: Vec<BudgetOverride>,
    /// The operator's edits of manifest-declared teammates, carried through the
    /// bundle so export→import preserves a console-shaped roster.
    overlay_agent_edits: Vec<AgentOverride>,
    /// The ids of manifest teammates the operator removed, carried through the
    /// bundle so an import does not restore them.
    overlay_retired_agents: Vec<String>,
    /// The operator's `[policy]` override, carried through the bundle so
    /// export→import preserves a console-set autonomy tier (issue #562).
    ///
    /// Without this an exported company would come back on the manifest's tier,
    /// silently re-tightening (or re-loosening) the approval gate on import —
    /// the same class of loss #343 fixed for spend caps.
    overlay_policy: Option<PolicyOverride>,
    /// The operator's console-added `[tools].allow` grants, carried through the
    /// bundle so export→import preserves an integration granted from a connect
    /// surface (rather than restoring it "Connected" and reaching nobody).
    overlay_tool_grants: Option<ToolGrantsOverride>,
    /// The operator-set per-desk tool ceilings, carried through the bundle so
    /// export→import preserves a console-narrowed department (rather than
    /// restoring it at the company's full grant).
    overlay_desk_tools: std::collections::BTreeMap<String, Vec<String>>,
    /// The workflow ids switched off, carried through the bundle so an import
    /// restores a paused workflow paused (issue #276).
    disabled_workflows: Vec<String>,
    /// Whether the operator had confirmed the company's display name
    /// (issue #1843), carried through the bundle so export→import preserves
    /// it.
    name_confirmed: bool,
    /// Epoch-millis the activation funnel completed (issue #1843), carried
    /// through the bundle so export→import does not silently re-gate an
    /// already-activated company behind onboarding.
    activation_completed_at: Option<u64>,
    /// Whether the source company had ever been saved by activation-aware
    /// code (PR #1875 review finding), carried through the bundle so import
    /// restores a legacy pre-#1843 company with its gate still unseen —
    /// otherwise `write_via_ports`'s save would stamp it seen on arrival and
    /// permanently block the grandfather back-fill for that company.
    activation_gate_seen: bool,
}

impl BundleContents {
    /// Reads the complete company state through the four durable ports.
    async fn read_via_ports(
        id: &CompanyId,
        store: Arc<dyn CompanyStore>,
        events: Arc<dyn EventLog>,
        memory: Arc<dyn MemoryStore>,
        context: Arc<dyn ContextStore>,
        facts: Option<Arc<dyn FactStore>>,
        scopes: Option<Arc<dyn MemoryScopes>>,
    ) -> Result<Self> {
        let record = store
            .load(id)
            .await?
            .ok_or_else(|| OpenCompanyError::CompanyNotFound(id.to_string()))?;
        // PR #1875 review finding: read alongside the record, not derived
        // from it — `CompanyRecord` carries no such field, only the store
        // does (see `CompanyStore::activation_gate_seen`'s doc comment).
        let activation_gate_seen = store.activation_gate_seen(id).await?;

        // Issue #358: the withdrawn half of a discussion never reaches the
        // bundle. This is the load-bearing half of that issue — hiding a
        // message on the console while the bundle keeps carrying it makes the
        // record *portable* instead of merely permanent, which is the worse
        // failure of the two.
        let events =
            scrub_redacted_discussion(events.read_from(id, EventSeq::new(0), usize::MAX).await?);
        let traces = memory.recent_traces(id, usize::MAX).await?;
        let archived_traces = match scopes {
            Some(scopes) => scopes.archived_traces(id).await?,
            None => Vec::new(),
        };
        let facts = match facts {
            Some(port) => port.list(id, None, None).await?,
            None => Vec::new(),
        };

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

        // Issue #1796: the bundle carries the **seed's** `[tools].allow`, not the
        // record's materialised one.
        //
        // `write_to_dir` serializes this manifest straight into the bundle's
        // `company.toml`, and that file BECOMES THE SEED for whatever host
        // serves the restored company. Writing the folded list there would hand
        // the next rebuild a seed that already grants `chargebee`, the carry
        // rule would correctly read that as "version control spoke" and drop the
        // override — and the console grant would have been silently promoted to
        // a manifest grant: attribution gone, and `DELETE …/tools/grants` unable
        // to reach it ever again. The override rides the bundle beside it, and
        // `restore_via_ports` re-folds, so the restored record is materialised
        // exactly as the builder would leave it.
        let mut manifest = record.manifest;
        manifest.tools.allow = crate::ports::types::seed_tool_allow(
            &manifest.tools.allow,
            record.overlay_tool_grants.as_ref(),
        );

        Ok(Self {
            id: id.clone(),
            manifest,
            lifecycle: record.lifecycle,
            template_provenance: record.template_provenance,
            setup: record.setup,
            ledger: record.ledger,
            events,
            traces,
            archived_traces,
            facts,
            context: chunks,
            overlay_agents: record.overlay_agents,
            overlay_desk_members: record.overlay_desk_members,
            overlay_desk_order: record.overlay_desk_order,
            overlay_desks: record.overlay_desks,
            overlay_workflows: record.overlay_workflows,
            overlay_budgets: record.overlay_budgets,
            overlay_agent_edits: record.overlay_agent_edits,
            overlay_retired_agents: record.overlay_retired_agents,
            overlay_policy: record.overlay_policy,
            overlay_tool_grants: record.overlay_tool_grants,
            overlay_desk_tools: record.overlay_desk_tools,
            disabled_workflows: record.disabled_workflows,
            name_confirmed: record.name_confirmed,
            activation_completed_at: record.activation_completed_at,
            activation_gate_seen,
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
        facts: Option<Arc<dyn FactStore>>,
        scopes: Option<Arc<dyn MemoryScopes>>,
    ) -> Result<()> {
        // Archived traces must remain in their recovery tier. Refuse before any
        // append-only writes when the import target cannot restore that tier.
        if !self.archived_traces.is_empty() && scopes.is_none() {
            return Err(OpenCompanyError::Store(format!(
                "bundle carries {} archived traces but the import target serves no archive tier",
                self.archived_traces.len()
            )));
        }
        // append-only, so a refusal after `store.save`/`append` would leave a
        // half-imported company whose retry duplicates history.
        if facts.is_none() && !self.facts.is_empty() {
            return Err(OpenCompanyError::Store(format!(
                "bundle carries {} operator facts but the import target serves no fact port",
                self.facts.len()
            )));
        }
        // Facts land FIRST, for the same append-only reason the refusal above
        // fires first: `upsert` is idempotent, so a failure here leaves a
        // retry-safe state — whereas a fact failure AFTER `store.save` and the
        // ledger/event appends would leave a half-import whose retry
        // duplicates history.
        if let Some(port) = &facts {
            for fact in &self.facts {
                port.upsert(&self.id, fact).await?;
            }
        }
        if let Some(scopes) = scopes {
            scopes
                .restore_archived_traces(&self.id, &self.archived_traces)
                .await?;
        }
        // The manifest + lifecycle; ledger is appended separately so the store's
        // append-only ledger stays authoritative.
        // The mirror of the strip in `read_via_ports`: the bundle holds the seed,
        // so the record written here is re-folded. Without it a restored company
        // would report its console grants as ungranted — and every reader of
        // `[tools].allow` would agree with that — until its first rebuild.
        let mut manifest = self.manifest.clone();
        manifest.tools.allow = crate::ports::types::effective_tool_allow(
            &manifest.tools.allow,
            self.overlay_tool_grants.as_ref(),
        );
        // `save_importing`, not `save`: this call is replaying a bundle's
        // prior state rather than a normal activation-aware write, so the
        // gate marker must land as `self.activation_gate_seen` — `false` for
        // a legacy pre-#1843 bundle — instead of unconditionally `true`
        // (PR #1875 review finding; see `CompanyStore::save_importing`'s doc
        // comment for the full reasoning).
        store
            .save_importing(
                &CompanyRecord {
                    overlay_agent_edits: self.overlay_agent_edits.clone(),
                    overlay_retired_agents: self.overlay_retired_agents.clone(),
                    id: self.id.clone(),
                    manifest,
                    ledger: Vec::new(),
                    lifecycle: self.lifecycle.clone(),
                    overlay_agents: self.overlay_agents.clone(),
                    overlay_desk_members: self.overlay_desk_members.clone(),
                    overlay_desk_order: self.overlay_desk_order.clone(),
                    overlay_desks: self.overlay_desks.clone(),
                    overlay_workflows: self.overlay_workflows.clone(),
                    overlay_budgets: self.overlay_budgets.clone(),
                    overlay_policy: self.overlay_policy.clone(),
                    overlay_tool_grants: self.overlay_tool_grants.clone(),
                    overlay_desk_tools: self.overlay_desk_tools.clone(),
                    disabled_workflows: self.disabled_workflows.clone(),
                    template_provenance: self.template_provenance.clone(),
                    setup: self.setup.clone(),
                    name_confirmed: self.name_confirmed,
                    activation_completed_at: self.activation_completed_at,
                    // Bundle export/import never carries a creation timestamp
                    // through (`BundleMeta`/`BundleContents` have no
                    // `created_at_millis` field) — `None` here matches every
                    // other `CompanyRecord` this module constructs.
                    created_at_millis: None,
                },
                self.activation_gate_seen,
            )
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
            overlay_agents: self.overlay_agents.clone(),
            overlay_desk_members: self.overlay_desk_members.clone(),
            overlay_desk_order: self.overlay_desk_order.clone(),
            overlay_desks: self.overlay_desks.clone(),
            overlay_workflows: self.overlay_workflows.clone(),
            overlay_budgets: self.overlay_budgets.clone(),
            overlay_agent_edits: self.overlay_agent_edits.clone(),
            overlay_retired_agents: self.overlay_retired_agents.clone(),
            overlay_policy: self.overlay_policy.clone(),
            overlay_tool_grants: self.overlay_tool_grants.clone(),
            overlay_desk_tools: self.overlay_desk_tools.clone(),
            disabled_workflows: self.disabled_workflows.clone(),
            template_provenance: self.template_provenance.clone(),
            setup: self.setup.clone(),
            name_confirmed: self.name_confirmed,
            activation_completed_at: self.activation_completed_at,
            activation_gate_seen: self.activation_gate_seen,
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
        if !self.archived_traces.is_empty() {
            write_file(
                &memory_dir.join(ARCHIVES_JSONL),
                jsonl(&self.archived_traces)?.as_bytes(),
            )
            .await?;
        } else {
            match tokio::fs::remove_file(memory_dir.join(ARCHIVES_JSONL)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(OpenCompanyError::Store(format!(
                        "cannot remove a stale archive file from the bundle: {e}"
                    )));
                }
            }
        }
        // Only when there are any: an empty file would make every new export
        // differ from an old host's byte-for-byte for no information. At the
        // bundle root, matching `paths::Bundle::facts_jsonl`. A factless
        // export must also REMOVE a stale file a previous export left in the
        // same directory — otherwise a later import resurrects facts that are
        // absent from the selected source.
        if !self.facts.is_empty() {
            write_file(&dest.join(FACTS_JSONL), jsonl(&self.facts)?.as_bytes()).await?;
        } else {
            match tokio::fs::remove_file(dest.join(FACTS_JSONL)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(OpenCompanyError::Store(format!(
                        "cannot remove a stale facts file from the bundle: {e}"
                    )));
                }
            }
        }

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

        // A bundle is the one place `overlay_budgets` arrives from outside this
        // process, so it is the one place the "at most one override per teammate"
        // invariant can be violated by data we did not write. Refuse rather than
        // resolve: `CompanyRecord::effective_budget` reads the first match, so
        // importing two rows for one teammate would apply whichever the bundle
        // happened to serialize first — possibly the obsolete one, possibly the
        // looser one, and with somebody else's name on the attribution. A bundle
        // that disagrees with itself about a spend cap has no right answer to
        // pick, and picking silently is how a revoked allowance comes back.
        if let Some(agent_id) = BudgetOverride::duplicate_agent_id(&meta.overlay_budgets) {
            return Err(OpenCompanyError::Store(format!(
                "invalid {META_JSON}: {} carries more than one budget override for teammate \
                 '{agent_id}'; at most one is allowed",
                meta.id
            )));
        }
        // The roster edits carry the same invariant for the same reason, and are
        // checked in the same breath: `CompanyRecord::agent_override` also reads
        // the first match, so two rows for one teammate would apply whichever the
        // bundle happened to serialize first — restoring a name the operator
        // changed, or a tool grant they narrowed, with nothing to say which row
        // won. Both refusals fire before any port is written, so a rejected
        // bundle leaves the target untouched.
        if let Some(agent_id) = AgentOverride::duplicate_agent_id(&meta.overlay_agent_edits) {
            return Err(OpenCompanyError::Store(format!(
                "invalid {META_JSON}: {} carries more than one edit for teammate \
                 '{agent_id}'; at most one is allowed",
                meta.id
            )));
        }

        let ledger = read_jsonl::<LedgerEntry>(&src.join(LEDGER_JSONL)).await?;
        // Scrubbed on the way IN as well as on the way out (issue #358), which
        // is not belt-and-braces: a bundle written by a host that predates this
        // carries the withdrawn text beside its tombstone, and importing it
        // as-is would write that text into a fresh journal — the resurrection
        // the issue names, arriving through the one door the exporter cannot
        // guard.
        let events =
            scrub_redacted_discussion(read_jsonl::<StoredEvent>(&src.join(EVENTS_JSONL)).await?);
        let traces =
            read_jsonl::<CompressedTrace>(&src.join(MEMORY_DIR).join(TRACES_JSONL)).await?;
        let archived_traces =
            read_jsonl::<CompressedTrace>(&src.join(MEMORY_DIR).join(ARCHIVES_JSONL)).await?;
        // Absent on bundles that predate facts-in-the-bundle: empty, not an error.
        let facts = read_jsonl::<FactRecord>(&src.join(FACTS_JSONL)).await?;

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
            template_provenance: meta.template_provenance,
            setup: meta.setup,
            ledger,
            events,
            traces,
            archived_traces,
            facts,
            context,
            overlay_agents: meta.overlay_agents,
            overlay_desk_members: meta.overlay_desk_members,
            overlay_desk_order: meta.overlay_desk_order,
            overlay_desks: meta.overlay_desks,
            overlay_workflows: meta.overlay_workflows,
            overlay_budgets: meta.overlay_budgets,
            overlay_agent_edits: meta.overlay_agent_edits,
            overlay_retired_agents: meta.overlay_retired_agents,
            overlay_policy: meta.overlay_policy,
            overlay_tool_grants: meta.overlay_tool_grants,
            overlay_desk_tools: meta.overlay_desk_tools,
            disabled_workflows: meta.disabled_workflows,
            name_confirmed: meta.name_confirmed,
            activation_completed_at: meta.activation_completed_at,
            activation_gate_seen: meta.activation_gate_seen,
        })
    }
}

/// Replaces the text of every discussion post a later tombstone withdrew
/// (issue #358).
///
/// ## Why the bundle is where this matters most
///
/// A withdrawal that only affected the console would leave the message in
/// `events.jsonl`, and the bundle is the copy that *leaves the instance* — it
/// is handed to support, restored onto a laptop, committed to a repository. So
/// a redaction that stops at the read fold does not make a pasted credential
/// less exposed; it makes it exposed somewhere nobody is looking.
///
/// ## What it does
///
/// Walks the log once, collecting the `(task_id, seq)` pairs named by
/// [`CompanyEvent::TaskDiscussionRedacted`], then rewrites the `text` of each
/// post they name to
/// [`REDACTED_DISCUSSION_TEXT`](crate::ports::tasks::REDACTED_DISCUSSION_TEXT).
/// Two passes rather than one because a tombstone always follows its post, so a
/// single forward pass would have already written the post out.
///
/// **The tombstone itself is kept.** Dropping it would leave the imported
/// company with a post whose text is a placeholder and no record of why, and
/// the fold would show it as an ordinary message reading "This message was
/// removed." — a sentence nobody wrote. Carried through, the imported thread
/// says the same thing the exporting one did, with the same attribution.
///
/// Every other event passes through untouched, including posts with no
/// tombstone: this is a substitution, not a filter, so the log's shape,
/// ordering and sequence numbering are exactly what they were.
fn scrub_redacted_discussion(events: Vec<StoredEvent>) -> Vec<StoredEvent> {
    use std::collections::HashSet;

    let withdrawn: HashSet<(String, u64)> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            CompanyEvent::TaskDiscussionRedacted { task_id, seq, .. } => {
                Some((task_id.clone(), *seq))
            }
            _ => None,
        })
        .collect();
    if withdrawn.is_empty() {
        return events;
    }

    events
        .into_iter()
        .map(|mut stored| {
            if let CompanyEvent::TaskDiscussionPosted { task_id, text, .. } = &mut stored.event
                && withdrawn.contains(&(task_id.clone(), stored.seq.value()))
            {
                *text = crate::ports::tasks::REDACTED_DISCUSSION_TEXT.to_string();
            }
            stored
        })
        .collect()
}

/// Exports `id`'s complete state through the ports into an unpacked bundle
/// directory at `dest`.
///
/// Total by construction: every port is drained (`read_from(0, MAX)`,
/// `recent_traces(MAX)`, `list("")` + `peek`), so an export never depends on a
/// backend's private on-disk shape. When [`ExportOpts::include_secrets`] is set
/// and [`ExportOpts::fs_bundle`] points at the source fs bundle, the fs-only
/// `secrets/` and `keys/` directories are copied verbatim.
// Eight arguments is over clippy's default ceiling, taken knowingly: five of
// them are the durable ports, and folding them into a struct is a wider
// refactor than this addition warrants (the repo carries the same allow at
// its other port-heavy seams).
#[allow(clippy::too_many_arguments)]
pub async fn export_bundle(
    id: &CompanyId,
    dest: &Path,
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
    facts: Option<Arc<dyn FactStore>>,
    opts: ExportOpts,
) -> Result<()> {
    export_bundle_with_scopes(id, dest, store, events, memory, context, facts, None, opts).await
}

/// Exports a bundle while preserving an optional provider archive tier.
#[allow(clippy::too_many_arguments)]
pub async fn export_bundle_with_scopes(
    id: &CompanyId,
    dest: &Path,
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
    facts: Option<Arc<dyn FactStore>>,
    scopes: Option<Arc<dyn MemoryScopes>>,
    opts: ExportOpts,
) -> Result<()> {
    let contents =
        BundleContents::read_via_ports(id, store, events, memory, context, facts, scopes).await?;
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
    facts: Option<Arc<dyn FactStore>>,
) -> Result<CompanyId> {
    import_bundle_with_scopes(src, store, events, memory, context, facts, None).await
}

/// Imports a bundle while restoring its optional provider archive tier.
pub async fn import_bundle_with_scopes(
    src: &Path,
    store: Arc<dyn CompanyStore>,
    events: Arc<dyn EventLog>,
    memory: Arc<dyn MemoryStore>,
    context: Arc<dyn ContextStore>,
    facts: Option<Arc<dyn FactStore>>,
    scopes: Option<Arc<dyn MemoryScopes>>,
) -> Result<CompanyId> {
    let contents = BundleContents::read_from_dir(src).await?;
    let id = contents.id.clone();
    contents
        .write_via_ports(store, events, memory, context, facts, scopes)
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
///
/// **Import stays strict, by decision** (issue #387). The boot path now tolerates
/// a damaged ledger line — see
/// [`read_jsonl_lenient`](crate::store::fs::read_jsonl_lenient) — and this reader
/// deliberately does not follow it. The two are not the same situation:
///
/// * Boot has no alternative. The bundle is the company's only copy, refusing to
///   read it strands the tenant, and skipping keeps the bytes on disk for repair.
/// * Import does have one. The bundle being read is an *incoming* archive whose
///   source still exists, and refusing it costs nothing but a retry with a good
///   bundle. Half-importing instead would mint a company whose ledger silently
///   disagrees with the archive it claims to be, with no record of what was
///   dropped — an inconsistency that outlives the damaged file.
///
/// So a corrupt archive fails the import outright. That is the correct answer
/// here, and it should not be "fixed" to match the boot path.
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

    /// A minimal running company record for tests that only need one to exist.
    fn company_record(id: &CompanyId) -> CompanyRecord {
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
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
    }

    fn fs_ports(root: &Path) -> Ports {
        (
            Arc::new(FsCompanyStore::new(root.to_path_buf())),
            Arc::new(FsEventLog::new(root.to_path_buf())),
            Arc::new(FsMemoryStore::new(root.to_path_buf())),
            Arc::new(FsContextStore::new(root.to_path_buf())),
        )
    }

    struct ArchiveScopes {
        archived: Vec<CompressedTrace>,
        restored: Arc<std::sync::Mutex<Vec<CompressedTrace>>>,
        context: Arc<FsContextStore>,
    }

    #[async_trait::async_trait]
    impl MemoryScopes for ArchiveScopes {
        fn agent_context(&self, _agent_id: &str) -> Arc<dyn ContextStore> {
            self.context.clone()
        }

        fn desk_context(&self, _desk_id: &str) -> Arc<dyn ContextStore> {
            self.context.clone()
        }

        async fn archived_traces(&self, _company: &CompanyId) -> Result<Vec<CompressedTrace>> {
            Ok(self.archived.clone())
        }

        async fn restore_archived_traces(
            &self,
            _company: &CompanyId,
            traces: &[CompressedTrace],
        ) -> Result<()> {
            self.restored.lock().unwrap().extend_from_slice(traces);
            Ok(())
        }
    }

    #[tokio::test]
    async fn archive_traces_survive_bundle_roundtrip_in_the_archive_tier() {
        let home1 = tmp_root("archive-src");
        let home2 = tmp_root("archive-dst");
        let dest = tmp_root("archive-bundle");
        let id = CompanyId::new("archive-co");
        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&company_record(&id)).await.unwrap();
        let archived = vec![CompressedTrace {
            cycle_id: "evicted-cycle".into(),
            summary: "retained recovery trace".into(),
            at_millis: 7,
        }];
        let source_scopes = Arc::new(ArchiveScopes {
            archived: archived.clone(),
            restored: Arc::new(std::sync::Mutex::new(Vec::new())),
            context: Arc::new(FsContextStore::new(home1.clone())),
        });
        export_bundle_with_scopes(
            &id,
            &dest,
            s1,
            e1,
            m1,
            c1,
            None,
            Some(source_scopes),
            ExportOpts::default(),
        )
        .await
        .unwrap();
        assert!(dest.join(MEMORY_DIR).join(ARCHIVES_JSONL).is_file());

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let restored = Arc::new(std::sync::Mutex::new(Vec::new()));
        let target_scopes = Arc::new(ArchiveScopes {
            archived: Vec::new(),
            restored: restored.clone(),
            context: Arc::new(FsContextStore::new(home2)),
        });
        import_bundle_with_scopes(&dest, s2, e2, m2, c2, None, Some(target_scopes))
            .await
            .unwrap();
        assert_eq!(*restored.lock().unwrap(), archived);
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
                mentions: Vec::new(),
                parent: None,
                text: "kick off".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
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
            None,
            ExportOpts::default(),
        )
        .await
        .expect("export");

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let imported_id =
            import_bundle(&dest, s2.clone(), e2.clone(), m2.clone(), c2.clone(), None)
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

    /// PR #1875 review finding: importing a legacy bundle — one whose source
    /// company predates activation tracking, so `store.activation_gate_seen`
    /// answers `false` — must land in the target store still answering
    /// `false`. Before the fix, `write_via_ports` called plain `store.save`,
    /// which unconditionally stamps the marker `true`; every OTHER save
    /// really is made by activation-aware code, but import is replaying
    /// history, not writing it, so that stamp falsely marked a restored
    /// legacy company as already seen. That permanently blocks
    /// `RuntimeBuilder::build`'s pre-#1843 grandfather back-fill on the
    /// imported company's very next boot, showing an established operator
    /// the fresh-company onboarding gate.
    #[tokio::test]
    async fn import_preserves_unseen_activation_gate() {
        let home1 = tmp_root("gate-src");
        let home2 = tmp_root("gate-dst");
        let dest = tmp_root("gate-bundle");

        // A legacy company: `company.toml` on disk, no `meta.json` at all —
        // the exact shape a pre-#1843 bundle has. `FsCompanyStore::load`
        // reads a missing meta.json as `Meta::default()`
        // (`lifecycle: "running"`, no overlays), and its
        // `activation_gate_seen` reads the same absence as `false` — both
        // "never saved by activation-aware code".
        let (s1, e1, m1, c1) = fs_ports(&home1);
        let id = CompanyId::new("legacy-co");
        let bundle = Bundle::new(home1.clone(), &id);
        bundle.ensure_dirs().await.unwrap();
        tokio::fs::write(bundle.company_toml(), toml::to_string(&manifest()).unwrap())
            .await
            .unwrap();

        assert!(
            !s1.activation_gate_seen(&id).await.unwrap(),
            "fixture must start as a legacy, gate-unseen record"
        );

        export_bundle(
            &id,
            &dest,
            s1.clone(),
            e1.clone(),
            m1.clone(),
            c1.clone(),
            None,
            ExportOpts::default(),
        )
        .await
        .expect("export");

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let imported_id =
            import_bundle(&dest, s2.clone(), e2.clone(), m2.clone(), c2.clone(), None)
                .await
                .expect("import");
        assert_eq!(imported_id, id);

        assert!(
            !s2.activation_gate_seen(&id).await.unwrap(),
            "importing a legacy bundle must not stamp the activation gate as \
             seen — doing so hides an established operator's grandfather \
             back-fill behind the fresh-company onboarding gate"
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
            None,
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
            None,
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
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "paused".into(),
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

        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();
        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2.clone(), e2.clone(), m2, c2, None)
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

    /// Issue #358, the half that actually closes it: a withdrawn discussion
    /// message is not in the bundle, and an import cannot bring it back.
    ///
    /// Asserted three ways, because each is a different way to leak it:
    ///
    /// 1. the **bundle file** (`events.jsonl`) does not contain the secret —
    ///    this is the copy that leaves the instance, so grepping the bytes is
    ///    the assertion that matters most;
    /// 2. the **imported journal** carries the placeholder, not the text;
    /// 3. the **tombstone travels**, so the imported thread still reports that
    ///    a message was withdrawn rather than showing a bare placeholder that
    ///    reads like something a person typed.
    ///
    /// A post with no tombstone is untouched in the same bundle, so this is a
    /// substitution rather than a filter that eats discussion history.
    #[tokio::test]
    async fn a_withdrawn_discussion_message_does_not_survive_export_import() {
        let home1 = tmp_root("redact-src");
        let home2 = tmp_root("redact-dst");
        let dest = tmp_root("redact-bundle");
        let id = CompanyId::new("redact-co");
        const SECRET: &str = "sk-live-DO-NOT-SHIP-THIS";

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
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

        let leaked = e1
            .append(
                &id,
                CompanyEvent::TaskDiscussionPosted {
                    task_id: "t1".into(),
                    text: format!("blocked on the API key: {SECRET}"),
                    by: None,
                },
            )
            .await
            .unwrap();
        // A second post nobody withdrew, to prove the scrub is targeted.
        e1.append(
            &id,
            CompanyEvent::TaskDiscussionPosted {
                task_id: "t1".into(),
                text: "rotated it, we are unblocked".into(),
                by: None,
            },
        )
        .await
        .unwrap();
        e1.append(
            &id,
            CompanyEvent::TaskDiscussionRedacted {
                task_id: "t1".into(),
                seq: leaked.value(),
                by: Some(Actor {
                    kind: ActorKind::Operator,
                    id: "owner".into(),
                }),
            },
        )
        .await
        .unwrap();

        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();

        // 1. The bytes that leave the building.
        let shipped = tokio::fs::read_to_string(dest.join(EVENTS_JSONL))
            .await
            .unwrap();
        assert!(
            !shipped.contains(SECRET),
            "the withdrawn message shipped in the bundle: {shipped}"
        );
        assert!(
            shipped.contains(crate::ports::tasks::REDACTED_DISCUSSION_TEXT),
            "the withdrawn post is missing its placeholder: {shipped}"
        );
        assert!(
            shipped.contains("rotated it, we are unblocked"),
            "the scrub ate a post nobody withdrew: {shipped}"
        );

        // 2 and 3. What the importing instance ends up holding.
        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2, e2.clone(), m2, c2, None)
            .await
            .unwrap();
        let events = e2
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap();

        let posted: Vec<&str> = events
            .iter()
            .filter_map(|stored| match &stored.event {
                CompanyEvent::TaskDiscussionPosted { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            posted,
            vec![
                crate::ports::tasks::REDACTED_DISCUSSION_TEXT,
                "rotated it, we are unblocked"
            ],
            "the imported journal must carry the placeholder, not the secret"
        );
        assert!(
            events.iter().any(|stored| matches!(
                &stored.event,
                CompanyEvent::TaskDiscussionRedacted { task_id, seq, .. }
                    if task_id == "t1" && *seq == leaked.value()
            )),
            "the tombstone did not survive the round trip, so the imported thread \
             cannot say the message was withdrawn"
        );

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// The same guard on the way IN: a bundle written by a host that predates
    /// #358 carries the withdrawn text beside its tombstone, and importing it
    /// must not write that text into the fresh journal.
    ///
    /// Built by hand-editing the exported `events.jsonl` back to the
    /// pre-redaction bytes, which is exactly the shape such a bundle has.
    #[tokio::test]
    async fn an_old_bundle_cannot_smuggle_a_withdrawn_message_back_in() {
        let home1 = tmp_root("smuggle-src");
        let home2 = tmp_root("smuggle-dst");
        let dest = tmp_root("smuggle-bundle");
        let id = CompanyId::new("smuggle-co");
        const SECRET: &str = "sk-live-SMUGGLED";

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
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
        let leaked = e1
            .append(
                &id,
                CompanyEvent::TaskDiscussionPosted {
                    task_id: "t1".into(),
                    text: SECRET.into(),
                    by: None,
                },
            )
            .await
            .unwrap();
        e1.append(
            &id,
            CompanyEvent::TaskDiscussionRedacted {
                task_id: "t1".into(),
                seq: leaked.value(),
                by: None,
            },
        )
        .await
        .unwrap();

        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();

        // Put the secret back, as an older exporter would have written it.
        let path = dest.join(EVENTS_JSONL);
        let scrubbed = tokio::fs::read_to_string(&path).await.unwrap();
        let old_shape = scrubbed.replace(crate::ports::tasks::REDACTED_DISCUSSION_TEXT, SECRET);
        assert!(old_shape.contains(SECRET), "the fixture did not rewrite");
        tokio::fs::write(&path, old_shape).await.unwrap();

        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2, e2.clone(), m2, c2, None)
            .await
            .unwrap();
        let events = e2
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap();
        assert!(
            !events.iter().any(|stored| matches!(
                &stored.event,
                CompanyEvent::TaskDiscussionPosted { text, .. } if text.contains(SECRET)
            )),
            "an old bundle smuggled a withdrawn message into the new journal"
        );

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// Issue #85: a template-launched company's `template_provenance` survives an
    /// export → import round-trip intact (source_id, version, and path all carry
    /// through the bundle's `meta.json`), so exporting then importing never
    /// silently strips a company's origin template.
    #[tokio::test]
    async fn template_provenance_survives_roundtrip() {
        let home1 = tmp_root("prov-src");
        let home2 = tmp_root("prov-dst");
        let dest = tmp_root("prov-bundle");
        let id = CompanyId::new("prov-co");

        let provenance = TemplateProvenance {
            source_id: "agentic_law_firm".into(),
            version: None,
            path: Some("agentic_law_firm".into()),
        };

        // Register a company carrying template provenance in the source home.
        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
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
            template_provenance: Some(provenance.clone()),
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();

        // Export → import into a fresh home.
        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();
        let (s2, e2, m2, c2) = fs_ports(&home2);
        let imported = import_bundle(&dest, s2.clone(), e2, m2, c2, None)
            .await
            .unwrap();
        assert_eq!(imported, id, "id preserved through the bundle");

        // The imported record carries the identical provenance — all three fields.
        let rec = s2.load(&id).await.unwrap().expect("imported record");
        assert_eq!(
            rec.template_provenance,
            Some(provenance),
            "template provenance lost across the bundle round-trip"
        );

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// EVERY operator overlay — the team (`overlay_agents`), desk memberships
    /// (`overlay_desk_members`), the desk-order hierarchy (`overlay_desk_order`),
    /// operator-created desks (`overlay_desks`), and runtime-authored workflow
    /// graphs (`overlay_workflows`) — survives an export→import.
    /// A prior version threaded only `overlay_desk_order` through the bundle, so a
    /// round-trip silently ERASED operator-added teammates, desk memberships, and
    /// operator-created desks (data loss). This asserts all four come back intact
    /// and that the desk hierarchy still drives the routing lead.
    #[tokio::test]
    async fn operator_overlays_including_desk_order_survive_roundtrip() {
        let home1 = tmp_root("order-src");
        let home2 = tmp_root("order-dst");
        let dest = tmp_root("order-bundle");
        let id = CompanyId::new("order-co");

        // A manifest desk whose blueprint lead is `ceo`.
        let manifest: CompanyManifest = toml::from_str(
            r#"
            [company]
            name = "Order Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [[agent]]
            id = "cto"
            role = "Tech"

            [[group_chat]]
            id = "eng"
            name = "Engineering"
            members = ["ceo", "cto"]
        "#,
        )
        .expect("parse manifest");

        // Operator reorders the desk so `cto` becomes the lead — a non-empty order.
        let order = vec![OverlayDeskOrder {
            desk_id: "eng".into(),
            ordered: vec!["cto".into(), "ceo".into()],
        }];
        // An operator-added teammate, not in the manifest.
        let agents = vec![OverlayAgent {
            id: "designer".into(),
            name: "Dana Designer".into(),
            role: "Design".into(),
            description: Some("Owns the brand".into()),
            tools: None,
            model: None,
            harness: None,
        }];
        // That teammate added to the `eng` desk through the membership overlay.
        let desk_members = vec![OverlayDeskMember {
            desk_id: "eng".into(),
            agent_id: "designer".into(),
        }];
        // Operator-created desks the manifest never declared — one of each
        // responder mode, so the round-trip below (whole-vec equality) proves
        // the `auto` flag survives export→import rather than silently
        // reverting a leadless channel to a lead desk (issue #1835).
        let desks = vec![
            OverlayDesk {
                id: "growth".into(),
                name: "Growth".into(),
                description: Some("Marketing pod".into()),
                members: vec!["ceo".into()],
                responder: crate::ports::types::ResponderMode::default(),
            },
            OverlayDesk {
                id: "launch".into(),
                name: "Launch".into(),
                description: None,
                members: vec!["ceo".into(), "cto".into()],
                responder: crate::ports::types::ResponderMode::Auto,
            },
        ];
        // A workflow graph authored at runtime (issue #168). On a hosted tenant
        // this body is the ONLY copy — a bundle that dropped it would lose the
        // workflow outright.
        let workflows = vec![OverlayWorkflow {
            id: "console_flow".into(),
            toml: "id = \"console_flow\"\nname = \"Console flow\"\n\
                   [[node]]\nid = \"start\"\nkind = \"trigger\"\nname = \"Start\"\n"
                .into(),
        }];

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: manifest.clone(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
            overlay_agents: agents.clone(),
            overlay_desk_members: desk_members.clone(),
            overlay_desk_order: order.clone(),
            overlay_desks: desks.clone(),
            overlay_workflows: workflows.clone(),
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

        // Sanity: the override already flips the lead in the source.
        let src_record = s1.load(&id).await.unwrap().unwrap();
        assert_eq!(src_record.effective_desk_members("eng")[0], "cto");

        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();
        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2.clone(), e2, m2, c2, None)
            .await
            .unwrap();

        // Every overlay came across intact — not reset to an empty list.
        let dst_record = s2.load(&id).await.unwrap().unwrap();
        assert!(
            !dst_record.overlay_agents.is_empty(),
            "team overlay erased by the bundle round-trip"
        );
        assert_eq!(
            dst_record.overlay_agents, agents,
            "team overlay altered by the bundle round-trip"
        );
        assert!(
            !dst_record.overlay_desk_members.is_empty(),
            "desk-membership overlay erased by the bundle round-trip"
        );
        assert_eq!(
            dst_record.overlay_desk_members, desk_members,
            "desk-membership overlay altered by the bundle round-trip"
        );
        assert!(
            !dst_record.overlay_desk_order.is_empty(),
            "desk-order overlay erased by the bundle round-trip"
        );
        assert_eq!(
            dst_record.overlay_desk_order, order,
            "desk order overlay altered by the bundle round-trip"
        );
        assert!(
            !dst_record.overlay_desks.is_empty(),
            "operator-created desks erased by the bundle round-trip"
        );
        assert_eq!(
            dst_record.overlay_desks, desks,
            "operator-created desks altered by the bundle round-trip"
        );
        assert!(
            !dst_record.overlay_workflows.is_empty(),
            "runtime-authored workflows erased by the bundle round-trip"
        );
        assert_eq!(
            dst_record.overlay_workflows, workflows,
            "runtime-authored workflows altered by the bundle round-trip"
        );
        // And the hierarchy still drives routing: `cto` remains the lead after
        // import.
        assert_eq!(
            dst_record.effective_desk_members("eng")[0],
            "cto",
            "routing lead reverted to blueprint after import"
        );

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// A manifest naming two capped teammates, so a round-trip that dropped the
    /// overrides would fall back to real caps rather than to "uncapped" — the
    /// regression would still show as the *wrong* numbers, not as absent ones.
    fn budget_manifest() -> CompanyManifest {
        toml::from_str(
            r#"
            [company]
            name = "Budget Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"
            budget_usd_daily = 5.0

            [[agent]]
            id = "cto"
            role = "Tech"
            budget_usd_daily = 9.0

            # Carries no budget override, and exists so the retirement fixture
            # below can remove a teammate without leaving a cap behind for one
            # that is no longer on the roster — a pairing the product never
            # produces, since `remove_member` drops the override with the
            # teammate.
            [[agent]]
            id = "ops"
            role = "Operations"
        "#,
        )
        .expect("parse manifest")
    }

    fn admin_actor() -> Actor {
        Actor {
            kind: ActorKind::User,
            id: "user-admin".into(),
        }
    }

    /// Issue #343: **all three** budget states survive an export→import — not
    /// just the empty overlay every other fixture carries.
    ///
    /// The three are only distinct if serialization keeps them distinct, and two
    /// of the three collapse into each other under the obvious mistakes:
    /// `Some(0.0)` becomes `None` if the field is ever serialized with
    /// `skip_serializing_if = "is_zero"`-style cleverness, and an explicit `None`
    /// becomes "no entry at all" if the row is dropped when it carries no cap.
    /// Either collapse is a silent unrecoverable change to a spend cap: a
    /// teammate an admin muted starts spending again, or a teammate an admin
    /// deliberately uncapped inherits the manifest's cap back. Attribution is
    /// asserted alongside the cap because a restored cap nobody appears to have
    /// set is its own defect.
    #[tokio::test]
    async fn budget_overrides_survive_roundtrip_including_zero_and_explicit_none() {
        let home1 = tmp_root("budget-src");
        let home2 = tmp_root("budget-dst");
        let dest = tmp_root("budget-bundle");
        let id = CompanyId::new("budget-co");

        let budgets = vec![
            // Cap of exactly zero: "this teammate may not spend", NOT "no cap".
            BudgetOverride {
                agent_id: "ceo".into(),
                budget_usd_daily: Some(0.0),
                set_by: admin_actor(),
                at_millis: 1_700_000_000_000,
            },
            // Explicitly uncapped, beating the manifest's $9.
            BudgetOverride {
                agent_id: "cto".into(),
                budget_usd_daily: None,
                set_by: admin_actor(),
                at_millis: 1_700_000_000_001,
            },
        ];

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            id: id.clone(),
            manifest: budget_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: budgets.clone(),
            // Issue #562: a console-set tier rides the same bundle, and for the
            // same reason the paused workflow below does — a `None` here could
            // not have detected the field being dropped, and dropping it would
            // silently move an imported company's approval gate back to whatever
            // the manifest shipped with.
            overlay_policy: Some(PolicyOverride {
                mode: Some("auto".to_string()),
                always_approve: Some(vec!["payment.send".to_string()]),
                auto_approve_under_usd: Some(Some(25.0)),
                approval_ttl_hours: Some(48),
                set_by: admin_actor(),
                at_millis: 1_700_000_000_002,
            }),
            // Issue #1796: the console tool grants ride the same bundle, and
            // need it more sharply than the desk ceiling below — this is the
            // one overlay that WIDENS `[tools].allow`, so dropping it would
            // silently revoke an integration the operator granted from a
            // connect surface, leaving the imported company "Connected" and
            // reaching nobody.
            overlay_tool_grants: Some(ToolGrantsOverride {
                added: vec!["chargebee".to_string()],
                set_by: admin_actor(),
                at_millis: 1_700_000_000_003,
            }),
            // Non-empty for the same reason the tier above is: an empty map here
            // could not detect the field being dropped from the bundle, and
            // dropping it would silently restore an imported company's narrowed
            // desk at the company's full tool grant.
            overlay_desk_tools: std::collections::BTreeMap::from([(
                "research".to_string(),
                vec!["docs.*".to_string()],
            )]),
            // Issue #276: a paused workflow rides the same bundle. The empty
            // list this fixture used to carry could not have detected the field
            // being dropped — and dropping it would silently re-arm a schedule
            // an operator had switched off, which is the one direction an
            // import must never move on its own.
            disabled_workflows: vec!["digest".to_string()],
            // A console-shaped roster rides the same bundle: without the field
            // an imported company would silently come back on the blueprint's
            // names, roles and scopes, undoing every edit an operator made.
            overlay_agent_edits: vec![AgentOverride {
                agent_id: "ceo".to_string(),
                role: Some("Chief Vibes".to_string()),
                ..Default::default()
            }],
            // And a tombstone, for the sharper version of the same loss: the
            // blueprint still declares this teammate, so a bundle that dropped
            // the field would restore somebody the operator had removed.
            //
            // `ops` rather than one of the capped pair on purpose. Removing a
            // teammate drops its budget override with it, so a record holding
            // both a tombstone and a cap for the same id is a state no write
            // path can reach — a fixture that carried one would be asserting
            // that the bundle faithfully preserves something the product never
            // writes.
            overlay_retired_agents: vec!["ops".to_string()],
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();

        // Sanity: both overrides already beat the manifest in the source.
        let src_record = s1.load(&id).await.unwrap().unwrap();
        assert_eq!(src_record.effective_budget("ceo"), Some(0.0));
        assert_eq!(src_record.effective_budget("cto"), None);

        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();
        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2.clone(), e2, m2, c2, None)
            .await
            .unwrap();

        let dst_record = s2.load(&id).await.unwrap().unwrap();
        assert_eq!(
            dst_record.overlay_budgets, budgets,
            "budget overrides altered by the bundle round-trip"
        );
        assert!(
            !dst_record.workflow_enabled("digest"),
            "the bundle round-trip re-armed a paused workflow"
        );
        assert_eq!(
            dst_record
                .effective_agent("ceo")
                .expect("the roster still names the ceo")
                .role,
            "Chief Vibes",
            "the bundle round-trip restored the blueprint's role over the operator's edit"
        );
        assert!(
            dst_record.effective_agent("ops").is_none(),
            "the bundle round-trip restored a teammate the operator had removed"
        );
        // The capped pair is still on the roster, so the budget assertions below
        // are read through teammates that actually exist.
        assert!(
            dst_record.effective_agent("cto").is_some(),
            "cto was retired by accident"
        );
        // Issue #562: the console-set tier survives export→import, attribution
        // included. Without this the seeded fixture proves nothing — a bundle
        // path that dropped the field would still pass every other assertion
        // here, and an imported company would silently run the manifest's gate.
        let policy = dst_record
            .overlay_policy
            .as_ref()
            .expect("the policy override was dropped by the bundle round-trip");
        assert_eq!(policy.mode.as_deref(), Some("auto"));
        assert_eq!(
            policy.always_approve.as_deref(),
            Some(["payment.send".to_string()].as_slice())
        );
        assert_eq!(policy.auto_approve_under_usd, Some(Some(25.0)));
        assert_eq!(policy.approval_ttl_hours, Some(48));
        assert_eq!(policy.set_by, admin_actor());
        assert_eq!(policy.at_millis, 1_700_000_000_002);
        assert_eq!(
            dst_record.effective_policy().mode,
            "auto",
            "the imported company must run the tier the operator set, not the manifest's"
        );

        // Cap, attribution and timestamp, read the way every surface reads them.
        assert_eq!(
            dst_record.effective_budget("ceo"),
            Some(0.0),
            "a zero cap must survive as zero, not decay into uncapped"
        );
        assert_eq!(
            dst_record.effective_budget("cto"),
            None,
            "an explicitly-uncapped override must survive and still beat the manifest's $9"
        );
        let ceo = dst_record.budget_override("ceo").expect("ceo attribution");
        assert_eq!(ceo.set_by, admin_actor());
        assert_eq!(ceo.at_millis, 1_700_000_000_000);
        let cto = dst_record.budget_override("cto").expect(
            "an explicitly-uncapped override must keep its attribution row — it is exactly the \
             case an operator needs to see attributed",
        );
        assert_eq!(cto.set_by, admin_actor());
        assert_eq!(cto.at_millis, 1_700_000_000_001);

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// **A console tool grant must not be promoted to a seed grant by a
    /// round-trip** (issue #1796).
    ///
    /// `write_to_dir` serializes the bundle's manifest straight into
    /// `company.toml`, and that file becomes the SEED for whatever host serves
    /// the restored company. The record's manifest is materialised
    /// seed-plus-grants, so carrying it verbatim would write a seed that already
    /// grants `chargebee` — the next rebuild's carry rule would correctly read
    /// that as "version control spoke", drop the override, and the operator's
    /// attributed grant would have become a manifest grant that
    /// `DELETE …/tools/grants` can never reach again.
    ///
    /// So the bundle carries the seed and the override separately, and the
    /// restored record is re-folded. Both halves are asserted: the `company.toml`
    /// on disk must NOT name the namespace, and the imported record must.
    #[tokio::test]
    async fn a_console_tool_grant_survives_a_roundtrip_without_becoming_a_seed_grant() {
        let home1 = tmp_root("grants-src");
        let home2 = tmp_root("grants-dst");
        let dest = tmp_root("grants-bundle");
        let id = CompanyId::new("grants-co");

        let manifest: CompanyManifest = toml::from_str(
            r#"
            [company]
            name = "Grants Co"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"

            [tools]
            allow = ["*", "search"]
        "#,
        )
        .expect("valid manifest");

        let held = ToolGrantsOverride {
            added: vec!["chargebee".to_string()],
            set_by: admin_actor(),
            at_millis: 1_700_000_000_003,
        };

        // The record exactly as `PUT …/tools/grants` leaves it: the override
        // stored, and the grant folded into the manifest every reader consults.
        let mut folded = manifest.clone();
        folded.tools.allow.push("chargebee".to_string());

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            id: id.clone(),
            manifest: folded,
            ledger: Vec::new(),
            lifecycle: "running".into(),
            overlay_agents: Vec::new(),
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: Some(held.clone()),
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            overlay_agent_edits: Vec::new(),
            overlay_retired_agents: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        })
        .await
        .unwrap();

        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();

        // The bundle's `company.toml` IS the restored company's seed. It must
        // carry version control's own list and nothing the console added.
        let seed_toml = tokio::fs::read_to_string(dest.join(COMPANY_TOML))
            .await
            .expect("the bundle writes a company.toml");
        let seed: CompanyManifest = toml::from_str(&seed_toml).expect("a valid seed");
        assert_eq!(
            seed.tools.allow,
            vec!["*".to_string(), "search".to_string()],
            "the exported seed must not carry the console's grant"
        );

        let (s2, e2, m2, c2) = fs_ports(&home2);
        import_bundle(&dest, s2.clone(), e2, m2, c2, None)
            .await
            .unwrap();
        let dst = s2.load(&id).await.unwrap().unwrap();

        // The grant itself survives, still attributed to the operator...
        assert_eq!(
            dst.overlay_tool_grants.as_ref(),
            Some(&held),
            "the console grant was dropped by the bundle round-trip"
        );
        // ...and is folded back in, so the restored company grants it from the
        // first read rather than from its first rebuild.
        assert!(
            crate::company::grants_chargebee_explicit(&dst.manifest.tools.allow),
            "the restored record must grant it: {:?}",
            dst.manifest.tools.allow
        );
        assert_eq!(
            dst.manifest
                .tools
                .allow
                .iter()
                .filter(|g| *g == "chargebee")
                .count(),
            1,
            "folded twice"
        );
        // And the seed is still recoverable from it, which is what keeps the
        // grant revocable and keeps the next rebuild from clearing it.
        assert_eq!(
            crate::ports::types::seed_tool_allow(
                &dst.manifest.tools.allow,
                dst.overlay_tool_grants.as_ref()
            ),
            vec!["*".to_string(), "search".to_string()]
        );
    }

    /// Issue #343: a bundle carrying two overrides for one teammate is **refused**
    /// at import, not silently reduced to whichever row deserialized first.
    ///
    /// Import is the only boundary where `overlay_budgets` arrives from outside
    /// this process, so it is the only place the write path's one-per-teammate
    /// invariant can be broken. The two rows here disagree ($0 versus $50, set by
    /// different people), which is the point: there is no correct row to pick,
    /// and picking silently would either mute a teammate or restore an allowance
    /// an admin revoked, with the wrong name on the attribution either way.
    #[tokio::test]
    async fn a_bundle_with_duplicate_budget_overrides_is_rejected() {
        let home1 = tmp_root("dup-src");
        let home2 = tmp_root("dup-dst");
        let dest = tmp_root("dup-bundle");
        let id = CompanyId::new("dup-co");

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: budget_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
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
        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();

        // Forge the tampered/foreign bundle by rewriting its meta.json — the shape
        // an import can be handed but the write path can never produce.
        let meta_path = dest.join(META_JSON);
        let mut meta: BundleMeta =
            serde_json::from_str(&tokio::fs::read_to_string(&meta_path).await.unwrap()).unwrap();
        meta.overlay_budgets = vec![
            BudgetOverride {
                agent_id: "ceo".into(),
                budget_usd_daily: Some(0.0),
                set_by: admin_actor(),
                at_millis: 1_700_000_000_000,
            },
            BudgetOverride {
                agent_id: "ceo".into(),
                budget_usd_daily: Some(50.0),
                set_by: Actor {
                    kind: ActorKind::User,
                    id: "user-other".into(),
                },
                at_millis: 1_700_000_000_002,
            },
        ];
        tokio::fs::write(&meta_path, serde_json::to_string(&meta).unwrap())
            .await
            .unwrap();

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let err = import_bundle(&dest, s2.clone(), e2, m2, c2, None)
            .await
            .expect_err("import must refuse a bundle with two overrides for one teammate");
        let message = err.to_string();
        assert!(
            message.contains("ceo") && message.contains("budget override"),
            "the refusal must name the teammate so an operator can fix the bundle: {message}"
        );

        // And nothing was written: a refused import must not half-apply.
        assert!(
            s2.load(&id).await.unwrap().is_none(),
            "a rejected bundle must not persist a partial company record"
        );

        for dir in [home1, home2, dest] {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }
    }

    /// The same refusal for the roster edits, which arrive through the same one
    /// door and are read the same first-match way.
    ///
    /// The two rows here disagree about the teammate's role and were set by
    /// different people, which is the point: there is no correct row to pick.
    /// Applying whichever deserialized first would restore a name an operator
    /// changed — or, through `tools`, a grant they narrowed — and attribute it to
    /// somebody who did not do it.
    #[tokio::test]
    async fn a_bundle_with_duplicate_agent_edits_is_rejected() {
        let home1 = tmp_root("dupedit-src");
        let home2 = tmp_root("dupedit-dst");
        let dest = tmp_root("dupedit-bundle");
        let id = CompanyId::new("dupedit-co");

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: id.clone(),
            manifest: budget_manifest(),
            ledger: Vec::new(),
            lifecycle: "running".into(),
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
        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();

        // The shape an import can be handed but `upsert_agent_override` can never
        // produce: it replaces in place, so a second row for one teammate only
        // exists in a bundle written elsewhere.
        let meta_path = dest.join(META_JSON);
        let mut meta: BundleMeta =
            serde_json::from_str(&tokio::fs::read_to_string(&meta_path).await.unwrap()).unwrap();
        meta.overlay_agent_edits = vec![
            AgentOverride {
                agent_id: "ceo".into(),
                role: Some("Chief Vibes".into()),
                ..Default::default()
            },
            AgentOverride {
                agent_id: "ceo".into(),
                role: Some("Interim Chief".into()),
                tools: Some(Some(vec!["docs.read".into()])),
                ..Default::default()
            },
        ];
        tokio::fs::write(&meta_path, serde_json::to_string(&meta).unwrap())
            .await
            .unwrap();

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let err = import_bundle(&dest, s2.clone(), e2, m2, c2, None)
            .await
            .expect_err("import must refuse a bundle with two edits for one teammate");
        let message = err.to_string();
        assert!(
            message.contains("ceo") && message.contains("more than one edit"),
            "the refusal must name the teammate so an operator can fix the bundle: {message}"
        );

        // And nothing was written: a refused import must not half-apply.
        assert!(
            s2.load(&id).await.unwrap().is_none(),
            "a rejected bundle must not persist a partial company record"
        );

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
                mentions: Vec::new(),
                parent: None,
                text: "hi".into(),
                by: None,
                chat: None,
                deliverable: None,
                attachments: Vec::new(),
            }])
            .await
            .unwrap();

        let (s, e, m, c) = fs_ports(&home);
        let bundle_dir = tmp_root("tar-bundle").join(id.as_ref());
        export_bundle(&id, &bundle_dir, s, e, m, c, None, ExportOpts::default())
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
        let imported = import_bundle(&root, s2.clone(), e2, m2, c2, None)
            .await
            .unwrap();
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

    /// The third knowledge port finally travels: facts written on the source
    /// come back from the imported bundle, and the bundle carries them in
    /// `facts.jsonl (bundle root)` beside the traces they conceptually sit with.
    #[tokio::test]
    async fn operator_facts_travel_with_the_bundle() {
        use crate::ports::facts::FactStore;
        use crate::ports::{FactKind, FactRecord};
        use crate::store::FsOps;

        let home1 = tmp_root("facts-src");
        let home2 = tmp_root("facts-dst");
        let dest = tmp_root("facts-bundle");
        let id = CompanyId::new("facts-co");

        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&company_record(&id)).await.unwrap();
        let f1: Arc<dyn FactStore> = Arc::new(FsOps::new(home1.clone()));
        f1.upsert(
            &id,
            &FactRecord {
                id: "supplier".into(),
                kind: FactKind::Fact,
                title: "supplier".into(),
                body: "lathe parts come from Initech".into(),
                source: "cto".into(),
                updated_at_millis: 1,
            },
        )
        .await
        .unwrap();

        export_bundle(&id, &dest, s1, e1, m1, c1, Some(f1), ExportOpts::default())
            .await
            .unwrap();
        assert!(
            dest.join(FACTS_JSONL).is_file(),
            "the bundle must carry the facts file at the bundle root, where \
             the live fs layout keeps it"
        );

        let (s2, e2, m2, c2) = fs_ports(&home2);
        let f2: Arc<dyn FactStore> = Arc::new(FsOps::new(home2.clone()));
        let imported = import_bundle(&dest, s2, e2, m2, c2, Some(f2.clone()))
            .await
            .unwrap();
        let listed = f2.list(&imported, None, None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].body, "lathe parts come from Initech");
    }

    /// Both compatibility directions: a bundle written without facts (an old
    /// host, or an export run without the port) imports clean into a target
    /// that has one — empty, never an error — and a bundle WITH facts refuses
    /// a target with no fact port rather than dropping them silently.
    #[tokio::test]
    async fn facts_compatibility_is_explicit_in_both_directions() {
        use crate::ports::facts::FactStore;
        use crate::ports::{FactKind, FactRecord};
        use crate::store::FsOps;

        // Old bundle (no facts file) into a facts-capable target: clean.
        let home1 = tmp_root("factless-src");
        let home2 = tmp_root("factless-dst");
        let dest = tmp_root("factless-bundle");
        let id = CompanyId::new("factless-co");
        let (s1, e1, m1, c1) = fs_ports(&home1);
        s1.save(&company_record(&id)).await.unwrap();
        export_bundle(&id, &dest, s1, e1, m1, c1, None, ExportOpts::default())
            .await
            .unwrap();
        assert!(!dest.join(FACTS_JSONL).exists());
        let (s2, e2, m2, c2) = fs_ports(&home2);
        let f2: Arc<dyn FactStore> = Arc::new(FsOps::new(home2.clone()));
        let imported = import_bundle(&dest, s2, e2, m2, c2, Some(f2.clone()))
            .await
            .unwrap();
        assert!(f2.list(&imported, None, None).await.unwrap().is_empty());

        // Facts-bearing bundle into a target with no fact port: a refusal
        // naming the loss, not a silent drop.
        let home3 = tmp_root("factful-src");
        let home4 = tmp_root("factful-dst");
        let dest2 = tmp_root("factful-bundle");
        let id2 = CompanyId::new("factful-co");
        let (s3, e3, m3, c3) = fs_ports(&home3);
        s3.save(&company_record(&id2)).await.unwrap();
        let f3: Arc<dyn FactStore> = Arc::new(FsOps::new(home3.clone()));
        f3.upsert(
            &id2,
            &FactRecord {
                id: "f".into(),
                kind: FactKind::Fact,
                title: "t".into(),
                body: "b".into(),
                source: "s".into(),
                updated_at_millis: 1,
            },
        )
        .await
        .unwrap();
        export_bundle(
            &id2,
            &dest2,
            s3,
            e3,
            m3,
            c3,
            Some(f3),
            ExportOpts::default(),
        )
        .await
        .unwrap();
        let (s4, e4, m4, c4) = fs_ports(&home4);
        let err = import_bundle(&dest2, s4.clone(), e4, m4, c4, None)
            .await
            .expect_err("facts with no target port must refuse");
        assert!(err.to_string().contains("fact"), "{err}");
        // The property the refuse-before-write ordering exists for: NOTHING
        // landed. A refusal after `store.save` would leave a half-import
        // whose append-only retry duplicates history.
        assert!(
            s4.load(&id2).await.unwrap().is_none(),
            "the refusal must precede every write"
        );
    }
    /// The fact-port failure case of the ordering guarantee: facts are the
    /// FIRST write, so a failing fact port leaves zero company state behind —
    /// the retry-safety claim, asserted rather than narrated.
    #[tokio::test]
    async fn a_failing_fact_port_leaves_nothing_written() {
        use crate::ports::facts::FactStore;
        use crate::ports::{FactKind, FactRecord};
        use crate::store::FsOps;

        struct FailingFacts;
        #[async_trait::async_trait]
        impl FactStore for FailingFacts {
            async fn list(
                &self,
                _: &CompanyId,
                _: Option<&str>,
                _: Option<FactKind>,
            ) -> crate::Result<Vec<FactRecord>> {
                Ok(Vec::new())
            }
            async fn upsert(&self, _: &CompanyId, _: &FactRecord) -> crate::Result<()> {
                Err(crate::error::OpenCompanyError::Store(
                    "injected fact-port failure".into(),
                ))
            }
            async fn delete(&self, _: &CompanyId, _: &str) -> crate::Result<bool> {
                Ok(false)
            }
        }

        let home_src = tmp_root("factfail-src");
        let home_dst = tmp_root("factfail-dst");
        let dest = tmp_root("factfail-bundle");
        let id = CompanyId::new("factfail-co");
        let (s1, e1, m1, c1) = fs_ports(&home_src);
        s1.save(&company_record(&id)).await.unwrap();
        let f1: Arc<dyn FactStore> = Arc::new(FsOps::new(home_src.clone()));
        f1.upsert(
            &id,
            &FactRecord {
                id: "f".into(),
                kind: FactKind::Fact,
                title: "t".into(),
                body: "b".into(),
                source: "s".into(),
                updated_at_millis: 1,
            },
        )
        .await
        .unwrap();
        export_bundle(&id, &dest, s1, e1, m1, c1, Some(f1), ExportOpts::default())
            .await
            .unwrap();

        let (s2, e2, m2, c2) = fs_ports(&home_dst);
        let err = import_bundle(&dest, s2.clone(), e2, m2, c2, Some(Arc::new(FailingFacts)))
            .await
            .expect_err("the injected fact failure must surface");
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(
            s2.load(&id).await.unwrap().is_none(),
            "a fact-port failure must precede every append-only write"
        );
    }

    /// Re-exporting a now-factless company into the SAME directory must not
    /// leave the previous export's facts behind for a later import to
    /// resurrect.
    #[tokio::test]
    async fn a_factless_reexport_removes_the_stale_facts_file() {
        use crate::ports::facts::FactStore;
        use crate::ports::{FactKind, FactRecord};
        use crate::store::FsOps;

        let home = tmp_root("stale-src");
        let dest = tmp_root("stale-bundle");
        let id = CompanyId::new("stale-co");
        let (s1, e1, m1, c1) = fs_ports(&home);
        s1.save(&company_record(&id)).await.unwrap();
        let facts: Arc<dyn FactStore> = Arc::new(FsOps::new(home.clone()));
        facts
            .upsert(
                &id,
                &FactRecord {
                    id: "f".into(),
                    kind: FactKind::Fact,
                    title: "t".into(),
                    body: "b".into(),
                    source: "s".into(),
                    updated_at_millis: 1,
                },
            )
            .await
            .unwrap();
        export_bundle(
            &id,
            &dest,
            s1.clone(),
            e1.clone(),
            m1.clone(),
            c1.clone(),
            Some(facts.clone()),
            ExportOpts::default(),
        )
        .await
        .unwrap();
        assert!(dest.join(FACTS_JSONL).is_file());

        // The operator deletes the fact, then re-exports into the same dir.
        assert!(facts.delete(&id, "f").await.unwrap());
        export_bundle(
            &id,
            &dest,
            s1,
            e1,
            m1,
            c1,
            Some(facts),
            ExportOpts::default(),
        )
        .await
        .unwrap();
        assert!(
            !dest.join(FACTS_JSONL).exists(),
            "a factless re-export must remove the stale facts file"
        );
    }
}
