//! The `Company` aggregation root and its directly-owned leaf objects.
//!
//! [`CompanyGql`] is a **handle**, not an eager projection: it carries the
//! company id and its [`CompanyRuntime`], and every field is an async resolver
//! that awaits the relevant port or parser only when selected. Nested fields
//! are safe without re-checking auth because the handle is only ever reachable
//! through an authorized `companies` / `company` query.

use std::sync::Arc;

use async_graphql::{Context, ID, Object, SimpleObject};

use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyId;

/// The aggregation-root handle over one company. See the module docs.
pub struct CompanyGql {
    id: CompanyId,
    runtime: Arc<CompanyRuntime>,
}

impl CompanyGql {
    /// Builds a handle over a resolved company runtime.
    pub fn new(id: CompanyId, runtime: Arc<CompanyRuntime>) -> Self {
        Self { id, runtime }
    }

    /// The wrapped runtime, for resolver helpers in sibling modules.
    #[allow(dead_code)] // used by the read-surface resolver modules added next.
    pub(crate) fn runtime(&self) -> &Arc<CompanyRuntime> {
        &self.runtime
    }

    /// The company id, for resolver helpers in sibling modules.
    #[allow(dead_code)] // used by the read-surface resolver modules added next.
    pub(crate) fn company_id(&self) -> &CompanyId {
        &self.id
    }
}

#[Object(name = "Company")]
impl CompanyGql {
    /// The company id.
    async fn id(&self) -> ID {
        ID(self.id.as_ref().to_string())
    }

    /// The display name from the company charter.
    async fn name(&self, _ctx: &Context<'_>) -> async_graphql::Result<String> {
        Ok(self.runtime.status().await?.name)
    }

    /// Lifecycle state, e.g. `running`, `paused`, `archived`.
    async fn lifecycle(&self, _ctx: &Context<'_>) -> async_graphql::Result<String> {
        Ok(self.runtime.status().await?.lifecycle)
    }

    /// The number of approvals currently awaiting the operator.
    async fn pending_approvals(&self) -> i32 {
        self.runtime.pending_approvals().len() as i32
    }

    /// The approvals currently awaiting the operator for this company.
    async fn approvals(&self) -> Vec<ApprovalGql> {
        self.runtime
            .pending_approvals()
            .into_iter()
            .map(ApprovalGql::from)
            .collect()
    }
}

/// A parked approval awaiting the operator. Mirrors
/// [`ApprovalSummary`](crate::runtime::types::ApprovalSummary).
#[derive(SimpleObject)]
#[graphql(name = "Approval")]
pub struct ApprovalGql {
    /// The approval's id.
    pub id: ID,
    /// The parked effect's dotted kind.
    pub kind: String,
    /// The USD amount involved, if any.
    pub amount_usd: Option<f64>,
    /// Epoch-millis the effect was parked. `Float` round-trips the full u64
    /// range that would overflow GraphQL's `Int`.
    pub at_millis: f64,
}

impl From<crate::runtime::types::ApprovalSummary> for ApprovalGql {
    fn from(summary: crate::runtime::types::ApprovalSummary) -> Self {
        Self {
            id: ID(summary.id.as_ref().to_string()),
            kind: summary.kind,
            amount_usd: summary.amount_usd,
            at_millis: summary.at_millis as f64,
        }
    }
}
