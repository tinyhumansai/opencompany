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
use crate::company::runtime::{CompanyRuntime, OpsStores};
use crate::feedback::github::{GitHubClient, RateLimiter};
use crate::feedback::service::FeedbackFiler;
use crate::feedback::store::FeedbackStore;
use crate::feedback::tinyhumans::TinyHumansClient;
use crate::feedback::tool::BuiltinToolProvider;
use crate::feedback::types::ConsentMode;
#[cfg(feature = "openhuman")]
use crate::harness::provider::{HostedProvider, HostedProviderConfig};
#[cfg(feature = "openhuman")]
use crate::harness::{HarnessBrain, HarnessDeps};
use crate::openhuman::rpc::OpenHumanRpc;
use crate::openhuman::{OpenHumanChannelAdapter, OpenHumanToolProvider};
use crate::policy::ManifestApprovalGate;
#[cfg(feature = "openhuman")]
use crate::ports::WorkflowRunner;
use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
use crate::ports::{
    AgentEconomy, Brain, ChannelAdapter, CompanyStore, ContextStore, EventLog, FactStore,
    InboxStore, LoginCodeStore, MemoryStore, SecretStore, SessionStore, SkillStateStore, TaskStore,
    ToolProvider, UsageMeter, UserStore, WorkspaceStore,
};
use crate::runtime::channel::{OPERATOR_CHANNEL, OperatorChannel};
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
    tasks: Option<Arc<dyn TaskStore>>,
    workspace: Option<Arc<dyn WorkspaceStore>>,
    facts: Option<Arc<dyn FactStore>>,
    usage: Option<Arc<dyn UsageMeter>>,
    skills: Option<Arc<dyn SkillStateStore>>,
    users: Option<Arc<dyn UserStore>>,
    sessions: Option<Arc<dyn SessionStore>>,
    login_codes: Option<Arc<dyn LoginCodeStore>>,
    seed_dir: Option<PathBuf>,
    feedback: Option<Arc<FeedbackStore>>,
    github: Option<Arc<dyn GitHubClient>>,
    tinyhumans_feedback: Option<Arc<dyn TinyHumansClient>>,
    consent: ConsentMode,
    /// WS4: the embedded openhuman harness pool. Feature-gated so the default
    /// build is unaffected; wired through to [`CompanyRuntime`] when present.
    #[cfg(feature = "openhuman")]
    harness: Option<Arc<crate::harness::HarnessPool>>,
    /// WS4: hosted-inference config (endpoint + default model) for the harness
    /// brain. With both this and [`harness`](Self::harness) set — and no
    /// explicit brain — cognition routes through the embedded openhuman runtime.
    #[cfg(feature = "openhuman")]
    harness_inference: Option<(HostedProviderConfig, String)>,
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
            tasks: None,
            workspace: None,
            facts: None,
            usage: None,
            skills: None,
            users: None,
            sessions: None,
            login_codes: None,
            seed_dir: None,
            feedback: None,
            github: None,
            tinyhumans_feedback: None,
            consent: ConsentMode::default(),
            #[cfg(feature = "openhuman")]
            harness: None,
            #[cfg(feature = "openhuman")]
            harness_inference: None,
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

    /// WS4: sets the hosted-inference config (endpoint + default model) the
    /// harness brain drives. Combined with [`with_harness`](Self::with_harness)
    /// and no explicit brain, cognition routes through the embedded openhuman
    /// runtime; without it the harness pool stays wired but unused and the
    /// runtime keeps its hosted/echo brain. Feature-gated.
    #[cfg(feature = "openhuman")]
    pub fn with_harness_inference(
        mut self,
        config: HostedProviderConfig,
        default_model: impl Into<String>,
    ) -> Self {
        self.harness_inference = Some((config, default_model.into()));
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
    pub async fn build(self) -> Result<CompanyRuntime> {
        let home = self.home;
        let id = self.id;

        let store: Arc<dyn CompanyStore> = self
            .store
            .unwrap_or_else(|| Arc::new(FsCompanyStore::new(home.clone())));
        let events: Arc<dyn EventLog> = self
            .events
            .unwrap_or_else(|| Arc::new(FsEventLog::new(home.clone())));
        let memory: Arc<dyn MemoryStore> = self
            .memory
            .unwrap_or_else(|| Arc::new(FsMemoryStore::new(home.clone())));
        let context: Arc<dyn ContextStore> = self
            .context
            .unwrap_or_else(|| Arc::new(FsContextStore::new(home.clone())));
        // Effective grants narrow the company allow-list by per-agent tools.
        let grants = effective_grants(&self.manifest);
        let openhuman = self.openhuman;

        // Feedback family: the item store, secret store (for the scrubber), and
        // filing configuration. The consent mode is also the built-in feedback
        // tool's capture mode.
        let bundle = Bundle::new(home.clone(), &id);
        let feedback = self
            .feedback
            .unwrap_or_else(|| Arc::new(FeedbackStore::new(&bundle)));
        let secrets: Arc<dyn SecretStore> = self
            .secrets
            .unwrap_or_else(|| Arc::new(FsSecretStore::new(home.clone())));
        let inbox: Arc<dyn InboxStore> = self
            .inbox
            .unwrap_or_else(|| Arc::new(FsInboxStore::new(home.clone())));
        // The WS3 console ports default to a single shared fs backend.
        let fs_ops = Arc::new(FsOps::new(home.clone()));
        let ops = OpsStores {
            tasks: self.tasks.unwrap_or_else(|| fs_ops.clone()),
            workspace: self.workspace.unwrap_or_else(|| fs_ops.clone()),
            facts: self.facts.unwrap_or_else(|| fs_ops.clone()),
            usage: self.usage.unwrap_or_else(|| fs_ops.clone()),
            skills: self.skills.unwrap_or_else(|| fs_ops.clone()),
            users: self.users.unwrap_or_else(|| fs_ops.clone()),
            sessions: self.sessions.unwrap_or_else(|| fs_ops.clone()),
            login_codes: self.login_codes.unwrap_or_else(|| fs_ops.clone()),
        };

        // Idempotent workspace seeding: only when the workspace is empty (an
        // operator's deletions must stick, so a seeded-then-emptied workspace is
        // never re-seeded). Skills need no seeding — the store holds deltas only
        // and the effective set unions company-dir skills at read time.
        if let Some(seed_dir) = &self.seed_dir
            && ops.workspace.is_empty(&id).await?
        {
            seed_workspace(ops.workspace.as_ref(), &id, seed_dir).await?;
        }

        let consent = self.consent;
        let filer = Arc::new(FeedbackFiler {
            client: self.github,
            tinyhumans: self.tinyhumans_feedback,
            repo: crate::feedback::DEFAULT_REPO.to_string(),
            consent,
            limiter: RateLimiter::default(),
            quality: crate::feedback::QualityLedger::default(),
        });

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
        let brain: Arc<dyn Brain> = match self.brain {
            Some(brain) => brain,
            None => {
                // Clone the pool so it stays available for the downstream
                // `CompanyRuntime::harness` wiring — the brain and the runtime
                // deliberately share one pool.
                #[cfg(feature = "openhuman")]
                let harness_brain: Option<Arc<dyn Brain>> =
                    match (self.harness.clone(), self.harness_inference.clone()) {
                        (Some(pool), Some((provider_config, model))) => {
                            // Resolve the company's effective MCP servers to
                            // data (manifest ∪ runtime index, credentials
                            // materialized) before building sync deps. A corrupt
                            // runtime index degrades to no MCP servers rather
                            // than bricking the company boot.
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
                            let deps = HarnessDeps {
                                provider: Arc::new(HostedProvider::new(provider_config)),
                                provider_slug: "managed".to_string(),
                                context: context.clone(),
                                store: store.clone(),
                                meter: Some(fs_ops.clone()),
                                workspace_root: home.join("harness"),
                                model_override: Some(model),
                                tasks: Some(ops.tasks.clone()),
                                // Skill read surface (#28): the operator delta
                                // store + the company source dir (`companies/<name>`,
                                // held as `seed_dir`) whose `skills/` subtree
                                // supplies the committed bundles.
                                skills: Some(ops.skills.clone()),
                                skills_source_dir: self.seed_dir.clone(),
                                mcp_servers,
                                // Orchestrator read surface + delegation queue
                                // (#53): the company's facts + event log ground
                                // `query_company`; a fresh queue per company backs
                                // the delegation tools the brain drains.
                                facts: Some(ops.facts.clone()),
                                events: Some(events.clone()),
                                delegations: crate::harness::orchestrator::DelegationQueue::default(
                                ),
                            };
                            let record = CompanyRecord {
                                id: id.clone(),
                                manifest: self.manifest.clone(),
                                ledger: Vec::new(),
                                lifecycle: "running".to_string(),
                                overlay_agents: Vec::new(),
                            };
                            // Workflow agent nodes execute on the same pool as the
                            // brain — clone before both moves into `HarnessBrain`.
                            wf_runner = Some(Arc::new(HarnessWorkflowRunner::new(
                                pool.clone(),
                                deps.clone(),
                                record.clone(),
                            ))
                                as Arc<dyn WorkflowRunner>);
                            Some(Arc::new(HarnessBrain::new(pool, deps, record)) as Arc<dyn Brain>)
                        }
                        _ => None,
                    };
                #[cfg(not(feature = "openhuman"))]
                let harness_brain: Option<Arc<dyn Brain>> = None;

                if let Some(brain) = harness_brain {
                    brain
                } else {
                    let tool_catalog: Vec<ToolManifestEntry> = self
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
        // `save` only writes company.toml + meta.json; the append-only ledger
        // file is left untouched, so an existing ledger survives a rebuild.
        let existing = store.load(&id).await?;
        let lifecycle = existing
            .as_ref()
            .map(|r| r.lifecycle.clone())
            .unwrap_or_else(|| "running".to_string());
        // Preserve the operator team overlay across rebuilds — a rebuild never
        // rewrites the version-controlled manifest, and it must not drop
        // operator-added teammates either.
        let overlay_agents = existing
            .as_ref()
            .map(|r| r.overlay_agents.clone())
            .unwrap_or_default();
        let ledger = existing.map(|r| r.ledger).unwrap_or_default();
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: self.manifest.clone(),
                ledger,
                lifecycle,
                overlay_agents,
            })
            .await?;

        // Boot replay: load the journal and rehydrate parked approvals into the
        // gate so approvals survive a restart with their original ids.
        let journal = Arc::new(RuntimeJournal::new(
            Bundle::new(home.clone(), &id).journal_jsonl(),
        ));
        journal.load().await?;

        let gate = self
            .approvals
            .unwrap_or_else(|| Arc::new(ManifestApprovalGate::new(self.manifest.policy.clone())));
        for pending in journal.pending() {
            gate.rehydrate(pending.id, pending.effect, pending.at_millis);
        }

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
            ops,
            feedback,
            filer,
        );

        // The seed dir is the company's on-disk source directory
        // (`companies/<name>`); record it so read resolvers can find committed
        // skills/workflows content on the serve path.
        runtime.set_source_dir(self.seed_dir.clone());

        // MCP uses OpenHuman's process-global live connection registry. Keep a
        // runtime-owned config for this OpenCompany home so REST and agents see
        // the same installed servers, and reconnect persisted installs without
        // delaying company boot.
        #[cfg(feature = "mcp")]
        {
            let mcp = Arc::new(crate::harness::mcp::McpRuntime::new(home.join("mcp")));
            runtime.set_mcp(mcp.clone());
            tokio::spawn(async move { mcp.boot().await });
        }

        // WS4: attach the embedded harness pool when one was provided.
        #[cfg(feature = "openhuman")]
        if let Some(harness) = self.harness.clone() {
            runtime.set_harness(harness);
        }

        // #29: install the workflow runner captured from the harness arm so
        // `POST /workflows/{wid}/run` executes instead of reporting `not_wired`.
        #[cfg(feature = "openhuman")]
        if let Some(wf_runner) = wf_runner {
            runtime.set_workflow_runner(wf_runner);
        }

        // Boot lifecycle step 3: going-public. Best-effort and non-blocking —
        // any failure degrades to "private" with a warning and never fails boot.
        maybe_go_public(
            &economy,
            &self.manifest,
            &id,
            going_public,
            self.host_base_url.as_deref(),
        )
        .await;

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
#[cfg(feature = "tinyplace")]
async fn maybe_build_economy(
    manifest: &CompanyManifest,
    home: &std::path::Path,
    id: &CompanyId,
    store: Arc<dyn CompanyStore>,
    tinyplace_api_url: Option<String>,
    going_public: bool,
) -> Option<Arc<dyn AgentEconomy>> {
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
    let economy = TinyplaceEconomy::new(
        client,
        signer,
        store,
        id.clone(),
        manifest.budget.monthly_usd,
    )
    .going_public(going_public);
    Some(Arc::new(economy))
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
            if let Err(err) = economy.publish_card(&identity, &card).await {
                tracing::warn!(company = %id, "tiny.place publish_card failed ({err}); card is stale");
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

    #[test]
    fn slugifies_display_names() {
        assert_eq!(company_id_from_name("Acme Co!").as_ref(), "acme-co");
        assert_eq!(company_id_from_name("  Widgets  ").as_ref(), "widgets");
        assert_eq!(company_id_from_name("***").as_ref(), "company");
    }

    #[tokio::test]
    async fn user_auth_stores_default_to_fs_and_are_reachable() {
        use crate::ports::{
            InviteRecord, LoginCodeRecord, SessionRecord, UserRecord, UserRole, UserStatus,
        };

        let home = std::env::temp_dir().join(format!("oc-users-{}", crate::ports::generate_id()));
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

        tokio::fs::remove_dir_all(&home).await.ok();
    }

    #[tokio::test]
    async fn workspace_seeds_once_and_operator_deletions_stick() {
        let home = std::env::temp_dir().join(format!("oc-seed-{}", crate::ports::generate_id()));
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

        std::fs::remove_dir_all(&home).ok();
    }

    fn parse(toml_src: &str) -> CompanyManifest {
        toml::from_str(toml_src).expect("valid manifest")
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
}
