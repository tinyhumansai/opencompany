//! [`RuntimeBuilder`]: wires a [`CompanyRuntime`] from filesystem defaults.
//!
//! `fs_defaults` assembles the Phase-1 stack — fs-backed stores, the
//! manifest-`[policy]` [`ManifestApprovalGate`](crate::policy::ManifestApprovalGate),
//! the offline [`EchoBrain`], a built-in operator channel, and the stub tool
//! provider — with no agent economy. Operators swap any port through the
//! `with_*` setters before [`build`](RuntimeBuilder::build).
//!
//! `build` performs boot replay: it loads the runtime journal and rehydrates
//! any parked approvals into the gate so an approval survives a restart.

use std::path::PathBuf;
use std::sync::Arc;

use crate::Result;
use crate::app::config::BrainMode;
use crate::brain::medulla::MedullaTransport;
use crate::brain::medulla::wire::ToolManifestEntry;
use crate::brain::{EchoBrain, HostedMedullaBrain};
use crate::company::CompanyManifest;
#[cfg(feature = "openhuman")]
use crate::company::inference::{self, EnvDefault};
use crate::company::runtime::{CompanyMail, CompanyRuntime, OpsStores};
use crate::feedback::github::{GitHubClient, RateLimiter};
use crate::feedback::service::FeedbackFiler;
use crate::feedback::store::FeedbackStore;
use crate::feedback::tinyhumans::TinyHumansClient;
use crate::feedback::tool::BuiltinToolProvider;
use crate::feedback::types::ConsentMode;
#[cfg(feature = "openhuman")]
use crate::harness::provider::{HostedProviderConfig, TenantProvider};
#[cfg(feature = "openhuman")]
use crate::harness::{HarnessBrain, HarnessDeps};
use crate::openhuman::rpc::OpenHumanRpc;
use crate::openhuman::{OpenHumanChannelAdapter, OpenHumanToolProvider};
use crate::policy::ManifestApprovalGate;
#[cfg(feature = "openhuman")]
use crate::ports::WorkflowRunner;
use crate::ports::types::{
    CompanyId, CompanyRecord, OverlayWorkflow, SecretValue, TemplateProvenance,
};
use crate::ports::{
    AgentEconomy, ArtifactStore, Brain, ChannelAdapter, CompanyStore, ContextStore, EventLog,
    FactStore, InboxStore, LoginCodeStore, MemoryStore, RunStore, SecretStore, SessionStore,
    SkillStateStore, TaskStore, ToolProvider, UsageMeter, UserStore, WorkspaceStore,
};
use crate::runtime::board_events::BoardAnnouncer;
use crate::runtime::channel::{OPERATOR_CHANNEL, OperatorChannel};
use crate::runtime::handover::RuntimeHandover;
use crate::runtime::journal::RuntimeJournal;
use crate::runtime::tools::{StubToolProvider, grant_matches};
use crate::store::paths::Bundle;
use crate::store::{
    FsCompanyStore, FsContextStore, FsEventLog, FsInboxStore, FsMemoryStore, FsOps, FsSecretStore,
};
#[cfg(feature = "openhuman")]
use crate::workflows::HarnessWorkflowRunner;

/// Derives a filesystem-and-URL-safe company id from a display name.
///
/// Lowercases, collapses runs of non-alphanumeric characters into single
/// hyphens, and trims leading/trailing hyphens (`"Acme Co!"` → `"acme-co"`).
pub fn company_id_from_name(name: &str) -> CompanyId {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    CompanyId::new(if trimmed.is_empty() {
        "company"
    } else {
        trimmed
    })
}

/// Computes a company's effective tool grants: the company-wide
/// `[tools].allow` narrowed by per-agent `tools` (most-restrictive-wins).
///
/// An agent with no explicit `tools` inherits the full company allow-list; an
/// agent that lists tools contributes only those covered by the allow-list. The
/// result is the de-duplicated union across the roster, preserving order. A
/// company with no roster yields the allow-list unchanged.
pub fn effective_grants(manifest: &CompanyManifest) -> Vec<String> {
    let allow = &manifest.tools.allow;
    if manifest.agents.is_empty() {
        return dedup(allow.clone());
    }
    let mut grants: Vec<String> = Vec::new();
    for agent in &manifest.agents {
        if agent.tools.is_empty() {
            grants.extend(allow.iter().cloned());
        } else {
            for tool in &agent.tools {
                if allow_covers(allow, tool) {
                    grants.push(tool.clone());
                }
            }
        }
    }
    dedup(grants)
}

/// One agent's effective tool grants: its own `tools` narrowed by the company
/// `allow`-list, or the full allow-list when the agent lists none. This is the
/// per-agent slice of [`effective_grants`], used by the harness to decide which
/// tool families an individual agent receives.
///
/// Gated to the `openhuman` feature: its only caller is `build_roster`, which is
/// itself feature-gated, so the default build would otherwise flag it dead.
#[cfg(feature = "openhuman")]
pub(crate) fn agent_effective_grants(allow: &[String], agent_tools: &[String]) -> Vec<String> {
    let grants: Vec<String> = if agent_tools.is_empty() {
        allow.to_vec()
    } else {
        agent_tools
            .iter()
            .filter(|tool| allow_covers(allow, tool))
            .cloned()
            .collect()
    };
    dedup(grants)
}

/// Whether the company allow-list covers an agent-requested grant glob.
fn allow_covers(allow: &[String], tool: &str) -> bool {
    let literal = tool.strip_suffix('*').unwrap_or(tool);
    allow.iter().any(|grant| grant_matches(grant, literal))
}

/// De-duplicates a grant list while preserving first-seen order.
fn dedup(grants: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    grants
        .into_iter()
        .filter(|grant| seen.insert(grant.clone()))
        .collect()
}

/// Issue #208: the rebuilt record's `[workflows].enabled` list — the seed
/// manifest's ids first, in seed order, then every runtime-authored overlay
/// workflow id not already among them, in overlay order.
///
/// **Why the persisted record's own `enabled` list is deliberately not an
/// input.** `create_company_workflow` (the shared core behind the console's
/// `POST …/workflows` route and the orchestrator's `create_workflow` tool)
/// writes the graph body into `overlay_workflows` and pushes the id onto
/// `[workflows].enabled` in **one** store save, so overlay presence *is* the
/// runtime-enablement invariant — the overlay body is the durable half of a
/// write that always carried both. Deriving from the bodies rather than from
/// the old `enabled` list buys two things the old list cannot:
///
/// * **self-healing.** Every record written during the bug era has a surviving
///   overlay body whose enabled id a past restart already wiped. Deriving from
///   bodies re-enables those on the next boot with no migration.
/// * **no zombies.** An `enabled` id whose body no longer exists (a seed entry
///   the operator deleted from `company.toml`, or a graph removed at runtime)
///   is dropped instead of being carried forward forever with nothing to run.
///
/// Seed-removed ids are dropped on purpose: the version-controlled
/// `company.toml` stays authoritative for seed-authored entries, so deleting one
/// there takes effect on the next boot exactly as an operator expects.
///
/// This is the **only** manifest field a rebuild merges. Every other field is
/// seed-authoritative, and for two of them that is a security property rather
/// than a convention: `[tools]` and `[policy]` must be seed-wins, because a
/// record-wins merge would let a runtime write **outlive a seed rollback** —
/// privilege persisting after the operator revoked it in version control.
fn merge_enabled_workflows(seed_enabled: &[String], overlays: &[OverlayWorkflow]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::with_capacity(seed_enabled.len() + overlays.len());
    let mut seen = std::collections::HashSet::new();
    for id in seed_enabled {
        if seen.insert(id.clone()) {
            merged.push(id.clone());
        }
    }
    for overlay in overlays {
        if seen.insert(overlay.id.clone()) {
            merged.push(overlay.id.clone());
        }
    }
    merged
}

/// Builds one company's [`CompanyRuntime`] over a filesystem home.
pub struct RuntimeBuilder {
    home: PathBuf,
    id: CompanyId,
    manifest: CompanyManifest,
    brain: Option<Arc<dyn Brain>>,
    brain_mode: Option<BrainMode>,
    credential: Option<SecretValue>,
    api_url: Option<String>,
    transport: Option<Arc<dyn MedullaTransport>>,
    store: Option<Arc<dyn CompanyStore>>,
    events: Option<Arc<dyn EventLog>>,
    memory: Option<Arc<dyn MemoryStore>>,
    context: Option<Arc<dyn ContextStore>>,
    tools: Option<Arc<dyn ToolProvider>>,
    channels: Option<Vec<Arc<dyn ChannelAdapter>>>,
    economy: Option<Arc<dyn AgentEconomy>>,
    discoverable_override: Option<bool>,
    tinyplace_api_url: Option<String>,
    host_base_url: Option<String>,
    approvals: Option<Arc<ManifestApprovalGate>>,
    openhuman: Option<Arc<dyn OpenHumanRpc>>,
    secrets: Option<Arc<dyn SecretStore>>,
    inbox: Option<Arc<dyn InboxStore>>,
    mail: Option<CompanyMail>,
    tasks: Option<Arc<dyn TaskStore>>,
    workspace: Option<Arc<dyn WorkspaceStore>>,
    facts: Option<Arc<dyn FactStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    runs: Option<Arc<dyn RunStore>>,
    usage: Option<Arc<dyn UsageMeter>>,
    skills: Option<Arc<dyn SkillStateStore>>,
    users: Option<Arc<dyn UserStore>>,
    sessions: Option<Arc<dyn SessionStore>>,
    login_codes: Option<Arc<dyn LoginCodeStore>>,
    seed_dir: Option<PathBuf>,
    /// The repo-level shared skill library, passed to the harness so a pre-fix
    /// registry install (whose stored `SKILL.md` is a one-line stub) is healed
    /// from the live library. Empty when no repo checkout backs the host.
    skills_registry: Arc<[crate::company::SkillDoc]>,
    /// Issue #85: the source-template provenance to stamp on this company's
    /// record at *first* launch. Set by the launch path when the manifest was
    /// seeded from a template directory; left `None` for a raw-manifest
    /// provision. On a rebuild the record's own provenance is carried forward,
    /// so this only applies when no record exists yet.
    template_provenance: Option<TemplateProvenance>,
    feedback: Option<Arc<FeedbackStore>>,
    github: Option<Arc<dyn GitHubClient>>,
    tinyhumans_feedback: Option<Arc<dyn TinyHumansClient>>,
    consent: ConsentMode,
    /// WS4: the embedded openhuman harness pool. Feature-gated so the default
    /// build is unaffected; wired through to [`CompanyRuntime`] when present.
    #[cfg(feature = "openhuman")]
    harness: Option<Arc<crate::harness::HarnessPool>>,
    /// WS4/#56: the platform-injected managed inference default (endpoint +
    /// credential) and an optional roster-wide model override. This is the
    /// *lowest-precedence* inference source — a manifest `[inference]` section
    /// or a runtime console override outranks it. With [`harness`](Self::harness)
    /// set and any inference source configured, cognition routes through a
    /// per-tenant [`TenantProvider`](crate::harness::provider::TenantProvider).
    #[cfg(feature = "openhuman")]
    harness_inference: Option<(HostedProviderConfig, Option<String>)>,
    /// Issue #109: the MANAGED media-generation backend (env-resolved platform
    /// credential + URL). `None` fails closed — no image/video tools are wired.
    /// Threaded onto every harness-built agent's [`HarnessDeps`], but only
    /// consumed when a company **explicitly** grants the `media` namespace.
    #[cfg(feature = "openhuman")]
    media_backend: Option<crate::harness::toolbelt::MediaBackend>,
    /// Issue #238: the MANAGED web-search backend (env-resolved platform
    /// credential + URL). `None` fails closed — no `web_search` tool is wired.
    /// Threaded onto every harness-built agent's [`HarnessDeps`], but only
    /// consumed when a company **explicitly** grants the `search` namespace.
    #[cfg(feature = "openhuman")]
    search_backend: Option<crate::harness::search::SearchBackend>,
    /// Issue #290: the live state of the runtime this build is *replacing*.
    ///
    /// Present only on a rebuild. It supplies the per-instance pieces a second
    /// runtime must never duplicate (see [`RuntimeHandover`]), and its presence
    /// is also the "this is a rebuild" signal that suppresses the boot-only side
    /// effects below: journal replay, orphan-run reaping, workspace seeding,
    /// going-public, and the MCP re-boot.
    handover: Option<RuntimeHandover>,
}

impl RuntimeBuilder {
    /// Starts a builder for `manifest` rooted at the OpenCompany home `home`.
    ///
    /// The company id defaults to a slug of the manifest name; override it with
    /// [`with_id`](Self::with_id).
    pub fn new(home: impl Into<PathBuf>, manifest: CompanyManifest) -> Self {
        let id = company_id_from_name(&manifest.company.name);
        Self {
            home: home.into(),
            id,
            manifest,
            brain: None,
            brain_mode: None,
            credential: None,
            api_url: None,
            transport: None,
            store: None,
            events: None,
            memory: None,
            context: None,
            tools: None,
            channels: None,
            economy: None,
            discoverable_override: None,
            tinyplace_api_url: None,
            host_base_url: None,
            approvals: None,
            openhuman: None,
            secrets: None,
            inbox: None,
            mail: None,
            tasks: None,
            workspace: None,
            facts: None,
            artifacts: None,
            runs: None,
            usage: None,
            skills: None,
            users: None,
            sessions: None,
            login_codes: None,
            seed_dir: None,
            skills_registry: Arc::from([]),
            template_provenance: None,
            feedback: None,
            github: None,
            tinyhumans_feedback: None,
            consent: ConsentMode::default(),
            #[cfg(feature = "openhuman")]
            harness: None,
            #[cfg(feature = "openhuman")]
            harness_inference: None,
            #[cfg(feature = "openhuman")]
            media_backend: None,
            #[cfg(feature = "openhuman")]
            search_backend: None,
            handover: None,
        }
    }

    /// Overrides the derived company id.
    pub fn with_id(mut self, id: CompanyId) -> Self {
        self.id = id;
        self
    }

    /// Swaps the cognition brain (default [`EchoBrain`]).
    ///
    /// An explicit brain wins over hosted-brain selection: setting this bypasses
    /// [`with_brain_mode`](Self::with_brain_mode) entirely.
    pub fn with_brain(mut self, brain: Arc<dyn Brain>) -> Self {
        self.brain = Some(brain);
        self
    }

    /// Sets the brain mode driving hosted-brain selection (default
    /// [`BrainMode::Hosted`]).
    ///
    /// Hosted mode plus a credential selects the
    /// [`HostedMedullaBrain`](crate::brain::HostedMedullaBrain); anything else
    /// falls back to the degraded [`EchoBrain`].
    pub fn with_brain_mode(mut self, mode: BrainMode) -> Self {
        self.brain_mode = Some(mode);
        self
    }

    /// Provides the TinyHumans hosted-brain credential. Without it, hosted mode
    /// degrades to [`EchoBrain`]. Never logged.
    pub fn with_credential(mut self, credential: SecretValue) -> Self {
        self.credential = Some(credential);
        self
    }

    /// Sets the orchestration API base URL used to build the networked
    /// transport under the `medulla` feature.
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = Some(api_url.into());
        self
    }

    /// Injects a [`MedullaTransport`] for the hosted brain to drive.
    ///
    /// Always available (not feature-gated) so offline tests can wire the
    /// in-memory mock transport and exercise [`HostedMedullaBrain`] end-to-end
    /// in the default build. An injected transport takes precedence over the
    /// networked transport the `medulla` feature would otherwise construct.
    pub fn with_transport(mut self, transport: Arc<dyn MedullaTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Swaps the company store.
    pub fn with_store(mut self, store: Arc<dyn CompanyStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Swaps the event log.
    pub fn with_events(mut self, events: Arc<dyn EventLog>) -> Self {
        self.events = Some(events);
        self
    }

    /// Swaps the memory store.
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Swaps the context store.
    pub fn with_context(mut self, context: Arc<dyn ContextStore>) -> Self {
        self.context = Some(context);
        self
    }

    /// Swaps every durable port at once from one opened storage backend
    /// (see [`crate::store::select`]).
    pub fn with_stores(mut self, handles: &crate::store::StorageHandles) -> Self {
        self.tasks = Some(handles.tasks.clone());
        self.workspace = Some(handles.workspace.clone());
        self.facts = Some(handles.facts.clone());
        self.artifacts = Some(handles.artifacts.clone());
        self.runs = Some(handles.runs.clone());
        self.usage = Some(handles.usage.clone());
        self.skills = Some(handles.skills.clone());
        self.users = Some(handles.users.clone());
        self.sessions = Some(handles.sessions.clone());
        self.login_codes = Some(handles.login_codes.clone());
        self.with_store(handles.company.clone())
            .with_events(handles.events.clone())
            .with_memory(handles.memory.clone())
            .with_context(handles.context.clone())
            .with_secrets(handles.secrets.clone())
            .with_inbox(handles.inbox.clone())
    }

    /// Overlays just the memory + context ports from a selected memory engine
    /// (`OPENCOMPANY_MEMORY`, see [`crate::store::select`]).
    ///
    /// Applied *after* [`with_stores`](Self::with_stores) (or over the fs
    /// defaults), so a dedicated memory engine such as TinyCortex backs recall
    /// while the base backend keeps every other durable port.
    pub fn with_memory_overlay(self, overlay: &crate::store::MemoryOverlay) -> Self {
        self.with_memory(overlay.memory.clone())
            .with_context(overlay.context.clone())
    }

    /// Swaps the task board store (default: fs-backed).
    pub fn with_tasks(mut self, tasks: Arc<dyn TaskStore>) -> Self {
        self.tasks = Some(tasks);
        self
    }

    /// Swaps the human user directory (default: fs-backed).
    pub fn with_users(mut self, users: Arc<dyn UserStore>) -> Self {
        self.users = Some(users);
        self
    }

    /// Swaps the session store (default: fs-backed).
    pub fn with_sessions(mut self, sessions: Arc<dyn SessionStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// Swaps the login-code store (default: fs-backed).
    pub fn with_login_codes(mut self, login_codes: Arc<dyn LoginCodeStore>) -> Self {
        self.login_codes = Some(login_codes);
        self
    }

    /// Swaps the workspace store (default: fs-backed).
    pub fn with_workspace(mut self, workspace: Arc<dyn WorkspaceStore>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Swaps the facts store (default: fs-backed).
    pub fn with_facts(mut self, facts: Arc<dyn FactStore>) -> Self {
        self.facts = Some(facts);
        self
    }

    /// Swaps the artifact store (default: fs-backed).
    pub fn with_artifacts(mut self, artifacts: Arc<dyn ArtifactStore>) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    /// Swaps the task-run store (default: fs-backed).
    pub fn with_runs(mut self, runs: Arc<dyn RunStore>) -> Self {
        self.runs = Some(runs);
        self
    }

    /// Swaps the usage meter (default: fs-backed).
    pub fn with_usage(mut self, usage: Arc<dyn UsageMeter>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Swaps the skill-state store (default: fs-backed).
    pub fn with_skills(mut self, skills: Arc<dyn SkillStateStore>) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Sets the company definition directory (`companies/<name>`) the workspace
    /// tree is seeded from on first build. Without it, no seeding runs.
    pub fn with_seed_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.seed_dir = Some(dir.into());
        self
    }

    /// Sets the repo-level shared skill library (`skills/*/SKILL.md`), used by
    /// the harness to heal pre-fix registry installs. Unset leaves it empty,
    /// which simply skips healing.
    pub fn with_skills_registry(mut self, registry: Arc<[crate::company::SkillDoc]>) -> Self {
        self.skills_registry = registry;
        self
    }

    /// Records the source-template provenance to stamp on this company's record
    /// at first launch (issue #85). The launch path sets this when the manifest
    /// was seeded from a template directory; a raw-manifest provision leaves it
    /// unset so no provenance is fabricated. On a rebuild the persisted record's
    /// provenance is carried forward and this value is ignored.
    pub fn with_template_provenance(mut self, provenance: TemplateProvenance) -> Self {
        self.template_provenance = Some(provenance);
        self
    }

    /// Swaps the tool provider.
    pub fn with_tools(mut self, tools: Arc<dyn ToolProvider>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Overrides the channel adapters (default: a single operator channel).
    pub fn with_channels(mut self, channels: Vec<Arc<dyn ChannelAdapter>>) -> Self {
        self.channels = Some(channels);
        self
    }

    /// Wires an agent economy (default: none).
    ///
    /// An injected economy wins over the auto-wired tiny.place economy the
    /// `tinyplace` feature would otherwise construct at [`build`](Self::build).
    pub fn with_economy(mut self, economy: Arc<dyn AgentEconomy>) -> Self {
        self.economy = Some(economy);
        self
    }

    /// Forces going-public on (or off) regardless of `[place].discoverable`.
    ///
    /// Powers `serve --discoverable`, which opts every loaded company into the
    /// tiny.place economy. Left unset, the manifest's `[place].discoverable`
    /// decides.
    pub fn with_discoverable(mut self, discoverable: bool) -> Self {
        self.discoverable_override = Some(discoverable);
        self
    }

    /// Sets the tiny.place economy API base URL used to build the networked
    /// client under the `tinyplace` feature.
    pub fn with_tinyplace_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.tinyplace_api_url = Some(api_url.into());
        self
    }

    /// Sets the host base URL embedded in the published Agent Card endpoint.
    pub fn with_host_base_url(mut self, host_base_url: impl Into<String>) -> Self {
        self.host_base_url = Some(host_base_url.into());
        self
    }

    /// Swaps the approval gate (default: manifest `[policy]` gate).
    /// Issue #290: adopt the live state of the runtime this build replaces,
    /// instead of constructing a second copy of it.
    ///
    /// Setting this makes the build a **rebuild**: see [`RuntimeHandover`] for
    /// what is inherited and why each piece is a correctness matter, and
    /// [`rebuild_company`](crate::runtime::rebuild_company) for the quiesce →
    /// hand over → build → swap sequence a caller must follow around it.
    pub fn with_handover(mut self, handover: RuntimeHandover) -> Self {
        self.handover = Some(handover);
        self
    }

    pub fn with_approvals(mut self, approvals: Arc<ManifestApprovalGate>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// Attaches an OpenHuman JSON-RPC transport.
    ///
    /// When present and healthy at [`build`](Self::build) time, an
    /// `openhuman`-provider manifest routes tools (and `openhuman` channels)
    /// through it; otherwise the runtime degrades to built-in tools and the
    /// operator channel with a boot warning.
    pub fn with_openhuman_rpc(mut self, rpc: Arc<dyn OpenHumanRpc>) -> Self {
        self.openhuman = Some(rpc);
        self
    }

    /// WS4: attaches the embedded openhuman harness pool. When present, the
    /// runtime exposes it through [`CompanyRuntime::harness`] so the chat layer
    /// (WS3) can route desk turns through it; without it the runtime keeps its
    /// echo/hosted brain path unchanged. Feature-gated — the default build has
    /// no harness.
    #[cfg(feature = "openhuman")]
    pub fn with_harness(mut self, harness: Arc<crate::harness::HarnessPool>) -> Self {
        self.harness = Some(harness);
        self
    }

    /// WS4/#56: sets the platform-injected managed inference default (endpoint +
    /// credential) and an optional roster-wide model override
    /// (`OPENCOMPANY_INFERENCE_MODEL`). This is the lowest-precedence inference
    /// source; a manifest `[inference]` section or a runtime console override
    /// wins over it. Combined with [`with_harness`](Self::with_harness) and any
    /// configured inference source, cognition routes through a per-tenant
    /// [`TenantProvider`](crate::harness::provider::TenantProvider). Feature-gated.
    #[cfg(feature = "openhuman")]
    pub fn with_harness_inference(
        mut self,
        config: HostedProviderConfig,
        model_override: Option<String>,
    ) -> Self {
        self.harness_inference = Some((config, model_override));
        self
    }

    /// Issue #109: sets the MANAGED media-generation backend (platform
    /// credential + URL, resolved from the environment via
    /// [`media_backend_from_env`](crate::harness::provider::media_backend_from_env)).
    /// This is the ONLY path media generation is ever fed a credential — never a
    /// tenant secret — so a company can generate media only on the managed
    /// platform account. Absent (the default), media tools are never wired even
    /// for a company that grants `media`. Feature-gated.
    #[cfg(feature = "openhuman")]
    pub fn with_media_backend(
        mut self,
        media_backend: crate::harness::toolbelt::MediaBackend,
    ) -> Self {
        self.media_backend = Some(media_backend);
        self
    }

    /// Issue #238: sets the MANAGED web-search backend (platform credential +
    /// URL, resolved from the environment via
    /// [`search_backend_from_env`](crate::harness::provider::search_backend_from_env)).
    /// This is the ONLY path search is ever fed a credential — never a tenant
    /// secret — so a company can only ever search on the managed platform
    /// account. Absent (the default), `web_search` is never wired even for a
    /// company that grants `search`. Feature-gated.
    #[cfg(feature = "openhuman")]
    pub fn with_search_backend(
        mut self,
        search_backend: crate::harness::search::SearchBackend,
    ) -> Self {
        self.search_backend = Some(search_backend);
        self
    }

    /// Swaps the secret store (default: fs-backed). The feedback scrubber reads
    /// it to fail closed on secret leaks.
    pub fn with_secrets(mut self, secrets: Arc<dyn SecretStore>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Swaps the inbox store (default: fs-backed). Holds inbound and outbound
    /// email for the per-teammate inboxes.
    pub fn with_inbox(mut self, inbox: Arc<dyn InboxStore>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    /// Wires the company's outbound mail sender + credentials. Absent by default
    /// (email send is opt-in / hosted-only).
    pub fn with_mail(mut self, mail: CompanyMail) -> Self {
        self.mail = Some(mail);
        self
    }

    /// Overrides the feedback store (default: the company bundle's feedback
    /// family).
    pub fn with_feedback(mut self, feedback: Arc<FeedbackStore>) -> Self {
        self.feedback = Some(feedback);
        self
    }

    /// Wires a GitHub client for feedback filing (default: none → manual links).
    pub fn with_github(mut self, github: Arc<dyn GitHubClient>) -> Self {
        self.github = Some(github);
        self
    }

    /// Wires the TinyHumans hub for feedback forwarding (default: none → file
    /// to GitHub instead).
    ///
    /// Set this only on a provisioned instance — one with a TinyHumans
    /// credential. Its presence redirects feedback to the hub, where it is
    /// recorded on behalf of the credential's owner.
    pub fn with_tinyhumans_feedback(mut self, client: Arc<dyn TinyHumansClient>) -> Self {
        self.tinyhumans_feedback = Some(client);
        self
    }

    /// Sets the standing feedback consent mode (default: `manual`).
    pub fn with_feedback_consent(mut self, consent: ConsentMode) -> Self {
        self.consent = consent;
        self
    }

    /// Convenience: build a fully fs-backed runtime with all Phase-1 defaults.
    pub async fn fs_defaults(
        home: impl Into<PathBuf>,
        manifest: CompanyManifest,
    ) -> Result<CompanyRuntime> {
        Self::new(home, manifest).build().await
    }

    /// Assembles the runtime, materializing `company.toml` and replaying the
    /// journal to rebuild the approval queue.
    pub async fn build(mut self) -> Result<CompanyRuntime> {
        let home = self.home;
        let id = self.id;
        // Issue #290. Present ⇒ this is a rebuild of a live company, so every
        // per-instance piece below is inherited rather than constructed, and the
        // boot-only side effects are skipped. Absent ⇒ an ordinary boot, byte for
        // byte as before.
        let handover = self.handover.take();
        // On a rebuild the *brain* must be built over the inherited harness pool,
        // not a freshly minted one. The boot path mints a pool per build, so
        // without this the successor's brain would talk to a new pool while the
        // runtime reported the old one — two pools for one company, and every
        // agent's conversation history silently dropped. Done here, before any
        // field is moved out of `self`, so the brain arm below and the
        // `set_harness` wiring further down agree by construction.
        #[cfg(feature = "openhuman")]
        if let Some(pool) = handover.as_ref().and_then(|h| h.harness.clone()) {
            self.harness = Some(pool);
        }

        // Inherit-or-construct. The handover's handles outrank an explicitly
        // injected one: on a rebuild, a second store over the same data is the
        // bug, not the configuration.
        let store: Arc<dyn CompanyStore> = handover
            .as_ref()
            .map(|h| h.store.clone())
            .or(self.store)
            .unwrap_or_else(|| Arc::new(FsCompanyStore::new(home.clone())));
        let events: Arc<dyn EventLog> = handover
            .as_ref()
            .map(|h| h.events.clone())
            .or(self.events)
            .unwrap_or_else(|| Arc::new(FsEventLog::new(home.clone())));
        let memory: Arc<dyn MemoryStore> = handover
            .as_ref()
            .map(|h| h.memory.clone())
            .or(self.memory)
            .unwrap_or_else(|| Arc::new(FsMemoryStore::new(home.clone())));
        let context: Arc<dyn ContextStore> = handover
            .as_ref()
            .map(|h| h.context.clone())
            .or(self.context)
            .unwrap_or_else(|| Arc::new(FsContextStore::new(home.clone())));
        // Effective grants narrow the company allow-list by per-agent tools.
        let grants = effective_grants(&self.manifest);
        let openhuman = self.openhuman;

        // Feedback family: the item store, secret store (for the scrubber), and
        // filing configuration. The consent mode is also the built-in feedback
        // tool's capture mode.
        let bundle = Bundle::new(home.clone(), &id);
        let feedback = handover
            .as_ref()
            .map(|h| h.feedback.clone())
            .or(self.feedback)
            .unwrap_or_else(|| Arc::new(FeedbackStore::new(&bundle)));
        let secrets: Arc<dyn SecretStore> = handover
            .as_ref()
            .map(|h| h.secrets.clone())
            .or(self.secrets)
            .unwrap_or_else(|| Arc::new(FsSecretStore::new(home.clone())));
        let inbox: Arc<dyn InboxStore> = handover
            .as_ref()
            .map(|h| h.inbox.clone())
            .or(self.inbox)
            .unwrap_or_else(|| Arc::new(FsInboxStore::new(home.clone())));
        // The WS3 console ports default to a single shared fs backend.
        let fs_ops = Arc::new(FsOps::new(home.clone()));
        let ops = match handover.as_ref() {
            // A rebuild inherits the ops it was handed, announcer and all — the
            // wrap below happens once, at first construction. Re-wrapping an
            // inherited board would announce every write twice.
            Some(h) => h.ops.clone(),
            None => OpsStores {
                // Issue #464: the board announces its own writes. Wrapped here,
                // at the single place the store is chosen, so *every* writer —
                // REST, the cycle, a delegation, the settle mover — announces
                // without knowing it does. See [`BoardAnnouncer`].
                tasks: Arc::new(BoardAnnouncer::new(
                    self.tasks.unwrap_or_else(|| fs_ops.clone()),
                    events.clone(),
                )),
                workspace: self.workspace.unwrap_or_else(|| fs_ops.clone()),
                facts: self.facts.unwrap_or_else(|| fs_ops.clone()),
                artifacts: self.artifacts.unwrap_or_else(|| fs_ops.clone()),
                runs: self.runs.unwrap_or_else(|| fs_ops.clone()),
                usage: self.usage.unwrap_or_else(|| fs_ops.clone()),
                skills: self.skills.unwrap_or_else(|| fs_ops.clone()),
                users: self.users.unwrap_or_else(|| fs_ops.clone()),
                sessions: self.sessions.unwrap_or_else(|| fs_ops.clone()),
                login_codes: self.login_codes.unwrap_or_else(|| fs_ops.clone()),
            },
        };

        // Idempotent workspace seeding: only when the workspace is empty (an
        // operator's deletions must stick, so a seeded-then-emptied workspace is
        // never re-seeded). Skills need no seeding — the store holds deltas only
        // and the effective set unions company-dir skills at read time.
        //
        // Skipped entirely on a rebuild: the workspace belongs to a company that
        // is already running, so there is nothing to seed and the `is_empty`
        // probe would only race the live runtime's own writes.
        if handover.is_none()
            && let Some(seed_dir) = &self.seed_dir
            && ops.workspace.is_empty(&id).await?
        {
            seed_workspace(ops.workspace.as_ref(), &id, seed_dir).await?;
        }

        let consent = self.consent;
        // Inherited on a rebuild so the in-memory filing rate limiter survives.
        // A fresh limiter would make a rebuild loop a rate-limit bypass.
        let filer = match handover.as_ref() {
            Some(h) => h.filer.clone(),
            None => Arc::new(FeedbackFiler {
                client: self.github,
                tinyhumans: self.tinyhumans_feedback,
                repo: crate::feedback::DEFAULT_REPO.to_string(),
                consent,
                limiter: RateLimiter::default(),
                quality: crate::feedback::QualityLedger::default(),
            }),
        };

        // Probe OpenHuman once; an unreachable daemon degrades, never fails.
        let openhuman_healthy = match &openhuman {
            Some(rpc) => rpc.health().await.unwrap_or(false),
            None => false,
        };

        // Tools: route through OpenHuman only when the manifest asks for it and
        // the daemon is reachable; otherwise use the grant-enforcing built-in.
        let tools: Arc<dyn ToolProvider> = match self.tools {
            Some(tools) => tools,
            None => {
                let builtin: Arc<dyn ToolProvider> =
                    Arc::new(StubToolProvider::new(grants.clone()));
                if self.manifest.tools.provider == "openhuman" {
                    match &openhuman {
                        Some(rpc) if openhuman_healthy => Arc::new(OpenHumanToolProvider::new(
                            rpc.clone(),
                            grants.clone(),
                            builtin,
                        )),
                        Some(_) => {
                            tracing::warn!(
                                company = %id,
                                "openhuman tool provider requested but unreachable; using built-in tools"
                            );
                            builtin
                        }
                        None => builtin,
                    }
                } else {
                    builtin
                }
            }
        };

        // Wrap with the built-in `feedback` tool so the brain can always
        // self-report (the feedback tool is never gated); every other tool
        // still delegates to the selected provider, which enforces grants.
        let tools: Arc<dyn ToolProvider> = Arc::new(BuiltinToolProvider::new(
            tools,
            feedback.clone(),
            events.clone(),
            consent,
        ));

        // Channels: always the operator surface, plus any `openhuman` channel
        // the manifest enables when the daemon is reachable.
        let channels = match self.channels {
            Some(channels) => channels,
            None => {
                let mut channels: Vec<Arc<dyn ChannelAdapter>> =
                    vec![Arc::new(OperatorChannel::new())];
                if let Some(rpc) = &openhuman {
                    for (name, config) in &self.manifest.channels {
                        if name == OPERATOR_CHANNEL
                            || config.enabled == Some(false)
                            || config.provider.as_deref() != Some("openhuman")
                        {
                            continue;
                        }
                        if openhuman_healthy {
                            channels.push(Arc::new(OpenHumanChannelAdapter::new(
                                name.clone(),
                                rpc.clone(),
                            )));
                        } else {
                            tracing::warn!(
                                company = %id,
                                channel = %name,
                                "openhuman channel requested but unreachable; skipping"
                            );
                        }
                    }
                }
                channels
            }
        };

        // Boot replay: load the journal and rehydrate parked approvals into the
        // gate so approvals survive a restart with their original ids.
        //
        // **Constructed here, above the brain, on purpose (issue #227).** These
        // two used to be built after the brain, just before `CompanyRuntime::new`
        // — which put them out of reach of the `HarnessDeps` built inside the
        // brain arm, and that is precisely why workflow delivery could not park
        // a cold email recipient the way the agent path does. The block depends
        // on nothing but `home`, `id`, `self.approvals` and
        // `self.manifest.policy`, none of which the code it used to sit below
        // produces or mutates, so hoisting it is a pure move. The same two
        // `Arc`s go to the delivery deps and to the runtime — one gate, one
        // journal, one approvals queue.
        //
        // On a rebuild the journal is **inherited, never reopened**, and the
        // reason is now the in-memory state rather than the file. Since #386 a
        // second instance on one path cannot corrupt it — appends are whole
        // `O_APPEND` writes serialised on a process-wide per-path lock — but it
        // is still wasteful, and `load()` is skipped for the reason it is not
        // repeated at boot: the inherited journal is already replayed, and
        // re-reading it would re-apply records the live instance has since
        // resolved.
        let journal = match handover.as_ref() {
            Some(h) => h.journal.clone(),
            None => {
                let journal = Arc::new(RuntimeJournal::new(
                    Bundle::new(home.clone(), &id).journal_jsonl(),
                ));
                journal.load().await?;
                // Issue #386: a damaged line no longer fails the boot, which
                // means the company can come up on an incomplete history. That
                // is the right trade — an operator cannot repair a journal
                // through a console that will not start — but it is only
                // defensible if somebody is told. `load` already logged each
                // line; this is the one line that names the company, because
                // the effect keys behind it are what the at-most-once guarantee
                // is made of.
                let corruption = journal.corruption();
                if !corruption.is_empty() {
                    tracing::error!(
                        company = %id,
                        lines = corruption.len(),
                        first_line = corruption[0].line,
                        "journal lines could not be replayed; this company booted \
                         without them, so committed effects may be missing from \
                         the at-most-once set and approvals may be missing from \
                         the queue",
                    );
                }
                journal
            }
        };

        // Issue #242, the other half of boot replay: reclaim run records left
        // active by a previous host process.
        //
        // A run row is written *before* its cycle spawns, so a crash in that
        // gap — or anywhere inside the cycle — leaves a row claiming to be
        // Pending or Running that nothing will ever settle. Three invariants
        // make every such row provably dead rather than merely suspicious:
        // cycles are process-local `tokio::spawn`s, exactly one process owns a
        // company (the journal above is single-writer), and cycles serialise on
        // the per-company mutex — so nothing from this process can be in flight
        // yet. `reap_orphaned_runs` therefore needs no timeout heuristic, and it
        // never touches a parked run: WaitingApproval and Paused are waiting on
        // a person or an external condition, not on a process.
        //
        // It runs here, beside `journal.load()`, and well before the dispatch
        // and scheduler spawns further down, so no fresh run can be reaped by
        // mistake. A store failure is logged, never fatal: record-keeping must
        // not stop a company from booting.
        //
        // Issue #290: suppressed on a rebuild. The whole argument above rests on
        // "nothing from this process can be in flight yet" — true at boot, false
        // the moment a company has been serving. Mid-life this sweep would be
        // reclaiming rows it cannot prove are abandoned, which is the one thing
        // it promises never to do.
        //
        // Precisely which rows are at risk, since the answer is narrower than it
        // looks: `rebuild_company` quiesces and drains before reaching here, and
        // both `begin_run` and the terminality backstop sit inside the serial
        // lock, so no `Running` row survives the drain. `Pending` does — the
        // dispatch choke point mints a row *outside* that lock, so a board write
        // landing in the window leaves one behind. Reaping it would stamp the
        // wrong reason on it ("the host restarted"), and if the rebuild then
        // fails and `resume()` puts the company back to work, the row is already
        // terminal: its cycle's `begin_run` is rejected and a genuinely live
        // attempt runs with no record at all.
        //
        // Suppressing rather than leaning on the drain also keeps this resting
        // on the invariant the reaper states instead of on the current call
        // order. It costs nothing: a refused dispatch settles its own row
        // (`CompanyRuntime::abandon_run`), and the next real boot sweeps
        // anything that escapes.
        //
        // Issue #337: the sweep now also makes the **board** truthful. Failing
        // the row alone left the card sitting in In Progress claimed by an
        // attempt that provably no longer exists — and because
        // `task_enters_in_progress` fires on the *transition* into that column,
        // which already happened, nothing would ever re-drive it. So each
        // reaped run's card returns to To-do carrying the orphan reason, and a
        // re-dispatch from there mints a fresh attempt rather than resuming a
        // dead one.
        //
        // Suppressed on a rebuild for exactly the same reason the row sweep is,
        // and not one step further: the proof that these attempts are abandoned
        // is a boot-only proof, and a card is not a safer thing to guess about
        // than a row. The move is guarded on top of that (`advance_settled_card`
        // only ever leaves `in_progress`), so a card an operator parked in
        // Paused or a later attempt landed in In Review is untouched even here.
        if handover.is_none() {
            match crate::ports::runs::reap_orphaned_runs(ops.runs.as_ref(), &id).await {
                Ok(reaped) => {
                    for run in reaped {
                        match crate::runtime::advance::advance_settled_card(
                            ops.tasks.as_ref(),
                            &id,
                            &run.task_id,
                            crate::ports::runs::RunStatus::Failed,
                            crate::ports::runs::ORPHAN_ERROR,
                        )
                        .await
                        {
                            Ok(Some(column)) => tracing::info!(
                                company = %id,
                                run = %run.id,
                                task = %run.task_id,
                                column,
                                "returned a card stranded by a previous host process"
                            ),
                            Ok(None) => {}
                            // One card that will not move must not stop the
                            // rest and must not fail boot — record-keeping never
                            // stops a company from starting.
                            Err(err) => tracing::warn!(
                                company = %id,
                                run = %run.id,
                                task = %run.task_id,
                                error = %err,
                                "reaped an orphaned run but could not return its card"
                            ),
                        }
                    }
                }
                Err(err) => tracing::warn!(
                    company = %id,
                    error = %err,
                    "could not sweep orphaned run records at boot"
                ),
            }

            // Issue #337, the planning-side equivalent of the sweep above, and
            // it exists because that one structurally cannot cover it. A
            // planning pass mints no attempt row — there is no agent turn, no
            // tool loop and nothing to steer — so `reap_orphaned_runs` has
            // nothing to find, and a host that died mid-pass leaves a card
            // sitting in Planning with nothing anywhere claiming to work it.
            // The trigger is the *transition* into the column, which already
            // happened, so nothing would ever re-drive it.
            //
            // Gated on the handover for exactly the reason the two sweeps
            // around it are: at boot nothing from this process can be in
            // flight, which is what makes "found in Planning ⇒ interrupted" a
            // proof rather than a guess; during a rebuild that premise is
            // false and this would yank a live pass out from under itself.
            match crate::runtime::advance::sweep_stranded_planning(ops.tasks.as_ref(), &id).await {
                Ok(returned) => {
                    for task in returned {
                        tracing::info!(
                            company = %id,
                            %task,
                            "returned a card stranded in Planning by a previous host process"
                        );
                    }
                }
                Err(err) => tracing::warn!(
                    company = %id,
                    error = %err,
                    "could not sweep cards stranded in Planning at boot"
                ),
            }
        }

        // Issue #371, the workflow-side equivalent of the sweep above, and it
        // rests on the same three invariants: a workflow run is journaled with a
        // start before the engine call, every entry point drives the run future
        // in this process, and one process owns this journal. So a start with no
        // finish at boot is a run that died with the last host, and settling it
        // is what keeps `GET …/workflows/runs` honest when it folds an unmatched
        // start as `running: true`.
        //
        // Gated on the handover for exactly the reason the run reaper is: a
        // scheduler-spawned workflow run survives a live runtime swap, and
        // sweeping mid-life would stamp "interrupted by a host restart" on a run
        // still walking its graph — whose real outcome would then land after the
        // synthetic one, leaving two contradictory finishes for one run id.
        //
        // It reads the journal rather than a store, so it is deliberately placed
        // after `journal.load()` above and, like it, is best-effort: a failure is
        // logged inside the sweep and never stops a company booting.
        if handover.is_none() {
            crate::runtime::sweep_interrupted_runs(&events, &id).await;
        }

        // The policy gate, rehydrated from the journal replay above so approvals
        // survive a restart with their original ids.
        //
        // Inherited on a rebuild, along with its parked approvals: an approval
        // waiting on a person keeps its id, its parked effect and its TTL across
        // the swap, and rehydrating a fresh gate from the journal would resurrect
        // approvals the live gate has already resolved.
        let gate = match handover.as_ref() {
            Some(h) => h.approval_gate.clone(),
            None => {
                let gate = self.approvals.unwrap_or_else(|| {
                    Arc::new(ManifestApprovalGate::new(self.manifest.policy.clone()))
                });
                for pending in journal.pending() {
                    gate.rehydrate(pending.id, pending.effect, pending.at_millis);
                }
                gate
            }
        };

        // Issue #243: the single-use grant set, seeded from the same replay.
        //
        // Built here, with the journal and the gate, because both ends of the
        // approval round-trip need the SAME set — the runtime mints and sweeps,
        // the harness policy redeems — and the harness deps are constructed
        // several scopes deeper inside the brain match below. `GrantSet` is
        // feature-independent (these journal records replay in every build), so
        // the binding is unconditional; a build without the harness carries a set
        // nothing ever mints into.
        //
        // The window between "operator approved" and "agent re-issued the call"
        // spans a model turn, so a restart inside it is ordinary. Without this
        // seeding the approval would evaporate and the agent would come back
        // asking for a permission it had just been given. Consumed and expired
        // grants are folded out during replay, so this can only re-arm one that
        // never fired.
        //
        // Inherited on a rebuild for the same reason as the gate: the operator
        // who approved a blocked tool call moments before the swap must not be
        // asked to approve it again, and a set rebuilt from the journal replay
        // would re-arm grants the live set has already consumed.
        let grants = match handover.as_ref() {
            Some(h) => h.grants.clone(),
            None => {
                let grants = crate::runtime::grants::GrantSet::default();
                grants.rehydrate(journal.replayed_grants());
                grants
            }
        };

        // Issue #469: which turns are still blocked on a decision, on exactly
        // the same terms. A boot rebuilds it from the approvals the replay left
        // parked; a rebuild inherits the live one, because that one also knows
        // about decisions taken since the replay and a fresh count would ask a
        // turn to wait for them all over again.
        let continuations = match handover.as_ref() {
            Some(h) => h.continuations.clone(),
            None => {
                let continuations = crate::runtime::continuation::ContinuationQueue::default();
                continuations.rearm(journal.parked_turns());
                continuations
            }
        };

        // Brain selection, in precedence order:
        //   1. an explicit brain (test injection) always wins;
        //   2. under the `openhuman` feature, an attached harness pool + a
        //      hosted-inference config routes cognition through the embedded
        //      openhuman runtime (a real agent turn per operator message);
        //   3. otherwise hosted mode plus a credential selects the hosted
        //      Medulla brain (over an injected or, under `medulla`, a networked
        //      transport);
        //   4. every other combination degrades to the offline echo brain so
        //      the default build stays green.
        // Captured from the harness arm below so the workflow engine (#29) can
        // reuse the same metered pool/deps the brain runs on.
        #[cfg(feature = "openhuman")]
        let mut wf_runner: Option<Arc<dyn WorkflowRunner>> = None;
        // Issue #111: one in-flight steer registry per company, shared between the
        // harness deps (which register runs + install the steer hook) and the
        // runtime (which the operator steer routes reach). Captured from the
        // harness arm so `CompanyRuntime::set_steer` can be wired downstream.
        #[cfg(feature = "openhuman")]
        let mut steer_registry: Option<crate::company::steer::InflightRegistry> = None;
        // Issue #383: likewise one run supervisor per company, shared between the
        // harness deps (whose orchestrator `run_workflow` tool registers its runs)
        // and the runtime (which the console's cancel route reaches). Captured
        // from the harness arm for `CompanyRuntime::set_run_supervisor` below.
        #[cfg(feature = "openhuman")]
        let mut run_supervisor: Option<crate::runtime::RunSupervisor> = None;
        // Issue #337: the company's planning station, built from the SAME
        // `Arc<dyn HarnessModel>` the roster's agents run on so a console BYOK
        // switch re-points planning exactly as it re-points a turn. Captured
        // from the harness arm — where the provider is constructed — for
        // `CompanyRuntime::set_planner` below, the pattern the three handles
        // above already use.
        #[cfg(feature = "openhuman")]
        let mut planner: Option<Arc<crate::harness::planning::TaskPlanner>> = None;

        // Load the persisted record BEFORE constructing the brain so the brain's
        // in-memory record carries the operator overlays (team, desk memberships,
        // desk order/hierarchy, operator-created desks) rather than empty lists.
        // The brain's `desk_lead` resolver reads `overlay_desk_order`, so seeding
        // it from the persisted record is what makes a `/desks/{id}/order` reorder
        // take effect on routing after the runtime is rebuilt — otherwise desk
        // chats keep routing to the pre-reorder lead. `save` only writes
        // company.toml + meta.json; the append-only ledger file is left untouched,
        // so an existing ledger survives a rebuild.
        let existing = store.load(&id).await?;
        let lifecycle = existing
            .as_ref()
            .map(|r| r.lifecycle.clone())
            .unwrap_or_else(|| "running".to_string());
        let overlay_agents = existing
            .as_ref()
            .map(|r| r.overlay_agents.clone())
            .unwrap_or_default();
        let overlay_desk_members = existing
            .as_ref()
            .map(|r| r.overlay_desk_members.clone())
            .unwrap_or_default();
        let overlay_desk_order = existing
            .as_ref()
            .map(|r| r.overlay_desk_order.clone())
            .unwrap_or_default();
        let overlay_desks = existing
            .as_ref()
            .map(|r| r.overlay_desks.clone())
            .unwrap_or_default();
        // Issue #168: the runtime-authored workflow graph bodies. A rebuild that
        // dropped these would delete every workflow the console created on a
        // hosted tenant — they have no on-disk copy to fall back to.
        let overlay_workflows = existing
            .as_ref()
            .map(|r| r.overlay_workflows.clone())
            .unwrap_or_default();
        // Issue #343: the operator-set daily spend caps. Carried across the
        // rebuild for the same reason as the workflow bodies — the manifest is a
        // read-only boot snapshot on a hosted tenant, so dropping these would
        // silently revert every console-set cap to the number baked into the
        // image on the next restart, which is the exact failure #343 exists to
        // end.
        let overlay_budgets = existing
            .as_ref()
            .map(|r| r.overlay_budgets.clone())
            .unwrap_or_default();
        // Issue #208: `[workflows].enabled` is the one manifest field a runtime
        // write mutates (`create_company_workflow` pushes the new id alongside
        // the overlay body, in the same save). Rebuilding the record from the
        // freshly-parsed seed manifest therefore clobbered every console-created
        // workflow's enablement on each boot, leaving an orphaned graph body the
        // `list_workflows` route and the GraphQL `Company.workflows` resolver —
        // both of which read this field — no longer reported.
        //
        // Fold the merged list into `self.manifest` ONCE, here, rather than at
        // the two `CompanyRecord` construction sites below: both build their
        // manifest from `self.manifest.clone()`, and one of them is inside the
        // `openhuman`-gated harness arm that the default build never compiles.
        // Mutating the source keeps the two records in agreement by construction
        // instead of by a duplicated line only one CI job type-checks.
        //
        // Every other `self.manifest` reader in `build` (grants, tool provider,
        // channels, inference, MCP, plan, policy gate, place) reads fields this
        // merge never touches.
        self.manifest.workflows.enabled =
            merge_enabled_workflows(&self.manifest.workflows.enabled, &overlay_workflows);
        // Issue #85: carry an existing record's source-template provenance
        // forward across the rebuild (a rebuild never re-stamps it); on the very
        // first launch, stamp from the value the launch path recorded (a slug for
        // a template directory, `None` for a raw-manifest provision).
        let template_provenance = existing
            .as_ref()
            .and_then(|r| r.template_provenance.clone())
            .or_else(|| self.template_provenance.clone());
        let ledger = existing.map(|r| r.ledger).unwrap_or_default();

        let brain: Arc<dyn Brain> = match self.brain {
            Some(brain) => brain,
            None => {
                // Clone the pool so it stays available for the downstream
                // `CompanyRuntime::harness` wiring — the brain and the runtime
                // deliberately share one pool.
                #[cfg(feature = "openhuman")]
                let harness_brain: Option<Arc<dyn Brain>> = match self.harness.clone() {
                    Some(pool) => {
                        // The platform-injected managed default (endpoint +
                        // credential) is the lowest-precedence inference source.
                        let env_default =
                            self.harness_inference
                                .as_ref()
                                .map(|(config, _)| EnvDefault {
                                    base_url: config.base_url.clone(),
                                    // A handle, not a value: the managed
                                    // credential may be a platform token that
                                    // rotates in place, so it is read per request.
                                    credential: config.credential.clone(),
                                });
                        // An explicit `OPENCOMPANY_INFERENCE_MODEL` flattens the
                        // whole roster to one workload; otherwise each agent keeps
                        // its tier-derived model and the tenant
                        // `[inference].models` table maps it (`None` = no override).
                        let model_override = self
                            .harness_inference
                            .as_ref()
                            .and_then(|(_, model)| model.clone());

                        // Is any inference source configured — a runtime console
                        // override, a manifest `[inference]` section, or the
                        // managed env default? A corrupt runtime config degrades
                        // to "unconfigured" (managed/echo brain) rather than
                        // bricking boot.
                        let configured = inference::resolve_effective(
                            &id,
                            &self.manifest.inference,
                            env_default.as_ref(),
                            secrets.as_ref(),
                        )
                        .await
                        .unwrap_or_else(|err| {
                            tracing::warn!(
                                company = %id,
                                error = %err,
                                "resolving inference config failed; keeping the managed/echo brain"
                            );
                            None
                        })
                        .is_some();

                        if configured {
                            // One shared steer registry; the same handle is wired
                            // onto the runtime below.
                            let steer = crate::company::steer::InflightRegistry::new();
                            steer_registry = Some(steer.clone());
                            // Same shape, same reason (issue #383).
                            let supervisor = crate::runtime::RunSupervisor::new();
                            run_supervisor = Some(supervisor.clone());
                            // Resolve the company's effective MCP servers to data
                            // (manifest ∪ runtime index, credentials materialized)
                            // before building sync deps. A corrupt index degrades
                            // to no MCP servers rather than bricking boot.
                            let mcp_servers = crate::company::mcp::resolve_effective(
                                &id,
                                &self.manifest.mcp_servers,
                                secrets.as_ref(),
                            )
                            .await
                            .unwrap_or_else(|err| {
                                tracing::warn!(
                                    company = %id,
                                    error = %err,
                                    "resolving MCP servers failed; agents get no MCP tools"
                                );
                                Vec::new()
                            });
                            // Issue #110: resolve the per-tenant Composio config
                            // at boot from the company secret store (its own
                            // token, if any) else this instance's platform
                            // identity, plus the manifest toolkit allowlist and
                            // the env URL override, falling back to the tenant API
                            // base so staging Composio follows staging. Only
                            // companies that explicitly grant `composio` resolve at
                            // all; with no credential obtainable it stays `None`
                            // (fail closed). `HarnessPool::ensure` re-resolves this
                            // each turn so a console token change takes effect
                            // without restart.
                            let composio_config = if crate::company::grants_composio_explicit(
                                &self.manifest.tools.allow,
                            ) {
                                use crate::app::config::EnvSource;
                                let toolkits = self.manifest.tools.composio.toolkits.clone();
                                let env = crate::app::config::ProcessEnv;
                                let url =
                                    env.get(crate::harness::composio::COMPOSIO_BACKEND_URL_ENV);
                                let api_url =
                                    env.get(crate::harness::composio::TINYHUMANS_API_URL_ENV);
                                crate::harness::composio::TenantComposio::resolve(
                                    &id,
                                    secrets.as_ref(),
                                    toolkits,
                                    url,
                                    api_url,
                                    // Falls back to this instance's platform
                                    // identity when the company stored no token
                                    // of its own.
                                    crate::company::TinyhumansTokenSource::from_env(&env)
                                        .map(Arc::new),
                                )
                                .await
                            } else {
                                None
                            };
                            let deps = HarnessDeps {
                                // A per-tenant provider that re-resolves the
                                // effective inference config on every turn, so a
                                // console BYOK switch takes effect next turn with
                                // no rebuild.
                                provider: Arc::new(TenantProvider::new(
                                    id.clone(),
                                    secrets.clone(),
                                    self.manifest.inference.clone(),
                                    env_default,
                                )),
                                // Static fallback only; `HarnessPool::run` reads
                                // the live slug from the provider per turn.
                                provider_slug: "managed".to_string(),
                                context: context.clone(),
                                store: store.clone(),
                                meter: Some(fs_ops.clone()),
                                workspace_root: home.join("harness"),
                                model_override,
                                tasks: Some(ops.tasks.clone()),
                                artifacts: Some(ops.artifacts.clone()),
                                // Skill read surface (#28): the operator delta
                                // store + the company source dir (`companies/<name>`,
                                // held as `seed_dir`) whose `skills/` subtree
                                // supplies the committed bundles.
                                skills: Some(ops.skills.clone()),
                                skills_source_dir: self.seed_dir.clone(),
                                skills_registry: self.skills_registry.clone(),
                                mcp_servers,
                                // Orchestrator read surface + delegation queue
                                // (#53): the company's facts + event log ground
                                // `query_company`; a fresh queue per company backs
                                // the delegation tools the brain drains.
                                facts: Some(ops.facts.clone()),
                                events: Some(events.clone()),
                                delegations: crate::harness::orchestrator::DelegationQueue::default(
                                ),
                                // Issue #67: an empty runner handle, filled just
                                // below once the `HarnessWorkflowRunner` is built,
                                // so the orchestrator's `run_workflow` tool reaches
                                // the runner without a construction cycle.
                                workflow_runner:
                                    crate::harness::orchestrator::WorkflowRunnerHandle::default(),
                                // Error-hardening cell: a fresh MCP-failure queue
                                // the `OcMcpCallTool` decorator fills and the brain
                                // drains; and a LIVE secret-store handle so
                                // `HarnessPool::ensure` can re-resolve the effective
                                // MCP set each turn (MCP-freshness) rather than the
                                // snapshot frozen here at boot.
                                mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
                                pending_publishes:
                                    crate::harness::publish::PendingPublishQueue::default(),
                                // Issue #339: the workflow half of a card's
                                // output link, staged by the orchestrator's
                                // workflow tools and drained by the same
                                // dispatch settle that drains the publishes.
                                workflow_refs:
                                    crate::harness::workflow_refs::WorkflowRefQueue::default(),
                                // Issue #243: share the runtime's grant set, so a
                                // grant the runtime mints on approve is the one
                                // this agent's policy redeems on re-issue.
                                approval_requests:
                                    crate::harness::policy::ApprovalRequestQueue::with_grants(
                                        grants.clone(),
                                    ),
                                secrets: Some(secrets.clone()),
                                // Cell A: the `web` toolbelt SSRF allowlist.
                                // Domains come straight from the manifest.
                                web_allowed_domains: self
                                    .manifest
                                    .tools
                                    .web_allowed_domains
                                    .clone(),
                                // #113 P2: the company source dir so a workflow's
                                // `sub_workflow` node resolves a child by id from
                                // `workflows/<id>.toml`. Same origin as the skills
                                // source dir but a distinct seam.
                                workflow_source_dir: self.seed_dir.clone(),
                                // Issue #108: `capabilities` is the no-plan
                                // fallback (identity). When `[plan]` is set,
                                // `HarnessPool::ensure` resolves the per-tenant
                                // filter from the meter each turn and overwrites
                                // it; `plan` carries the resolved budget so it can.
                                capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
                                plan:
                                    crate::harness::capability_budget::CapabilityPlan::from_manifest(
                                        &self.manifest.plan,
                                    ),
                                // Issue #109: the MANAGED media-generation
                                // backend, resolved from the environment by the
                                // CLI (`attach_harness` → `media_backend_from_env`)
                                // and never from a tenant secret. `None` fails
                                // closed — `build_agent` wires no media tools even
                                // for a company that grants `media`.
                                media: self.media_backend.clone(),
                                // Issue #238: the MANAGED search backend,
                                // resolved from the environment by the CLI
                                // (`attach_harness` → `search_backend_from_env`)
                                // and never from a tenant secret. `None` fails
                                // closed. The daily call cap comes from THIS
                                // company's manifest, so one process-wide
                                // credential still yields a per-company budget;
                                // the clone carries the shared ledger, so every
                                // agent of the company draws on one budget
                                // rather than one each.
                                search: self.search_backend.clone().map(|backend| {
                                    backend.with_daily_call_cap(
                                        self.manifest
                                            .tools
                                            .search_daily_calls
                                            .unwrap_or(crate::company::DEFAULT_SEARCH_DAILY_CALLS),
                                    )
                                }),
                                // Issue #110: the per-tenant Composio config
                                // resolved above (token from the secret store,
                                // never an env/platform key). `None` fails closed.
                                composio: composio_config,
                                steer,
                                run_supervisor: supervisor,
                                // Issue #170: the ports an `output` node's
                                // `destination` needs. This is the ONLY site
                                // that wires them — every other `HarnessDeps`
                                // construction leaves `None`, which fails closed
                                // with a loud "not wired" row on the run result.
                                // All four are already resolved above: the
                                // company's own mailbox handle, its inboxes (for
                                // the established-thread gate and the outbound
                                // audit record), its user directory (how `owner`
                                // resolves server-side), and the wired channel
                                // adapters (always at least `operator`).
                                delivery: Some(crate::workflows::WorkflowDeliveryDeps {
                                    mail: self.mail.clone(),
                                    inbox: inbox.clone(),
                                    users: ops.users.clone(),
                                    channels: channels.clone(),
                                    // Issue #227: the same gate and journal the
                                    // runtime gets below — one approvals queue,
                                    // so a report parked by a workflow lands in
                                    // the operator's list beside one parked by
                                    // an agent, and rehydrates on restart with
                                    // its original id. Both halves or neither,
                                    // by construction.
                                    parking: Some(crate::workflows::DeliveryParking {
                                        approvals: gate.clone(),
                                        journal: journal.clone(),
                                    }),
                                }),
                                // Issue #237: the SAME workspace handle the
                                // console's REST/GraphQL surface writes through
                                // (`ops.workspace`, seeded just above), so an
                                // operator's edit to `Standards/` is what the
                                // next agent turn reads. The tools cache
                                // nothing, so no rebuild is needed for an edit
                                // to take effect.
                                workspace: Some(ops.workspace.clone()),
                            };
                            let record = CompanyRecord {
                                id: id.clone(),
                                manifest: self.manifest.clone(),
                                ledger: Vec::new(),
                                lifecycle: lifecycle.clone(),
                                // Seed the brain from the persisted overlays so
                                // desk routing (`desk_lead` → `effective_desk_members`
                                // → `overlay_desk_order`) reflects the operator's
                                // current hierarchy, not the blueprint default.
                                overlay_agents: overlay_agents.clone(),
                                overlay_desk_members: overlay_desk_members.clone(),
                                overlay_desk_order: overlay_desk_order.clone(),
                                overlay_desks: overlay_desks.clone(),
                                overlay_workflows: overlay_workflows.clone(),
                                overlay_budgets: overlay_budgets.clone(),
                                template_provenance: template_provenance.clone(),
                            };
                            // Workflow agent nodes execute on the same pool as the
                            // brain — clone before both moves into `HarnessBrain`.
                            let runner: Arc<dyn WorkflowRunner> =
                                Arc::new(HarnessWorkflowRunner::new(
                                    pool.clone(),
                                    deps.clone(),
                                    record.clone(),
                                ));
                            // Issue #67: fill the shared handle on `deps` (a clone
                            // of which the runner holds, and which moves into the
                            // brain below) so the orchestrator's `run_workflow` tool
                            // reaches this runner. The handle stores a `Weak`; the
                            // strong ref lives on the runtime via
                            // `set_workflow_runner`, so this is not a strong cycle.
                            deps.workflow_runner.set(&runner);
                            wf_runner = Some(runner);
                            // Issue #337: built from these same deps, so it
                            // shares the tenant provider and the model override
                            // rather than resolving a second credential path
                            // that could drift from the roster's.
                            planner = Some(Arc::new(
                                crate::harness::planning::TaskPlanner::from_deps(&deps),
                            ));
                            Some(Arc::new(
                                // Issue #242: the same run store the dispatch
                                // choke point mints into and the boot reaper
                                // sweeps, so an attempt's trace, cost and
                                // status all land on the row it opened.
                                HarnessBrain::new(pool, deps, record).with_runs(ops.runs.clone()),
                            ) as Arc<dyn Brain>)
                        } else {
                            // Do not degrade silently (issue #174): an openhuman
                            // build with no resolvable inference source disables
                            // the harness path and falls through to
                            // `select_hosted_or_echo`. Say that much and no more —
                            // whether Usage then reads zero depends on what that
                            // selection lands on (hosted Medulla with a credential
                            // and a transport does meter per cycle; the echo brain
                            // runs no model at all), so promising zero tokens here
                            // would be wrong half the time. The inference-status
                            // route reports the path actually selected.
                            tracing::warn!(
                                company = %id,
                                "no inference source resolved (no runtime override, no manifest [inference], no managed default); \
                                 the openhuman harness is disabled for this company — falling back to hosted/echo cognition, \
                                 see the inference-status route for the path actually selected"
                            );
                            None
                        }
                    }
                    None => None,
                };
                #[cfg(not(feature = "openhuman"))]
                let harness_brain: Option<Arc<dyn Brain>> = None;

                if let Some(brain) = harness_brain {
                    brain
                } else {
                    let mut tool_catalog: Vec<ToolManifestEntry> = self
                        .manifest
                        .tools
                        .allow
                        .iter()
                        .map(|name| ToolManifestEntry {
                            name: name.clone(),
                            description: None,
                            input_schema: None,
                        })
                        .collect();
                    // Issue #176: advertise the delegation tools to Medulla on
                    // the hosted path, so a hosted company's orchestrator can
                    // delegate exactly as the harness one does. The device
                    // services the resulting tool-call frames in `CycleHostImpl`
                    // (a durable board-card hand-off) with no local cognition.
                    // De-duped against `tools.allow` so a manifest that already
                    // lists a delegation tool is not advertised twice.
                    for entry in crate::runtime::delegation_tools::delegation_manifest_entries() {
                        if !tool_catalog.iter().any(|e| e.name == entry.name) {
                            tool_catalog.push(entry);
                        }
                    }
                    select_hosted_or_echo(
                        self.brain_mode.unwrap_or(BrainMode::Hosted),
                        self.credential,
                        self.transport,
                        self.api_url,
                        &id,
                        tool_catalog,
                    )
                }
            }
        };

        // Materialize the manifest so status/roster loads have a record to read.
        // The persisted overlays + provenance + ledger + lifecycle were read above
        // (before the brain was constructed, so the brain could be seeded from
        // them), and must not be dropped here: not the operator-added teammates,
        // desk memberships, desk order, operator-created desks, runtime-authored
        // workflow graphs, nor the source-template provenance.
        //
        // The record's manifest is NOT simply the seed manifest (issue #208). A
        // rebuild never rewrites the version-controlled `company.toml` *file* —
        // that much has always held — but the manifest it *persists onto the
        // record* is the seed manifest with `[workflows].enabled` merged against
        // the surviving overlay bodies, so a workflow enabled at runtime is still
        // enabled after a restart. Every other manifest field is seed-authoritative:
        // the seed wins, and for `[tools]` / `[policy]` that is a security property
        // — a record-wins merge would let a runtime grant outlive the operator
        // revoking it in version control.
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: self.manifest.clone(),
                ledger,
                lifecycle,
                overlay_agents,
                overlay_desk_members,
                overlay_desk_order,
                overlay_desks,
                overlay_workflows,
                overlay_budgets,
                template_provenance,
            })
            .await?;

        // Economy: an injected economy wins; otherwise the `tinyplace` feature
        // auto-wires one for a discoverable company with a handle. Going-public
        // (the paid handle-claim) fires only when discovery is enabled.
        let going_public = self
            .discoverable_override
            .unwrap_or(self.manifest.place.discoverable);
        let economy: Option<Arc<dyn AgentEconomy>> = match self.economy {
            Some(economy) => Some(economy),
            None => {
                maybe_build_economy(
                    &self.manifest,
                    &home,
                    &id,
                    store.clone(),
                    self.tinyplace_api_url.clone(),
                    going_public,
                )
                .await
            }
        };

        let mut runtime = CompanyRuntime::new(
            id.clone(),
            brain,
            store,
            events,
            memory,
            context,
            tools,
            channels,
            economy.clone(),
            gate,
            journal,
            secrets,
            inbox,
            self.mail,
            ops,
            feedback,
            filer,
            grants,
        );

        // The seed dir is the company's on-disk source directory
        // (`companies/<name>`); record it so read resolvers can find committed
        // skills/workflows content on the serve path.
        runtime.set_source_dir(self.seed_dir.clone());

        // Issue #290: adopt the outgoing runtime's serialising mutexes. Two
        // runtimes for one company each holding their own `serial` would let two
        // cycles run at once against a store whose `save` writes the whole
        // record; two `task_writes` would let two board edits each validate
        // against a snapshot predating the other. Adopting them is also what
        // makes the quiesce drain mean something after the swap.
        if let Some(h) = handover.as_ref() {
            runtime.adopt_locks(h.serial.clone(), h.task_writes.clone());
        }
        runtime.adopt_continuations(continuations);

        // MCP uses OpenHuman's process-global live connection registry. Keep a
        // runtime-owned config for this OpenCompany home so REST and agents see
        // the same installed servers, and reconnect persisted installs without
        // delaying company boot.
        //
        // A rebuild adopts the live one and does **not** re-boot it: the connect
        // map is keyed by server id and shared process-wide, so re-dialling would
        // replace connections the outgoing runtime's agents may still be
        // mid-call on.
        #[cfg(feature = "mcp")]
        {
            match handover.as_ref().and_then(|h| h.mcp.clone()) {
                Some(mcp) => runtime.set_mcp(mcp),
                None => {
                    let mcp = Arc::new(crate::harness::mcp::McpRuntime::new(home.join("mcp")));
                    runtime.set_mcp(mcp.clone());
                    tokio::spawn(async move { mcp.boot().await });
                }
            }
        }

        // WS4: attach the embedded harness pool when one was provided. On a
        // rebuild the outgoing pool wins over any freshly minted one, so each
        // agent's conversation history survives the swap instead of being
        // silently dropped.
        #[cfg(feature = "openhuman")]
        if let Some(harness) = handover
            .as_ref()
            .and_then(|h| h.harness.clone())
            .or_else(|| self.harness.clone())
        {
            runtime.set_harness(harness);
        }

        // Issue #111: attach the same steer registry the harness deps hold, so the
        // operator steer routes and the in-flight strip reach the runs the brain
        // registers. Only present on the harness path; the default build leaves
        // the runtime's registry empty (every steer is `not in flight`).
        #[cfg(feature = "openhuman")]
        if let Some(registry) = steer_registry {
            runtime.set_steer(registry);
        }

        // Issue #383: attach the same run supervisor the harness deps hold, so a
        // run started by the orchestrator's `run_workflow` tool lands in the map
        // the console's cancel route reads. On the default build the runtime keeps
        // the empty one it was constructed with — nothing can start a run there,
        // so every cancel is a clean 404.
        #[cfg(feature = "openhuman")]
        if let Some(supervisor) = run_supervisor {
            runtime.set_run_supervisor(supervisor);
        }

        // #29: install the workflow runner captured from the harness arm so
        // `POST /workflows/{wid}/run` executes instead of reporting `not_wired`.
        #[cfg(feature = "openhuman")]
        if let Some(wf_runner) = wf_runner {
            runtime.set_workflow_runner(wf_runner);
        }

        // Issue #337: install the planning station, so a card dragged into
        // Planning is actually planned rather than resting in a column nothing
        // reads. Without it — the default build, or an openhuman build with no
        // resolvable inference source — the column stays inert exactly as it
        // was before #337, and the boot sweep returns anything left there.
        //
        // Deliberately NOT inherited across a rebuild, unlike the harness pool
        // above. A pool carries each agent's conversation history, which is why
        // dropping it would lose something; a planner carries a model handle and
        // a set of in-flight card ids. Rebuilding it from the successor's deps
        // is what makes a console BYOK switch reach planning, and the in-flight
        // set it leaves behind is empty of anything that matters — a pass
        // interrupted by a rebuild has no settle to reach the board, and the
        // card it was planning is recovered by the next boot's sweep.
        #[cfg(feature = "openhuman")]
        if let Some(planner) = planner {
            runtime.set_planner(planner);
        }

        // Boot lifecycle step 3: going-public. Best-effort and non-blocking —
        // any failure degrades to "private" with a warning and never fails boot.
        //
        // Skipped on a rebuild (issue #290): the handle claim is a paid,
        // networked, once-per-boot action, and a company that is already public
        // does not become more public by claiming again. Firing it on every
        // inference save would spend money for nothing.
        if handover.is_none() {
            maybe_go_public(
                &economy,
                &self.manifest,
                &id,
                going_public,
                self.host_base_url.as_deref(),
            )
            .await;
        }

        Ok(runtime)
    }
}

/// Seeds a company's workspace tree from `companies/<name>/workspace/**` using
/// the WS1 walker. Ids are minted per node; parents are created before children
/// because [`walk_workspace`](crate::company::workspace_seed::walk_workspace)
/// returns nodes sorted by relative path.
async fn seed_workspace(
    workspace: &dyn WorkspaceStore,
    id: &CompanyId,
    seed_dir: &std::path::Path,
) -> Result<()> {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::company::workspace_seed::{NodeKind as SeedKind, walk_workspace};
    use crate::ports::now_millis;
    use crate::ports::workspace::{NodeKind, WorkspaceNode};

    let nodes = walk_workspace(&seed_dir.join("workspace"))?;
    let mut path_to_id: HashMap<PathBuf, String> = HashMap::new();
    for seed in nodes {
        let name = match seed.rel_path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let parent_id = seed
            .rel_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .and_then(|p| path_to_id.get(p).cloned());
        let kind = match seed.kind {
            SeedKind::Folder => NodeKind::Folder,
            SeedKind::Markdown => NodeKind::File,
        };
        let node = WorkspaceNode {
            id: crate::ports::generate_id(),
            name,
            kind,
            parent_id,
            updated_at_millis: now_millis(),
        };
        workspace.create(id, &node, seed.content.as_deref()).await?;
        path_to_id.insert(seed.rel_path.clone(), node.id);
    }
    Ok(())
}

/// Auto-wires the tiny.place economy for a discoverable company (feature build).
///
/// Returns `None` unless `[place].discoverable` is set and a `@handle` is
/// present; a missing/unreadable identity key degrades to `None` with a warning.
///
/// # The one place the Agent-Card replayer is attached (issue #454)
///
/// This function is the **only** production path that builds a concrete
/// [`TinyplaceEconomy`], and the last point at which its outbox is still
/// reachable: the return type erases it to `Arc<dyn AgentEconomy>`, a trait with
/// no flush surface, which is precisely how the outbox came to have a `drain()`
/// whose only caller lived in its own test module. So
/// [`spawn_outbox_replayer`](crate::economy::adapter::spawn_outbox_replayer) is
/// called here, before the erasure, and calling it is what entitles
/// `publish_card` to answer `Ok(())` while offline. Delete the call and every
/// offline publish starts erroring instead of lying — which is the failure
/// direction we want, and which a test asserts.
#[cfg(feature = "tinyplace")]
async fn maybe_build_economy(
    manifest: &CompanyManifest,
    home: &std::path::Path,
    id: &CompanyId,
    store: Arc<dyn CompanyStore>,
    tinyplace_api_url: Option<String>,
    going_public: bool,
) -> Option<Arc<dyn AgentEconomy>> {
    use crate::economy::adapter::{OUTBOX_REPLAY_INTERVAL, spawn_outbox_replayer};
    use crate::economy::signer::load_or_create_signer;
    use crate::economy::{HttpTinyplaceClient, TinyplaceEconomy};
    use crate::store::paths::Bundle;

    if !(manifest.place.discoverable && manifest.company.handle.is_some()) {
        return None;
    }

    let bundle = Bundle::new(home.to_path_buf(), id);
    let signer = match load_or_create_signer(&bundle).await {
        Ok(signer) => Arc::new(signer),
        Err(err) => {
            tracing::warn!(company = %id, "tiny.place identity unavailable ({err}); staying private");
            return None;
        }
    };

    let base = tinyplace_api_url
        .unwrap_or_else(|| crate::app::config::DEFAULT_TINYPLACE_API_URL.to_string());
    let client = Arc::new(HttpTinyplaceClient::new(base, signer.clone()));
    let economy = Arc::new(
        TinyplaceEconomy::new(
            client,
            signer,
            store,
            id.clone(),
            manifest.budget.monthly_usd,
        )
        .going_public(going_public),
    );
    // Issue #454: attach the replayer while the concrete type is still in hand.
    // Without this line the outbox has no drain, and `publish_card` knows it —
    // it stops queuing and starts returning the unreachable error instead.
    spawn_outbox_replayer(&economy, OUTBOX_REPLAY_INTERVAL);
    Some(economy)
}

/// Default build: no tiny.place economy is linked.
#[cfg(not(feature = "tinyplace"))]
async fn maybe_build_economy(
    _manifest: &CompanyManifest,
    _home: &std::path::Path,
    _id: &CompanyId,
    _store: Arc<dyn CompanyStore>,
    _tinyplace_api_url: Option<String>,
    _going_public: bool,
) -> Option<Arc<dyn AgentEconomy>> {
    None
}

/// Runs the going-public flow best-effort: `ensure_registered` then, on success,
/// `publish_card`. Every outcome degrades to a warning; boot never blocks.
#[cfg(feature = "tinyplace")]
async fn maybe_go_public(
    economy: &Option<Arc<dyn AgentEconomy>>,
    manifest: &CompanyManifest,
    id: &CompanyId,
    going_public: bool,
    host_base_url: Option<&str>,
) {
    use crate::economy::build_agent_card;
    use crate::ports::types::{CompanyIdentity, RegistrationState};

    if !going_public {
        return;
    }
    let (Some(economy), Some(handle)) = (economy, manifest.company.handle.clone()) else {
        return;
    };
    let identity = CompanyIdentity {
        company: id.clone(),
        handle,
    };
    match economy.ensure_registered(&identity).await {
        Ok(RegistrationState::Registered { .. }) => {
            let base = host_base_url
                .map(str::to_string)
                .unwrap_or_else(|| format!("http://{}", crate::app::config::DEFAULT_BIND));
            let card = build_agent_card(manifest, &base);
            // Issue #454: an error here now means the card was NOT queued and
            // nothing will retry it — the offline-but-recoverable case returns
            // `Ok` and logs its own "queued for replay" line from the adapter, so
            // the two are no longer the same message.
            if let Err(err) = economy.publish_card(&identity, &card).await {
                tracing::warn!(
                    company = %id,
                    "tiny.place publish_card failed ({err}); the card was not queued for replay, \
                     so the directory entry stays stale until the next boot"
                );
            } else {
                tracing::info!(company = %id, handle = %identity.handle, "tiny.place: discoverable (public)");
            }
        }
        Ok(RegistrationState::Unregistered) => {
            tracing::warn!(company = %id, "tiny.place: private (awaiting funding/identity approval)");
        }
        Err(err) => {
            tracing::warn!(company = %id, "tiny.place go-public failed ({err}); staying private");
        }
    }
}

/// Default build: going-public is a no-op with no tiny.place economy.
#[cfg(not(feature = "tinyplace"))]
async fn maybe_go_public(
    _economy: &Option<Arc<dyn AgentEconomy>>,
    _manifest: &CompanyManifest,
    _id: &CompanyId,
    _going_public: bool,
    _host_base_url: Option<&str>,
) {
}

/// Chooses the hosted Medulla brain or the degraded echo brain.
///
/// An injected transport is used verbatim; otherwise the networked transport is
/// built under the `medulla` feature (and degrades to echo without it).
fn select_hosted_or_echo(
    mode: BrainMode,
    credential: Option<SecretValue>,
    transport: Option<Arc<dyn MedullaTransport>>,
    api_url: Option<String>,
    id: &CompanyId,
    tool_catalog: Vec<ToolManifestEntry>,
) -> Arc<dyn Brain> {
    match (mode, credential) {
        (BrainMode::Hosted, Some(credential)) => match transport {
            Some(transport) => Arc::new(HostedMedullaBrain::new(
                transport,
                id,
                id.as_ref(),
                credential,
                tool_catalog,
            )),
            None => build_networked_brain(credential, api_url, id, tool_catalog),
        },
        // Sidecar mode routes to the local sidecar brain under the `sidecar`
        // feature, degrading to echo when no sidecar process is configured.
        (BrainMode::Sidecar, _) => build_sidecar_brain(id, tool_catalog),
        // No credential in hosted mode: offline echo.
        _ => Arc::new(EchoBrain::new()),
    }
}

/// Builds the local-sidecar brain over the stdio transport with a host-bound
/// inference client.
///
/// The offline end-to-end test injects a fully mocked [`SidecarBrain`] through
/// [`RuntimeBuilder::with_brain`], so this path only needs to serve a real
/// deployment. Because no sidecar process endpoint is configured today, it
/// degrades to the offline echo brain with a warning — mirroring
/// [`build_networked_brain`]'s degrade-to-echo behavior. Rebuild with
/// `--features sidecar` and inject a configured transport to drive a real
/// sidecar.
#[cfg(feature = "sidecar")]
fn build_sidecar_brain(id: &CompanyId, _tool_catalog: Vec<ToolManifestEntry>) -> Arc<dyn Brain> {
    tracing::warn!(
        company = %id,
        "sidecar brain requires a configured sidecar process; using the offline echo brain"
    );
    Arc::new(EchoBrain::new())
}

/// Default build: the sidecar brain is not linked, so sidecar mode degrades to
/// the offline echo brain. Rebuild with `--features sidecar` for the sidecar
/// brain.
#[cfg(not(feature = "sidecar"))]
fn build_sidecar_brain(_id: &CompanyId, _tool_catalog: Vec<ToolManifestEntry>) -> Arc<dyn Brain> {
    Arc::new(EchoBrain::new())
}

/// Builds the hosted brain over the networked `HttpSocketTransport`.
#[cfg(feature = "medulla")]
fn build_networked_brain(
    credential: SecretValue,
    api_url: Option<String>,
    id: &CompanyId,
    tool_catalog: Vec<ToolManifestEntry>,
) -> Arc<dyn Brain> {
    use crate::brain::medulla::HttpSocketTransport;

    let base = api_url.unwrap_or_else(|| crate::app::config::DEFAULT_API_URL.to_string());
    let transport = Arc::new(HttpSocketTransport::new(base, credential.clone()));
    Arc::new(HostedMedullaBrain::new(
        transport,
        id,
        id.as_ref(),
        credential,
        tool_catalog,
    ))
}

/// Default build: no network transport is linked, so hosted-with-credential
/// degrades to the offline echo brain. Rebuild with `--features medulla` to get
/// real hosted cognition.
#[cfg(not(feature = "medulla"))]
fn build_networked_brain(
    _credential: SecretValue,
    _api_url: Option<String>,
    _id: &CompanyId,
    _tool_catalog: Vec<ToolManifestEntry>,
) -> Arc<dyn Brain> {
    Arc::new(EchoBrain::new())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::openhuman::MockOpenHumanRpc;
    use crate::ports::types::ToolCall;

    fn tmp_home(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("tempdir")
    }

    /// Issue #242, the property this whole PR exists to create, proven across a
    /// real restart: a host killed mid-run leaves the attempt's **partial trace
    /// intact**, and the next boot settles the row it stranded.
    ///
    /// The kill is simulated by simply not settling — which is exactly what a
    /// `SIGKILL` looks like from the store's side, and the reason the boot
    /// reaper's claim is a proof rather than a timeout heuristic: a cycle is a
    /// process-local spawn, so an active row at boot cannot belong to anything
    /// still alive.
    #[tokio::test]
    async fn a_killed_run_keeps_its_partial_trace_and_is_settled_on_the_next_boot() {
        use crate::ports::runs::{NewRun, RunStatus, RunStepRecord};
        use crate::ports::types::{EventSeq, TurnStep, TurnStepKind, TurnStepStatus};

        let home = tmp_home("opencompany-run-restart-");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = CompanyId::new("acme");

        // --- boot 1: a card is dispatched, starts, writes two steps… and dies.
        {
            let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
                .with_id(id.clone())
                .build()
                .await
                .expect("first boot");
            let runs = rt.runs();
            runs.create_run(
                &id,
                NewRun {
                    id: "run-1".to_string(),
                    task_id: "t-1".to_string(),
                    agent_id: "ceo".to_string(),
                },
            )
            .await
            .expect("mint");
            runs.begin_run(&id, "run-1", EventSeq::new(3))
                .await
                .expect("begin");
            for (step_seq, label, status) in [
                (0u32, "Reading the brief", TurnStepStatus::Ok),
                (1, "Searching the web", TurnStepStatus::Running),
            ] {
                runs.append_run_step(
                    &id,
                    &RunStepRecord {
                        run_id: "run-1".to_string(),
                        step_seq,
                        at_millis: 100 + step_seq as u64,
                        step: TurnStep {
                            kind: TurnStepKind::ToolCall,
                            status,
                            label: label.to_string(),
                            detail: None,
                            elapsed_ms: None,
                            ..TurnStep::default()
                        },
                    },
                )
                .await
                .expect("append step");
            }
            // …and the process is gone. Nothing settles the row.
        }

        // --- boot 2: the builder's reaper runs before anything is dispatched.
        let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .expect("second boot");

        let reaped = rt
            .runs()
            .get_run(&id, "run-1")
            .await
            .expect("read")
            .expect("the row survives the restart");
        assert_eq!(
            reaped.status,
            RunStatus::Failed,
            "an attempt whose process died must not still claim to be running"
        );
        assert_eq!(
            reaped.error.as_deref(),
            Some(crate::ports::runs::ORPHAN_ERROR)
        );

        // The whole point: the steps written before the kill are still there,
        // including the tool call that never got to finish.
        let steps = rt
            .runs()
            .list_run_steps(&id, "run-1")
            .await
            .expect("list steps");
        assert_eq!(steps.len(), 2, "the partial trace must survive the restart");
        assert_eq!(steps[0].step.label, "Reading the brief");
        assert_eq!(steps[0].step.status, TurnStepStatus::Ok);
        assert_eq!(
            steps[1].step.status,
            TurnStepStatus::Running,
            "the call that was in flight when the host died reads as in flight"
        );
    }

    #[test]
    fn slugifies_display_names() {
        assert_eq!(company_id_from_name("Acme Co!").as_ref(), "acme-co");
        assert_eq!(company_id_from_name("  Widgets  ").as_ref(), "widgets");
        assert_eq!(company_id_from_name("***").as_ref(), "company");
    }

    /// The shipped companies actually hand their agents the workspace tools
    /// (issue #177, gap 2).
    ///
    /// Before this, `[tools].allow` listed no `workspace` grant while every
    /// agent enumerated its tools explicitly — and per-agent grants are narrowed
    /// by the company allow-list, so *no* agent received even `workspace_list`.
    /// The tools existed (#237) and no shipped company could reach them, which
    /// made the "an agent writes a note, the operator sees it" round trip
    /// impossible out of the box.
    ///
    /// Reads are namespace-covered and writes need an explicit grant, so this
    /// also pins the asymmetry: readers must NOT come out write-capable.
    #[cfg(feature = "openhuman")]
    #[test]
    fn shipped_companies_grant_the_workspace_tools() {
        use crate::company::grants_workspace_write_explicit;
        use crate::harness::build::grants_cover;

        for company in ["e2e_harness", "openhuman_demo"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("companies")
                .join(company);
            let manifest = CompanyManifest::from_path(&path)
                .unwrap_or_else(|e| panic!("{company} manifest must parse: {e}"));

            for agent in &manifest.agents {
                let grants = agent_effective_grants(&manifest.tools.allow, &agent.tools);
                assert!(
                    grants_cover(&grants, "workspace"),
                    "{company}/{} must reach the workspace tools; effective grants: {grants:?}",
                    agent.id
                );
                // Only the writer edits notes; everyone else is read-only, so a
                // reader can never overwrite operator-owned guidance.
                assert_eq!(
                    grants_workspace_write_explicit(&grants),
                    agent.id == "writer",
                    "{company}/{} write access is wrong; effective grants: {grants:?}",
                    agent.id
                );
                // Every shipped agent asks for `mcp:*`, and `agent_effective_grants`
                // intersects that request with the company allow-list — so an
                // allow-list that omits it silently hands the agent no MCP at all.
                // Both manifests were in exactly that state before this test
                // existed (`openhuman_demo` had no allow-list, which covers
                // nothing, so its agents resolved to an empty toolbelt). Asserted
                // here because the symptom is a missing capability, not an error:
                // nothing logs, nothing fails, the tools are simply absent.
                //
                // Probed with `grant_matches` against a concrete `mcp:<server>`
                // name rather than `grants_cover`: MCP grants are colon-namespaced
                // (`mcp:*`, `mcp:notion`) while `grants_cover` only understands the
                // dot form, so it answers `false` for a grant list that plainly
                // contains `mcp:*`.
                if agent.tools.iter().any(|tool| tool == "mcp:*") {
                    assert!(
                        grants
                            .iter()
                            .any(|grant| grant_matches(grant, "mcp:any-server")),
                        "{company}/{} asks for mcp:* but the allow-list does not \
                         cover it; effective grants: {grants:?}",
                        agent.id
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn user_auth_stores_default_to_fs_and_are_reachable() {
        use crate::ports::{
            InviteRecord, LoginCodeRecord, SessionRecord, UserRecord, UserRole, UserStatus,
        };

        let home_dir = tmp_home("oc-users-");
        let home = home_dir.path().to_path_buf();
        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");
        // No with_users/with_sessions/with_login_codes override: the builder must
        // fall back to the shared fs backend rather than leaving a hole.
        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();

        runtime
            .users()
            .upsert_user(
                &id,
                &UserRecord {
                    id: "u1".into(),
                    email: "ada@example.com".into(),
                    display_name: None,
                    role: UserRole::Admin,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 1,
                    last_seen_at_millis: None,
                    updated_at_millis: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            runtime
                .users()
                .find_user_by_email(&id, "ada@example.com")
                .await
                .unwrap()
                .unwrap()
                .id,
            "u1"
        );

        runtime
            .users()
            .upsert_invite(
                &id,
                &InviteRecord {
                    id: "i1".into(),
                    email: "bob@example.com".into(),
                    role: UserRole::Member,
                    invited_by: "manifest".into(),
                    created_at_millis: 1,
                    expires_at_millis: 10,
                    accepted_at_millis: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(runtime.users().list_invites(&id).await.unwrap().len(), 1);

        runtime
            .sessions()
            .create(
                &id,
                &SessionRecord {
                    id: "s1".into(),
                    token_hash: "hash".into(),
                    user_id: "u1".into(),
                    created_at_millis: 1,
                    expires_at_millis: 10,
                    user_agent: None,
                },
            )
            .await
            .unwrap();
        assert!(
            runtime
                .sessions()
                .find_by_token_hash(&id, "hash")
                .await
                .unwrap()
                .is_some()
        );

        runtime
            .login_codes()
            .create(
                &id,
                &LoginCodeRecord {
                    id: "c1".into(),
                    code_hash: "codehash".into(),
                    email: "ada@example.com".into(),
                    created_at_millis: 1,
                    expires_at_millis: 10,
                    consumed_at_millis: None,
                },
            )
            .await
            .unwrap();
        assert!(
            runtime
                .login_codes()
                .consume(&id, "codehash", 2)
                .await
                .unwrap()
                .is_some()
        );
    }

    /// Issue #242: a run row left active by a dead host is reclaimed at the next
    /// boot, and a parked one is not.
    ///
    /// The store is the default fs backend over the same home, so the second
    /// `build()` is a genuine restart of the same company — this asserts the
    /// reaper is *wired into boot*, not merely that the port function works
    /// (which the conformance suite covers for all three backends).
    #[tokio::test]
    async fn boot_reaps_runs_stranded_by_a_previous_host() {
        use crate::ports::runs::{NewRun, ORPHAN_ERROR, RunOutcome, RunStatus};

        let home_dir = tmp_home("oc-run-reap-");
        let home = home_dir.path().to_path_buf();
        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");
        let spec = |run: &str, task: &str| NewRun {
            id: run.to_string(),
            task_id: task.to_string(),
            agent_id: "ceo".to_string(),
        };

        let first_boot = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let runs = first_boot.runs().clone();

        // Two attempts the host is "running", and one parked for a person.
        runs.create_run(&id, spec("pending", "card-a"))
            .await
            .unwrap();
        runs.create_run(&id, spec("running", "card-b"))
            .await
            .unwrap();
        runs.begin_run(&id, "running", crate::ports::types::EventSeq::new(1))
            .await
            .unwrap();
        runs.create_run(&id, spec("review", "card-c"))
            .await
            .unwrap();
        runs.begin_run(&id, "review", crate::ports::types::EventSeq::new(2))
            .await
            .unwrap();
        runs.finish_run(&id, "review", RunOutcome::new(RunStatus::WaitingApproval))
            .await
            .unwrap();

        // The host dies here — no settle, no journal entry, nothing.
        drop(first_boot);

        let second_boot = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let runs = second_boot.runs();

        for stranded in ["pending", "running"] {
            let run = runs.get_run(&id, stranded).await.unwrap().unwrap();
            assert_eq!(
                run.status,
                RunStatus::Failed,
                "{stranded} outlived its process and must be reclaimed"
            );
            assert_eq!(run.error.as_deref(), Some(ORPHAN_ERROR));
            assert!(run.finished_at_millis.is_some());
        }

        // Parked is not orphaned: this one is waiting on a person, and a restart
        // must not throw that work away.
        let review = runs.get_run(&id, "review").await.unwrap().unwrap();
        assert_eq!(review.status, RunStatus::WaitingApproval);
        assert_eq!(review.error, None);

        assert!(runs.list_stale_active(&id).await.unwrap().is_empty());
    }

    /// Issue #337, the crash-truthfulness half: reaping the *row* is not enough
    /// — the **card** has to leave In Progress too, or the board keeps claiming
    /// work that provably is not being done and nothing will ever re-drive it
    /// (`task_enters_in_progress` fires on the transition, which already
    /// happened).
    ///
    /// Three things at once, because they are one behaviour: the stranded card
    /// returns to To-do with the reason readable on it, a card parked for a
    /// person is untouched, and re-dispatching the returned card starts a
    /// **new** attempt rather than resuming the dead one.
    #[tokio::test]
    async fn boot_returns_a_stranded_card_and_leaves_a_parked_one_alone() {
        use crate::ports::runs::{NewRun, ORPHAN_ERROR, RunOutcome, RunStatus};
        use crate::ports::tasks::{COLUMN_IN_PROGRESS, COLUMN_PAUSED, COLUMN_TODO, TaskRecord};

        let home_dir = tmp_home("oc-run-reap-cards-");
        let home = home_dir.path().to_path_buf();
        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");
        let card = |task: &str, column: &str| TaskRecord {
            id: task.to_string(),
            title: "Draft the spec".to_string(),
            note: Some("[maya] started".to_string()),
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee: "ceo".to_string(),
            updated_at_millis: 1,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
        };

        let first_boot = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let runs = first_boot.runs().clone();
        let tasks = first_boot.tasks().clone();

        // `card-a` is being worked by an attempt that will die with the host.
        // `card-b` is parked for a person, and its run is parked with it.
        tasks
            .upsert(&id, &card("card-a", COLUMN_IN_PROGRESS))
            .await
            .unwrap();
        tasks
            .upsert(&id, &card("card-b", COLUMN_PAUSED))
            .await
            .unwrap();
        runs.create_run(
            &id,
            NewRun {
                id: "run-a".to_string(),
                task_id: "card-a".to_string(),
                agent_id: "ceo".to_string(),
            },
        )
        .await
        .unwrap();
        runs.begin_run(&id, "run-a", crate::ports::types::EventSeq::new(1))
            .await
            .unwrap();
        runs.create_run(
            &id,
            NewRun {
                id: "run-b".to_string(),
                task_id: "card-b".to_string(),
                agent_id: "ceo".to_string(),
            },
        )
        .await
        .unwrap();
        runs.begin_run(&id, "run-b", crate::ports::types::EventSeq::new(2))
            .await
            .unwrap();
        runs.finish_run(&id, "run-b", RunOutcome::new(RunStatus::Paused))
            .await
            .unwrap();

        // The host dies here — `kill -9`, no settle, no journal entry.
        drop(first_boot);

        let second_boot = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let tasks = second_boot.tasks();
        let after = |task: &'static str| {
            let tasks = tasks.clone();
            let id = id.clone();
            async move {
                tasks
                    .list(&id)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|t| t.id == task)
                    .expect("card survives the restart")
            }
        };

        // The stranded card is back in To-do, and says why in words an operator
        // can act on rather than silently.
        let stranded = after("card-a").await;
        assert_eq!(stranded.column, COLUMN_TODO);
        let note = stranded.note.expect("note");
        assert!(note.contains(ORPHAN_ERROR), "{note}");
        assert!(
            note.contains("[maya] started"),
            "the note is append-only; what the run already said must survive: {note}"
        );

        // The parked card is exactly as it was. Its run was `Paused`, so the
        // reaper never saw it — and even if it had, the mover only ever leaves
        // In Progress.
        let parked = after("card-b").await;
        assert_eq!(parked.column, COLUMN_PAUSED);
        assert_eq!(parked.note.as_deref(), Some("[maya] started"));

        // Re-dispatching the returned card mints a **new** attempt. Nothing
        // resurrects `run-a`, which is terminal.
        let runs = second_boot.runs();
        assert_eq!(
            runs.get_run(&id, "run-a").await.unwrap().unwrap().status,
            RunStatus::Failed
        );
        let next = runs
            .create_run(
                &id,
                NewRun {
                    id: "run-a2".to_string(),
                    task_id: "card-a".to_string(),
                    agent_id: "ceo".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            next.attempt, 2,
            "a card that came back to To-do is re-tried, not resumed"
        );
    }

    #[tokio::test]
    async fn workspace_seeds_once_and_operator_deletions_stick() {
        let home_dir = tmp_home("oc-seed-");
        let home = home_dir.path().to_path_buf();
        // A company definition dir with a workspace subtree.
        let seed_dir = home.join("def");
        std::fs::create_dir_all(seed_dir.join("workspace/Brand")).unwrap();
        std::fs::write(seed_dir.join("workspace/README.md"), "# Root").unwrap();
        std::fs::write(seed_dir.join("workspace/Brand/voice.md"), "# Voice").unwrap();

        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .with_seed_dir(seed_dir.clone())
            .build()
            .await
            .unwrap();
        // Seeded: README.md, Brand/, Brand/voice.md.
        let tree = runtime.workspace().tree(&id).await.unwrap();
        assert_eq!(tree.len(), 3);
        assert!(tree.iter().any(|n| n.name == "voice.md"));

        // Operator deletes a node.
        let voice = tree.iter().find(|n| n.name == "voice.md").unwrap();
        runtime.workspace().delete(&id, &voice.id).await.unwrap();

        // Rebuild: the deletion sticks (no re-seed).
        drop(runtime);
        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .with_seed_dir(seed_dir)
            .build()
            .await
            .unwrap();
        let tree = runtime.workspace().tree(&id).await.unwrap();
        assert_eq!(
            tree.len(),
            2,
            "workspace re-seeded despite operator deletion"
        );
        assert!(!tree.iter().any(|n| n.name == "voice.md"));
        // Sanity: the record store still loads.
        assert!(runtime.store().load(&id).await.unwrap().is_some());
    }

    /// Issue #85: the launch path's template provenance is stamped onto the
    /// record at first build, survives a rebuild that supplies no provenance
    /// (carried forward), and a company built with no provenance records `None`.
    #[tokio::test]
    async fn template_provenance_stamped_at_launch_and_carried_forward() {
        let home_dir = tmp_home("oc-prov-");
        let home = home_dir.path().to_path_buf();
        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");
        let provenance = TemplateProvenance {
            source_id: "agentic_law_firm".to_string(),
            version: None,
            path: Some("companies/agentic_law_firm".to_string()),
        };

        // First launch from a template: provenance is stamped onto the record.
        let runtime = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .with_template_provenance(provenance.clone())
            .build()
            .await
            .unwrap();
        let stamped = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(stamped.template_provenance.as_ref(), Some(&provenance));
        drop(runtime);

        // Rebuild without re-supplying provenance: the record carries it forward.
        let runtime = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let carried = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            carried.template_provenance,
            Some(provenance),
            "provenance was dropped on rebuild"
        );
        drop(runtime);

        // A company built with no provenance (raw-manifest provision) records None.
        let other = CompanyId::new("raw");
        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(other.clone())
            .build()
            .await
            .unwrap();
        let raw = runtime.store().load(&other).await.unwrap().unwrap();
        assert!(raw.template_provenance.is_none());
    }

    fn parse(toml_src: &str) -> CompanyManifest {
        toml::from_str(toml_src).expect("valid manifest")
    }

    /// A bodiless overlay stub — `merge_enabled_workflows` only reads the id.
    fn overlay(id: &str) -> OverlayWorkflow {
        OverlayWorkflow {
            id: id.to_string(),
            toml: String::new(),
        }
    }

    #[test]
    fn merge_enabled_appends_overlay_only_ids() {
        let merged = merge_enabled_workflows(
            &["seed_one".to_string()],
            &[overlay("console_made"), overlay("also_console")],
        );
        assert_eq!(merged, vec!["seed_one", "console_made", "also_console"]);
    }

    #[test]
    fn merge_enabled_dedupes_at_the_seed_position() {
        // `shared` is in both lists: it keeps its seed slot (first), and the
        // overlay does not append a second copy at the end.
        let merged = merge_enabled_workflows(
            &["shared".to_string(), "seed_only".to_string()],
            &[overlay("shared"), overlay("overlay_only")],
        );
        assert_eq!(merged, vec!["shared", "seed_only", "overlay_only"]);
    }

    #[test]
    fn merge_enabled_first_boot_leaves_seed_unchanged() {
        let seed = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(merge_enabled_workflows(&seed, &[]), seed);
    }

    #[test]
    fn merge_enabled_preserves_order_and_dedupes_within_each_list() {
        let merged = merge_enabled_workflows(
            &["b".to_string(), "a".to_string(), "b".to_string()],
            &[overlay("z"), overlay("a"), overlay("z")],
        );
        assert_eq!(merged, vec!["b", "a", "z"]);
    }

    #[test]
    fn merge_enabled_of_nothing_is_empty() {
        assert!(merge_enabled_workflows(&[], &[]).is_empty());
    }

    // --- Issue #208: two-build rebuild semantics over one home dir ----------

    /// A seed manifest with the roster the create-path draft below references.
    fn wf_manifest(extra: &str) -> CompanyManifest {
        parse(&format!(
            "[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n\
             [[agent]]\nid=\"assistant\"\nrole=\"Assistant\"\n{extra}"
        ))
    }

    /// The minimal valid three-node graph the create path accepts, mirroring
    /// `workflow_create`'s own `valid_draft`.
    fn wf_draft(id: &str, name: &str) -> crate::company::RawWorkflow {
        use crate::company::{RawEdge, RawNode, RawWorkflow};
        let node = |id: &str, kind: &str, name: &str, agent: Option<&str>| RawNode {
            id: id.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            summary: None,
            agent: agent.map(str::to_string),
            schedule: None,
            config: None,
            on_error: None,
            retry: None,
            requires_approval: None,
            destination: None,
        };
        RawWorkflow {
            id: id.to_string(),
            name: name.to_string(),
            description: Some("A tiny graph.".to_string()),
            nodes: vec![
                node("start", "trigger", "Start", None),
                node("worker", "agent", "Worker", Some("assistant")),
                node("done", "output", "Report", None),
            ],
            edges: vec![
                RawEdge {
                    from: "start".to_string(),
                    to: "worker".to_string(),
                    label: None,
                },
                RawEdge {
                    from: "worker".to_string(),
                    to: "done".to_string(),
                    label: Some("ok".to_string()),
                },
            ],
        }
    }

    /// Issue #208: a workflow created at runtime through the real create path
    /// (console `POST …/workflows` / orchestrator `create_workflow`) is still
    /// enabled after the runtime is rebuilt on the same home dir — and the
    /// `enabled_workflow_ids` accessor both REST `list_workflows` and the
    /// GraphQL `Company.workflows` resolver read still reports it.
    #[tokio::test]
    async fn runtime_created_workflow_stays_enabled_across_a_rebuild() {
        let home_dir = tempfile::Builder::new()
            .prefix("oc-wf-enabled-")
            .tempdir()
            .expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let manifest = wf_manifest("[workflows]\nenabled=[\"seeded_pipeline\"]\n");
        let id = CompanyId::new("acme");

        let runtime = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        // The real writer: overlay body + enabled id in one save.
        crate::company::create_company_workflow(
            &id,
            None,
            runtime.store(),
            None,
            wf_draft("daily_digest", "Daily Digest"),
        )
        .await
        .unwrap();
        let created = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            created.manifest.workflows.enabled,
            vec!["seeded_pipeline", "daily_digest"]
        );
        drop(runtime);

        // Rebuild from the same seed manifest — the seed knows nothing about
        // `daily_digest`, so this is exactly the boot that used to lose it.
        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let rebuilt = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            rebuilt.manifest.workflows.enabled,
            vec!["seeded_pipeline", "daily_digest"],
            "the rebuild dropped the runtime-enabled workflow"
        );
        assert!(
            rebuilt
                .overlay_workflows
                .iter()
                .any(|w| w.id == "daily_digest"),
            "the graph body should be untouched by this fix"
        );
        // What the REST + GraphQL workflow lists actually read.
        assert_eq!(
            runtime.enabled_workflow_ids().await.unwrap(),
            vec!["seeded_pipeline", "daily_digest"]
        );
    }

    /// Issue #208: a record written during the bug era — overlay graph body
    /// intact, its enabled id already wiped by an earlier restart — is healed
    /// by the next rebuild, with no migration.
    #[tokio::test]
    async fn rebuild_reenables_a_bug_era_orphaned_overlay_body() {
        let home_dir = tempfile::Builder::new()
            .prefix("oc-wf-heal-")
            .tempdir()
            .expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let manifest = wf_manifest("");
        let id = CompanyId::new("acme");

        let runtime = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let store = runtime.store().clone();
        let mut record = store.load(&id).await.unwrap().unwrap();
        // Bug-era shape: body present, `enabled` clobbered back to the seed's.
        record.overlay_workflows.push(OverlayWorkflow {
            id: "orphaned".to_string(),
            toml: "id = \"orphaned\"\n".to_string(),
        });
        record.manifest.workflows.enabled.clear();
        store.save(&record).await.unwrap();
        drop(runtime);

        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        assert_eq!(
            runtime.enabled_workflow_ids().await.unwrap(),
            vec!["orphaned"],
            "an orphaned bug-era overlay body was not re-enabled"
        );
    }

    /// Issue #208: `[workflows].enabled` is the ONLY merged field. A
    /// seed-authoritative field that diverged on the record — here a
    /// runtime-granted tool, the case where record-wins would let privilege
    /// outlive a seed rollback — is overwritten by the seed on rebuild.
    #[tokio::test]
    async fn rebuild_keeps_every_other_manifest_field_seed_authoritative() {
        let home_dir = tempfile::Builder::new()
            .prefix("oc-wf-seedwins-")
            .tempdir()
            .expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let manifest = wf_manifest("[tools]\nallow=[\"memory.*\"]\n");
        let id = CompanyId::new("acme");

        let runtime = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let store = runtime.store().clone();
        let mut record = store.load(&id).await.unwrap().unwrap();
        record.manifest.tools.allow.push("email.*".to_string());
        record.manifest.company.name = "Renamed At Runtime".to_string();
        store.save(&record).await.unwrap();
        drop(runtime);

        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let rebuilt = runtime.store().load(&id).await.unwrap().unwrap();
        assert_eq!(
            rebuilt.manifest.tools.allow,
            vec!["memory.*"],
            "a runtime tool grant survived a seed rollback"
        );
        assert_eq!(rebuilt.manifest.company.name, "Acme");
    }

    /// Issue #208: an enabled id with no surviving graph body — a seed entry
    /// the operator deleted from `company.toml` — is dropped rather than
    /// carried forward forever with nothing to run.
    #[tokio::test]
    async fn rebuild_drops_an_enabled_id_with_no_body() {
        let home_dir = tempfile::Builder::new()
            .prefix("oc-wf-zombie-")
            .tempdir()
            .expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("acme");

        // First boot from a seed that enables `retired`.
        let runtime = RuntimeBuilder::new(
            home.clone(),
            wf_manifest("[workflows]\nenabled=[\"retired\"]\n"),
        )
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
        assert_eq!(
            runtime.enabled_workflow_ids().await.unwrap(),
            vec!["retired"]
        );
        drop(runtime);

        // The operator removes it from the version-controlled seed. No overlay
        // body was ever written for it, so nothing carries it forward.
        let runtime = RuntimeBuilder::new(home.clone(), wf_manifest(""))
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        assert!(
            runtime.enabled_workflow_ids().await.unwrap().is_empty(),
            "a bodiless enabled id zombied past its removal from the seed"
        );
    }

    #[test]
    fn effective_grants_no_roster_is_company_allow() {
        let manifest = parse("[company]\nname=\"X\"\n[tools]\nallow=[\"email.*\",\"email.*\"]\n");
        assert_eq!(effective_grants(&manifest), vec!["email.*".to_string()]);
    }

    #[test]
    fn effective_grants_agent_without_tools_inherits_allow() {
        let manifest = parse(
            "[company]\nname=\"X\"\n[[agent]]\nid=\"a\"\nrole=\"A\"\n[tools]\nallow=[\"email.*\"]\n",
        );
        assert_eq!(effective_grants(&manifest), vec!["email.*".to_string()]);
    }

    #[test]
    fn effective_grants_agent_tools_intersect_allow() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            [[agent]]
            id = "a"
            role = "A"
            tools = ["email.send", "payment.send"]
            [tools]
            allow = ["email.*"]
            "#,
        );
        // `email.send` is covered by `email.*`; `payment.send` is not.
        assert_eq!(effective_grants(&manifest), vec!["email.send".to_string()]);
    }

    fn openhuman_manifest() -> CompanyManifest {
        parse(
            r#"
            [company]
            name = "Acme"
            [[agent]]
            id = "ceo"
            role = "Chief"
            [tools]
            provider = "openhuman"
            allow = ["email.*"]
            [channels.email]
            provider = "openhuman"
            "#,
        )
    }

    #[tokio::test]
    async fn healthy_openhuman_wires_provider_and_channel() {
        let dir = tempfile::tempdir().unwrap();
        let rpc = Arc::new(MockOpenHumanRpc::new().with_result(
            "openhuman.tools_invoke",
            serde_json::json!({ "ok": true, "output": {} }),
        ));
        let runtime = RuntimeBuilder::new(dir.path(), openhuman_manifest())
            .with_openhuman_rpc(rpc.clone())
            .build()
            .await
            .unwrap();

        // Operator + the openhuman-backed email channel.
        assert_eq!(runtime.channels.len(), 2);
        assert!(runtime.channels.iter().any(|c| c.channel_id() == "email"));

        // A granted call routes through the OpenHuman transport.
        let result = runtime
            .tools
            .invoke(
                runtime.id(),
                ToolCall {
                    tool: "email.send".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(rpc.call_count(), 1);
    }

    #[tokio::test]
    async fn unreachable_openhuman_degrades_to_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let rpc = Arc::new(MockOpenHumanRpc::new().unhealthy());
        let runtime = RuntimeBuilder::new(dir.path(), openhuman_manifest())
            .with_openhuman_rpc(rpc.clone())
            .build()
            .await
            .unwrap();

        // No openhuman channel is added when the daemon is unreachable.
        assert_eq!(runtime.channels.len(), 1);
        assert_eq!(runtime.channels[0].channel_id(), "operator");

        // Tools degrade to the grant-enforcing built-in: ungranted rejected,
        // granted returns a well-formed not-implemented result — and the RPC
        // transport is never touched.
        let ungranted = runtime
            .tools
            .invoke(
                runtime.id(),
                ToolCall {
                    tool: "payment.send".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            ungranted,
            crate::OpenCompanyError::ToolNotGranted(t) if t == "payment.send"
        ));

        let granted = runtime
            .tools
            .invoke(
                runtime.id(),
                ToolCall {
                    tool: "email.send".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        assert!(!granted.ok);
        // Only the boot-time `health()` probe touched the transport.
        assert_eq!(rpc.call_count(), 0);
    }

    #[cfg(feature = "tinyplace")]
    #[tokio::test]
    async fn discoverable_company_registers_and_publishes_without_blocking() {
        use crate::economy::signer::LocalSigner;
        use crate::economy::{MockTinyplaceClient, TinyplaceEconomy};
        use crate::ports::AgentEconomy;
        use crate::ports::CompanyStore;
        use crate::store::FsCompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let manifest = parse(
            r#"
            [company]
            name = "Acme"
            handle = "acme"
            [place]
            discoverable = true
            skills = [{ id = "seo.audit", price_usd = "25.00" }]
            "#,
        );
        let id = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> = Arc::new(FsCompanyStore::new(dir.path().to_path_buf()));
        let signer = Arc::new(LocalSigner::generate());
        let mock = Arc::new(MockTinyplaceClient::new());
        let economy: Arc<dyn AgentEconomy> = Arc::new(
            TinyplaceEconomy::new(mock.clone(), signer, store, id.clone(), None).going_public(true),
        );

        let runtime = RuntimeBuilder::new(dir.path().to_path_buf(), manifest)
            .with_id(id)
            .with_economy(economy)
            .with_discoverable(true)
            .build()
            .await
            .unwrap();

        // The economy is wired, and boot registered + published the card.
        assert!(runtime.has_economy());
        assert_eq!(mock.count("register_name"), 1, "boot claimed the handle");
        assert_eq!(mock.count("put_agent"), 1, "boot published the card");
    }

    /// Issue #454, at the construction path that actually runs in production.
    ///
    /// The economy above is *injected*, so it proves nothing about how a real
    /// company's economy is assembled. This one goes through
    /// [`maybe_build_economy`] — the only production builder of a
    /// [`TinyplaceEconomy`] — and asserts the property that only holds when the
    /// outbox replayer was attached before the type erasure: an offline
    /// `publish_card` returns `Ok`, because there is now something that will send
    /// the card it queued.
    ///
    /// **This test is the guard on that one line.** Delete
    /// `spawn_outbox_replayer(&economy, …)` from `maybe_build_economy` and the
    /// publish below returns `tinyplace_unreachable` instead — verified by doing
    /// exactly that.
    #[cfg(feature = "tinyplace")]
    #[tokio::test]
    async fn discoverable_path_builds_an_economy_that_can_degrade_offline() {
        use crate::economy::build_agent_card;
        use crate::ports::CompanyStore;
        use crate::ports::types::CompanyIdentity;
        use crate::store::FsCompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let manifest = parse(
            r#"
            [company]
            name = "Acme"
            handle = "acme"
            [place]
            discoverable = true
            "#,
        );
        let id = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> = Arc::new(FsCompanyStore::new(dir.path().to_path_buf()));

        // A port nothing listens on: every call is refused, which is exactly the
        // `unreachable` condition the outbox exists for. Bound and released so
        // the OS confirms it is free, rather than guessing a number.
        let dead = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap()
        };

        let economy = maybe_build_economy(
            &manifest,
            dir.path(),
            &id,
            store,
            Some(format!("http://{dead}")),
            true,
        )
        .await
        .expect("a discoverable company with a handle gets an economy");

        let identity = CompanyIdentity {
            company: id.clone(),
            handle: "acme".to_string(),
        };
        let card = build_agent_card(&manifest, "http://127.0.0.1:8080");
        economy
            .publish_card(&identity, &card)
            .await
            .expect("the built economy degrades offline, which it may only do with a replayer");
    }

    /// Spawns an in-process OpenAI-compatible stub that answers every
    /// chat-completion with `marker`, so a harness turn can run without a real
    /// inference backend. Mirrors the provider-test helper of the same name.
    #[cfg(feature = "openhuman")]
    async fn spawn_stub(marker: &'static str) -> String {
        use axum::routing::post;
        use axum::{Json, Router};

        let app = Router::new().route(
            "/chat/completions",
            post(move || async move {
                Json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": marker } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// Builder-level regression for the `overlay_desk_order` seeding path (#133).
    /// The harness test `desk_order_change_updates_routing_after_rebuild` exercises
    /// `brain_over(record)` directly; this one drives the real
    /// [`RuntimeBuilder::build`] wiring end-to-end: a persisted record carries a
    /// NON-EMPTY `overlay_desk_order` that promotes `eng2` over the blueprint lead
    /// `eng1`, and after `build()` a desk-addressed cycle must run on `eng2` — the
    /// reordered lead — proving the builder seeds the operator order into the brain
    /// rather than an empty default. The harness records each turn under a
    /// `task-outcome/{agent_id}` context chunk, which is the observable seam.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn build_seeds_desk_order_into_brain_routing() {
        use crate::harness::HarnessPool;
        use crate::ports::types::{CompanyEvent, OverlayDeskOrder};
        use crate::store::{FsCompanyStore, FsContextStore};

        let home_dir = tmp_home("oc-seed-order-");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("order-co");

        // A desk `eng` whose blueprint lead is `eng1` (declared first).
        let manifest = parse(
            r#"
            [company]
            name = "Order Co"

            [policy]
            mode = "full"

            [[agent]]
            id = "eng1"
            role = "Engineer One"

            [[agent]]
            id = "eng2"
            role = "Engineer Two"

            [[group_chat]]
            id = "eng"
            name = "Engineering"
            members = ["eng1", "eng2"]
            "#,
        );

        // Persist a record whose operator order promotes `eng2` above `eng1`.
        let store = FsCompanyStore::new(home.clone());
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: vec![OverlayDeskOrder {
                    desk_id: "eng".to_string(),
                    ordered: vec!["eng2".to_string(), "eng1".to_string()],
                }],
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        // Build the runtime with an embedded harness pool + a stub inference
        // backend, so `build()` constructs the seeded `HarnessBrain`.
        let stub = spawn_stub("desk lead reply").await;
        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .with_harness(Arc::new(HarnessPool::new()))
            .with_harness_inference(
                HostedProviderConfig {
                    base_url: stub,
                    credential: crate::company::Credential::from_value("k"),
                    extra_headers: Vec::new(),
                },
                None,
            )
            .build()
            .await
            .unwrap();

        // A message addressed to the `eng` desk must be answered by the reordered
        // lead `eng2`, not the blueprint lead `eng1`.
        runtime
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                parent: None,
                text: "who leads?".to_string(),
                by: None,
                chat: Some("eng".to_string()),
            }])
            .await
            .expect("cycle");

        // The harness writes the turn under `task-outcome/{responder}`; the
        // responder must be the reordered lead.
        let context: Arc<dyn ContextStore> = Arc::new(FsContextStore::new(home.clone()));
        let outcomes = context.list(&id, "task-outcome/").await.unwrap();
        let labels: Vec<&str> = outcomes.iter().map(|m| m.label.as_str()).collect();
        assert!(
            labels.contains(&"task-outcome/eng2"),
            "desk turn did not route to the reordered lead eng2; saw {labels:?}"
        );
        assert!(
            !labels.contains(&"task-outcome/eng1"),
            "desk turn routed to the blueprint lead eng1 — the builder dropped the operator desk order; saw {labels:?}"
        );
    }
}
