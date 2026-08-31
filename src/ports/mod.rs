//! Port contracts: the kernel's dependency-inverted seams.
//!
//! Each port is one trait in one file, matching the binding names in
//! `docs/spec/runtime/ports.md`. Traits are `#[async_trait::async_trait]` so
//! they remain object-safe as `Arc<dyn Port>`; sync accessor/stream methods
//! (`EventLog::subscribe`, `ChannelAdapter::inbound`, `channel_id`) stay plain
//! `fn`. Shared payload/id/enum types live in [`types`].

mod ids;

pub mod acp;
pub mod approvals;
pub mod artifacts;
pub mod blockers;
pub mod brain;
pub mod channel;
pub mod context;
pub mod deep_trace;
pub mod economy;
pub mod events;
pub mod facts;
pub mod inbox;
pub mod journal;
pub mod ledgers;
pub mod login_codes;
pub mod memory;
pub mod notifications;
pub mod read_state;
pub mod run_output;
pub mod runs;
pub mod schedule_fires;
pub mod secrets;
pub mod sessions;
pub mod skills_state;
pub mod store;
pub mod tasks;
pub mod tools;
pub mod types;
pub mod usage;
pub mod users;
pub mod workflow_revisions;
pub mod workflow_runner;
pub mod workflow_verdict;
pub mod workspace;

pub use acp::{AcpAgent, AcpAgentFactory, AcpTurn, AcpUpdate};
pub use approvals::ApprovalGate;
pub use artifacts::{
    ArtifactAuthor, ArtifactDiff, ArtifactKind, ArtifactRecord, ArtifactStore, ArtifactVersion,
    DiffLine, DiffOp,
};
pub use blockers::{
    BLOCKER_EFFECT_PREFIX, BlockerKind, BlockerPayload, BlockerSource, BlockerStep,
};
pub use brain::{Brain, Cognition, CycleHost, UsageMetering};
pub use channel::ChannelAdapter;
pub use context::ContextStore;
pub use deep_trace::{
    DEEP_ARGUMENTS_CHAR_CAP, DEEP_OUTPUT_CHAR_CAP, DEEP_REASONING_CHAR_CAP, DeepTraceStore,
    MAX_DEEP_RUNS_PER_COMPANY, MAX_DEEP_STEPS_PER_RUN, RunStepDetailRecord, TurnStepDetail,
    bound_detail,
};
pub use economy::AgentEconomy;
pub use events::{EventLog, PruneReport, RetentionClass, RetentionPolicy, plan_prune};
pub use facts::{FactKind, FactRecord, FactStore};
pub use ids::{
    AGENT_SLUG_FALLBACK, CONFINED_AGENT_ID, SYSTEM_AUTHOR, agent_slug, generate_id, now_millis,
};
pub use inbox::{EmailRecord, InboxMeta, InboxStore};
pub use journal::{Durability, JournalStore};
pub use ledgers::LedgerStore;
pub use login_codes::{LoginCodeRecord, LoginCodeStore};
pub use memory::MemoryStore;
pub use notifications::{Notification, NotificationStore, NotificationView, Subject, SubjectKind};
pub use read_state::{ChannelRead, ReadStateStore};
pub use run_output::{
    MAX_RUN_OUTPUTS_PER_COMPANY, WorkflowRunOutputRecord, WorkflowRunOutputStore, bound_node_output,
};
pub use runs::{
    NewRun, RunFilter, RunOutcome, RunRecord, RunStatus, RunStepRecord, RunStore,
    reap_orphaned_runs,
};
pub use schedule_fires::ScheduleFireStore;
pub use secrets::SecretStore;
pub use sessions::{SessionKind, SessionRecord, SessionStore};
pub use skills_state::{SkillSource, SkillState, SkillStateStore};
pub use store::CompanyStore;
pub use tasks::{TaskRecord, TaskStore};
pub use tools::ToolProvider;
pub use types::*;
pub use usage::{SampleKind, UsageMeter, UsageSample};
pub use users::{
    InviteRecord, LoginIdentity, UserRecord, UserRole, UserStatus, UserStore,
    decode_wallet_address, derive_display_name, normalize_email, normalize_wallet,
};
pub use workflow_revisions::{
    MAX_WORKFLOW_REVISIONS, WorkflowRevisionRecord, WorkflowRevisionStore,
};
pub use workflow_runner::{
    DeliveryReason, DeliveryReport, DeliveryStatus, RunCancel, WorkflowApprovalOutcome,
    WorkflowBlockedNode, WorkflowBoardAction, WorkflowRun, WorkflowRunApprovalRow,
    WorkflowRunBoardRow, WorkflowRunContext, WorkflowRunNodeRow, WorkflowRunner,
};
pub use workflow_verdict::{
    RunVerdictFacts, WorkflowRunVerdict, awaiting_count, is_undelivered, undelivered_count,
};
pub use workspace::{NodeKind, WorkspaceNode, WorkspaceOrigin, WorkspaceStore};

#[cfg(test)]
mod test {
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
        _economy: &dyn AgentEconomy,
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
}
