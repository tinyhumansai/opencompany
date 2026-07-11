//! The [`ApprovalGate`] port: policy evaluation and the approval queue.

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{Actor, ApprovalId, CompanyId, Effect, PolicyDecision, Verdict};

/// Policy evaluation and the approval queue.
///
/// `evaluate` returns a bare [`PolicyDecision`]; the [`ApprovalId`] for a
/// `RequireApproval` decision is minted at `park`, not at `evaluate`.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Evaluates an effect against the company's policy.
    async fn evaluate(&self, company: &CompanyId, effect: &Effect) -> Result<PolicyDecision>;
    /// Parks an effect for operator approval, returning its id.
    async fn park(&self, company: &CompanyId, effect: Effect) -> Result<ApprovalId>;
    /// Resolves a parked effect; returns the effect to execute on approval,
    /// or `None` on denial.
    async fn resolve(&self, id: &ApprovalId, verdict: Verdict, by: Actor)
    -> Result<Option<Effect>>;
}
