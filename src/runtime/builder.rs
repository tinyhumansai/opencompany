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
use std::str::FromStr;
use std::sync::Arc;

use crate::Result;
use crate::app::config::{AuthMode, BrainMode};
use crate::brain::medulla::MedullaTransport;
use crate::brain::medulla::wire::ToolManifestEntry;
use crate::brain::{EchoBrain, HostedMedullaBrain};
#[cfg(feature = "openhuman")]
use crate::company::inference::{self, EnvDefault};
use crate::company::runtime::{CompanyMail, CompanyRuntime, OpsStores};
use crate::company::{CompanyManifest, GroupChat, Policy};
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
    CompanyId, CompanyRecord, OverlayWorkflow, PolicyOverride, SecretValue, TemplateProvenance,
};
use crate::ports::{
    AgentEconomy, ArtifactStore, Brain, ChannelAdapter, CompanyStore, ContextStore, EventLog,
    FactStore, InboxStore, LoginCodeStore, MemoryStore, RunStore, SecretStore, SessionStore,
    SkillStateStore, TaskStore, ToolProvider, UsageMeter, UserStore, WorkflowRevisionStore,
    WorkspaceStore,
};
// Separate line (#241) so this addition is a pure append, not a reflow of the
// grouped import that sibling store-seam branches (#274, #596) also edit.
use crate::ports::ScheduleFireStore;
// Separate line (#596) for the same reason.
use crate::ports::WorkflowRunOutputStore;
use crate::runtime::board_events::BoardAnnouncer;
use crate::runtime::channel::{DeskChannel, OPERATOR_CHANNEL, OperatorChannel};
use crate::runtime::handover::RuntimeHandover;
use crate::runtime::journal::RuntimeJournal;
use crate::runtime::tools::{StubToolProvider, grant_matches};
use crate::runtime::workspace_events::WorkspaceAnnouncer;
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
/// Compiled in **every** build since issue #264. It used to be gated to
/// `openhuman` because `build_roster` was its only caller, but the agent detail
/// route (`GET {scope}/team/{agent_id}`) now answers the same question for the
/// console, and that route ships in the default build. Both callers must read
/// the *same* function or the console would show an operator a tool list the
/// harness does not actually grant — which is precisely the verification gap
/// #264 was filed about.
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

/// One agent's effective grants under the **three-level** narrowing
/// `[tools].allow ∩ desk.tools ∩ [[agent]].tools`.
///
/// `desk_allows` is the `tools` ceiling of every desk this agent sits on — the
/// effective membership (manifest desks plus console-added ones), not just the
/// manifest's, so a teammate added to a desk through the console is scoped the
/// same as one written into `company.toml`.
///
/// This is [`agent_effective_grants`] applied twice, deliberately rather than
/// incidentally: narrowing is one operation, and expressing the desk level as a
/// second application of it is what guarantees the middle level can only ever
/// *remove* capability. There is no path through this function that returns a
/// grant the company allow-list does not already cover.
///
/// # The union, and the sharp edge in it
///
/// Desks combine by **union**, because desk membership is additive: joining the
/// growth desk is how a marketer gains the ad tools. Intersecting instead would
/// make each additional desk silently revoke capability, so adding someone to a
/// desk could break work they already did.
///
/// The edge that follows: a desk with an empty `tools` ceiling narrows nothing,
/// so an agent on both a restricted desk and an unrestricted one ends up
/// **unrestricted**. That is the honest consequence of "empty means no
/// ceiling" plus union, and it is the safe direction — a company that means to
/// restrict a teammate states the ceiling on every desk that teammate sits on,
/// or states it on the teammate. It is *not* a hole in the company grant: the
/// widest this can ever resolve to is `allow` itself.
pub(crate) fn agent_scoped_grants(
    allow: &[String],
    desk_allows: &[&[String]],
    agent_tools: &[String],
) -> Vec<String> {
    // No desk states a ceiling (the common case, and every pre-existing
    // manifest) → the desk level is skipped entirely and this is exactly the
    // two-level behaviour that shipped before desks could scope anything.
    let ceiling: Vec<String> = if desk_allows.iter().all(|desk| desk.is_empty()) {
        allow.to_vec()
    } else {
        dedup(
            desk_allows
                .iter()
                .flat_map(|desk| agent_effective_grants(allow, desk))
                .collect(),
        )
    };
    agent_effective_grants(&ceiling, agent_tools)
}

/// Whether the company allow-list covers an agent-requested grant glob.
///
/// `pub(crate)` so the tool catalog (`crate::company::tool_catalog`) can answer
/// "does this company grant that?" with the *same* matcher the roster build
/// uses. A catalog doing its own matching would be free to advertise a grant the
/// gate does not honour — precisely the disagreement between what the console
/// shows and what an agent can actually do that the catalog exists to end.
pub(crate) fn allow_covers(allow: &[String], tool: &str) -> bool {
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
/// Whether the operator's console `[policy]` override survives this rebuild
/// (issue #562).
///
/// **Version control wins when it speaks, and stays quiet when it doesn't.**
/// The override is kept while the seed's `[policy]` is unchanged, and dropped
/// the moment the seed's `[policy]` differs from the one the previous boot ran.
///
/// # Why the condition is the whole point
///
/// The override is deliberately *not* a manifest merge — [`CompanyRecord::effective_policy`]
/// resolves it ahead of the manifest instead, so `record.manifest` stays
/// seed-authoritative and the invariant stated on [`merge_enabled_workflows`]
/// is untouched in the letter.
///
/// Carrying it unconditionally would nonetheless reproduce that invariant's own
/// named harm in the spirit. An operator tightens `[policy]` in `company.toml`,
/// redeploys, and a looser console override silently wins — *"a runtime write
/// outliving a seed rollback; privilege persisting after the operator revoked it
/// in version control."* That reasoning is exactly why `[tools]` and `[policy]`
/// are the two fields singled out, and an approval gate is precisely the thing
/// it was written about. The per-teammate spend cap (issue #343) makes the
/// opposite trade and is defensible doing so: a cap is a number, not the gate.
///
/// # Why it is conditional rather than always-clear
///
/// Dropping the override on *every* rebuild would be the opposite failure, and
/// just as silent: a routine redeploy that changed nothing would revert the
/// operator's console action, and nothing in the console would show the tier had
/// moved back. So the seed only wins when it actually says something new.
///
/// # What counts as the seed speaking
///
/// Any change to `[policy]` — `mode`, `always_approve`, or
/// `auto_approve_under_usd`. Comparing the whole block rather than just the
/// field being overridden is deliberate: an operator who edits `[policy]` at all
/// has turned their attention to the approval gate, and resolving *which* of
/// their edits was meant to override the console is a guess. Clearing and
/// letting them re-apply is the answer that cannot silently pick wrong.
///
/// `previous_seed` is the prior boot's `record.manifest.policy`, which is that
/// boot's seed verbatim — `[policy]` is never merged, so the persisted manifest
/// is the seed for this field.
///
/// This is a **second** clearing path, not a replacement for the explicit
/// `DELETE …/policy` reset: that one is how an operator clears their own
/// override without touching version control.
fn carry_policy_override(
    previous_seed: &Policy,
    next_seed: &Policy,
    held: Option<&PolicyOverride>,
) -> Option<PolicyOverride> {
    let held = held?;
    (previous_seed == next_seed).then(|| held.clone())
}

/// Carries the operator's per-desk tool ceilings across a rebuild, dropping any
/// whose desk had its seed `tools` edited in version control.
///
/// The same rule [`carry_policy_override`] applies to the approval gate, applied
/// per desk to the capability gate — and it belongs here for the *stronger* of
/// that function's two reasons. A desk ceiling decides which tools its members
/// are wired with, so a console override outliving a seed that narrowed the desk
/// is a runtime widening surviving the operator revoking it in version control.
/// That is the exact failure the `[tools]`/`[policy]` seed-wins rule exists to
/// prevent, and a per-desk grant is squarely within it rather than being "a
/// number, not the gate" the way a spend cap is.
///
/// Per **desk** rather than whole-block, unlike the policy rule: desks are
/// independent of one another, so an operator editing the finance desk's
/// ceiling has said nothing about the creative desk's, and clearing both would
/// revert a console action nobody's edit was about. Within a single desk the
/// comparison is still whole-value, for the reason the policy rule gives — an
/// operator who edited that desk's grant at all has turned their attention to
/// it, and guessing which half of the edit was meant to win cannot silently
/// pick right.
///
/// An override for a desk the seed does not declare (an operator-created desk)
/// is always carried: version control never spoke about it, so it has no seed
/// value that could have changed.
fn carry_desk_tool_overrides(
    previous_seed: &[GroupChat],
    next_seed: &[GroupChat],
    held: &std::collections::BTreeMap<String, Vec<String>>,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let seed_tools = |desks: &[GroupChat], desk_id: &str| {
        desks
            .iter()
            .find(|desk| desk.id == desk_id)
            .map(|desk| desk.tools.clone())
    };
    held.iter()
        .filter(|(desk_id, _)| seed_tools(previous_seed, desk_id) == seed_tools(next_seed, desk_id))
        .map(|(desk_id, ceiling)| (desk_id.clone(), ceiling.clone()))
        .collect()
}

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
    /// Install-wide default MCP servers (issue #527), from resolved config.
    default_mcp_servers: Vec<crate::company::McpServer>,
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
    /// The deployment's standing bootstrap-admin address
    /// (`AppConfig::bootstrap_admin`), pre-normalized, when the platform injects
    /// one (issue #661 / M8). Threaded onto the workflow delivery bundle so an
    /// `owner` report on a fresh tenant reaches the company's creator before
    /// their first sign-in mints a user record. `None` everywhere but the hosted
    /// serve path.
    bootstrap_admin: Option<String>,
    /// A host-wide override of this company's `[users].mode`
    /// (`AppConfig::auth_mode_override`). `None` — the usual case — leaves the
    /// manifest to answer.
    auth_mode_override: Option<AuthMode>,
    tasks: Option<Arc<dyn TaskStore>>,
    ledgers: Option<Arc<dyn crate::ports::ledgers::LedgerStore>>,
    workspace: Option<Arc<dyn WorkspaceStore>>,
    /// Issue #553: the byte limits the workspace is held to. Defaults to a
    /// 256 MiB per-file cap and an unlimited tree, so a runtime built without
    /// naming a quota is still not a way to write an unbounded file.
    workspace_quota: crate::runtime::WorkspaceQuota,
    /// Whether private per-agent filesystem workspaces keep automatic Git
    /// checkpoints after tool calls.
    workspace_git_enabled: bool,
    /// Issue #752: which storage backend is serving this host's secrets. Only
    /// the repository-credential gates read it, and the default is the refusing
    /// side (`fs`) — a runtime built without naming a backend is assumed to keep
    /// secrets as plaintext on disk, because that is what
    /// [`with_stores`](Self::with_stores) not being called actually means.
    storage_kind: crate::store::StorageKind,
    facts: Option<Arc<dyn FactStore>>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    runs: Option<Arc<dyn RunStore>>,
    workflow_revisions: Option<Arc<dyn WorkflowRevisionStore>>,
    schedule_fires: Option<Arc<dyn ScheduleFireStore>>,
    run_output_store: Option<Arc<dyn WorkflowRunOutputStore>>,
    usage: Option<Arc<dyn UsageMeter>>,
    skills: Option<Arc<dyn SkillStateStore>>,
    read_state: Option<Arc<dyn crate::ports::read_state::ReadStateStore>>,
    notifications: Option<Arc<dyn crate::ports::notifications::NotificationStore>>,
    users: Option<Arc<dyn UserStore>>,
    sessions: Option<Arc<dyn SessionStore>>,
    login_codes: Option<Arc<dyn LoginCodeStore>>,
    /// The runtime journal's durable sink (issue #726).
    ///
    /// `None` selects the filesystem default below — the company bundle's
    /// `journal.jsonl`, which is where the journal has always lived. Set from
    /// [`with_stores`](Self::with_stores) on every backend, because on a hosted
    /// mongodb tenant the bundle directory is ephemeral scratch and a journal
    /// left there loses every committed effect key and every parked approval on
    /// the next container replacement.
    journal_store: Option<Arc<dyn crate::ports::journal::JournalStore>>,
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
            default_mcp_servers: Vec::new(),
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
            bootstrap_admin: None,
            auth_mode_override: None,
            tasks: None,
            ledgers: None,
            workspace: None,
            workspace_quota: crate::runtime::WorkspaceQuota::default(),
            workspace_git_enabled: false,
            storage_kind: crate::store::StorageKind::default(),
            facts: None,
            artifacts: None,
            runs: None,
            workflow_revisions: None,
            schedule_fires: None,
            run_output_store: None,
            usage: None,
            skills: None,
            read_state: None,
            notifications: None,
            users: None,
            sessions: None,
            login_codes: None,
            journal_store: None,
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
    /// Sets the install-wide default MCP servers (issue #527) — the normalized
    /// `[[default_mcp_server]]` list from the instance `config.toml`. They merge
    /// underneath this company's manifest servers, so a company that declares a
    /// server of the same name keeps its own.
    pub fn with_default_mcp_servers(mut self, servers: Vec<crate::company::McpServer>) -> Self {
        self.default_mcp_servers = servers;
        self
    }

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
        self.ledgers = Some(handles.ledgers.clone());
        self.workspace = Some(handles.workspace.clone());
        self.facts = Some(handles.facts.clone());
        self.artifacts = Some(handles.artifacts.clone());
        self.runs = Some(handles.runs.clone());
        self.workflow_revisions = Some(handles.workflow_revisions.clone());
        self.schedule_fires = Some(handles.schedule_fires.clone());
        self.run_output_store = Some(handles.run_outputs.clone());
        self.usage = Some(handles.usage.clone());
        self.skills = Some(handles.skills.clone());
        self.read_state = Some(handles.read_state.clone());
        self.notifications = Some(handles.notifications.clone());
        self.users = Some(handles.users.clone());
        self.sessions = Some(handles.sessions.clone());
        self.login_codes = Some(handles.login_codes.clone());
        self.journal_store = Some(handles.journal.clone());
        self.with_store(handles.company.clone())
            .with_events(handles.events.clone())
            .with_memory(handles.memory.clone())
            .with_context(handles.context.clone())
            .with_secrets(handles.secrets.clone())
            .with_inbox(handles.inbox.clone())
    }

    /// Overlays the memory ports from a selected memory engine
    /// (`OPENCOMPANY_MEMORY`, see [`crate::store::select`]).
    ///
    /// Applied *after* [`with_stores`](Self::with_stores) (or over the fs
    /// defaults), so a dedicated memory engine backs recall while the base
    /// backend keeps every other durable port.
    ///
    /// Memory and context always come from the overlay. `FactStore` comes from
    /// it only when the engine serves facts as well — the embedded engine
    /// implements memory + context alone and leaves facts on the base backend,
    /// while an engine bound through the `MemoryProvider` contract covers all
    /// three ports. Taking whichever the overlay offers is what keeps one
    /// company's memory on one engine instead of split across two (issue #914).
    pub fn with_memory_overlay(self, overlay: &crate::store::MemoryOverlay) -> Self {
        let builder = self
            .with_memory(overlay.memory.clone())
            .with_context(overlay.context.clone());
        match &overlay.facts {
            Some(facts) => builder.with_facts(facts.clone()),
            None => builder,
        }
    }

    /// Swaps just the runtime journal's durable sink (default: the company
    /// bundle's `journal.jsonl`).
    ///
    /// [`with_stores`](Self::with_stores) sets this alongside every other port,
    /// so production never calls it. It exists for the same reason
    /// [`with_memory_overlay`](Self::with_memory_overlay) does — one port,
    /// swapped on its own — and it is what lets a test put the at-most-once set
    /// somewhere the company bundle is not.
    pub fn with_journal_store(
        mut self,
        store: Arc<dyn crate::ports::journal::JournalStore>,
    ) -> Self {
        self.journal_store = Some(store);
        self
    }

    /// Swaps the task board store (default: fs-backed).
    /// Injects the ledger store.
    #[must_use]
    pub fn with_ledgers(mut self, ledgers: Arc<dyn crate::ports::ledgers::LedgerStore>) -> Self {
        self.ledgers = Some(ledgers);
        self
    }

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

    /// Sets the workspace's byte limits (default: 256 MiB per file, unlimited
    /// tree). See [`QuotaEnforcedWorkspace`](crate::runtime::QuotaEnforcedWorkspace).
    pub fn with_workspace_quota(mut self, quota: crate::runtime::WorkspaceQuota) -> Self {
        self.workspace_quota = quota;
        self
    }

    /// Enables or disables automatic Git checkpoints in private agent
    /// workspaces. Disabled by default.
    pub fn with_workspace_git_enabled(mut self, enabled: bool) -> Self {
        self.workspace_git_enabled = enabled;
        self
    }

    /// Records which storage backend serves this host's secrets (issue #752).
    ///
    /// Separate from [`with_stores`](Self::with_stores) because the two answer
    /// different questions: `with_stores` hands over port *implementations*,
    /// while this is the deployment's own name for the backend — the string an
    /// operator set in `OPENCOMPANY_STORAGE` and the one the refusal quotes back
    /// at them. Defaults to `fs`, the refusing side.
    pub fn with_storage_kind(mut self, kind: crate::store::StorageKind) -> Self {
        self.storage_kind = kind;
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

    /// Swaps the workflow-revision store (default: fs-backed).
    pub fn with_workflow_revisions(
        mut self,
        workflow_revisions: Arc<dyn WorkflowRevisionStore>,
    ) -> Self {
        self.workflow_revisions = Some(workflow_revisions);
        self
    }

    /// Swaps the scheduler fire-claim store (default: fs-backed).
    pub fn with_schedule_fires(mut self, schedule_fires: Arc<dyn ScheduleFireStore>) -> Self {
        self.schedule_fires = Some(schedule_fires);
        self
    }

    /// Swaps the per-node run-output store (default: fs-backed; #596).
    pub fn with_run_output_store(
        mut self,
        run_output_store: Arc<dyn WorkflowRunOutputStore>,
    ) -> Self {
        self.run_output_store = Some(run_output_store);
        self
    }

    /// Swaps the usage meter (default: fs-backed).
    pub fn with_usage(mut self, usage: Arc<dyn UsageMeter>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Swaps the per-person channel read markers (default: fs-backed).
    pub fn with_read_state(
        mut self,
        read_state: Arc<dyn crate::ports::read_state::ReadStateStore>,
    ) -> Self {
        self.read_state = Some(read_state);
        self
    }

    /// Swaps the durable notification store (default: fs-backed).
    pub fn with_notifications(
        mut self,
        notifications: Arc<dyn crate::ports::notifications::NotificationStore>,
    ) -> Self {
        self.notifications = Some(notifications);
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

    /// Wires the deployment's standing bootstrap-admin address (issue #661 / M8),
    /// pre-normalized by `AppConfig::bootstrap_admin`. Absent by default: only
    /// the hosted serve path injects one, and `None` is a clean no-op that leaves
    /// `owner` delivery resolving admins from the user store alone.
    pub fn with_bootstrap_admin(mut self, bootstrap_admin: Option<String>) -> Self {
        self.bootstrap_admin = bootstrap_admin;
        self
    }

    /// Overrides how humans sign in to this company, beating its manifest's
    /// `[users].mode`.
    ///
    /// This is where [`AppConfig::auth_mode_override`](crate::AppConfig) — the
    /// `OPENCOMPANY_AUTH_MODE` / `config.toml` layers — wins, and it wins here
    /// rather than at each read so that every part of the runtime sees one
    /// answer. `None` is a clean no-op leaving the manifest to decide.
    ///
    /// The mode is resolved **once, at build**, and cached on the runtime,
    /// because it is read on the request hot path: the alternative is a
    /// `CompanyStore` read per request to re-parse a manifest that cannot change
    /// without a rebuild.
    pub fn with_auth_mode_override(mut self, auth_mode: Option<AuthMode>) -> Self {
        self.auth_mode_override = auth_mode;
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
        // Chosen before the ops struct because two of its members need it: the
        // ledger store itself, and the workspace guard that names a refusal
        // after the ledger owning the file.
        let ledgers_for_guard: Arc<dyn crate::ports::ledgers::LedgerStore> =
            self.ledgers.clone().unwrap_or_else(|| fs_ops.clone());
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
                // Issue #327: and the tree announces its own writes, for the
                // same reason and in the same place. Every writer — the
                // console routes, the agent tools, the publish drain, the
                // seeder below — passes through this port, so none of them has
                // to remember to emit. See [`WorkspaceAnnouncer`].
                // Issue #553: and the tree refuses what it cannot afford,
                // wrapped INSIDE the announcer so a refused write is never
                // announced — the feed must not claim a file appeared that the
                // quota rejected. See [`QuotaEnforcedWorkspace`].
                // And the `derived/` folder refuses a hand-written edit,
                // wrapped INSIDE both so a refused edit is never announced and
                // never charged — and so that every writer, console or agent or
                // workflow node, obeys without knowing it does. See
                // [`DerivedGuardWorkspace`].
                workspace: Arc::new(WorkspaceAnnouncer::new(
                    Arc::new(crate::runtime::QuotaEnforcedWorkspace::new(
                        Arc::new(crate::runtime::DerivedGuardWorkspace::new(
                            self.workspace.unwrap_or_else(|| fs_ops.clone()),
                            ledgers_for_guard.clone(),
                        )),
                        self.workspace_quota,
                    )),
                    events.clone(),
                )),
                ledgers: ledgers_for_guard.clone(),
                facts: self.facts.unwrap_or_else(|| fs_ops.clone()),
                artifacts: self.artifacts.unwrap_or_else(|| fs_ops.clone()),
                // Issue #1015: every attempt status change journals a frame, so
                // the task screen can be pushed rather than polled. Wrapped here
                // rather than at the cycle's call sites because
                // `reap_orphaned_runs` settles crash-killed runs through
                // `finish_run` directly — see `runtime::run_events`.
                runs: Arc::new(crate::runtime::run_events::EventingRunStore::new(
                    self.runs.unwrap_or_else(|| fs_ops.clone()),
                    events.clone(),
                )),
                workflow_revisions: self.workflow_revisions.unwrap_or_else(|| fs_ops.clone()),
                schedule_fires: self.schedule_fires.unwrap_or_else(|| fs_ops.clone()),
                workflow_run_outputs: self
                    .run_output_store
                    .clone()
                    .unwrap_or_else(|| fs_ops.clone()),
                usage: self.usage.unwrap_or_else(|| fs_ops.clone()),
                skills: self.skills.unwrap_or_else(|| fs_ops.clone()),
                read_state: self.read_state.unwrap_or_else(|| fs_ops.clone()),
                notifications: self.notifications.unwrap_or_else(|| fs_ops.clone()),
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

        // Issue #551: lay down the workspace's system roots — `Agents/` and
        // `Desks/` — beside the template-seeded top-level folders, so anything
        // an agent or a desk produces has a named home both the operator and
        // the other agents can navigate to. The roots only; the folder for a
        // given agent or desk is minted the first time that agent or desk
        // actually produces something (see
        // [`company::workspace_scaffold`](crate::company::workspace_scaffold)).
        //
        // Gated on `handover.is_none()` and nothing else, deliberately:
        //
        //  * NOT on `seed_dir` — a provisioned tenant and the desktop build have
        //    no company bundle to seed from, and their workspace needs the same
        //    shape.
        //  * NOT on `is_empty` — that gate exists so an operator's deletions
        //    stick against re-seeding, which is a different question. An
        //    existing company with notes already in it picks the roots up on
        //    its next boot, which is the only way it ever gets them.
        //  * NOT on the roster — the roots are part of what a workspace *is*,
        //    so a company with no agents at all still gets both.
        //
        // It is idempotent, so running it on every boot costs one tree read.
        // This is the feature's only eager seam: everything that used to be
        // re-provisioned on a roster change is now minted on demand instead.
        if handover.is_none() {
            crate::company::workspace_scaffold::ensure_workspace_scaffold(
                ops.workspace.as_ref(),
                &id,
            )
            .await?;
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
        let mut channels = match self.channels {
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
                // Issue #726: the journal's sink comes from the selected backend
                // whenever one is open, and falls back to the company bundle's
                // `journal.jsonl` only when it is not. Never a silent fs
                // fallback under a database backend — that is the rule
                // `open_storage` already states for every other durable port,
                // and the journal was the one store outside it. On a hosted
                // mongodb tenant `/data` is documented ephemeral scratch, so a
                // journal left there loses every committed effect key and every
                // parked approval on the next container replacement: previously
                // executed effects become eligible to fire again, and parked
                // approvals and grants silently vanish.
                let (sink, sink_name) = match self.journal_store.clone() {
                    Some(store) => (store, "backend"),
                    None => (
                        Arc::new(crate::store::FsJournalStore::new(home.clone()))
                            as Arc<dyn crate::ports::journal::JournalStore>,
                        "filesystem",
                    ),
                };

                // The one-time import off the filesystem, gated on the sink's
                // own receipt. Verbatim and in file order, raw strings — so a
                // corrupt or merged line migrates byte-for-byte and the journal's
                // own recovery still applies to it downstream.
                //
                // The receipt is what makes this safe to retry: `complete_import`
                // clears before it copies and records the receipt last, so an
                // interrupted import re-runs the whole copy instead of leaving a
                // truncated prefix behind a closed gate. A truncated prefix is
                // the bug itself — it drops at-most-once keys.
                //
                // Fatal on failure, deliberately. Booting with an empty journal
                // because the import errored is indistinguishable, to every
                // effect the company then runs, from having never executed
                // anything.
                //
                // The gate is closed even when there is no file to import (an
                // import of zero lines). That is one step stronger than "import
                // if the file exists", and the step is load-bearing: it makes a
                // `journal.jsonl` that appears *later* — a rollback, a stray copy
                // into the data dir — unable to wipe and replace a journal the
                // backend has since accumulated.
                if !sink.journal_imported(&id).await? {
                    let legacy = Bundle::new(home.clone(), &id).journal_jsonl();
                    let lines: Vec<String> = crate::store::fs::read_lines_lossy(&legacy)
                        .await?
                        .into_iter()
                        .filter(|line| !line.trim().is_empty())
                        .collect();
                    let count = lines.len();
                    sink.complete_import(&id, lines).await?;
                    if count > 0 {
                        tracing::info!(
                            company = %id,
                            lines = count,
                            "imported the filesystem journal into the storage backend; \
                             the source file is left in place",
                        );
                    }
                }

                let journal = Arc::new(RuntimeJournal::with_store(sink, id.clone()));
                journal.load().await?;
                // Which sink the at-most-once guarantee is actually resting on.
                // Worth one line at boot: "filesystem" under
                // `OPENCOMPANY_STORAGE=mongodb` would mean the guarantee is
                // resting on scratch, and that is not a thing an operator can
                // otherwise see.
                tracing::info!(
                    company = %id,
                    sink = sink_name,
                    "runtime journal ready",
                );
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
                        // Issue #983: a reaped chat turn names no card, so the
                        // row settle above was the whole of its cleanup. The
                        // transcript half — folding its unterminated
                        // `TurnStarted` into a `TurnFailed` — is the journal
                        // sweep further down, not this one.
                        let Some(task_id) = run.task_id.as_deref() else {
                            continue;
                        };
                        match crate::runtime::advance::advance_settled_card(
                            ops.tasks.as_ref(),
                            &id,
                            task_id,
                            crate::ports::runs::RunStatus::Failed,
                            crate::ports::runs::ORPHAN_ERROR,
                        )
                        .await
                        {
                            Ok(Some(column)) => tracing::info!(
                                company = %id,
                                run = %run.id,
                                task = %task_id,
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
                                task = %task_id,
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

            // Issue #983, the chat-turn equivalent, resting on the same three
            // invariants: a turn journals a `TurnStarted` before it takes the
            // serial lock, every turn is driven in this process, and one process
            // owns this journal. So a start with no terminal at boot is a turn
            // that died with the last host — and without this the operator's
            // question sits in the transcript with no answer after it and no
            // explanation, which is indistinguishable from a message that never
            // warranted a reply.
            //
            // The row half of the same turn is reclaimed by
            // `reap_orphaned_runs` above; this is the transcript half. Both are
            // needed: the row makes a status query honest, the event makes the
            // conversation honest, and neither can be derived from the other.
            //
            // Gated on the handover for exactly the reason the sweeps above are,
            // and here the mis-fire is the worst of the three: a chat turn
            // survives a live runtime swap — the drain covers its *cycle*, but
            // the spawned task journals the replies and settles the row after
            // that cycle returns — so sweeping mid-life would tell the operator
            // their turn failed and then answer it.
            crate::runtime::sweep_interrupted_turns(&events, &id).await;

            // Issue #390, the cycle-level equivalent, resting on the same three
            // invariants: a cycle journals a start before it takes the serial
            // lock, every cycle is driven in this process, and one process owns
            // this journal. So a start with no finish at boot is a cycle that
            // died with the last host.
            //
            // Gated on the handover for exactly the same reason as the sweep
            // above: a cycle survives a live runtime swap, and sweeping mid-life
            // would stamp "interrupted by a host restart" on one still running,
            // whose real finish would then land after the synthetic one.
            //
            // Placed after `journal.load()`, whose replay is what populates the
            // open set, and best-effort inside for the same reason.
            let settled = journal.sweep_interrupted_cycles().await;
            if settled > 0 {
                tracing::info!(
                    company = %id,
                    settled,
                    "settled cycles left open by a previous host process"
                );
            }
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

        // Issue #978: the run-scoped half of the same fact, on exactly the same
        // terms. A boot rebuilds it from the gates the replay left parked — the
        // journal keeps their whole effect while they are parked, which is what
        // makes a rehydrated batch re-dispatchable — and a rebuild inherits the
        // live one, because that one also knows the verdicts taken since the
        // replay and a fresh copy would re-ask about them.
        let workflow_gates = match handover.as_ref() {
            Some(h) => h.workflow_gates.clone(),
            None => {
                let gates = crate::runtime::workflow_gates::WorkflowGateQueue::default();
                let parked = journal.pending();
                gates.rearm(parked.iter().filter_map(|entry| {
                    entry
                        .batch
                        .clone()
                        .map(|turn| (turn, entry.id.clone(), &entry.effect))
                }));
                gates
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
        // Issue #580: the company's workflow builder, built from the SAME deps as
        // the planner (shared provider + model override) and installed the same
        // way via `CompanyRuntime::set_builder` below.
        #[cfg(feature = "openhuman")]
        let mut builder: Option<Arc<crate::harness::workflow_build::WorkflowBuilder>> = None;
        #[cfg(feature = "openhuman")]
        let mut workflow_harness_deps: Option<crate::harness::HarnessDeps> = None;

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

        // Desks are delivery destinations as well as inbound conversation
        // threads. Resolve both manifest and operator-created candidates
        // through CompanyRecord so this wiring cannot drift from the desk
        // existence rules used by the server and the harness.
        // Built fresh rather than cloned from `existing`: the persisted record
        // carries the manifest of a PREVIOUS boot, so cloning it would wire
        // desk channels from a stale `[[group_chat]]` list — a desk added to
        // `company.toml` would never become a delivery destination, and one
        // removed there would linger as one. The overlay halves are already
        // lifted out of `existing` above, so this loses nothing.
        let desk_record = CompanyRecord {
            id: id.clone(),
            manifest: self.manifest.clone(),
            ledger: Vec::new(),
            lifecycle: lifecycle.clone(),
            overlay_agents: overlay_agents.clone(),
            overlay_desk_members: overlay_desk_members.clone(),
            overlay_desk_order: overlay_desk_order.clone(),
            overlay_desks: overlay_desks.clone(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
        };
        let mut desk_ids = Vec::new();
        let candidates = desk_record
            .manifest
            .group_chats
            .iter()
            .map(|desk| desk.id.as_str())
            .chain(
                desk_record
                    .overlay_desks
                    .iter()
                    .map(|desk| desk.id.as_str()),
            );
        for candidate in candidates {
            if let Some(desk_id) = desk_record.resolve_desk_id(candidate)
                && desk_record.desk_exists(&desk_id)
                && !desk_ids.contains(&desk_id)
            {
                desk_ids.push(desk_id);
            }
        }
        for desk_id in desk_ids {
            channels.push(Arc::new(DeskChannel::new(
                id.clone(),
                desk_id,
                events.clone(),
            )));
        }
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
        // Issue #562: the operator's `[policy]` override, carried across the
        // rebuild — but ONLY while the seed's `[policy]` has not itself changed.
        // See `carry_policy_override` for why that condition is the whole point.
        let overlay_policy = existing.as_ref().and_then(|r| {
            let carried = carry_policy_override(
                &r.manifest.policy,
                &self.manifest.policy,
                r.overlay_policy.as_ref(),
            );
            if carried.is_none() && r.overlay_policy.is_some() {
                tracing::info!(
                    company = %id,
                    seed_mode = %self.manifest.policy.mode,
                    "[policy] the seed `[policy]` changed, so the console override was cleared — \
                     version control wins when it speaks"
                );
            }
            carried
        });
        // The per-desk tool ceilings, carried across the rebuild under the same
        // seed-wins rule as the policy override above — see
        // `carry_desk_tool_overrides` for why a desk grant needs it even more
        // than the approval tier does.
        let overlay_desk_tools = existing
            .as_ref()
            .map(|r| {
                let carried = carry_desk_tool_overrides(
                    &r.manifest.group_chats,
                    &self.manifest.group_chats,
                    &r.overlay_desk_tools,
                );
                for desk_id in r.overlay_desk_tools.keys() {
                    if !carried.contains_key(desk_id) {
                        tracing::info!(
                            company = %id,
                            desk = %desk_id,
                            "[tools] the seed changed this desk's `tools`, so the console \
                             ceiling was cleared — version control wins when it speaks"
                        );
                    }
                }
                carried
            })
            .unwrap_or_default();
        // Issue #276: the workflows the operator switched off. This is the field
        // that makes the pause switch durable, and it is carried across the
        // rebuild for a sharper reason than the two above: `merge_enabled_workflows`
        // below re-derives `[workflows].enabled` from the seed ∪ overlay ids, so
        // that list re-arms everything on every boot by design. If the disable
        // set were dropped here, a paused workflow would resume firing at the
        // next restart — the one failure mode a safety switch may not have.
        let disabled_workflows = existing
            .as_ref()
            .map(|r| r.disabled_workflows.clone())
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

        // Issue #752: a company whose roster holds `repo` does not come up on a
        // backend that keeps secrets as plaintext on this container's disk.
        //
        // The bind-time refusal in `RepoManager::bind` covers new credentials.
        // It cannot cover a company that bound one *before* this gate existed —
        // that credential is already sitting on `/data`, and its agents would
        // keep checking out under it forever. So the same condition is asked
        // again here, where the answer is "this company does not start", and an
        // operator restarting a tenant finds out at the moment they can still
        // act on it.
        //
        // Read over the **effective roster**, not `[tools].allow` alone: an
        // agent naming `tools = ["repo"]` under a company allow-list of `["*"]`
        // holds an explicit `repo` grant that `grants_repo_explicit(&allow)`
        // does not see, because the wildcard deliberately does not confer the
        // namespace. Checking only the company line would miss exactly the
        // configuration a wildcard company is most likely to have.
        //
        // NOT feature-gated, for the reason `build_agent` states about the repo
        // tools themselves: a control compiled only under `openhuman` is a
        // control most CI lanes never type-check.
        if self.storage_kind.secrets_are_plaintext_on_disk() {
            let roster_holds_repo =
                crate::company::grants_repo_explicit(&self.manifest.tools.allow)
                    || self
                        .manifest
                        .agents
                        .iter()
                        .map(|agent| {
                            agent_effective_grants(&self.manifest.tools.allow, &agent.tools)
                        })
                        .chain(overlay_agents.iter().map(|overlay| {
                            agent_effective_grants(&self.manifest.tools.allow, &overlay.tools)
                        }))
                        .any(|grants| crate::company::grants_repo_explicit(&grants));
            if roster_holds_repo {
                return Err(crate::error::OpenCompanyError::Config(
                    crate::store::plaintext_secret_refusal(self.storage_kind),
                ));
            }
        }

        // Issue #245: the company's repository mirror cache, rooted at the same
        // `companies/<slug>/` prefix the bundle uses so a company's whole
        // footprint sits in one subtree — and therefore inside the one quota
        // walk `DataLayout::usage_bytes` already does.
        //
        // Built here rather than lazily in the route because the location is a
        // property of *this* runtime's home, and a route that had to derive it
        // would be the second place that knows where a company's bytes live.
        //
        // The cache is capped by the same `tree_quota_bytes` that bounds the
        // workspace tree: both answer "how much may one company hold on this
        // host", and a mirror is company-held binary payload like any other. It
        // needs no new knob, and a hosted tenant that already sets one gets the
        // cache covered without touching its config.
        let repos = {
            let manager = crate::runtime::RepoManager::new(
                id.clone(),
                Bundle::new(home.clone(), &id).repos_dir(),
                secrets.clone(),
            )
            .with_quota(self.workspace_quota.tree_quota_bytes)
            // Issue #752: the manager refuses a bind when a credential would
            // land as plaintext on this container's disk, so it has to be told
            // which backend is actually serving secrets.
            .with_storage_kind(self.storage_kind);
            // The forge REST client (pull-request metadata + diff). Without the
            // feature there is no HTTP client to give it, and the PR route says
            // so rather than answering with an empty diff. Shadowed rather than
            // reassigned, the same way `ops::router` folds in its feature-gated
            // fragment.
            #[cfg(feature = "github")]
            let manager =
                manager.with_host(std::sync::Arc::new(crate::runtime::HttpRepoHost::new()));
            std::sync::Arc::new(manager)
        };

        // Issue #245, agent half: the bindings, read once here so the
        // synchronous `build_agent` can name them in a tool description and
        // resolve against them. Read only when the company explicitly grants
        // `repo` — everything else has no repository tools to feed — and a
        // read error degrades to empty with a warning rather than failing a
        // boot over a capability the company may not even use.
        // `HarnessPool::ensure` re-reads this every turn, so a bind, a rotation
        // or a revoke lands without a restart; this is only the first value.
        #[cfg(feature = "openhuman")]
        let repo_bindings = if crate::company::grants_repo_explicit(&self.manifest.tools.allow) {
            repos.list().await.unwrap_or_else(|err| {
                tracing::warn!(
                    company = %id,
                    error = %err,
                    "reading the repository bindings failed; agents start with none"
                );
                Vec::new()
            })
        } else {
            Vec::new()
        };

        // Issue #245: a checkout's lifecycle is one turn's, and a host killed
        // mid-turn ends no turn — so whatever the last process left under
        // `<harness>/<company>/*/workspace/repos` is deleted before this one
        // starts. Tenant-scoped (issue #664): it walks this company's subtree
        // and nothing above it.
        #[cfg(feature = "openhuman")]
        crate::harness::repo::sweep_orphaned_checkouts(&home.join("harness"), &id).await;

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
                            // Same shape, same reason (issue #383). Issue #401:
                            // the per-company concurrency ceiling comes from the
                            // manifest (validated `>= 1`), so this supervisor —
                            // the one the harness deps and the console cancel
                            // route both hold — enforces that cap on every run.
                            let supervisor = crate::runtime::RunSupervisor::with_limit(
                                self.manifest.workflows.max_in_flight_runs,
                            );
                            run_supervisor = Some(supervisor.clone());
                            // Resolve the company's effective MCP servers to data
                            // (manifest ∪ runtime index, credentials materialized)
                            // before building sync deps. A corrupt index degrades
                            // to no MCP servers rather than bricking boot.
                            let mcp_servers = crate::company::mcp::resolve_effective(
                                &id,
                                &self.default_mcp_servers,
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
                            // Issue #788: the per-company Chargebee connection,
                            // resolved from THIS company's secret store — never
                            // the environment, because two companies on one host
                            // bill two different sites. Only companies that
                            // explicitly grant `chargebee` resolve at all; with
                            // either half of the pair missing it stays `None`
                            // (fail closed). `HarnessPool::ensure` re-resolves it
                            // each turn, so a key saved in the console's Billing
                            // settings takes effect without a restart.
                            // Issue #789: the per-company PayPal connection,
                            // resolved from this company's own secret store for
                            // the same reason chargebee is.
                            //
                            // A store read error degrades to `None` HERE, unlike
                            // in `HarnessPool::resolve_*`, which keeps the last
                            // known connection: at boot there is no last known
                            // one to keep. It is warned rather than fatal —
                            // refusing to start the company over an unreadable
                            // billing credential would take down every other
                            // tool it has — and the next turn re-resolves.
                            #[cfg(feature = "paypal")]
                            let paypal_config = if crate::company::grants_paypal_explicit(
                                &self.manifest.tools.allow,
                            ) {
                                crate::harness::paypal::TenantPaypal::resolve(&secrets, &id)
                                    .await
                                    .unwrap_or_else(|err| {
                                        tracing::warn!(
                                            company = %id,
                                            "[paypal] could not read the billing credential at \
                                             boot; wiring no PayPal tools this turn: {err}"
                                        );
                                        None
                                    })
                            } else {
                                None
                            };
                            #[cfg(feature = "chargebee")]
                            let chargebee_config = if crate::company::grants_chargebee_explicit(
                                &self.manifest.tools.allow,
                            ) {
                                crate::harness::chargebee::TenantChargebee::resolve(&secrets, &id)
                                    .await
                                    .unwrap_or_else(|err| {
                                        tracing::warn!(
                                            company = %id,
                                            "[chargebee] could not read the billing credential at \
                                             boot; wiring no Chargebee tools this turn: {err}"
                                        );
                                        None
                                    })
                            } else {
                                None
                            };
                            // The per-company hosting connection, resolved
                            // from this company's own secret store for the same
                            // reason the billing pair above is: two companies on
                            // one host deploy to two different hosting accounts,
                            // and a deployment publishes files to the internet
                            // under the account's own name. An ambient
                            // environment variable could only ever be somebody
                            // else's account, so none is consulted.
                            let hosting_config = if crate::company::grants_hosting_explicit(
                                &self.manifest.tools.allow,
                            ) {
                                crate::harness::hosting::TenantHosting::resolve(&secrets, &id)
                                    .await
                                    .unwrap_or_else(|err| {
                                        tracing::warn!(
                                            company = %id,
                                            "[hosting] could not read the hosting credential at \
                                             boot; wiring no hosting tools this turn: {err}"
                                        );
                                        None
                                    })
                            } else {
                                None
                            };
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
                            // Resolved to data here because `build_agent` is
                            // synchronous. A store that cannot answer yields an
                            // empty registry, which costs the prompt its
                            // catalogue and leaves every tool working.
                            let ledger_registry = crate::ledger::Registry::build(
                                ops.ledgers.list_specs(&id).await.unwrap_or_else(|error| {
                                    tracing::warn!(
                                        company = %id,
                                        %error,
                                        "could not read this company's ledger declarations; \
                                         agents get the built-ins only"
                                    );
                                    Vec::new()
                                }),
                            );
                            let deps = HarnessDeps {
                                // Carried so live re-resolution merges the same
                                // three layers boot did (issue #527).
                                default_mcp_servers: self.default_mcp_servers.clone(),
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
                                workspace_git_enabled: self.workspace_git_enabled,
                                // Issue #775: the shell audit sink is HOST-owned
                                // and hangs off the data root, resolving to
                                // `companies/<slug>/audit/<agent>/` — a sibling
                                // of the `harness/` tree above, never inside it.
                                // Passed explicitly rather than derived from
                                // `workspace_root`'s parent so the boundary is a
                                // stated fact rather than a directory
                                // coincidence.
                                audit_root: home.clone(),
                                model_override,
                                tasks: Some(ops.tasks.clone()),
                                artifacts: Some(ops.artifacts.clone()),
                                ledgers: Some(ops.ledgers.clone()),
                                ledger_registry,
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
                                run_outputs: crate::harness::orchestrator::RunOutputCache::default(
                                ),
                                // Issue #596: the DURABLE, console-facing run
                                // output store — distinct from `run_outputs`
                                // above (the in-process agent cache). The runner
                                // persists each settled run's bounded node output
                                // here so a past run is readable from the console.
                                // `None` degrades to no-persist, like `events`.
                                run_output_store: self.run_output_store.clone(),
                                // Issue #661 (M7): the SAME revision store the
                                // console's workflow PUT/DELETE routes use, so
                                // an agent edit snapshots the prior body and an
                                // agent delete cascades the history exactly as
                                // an operator's does. Taken from `ops` rather
                                // than from `self`, which is `None` whenever the
                                // caller did not override the filesystem default.
                                workflow_revisions: Some(ops.workflow_revisions.clone()),
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
                                #[cfg(feature = "chargebee")]
                                chargebee: chargebee_config,
                                #[cfg(feature = "paypal")]
                                paypal: paypal_config,
                                hosting: hosting_config,
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
                                    // Issue #661 / M8: the deployment's standing
                                    // bootstrap admin, so an `owner` report on a
                                    // fresh tenant reaches the company's creator
                                    // before their first sign-in mints a user
                                    // record. `None` off the hosted serve path.
                                    bootstrap_admin: self.bootstrap_admin.clone(),
                                    // The operator adapter is an interactive
                                    // response surface, not a workflow delivery
                                    // destination: its buffer has no durable
                                    // reader. Desk and provider adapters are the
                                    // accepted workflow write paths. The rule
                                    // itself lives next to the operator-channel
                                    // constant (issue #981) so this set, the
                                    // set the console's picker offers and the
                                    // set delivery accepts cannot disagree.
                                    channels: channels
                                        .iter()
                                        .filter(|channel| {
                                            crate::runtime::channel::is_deliverable_channel(
                                                channel.channel_id(),
                                            )
                                        })
                                        .cloned()
                                        .collect(),
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
                                        // Issue #978: the SAME two queues the
                                        // runtime gets below. A gate parked by a
                                        // run has to arm the counters the
                                        // resolve path releases, or the run is
                                        // never continued at all.
                                        continuations: continuations.clone(),
                                        gates: workflow_gates.clone(),
                                    }),
                                    // Issue #529: the same journal the runner
                                    // writes its start/per-node trail to, so a
                                    // dispatch's write-behind delivery record
                                    // lands in the one log the boot-time fold
                                    // reads back. One company owns one journal;
                                    // this is that handle.
                                    events: events.clone(),
                                }),
                                // Issue #237: the SAME workspace handle the
                                // console's REST/GraphQL surface writes through
                                // (`ops.workspace`, seeded just above), so an
                                // operator's edit to `Standards/` is what the
                                // next agent turn reads. The tools cache
                                // nothing, so no rebuild is needed for an edit
                                // to take effect.
                                workspace: Some(ops.workspace.clone()),
                                // Issue #245, agent half: the SAME manager the
                                // console's bind/revoke routes write through
                                // (wired onto the runtime below), so an operator
                                // binding a repository is what the next turn's
                                // `repo_checkout` resolves against — and the
                                // one thing in this process holding the token.
                                repos: Some(repos.clone()),
                                repo_bindings,
                                // One ledger per company runtime, shared by every
                                // agent's tools and claimed per turn by the
                                // brain's `CheckoutJanitor`.
                                checkouts: crate::harness::repo::CheckoutLedger::default(),
                            };
                            workflow_harness_deps = Some(deps.clone());
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
                                overlay_policy: overlay_policy.clone(),
                                overlay_desk_tools: Default::default(),
                                disabled_workflows: disabled_workflows.clone(),
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
                            // Issue #580: built from the same deps, so it shares
                            // the tenant provider and model override with the
                            // roster and the planner rather than resolving a
                            // second credential path.
                            builder = Some(Arc::new(
                                crate::harness::workflow_build::WorkflowBuilder::from_deps(&deps),
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
                overlay_policy,
                overlay_desk_tools,
                disabled_workflows,
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
        // How humans sign in, resolved once here rather than per request: the
        // host-wide override wins, else the manifest's `[users].mode`. An
        // unparseable manifest mode cannot reach this point — `validate` names it
        // — so falling back to the default here is for a hand-built manifest in a
        // test, not for a live misconfiguration.
        runtime.set_auth_mode(
            self.auth_mode_override.unwrap_or_else(|| {
                AuthMode::from_str(&self.manifest.users.mode).unwrap_or_default()
            }),
        );
        runtime.set_repos(repos);
        // Install-wide MCP defaults (issue #527) — set before anything resolves
        // the effective server set, so the first resolution already sees them.
        runtime.set_default_mcp_servers(self.default_mcp_servers.clone());

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
        runtime.adopt_workflow_gates(workflow_gates);

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
        // Issue #580: same rebuild treatment as the planner — rebuilt from the
        // successor's deps (so a BYOK switch reaches building), with an empty
        // in-flight set that matters to nothing (a pass interrupted by a rebuild
        // has no settle to reach the board; the boot reaper settles its run).
        #[cfg(feature = "openhuman")]
        if let Some(builder) = builder {
            runtime.set_builder(builder);
        }
        #[cfg(feature = "openhuman")]
        if let Some(deps) = workflow_harness_deps {
            runtime.set_workflow_harness_deps(deps);
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

        // Issue #86: seed the kill switch from the event log, so a company
        // stopped before a restart comes back up stopped.
        //
        // A rebuild inherits the live gate (see the `handover` arm above), and
        // with it the flag already in memory — so only a cold boot, which has no
        // live flag to inherit, replays. Re-seeding on a rebuild would be
        // harmless today and would quietly become wrong the moment anything
        // engages the stop without journaling it first, which is exactly what
        // `emergency_pause` does in the window before its append lands.
        //
        // The read is deliberately NOT `?`-propagated: `hydrate_emergency` turns
        // an unreadable log into a stopped company, which is a strictly better
        // outcome than refusing to boot — an operator can release a stop, but
        // cannot reach a company that never came up.
        if handover.is_none() {
            let engaged = crate::policy::gate::replayed_emergency(runtime.events(), runtime.id())
                .await
                .map(Some);
            runtime.hydrate_emergency(engaged);
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
    use crate::ports::workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin};

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
            // Shipped with the company bundle: authored by neither the operator
            // nor an agent, and the console says exactly that (issue #326).
            created_by: WorkspaceOrigin::Seed,
            updated_by: WorkspaceOrigin::Seed,
            mime: None,
            size: None,
            sha256: None,
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
    use crate::runtime::journal::ExecutedEffect;

    fn tmp_home(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("tempdir")
    }

    /// Automatic Git checkpoints are opt-in and stay off unless the operator
    /// flips the switch. The default is asserted here so a silent change to the
    /// host default — which would start shelling out to `git` in every agent
    /// workspace — cannot slip past.
    #[test]
    fn workspace_git_checkpoints_default_off_and_switchable() {
        let home = tmp_home("opencompany-workspace-git-");
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n")
                .expect("manifest");
        let builder = RuntimeBuilder::new(home.path().to_path_buf(), manifest);
        assert!(
            !builder.workspace_git_enabled,
            "workspace Git checkpoints must default to off"
        );
        let enabled = builder.with_workspace_git_enabled(true);
        assert!(enabled.workspace_git_enabled);
        assert!(
            !enabled
                .with_workspace_git_enabled(false)
                .workspace_git_enabled,
            "the switch must also be able to turn checkpoints back off"
        );
    }

    mod scoped_grants {
        use super::*;

        fn strings(values: &[&str]) -> Vec<String> {
            values.iter().map(|v| v.to_string()).collect()
        }

        /// Runs the three-level narrowing over `&str` slices, so each case below
        /// reads as the table row it is.
        fn scope(company: &[&str], desks: &[&[&str]], agent: &[&str]) -> Vec<String> {
            let company = strings(company);
            let desk_owned: Vec<Vec<String>> = desks.iter().map(|d| strings(d)).collect();
            let desk_refs: Vec<&[String]> = desk_owned.iter().map(Vec::as_slice).collect();
            agent_scoped_grants(&company, &desk_refs, &strings(agent))
        }

        /// No desk and no per-agent list: the company grant passes through
        /// untouched. This is the shape every pre-existing manifest has, so it
        /// is the case that must be byte-identical to the old behaviour.
        #[test]
        fn empty_levels_pass_through() {
            assert_eq!(scope(&["*", "search"], &[], &[]), ["*", "search"]);
            assert_eq!(scope(&["*", "search"], &[&[]], &[]), ["*", "search"]);
            assert_eq!(scope(&["*", "search"], &[&[], &[]], &[]), ["*", "search"]);
        }

        /// The middle level does the work the feature exists for: a department
        /// ceiling narrows every member without touching any member's own line.
        #[test]
        fn a_desk_ceiling_narrows_its_members() {
            assert_eq!(scope(&["*", "search"], &[&["docs.*"]], &[]), ["docs.*"]);
        }

        /// And the agent narrows further still.
        #[test]
        fn an_agent_narrows_below_its_desk() {
            assert_eq!(
                scope(&["*", "search"], &[&["docs.*", "web"]], &["docs.*"]),
                ["docs.*"]
            );
        }

        /// Desks union, so joining a second desk *adds* capability rather than
        /// removing it. Intersecting would make adding someone to a desk break
        /// the job they already did.
        #[test]
        fn desks_combine_by_union() {
            assert_eq!(
                scope(&["*", "search"], &[&["docs.*"], &["web"]], &[]),
                ["docs.*", "web"]
            );
        }

        /// The documented sharp edge, asserted so it cannot change silently: a
        /// desk with no ceiling narrows nothing, so an agent on both a
        /// restricted and an unrestricted desk ends up unrestricted.
        ///
        /// Asserted by **coverage** rather than by list equality. The union
        /// leaves the restricted desk's `docs.*` in the result beside the open
        /// desk's `*`, which is redundant but not wrong — `*` already covers it
        /// — and pinning the exact list here would be asserting the shape of the
        /// bookkeeping instead of the capability it resolves to.
        #[test]
        fn an_unceilinged_desk_widens_the_union_back_to_the_company_grant() {
            let company = ["*", "search"];
            let resolved = scope(&company, &[&["docs.*"], &[]], &[]);
            for grant in company {
                assert!(
                    allow_covers(&resolved, grant),
                    "`{grant}` must survive an open desk: {resolved:?}"
                );
            }
        }

        /// The invariant that makes this safe to add: no path through the
        /// narrowing can yield a grant the company did not already allow. A desk
        /// ceiling naming something outside `[tools].allow` cannot widen.
        #[test]
        fn a_desk_can_never_widen_past_the_company_grant() {
            // `search` is deliberately not in the company allow-list, and `*`
            // never confers it.
            assert_eq!(
                scope(&["docs.*"], &[&["search", "shell"]], &[]),
                Vec::<String>::new()
            );
            // Nor can the agent reach past a desk that did not grant it.
            assert_eq!(
                scope(&["*"], &[&["docs.*"]], &["shell"]),
                Vec::<String>::new()
            );
        }

        /// Adding the desk level must not disturb the two-level answer for a
        /// company whose desks declare nothing — the regression that would hit
        /// every shipped company at once.
        #[test]
        fn matches_the_two_level_resolver_when_no_desk_has_a_ceiling() {
            for (company, agent) in [
                (&["*", "media"][..], &[][..]),
                (&["*", "media"][..], &["docs.*"][..]),
                (&["docs.*", "web"][..], &["web"][..]),
                (&["docs.*"][..], &["shell"][..]),
            ] {
                assert_eq!(
                    agent_scoped_grants(&strings(company), &[&[], &[]], &strings(agent)),
                    agent_effective_grants(&strings(company), &strings(agent)),
                    "company={company:?} agent={agent:?}"
                );
            }
        }
    }

    mod desk_tool_carry {
        use super::*;

        fn desk(id: &str, tools: &[&str]) -> GroupChat {
            GroupChat {
                id: id.to_string(),
                name: id.to_string(),
                description: None,
                members: Vec::new(),
                tools: tools.iter().map(|t| t.to_string()).collect(),
            }
        }

        fn held(entries: &[(&str, &[&str])]) -> std::collections::BTreeMap<String, Vec<String>> {
            entries
                .iter()
                .map(|(id, tools)| {
                    (
                        id.to_string(),
                        tools.iter().map(|t| t.to_string()).collect(),
                    )
                })
                .collect()
        }

        /// A routine redeploy that changed nothing keeps the operator's console
        /// ceilings — clearing on every rebuild would silently revert a console
        /// action with nothing to show it had moved.
        #[test]
        fn an_unchanged_seed_carries_the_override() {
            let seed = [desk("finance", &["docs.*"])];
            let carried = carry_desk_tool_overrides(&seed, &seed, &held(&[("finance", &["web"])]));
            assert_eq!(
                carried.get("finance").map(Vec::as_slice),
                Some(&["web".to_string()][..])
            );
        }

        /// The security property: version control narrowing a desk must not be
        /// silently overridden by a wider console value set before the edit.
        #[test]
        fn a_changed_seed_clears_that_desks_override() {
            let carried = carry_desk_tool_overrides(
                &[desk("finance", &["*"])],
                &[desk("finance", &["docs.*"])],
                &held(&[("finance", &["web"])]),
            );
            assert!(carried.is_empty(), "{carried:?}");
        }

        /// Per desk, not whole-block: editing one department says nothing about
        /// another, and clearing both would revert an action nobody's edit was
        /// about.
        #[test]
        fn editing_one_desk_leaves_another_desks_override_alone() {
            let carried = carry_desk_tool_overrides(
                &[desk("finance", &["*"]), desk("creative", &["docs.*"])],
                &[desk("finance", &["docs.*"]), desk("creative", &["docs.*"])],
                &held(&[("finance", &["web"]), ("creative", &["media"])]),
            );
            assert!(!carried.contains_key("finance"), "{carried:?}");
            assert_eq!(
                carried.get("creative").map(Vec::as_slice),
                Some(&["media".to_string()][..])
            );
        }

        /// An operator-created desk has no seed value that could have changed,
        /// so its ceiling always survives a rebuild.
        #[test]
        fn an_override_for_a_desk_the_seed_does_not_declare_is_carried() {
            let carried = carry_desk_tool_overrides(
                &[desk("finance", &["*"])],
                &[desk("finance", &["*"])],
                &held(&[("adhoc", &["docs.*"])]),
            );
            assert!(carried.contains_key("adhoc"), "{carried:?}");
        }

        /// A desk deleted from the seed *has* changed — from declaring a ceiling
        /// to declaring nothing — so its stale override is dropped rather than
        /// outliving the desk in version control.
        #[test]
        fn deleting_a_desk_from_the_seed_clears_its_override() {
            let carried = carry_desk_tool_overrides(
                &[desk("finance", &["docs.*"])],
                &[],
                &held(&[("finance", &["web"])]),
            );
            assert!(carried.is_empty(), "{carried:?}");
        }
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
            runs.create_run(&id, NewRun::for_task("run-1", "t-1", "ceo"))
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

    /// **Issue #726, the headline**: a company whose data directory is destroyed
    /// keeps its at-most-once set and its parked approvals, because the journal
    /// lives in the storage backend rather than on the filesystem.
    ///
    /// This is the hosted failure, reproduced: on a mongodb tenant `/data` is
    /// documented ephemeral scratch, so container replacement — a deploy, a
    /// reschedule, a node drain, an OOM kill — takes `journal.jsonl` with it.
    /// Before this change every effect that had already executed became eligible
    /// to fire a second time and every parked approval, grant and standing grant
    /// silently vanished. The `remove_dir_all` below IS that container
    /// replacement.
    #[tokio::test]
    async fn a_backend_journal_survives_the_loss_of_the_whole_data_directory() {
        use crate::ports::journal::MemoryJournalStore;
        use crate::ports::types::EffectGroup;
        use crate::runtime::journal::{ApprovalConversation, TaskLink};

        let home = tmp_home("opencompany-journal-durability-");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = CompanyId::new("acme");
        // One sink, shared across both boots — the database that outlives the
        // container, standing in for sqlite/mongodb so the proof holds in the
        // default build rather than only behind a cargo feature.
        let sink = Arc::new(MemoryJournalStore::default());
        let approval = crate::ports::types::ApprovalId::new("ap-1");
        let effect = crate::ports::types::Effect {
            kind: "filing.submit".into(),
            group: EffectGroup::Sign,
            amount_usd: None,
            established_thread: false,
            first_time_counterparty: false,
            payload: serde_json::Value::Null,
            agent: None,
            run_id: None,
        };

        // --- boot 1: an effect executes at most once, and an approval parks.
        {
            let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
                .with_id(id.clone())
                .with_journal_store(sink.clone())
                .build()
                .await
                .expect("first boot");
            crate::runtime::cycle::execute_effect_once(&rt, "k", &effect, Some("t-1"))
                .await
                .expect("execute the effect once");
            rt.journal
                .record_parked(
                    &approval,
                    &effect,
                    1_000,
                    TaskLink::Task { id: "t-1".into() },
                    ApprovalConversation::default(),
                    None,
                )
                .await
                .expect("park an approval");
            assert!(rt.journal.is_executed("k"));
        }

        // --- the container is replaced: `/data` is gone, every byte of it.
        std::fs::remove_dir_all(home.path()).expect("destroy the data directory");
        assert!(
            !Bundle::new(home.path().to_path_buf(), &id)
                .journal_jsonl()
                .exists(),
            "the filesystem journal must really be gone for this to prove anything"
        );

        // --- boot 2: same backend, brand new (empty) data directory.
        let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .with_journal_store(sink)
            .build()
            .await
            .expect("second boot");

        assert!(
            rt.journal.is_executed("k"),
            "the committed key must survive the container: without it the effect \
             is eligible to fire a second time"
        );
        let pending = rt.journal.pending();
        assert_eq!(pending.len(), 1, "the parked approval must survive too");
        assert_eq!(
            pending[0].id, approval,
            "and with its original id, so the operator's console link still resolves"
        );
        assert_eq!(
            pending[0].task,
            Some(TaskLink::Task { id: "t-1".into() }),
            "and still linked to the card it was parked for"
        );
    }

    /// A company's journal file, as a previous host left it: `keys` committed in
    /// order, at the bundle path the fs journal has always used.
    async fn seed_filesystem_journal(home: &std::path::Path, id: &CompanyId, keys: &[&str]) {
        let journal = RuntimeJournal::new(Bundle::new(home.to_path_buf(), id).journal_jsonl());
        for (n, key) in keys.iter().enumerate() {
            journal
                .record_executed(
                    key,
                    ExecutedEffect {
                        kind: "filing.submit".into(),
                        amount_usd: None,
                        task_id: Some("t-1".into()),
                        at_millis: 1_000 + n as u64,
                        irreversible: true,
                    },
                )
                .await
                .expect("seed a legacy journal line");
        }
    }

    /// **Issue #726**: an existing filesystem journal is imported into the
    /// backend exactly **once**, and the receipt is what makes the second boot a
    /// no-op.
    ///
    /// The re-import is not a cosmetic inefficiency. `complete_import` clears
    /// before it copies, so a second import would delete every key the backend
    /// accumulated after the first one — un-committing effects that have already
    /// run. The gate is the only thing standing between the migration and that.
    #[tokio::test]
    async fn a_filesystem_journal_is_imported_once_and_the_receipt_blocks_the_rest() {
        use crate::ports::journal::MemoryJournalStore;

        let home = tmp_home("opencompany-journal-import-");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = CompanyId::new("acme");
        seed_filesystem_journal(home.path(), &id, &["k-legacy"]).await;

        let sink = Arc::new(MemoryJournalStore::default());
        let legacy_path = Bundle::new(home.path().to_path_buf(), &id).journal_jsonl();

        // --- boot 1: the file is imported, and left where it was.
        {
            let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest.clone())
                .with_id(id.clone())
                .with_journal_store(sink.clone())
                .build()
                .await
                .expect("first boot");
            assert!(
                rt.journal.is_executed("k-legacy"),
                "the pre-existing at-most-once key must reach the backend"
            );
            assert!(
                legacy_path.exists(),
                "the source file stays in place: a rollback to an older binary \
                 must still find the history it knows how to read"
            );
            // A key committed after the migration — this is what a second import
            // would destroy.
            rt.journal
                .record_executed(
                    "k-after",
                    ExecutedEffect {
                        kind: "payment.send".into(),
                        amount_usd: Some(12.0),
                        task_id: Some("t-2".into()),
                        at_millis: 2_000,
                        irreversible: true,
                    },
                )
                .await
                .expect("commit a key against the backend");
        }

        // --- boot 2: the receipt is closed, so nothing is re-imported.
        let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .with_journal_store(sink)
            .build()
            .await
            .expect("second boot");
        assert!(
            rt.journal.is_executed("k-after"),
            "a re-import would have cleared this key and re-armed an effect that \
             has already run"
        );
        assert!(rt.journal.is_executed("k-legacy"));
    }

    /// **Issue #726**: an import interrupted between the copy and the receipt is
    /// re-run whole, not resumed.
    ///
    /// A partial copy behind a closed gate is the bug the receipt exists to
    /// prevent — it is a set of at-most-once keys that quietly went missing. So
    /// the retry must **replace** what the interrupted attempt wrote rather than
    /// append to it: no duplicates, and nothing from the source left behind.
    #[tokio::test]
    async fn an_import_interrupted_before_its_receipt_is_re_run_whole() {
        use crate::ports::journal::{JournalStore, MemoryJournalStore};

        let home = tmp_home("opencompany-journal-partial-");
        let manifest: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        let id = CompanyId::new("acme");
        seed_filesystem_journal(home.path(), &id, &["k-0", "k-1", "k-2"]).await;

        // A crash mid-import: the first line copied, the receipt never written.
        let sink = Arc::new(MemoryJournalStore::default());
        let source = crate::store::fs::read_lines_lossy(
            &Bundle::new(home.path().to_path_buf(), &id).journal_jsonl(),
        )
        .await
        .expect("read the source journal");
        sink.complete_import(&id, vec![source[0].clone()])
            .await
            .expect("a partial copy");
        sink.forget_receipt(&id);

        let rt = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(id.clone())
            .with_journal_store(sink.clone())
            .build()
            .await
            .expect("boot after the interrupted import");

        for key in ["k-0", "k-1", "k-2"] {
            assert!(
                rt.journal.is_executed(key),
                "{key} must be present after the retry: a resumed-rather-than-restarted \
                 import is how at-most-once keys go missing"
            );
        }
        assert_eq!(
            sink.read_journal(&id).await.expect("read back").len(),
            3,
            "the retry replaces the partial copy; it must not append a second one"
        );
        assert!(
            sink.journal_imported(&id).await.expect("gate"),
            "and the retry closes the gate"
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

            // The company's own roster only: the global baseline is appended to
            // every manifest, and what it is granted is the company-wide belt
            // like any teammate that requests nothing — not something this
            // bundle's author decided, so not something this test pins.
            for agent in manifest.agents.iter().filter(|agent| !agent.global) {
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
                    notified_at_millis: None,
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
                    kind: crate::ports::SessionKind::Browser,
                    label: None,
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
        let spec = |run: &str, task: &str| NewRun::for_task(run, task, "ceo");

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

    /// Issue #983: a chat turn the host died under is reclaimed on **both**
    /// halves — a `Failed` row carrying the orphan reason, and a `TurnFailed`
    /// line closing the transcript bracket.
    ///
    /// Both are needed and neither is derivable from the other. The row makes
    /// `GET {scope}/runs` honest; the event makes the *conversation* honest,
    /// and without it the operator's question sits there with no answer and no
    /// explanation — which is what a message that never warranted a reply looks
    /// like too. The turn names no card, which is exactly why the row sweep
    /// alone leaves nothing an operator would ever find.
    #[tokio::test]
    async fn boot_reclaims_a_chat_turn_stranded_by_a_previous_host() {
        use crate::ports::runs::{NewRun, ORPHAN_ERROR, RunStatus};
        use crate::ports::types::{CompanyEvent, EventSeq};

        let home_dir = tmp_home("oc-turn-reap-");
        let home = home_dir.path().to_path_buf();
        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");

        let first_boot = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        first_boot
            .runs()
            .create_run(&id, NewRun::for_chat("turn-dead", "general", "general"))
            .await
            .unwrap();
        first_boot
            .runs()
            .begin_run(&id, "turn-dead", EventSeq::new(1))
            .await
            .unwrap();
        first_boot
            .events()
            .append(
                &id,
                CompanyEvent::TurnStarted {
                    turn_id: "turn-dead".to_string(),
                    chat_id: "general".to_string(),
                    parent: None,
                    by: None,
                },
            )
            .await
            .unwrap();

        // The host dies here: no settle, no reply, no failure line.
        drop(first_boot);

        let second_boot = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();

        let row = second_boot
            .runs()
            .get_run(&id, "turn-dead")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, RunStatus::Failed);
        assert_eq!(row.error.as_deref(), Some(ORPHAN_ERROR));
        assert_eq!(row.task_id, None, "a chat turn attempted no card");

        let swept: Vec<String> = second_boot
            .events()
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|s| match s.event {
                CompanyEvent::TurnFailed { turn_id, error } if turn_id == "turn-dead" => {
                    Some(error)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            swept,
            vec![crate::runtime::TURN_INTERRUPTED_BY_RESTART.to_string()],
            "the transcript bracket was left open by the boot sweep"
        );
    }

    /// **The negative half, and the one that matters most.** A live runtime
    /// rebuild must sweep *neither* half of a chat turn.
    ///
    /// This is the #290 lesson in its sharpest form. A rebuild happens in a
    /// process that has been serving, so "nothing from this process can be in
    /// flight" — the whole proof both sweeps rest on — is false. And a chat turn
    /// is more exposed than a workflow run: `rebuild_company` quiesces and
    /// drains the *cycle* lock, but the spawned turn task journals its replies
    /// and settles its row **after** the cycle returns, so a turn is routinely
    /// live at exactly the moment a rebuild reaches here. Sweeping would fail
    /// the row out from under it — its own settle is then rejected by the
    /// transition table — and tell the operator in the transcript that the turn
    /// failed, moments before its answer arrives.
    #[tokio::test]
    async fn a_rebuild_sweeps_no_live_chat_turn() {
        use crate::ports::runs::{NewRun, RunStatus};
        use crate::ports::types::{CompanyEvent, EventSeq};

        let home_dir = tmp_home("oc-turn-rebuild-");
        let home = home_dir.path().to_path_buf();
        let manifest = parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n");
        let id = CompanyId::new("acme");

        let live = RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        live.runs()
            .create_run(&id, NewRun::for_chat("turn-live", "general", "general"))
            .await
            .unwrap();
        live.runs()
            .begin_run(&id, "turn-live", EventSeq::new(1))
            .await
            .unwrap();
        live.events()
            .append(
                &id,
                CompanyEvent::TurnStarted {
                    turn_id: "turn-live".to_string(),
                    chat_id: "general".to_string(),
                    parent: None,
                    by: None,
                },
            )
            .await
            .unwrap();

        // The swap, as `rebuild_company` performs it: quiesce, hand over, build.
        live.quiesce().await;
        let successor = RuntimeBuilder::new(home, manifest)
            .with_id(id.clone())
            .with_handover(live.handover())
            .build()
            .await
            .unwrap();

        let row = successor
            .runs()
            .get_run(&id, "turn-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.status,
            RunStatus::Running,
            "a rebuild failed a turn that is still working"
        );
        assert!(
            !successor
                .events()
                .read_from(&id, EventSeq::new(0), usize::MAX)
                .await
                .unwrap()
                .iter()
                .any(|s| matches!(&s.event, CompanyEvent::TurnFailed { .. })),
            "a rebuild told the operator a live turn had failed"
        );

        // And the turn's own settle still lands, because nothing took the row
        // to a terminal state behind its back.
        successor
            .runs()
            .finish_run(
                &id,
                "turn-live",
                crate::ports::runs::RunOutcome::new(RunStatus::Succeeded),
            )
            .await
            .expect("the live turn can still settle itself");
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
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
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
        runs.create_run(&id, NewRun::for_task("run-a", "card-a", "ceo"))
            .await
            .unwrap();
        runs.begin_run(&id, "run-a", crate::ports::types::EventSeq::new(1))
            .await
            .unwrap();
        runs.create_run(&id, NewRun::for_task("run-b", "card-b", "ceo"))
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
            .create_run(&id, NewRun::for_task("run-a2", "card-a", "ceo"))
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
        // Seeded: README.md, Brand/, Brand/voice.md — plus runtime scaffold
        // (`Agents/` and `secrets/README.md`) which is not what the re-seed
        // gate is about.
        let seeded = |tree: &[crate::ports::WorkspaceNode]| {
            let secrets = tree
                .iter()
                .find(|node| {
                    node.parent_id.is_none()
                        && node.name == crate::company::workspace_scaffold::SECRETS_ROOT
                })
                .map(|node| node.id.as_str());
            let mut names: Vec<String> = tree
                .iter()
                .filter(|node| {
                    !crate::company::workspace_scaffold::SYSTEM_ROOTS.contains(&node.name.as_str())
                        && node.parent_id.as_deref() != secrets
                })
                .map(|node| node.name.clone())
                .collect();
            names.sort();
            names
        };
        let tree = runtime.workspace().tree(&id).await.unwrap();
        assert_eq!(seeded(&tree), vec!["Brand", "README.md", "voice.md"]);

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
            seeded(&tree),
            vec!["Brand", "README.md"],
            "workspace re-seeded despite operator deletion"
        );
        // Sanity: the record store still loads.
        assert!(runtime.store().load(&id).await.unwrap().is_some());
    }

    /// Boot lays down `Agents/` and operator-only `secrets/README.md`. `Desks/`
    /// has no producer, so it is minted on first use instead of standing empty.
    ///
    /// The per-agent folder is deliberately absent: it is minted the first time
    /// that agent produces something, so a roster of teammates who have done
    /// nothing yet leaves no trace in the tree.
    ///
    /// Also pins the two gates the seeding block above does NOT share: this
    /// runs with **no** `seed_dir` (the provisioned-tenant and desktop shape),
    /// and it runs again on a workspace that is no longer empty — which is how
    /// an existing company picks the root up.
    #[tokio::test]
    async fn boot_provisions_the_system_roots_and_nothing_inside_them() {
        use crate::company::workspace_scaffold::{AGENTS_ROOT, SECRETS_ROOT};
        use crate::ports::workspace::{NodeKind, WorkspaceOrigin};

        let home_dir = tmp_home("oc-agents-");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("acme");
        let roster = |agents: &str| {
            parse(&format!(
                "[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n{agents}"
            ))
        };

        let runtime = RuntimeBuilder::new(
            home.clone(),
            roster("[[agent]]\nid=\"ceo\"\nrole=\"Chief Executive\"\n"),
        )
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
        let tree = runtime.workspace().tree(&id).await.unwrap();
        let mut names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![AGENTS_ROOT, "README.md", SECRETS_ROOT],
            "boot provisions the managed roots with no seed dir — no `Desks/`, and no \
             folder for a teammate that has produced nothing"
        );
        for node in &tree {
            assert_eq!(node.created_by, WorkspaceOrigin::Seed);
        }
        assert_eq!(
            tree.iter()
                .filter(|node| node.parent_id.is_none() && node.kind == NodeKind::Folder)
                .count(),
            2
        );

        // An existing, non-empty workspace: an `is_empty` gate would have
        // skipped this boot entirely, and a company that predates the feature
        // would never get its roots.
        //
        // With one managed root, deleting it would leave the tree empty and
        // stop pinning that. A lazily-minted desk folder stands in for the
        // content a real company would have — and doubles as the #645 check
        // that boot neither re-manages, duplicates nor disturbs a `Desks/` that
        // already exists.
        crate::company::workspace_scaffold::ensure_desk_folder(
            runtime.workspace().as_ref(),
            &id,
            "creative_studio",
        )
        .await
        .unwrap();
        let agents_root = tree
            .iter()
            .find(|n| n.name == AGENTS_ROOT)
            .unwrap()
            .id
            .clone();
        runtime.workspace().delete(&id, &agents_root).await.unwrap();
        drop(runtime);
        let runtime = RuntimeBuilder::new(
            home,
            roster(
                "[[agent]]\nid=\"ceo\"\nrole=\"Chief Executive\"\n\
                 [[agent]]\nid=\"cmo\"\nrole=\"Chief Marketing\"\n",
            ),
        )
        .with_id(id.clone())
        .build()
        .await
        .unwrap();
        let tree = runtime.workspace().tree(&id).await.unwrap();
        let mut names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                AGENTS_ROOT,
                "Desks",
                "README.md",
                "creative_studio",
                SECRETS_ROOT,
            ],
            "the deleted root was re-provisioned, and the unmanaged `Desks/` left as it stood"
        );
    }

    /// The root is part of what a workspace *is*, not a projection of the
    /// roster: a company with no agents at all still gets it.
    #[tokio::test]
    async fn boot_provisions_the_roots_for_a_company_with_no_agents() {
        use crate::company::workspace_scaffold::{AGENTS_ROOT, SECRETS_ROOT};

        let home_dir = tmp_home("oc-noagents-");
        let id = CompanyId::new("acme");
        let runtime = RuntimeBuilder::new(
            home_dir.path().to_path_buf(),
            parse("[company]\nname=\"Acme\"\n[policy]\nmode=\"full\"\n"),
        )
        .with_id(id.clone())
        .build()
        .await
        .unwrap();

        let tree = runtime.workspace().tree(&id).await.unwrap();
        let mut names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec![AGENTS_ROOT, "README.md", SECRETS_ROOT]);
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

    // ---- issue #752: `repo` needs a backend that keeps secrets off disk ----

    /// A company that already bound a repository predates the bind-time gate,
    /// so the *restart* is where it has to be caught. A repo-granted company on
    /// an fs host does not come up.
    #[tokio::test]
    async fn a_repo_granted_company_does_not_boot_on_a_plaintext_secret_backend() {
        let home = tmp_home("oc-752-boot-");
        let manifest = parse(
            r#"
            [company]
            name = "Acme"
            [policy]
            mode = "full"
            [tools]
            allow = ["repo", "shell"]
            [[agent]]
            id = "ceo"
            role = "Chief"
            "#,
        );
        let err = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(CompanyId::new("acme"))
            // The default, spelled out: this is what a local `serve` is.
            .with_storage_kind(crate::store::StorageKind::Fs)
            .build()
            .await
            .expect_err("an fs host must refuse to bring up a repo-granted company");
        let message = err.to_string();
        assert!(message.contains("OPENCOMPANY_STORAGE=fs"), "{message}");
        assert!(message.contains("OPENCOMPANY_STORAGE=mongodb"), "{message}");
        assert!(message.contains("`repo` grant"), "{message}");
    }

    /// The wildcard case, which a check against `[tools].allow` alone would let
    /// through: `*` deliberately does **not** confer `repo`, so the company line
    /// reads as ungranted while the agent naming `repo` explicitly holds it.
    #[tokio::test]
    async fn an_agent_that_names_repo_under_a_wildcard_company_is_caught_too() {
        let home = tmp_home("oc-752-boot-wildcard-");
        let manifest = parse(
            r#"
            [company]
            name = "Acme"
            [policy]
            mode = "full"
            [tools]
            allow = ["*"]
            [[agent]]
            id = "ceo"
            role = "Chief"
            tools = ["repo"]
            "#,
        );
        // The company line itself is not an explicit grant — if it were, this
        // test would pass for the wrong reason.
        assert!(!crate::company::grants_repo_explicit(&manifest.tools.allow));
        let err = RuntimeBuilder::new(home.path().to_path_buf(), manifest)
            .with_id(CompanyId::new("acme"))
            .with_storage_kind(crate::store::StorageKind::Fs)
            .build()
            .await
            .expect_err("an agent-level `repo` grant must be caught at boot");
        assert!(err.to_string().contains("OPENCOMPANY_STORAGE=fs"), "{err}");
    }

    /// The other side, without which the two above only prove the check is on:
    /// the same company boots on the backend that keeps secrets out of the
    /// container, and a company that grants no `repo` boots on fs unaffected.
    #[tokio::test]
    async fn the_same_company_boots_where_secrets_leave_the_container() {
        let home = tmp_home("oc-752-boot-ok-");
        let granted = parse(
            r#"
            [company]
            name = "Acme"
            [policy]
            mode = "full"
            [tools]
            allow = ["repo", "shell"]
            [[agent]]
            id = "ceo"
            role = "Chief"
            "#,
        );
        RuntimeBuilder::new(home.path().to_path_buf(), granted)
            .with_id(CompanyId::new("acme"))
            .with_storage_kind(crate::store::StorageKind::Mongodb)
            .build()
            .await
            .expect("mongodb-backed secrets must clear the #752 boot gate");

        let ungranted_home = tmp_home("oc-752-boot-ungranted-");
        let ungranted = parse(
            r#"
            [company]
            name = "Acme"
            [policy]
            mode = "full"
            [tools]
            allow = ["shell", "web"]
            [[agent]]
            id = "ceo"
            role = "Chief"
            "#,
        );
        RuntimeBuilder::new(ungranted_home.path().to_path_buf(), ungranted)
            .with_id(CompanyId::new("acme"))
            .with_storage_kind(crate::store::StorageKind::Fs)
            .build()
            .await
            .expect("a company without `repo` is untouched by this gate");
    }

    // ---- `[policy]` override across a rebuild (issue #562) ----------------

    fn seed_policy(mode: &str, always: &[&str], under: Option<f64>) -> Policy {
        Policy {
            mode: mode.to_string(),
            always_approve: always.iter().map(|s| s.to_string()).collect(),
            auto_approve_under_usd: under,
            approval_ttl_hours: None,
        }
    }

    fn held_override(mode: &str) -> PolicyOverride {
        use crate::ports::types::{Actor, ActorKind};
        PolicyOverride {
            mode: Some(mode.to_string()),
            always_approve: None,
            set_by: Actor {
                kind: ActorKind::User,
                id: "admin-1".to_string(),
            },
            at_millis: 1_700_000_000_000,
        }
    }

    /// A rebuild that does not touch `[policy]` leaves the console override
    /// alone (issue #562).
    ///
    /// The half that makes the feature durable. Clearing on every rebuild would
    /// mean a routine redeploy silently reverting the operator's console action,
    /// with nothing in the console showing the tier had moved back — the exact
    /// mirror of the failure the other half prevents.
    #[test]
    fn an_unchanged_seed_policy_leaves_the_override_alone() {
        let seed = seed_policy("supervised", &["payment.send"], None);
        let carried = carry_policy_override(&seed, &seed.clone(), Some(&held_override("full")));
        assert_eq!(carried.and_then(|o| o.mode).as_deref(), Some("full"));
    }

    /// A seed `[policy]` change clears the override — version control wins when
    /// it speaks.
    ///
    /// **The security half.** Without it, an operator tightening `[policy]` in
    /// `company.toml` and redeploying would find a looser console override
    /// silently still in force: a runtime write outliving a seed rollback, which
    /// is the named harm that makes `[tools]` / `[policy]` seed-authoritative in
    /// the first place. An approval gate is precisely what that rule was written
    /// about.
    #[test]
    fn a_changed_seed_policy_clears_the_override() {
        let before = seed_policy("full", &["payment.send"], None);
        let tightened = seed_policy("supervised", &["payment.send"], None);
        assert!(
            carry_policy_override(&before, &tightened, Some(&held_override("full"))).is_none(),
            "a tightened seed must clear a looser console override"
        );

        // Loosening the seed clears it too. The rule is "the seed spoke", not
        // "the seed got stricter" — an operator who edits `[policy]` at all has
        // turned their attention to the gate, and guessing which of their edits
        // was meant to lose to the console is a guess that can pick wrong
        // silently.
        let loosened = seed_policy("full", &["payment.send"], None);
        assert!(
            carry_policy_override(&tightened, &loosened, Some(&held_override("readonly")))
                .is_none()
        );
    }

    /// Any field of `[policy]` counts as the seed speaking, not just `mode`.
    ///
    /// `always_approve` is the operator's real lever — it wins over every tier
    /// including `full` — so an edit to it that left a console override standing
    /// would be the same hole through a different field.
    #[test]
    fn every_policy_field_counts_as_the_seed_speaking() {
        let base = seed_policy("supervised", &["payment.send"], None);

        let list_changed = seed_policy("supervised", &["payment.send", "filing.submit"], None);
        assert!(
            carry_policy_override(&base, &list_changed, Some(&held_override("full"))).is_none()
        );

        let threshold_changed = seed_policy("supervised", &["payment.send"], Some(1.0));
        assert!(
            carry_policy_override(&base, &threshold_changed, Some(&held_override("full")))
                .is_none()
        );
    }

    /// With no override held there is nothing to carry, whatever the seed did.
    #[test]
    fn no_override_carries_nothing() {
        let before = seed_policy("supervised", &[], None);
        let after = seed_policy("full", &[], None);
        assert!(carry_policy_override(&before, &before.clone(), None).is_none());
        assert!(carry_policy_override(&before, &after, None).is_none());
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

        // The accessor the console's channel picker reads (#813) names the
        // openhuman-backed provider channel and NOT `operator`: delivery
        // refuses the operator adapter by name, so offering it as a
        // destination would offer the one target guaranteed to fail (#981).
        let deliverable = runtime.deliverable_channel_ids();
        assert_eq!(deliverable, vec!["email".to_string()], "{deliverable:?}");

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
        // The accessor the console's channel picker reads (#813): `operator` is
        // the only wired adapter here, and it is not a delivery target — so a
        // workflow on this runtime has NOWHERE to deliver, and the honest
        // answer is an empty picker (#981). It previously answered
        // `["operator"]`, which is what put the guaranteed-to-fail target in
        // front of authors.
        assert!(
            runtime.deliverable_channel_ids().is_empty(),
            "an operator-only runtime has no workflow delivery channel: {:?}",
            runtime.deliverable_channel_ids()
        );

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

    #[tokio::test]
    async fn wires_manifest_and_overlay_desks_as_delivery_channels() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = parse(
            r#"
            [company]
            name = "Acme"
            [[agent]]
            id = "ceo"
            role = "Chief"
            [[group_chat]]
            id = "engineering"
            name = "Engineering"
            members = ["ceo"]
            "#,
        );
        let id = CompanyId::new("acme");
        FsCompanyStore::new(dir.path())
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: vec![crate::ports::types::OverlayDesk {
                    id: "research".to_string(),
                    name: "Research".to_string(),
                    description: None,
                    members: vec!["ceo".to_string()],
                }],
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(dir.path(), manifest)
            .with_id(id)
            .build()
            .await
            .unwrap();

        let ids: Vec<_> = runtime
            .channels
            .iter()
            .map(|channel| channel.channel_id())
            .collect();
        assert!(ids.contains(&"engineering"));
        assert!(ids.contains(&"research"));

        // Both desks are real delivery targets — they write to the company's
        // durable event log — and `operator` is not one of them (#981).
        let deliverable = runtime.deliverable_channel_ids();
        assert!(
            deliverable.contains(&"engineering".to_string()),
            "{deliverable:?}"
        );
        assert!(
            deliverable.contains(&"research".to_string()),
            "{deliverable:?}"
        );
        assert!(
            !deliverable.contains(&"operator".to_string()),
            "{deliverable:?}"
        );
    }

    /// **The invariant that would have caught #981.** The picker's set and the
    /// delivery layer's set are produced by the same `build()`, from the same
    /// adapters, and must be the same list.
    ///
    /// They were not. `WorkflowDeliveryDeps.channels` dropped `operator` with an
    /// inline filter while the accessor the console reads returned every adapter
    /// — so an author was offered a destination the runner refuses by name, and
    /// nothing in the build asserted the two agreed. Pinning them together is
    /// what makes a future divergence a test failure rather than a run that
    /// reports `channel-not-wired` for a target the console suggested.
    ///
    /// Needs the harness arm, because that is the only site that wires
    /// `WorkflowDeliveryDeps` at all.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn the_picker_set_equals_the_delivery_deps_the_same_build_wired() {
        use crate::harness::HarnessPool;

        let home_dir = tmp_home("oc-981-invariant-");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("invariant-co");
        let manifest = parse(
            r#"
            [company]
            name = "Invariant Co"

            [policy]
            mode = "full"

            [[agent]]
            id = "eng1"
            role = "Engineer One"

            [[group_chat]]
            id = "engineering"
            name = "Engineering"
            members = ["eng1"]
            "#,
        );

        let stub = spawn_stub("ack").await;
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

        let delivery = runtime
            .workflow_harness_deps
            .as_ref()
            .expect("the harness arm wires workflow deps")
            .delivery
            .as_ref()
            .expect("the harness arm wires delivery deps");
        let deps_channels: Vec<String> = delivery
            .channels
            .iter()
            .map(|channel| channel.channel_id().to_string())
            .collect();

        assert_eq!(
            runtime.deliverable_channel_ids(),
            deps_channels,
            "the destination picker offers a set the delivery layer does not accept"
        );
        // Not vacuous in either direction: the runtime really did wire the
        // operator adapter, and the desk really is deliverable.
        assert!(
            runtime
                .channels
                .iter()
                .any(|channel| channel.channel_id() == OPERATOR_CHANNEL),
            "the operator adapter must be wired, or the exclusion proves nothing"
        );
        assert_eq!(deps_channels, vec!["engineering".to_string()]);
    }

    /// A desk added to `company.toml` since the last boot is wired on this one.
    ///
    /// The persisted record carries the manifest of a PREVIOUS boot, so reusing
    /// it for desk resolution silently wires yesterday's `[[group_chat]]` list:
    /// a desk the operator has just added would never become a delivery
    /// destination, and the only symptom would be a workflow refusing a
    /// destination the manifest plainly declares. The overlay halves still come
    /// from the persisted record, which the `research` assertion pins.
    #[tokio::test]
    async fn a_desk_added_to_the_manifest_since_the_last_boot_is_wired() {
        use crate::ports::CompanyStore;
        use crate::store::FsCompanyStore;

        let dir = tempfile::tempdir().unwrap();
        let common = r#"
            [company]
            name = "Acme"
            [[agent]]
            id = "ceo"
            role = "Chief"
            [[group_chat]]
            id = "engineering"
            name = "Engineering"
            members = ["ceo"]
        "#;
        let persisted = parse(common);
        let booting = parse(&format!(
            "{common}\n[[group_chat]]\nid = \"growth\"\nname = \"Growth\"\nmembers = [\"ceo\"]\n"
        ));
        let id = CompanyId::new("acme");
        FsCompanyStore::new(dir.path())
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: persisted,
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: vec![crate::ports::types::OverlayDesk {
                    id: "research".to_string(),
                    name: "Research".to_string(),
                    description: None,
                    members: vec!["ceo".to_string()],
                }],
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        let runtime = RuntimeBuilder::new(dir.path(), booting)
            .with_id(id)
            .build()
            .await
            .unwrap();

        let ids: Vec<_> = runtime
            .channels
            .iter()
            .map(|channel| channel.channel_id())
            .collect();
        assert!(ids.contains(&"growth"), "{ids:?}");
        assert!(ids.contains(&"engineering"), "{ids:?}");
        assert!(ids.contains(&"research"), "{ids:?}");
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

    /// **Issue #276's durability claim, at the path that actually threatens it.**
    ///
    /// `merge_enabled_workflows` re-derives `[workflows].enabled` from seed ∪
    /// overlay ids on every build — it re-arms that list by design. The pause
    /// switch is a separate field precisely so a rebuild cannot undo it, and
    /// this is the test that says so: disable a workflow, build again over the
    /// same store, and it must still be disabled.
    ///
    /// The store round-trip tests cover save→load; this covers build→save, which
    /// is a different write and the one that would silently re-arm every paused
    /// schedule on restart.
    #[tokio::test]
    async fn a_rebuild_keeps_a_paused_workflow_paused() {
        use crate::ports::types::OverlayWorkflow;
        use crate::store::FsCompanyStore;

        let home_dir = tmp_home("oc-paused-rebuild-");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("pause-co");
        let manifest = parse(
            r#"
            [company]
            name = "Pause Co"

            [[agent]]
            id = "assistant"
            role = "Assistant"
            "#,
        );

        // First build materializes the record.
        RuntimeBuilder::new(home.clone(), manifest.clone())
            .with_id(id.clone())
            .build()
            .await
            .unwrap();

        // An overlay workflow, switched off — the state an operator would have
        // left behind by clicking Pause.
        let store = FsCompanyStore::new(home.clone());
        let mut record = store.load(&id).await.unwrap().unwrap();
        record.overlay_workflows.push(OverlayWorkflow {
            id: "digest".to_string(),
            toml: "id = \"digest\"\nname = \"Digest\"\n[[node]]\nid = \"start\"\nkind = \"trigger\"\nname = \"Start\"\n"
                .to_string(),
        });
        record.set_workflow_enabled("digest", false);
        store.save(&record).await.unwrap();

        // Rebuild, exactly as a restart does.
        RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();

        let rebuilt = store.load(&id).await.unwrap().unwrap();
        assert!(
            !rebuilt.workflow_enabled("digest"),
            "the rebuild re-armed a paused workflow — `disabled_workflows` was not carried forward"
        );
        // And the merge it has to survive did run: the id is back in the
        // manifest's declaration list, which is exactly why that list could not
        // have been the switch.
        assert!(
            rebuilt
                .manifest
                .workflows
                .enabled
                .contains(&"digest".to_string()),
            "merge_enabled_workflows did not run, so this test proves nothing"
        );
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

    /// Issue #707: a desk reorder reaches a **resident** runtime, with no
    /// rebuild and no restart.
    ///
    /// This is the assertion that was missing. The neighbouring #133 test is
    /// named `..._after_rebuild` and rebuilds the brain before asserting, so it
    /// only ever proved the builder *seeds* the order — nobody had asked what a
    /// live company does when the operator saves one. The answer was: keep
    /// routing to the old lead until the process restarted, because
    /// `HarnessBrain.record` was a build-time snapshot and the only caller of
    /// `rebuild_company` is an inference-settings change.
    ///
    /// So the write here goes through the store exactly as
    /// `set_desk_order` (`src/server/operator.rs`) does — load, mutate, save —
    /// and then the SAME runtime object runs a second turn. No rebuild happens
    /// anywhere in this test, which is the whole point of it.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_desk_reorder_reaches_a_resident_runtime_without_a_rebuild() {
        use crate::harness::HarnessPool;
        use crate::ports::types::{CompanyEvent, OverlayDeskOrder};
        use crate::store::{FsCompanyStore, FsContextStore};

        let home_dir = tmp_home("oc-707-order-");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("order-co");

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

        // The blueprint lead is `eng1`; no operator order yet.
        let store = FsCompanyStore::new(home.clone());
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

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

        let desk_turn = |text: &'static str| CompanyEvent::OperatorMessage {
            parent: None,
            text: text.to_string(),
            by: None,
            chat: Some("eng".to_string()),
            deliverable: None,
        };

        // Baseline: the blueprint lead answers. Asserted rather than assumed, so
        // a later failure cannot be explained away as "the desk never routed".
        runtime
            .run_cycle(vec![desk_turn("who leads?")])
            .await
            .expect("first cycle");
        let context: Arc<dyn ContextStore> = Arc::new(FsContextStore::new(home.clone()));
        let labels = |outcomes: Vec<crate::ports::types::ChunkMeta>| -> Vec<String> {
            outcomes.into_iter().map(|m| m.label).collect()
        };
        let before = labels(context.list(&id, "task-outcome/").await.unwrap());
        assert!(
            before.contains(&"task-outcome/eng1".to_string()),
            "the blueprint lead must answer before the reorder; saw {before:?}"
        );

        // The console write: load, mutate, save. Nothing rebuilds.
        let mut record = store.load(&id).await.unwrap().expect("record");
        record.overlay_desk_order.push(OverlayDeskOrder {
            desk_id: "eng".to_string(),
            ordered: vec!["eng2".to_string(), "eng1".to_string()],
        });
        store.save(&record).await.unwrap();

        // The same runtime, a second turn.
        runtime
            .run_cycle(vec![desk_turn("who leads now?")])
            .await
            .expect("second cycle");
        let after = labels(context.list(&id, "task-outcome/").await.unwrap());
        assert!(
            after.contains(&"task-outcome/eng2".to_string()),
            "the reordered lead eng2 never answered — the resident brain routed on a stale \
             record; saw {after:?}"
        );
    }

    /// Issue #707, the same defect through `overlay_desks` + `overlay_desk_members`:
    /// a desk the operator creates on a **resident** runtime is reachable.
    ///
    /// Sharper than staleness alone, because it pins a divergence: the store
    /// resolves the new desk's lead while the runtime routes as though the desk
    /// does not exist. Both are asserted, at the same instant.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    async fn a_new_overlay_desk_is_reachable_on_a_resident_runtime() {
        use crate::harness::HarnessPool;
        use crate::ports::types::{CompanyEvent, OverlayDesk, OverlayDeskMember};
        use crate::store::{FsCompanyStore, FsContextStore};

        let home_dir = tmp_home("oc-707-desk-");
        let home = home_dir.path().to_path_buf();
        let id = CompanyId::new("desk-co");

        let manifest = parse(
            r#"
            [company]
            name = "Desk Co"

            [policy]
            mode = "full"

            [[agent]]
            id = "eng1"
            role = "Engineer One"

            [[agent]]
            id = "eng2"
            role = "Engineer Two"
            "#,
        );

        let store = FsCompanyStore::new(home.clone());
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
            })
            .await
            .unwrap();

        let stub = spawn_stub("desk reply").await;
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

        // The console creates a desk and puts `eng2` on it.
        let mut record = store.load(&id).await.unwrap().expect("record");
        // Deliberately EMPTY: the `OverlayDeskMember` row below is the only
        // membership source, so this test cannot pass by way of a desk's own
        // founding members. Without that, it would still be green if
        // `effective_desk_members` ignored `overlay_desk_members` outright —
        // which is half of what it is here to prove.
        record.overlay_desks.push(OverlayDesk {
            id: "design".to_string(),
            name: "Design".to_string(),
            description: None,
            members: Vec::new(),
        });
        record.overlay_desk_members.push(OverlayDeskMember {
            desk_id: "design".to_string(),
            agent_id: "eng2".to_string(),
        });
        store.save(&record).await.unwrap();

        // What every already-correct consumer sees at this instant.
        let fresh = store.load(&id).await.unwrap().unwrap();
        assert_eq!(
            crate::runtime::delegation_tools::desk_lead(&fresh, "design"),
            Some("eng2".to_string()),
            "the stored record must resolve the new desk, or this test proves nothing"
        );

        runtime
            .run_cycle(vec![CompanyEvent::OperatorMessage {
                parent: None,
                text: "hello design".to_string(),
                by: None,
                chat: Some("design".to_string()),
                deliverable: None,
            }])
            .await
            .expect("cycle");

        let context: Arc<dyn ContextStore> = Arc::new(FsContextStore::new(home.clone()));
        let routed: Vec<String> = context
            .list(&id, "task-outcome/")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.label)
            .collect();
        assert!(
            routed.contains(&"task-outcome/eng2".to_string()),
            "a desk chat must reach the desk's member; the runtime routed as though the desk \
             did not exist; saw {routed:?}"
        );
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
                overlay_policy: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
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
                deliverable: None,
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
