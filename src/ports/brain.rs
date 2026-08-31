//! The cognition seam: [`Brain`] and its mid-cycle callback surface
//! [`CycleHost`].
//!
//! The kernel never reimplements the cycle; it hands events to a `Brain` and
//! services the brain's callbacks through a `CycleHost`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::metering::ModelSlug;
use crate::ports::types::{
    ApprovalId, ContextOp, ContextOpResult, CycleRequest, CycleResult, Effect, EffectDisposition,
    ToolCall, ToolResult,
};

/// Where a cognition path's inference usage is metered — the operator's answer
/// to "why does Usage read zero?" (issue #174).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageMetering {
    /// Metered per agent turn by the path itself, from the provider's real
    /// token/cost totals (the `openhuman` harness). A zero reading here means
    /// nothing ran.
    PerTurn,
    /// Metered per cycle by the runtime, from what the brain reports as
    /// [`CycleResult::token_usage`](crate::ports::types::CycleResult::token_usage)
    /// — the hosted Medulla path reads it off the `orch:usage` wire frame, so a
    /// zero reading means the upstream reported no usage.
    #[default]
    PerCycle,
    /// No model inference happens on this path, so there is nothing to meter
    /// (the offline echo brain). A zero reading is the truth, not a gap.
    None,
}

/// The [`Cognition::path`] label of the embedded-harness brain — the only path
/// a tenant `[inference]` / console BYOK config actually feeds.
///
/// A named constant rather than a literal because two places must agree: the
/// brain that *reports* it (`crate::harness::brain`) and the inference-status
/// route that *tests* it to decide whether a saved config has reached the
/// running brain (issue #266). A literal in both would let a wording change on
/// one side silently turn the test into a permanent `false`.
pub const HARNESS_PATH: &str = "harness";

/// The [`Cognition::path`] label of the offline [`EchoBrain`](crate::brain::EchoBrain)
/// — the degraded fallback that runs no model at all and answers every operator
/// message with `"You said: …"`.
///
/// A named constant for the same reason [`HARNESS_PATH`] is one: the brain that
/// *reports* it (`crate::brain::echo`) and the inference-status route that
/// *tests* it to decide whether this company can think (issue #1735) must agree,
/// and a literal in both would let a wording change on one side turn the test
/// into a permanent "configured".
pub const ECHO_PATH: &str = "echo";

/// How a [`Brain`] does cognition, for metering and operator diagnosis.
///
/// The runtime reads [`Self::provider`] when it meters a cycle's usage, and the
/// console's inference-status route surfaces the whole descriptor so an operator
/// can tell "no inference source configured, so nothing was spent" from
/// "inference ran but the meter never saw it".
///
/// Deliberately not `Serialize`: the labels are `&'static str` build facts, so a
/// route projects them onto its own DTO (`cognition` + `usageMetering`) rather
/// than leaking this shape onto the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cognition {
    /// Stable label for the path: `harness` ([`HARNESS_PATH`]), `hosted`,
    /// `echo`, `sidecar`, or `custom` for an injected brain.
    pub path: &'static str,
    /// The provider slug this path's cycle usage is metered under.
    pub provider: &'static str,
    /// Which model this path's cycle usage is metered against, folded onto the
    /// closed [`ModelSlug`] vocabulary (issue #1749).
    ///
    /// `None` when the path cannot name one — an injected brain, the offline
    /// echo brain (which runs no model), the sidecar (whose model is the host
    /// [`InferenceClient`](crate::ports::InferenceClient)'s business), and the
    /// hosted Medulla path (where the model is chosen upstream and never
    /// reaches this process). Reporting a guess would be worse than reporting
    /// nothing: an operator reading a model name off the Usage view would have
    /// no way to tell one that was observed from one that was assumed.
    ///
    /// The `openhuman` harness names its model per **turn** rather than here —
    /// it meters [`UsageMetering::PerTurn`] and reports zero cycle usage, so
    /// this field would never reach a sample on that path. See
    /// `harness::built_in::provider::HarnessModel::telemetry_model`.
    pub model: Option<ModelSlug>,
    /// Where this path's usage is metered.
    pub metering: UsageMetering,
}

impl Default for Cognition {
    fn default() -> Self {
        Self {
            // An injected brain (a test double, a caller-supplied brain) reports
            // its usage through `CycleResult` like the hosted path, but cannot
            // name the provider that served it.
            path: "custom",
            provider: crate::metering::UNKNOWN_PROVIDER,
            model: None,
            metering: UsageMetering::PerCycle,
        }
    }
}

/// The cognition port. One `run_cycle` call turns a batch of events into a
/// [`CycleResult`], calling back into the host for tools, context, and effects.
#[async_trait]
pub trait Brain: Send + Sync {
    /// Runs one cycle over the request, using `host` for mid-cycle callbacks.
    async fn run_cycle(&self, req: CycleRequest, host: &dyn CycleHost) -> Result<CycleResult>;

    /// How this brain does cognition and where its usage is metered.
    ///
    /// The default describes an injected brain: usage comes back through
    /// [`CycleResult::token_usage`](crate::ports::types::CycleResult::token_usage)
    /// under an unknown provider. A brain that meters itself must say so with
    /// [`UsageMetering::PerTurn`] **and** report zero `token_usage`, or its spend
    /// is counted twice.
    fn cognition(&self) -> Cognition {
        Cognition::default()
    }
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
    /// Parks an effect for operator approval **without** re-evaluating it.
    ///
    /// The counterpart to [`emit_effect`](Self::emit_effect), for a decision the
    /// brain has already made. A brain that hosts its own policy layer — the
    /// harness brain's openhuman `ApprovalPolicy`, which blocks a gated tool call
    /// *inside* the agent turn — has no effect to submit for evaluation: the
    /// verdict is already `RequireApproval`, and the projected effect exists only
    /// so the operator can see and resolve it. Routing it through `emit_effect`
    /// would re-decide it against the coarser
    /// [`ApprovalGate`](crate::ports::ApprovalGate) taxonomy, which allows (and
    /// so "executes") anything it classifies as
    /// [`EffectGroup::Other`](crate::ports::types::EffectGroup::Other) — the
    /// request would vanish instead of parking.
    ///
    /// Parks on the gate, journals the record so it survives a restart, and
    /// returns the new [`ApprovalId`]. (Issue #172.)
    async fn park_effect(&self, effect: Effect) -> Result<ApprovalId>;
}
