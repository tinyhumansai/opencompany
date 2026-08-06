# Effect ports

The seams a company acts on the world through — calling tools, paying and being
paid, and passing every effect through policy first. Part of the port contracts
indexed by [ports.md](ports.md).

## ToolProvider

Tool catalog + invocation, scoped per company. Backed by OpenHuman JSON-RPC
by default, TinyAgents built-ins as fallback.

```rust
// src/ports/tools.rs
pub trait ToolProvider: Send + Sync {
    async fn catalog(&self, company: &CompanyId) -> Result<Vec<ToolSpec>>;
    async fn invoke(&self, company: &CompanyId, call: ToolCall) -> Result<ToolResult>;
}
```

Tool grants come from the manifest (`[tools].allow`, per-agent `tools`);
`invoke` MUST reject calls outside the grant before any side effect.

## AgentEconomy

The tiny.place seam ([integrations/tinyplace.md](../integrations/tinyplace.md)).

```rust
// src/ports/economy.rs
pub trait AgentEconomy: Send + Sync {
    async fn ensure_registered(&self, identity: &CompanyIdentity)
        -> Result<RegistrationState>;
    async fn publish_card(&self, identity: &CompanyIdentity, card: &AgentCard)
        -> Result<()>;
    async fn send_a2a_task(&self, to: &AgentAddr, task: A2aTask)
        -> Result<A2aTaskHandle>;
    async fn quote(&self, requirement: &PaymentRequirement) -> Result<Quote>;
    async fn pay(&self, quote: &Quote, budget: &BudgetScope) -> Result<PaymentReceipt>;
}
```

`pay` MUST fail if the `BudgetScope` (derived from `[budget]` and delegated
signer caps) would be exceeded; the ledger records every receipt.

## ApprovalGate

Policy evaluation and the approval queue
([company-brain/approvals.md](../company-brain/approvals.md)).

```rust
// src/ports/approvals.rs
pub trait ApprovalGate: Send + Sync {
    async fn evaluate(&self, company: &CompanyId, effect: &Effect)
        -> Result<PolicyDecision>; // Allow | RequireApproval | Deny
    async fn park(&self, company: &CompanyId, effect: Effect) -> Result<ApprovalId>;
    async fn resolve(&self, id: &ApprovalId, verdict: Verdict, by: Actor)
        -> Result<Option<Effect>>;
}
```
