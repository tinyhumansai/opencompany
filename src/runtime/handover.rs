//! [`RuntimeHandover`]: the per-instance state a rebuilt company runtime must
//! **inherit** rather than reconstruct (issue #290).
//!
//! Rebuilding a company — so that first-time BYOK setup moves it off the offline
//! echo brain without a process restart — is not "call
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build) twice". Most
//! of a runtime is `Arc<dyn Port>` handles that are safe to share, but several
//! pieces carry state that exists *only* in the live instance, and constructing
//! a second copy of any of them is a correctness bug rather than a waste:
//!
//! - **The journal is single-writer.** [`RuntimeJournal::append`] emits a record
//!   and its newline as two writes under a *per-instance* lock. A second journal
//!   over the same path interleaves two records onto one line, which fails to
//!   parse on replay — bricking the next boot and
//!   [`CompanyRuntime::recover`](crate::company::runtime::CompanyRuntime), not
//!   merely this process.
//! - **At-most-once effects are per-instance.** The executed-key set is built in
//!   memory at `load()`. A second instance never sees the first's commits, so a
//!   send or a spend runs twice.
//! - **The two serialising mutexes are per-instance.** `serial` guards a whole
//!   cycle; `task_writes` guards the board's read-modify-write. Two mutexes mean
//!   both invariants lapse across the swap.
//! - **The filesystem event log's `seq` is per-instance**, derived from a line
//!   count under its own lock, and its broadcast sender is what an open console
//!   SSE connection is subscribed to. A fresh one mints duplicate sequence
//!   numbers and leaves every open stream permanently deaf.
//! - **The feedback filer's rate limiter is in memory**, so rebuilding it turns a
//!   rebuild loop into a rate-limit bypass.
//! - **The harness pool holds each agent's conversation history**, which a fresh
//!   pool would silently drop; and the MCP runtime dials into a *process-global*
//!   connection map keyed by server id, so a re-boot replaces connections the
//!   outgoing runtime's agents may still be mid-call on.
//!
//! What is deliberately **not** carried over:
//!
//! - **The brain, tools, channels, workflow runner and economy.** Replacing
//!   those is the entire point of a rebuild.
//! - **The in-flight steer registry.** The successor's harness deps mint their
//!   own, and the operator steer routes read whichever runtime is registered.
//!   That is sound only because a swap happens after
//!   [`quiesce`](crate::company::runtime::CompanyRuntime::quiesce) has drained
//!   the cycle that would have registered a run, so the outgoing registry is
//!   empty at the swap point.
//! - **`source_dir`, mail, and the boot-only inputs.** Those are re-supplied by
//!   the rebuild caller from the same values boot used, so the successor is
//!   configured by one code path rather than two.

use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;

use crate::company::runtime::{CompanyRuntime, OpsStores};
use crate::feedback::service::FeedbackFiler;
use crate::feedback::store::FeedbackStore;
use crate::policy::ManifestApprovalGate;
use crate::ports::{CompanyStore, ContextStore, EventLog, InboxStore, MemoryStore, SecretStore};
use crate::runtime::blocked_nodes::BlockedNodeQueue;
use crate::runtime::continuation::ContinuationQueue;
use crate::runtime::grants::GrantSet;
use crate::runtime::journal::RuntimeJournal;
use crate::runtime::workflow_gates::WorkflowGateQueue;

/// The live state a successor runtime adopts from the runtime it replaces.
///
/// Produced by [`CompanyRuntime::handover`] and consumed by
/// [`RuntimeBuilder::with_handover`](crate::runtime::RuntimeBuilder::with_handover).
/// Cheap to make: every field is an `Arc` clone or an `Arc`-backed handle.
///
/// Its presence is also what tells `build()` it is rebuilding rather than
/// booting, which suppresses the boot-only side effects (journal replay, orphan
/// run reaping, going-public, MCP re-boot) that must not fire a second time.
#[derive(Clone)]
pub struct RuntimeHandover {
    pub(crate) store: Arc<dyn CompanyStore>,
    pub(crate) events: Arc<dyn EventLog>,
    pub(crate) memory: Arc<dyn MemoryStore>,
    pub(crate) context: Arc<dyn ContextStore>,
    pub(crate) inbound_context: Arc<dyn ContextStore>,
    pub(crate) scratch_context: Option<Arc<dyn ContextStore>>,
    pub(crate) memory_scopes: Option<Arc<dyn crate::store::MemoryScopes>>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) inbox: Arc<dyn InboxStore>,
    pub(crate) ops: OpsStores,
    pub(crate) feedback: Arc<FeedbackStore>,
    pub(crate) filer: Arc<FeedbackFiler>,
    pub(crate) journal: Arc<RuntimeJournal>,
    pub(crate) approval_gate: Arc<ManifestApprovalGate>,
    pub(crate) grants: GrantSet,
    /// Issue #469: the turns still blocked on a decision. Live per-instance
    /// state like the grant set — a swap mid-turn that forgot which turns were
    /// waiting would continue the next one on its first decision instead of its
    /// last.
    pub(crate) continuations: ContinuationQueue,
    /// Issue #978: the parked gates of every workflow run still awaiting a
    /// decision. Inherited for [`continuations`](Self::continuations)' reason
    /// exactly — a successor that forgot them would re-ask about every gate of a
    /// partly-decided run.
    pub(crate) workflow_gates: WorkflowGateQueue,
    /// Issue #899 (Stage 1): the blocked-agent-node stashes still awaiting a
    /// decision. Inherited live for [`continuations`](Self::continuations)'
    /// reason — a successor that forgot them would release a blocked node's
    /// batch with nothing to spawn and tell the operator to re-run a workflow
    /// that is in fact ready to continue.
    pub(crate) blocked_nodes: BlockedNodeQueue,
    pub(crate) serial: Arc<TokioMutex<()>>,
    /// The per-agent lock slots. Inherited across the swap for the same reason
    /// as `serial`: a fresh map would let an agent mid-turn start a second turn
    /// beside itself.
    pub(crate) per_agent: Arc<TokioMutex<std::collections::HashMap<String, Arc<TokioMutex<()>>>>>,
    pub(crate) task_writes: Arc<TokioMutex<()>>,
    #[cfg(feature = "openhuman")]
    pub(crate) harness: Option<Arc<crate::harness::HarnessPool>>,
    #[cfg(feature = "mcp")]
    pub(crate) mcp: Option<Arc<crate::harness::mcp::McpRuntime>>,
}

impl std::fmt::Debug for RuntimeHandover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandover").finish_non_exhaustive()
    }
}

impl CompanyRuntime {
    /// Snapshots the live state a successor must inherit (issue #290).
    ///
    /// Take this *after* [`quiesce`](Self::quiesce): the handover is only sound
    /// at a point with no cycle in flight, because a cycle holds `serial` and
    /// writes the journal, and the successor is about to own both.
    pub fn handover(&self) -> RuntimeHandover {
        RuntimeHandover {
            store: self.store.clone(),
            events: self.events.clone(),
            memory: self.memory.clone(),
            context: self.context.clone(),
            inbound_context: self.inbound_context.clone(),
            scratch_context: self.scratch_context.clone(),
            memory_scopes: self.memory_scopes.clone(),
            secrets: self.secrets.clone(),
            inbox: self.inbox.clone(),
            ops: self.ops.clone(),
            feedback: self.feedback.clone(),
            filer: self.filer.clone(),
            journal: self.journal.clone(),
            approval_gate: self.approval_gate.clone(),
            grants: self.grants.clone(),
            continuations: self.continuations.clone(),
            workflow_gates: self.workflow_gates.clone(),
            blocked_nodes: self.blocked_nodes.clone(),
            serial: self.serial.clone(),
            per_agent: self.per_agent.clone(),
            task_writes: self.task_writes.clone(),
            #[cfg(feature = "openhuman")]
            harness: self.harness.clone(),
            #[cfg(feature = "mcp")]
            mcp: self.mcp.clone(),
        }
    }
}
