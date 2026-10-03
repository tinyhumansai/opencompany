use std::sync::Arc;

use super::*;

// A compile-time proof that every port is object-safe. If any trait were
// not dyn-compatible (e.g. a bare `async fn` without `#[async_trait]`),
// this signature would fail to compile.
#[allow(clippy::too_many_arguments, dead_code)]
fn assert_object_safe(
    _brain: &dyn Brain,
    _host: &dyn CycleHost,
    _store: &dyn CompanyStore,
    _events: &dyn EventLog,
    _memory: &dyn MemoryStore,
    _context: &dyn ContextStore,
    _channel: &dyn ChannelAdapter,
    _tools: &dyn ToolProvider,
    _approvals: &dyn ApprovalGate,
    _secrets: &dyn SecretStore,
    _inbox: &dyn crate::ports::inbox::InboxStore,
    _tasks: &dyn crate::ports::tasks::TaskStore,
    _workspace: &dyn crate::ports::workspace::WorkspaceStore,
    _facts: &dyn crate::ports::facts::FactStore,
    _usage: &dyn crate::ports::usage::UsageMeter,
    _skills: &dyn crate::ports::skills_state::SkillStateStore,
    _notifications: &dyn crate::ports::notifications::NotificationStore,
    _read_state: &dyn crate::ports::read_state::ReadStateStore,
    _users: &dyn crate::ports::users::UserStore,
    _sessions: &dyn crate::ports::sessions::SessionStore,
    _login_codes: &dyn crate::ports::login_codes::LoginCodeStore,
    _runs: &dyn crate::ports::runs::RunStore,
    _workflow_revisions: &dyn crate::ports::workflow_revisions::WorkflowRevisionStore,
    _schedule_fires: &dyn crate::ports::schedule_fires::ScheduleFireStore,
    _run_output: &dyn crate::ports::run_output::WorkflowRunOutputStore,
    _workflow_runner: &dyn crate::ports::workflow_runner::WorkflowRunner,
    _journal: &dyn crate::ports::journal::JournalStore,
    _ledgers: &dyn crate::ports::ledgers::LedgerStore,
) {
}

// A no-op Brain proves `Arc<dyn Brain>` can actually be constructed.
struct NoopBrain;

#[async_trait::async_trait]
impl Brain for NoopBrain {
    async fn run_cycle(
        &self,
        req: CycleRequest,
        _host: &dyn CycleHost,
    ) -> crate::Result<CycleResult> {
        let _ = req;
        Ok(CycleResult {
            channel_responses: Vec::new(),
            new_traces: Vec::new(),
            ledger_deltas: Vec::new(),
            token_usage: TokenUsage::default(),
        })
    }
}

#[test]
fn ports_are_dyn_compatible() {
    let brain: Arc<dyn Brain> = Arc::new(NoopBrain);
    // Using it as a trait object exercises the vtable.
    let _: &dyn Brain = brain.as_ref();
}
