//! The [`AgentEconomy`] port: the tiny.place seam.

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{
    A2aTask, A2aTaskHandle, AgentAddr, AgentCard, BudgetScope, CompanyIdentity, PaymentReceipt,
    PaymentRequirement, Quote, RegistrationState,
};

/// The tiny.place economy seam: identity, agent cards, A2A tasks, and payments.
///
/// `pay` MUST fail if the [`BudgetScope`] would be exceeded; the ledger records
/// every receipt.
#[async_trait]
pub trait AgentEconomy: Send + Sync {
    /// Ensures the company is registered, returning its registration state.
    async fn ensure_registered(&self, identity: &CompanyIdentity) -> Result<RegistrationState>;
    /// Publishes or updates the company's Agent Card.
    async fn publish_card(&self, identity: &CompanyIdentity, card: &AgentCard) -> Result<()>;
    /// Sends an A2A task to another agent.
    async fn send_a2a_task(&self, to: &AgentAddr, task: A2aTask) -> Result<A2aTaskHandle>;
    /// Requests a firm quote for a payment requirement.
    async fn quote(&self, requirement: &PaymentRequirement) -> Result<Quote>;
    /// Pays a quote within a budget scope, returning a receipt.
    async fn pay(&self, quote: &Quote, budget: &BudgetScope) -> Result<PaymentReceipt>;
}
