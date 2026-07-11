//! The [`CompanyRuntime`] assembly: one running company's wired-together ports.
//!
//! The struct matches the sketch in `docs/spec/runtime/ports.md` — the nine
//! ports, with `economy` the only optional one. Three runtime-internal fields
//! are added: the company `id`, a per-company serial lock so exactly one cycle
//! runs at a time, and the [`RuntimeJournal`] backing at-most-once effects and
//! the durable approval queue.
//!
//! The cycle logic itself lives in [`CycleRunner`](crate::runtime::CycleRunner);
//! the methods here are thin delegations so callers hold a single
//! `Arc<CompanyRuntime>`.

use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::{Actor, ApprovalId, CompanyEvent, CompanyId, Verdict};
use crate::ports::{
    AgentEconomy, ApprovalGate, Brain, ChannelAdapter, CompanyStore, ContextStore, EventLog,
    MemoryStore, ToolProvider,
};
use crate::runtime::CycleRunner;
use crate::runtime::journal::RuntimeJournal;
use crate::runtime::types::{ApprovalSummary, CompanyStatus, CycleReport};

/// A running company: its brain, stores, channels, and policy gate, wired
/// together behind a serial cycle loop.
pub struct CompanyRuntime {
    pub(crate) id: CompanyId,
    pub(crate) brain: Arc<dyn Brain>,
    pub(crate) store: Arc<dyn CompanyStore>,
    pub(crate) events: Arc<dyn EventLog>,
    pub(crate) memory: Arc<dyn MemoryStore>,
    pub(crate) context: Arc<dyn ContextStore>,
    pub(crate) tools: Arc<dyn ToolProvider>,
    pub(crate) channels: Vec<Arc<dyn ChannelAdapter>>,
    pub(crate) economy: Option<Arc<dyn AgentEconomy>>,
    pub(crate) approvals: Arc<dyn ApprovalGate>,
    pub(crate) journal: Arc<RuntimeJournal>,
    /// Held for the duration of a cycle so cycles never interleave per company.
    pub(crate) serial: TokioMutex<()>,
}

impl CompanyRuntime {
    /// Assembles a runtime from its ports. Most callers use
    /// [`RuntimeBuilder`](crate::runtime::RuntimeBuilder) instead.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: CompanyId,
        brain: Arc<dyn Brain>,
        store: Arc<dyn CompanyStore>,
        events: Arc<dyn EventLog>,
        memory: Arc<dyn MemoryStore>,
        context: Arc<dyn ContextStore>,
        tools: Arc<dyn ToolProvider>,
        channels: Vec<Arc<dyn ChannelAdapter>>,
        economy: Option<Arc<dyn AgentEconomy>>,
        approvals: Arc<dyn ApprovalGate>,
        journal: Arc<RuntimeJournal>,
    ) -> Self {
        Self {
            id,
            brain,
            store,
            events,
            memory,
            context,
            tools,
            channels,
            economy,
            approvals,
            journal,
            serial: TokioMutex::new(()),
        }
    }

    /// This company's id.
    pub fn id(&self) -> &CompanyId {
        &self.id
    }

    /// Whether an agent economy (tiny.place) is wired in.
    pub fn has_economy(&self) -> bool {
        self.economy.is_some()
    }

    /// Runs one cycle over a batch of events, returning what happened.
    pub async fn run_cycle(&self, events: Vec<CompanyEvent>) -> Result<CycleReport> {
        CycleRunner::new(self).run(events).await
    }

    /// Resolves a parked approval and runs a follow-up cycle so the brain learns
    /// the verdict. Returns the follow-up cycle's report.
    pub async fn resolve_approval(
        &self,
        id: &ApprovalId,
        verdict: Verdict,
        by: Actor,
    ) -> Result<CycleReport> {
        CycleRunner::new(self)
            .resolve_approval(id, verdict, by)
            .await
    }

    /// Replays the journal to rebuild the executed-key set and approval queue.
    pub async fn recover(&self) -> Result<()> {
        self.journal.load().await
    }

    /// The approvals currently awaiting the operator.
    pub fn pending_approvals(&self) -> Vec<ApprovalSummary> {
        self.journal
            .pending()
            .into_iter()
            .map(|p| ApprovalSummary {
                id: p.id,
                kind: p.effect.kind,
                amount_usd: p.effect.amount_usd,
                at_millis: p.at_millis,
            })
            .collect()
    }

    /// A status snapshot, loading the company record for name and lifecycle.
    pub async fn status(&self) -> Result<CompanyStatus> {
        let record = self.store.load(&self.id).await?;
        let (name, lifecycle) = match record {
            Some(record) => (record.manifest.company.name, record.lifecycle),
            None => (self.id.to_string(), "running".to_string()),
        };
        Ok(CompanyStatus {
            id: self.id.clone(),
            name,
            lifecycle,
            pending_approvals: self.journal.pending().len(),
        })
    }

    /// Rejects operation on a company that is not accepting work.
    ///
    /// Returns [`OpenCompanyError::LifecycleConflict`] when the loaded record's
    /// lifecycle is anything other than `running`.
    pub async fn ensure_running(&self) -> Result<()> {
        if let Some(record) = self.store.load(&self.id).await?
            && record.lifecycle != "running"
        {
            return Err(OpenCompanyError::LifecycleConflict(record.lifecycle));
        }
        Ok(())
    }
}

impl std::fmt::Debug for CompanyRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanyRuntime")
            .field("id", &self.id)
            .field("channels", &self.channels.len())
            .field("has_economy", &self.economy.is_some())
            .finish_non_exhaustive()
    }
}
