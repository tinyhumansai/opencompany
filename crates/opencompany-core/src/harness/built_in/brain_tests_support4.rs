use super::*;
use crate::company::steer::{InflightKind, InflightRegistry};
use crate::ports::TaskStore;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use tinyinference::message::Message;
use tinyinference::model::{ChatModel, ModelRequest, ModelResponse};

/// A `FixedOutcomeTurn` whose single turn reports a budget pause — the
/// account itself is out of inference credits, so `outcome.reply` is
/// host-authored pause copy, not an answer.
pub(super) fn budget_paused_outcome(agent: &str) -> crate::harness::built_in::TurnOutcome {
    crate::harness::built_in::TurnOutcome {
        reply: BUDGET_PAUSED_PLACEHOLDER_REPLY.to_string(),
        steps: Vec::new(),
        hit_iteration_cap: false,
        abnormal_stop: None,
        halted_for_spend: None,
        budget_paused: Some(crate::harness::BudgetPause {
            agent: agent.to_string(),
            summary: "add credits and try again".to_string(),
        }),
    }
}

/// A `FixedOutcomeTurn` whose single turn halted for spend — the
/// teammate's own declared cap was reached mid-turn.
pub(super) fn spend_halted_outcome(agent: &str) -> crate::harness::built_in::TurnOutcome {
    crate::harness::built_in::TurnOutcome {
        reply: "partial answer before the brake fired".to_string(),
        steps: Vec::new(),
        hit_iteration_cap: false,
        abnormal_stop: None,
        halted_for_spend: Some(crate::harness::SpendHalt {
            agent: agent.to_string(),
            spent_usd: 5.5,
            cap_usd: 5.0,
        }),
        budget_paused: None,
    }
}

/// A `FixedOutcomeTurn` whose single turn is a pre-dispatch spend
/// refusal — the meter that a declared cap needs could not be read, so
/// no model call ran and `outcome.reply` is host-authored refusal copy.
pub(super) fn abnormal_stop_outcome(reply: &str) -> crate::harness::built_in::TurnOutcome {
    crate::harness::built_in::TurnOutcome {
        reply: reply.to_string(),
        steps: Vec::new(),
        hit_iteration_cap: false,
        abnormal_stop: Some("[stopped: dispatch refused]".to_string()),
        halted_for_spend: None,
        budget_paused: None,
    }
}

/// A bare brain over a fresh temp-dir store, for tests that only need
/// `HiveDeskRunner`'s `brain`/`host` fields satisfied and are not
/// exercising the approval-parking path itself.
pub(super) fn hive_test_brain(dir: &std::path::Path) -> HarnessBrain {
    brain_with_approval_queue(dir, crate::harness::policy::ApprovalRequestQueue::default())
}

pub(super) fn hive_desk_runner<'a>(
    brain: &'a HarnessBrain,
    host: &'a dyn CycleHost,
    outcome: crate::harness::built_in::TurnOutcome,
) -> HiveDeskRunner<'a> {
    HiveDeskRunner {
        run_turn: Arc::new(FixedOutcomeTurn {
            outcome,
            approval_requests: None,
        }),
        company: CompanyId::new("acme"),
        chat_id: Some("lab".to_string()),
        thread_root: None,
        trigger_seq: None,
        brain,
        host,
    }
}

/// A brain whose provider steers the dispatched card `key` with `actions`
/// (one per turn). Returns the brain + its task store so a test can seed the
/// card and read the disposition back.
pub(super) fn brain_that_steers_itself(
    dir: &std::path::Path,
    key: &str,
    actions: Vec<SteerAction>,
) -> (HarnessBrain, Arc<FsOps>, Arc<SteeringProvider>) {
    let steer = InflightRegistry::new();
    let tasks = Arc::new(FsOps::new(dir));
    let provider = Arc::new(SteeringProvider {
        steer: steer.clone(),
        company: CompanyId::new("acme"),
        key: key.to_string(),
        actions: StdMutex::new(actions.into_iter().collect()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let deps = HarnessDeps {
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: provider.clone(),
        provider_slug: "steering".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: None,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: None,
        tasks: Some(tasks.clone()),
        // Same handle as `tasks` (FsOps is both stores), so a steered run's
        // artifact side effect — or the absence of one — is observable.
        artifacts: Some(tasks.clone()),
        skills: None,
        skills_source_dir: None,
        skills_registry: std::sync::Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: orchestrator::DelegationQueue::default(),
        workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
        mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
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
        steer,
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    };
    (
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record()),
        tasks,
        provider,
    )
}

// --- CEO-relay hand-back (delegate_to_desk second turn) ------------------

/// A provider that simulates the orchestrator queuing a `delegate_to_desk`
/// on its turns: on each invoke it pops the next scripted delegation (if any)
/// onto the shared queue — exactly what the real tool call does — then echoes
/// the last user message so a test can read the turn's reply. Sharing the
/// queue handle with [`HarnessDeps::delegations`] is what lets the brain
/// drain it after the turn.
/// Whether this request is a triage escalation rather than an agent turn
/// (issue #678).
///
/// Keyed on the system prompt's opening sentence, which
/// `harness::triage::system_prompt` owns. Coupling a fixture to prose is
/// ordinarily a smell; here the alternative is worse, because the only
/// other thing distinguishing the two is "carries no tools", and a turn
/// whose agent happens to have an empty belt would be misread as a
/// classification. Pinned by `a_triage_request_is_recognised_as_one`.
pub(super) fn is_triage_request(request: &ModelRequest) -> bool {
    request
        .messages
        .first()
        .map(|m| m.text().contains("You classify one message"))
        .unwrap_or(false)
}

/// Whether this request is a card-*titling* call rather than an agent turn.
///
/// The same hazard [`is_triage_request`] documents, one workload over. Naming
/// a card rides the same `HarnessModel` handle the roster runs on, so a
/// fixture that scripts "turn N queues this delegation" has its script shifted
/// by every title a turn happens to mint — the delegation written for the
/// relay gets staged by the desk lead instead, one level down, where a nested
/// drain runs hand-offs by design. That read as the relay re-delegating when
/// the relay had done nothing of the kind.
///
/// Keyed on the system prompt's opening sentence, which
/// [`title::system_prompt`](super::title) owns, for the reason
/// [`is_triage_request`] gives. Pinned by
/// `a_titling_request_is_recognised_as_one`.
pub(super) fn is_titling_request(request: &ModelRequest) -> bool {
    request
        .messages
        .first()
        .map(|m| m.text().contains("You name tasks"))
        .unwrap_or(false)
}

/// A provider for the selection rung (issue #1835): a request opening with
/// the selector's own system prompt gets the scripted reply; anything else
/// echoes. Keyed on the prompt's opening sentence for the reason
/// [`is_triage_request`] documents, and pinned the same way below.
pub(super) struct SelectingProvider {
    pub(super) reply: String,
    pub(super) selector_calls: std::sync::atomic::AtomicUsize,
}

pub(super) fn is_selection_request(request: &ModelRequest) -> bool {
    request
        .messages
        .first()
        .map(|m| {
            m.text()
                .contains("You route one message in a group channel")
        })
        .unwrap_or(false)
}

#[async_trait]
impl ChatModel<()> for SelectingProvider {
    async fn invoke(
        &self,
        _state: &(),
        request: ModelRequest,
    ) -> tinyinference::Result<ModelResponse> {
        if is_selection_request(&request) {
            self.selector_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(ModelResponse::assistant(self.reply.clone()));
        }
        let message = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m, Message::User(_)))
            .map(|m| m.text())
            .unwrap_or_default();
        Ok(ModelResponse::assistant(format!("mock: {message}")))
    }
}

impl HarnessModel for SelectingProvider {
    fn telemetry_provider_id(&self) -> String {
        "selecting".to_string()
    }
}

/// A brain whose provider answers every selection request with `reply`.
/// The record is [`record_with_desk`] — `engineer` + `chief`, a lead
/// `eng_desk` — plus an `auto` overlay channel `launch` holding both.
pub(super) fn brain_that_selects(
    dir: &std::path::Path,
    reply: &str,
) -> (HarnessBrain, Arc<SelectingProvider>) {
    brain_that_selects_with(dir, reply, None, None)
}

/// [`brain_that_selects`], plus an optional plan and usage meter, so a
/// test can place the company past its plan-level total-token ceiling.
pub(super) fn brain_that_selects_with(
    dir: &std::path::Path,
    reply: &str,
    plan: Option<crate::harness::capability_budget::CapabilityPlan>,
    meter: Option<Arc<dyn crate::ports::usage::UsageMeter>>,
) -> (HarnessBrain, Arc<SelectingProvider>) {
    let provider = Arc::new(SelectingProvider {
        reply: reply.to_string(),
        selector_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let deps = HarnessDeps {
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: provider.clone(),
        provider_slug: "selecting".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
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
        delegations: orchestrator::DelegationQueue::default(),
        workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
        mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
        workflow_source_dir: None,
        plan,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        steer: InflightRegistry::new(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    };
    let mut record = record_with_desk();
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "launch".to_string(),
        name: "Launch week".to_string(),
        description: None,
        members: vec!["engineer".to_string(), "chief".to_string()],
        responder: crate::ports::types::ResponderMode::Auto,
        hive: Default::default(),
    });
    (
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record),
        provider,
    )
}

/// A meter whose every query reports spend past any ceiling a test sets.
pub(super) struct SpentMeter;

#[async_trait]
impl crate::ports::usage::UsageMeter for SpentMeter {
    async fn record(
        &self,
        _company: &CompanyId,
        _sample: &crate::ports::usage::UsageSample,
    ) -> crate::Result<()> {
        Ok(())
    }
    async fn query(
        &self,
        _company: &CompanyId,
        _since: u64,
    ) -> crate::Result<Vec<crate::ports::usage::UsageSample>> {
        Ok(vec![crate::ports::usage::UsageSample {
            at_millis: 0,
            agent: "someone".to_string(),
            provider: "test".to_string(),
            input_tokens: 10_000,
            output_tokens: 0,
            cached_input_tokens: 0,
            cost_usd: 0.0,
            kind: crate::ports::usage::SampleKind::Inference,
            run_id: None,
            model: None,
        }])
    }
}

pub(super) struct DelegatingProvider {
    pub(super) queue: orchestrator::DelegationQueue,
    pub(super) pushes: StdMutex<VecDeque<Vec<Delegation>>>,
    pub(super) calls: std::sync::atomic::AtomicUsize,
    /// The same task store the brain writes through, so each invoke can
    /// snapshot the board **as that turn sees it** — the only way a test can
    /// observe a dispatched card mid-run (issue #204).
    pub(super) tasks: Arc<FsOps>,
    /// `(column, assignee)` of the company's card at each invoke, in order.
    pub(super) board: StdMutex<Vec<(String, String)>>,
    /// How this provider misbehaves, by invoke number.
    pub(super) faults: TurnFaults,
    /// The same registry wired into [`HarnessDeps::steer`], so a scripted
    /// invoke can cancel its own in-flight delegation.
    pub(super) steer: InflightRegistry,
}

/// How a [`DelegatingProvider`] misbehaves, keyed by 1-based invoke number
/// (issue #213 review).
#[derive(Default)]
pub(super) struct TurnFaults {
    /// Every invoke from here on ERRORS instead of answering, so a test can
    /// make a delegate's own run fail. A *from*, not an *on*: openhuman's
    /// agent loop retries a failed provider call within the same turn, so
    /// failing a single invoke only makes the turn succeed on its retry.
    pub(super) fail_from: Option<usize>,
    /// Invokes that CANCEL their own in-flight delegation mid-run, so the
    /// delegated reply is discarded exactly as an operator cancel does.
    pub(super) cancel_on: Vec<usize>,
    /// Desk keys the first turn's `delegate_to_desk` calls named and the
    /// tool REFUSED (issue #272). A refusal never becomes a `Delegation`,
    /// so this is how a test reproduces one without standing up the tool.
    pub(super) refused_on_first: Vec<String>,
}

impl DelegatingProvider {
    /// The board snapshot each turn ran against, in invoke order.
    pub(super) fn board(&self) -> Vec<(String, String)> {
        self.board.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatModel<()> for DelegatingProvider {
    async fn invoke(
        &self,
        _state: &(),
        request: ModelRequest,
    ) -> tinyinference::Result<ModelResponse> {
        // Issue #678: a triage escalation is a classification, not a turn.
        // It rides the same `HarnessModel` handle the roster runs on, so
        // without this it would consume a scripted push and shift every
        // turn's script by one — the delegation the test wrote for turn 1
        // would be staged by a call that is not turn 1.
        //
        // Answering `chatter` rather than declining keeps these fixtures on
        // the ungated path they were written for: only an `answer` verdict
        // narrows the claim, so `chatter` leaves the gate exactly where the
        // abstention left it. A test that wants the narrowing drives it
        // through `DelegationRunner::with_triage` directly, where the
        // verdict is scripted.
        if is_triage_request(&request) {
            return Ok(ModelResponse::assistant("chatter".to_string()));
        }
        // A title is not a turn either — see `is_titling_request`. Answered
        // rather than declined so the card still gets a headline: a refusal
        // here would fall back to the truncating path and change what these
        // fixtures assert about card titles.
        if is_titling_request(&request) {
            return Ok(ModelResponse::assistant("Handle the request".to_string()));
        }
        let invoke = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if self.faults.fail_from.is_some_and(|from| invoke >= from) {
            return Err(tinyinference::Error::Model(
                "the delegate's provider fell over".to_string(),
            ));
        }
        if self.faults.cancel_on.contains(&invoke) {
            // Cancel the entry this path actually registered.
            //
            // A CHAT-turn delegation still runs inside its delegator's
            // turn, so it has its own `Delegation` entry and that is the
            // one to cancel — cancelling the card there would end the whole
            // run. A DISPATCHED card's hand-off no longer works that way:
            // since the async hand-off the delegate runs as its own
            // dispatch, so the only entry in flight is the card's `Task`,
            // and it IS the delegate's run. Preferring `Delegation` keeps
            // the chat path targeting exactly what it did before.
            let company = CompanyId::new("acme");
            let entries = self.steer.list(&company);
            let target = entries
                .iter()
                .find(|e| e.kind == InflightKind::Delegation)
                .or_else(|| entries.iter().find(|e| e.kind == InflightKind::Task))
                .cloned();
            if let Some(entry) = target {
                let _ = self.steer.steer(&company, &entry.key, SteerAction::Cancel);
            }
        }
        let snapshot = self
            .tasks
            .list(&CompanyId::new("acme"))
            .await
            .ok()
            .and_then(|cards| cards.into_iter().next())
            .map(|card| (card.column, card.assignee))
            .unwrap_or_default();
        self.board.lock().unwrap().push(snapshot);
        for delegation in self.pushes.lock().unwrap().pop_front().unwrap_or_default() {
            self.queue.push(delegation);
        }
        if invoke == 1 {
            for desk in &self.faults.refused_on_first {
                self.queue.push_refusal(desk.clone());
            }
        }
        let message = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m, Message::User(_)))
            .map(|m| m.text())
            .unwrap_or_default();
        Ok(ModelResponse::assistant(format!("did: {message}")))
    }
}

impl HarnessModel for DelegatingProvider {
    fn telemetry_provider_id(&self) -> String {
        "delegating".to_string()
    }
}

/// A brain over the desk-bearing record whose provider is a
/// [`DelegatingProvider`] scripted to push `pushes[i]` on invoke `i + 1`.
/// Returns the brain plus the shared provider so a test can read the invoke
/// count.
pub(super) fn brain_that_delegates(
    dir: &std::path::Path,
    pushes: Vec<Option<Delegation>>,
) -> (HarnessBrain, Arc<DelegatingProvider>) {
    brain_that_delegates_with(
        dir,
        pushes.into_iter().map(Vec::from_iter).collect(),
        TurnFaults::default(),
    )
}

/// [`brain_that_delegates`], but each invoke pushes a whole *set* of
/// delegations (a single turn can queue several), and per-invoke faults can
/// make a delegate's run fail or be cancelled mid-flight.
pub(super) fn brain_that_delegates_with(
    dir: &std::path::Path,
    pushes: Vec<Vec<Delegation>>,
    faults: TurnFaults,
) -> (HarnessBrain, Arc<DelegatingProvider>) {
    let queue = orchestrator::DelegationQueue::default();
    let tasks = Arc::new(FsOps::new(dir));
    let steer = InflightRegistry::new();
    let provider = Arc::new(DelegatingProvider {
        queue: queue.clone(),
        pushes: StdMutex::new(pushes.into_iter().collect()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        tasks: tasks.clone(),
        board: StdMutex::new(Vec::new()),
        faults,
        steer: steer.clone(),
    });
    let deps = HarnessDeps {
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: provider.clone(),
        provider_slug: "delegating".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: None,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: None,
        tasks: Some(tasks),
        skills: None,
        skills_source_dir: None,
        skills_registry: std::sync::Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        artifacts: None,
        delegations: queue,
        workflow_runner: orchestrator::WorkflowRunnerHandle::default(),
        mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
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
        steer,
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
        workflow_runs: None,
        deep_trace: None,
    };
    (
        HarnessBrain::new(Arc::new(HarnessPool::new()), deps, record_with_desk()),
        provider,
    )
}
