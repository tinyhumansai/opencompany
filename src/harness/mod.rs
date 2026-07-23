//! WS4 — openhuman embedded as a library (the harness).
//!
//! This module supersedes the out-of-process OpenHuman seam
//! (`src/openhuman/{launcher,rpc,tools,channel}.rs`, JSON-RPC behind
//! `openhuman-rpc`) with **direct library embedding** of `vendor/openhuman`
//! (`openhuman_core`): one openhuman [`Agent`](oh::agent::Agent) per manifest
//! `[[agent]]`, wired with memory, an inference provider, an approval policy,
//! and a workspace through [`AgentBuilder`](oh::agent::AgentBuilder).
//!
//! Compiled only under `feature = "openhuman"`. The default build links none of
//! it and keeps its offline, echo-brained behaviour.
//!
//! ## Layout
//!
//! * [`build`] — manifest `[[agent]]` → `AgentBuilder`.
//! * [`provider`] — hosted Medulla [`Provider`] + a `MockProvider` for tests.
//! * [`memory`] — [`OcMemory`](memory::OcMemory): openhuman `Memory` over the
//!   opencompany [`ContextStore`](crate::ports::ContextStore).
//! * [`policy`] — [`ApprovalPolicy`](policy::ApprovalPolicy): `[policy]` →
//!   openhuman `ToolPolicy`.
//! * [`cost`] — [`TurnCost`](oh::agent::cost::TurnCost) → ledger + usage meter.
//!
//! ## Flagged seams
//!
//! * **Group-chat / desk routing** is opencompany's job (openhuman is
//!   single-agent). v1 is single-responder; the full ops `chat` handler that
//!   resolves a desk's members and journals the `AgentReply` is WS3.
//!
//! Live turn cost is **wired**: [`CompanyAgent::run`] reads the completed turn's
//! token/cost totals from openhuman's public
//! [`Agent::last_turn_usage`](oh::agent::Agent::last_turn_usage) accessor and
//! [`HarnessPool::run`] records them through [`cost::record_turn_cost`]. Usage
//! only reaches the ledger/meter when the provider reports it — the
//! [`HostedProvider`](provider::HostedProvider) parses it off the wire; the
//! offline [`MockProvider`](provider::MockProvider) does not, so test turns stay
//! inert.

pub mod brain;
pub mod build;
pub mod cost;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod memory;
pub mod memory_loop;
pub mod orchestrator;
pub mod policy;
pub mod provider;
pub mod skills;

pub use brain::HarnessBrain;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use openhuman_core::openhuman as oh;
use tokio::sync::{Mutex, RwLock};

use oh::agent::Agent;
use oh::inference::provider::Provider;

use crate::company::Policy;
use crate::company::mcp::McpServerDecl;
use crate::error::OpenCompanyError;
use crate::harness::cost::{TurnUsage, record_turn_cost};
use crate::harness::orchestrator::DelegationQueue;
use crate::harness::policy::ApprovalPolicy;
use crate::ports::skills_state::{SkillState, SkillStateStore};
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{CompanyStore, ContextStore, EventLog, FactStore, TaskStore, UsageMeter};

/// Shared dependencies every harness-built agent draws on.
#[derive(Clone)]
pub struct HarnessDeps {
    /// The inference provider shared across a company's agents.
    pub provider: Arc<dyn Provider>,
    /// Stable provider slug attributed to usage samples (e.g. `managed`).
    pub provider_slug: String,
    /// Context store backing every agent's [`OcMemory`](memory::OcMemory).
    pub context: Arc<dyn ContextStore>,
    /// Company store the cost hook appends ledger entries to.
    pub store: Arc<dyn CompanyStore>,
    /// Optional usage meter (WS5 seam); `None` skips usage sampling.
    pub meter: Option<Arc<dyn UsageMeter>>,
    /// Root under which per-agent workspace directories are created
    /// (`{root}/{company}/{agent}/workspace`).
    pub workspace_root: PathBuf,
    /// Optional model/tier applied to every agent, overriding the per-agent
    /// `tier` → model mapping. Set from the resolved hosted-inference model so
    /// the whole roster addresses the configured workload (e.g. `chat-v1`).
    /// `None` keeps each agent's tier-derived default.
    pub model_override: Option<String>,
    /// The company's task board, so a [`TaskDispatched`] cycle can load the
    /// dispatched card and write its result back. `None` off the task path (the
    /// chat brain leaves the board untouched).
    ///
    /// [`TaskDispatched`]: crate::ports::types::CompanyEvent::TaskDispatched
    pub tasks: Option<Arc<dyn TaskStore>>,
    /// The company's skill-delta store, so a built agent can see its effective
    /// skill set (company-dir skills ∪ operator deltas ∪ custom docs) as read
    /// tools + a prompt catalogue. `None` leaves the agent skill-less (the chat
    /// path off the skills seam builds no skill surface).
    ///
    /// See [`skills`](crate::harness::skills) — this is the read-only catalogue
    /// slice; skill *execution* is deferred.
    pub skills: Option<Arc<dyn SkillStateStore>>,
    /// The company's source directory (`companies/<name>`), whose `skills/`
    /// subtree supplies the committed skill bundles unioned into the effective
    /// set. `None` surfaces only the operator deltas.
    pub skills_source_dir: Option<PathBuf>,
    /// The company's effective MCP servers (issue #50), resolved to **data**
    /// (manifest `[[mcp_server]]` ∪ the runtime index, with each server's
    /// outbound credential materialized to
    /// [`AuthMaterial`](crate::company::mcp::AuthMaterial)) before deps
    /// construction. `build_agent` is synchronous but the
    /// [`SecretStore`](crate::ports::SecretStore) is async, so the runtime
    /// builder resolves these ahead of time; each agent then filters the set by
    /// its `mcp:*` tool grants. Empty leaves the agent with no MCP bridge tools.
    pub mcp_servers: Vec<McpServerDecl>,
    /// The company's durable [`FactStore`], surfaced to the orchestrator agent
    /// through the `query_company` read tool (issue #53). `None` leaves the
    /// orchestrator without the facts half of its insight surface (the chat path
    /// off the orchestrator seam wires nothing).
    pub facts: Option<Arc<dyn FactStore>>,
    /// The company's [`EventLog`], surfaced to the orchestrator agent through
    /// the `query_company` read tool for recent-activity context (issue #53).
    /// `None` leaves the orchestrator without the recent-events half.
    pub events: Option<Arc<dyn EventLog>>,
    /// The shared delegation queue the orchestrator's `spawn_task` /
    /// `delegate_to_desk` tools push onto and the [`HarnessBrain`] drains after
    /// an orchestrator turn (issue #53). A [`DelegationQueue`] is a cheap shared
    /// handle; cloning `HarnessDeps` shares one queue between the tools built
    /// into the agent and the brain that drains it. Default is an empty queue.
    pub delegations: DelegationQueue,
}

/// One live openhuman agent, keyed by its manifest id.
pub struct CompanyAgent {
    /// The manifest agent id.
    pub agent_id: String,
    /// The manifest agent's human-readable role.
    pub role: String,
    /// The embedded openhuman session. A [`Mutex`] because a `turn` takes
    /// `&mut self` and one agent must serialise its own turns.
    agent: Mutex<Agent>,
}

impl CompanyAgent {
    /// Runs one turn against this agent, returning its reply text and the turn's
    /// token/cost totals.
    ///
    /// The usage is read from the just-completed turn via openhuman's public
    /// [`Agent::last_turn_usage`](oh::agent::Agent::last_turn_usage) accessor
    /// while the agent lock is still held (so a concurrent turn can't overwrite
    /// it). An offline provider that reports no usage yields a zero
    /// [`TurnUsage`], which the cost hook treats as inert.
    pub async fn run(&self, message: &str) -> crate::Result<(String, TurnUsage)> {
        let mut agent = self.agent.lock().await;
        let reply = agent
            .turn(message)
            .await
            .map_err(|e| OpenCompanyError::Harness(format!("turn for '{}': {e}", self.agent_id)))?;
        let usage = agent
            .last_turn_usage()
            .map(|u| TurnUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cached_input_tokens: u.cached_input_tokens,
                cost_usd: u.cost_usd,
            })
            .unwrap_or_default();
        Ok((reply, usage))
    }
}

/// A company's cached roster together with the skill deltas it was built from.
///
/// Caching the deltas is what lets [`HarnessPool::ensure`] tell whether the
/// operator's effective skill set has moved (a skill authored, enabled, or
/// disabled in the console *after* the roster was first built) and rebuild only
/// then — so a fresh skill reaches every agent on the next cycle without a
/// process restart, and an unchanged set leaves the live agents (and their
/// conversation state) untouched.
struct CompanyRoster {
    /// The live agents, one per manifest `[[agent]]`.
    agents: Vec<Arc<CompanyAgent>>,
    /// The skill deltas the agents were built from, sorted by slug so the
    /// freshness comparison is store-order-agnostic.
    skill_deltas: Vec<SkillState>,
}

/// A pool of live agents, one roster per company.
pub struct HarnessPool {
    rosters: RwLock<HashMap<CompanyId, CompanyRoster>>,
}

impl Default for HarnessPool {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessPool {
    /// Builds an empty pool.
    pub fn new() -> Self {
        Self {
            rosters: RwLock::new(HashMap::new()),
        }
    }

    /// Ensures a company's roster is built, cached, and **current with the
    /// operator's skill deltas**.
    ///
    /// On every call the current deltas are fetched once (a cheap store `list`,
    /// sorted by slug so the compare is store-order-agnostic) and matched against
    /// the deltas the cached roster was built from:
    ///
    /// * no cached roster, or the deltas moved → (re)build the roster and cache
    ///   it alongside the deltas it was built from;
    /// * deltas unchanged → the cached roster is still current; return it
    ///   untouched (no rebuild, so each agent's conversation history survives).
    ///
    /// This is what makes a skill authored / enabled / disabled in the console
    /// reach every agent on the **next** cycle — [`HarnessBrain`] calls `ensure`
    /// at the top of each cycle — rather than only on the first build (the prior
    /// behaviour cached the roster forever and never re-read the deltas, so a
    /// skill written after the operator's first turn was invisible until
    /// restart). A company with no skills store (`deps.skills == None`) compares
    /// empty-to-empty, so it still builds exactly once.
    pub async fn ensure(&self, company: &CompanyRecord, deps: &HarnessDeps) -> crate::Result<()> {
        // Current operator skill deltas, sorted by slug for a store-agnostic
        // compare. `build_roster`/`build_agent` stay synchronous and fold these
        // into each agent's effective skill set.
        let mut skill_deltas = match &deps.skills {
            Some(store) => store.list(&company.id).await?,
            None => Vec::new(),
        };
        skill_deltas.sort_by(|a, b| a.slug.cmp(&b.slug));

        // Fast path: a cached roster built from the same deltas is still current.
        {
            let guard = self.rosters.read().await;
            if let Some(roster) = guard.get(&company.id)
                && roster.skill_deltas == skill_deltas
            {
                return Ok(());
            }
        }

        // The deltas moved (or there is no roster yet): rebuild. Build outside
        // the write lock (synchronous but non-trivial), then install under a
        // double-check so a racing `ensure` that already rebuilt from the same
        // deltas wins and this build is dropped.
        let agents = build_roster(company, deps, &skill_deltas)?;
        let mut guard = self.rosters.write().await;
        if let Some(existing) = guard.get(&company.id)
            && existing.skill_deltas == skill_deltas
        {
            return Ok(());
        }
        guard.insert(
            company.id.clone(),
            CompanyRoster {
                agents,
                skill_deltas,
            },
        );
        Ok(())
    }

    /// Routes a message to one agent and returns its reply, recording the turn's
    /// cost. `agent_id` must name a member of the company's roster.
    ///
    /// Desk routing (which agent answers a group chat) is the caller's job — v1
    /// is single-responder and the WS3 chat handler picks the addressed member.
    pub async fn run(
        &self,
        company: &CompanyId,
        agent_id: &str,
        message: &str,
        deps: &HarnessDeps,
    ) -> crate::Result<String> {
        let agent = {
            let guard = self.rosters.read().await;
            let roster = guard
                .get(company)
                .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.to_string()))?;
            roster
                .agents
                .iter()
                .find(|a| a.agent_id == agent_id)
                .cloned()
                .ok_or_else(|| {
                    OpenCompanyError::InvalidRequest(format!(
                        "agent '{agent_id}' is not on company '{company}' roster"
                    ))
                })?
        };

        // Retrieve→inject: pull the top-K prior task outcomes relevant to this
        // message and prepend them as context. On a cold store this yields no
        // hits and the message is passed through unchanged.
        let hits = deps
            .context
            .search(company, message, memory_loop::RETRIEVE_TOP_K)
            .await?;
        let augmented = memory_loop::inject(message, &hits);

        // Run the turn and record its real cost. `CompanyAgent::run` reads the
        // turn's token/cost totals from openhuman's public `last_turn_usage()`
        // accessor; a zero-usage turn (offline provider) writes nothing.
        let (reply, turn_cost) = agent.run(&augmented).await?;
        record_turn_cost(
            &turn_cost,
            agent_id,
            &deps.provider_slug,
            company,
            deps.store.as_ref(),
            deps.meter.as_deref(),
        )
        .await?;

        // Store: persist the outcome (original task + reply) so it compounds
        // into later turns. Without this the harness never writes memory back.
        deps.context
            .put(
                company,
                memory_loop::outcome_chunk(agent_id, message, &reply),
            )
            .await?;

        Ok(reply)
    }

    /// Number of companies currently resident in the pool (test/observability).
    pub async fn resident_companies(&self) -> usize {
        self.rosters.read().await.len()
    }
}

/// Build every roster agent for a company from its manifest.
///
/// `skill_deltas` are the company's operator skill overrides (fetched once by
/// the async caller); every agent folds them into its effective skill set.
pub(crate) fn build_roster(
    company: &CompanyRecord,
    deps: &HarnessDeps,
    skill_deltas: &[SkillState],
) -> crate::Result<Vec<Arc<CompanyAgent>>> {
    let policy: &Policy = &company.manifest.policy;
    let company_name = &company.manifest.company.name;
    // The orchestrator agent (tier `orchestrator`, else the first agent) receives
    // the delegating-orchestrator persona + tools (issue #53).
    let orchestrator = orchestrator::orchestrator_id(&company.manifest.agents);
    company
        .manifest
        .agents
        .iter()
        .map(|manifest_agent| {
            let agent_policy = ApprovalPolicy::new(policy, manifest_agent.budget_usd_daily);
            let is_orchestrator = orchestrator.as_deref() == Some(manifest_agent.id.as_str());
            let grants = crate::runtime::builder::agent_effective_grants(
                &company.manifest.tools.allow,
                &manifest_agent.tools,
            );
            let agent = build::build_agent(
                &company.id,
                company_name,
                manifest_agent,
                agent_policy,
                deps,
                &grants,
                skill_deltas,
                is_orchestrator,
            )?;
            Ok(Arc::new(CompanyAgent {
                agent_id: manifest_agent.id.clone(),
                role: manifest_agent.role.clone(),
                agent: Mutex::new(agent),
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    #[cfg(feature = "mcp")]
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::company::CompanyManifest;
    use crate::harness::provider::MockProvider;
    use crate::ports::UsageSample;
    use crate::ports::types::{
        ChunkAddr, ChunkHit, ChunkMeta, CompanySummary, ContextChunk, LedgerEntry,
    };

    /// In-memory `ContextStore` so `OcMemory` has somewhere to land.
    #[derive(Default)]
    struct MockContext {
        chunks: StdMutex<Vec<(ChunkAddr, ContextChunk)>>,
    }

    #[async_trait]
    impl ContextStore for MockContext {
        async fn put(&self, _id: &CompanyId, chunk: ContextChunk) -> crate::Result<ChunkAddr> {
            let mut guard = self.chunks.lock().unwrap();
            let addr = ChunkAddr::new(format!("addr-{}", guard.len()));
            guard.push((addr.clone(), chunk));
            Ok(addr)
        }
        async fn list(&self, _id: &CompanyId, prefix: &str) -> crate::Result<Vec<ChunkMeta>> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .filter(|(_, c)| c.label.starts_with(prefix))
                .map(|(addr, c)| ChunkMeta {
                    addr: addr.clone(),
                    label: c.label.clone(),
                    len: c.body.len(),
                })
                .collect())
        }
        async fn peek(
            &self,
            _id: &CompanyId,
            addr: &ChunkAddr,
            _range: Option<std::ops::Range<usize>>,
        ) -> crate::Result<String> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .find(|(a, _)| a == addr)
                .map(|(_, c)| c.body.clone())
                .unwrap_or_default())
        }
        async fn search(
            &self,
            _id: &CompanyId,
            query: &str,
            limit: usize,
        ) -> crate::Result<Vec<ChunkHit>> {
            let guard = self.chunks.lock().unwrap();
            Ok(guard
                .iter()
                .filter(|(_, c)| c.body.contains(query))
                .take(limit)
                .map(|(addr, c)| ChunkHit {
                    addr: addr.clone(),
                    snippet: c.body.clone(),
                    score: 1.0,
                })
                .collect())
        }
    }

    /// `CompanyStore` that records what the cost hook appends.
    #[derive(Default)]
    struct RecordingStore {
        ledger: StdMutex<Vec<LedgerEntry>>,
    }

    #[async_trait]
    impl CompanyStore for RecordingStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(None)
        }
        async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, entry: LedgerEntry) -> crate::Result<()> {
            self.ledger.lock().unwrap().push(entry);
            Ok(())
        }
    }

    /// Records usage samples so a zero-usage turn can be asserted inert.
    #[derive(Default)]
    struct RecordingMeter {
        samples: StdMutex<Vec<UsageSample>>,
    }

    #[async_trait]
    impl UsageMeter for RecordingMeter {
        async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
            self.samples.lock().unwrap().push(sample.clone());
            Ok(())
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Ok(self.samples.lock().unwrap().clone())
        }
    }

    /// In-memory [`SkillStateStore`] so a test can author/enable/disable skills
    /// between `ensure` calls and assert the roster refreshes (or doesn't).
    #[derive(Default)]
    struct MockSkillStore {
        deltas: StdMutex<Vec<SkillState>>,
    }

    #[async_trait]
    impl SkillStateStore for MockSkillStore {
        async fn list(&self, _company: &CompanyId) -> crate::Result<Vec<SkillState>> {
            Ok(self.deltas.lock().unwrap().clone())
        }
        async fn set(&self, _company: &CompanyId, state: &SkillState) -> crate::Result<()> {
            let mut g = self.deltas.lock().unwrap();
            g.retain(|d| d.slug != state.slug);
            g.push(state.clone());
            Ok(())
        }
        async fn remove(&self, _company: &CompanyId, slug: &str) -> crate::Result<bool> {
            let mut g = self.deltas.lock().unwrap();
            let before = g.len();
            g.retain(|d| d.slug != slug);
            Ok(g.len() != before)
        }
    }

    /// A console-authored custom skill delta: a full `SKILL.md` carried inline in
    /// `custom_doc`, with `body_marker` embedded in the body so a `describe_workflow`
    /// assertion can prove the *content* (not just the slug) reached the agent.
    fn custom_skill_delta(
        slug: &str,
        name: &str,
        description: &str,
        body_marker: &str,
    ) -> SkillState {
        use crate::ports::skills_state::SkillSource;
        SkillState {
            slug: slug.to_string(),
            enabled: true,
            source: SkillSource::Custom,
            custom_doc: Some(format!(
                "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n{body_marker}\n"
            )),
        }
    }

    fn manifest() -> CompanyManifest {
        toml::from_str(
            r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction."

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds the product."
"#,
        )
        .expect("valid manifest")
    }

    fn record() -> CompanyRecord {
        CompanyRecord {
            id: CompanyId::new("acme"),
            manifest: manifest(),
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: Vec::new(),
        }
    }

    struct Fixture {
        deps: HarnessDeps,
        store: Arc<RecordingStore>,
        meter: Arc<RecordingMeter>,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(RecordingStore::default());
        let meter = Arc::new(RecordingMeter::default());
        Fixture {
            deps: HarnessDeps {
                provider: Arc::new(MockProvider::new("mock: ")),
                provider_slug: "mock".to_string(),
                context: Arc::new(MockContext::default()),
                store: store.clone(),
                meter: Some(meter.clone()),
                workspace_root: dir.path().to_path_buf(),
                model_override: None,
                tasks: None,
                skills: None,
                skills_source_dir: None,
                mcp_servers: Vec::new(),
                facts: None,
                events: None,
                delegations: DelegationQueue::default(),
            },
            store,
            meter,
            _dir: dir,
        }
    }

    #[cfg(feature = "mcp")]
    struct McpToolCallProvider {
        server_id: String,
        calls: AtomicUsize,
    }

    #[cfg(feature = "mcp")]
    #[async_trait]
    impl Provider for McpToolCallProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(format!(
                    "<tool_call>{{\"name\":\"mcp_registry_tool_call\",\"arguments\":{{\"server_id\":\"{}\",\"tool_name\":\"echo\",\"arguments\":{{\"text\":\"agent-mcp\"}}}}}}</tool_call>",
                    self.server_id
                ))
            } else {
                Ok(format!("__MOCK_LLM__ {message}"))
            }
        }
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn agent_executes_connected_mcp_tool() {
        use std::collections::HashMap;
        use std::process::Command;

        use oh::mcp_registry::types::{CommandKind, InstalledServer, Transport};

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipping MCP agent test because node is unavailable");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("agent-mcp-stub.cjs");
        std::fs::write(
            &script,
            r#"
const readline = require('node:readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (v) => process.stdout.write(JSON.stringify(v) + '\n');
rl.on('line', (line) => {
  const r = JSON.parse(line); if (!r.id) return;
  if (r.method === 'initialize') send({jsonrpc:'2.0',id:r.id,result:{protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'agent-test',version:'1'}}});
  else if (r.method === 'tools/list') send({jsonrpc:'2.0',id:r.id,result:{tools:[{name:'echo',description:'Echo text',inputSchema:{type:'object'}}]}});
  else if (r.method === 'tools/call') send({jsonrpc:'2.0',id:r.id,result:{content:[{type:'text',text:'echo: ' + r.params.arguments.text}]}});
});
"#,
        )
        .expect("write stub");

        let mcp = crate::harness::mcp::McpRuntime::new(dir.path().join("mcp"));
        let server = InstalledServer {
            server_id: uuid::Uuid::new_v4().to_string(),
            qualified_name: "agent-test".to_string(),
            display_name: "Agent Test".to_string(),
            description: None,
            icon_url: None,
            command_kind: CommandKind::Binary,
            command: "node".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env_keys: vec![],
            config: None,
            installed_at: 0,
            last_connected_at: None,
            transport: Transport::Stdio,
            enabled: true,
        };
        mcp.install(&server, &HashMap::new()).expect("install");
        mcp.connect(&server.server_id).await.expect("connect");

        let mut fx = fixture();
        fx.deps.provider = Arc::new(McpToolCallProvider {
            server_id: server.server_id.clone(),
            calls: AtomicUsize::new(0),
        });
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");
        let reply = pool
            .run(&rec.id, "ceo", "use the MCP echo tool", &fx.deps)
            .await
            .expect("agent turn");
        assert!(reply.contains("__MOCK_LLM__"), "{reply}");
        assert!(reply.contains("echo: agent-mcp"), "{reply}");

        mcp.disconnect(&server.server_id).await.expect("disconnect");
    }

    #[tokio::test]
    async fn roster_builds_every_manifest_agent() {
        let fx = fixture();
        let roster = build_roster(&record(), &fx.deps, &[]).expect("roster builds");
        let ids: Vec<_> = roster.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["ceo", "engineer"]);
        assert_eq!(roster[0].role, "Chief Executive");
    }

    /// The roster builds end-to-end with the skill read surface wired: the
    /// effective set materializes, the read tools build, and the catalogue folds
    /// into the persona — all without error — and the scratch tree lands under
    /// the agent's workspace root.
    #[tokio::test]
    async fn roster_builds_with_skill_surface_wired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = tempfile::tempdir().expect("source");
        let skill_dir = source.path().join("skills").join("web-research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Web Research\ndescription: Answer a question\n---\n\n# Web Research\n",
        )
        .unwrap();

        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: None,
            skills_source_dir: Some(source.path().to_path_buf()),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
        };

        let roster = build_roster(&record(), &deps, &[]).expect("roster builds with skills");
        assert_eq!(roster.len(), 2);
        // The scratch skill tree was materialized for the first roster agent.
        assert!(
            dir.path()
                .join("acme")
                .join("ceo")
                .join("skill-catalog")
                .join("skills")
                .join("web-research")
                .join("SKILL.md")
                .is_file(),
            "the effective skill bundle should be materialized under the agent workspace"
        );
    }

    #[tokio::test]
    async fn run_executes_a_turn_on_the_openhuman_runtime() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        let reply = pool
            .run(&rec.id, "ceo", "hello-marker", &fx.deps)
            .await
            .expect("turn runs");

        assert!(
            reply.contains("hello-marker"),
            "reply should echo the prompt through the agent: {reply:?}"
        );
    }

    #[tokio::test]
    async fn run_stores_outcomes_and_injects_them_into_later_turns() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        // Cold store: nothing to inject on the first turn.
        let first = pool
            .run(&rec.id, "ceo", "alpha task", &fx.deps)
            .await
            .expect("first turn");
        assert!(
            !first.contains("Relevant prior work"),
            "a cold turn injects nothing: {first:?}"
        );

        // The outcome was written back under the task-outcome prefix.
        let stored = fx
            .deps
            .context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1, "the first turn stores its outcome");

        // Second turn: the prior outcome (its body contains "alpha") is
        // retrieved and injected, so the agent sees the preamble.
        let second = pool
            .run(&rec.id, "ceo", "alpha", &fx.deps)
            .await
            .expect("second turn");
        assert!(
            second.contains("Relevant prior work"),
            "the second turn injects the retrieved outcome: {second:?}"
        );

        let stored = fx
            .deps
            .context
            .list(&rec.id, memory_loop::OUTCOME_LABEL_PREFIX)
            .await
            .unwrap();
        assert_eq!(stored.len(), 2, "the second turn stores its outcome too");
    }

    #[tokio::test]
    async fn ensure_is_idempotent() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("first ensure");
        pool.ensure(&rec, &fx.deps).await.expect("second ensure");
        assert_eq!(pool.resident_companies().await, 1);
    }

    // --- Skill freshness (issue #41) ---------------------------------------

    /// Deps wired to an in-memory [`MockSkillStore`] (no company-dir source, so
    /// only operator deltas drive the effective set) plus the store handle and
    /// the workspace tempdir so a test can author skills and read back the
    /// per-agent materialized catalogue.
    fn skill_fixture() -> (HarnessDeps, Arc<MockSkillStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = Arc::new(MockSkillStore::default());
        let deps = HarnessDeps {
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            context: Arc::new(MockContext::default()),
            store: Arc::new(RecordingStore::default()),
            meter: None,
            workspace_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            skills: Some(skills.clone()),
            skills_source_dir: None,
            mcp_servers: Vec::new(),
        };
        (deps, skills, dir)
    }

    /// The first cached agent for `agent_id`, cloned out of the pool so two
    /// snapshots across `ensure` calls can be compared with [`Arc::ptr_eq`].
    async fn cached_agent(pool: &HarnessPool, id: &CompanyId, agent_id: &str) -> Arc<CompanyAgent> {
        let guard = pool.rosters.read().await;
        guard
            .get(id)
            .expect("roster resident")
            .agents
            .iter()
            .find(|a| a.agent_id == agent_id)
            .expect("agent on roster")
            .clone()
    }

    /// A skill the operator authors in the console *after* the roster is already
    /// live must reach the agent on the next `ensure` — with its BODY, not just
    /// its name. This is the core #41 regression: the pool used to cache the
    /// roster on the first build and never re-read the deltas.
    #[tokio::test]
    async fn late_authored_skill_reaches_agent_on_next_ensure() {
        use oh::config::Config;
        use oh::skills::tools::WorkflowDescribeTool;
        use oh::tools::Tool;
        use serde_json::json;

        let (deps, skills, dir) = skill_fixture();
        let pool = HarnessPool::new();
        let rec = record();

        // First cycle: roster built with no skills yet.
        pool.ensure(&rec, &deps).await.expect("first ensure");

        // Operator authors a custom skill AFTER the roster exists.
        skills
            .set(
                &rec.id,
                &custom_skill_delta(
                    "invoicing",
                    "Invoicing",
                    "Draft an invoice for a client",
                    "STEP-MARKER-total-due",
                ),
            )
            .await
            .unwrap();

        // Next cycle rebuilds the roster from the moved deltas.
        pool.ensure(&rec, &deps).await.expect("second ensure");

        // `describe_workflow` over the responder's materialized catalogue returns
        // the skill's inline body — proving the CONTENT reached the agent, not
        // merely that a same-named entry exists.
        let catalog = dir.path().join("acme").join("ceo").join("skill-catalog");
        let config = Arc::new(Config {
            workspace_dir: catalog,
            ..Default::default()
        });
        let tool = WorkflowDescribeTool::new(config);
        let out = tool
            .execute(json!({ "workflow_id": "invoicing" }))
            .await
            .expect("describe the fresh skill");
        let text = out.output_for_llm(false);
        assert!(
            text.contains("STEP-MARKER-total-due"),
            "the skill body must reach the agent: {text}"
        );
        assert!(
            text.contains("Draft an invoice for a client"),
            "the skill description must surface: {text}"
        );
    }

    /// When the deltas have not moved, a second `ensure` must NOT rebuild — the
    /// same `Arc<CompanyAgent>` is handed back, so live conversation state is
    /// preserved and no work is wasted.
    #[tokio::test]
    async fn unchanged_deltas_reuse_the_same_roster() {
        let (deps, skills, _dir) = skill_fixture();
        let pool = HarnessPool::new();
        let rec = record();

        skills
            .set(
                &rec.id,
                &custom_skill_delta("invoicing", "Invoicing", "Draft an invoice", "BODY-A"),
            )
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = cached_agent(&pool, &rec.id, "ceo").await;

        // No delta change between the two calls.
        pool.ensure(&rec, &deps).await.expect("second ensure");
        let after = cached_agent(&pool, &rec.id, "ceo").await;

        assert!(
            Arc::ptr_eq(&before, &after),
            "an unchanged skill set must not rebuild the roster"
        );
    }

    /// Disabling a skill drops it from the effective set on the next `ensure`:
    /// the roster rebuilds (a new agent instance) and the skill is gone from the
    /// materialized catalogue.
    #[tokio::test]
    async fn disabling_a_skill_drops_it_on_next_ensure() {
        use crate::ports::skills_state::{SkillSource, SkillState};

        let (deps, skills, dir) = skill_fixture();
        let pool = HarnessPool::new();
        let rec = record();

        skills
            .set(
                &rec.id,
                &custom_skill_delta("invoicing", "Invoicing", "Draft an invoice", "BODY-A"),
            )
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("first ensure");
        let before = cached_agent(&pool, &rec.id, "ceo").await;
        let materialized = dir
            .path()
            .join("acme")
            .join("ceo")
            .join("skill-catalog")
            .join("skills")
            .join("invoicing");
        assert!(materialized.is_dir(), "skill materialized on first build");

        // Operator disables the skill (a custom skill, disabled, drops entirely).
        skills
            .set(
                &rec.id,
                &SkillState {
                    slug: "invoicing".to_string(),
                    enabled: false,
                    source: SkillSource::Custom,
                    custom_doc: None,
                },
            )
            .await
            .unwrap();
        pool.ensure(&rec, &deps).await.expect("second ensure");
        let after = cached_agent(&pool, &rec.id, "ceo").await;

        assert!(
            !Arc::ptr_eq(&before, &after),
            "a moved skill set must rebuild the roster"
        );
        assert!(
            !materialized.exists(),
            "the disabled skill must be gone from the materialized catalogue"
        );
    }

    #[tokio::test]
    async fn turns_are_serialised_and_history_survives() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        pool.run(&rec.id, "ceo", "first", &fx.deps)
            .await
            .expect("first turn");
        let second = pool
            .run(&rec.id, "ceo", "second", &fx.deps)
            .await
            .expect("second turn");

        assert!(second.contains("second"));
    }

    #[tokio::test]
    async fn unknown_agent_is_invalid_request() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");

        let err = pool
            .run(&rec.id, "nobody", "hi", &fx.deps)
            .await
            .expect_err("unknown agent rejected");
        assert!(
            matches!(err, OpenCompanyError::InvalidRequest(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unknown_company_is_not_found() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let err = pool
            .run(&CompanyId::new("ghost"), "ceo", "hi", &fx.deps)
            .await
            .expect_err("unknown company rejected");
        assert!(
            matches!(err, OpenCompanyError::CompanyNotFound(_)),
            "{err:?}"
        );
    }

    /// Pins the documented inert-metering contract: until the provider reports
    /// usage, a turn writes neither a ledger entry nor a usage sample.
    #[tokio::test]
    async fn zero_usage_turn_writes_nothing() {
        let fx = fixture();
        let pool = HarnessPool::new();
        let rec = record();
        pool.ensure(&rec, &fx.deps).await.expect("ensure");
        pool.run(&rec.id, "ceo", "hi", &fx.deps)
            .await
            .expect("turn");

        assert!(fx.store.ledger.lock().unwrap().is_empty());
        assert!(fx.meter.samples.lock().unwrap().is_empty());
    }
}
