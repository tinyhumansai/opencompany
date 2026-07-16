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
pub mod inbox;
pub mod memory;
pub mod secrets;
pub mod store;
pub mod tools;
pub mod types;

pub use approvals::ApprovalGate;
pub use brain::{Brain, CycleHost};
pub use channel::ChannelAdapter;
pub use context::ContextStore;
pub use economy::AgentEconomy;
pub use events::EventLog;
pub use ids::{generate_id, now_millis};
pub use inbox::{EmailRecord, InboxStore};
pub use memory::MemoryStore;
pub use secrets::SecretStore;
pub use store::CompanyStore;
pub use tools::ToolProvider;
pub use types::*;

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
