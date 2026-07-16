//! WS5 — Usage & Finances metering.
//!
//! Pure, I/O-free projections that turn the runtime's raw accounting data into
//! the two console read surfaces:
//!
//! - [`bucket_usage`] — [`UsageSample`](crate::ports::usage::UsageSample)s →
//!   [`Usage`] (daily token series, tokens by teammate, calls by provider,
//!   totals) over a 7/30/90-day [`UsageRange`].
//! - [`finances_from`] — the ledger + `[budget]` + optional economy wallet
//!   balance → [`Finances`] (balance, budget vs spend, revenue, spend by
//!   category, the transaction journal).
//!
//! WS2 owns the async-graphql wrappers (`graphql/usage.rs`,
//! `graphql/finances.rs`); this module deliberately has no async-graphql
//! dependency so the projections can be unit-tested against seeded data and
//! land ahead of the real cost-hook stream.

use std::collections::HashMap;

use crate::company::Agent;
use crate::ports::types::OverlayAgent;

mod calendar;
mod finances;
mod types;
mod usage;

pub use finances::{category_label, finances_from};
pub use types::{
    AgentTokens, CategorySpend, Direction, Finances, ProviderCalls, Transaction, Usage, UsagePoint,
    UsageRange, UsageTotals,
};
pub use usage::bucket_usage;

/// Builds the teammate id → display-name map [`bucket_usage`] resolves against,
/// in prosumer language.
///
/// Manifest teammates have no explicit display name, so their job title
/// ([`Agent::role`]) is used; operator-added overlay teammates
/// ([`OverlayAgent::name`]) override by id. Any id absent from the map falls
/// back to the raw id at bucket time.
pub fn roster_display_names(agents: &[Agent], overlay: &[OverlayAgent]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for agent in agents {
        map.insert(agent.id.clone(), agent.role.clone());
    }
    for member in overlay {
        map.insert(member.id.clone(), member.name.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_uses_role_then_overlay_name() {
        let agents = vec![
            Agent {
                id: "strategy".into(),
                role: "Strategy desk".into(),
                description: None,
                tier: None,
                tools: vec![],
                budget_usd_daily: None,
            },
            Agent {
                id: "creative".into(),
                role: "Creative studio".into(),
                description: None,
                tier: None,
                tools: vec![],
                budget_usd_daily: None,
            },
        ];
        let overlay = vec![OverlayAgent {
            id: "creative".into(),
            name: "Creative studio (renamed)".into(),
            role: "Creative".into(),
            description: None,
        }];
        let map = roster_display_names(&agents, &overlay);
        assert_eq!(map.get("strategy").unwrap(), "Strategy desk");
        // Overlay name overrides the manifest role for the same id.
        assert_eq!(map.get("creative").unwrap(), "Creative studio (renamed)");
    }
}
