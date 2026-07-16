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
use crate::company::runtime::CompanyRuntime;
use crate::feedback::github::{GitHubClient, RateLimiter};
use crate::feedback::service::FeedbackFiler;
use crate::feedback::store::FeedbackStore;
use crate::feedback::tool::BuiltinToolProvider;
use crate::feedback::types::ConsentMode;
use crate::openhuman::rpc::OpenHumanRpc;
use crate::openhuman::{OpenHumanChannelAdapter, OpenHumanToolProvider};
use crate::policy::ManifestApprovalGate;
use crate::ports::types::{CompanyId, CompanyRecord, SecretValue};
use crate::ports::{
    AgentEconomy, Brain, ChannelAdapter, CompanyStore, ContextStore, EventLog, InboxStore,
    MemoryStore, SecretStore, ToolProvider,
};
use crate::runtime::channel::{OPERATOR_CHANNEL, OperatorChannel};
use crate::runtime::journal::RuntimeJournal;
use crate::runtime::tools::{StubToolProvider, grant_matches};
use crate::store::paths::Bundle;
use crate::store::{
    FsCompanyStore, FsContextStore, FsEventLog, FsInboxStore, FsMemoryStore, FsSecretStore,
};

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
    feedback: Option<Arc<FeedbackStore>>,
    github: Option<Arc<dyn GitHubClient>>,
    consent: ConsentMode,
    /// WS4: the embedded openhuman harness pool. Feature-gated so the default
    /// build is unaffected; wired through to [`CompanyRuntime`] when present.
    #[cfg(feature = "openhuman")]
    harness: Option<Arc<crate::harness::HarnessPool>>,
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
            feedback: None,
            github: None,
            consent: ConsentMode::default(),
            #[cfg(feature = "openhuman")]
            harness: None,
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
    pub fn with_stores(self, handles: &crate::store::StorageHandles) -> Self {
        self.with_store(handles.company.clone())
            .with_events(handles.events.clone())
            .with_memory(handles.memory.clone())
            .with_context(handles.context.clone())
            .with_secrets(handles.secrets.clone())
            .with_inbox(handles.inbox.clone())
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
        let consent = self.consent;
        let filer = Arc::new(FeedbackFiler {
            client: self.github,
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

        // Brain selection: an explicit brain wins; otherwise hosted mode plus a
        // credential selects the hosted Medulla brain (over an injected or, under
        // the `medulla` feature, a networked transport). Every other combination
        // — no credential, sidecar mode, or a hosted default build with no
        // transport — degrades to the offline echo brain so the default build
        // stays green.
        let brain: Arc<dyn Brain> = match self.brain {
            Some(brain) => brain,
            None => {
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
        };

        // Materialize the manifest so status/roster loads have a record to read.
        // `save` only writes company.toml + meta.json; the append-only ledger
        // file is left untouched, so an existing ledger survives a rebuild.
        let existing = store.load(&id).await?;
        let lifecycle = existing
            .as_ref()
            .map(|r| r.lifecycle.clone())
            .unwrap_or_else(|| "running".to_string());
        let ledger = existing.map(|r| r.ledger).unwrap_or_default();
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: self.manifest.clone(),
                ledger,
                lifecycle,
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

        #[cfg_attr(not(feature = "openhuman"), allow(unused_mut))]
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
            feedback,
            filer,
        );

        // WS4: attach the embedded harness pool when one was provided.
        #[cfg(feature = "openhuman")]
        if let Some(harness) = self.harness.clone() {
            runtime.set_harness(harness);
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
