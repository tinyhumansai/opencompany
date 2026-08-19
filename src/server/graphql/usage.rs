//! The usage read: `Company.usage(range)` over WS5's metering projection.
//!
//! The token/call counts are `u64` in the meter; they are surfaced as `Float`
//! (matching `frontend/src/lib/usage-sample.ts`, which types them as `number`)
//! so large totals never overflow GraphQL's 32-bit `Int`.

use std::sync::Arc;

use async_graphql::{Context, Enum, SimpleObject};

use super::now_millis;
use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::metering::{
    ProviderCalls, Usage, UsagePoint, UsageRange, bucket_usage, roster_display_names,
};

/// The usage lookback window.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Default)]
#[graphql(name = "UsageRange")]
pub enum UsageRangeGql {
    /// The last 7 days.
    D7,
    /// The last 30 days (the default).
    #[default]
    D30,
    /// The last 90 days.
    D90,
}

impl From<UsageRangeGql> for UsageRange {
    fn from(range: UsageRangeGql) -> Self {
        match range {
            UsageRangeGql::D7 => UsageRange::D7,
            UsageRangeGql::D30 => UsageRange::D30,
            UsageRangeGql::D90 => UsageRange::D90,
        }
    }
}

/// One day's token totals.
#[derive(SimpleObject)]
#[graphql(name = "UsagePoint")]
pub struct UsagePointGql {
    /// The day, `YYYY-MM-DD`.
    pub date: String,
    /// Input tokens that day.
    pub input_tokens: f64,
    /// Output tokens that day.
    pub output_tokens: f64,
}

/// Token totals for one teammate.
#[derive(SimpleObject)]
#[graphql(name = "AgentTokens")]
pub struct AgentTokensGql {
    /// The teammate display name.
    pub name: String,
    /// Total tokens attributed to them.
    pub tokens: f64,
    /// Source-currency USD attributed to the teammate, when visible.
    pub cost_usd: Option<f64>,
}

/// Call totals for one provider.
#[derive(SimpleObject)]
#[graphql(name = "ProviderCalls")]
pub struct ProviderCallsGql {
    /// The provider id.
    pub provider: String,
    /// Total calls to it.
    pub calls: f64,
}

/// Aggregate usage totals for the window.
#[derive(SimpleObject)]
#[graphql(name = "UsageTotals")]
pub struct UsageTotalsGql {
    /// Total input tokens.
    pub input_tokens: f64,
    /// Total output tokens.
    pub output_tokens: f64,
    /// Total tokens (input + output).
    pub tokens: f64,
    /// Total inference cost, USD.
    pub cost_usd: Option<f64>,
    /// Total OAuth calls.
    pub oauth_calls: f64,
    /// Number of distinct connections used.
    pub connections: i32,
    /// Total metered web searches (issue #238). Their USD cost is already
    /// inside `cost_usd`; this is the call count.
    pub search_calls: f64,
}

/// The usage read surface for a company.
#[derive(SimpleObject)]
#[graphql(name = "Usage")]
pub struct UsageGql {
    /// The daily token series.
    pub series: Vec<UsagePointGql>,
    /// Tokens by teammate.
    pub by_agent: Vec<AgentTokensGql>,
    /// Calls by provider.
    pub by_provider: Vec<ProviderCallsGql>,
    /// Aggregate totals.
    pub totals: UsageTotalsGql,
    /// Whether positive cost was withheld by the reader's role.
    pub cost_hidden: bool,
}

impl UsageGql {
    fn new(usage: Usage, may_read_cost: bool) -> Self {
        let cost_hidden = !may_read_cost && usage.totals.cost_usd > 0.0;
        Self {
            series: usage.series.into_iter().map(UsagePointGql::from).collect(),
            by_agent: usage
                .by_agent
                .into_iter()
                .map(|agent| AgentTokensGql {
                    name: agent.name,
                    tokens: agent.tokens as f64,
                    cost_usd: may_read_cost.then_some(agent.cost_usd),
                })
                .collect(),
            by_provider: usage
                .by_provider
                .into_iter()
                .map(ProviderCallsGql::from)
                .collect(),
            totals: UsageTotalsGql {
                input_tokens: usage.totals.input_tokens as f64,
                output_tokens: usage.totals.output_tokens as f64,
                tokens: usage.totals.tokens as f64,
                cost_usd: may_read_cost.then_some(usage.totals.cost_usd),
                oauth_calls: usage.totals.oauth_calls as f64,
                connections: usage.totals.connections as i32,
                search_calls: usage.totals.search_calls as f64,
            },
            cost_hidden,
        }
    }
}

impl From<UsagePoint> for UsagePointGql {
    fn from(point: UsagePoint) -> Self {
        Self {
            date: point.date,
            input_tokens: point.input_tokens as f64,
            output_tokens: point.output_tokens as f64,
        }
    }
}

impl From<ProviderCalls> for ProviderCallsGql {
    fn from(provider: ProviderCalls) -> Self {
        Self {
            provider: provider.provider,
            calls: provider.calls as f64,
        }
    }
}

/// Resolves `Company.usage(range)`.
pub(crate) async fn resolve(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
    range: UsageRangeGql,
) -> async_graphql::Result<UsageGql> {
    let _ = ctx.data::<AppState>()?;
    let auth = ctx.data::<crate::server::graphql::auth::GqlAuth>()?;
    let may_read_cost = crate::server::approval_visibility::may_read_approval_contents(auth);
    let range: UsageRange = range.into();
    let now = now_millis();
    let since = now.saturating_sub(range.days().saturating_mul(super::MILLIS_PER_DAY));
    let samples = runtime.usage().query(runtime.id(), since).await?;

    let record = runtime.store().load(runtime.id()).await?;
    let roster = record
        .as_ref()
        .map(|record| roster_display_names(&record.manifest.agents, &record.overlay_agents))
        .unwrap_or_default();

    Ok(UsageGql::new(
        bucket_usage(&samples, range, now, &roster),
        may_read_cost,
    ))
}
