//! Result types for the WS5 Usage & Finances read surfaces.
//!
//! These are plain data structs produced by the pure aggregation functions in
//! [`super::usage`] and [`super::finances`]. WS2 wraps them in async-graphql
//! objects (`graphql/usage.rs`, `graphql/finances.rs`); nothing here depends on
//! async-graphql, so the projections stay trivially unit-testable. Field names
//! match the console's TypeScript shapes (serde `camelCase`).

use serde::{Deserialize, Serialize};

/// The console's usage window: 7, 30, or 90 days ending "now".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UsageRange {
    /// The last 7 days.
    D7,
    /// The last 30 days.
    D30,
    /// The last 90 days (the retention ceiling — see
    /// [`crate::ports::usage::RETENTION_DAYS`]).
    D90,
}

impl UsageRange {
    /// The number of daily buckets this range spans.
    pub fn days(self) -> u64 {
        match self {
            UsageRange::D7 => 7,
            UsageRange::D30 => 30,
            UsageRange::D90 => 90,
        }
    }
}

/// One day's token burn in the usage series (zero-filled across the range).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePoint {
    /// The bucket's UTC calendar day, ISO `YYYY-MM-DD`.
    pub date: String,
    /// Input/prompt tokens consumed on this day.
    pub input_tokens: u64,
    /// Output/completion tokens produced on this day.
    pub output_tokens: u64,
}

/// Tokens attributed to one teammate over the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTokens {
    /// The teammate's display name (prosumer language), resolved from the roster.
    pub name: String,
    /// Total input + output tokens attributed to the teammate.
    pub tokens: u64,
}

/// OAuth-connected tool calls counted per provider over the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCalls {
    /// The connected provider slug (e.g. `github`, `gmail`).
    pub provider: String,
    /// The number of OAuth-connected tool calls made through the provider.
    pub calls: u64,
}

/// Rolled-up totals for the whole window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    /// Total input tokens across the window.
    pub input_tokens: u64,
    /// Total output tokens across the window.
    pub output_tokens: u64,
    /// Input + output tokens.
    pub tokens: u64,
    /// Total USD cost attributed across the window.
    pub cost_usd: f64,
    /// Total OAuth-connected tool calls across the window.
    pub oauth_calls: u64,
    /// Distinct connected providers seen in the window.
    pub connections: u64,
}

/// The Usage read surface: daily series, per-teammate and per-provider
/// breakdowns, and window totals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// One [`UsagePoint`] per day in the range, oldest first, zero-filled.
    pub series: Vec<UsagePoint>,
    /// Tokens per teammate, highest first.
    pub by_agent: Vec<AgentTokens>,
    /// OAuth calls per provider, highest first.
    pub by_provider: Vec<ProviderCalls>,
    /// Window totals.
    pub totals: UsageTotals,
}

/// Which way money moved for a [`Transaction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Money into the company (revenue).
    In,
    /// Money out of the company (spend).
    Out,
}

/// Spend attributed to one prosumer-facing category.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategorySpend {
    /// The prosumer category label (e.g. `Inference`, `Tools`, `Payments`).
    pub category: String,
    /// The total USD spent in the category this month (positive magnitude).
    pub amount: f64,
}

/// One ledger movement, projected for the console's transactions list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// A stable id derived from the entry's position in the ledger.
    pub id: String,
    /// The entry's UTC calendar day, ISO `YYYY-MM-DD`.
    pub date: String,
    /// A human-readable memo describing the movement.
    pub description: String,
    /// The prosumer category label for the movement.
    pub category: String,
    /// The absolute USD amount (positive magnitude; sign is in `direction`).
    pub amount_usd: f64,
    /// Whether the money came in or went out.
    pub direction: Direction,
}

/// The Finances read surface: balance, budget vs spend, revenue, spend by
/// category, and the transaction journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finances {
    /// The wallet balance (economy wallet when present, else bookkeeping net).
    pub balance_usd: f64,
    /// The monthly budget cap from `[budget].monthly_usd` (0 when uncapped).
    pub budget_usd: f64,
    /// Current-month outgoing spend (positive magnitude).
    pub spent_usd: f64,
    /// Current-month incoming revenue (positive magnitude).
    pub revenue_usd: f64,
    /// Current-month net (`revenue_usd - spent_usd`).
    pub net_usd: f64,
    /// Current-month spend grouped by prosumer category, highest first.
    pub by_category: Vec<CategorySpend>,
    /// Every monetary ledger entry, newest first (WS2 pages this slice).
    pub transactions: Vec<Transaction>,
}
