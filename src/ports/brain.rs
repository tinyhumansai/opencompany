//! The cognition seam: [`Brain`] and its mid-cycle callback surface
//! [`CycleHost`].
//!
//! The kernel never reimplements the cycle; it hands events to a `Brain` and
//! services the brain's callbacks through a `CycleHost`.

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{
    ContextOp, ContextOpResult, CycleRequest, CycleResult, Effect, EffectDisposition, ToolCall,
    ToolResult,
};

/// The cognition port. One `run_cycle` call turns a batch of events into a
/// [`CycleResult`], calling back into the host for tools, context, and effects.
#[async_trait]
pub trait Brain: Send + Sync {
    /// Runs one cycle over the request, using `host` for mid-cycle callbacks.
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult>;
}

/// Callbacks the brain makes into the host mid-cycle.
#[async_trait]
pub trait CycleHost: Send + Sync {
    /// Invokes a tool through the host's [`ToolProvider`](super::tools::ToolProvider).
    async fn call_tool(&self, call: ToolCall) -> Result<ToolResult>;
    /// Issues a context operation against the host's context store.
    async fn context_op(&self, op: ContextOp) -> Result<ContextOpResult>;
    /// Emits a side effect for policy evaluation and dispatch.
    async fn emit_effect(&self, effect: Effect) -> Result<EffectDisposition>;
}
