//! The finances read: `Company.finances` over WS5's ledger projection.
//!
//! `finances_from` folds the company ledger, the manifest `[budget]`, and (when
//! present) the economy wallet balance into the console's finance surface.

use std::sync::Arc;

use async_graphql::{Object, SimpleObject};

use super::now_millis;
use crate::company::runtime::CompanyRuntime;
use crate::metering::{CategorySpend, Direction, Finances, Transaction, finances_from};

/// Spend rolled up by category.
#[derive(SimpleObject)]
#[graphql(name = "CategorySpend")]
pub struct CategorySpendGql {
    /// The category label.
    pub category: String,
    /// The amount spent in it, USD.
    pub amount: f64,
}

impl From<CategorySpend> for CategorySpendGql {
    fn from(spend: CategorySpend) -> Self {
        Self {
            category: spend.category,
            amount: spend.amount,
        }
    }
}

/// One ledger transaction in the finance journal.
#[derive(SimpleObject)]
#[graphql(name = "Transaction")]
pub struct TransactionGql {
    /// The transaction id.
    pub id: async_graphql::ID,
    /// The transaction date, `YYYY-MM-DD`.
    pub date: String,
    /// A human description.
    pub description: String,
    /// The spend category.
    pub category: String,
    /// The amount, USD (a positive magnitude; see `direction`).
    pub amount_usd: f64,
    /// `in` for revenue, `out` for spend.
    pub direction: String,
}

impl From<Transaction> for TransactionGql {
    fn from(tx: Transaction) -> Self {
        Self {
            id: async_graphql::ID(tx.id),
            date: tx.date,
            description: tx.description,
            category: tx.category,
            amount_usd: tx.amount_usd,
            direction: match tx.direction {
                Direction::In => "in",
                Direction::Out => "out",
            }
            .to_string(),
        }
    }
}

/// The finance read surface for a company. Wraps the computed [`Finances`] so
/// `transactions(first, offset)` can slice the journal on demand.
pub struct FinancesGql {
    inner: Finances,
}

#[Object(name = "Finances")]
impl FinancesGql {
    /// The current balance, USD.
    async fn balance_usd(&self) -> f64 {
        self.inner.balance_usd
    }

    /// The monthly budget, USD.
    async fn budget_usd(&self) -> f64 {
        self.inner.budget_usd
    }

    /// Total spend this period, USD.
    async fn spent_usd(&self) -> f64 {
        self.inner.spent_usd
    }

    /// Total revenue this period, USD.
    async fn revenue_usd(&self) -> f64 {
        self.inner.revenue_usd
    }

    /// Net (revenue − spend), USD.
    async fn net_usd(&self) -> f64 {
        self.inner.net_usd
    }

    /// Spend rolled up by category.
    async fn by_category(&self) -> Vec<CategorySpendGql> {
        self.inner
            .by_category
            .iter()
            .cloned()
            .map(CategorySpendGql::from)
            .collect()
    }

    /// A page of the transaction journal, most-recent first.
    async fn transactions(
        &self,
        #[graphql(default = 50)] first: i32,
        #[graphql(default = 0)] offset: i32,
    ) -> Vec<TransactionGql> {
        self.inner
            .transactions
            .iter()
            .skip(offset.max(0) as usize)
            .take(first.max(0) as usize)
            .cloned()
            .map(TransactionGql::from)
            .collect()
    }
}

/// Resolves `Company.finances`.
pub(crate) async fn resolve(runtime: &Arc<CompanyRuntime>) -> async_graphql::Result<FinancesGql> {
    let record = runtime.store().load(runtime.id()).await?;
    let (ledger, budget) = match &record {
        Some(record) => (record.ledger.clone(), record.manifest.budget.clone()),
        None => (Vec::new(), crate::company::Budget::default()),
    };
    // The economy wallet balance is not surfaced through a read accessor, so the
    // projection runs ledger-only; `has_economy` gates whether one exists.
    let economy_balance = None;
    let finances = finances_from(&ledger, &budget, economy_balance, now_millis());
    Ok(FinancesGql { inner: finances })
}
