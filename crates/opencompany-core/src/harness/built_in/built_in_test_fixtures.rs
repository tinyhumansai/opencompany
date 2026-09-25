//! Shared fixtures for `built_in`'s own inline tests (part 1 of 2):
//! mock stores, scripted providers, and the `Fixture`/`manifest`/`record`
//! builders test groups reach for. Split out of the inline `mod tests` block,
//! and split further across files because the combined fixtures exceeded the
//! 750-line file limit.

use super::*;
pub(super) use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
pub(super) use tinyinference::model::{ChatModel, ModelRequest, ModelResponse};

pub(super) use crate::company::CompanyManifest;
pub(super) use crate::harness::provider::MockProvider;
pub(super) use crate::ports::UsageSample;
pub(super) use crate::ports::types::{
    ChunkAddr, ChunkHit, ChunkMeta, CompanySummary, ContextChunk, LedgerEntry,
};
// The two-level resolver. Test-only now: the roster build goes through
// `agent_scoped_grants`, and these tests assert the desk-less case still
// resolves identically to what shipped before desks could scope tools.
pub(super) use crate::runtime::builder::agent_effective_grants;

pub(super) fn fp_entry_full(
    mode: Option<&str>,
    always: Option<Vec<&str>>,
    cap: Option<Option<f64>>,
    ttl: Option<u64>,
) -> PolicyOverride {
    use crate::ports::types::{Actor, ActorKind};
    PolicyOverride {
        mode: mode.map(str::to_string),
        always_approve: always.map(|v| v.into_iter().map(str::to_string).collect()),
        auto_approve_under_usd: cap,
        approval_ttl_hours: ttl,
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-1".to_string(),
        },
        at_millis: 1_700_000_000_000,
    }
}

/// An effective `[policy]` block for fingerprint tests — what the roster is
/// actually built from (`CompanyRecord::effective_policy`).
pub(super) fn fp_policy(mode: &str, always: &[&str], cap: Option<f64>, ttl: Option<u64>) -> Policy {
    Policy {
        mode: mode.to_string(),
        always_approve: always.iter().map(|k| (*k).to_string()).collect(),
        auto_approve_under_usd: cap,
        approval_ttl_hours: ttl,
    }
}

/// In-memory `ContextStore` so `OcMemory` has somewhere to land.
#[derive(Default)]
pub(super) struct MockContext {
    pub(super) chunks: StdMutex<Vec<(ChunkAddr, ContextChunk)>>,
    // Monotonic, NOT chunks.len(): a delete shrinks the vec, and a
    // len-derived addr would then collide with a surviving chunk's.
    pub(super) next_addr: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ContextStore for MockContext {
    async fn put(&self, _id: &CompanyId, chunk: ContextChunk) -> crate::Result<ChunkAddr> {
        let n = self
            .next_addr
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut guard = self.chunks.lock().unwrap();
        let addr = ChunkAddr::new(format!("addr-{n}"));
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
                // The mock does not model store time; these tests exercise
                // the harness, not the Brain's freshness stat.
                stored_at_millis: 0,
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
    async fn delete(&self, _id: &CompanyId, addr: &ChunkAddr) -> crate::Result<bool> {
        let mut guard = self.chunks.lock().unwrap();
        let before = guard.len();
        guard.retain(|(a, _)| a != addr);
        Ok(guard.len() < before)
    }
    async fn delete_label(
        &self,
        _id: &CompanyId,
        addr: &ChunkAddr,
        label: &str,
    ) -> crate::Result<bool> {
        let mut guard = self.chunks.lock().unwrap();
        let before = guard.len();
        guard.retain(|(a, c)| !(a == addr && c.label == label));
        Ok(guard.len() < before)
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
pub(super) struct RecordingStore {
    pub(super) ledger: StdMutex<Vec<LedgerEntry>>,
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

/// `CompanyStore` whose `append_ledger` always fails — the "ledger write
/// that also failed" `turn_result_after_metering`'s own doc names, and
/// the fixture `a_metering_failure_does_not_swallow_a_budget_pause_marker`
/// needs to force `meter_turn_costs` into its `Err` arm on a turn that
/// otherwise succeeded (Codex review, PR #2053).
#[derive(Default)]
pub(super) struct FailingLedgerStore;

#[async_trait]
impl CompanyStore for FailingLedgerStore {
    async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        Ok(None)
    }
    async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
        Ok(())
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        Ok(Vec::new())
    }
    async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
        Err(OpenCompanyError::Harness(
            "scripted ledger outage".to_string(),
        ))
    }
}

/// Records usage samples so a zero-usage turn can be asserted inert.
#[derive(Default)]
pub(super) struct RecordingMeter {
    pub(super) samples: StdMutex<Vec<UsageSample>>,
}

#[async_trait]
impl UsageMeter for RecordingMeter {
    async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
        self.samples.lock().unwrap().push(sample.clone());
        Ok(())
    }
    /// Honours `since_millis`, per the port contract ("every sample at or
    /// after `since_millis`"). The per-agent daily cap (issue #304) is a
    /// windowed read, so a double that returned everything regardless would
    /// make the day-rollover test pass against any boundary the code
    /// computed — including none at all.
    async fn query(&self, _company: &CompanyId, since: u64) -> crate::Result<Vec<UsageSample>> {
        Ok(self
            .samples
            .lock()
            .unwrap()
            .iter()
            .filter(|sample| sample.at_millis >= since)
            .cloned()
            .collect())
    }
}

/// A meter whose reads always fail — for the dispatch gate's fail-open pin.
pub(super) struct FailingMeter;

#[async_trait]
impl UsageMeter for FailingMeter {
    async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> crate::Result<()> {
        Ok(())
    }
    async fn query(&self, _company: &CompanyId, _since: u64) -> crate::Result<Vec<UsageSample>> {
        Err(OpenCompanyError::Store("meter unavailable".into()))
    }
}

pub(super) fn manifest() -> CompanyManifest {
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

pub(super) fn record() -> CompanyRecord {
    CompanyRecord {
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        id: CompanyId::new("acme"),
        manifest: manifest(),
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        setup: None,
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
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
    }
}

pub(super) struct Fixture {
    pub(super) deps: HarnessDeps,
    pub(super) store: Arc<RecordingStore>,
    pub(super) meter: Arc<RecordingMeter>,
    pub(super) _dir: tempfile::TempDir,
}

pub(super) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(RecordingStore::default());
    let meter = Arc::new(RecordingMeter::default());
    Fixture {
        deps: HarnessDeps {
            takeovers: Default::default(),
            emergency_gate: None,
            notifications: None,
            ledgers: None,
            ledger_registry: Default::default(),
            provider: Arc::new(MockProvider::new("mock: ")),
            provider_slug: "mock".to_string(),
            serves: None,
            context: Arc::new(MockContext::default()),
            store: store.clone(),
            meter: Some(meter.clone()),
            workspace_root: dir.path().to_path_buf(),
            mcp_home: None,
            workspace_git_enabled: false,
            audit_root: dir.path().to_path_buf(),
            model_override: None,
            tasks: None,
            artifacts: None,
            skills: None,
            skills_source_dir: None,
            skills_registry: std::sync::Arc::from([]),
            default_mcp_servers: Vec::new(),
            mcp_servers: Vec::new(),
            facts: None,
            events: None,
            delegations: DelegationQueue::default(),
            workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
            mcp_failures: McpFailureQueue::default(),
            pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
            workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
            run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
            run_output_store: None,
            workflow_runs: None,
            deep_trace: None,
            workflow_revisions: None,
            approval_requests: ApprovalRequestQueue::default(),
            approval_parker: None,
            secrets: None,
            web_allowed_domains: Vec::new(),
            capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
            workflow_source_dir: None,
            plan: None,
            media: None,
            composio: None,
            #[cfg(feature = "chargebee")]
            chargebee: None,
            #[cfg(feature = "paypal")]
            paypal: None,
            hosting: None,
            steer: crate::company::steer::InflightRegistry::default(),
            run_supervisor: crate::runtime::RunSupervisor::default(),
            delivery: None,
            search: None,
            tenant_search: None,
            workspace: None,
        },
        store,
        meter,
        _dir: dir,
    }
}

/// A provider double with its own, distinct telemetry identity — stands
/// in for a pinned agent's own `TenantProvider` sibling
/// ([`HarnessModel::pinned`]) without going through real pin resolution.
pub(super) struct PinnedProvider;

#[async_trait]
impl ChatModel<()> for PinnedProvider {
    async fn invoke(
        &self,
        _state: &(),
        _request: ModelRequest,
    ) -> tinyinference::Result<ModelResponse> {
        Ok(ModelResponse::assistant("ok"))
    }
}

impl HarnessModel for PinnedProvider {
    fn telemetry_provider_id(&self) -> String {
        "pinned-provider".to_string()
    }

    fn telemetry_model(&self) -> Option<crate::metering::ModelSlug> {
        Some(crate::metering::ModelSlug::OTHER)
    }
}

/// A provider double whose every call fails — stands in for a company
/// with no default configured at all (X12's "pinned-only" case), so a
/// caller of [`pass_model`] can be proven to fall back to the agent's own
/// pin instead of losing the pass outright.
pub(super) struct AlwaysFailsProvider;

#[async_trait]
impl ChatModel<()> for AlwaysFailsProvider {
    async fn invoke(
        &self,
        _state: &(),
        _request: ModelRequest,
    ) -> tinyinference::Result<ModelResponse> {
        Err(tinyinference::Error::Model(
            "no company default configured".to_string(),
        ))
    }
}

impl HarnessModel for AlwaysFailsProvider {
    fn telemetry_provider_id(&self) -> String {
        "no-default".to_string()
    }
}

/// One request the scripted provider observed, for tests that assert on what
/// each individual provider call carried (issue #1871).
#[derive(Clone, Debug)]
pub(super) struct CapturedCall {
    /// Tool declaration names sent with the call, in wire order.
    pub(super) tools: Vec<String>,
    /// The request's message kinds, in wire order.
    pub(super) roles: Vec<CapturedRole>,
}

impl CapturedCall {
    /// How many messages of one kind the call carried.
    pub(super) fn count(&self, role: CapturedRole) -> usize {
        self.roles.iter().filter(|seen| **seen == role).count()
    }
}

/// The provider-visible kind of one request message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CapturedRole {
    /// A system/developer instruction row.
    System,
    /// A user input row.
    User,
    /// Assistant prose carrying no tool calls.
    Assistant,
    /// An assistant message carrying native tool calls.
    AssistantToolCalls,
    /// A tool-result row.
    ToolResult,
    /// A host out-of-band record, if one ever reaches the wire.
    Custom,
}

/// A model that plays back a scripted sequence of outcomes, one per
/// [`invoke`](ChatModel::invoke) call, so the empty-response retry wrapper can
/// be driven deterministically. `Ok("")` is the transient empty class (the
/// harness turn raises the empty-response error on a blank assistant reply);
/// `Err(_)` is a hard error.
pub(super) struct ScriptedProvider {
    pub(super) script: StdMutex<std::collections::VecDeque<Result<String, String>>>,
    pub(super) calls: std::sync::atomic::AtomicUsize,
    /// Usage stamped on every scripted `Ok` response, when the case needs a
    /// provider that reports any (`None` — the default — mirrors
    /// `MockProvider`, whose replies carry none at all). Only a response
    /// carrying usage makes openhuman publish the live
    /// `TurnCostUpdated` tally the metering path depends on.
    pub(super) usage: Option<tinyinference::Usage>,
    /// What an exhausted script answers: the default `"exhausted"` reply,
    /// or a permanent error.
    ///
    /// A case that needs the turn to *fail* has to script a provider that
    /// stays failed, because openhuman retries a provider error inside its
    /// own loop — a finite run of `Err` entries is simply consumed and the
    /// turn then succeeds on the fallback reply, which is how this
    /// scripting seam quietly turned a failure case into a passing one.
    pub(super) fail_when_exhausted: bool,
    /// Per-call snapshot of what each individual provider call actually
    /// carried — without this a test could only check the final history,
    /// which merges both attempts into one view and hides per-attempt
    /// differences (issue #1871).
    pub(super) captured: StdMutex<Vec<CapturedCall>>,
}

impl ScriptedProvider {
    pub(super) fn new(outcomes: Vec<Result<String, String>>) -> Self {
        Self {
            script: StdMutex::new(outcomes.into_iter().collect()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            usage: None,
            fail_when_exhausted: false,
            captured: StdMutex::new(Vec::new()),
        }
    }

    /// Report `usage` on every scripted `Ok` response.
    pub(super) fn reporting_usage(mut self, usage: tinyinference::Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Fail every call past the end of the script, permanently.
    pub(super) fn failing_when_exhausted(mut self) -> Self {
        self.fail_when_exhausted = true;
        self
    }
}

#[async_trait]
impl ChatModel<()> for ScriptedProvider {
    async fn invoke(
        &self,
        _state: &(),
        _request: ModelRequest,
    ) -> tinyinference::Result<ModelResponse> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Record the call's tool count and per-message kinds, in wire order,
        // for tests that need to inspect what each individual provider call
        // actually saw.
        let roles = _request
            .messages
            .iter()
            .map(|message| match message {
                tinyinference::Message::System(_) => CapturedRole::System,
                tinyinference::Message::User(_) => CapturedRole::User,
                tinyinference::Message::Assistant(assistant) if assistant.tool_calls.is_empty() => {
                    CapturedRole::Assistant
                }
                tinyinference::Message::Assistant(_) => CapturedRole::AssistantToolCalls,
                tinyinference::Message::Tool(_) => CapturedRole::ToolResult,
                tinyinference::Message::Custom(_) => CapturedRole::Custom,
            })
            .collect();
        self.captured.lock().unwrap().push(CapturedCall {
            tools: _request
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            roles,
        });
        let with_usage = |reply: &str| {
            let mut response = ModelResponse::assistant(reply);
            response.usage = self.usage;
            response
        };
        match self.script.lock().unwrap().pop_front() {
            Some(Ok(reply)) => Ok(with_usage(&reply)),
            Some(Err(err)) => Err(tinyinference::Error::Model(err)),
            None if self.fail_when_exhausted => Err(tinyinference::Error::Model(
                "scripted provider is permanently down".to_string(),
            )),
            None => Ok(with_usage("exhausted")),
        }
    }
}

impl HarnessModel for ScriptedProvider {
    fn telemetry_provider_id(&self) -> String {
        "scripted".to_string()
    }
}

/// The process-wide OpenHuman runtime, for a fixture that builds a roster
/// synchronously (plan hive-desks, Phase 2). Ephemeral workspace, no key.
pub(super) fn test_runtime() -> Arc<openhuman_embed::Runtime> {
    crate::harness::openhuman_runtime::global_blocking(
        crate::harness::openhuman_runtime::RuntimeBoot::ephemeral(),
    )
    .expect("the OpenHuman runtime boots")
}
