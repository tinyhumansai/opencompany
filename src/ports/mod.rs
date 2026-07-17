//! Port contracts: the kernel's dependency-inverted seams.
//!
//! Each port is one trait in one file, matching the binding names in
//! `docs/spec/runtime/ports.md`. Traits are `#[async_trait::async_trait]` so
//! they remain object-safe as `Arc<dyn Port>`; sync accessor/stream methods
//! (`EventLog::subscribe`, `ChannelAdapter::inbound`, `channel_id`) stay plain
//! `fn`. Shared payload/id/enum types live in [`types`].

mod ids;

pub mod approvals;
pub mod brain;
pub mod channel;
pub mod context;
pub mod economy;
pub mod events;
pub mod facts;
pub mod inbox;
pub mod login_codes;
pub mod memory;
pub mod secrets;
pub mod sessions;
pub mod skills_state;
pub mod store;
pub mod tasks;
pub mod tools;
pub mod types;
pub mod usage;
pub mod users;
pub mod workspace;

pub use approvals::ApprovalGate;
pub use brain::{Brain, CycleHost};
pub use channel::ChannelAdapter;
pub use context::ContextStore;
pub use economy::AgentEconomy;
pub use events::EventLog;
pub use facts::{FactKind, FactRecord, FactStore};
pub use ids::{generate_id, now_millis};
pub use inbox::{EmailRecord, InboxMeta, InboxStore};
pub use login_codes::{LoginCodeRecord, LoginCodeStore};
pub use memory::MemoryStore;
pub use secrets::SecretStore;
pub use sessions::{SessionRecord, SessionStore};
pub use skills_state::{SkillSource, SkillState, SkillStateStore};
pub use store::CompanyStore;
pub use tasks::{TaskRecord, TaskStore};
pub use tools::ToolProvider;
pub use types::*;
pub use usage::{SampleKind, UsageMeter, UsageSample};
pub use users::{InviteRecord, UserRecord, UserRole, UserStatus, UserStore, normalize_email};
pub use workspace::{NodeKind, WorkspaceNode, WorkspaceStore};

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
        _users: &dyn crate::ports::users::UserStore,
        _sessions: &dyn crate::ports::sessions::SessionStore,
        _login_codes: &dyn crate::ports::login_codes::LoginCodeStore,
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
