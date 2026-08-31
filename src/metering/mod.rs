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
//! The write-side pieces sit here rather than at their (feature-gated) call
//! sites, so their contracts are compiled and tested by the default CI build —
//! see each module's docs:
//!
//! - [`oauth`] mints the
//!   [`SampleKind::OauthCall`](crate::ports::usage::SampleKind) samples
//!   [`bucket_usage`] turns into the calls-by-provider chart.
//! - [`search`] mints the
//!   [`SampleKind::SearchCall`](crate::ports::usage::SampleKind) samples behind
//!   the `searchCalls` counter and the cost the managed search backend charged
//!   (issue #238) — a *priced* call, deliberately not folded into the
//!   zero-cost, connection-minting `OauthCall` stream.
//! - [`inference`] mints the
//!   [`SampleKind::Inference`](crate::ports::usage::SampleKind) samples behind
//!   the token series and the token/cost totals, for **every** cognition path
//!   rather than only the `openhuman` harness (issue #174).
//! - [`planning`] mints the
//!   [`SampleKind::PlanningCall`](crate::ports::usage::SampleKind) samples one
//!   planning pass produces (issue #337) — charged to the whole-company bucket
//!   rather than to the card's assignee, because planning is frequently what
//!   *picks* the assignee and because a teammate at its daily cap must not be
//!   unable to have work planned for it.
//!
//! WS2 owns the async-graphql wrappers (`graphql/usage.rs`,
//! `graphql/finances.rs`); this module deliberately has no async-graphql
//! dependency so the projections can be unit-tested against seeded data and
//! land ahead of the real cost-hook stream.

use std::collections::HashMap;

use crate::company::Agent;
use crate::ports::types::OverlayAgent;

mod calendar;
pub mod capability;
pub mod daily_budget;
mod finances;
pub mod inference;
/// Issue #1749: [`ModelSlug`], the closed vocabulary a metered sample names its
/// model in. See [`model`].
pub mod model;
pub mod oauth;
/// Issue #337: the planning pass's usage sample and its company-bucket
/// attribution rule. See [`planning`].
pub mod planning;
/// First-run company setup's usage sample and its company-bucket attribution
/// rule (a sibling of [`planning`], not of an agent turn — the pass runs before
/// the roster it is building exists). See [`roster_build`].
/// Issue #1776: what one drafted teammate mandate or persona costs, charged to
/// the company rather than to the teammate it describes. See [`profile_draft`].
pub mod profile_draft;
pub mod roster_build;
pub mod search;
pub mod selector;
pub mod triage;
mod types;
mod usage;
/// Issue #580: the workflow-builder pass's usage sample and its
/// assignee/run attribution rule (the mirror image of [`planning`]). See
/// [`workflow_build`].
pub mod workflow_build;

pub use capability::{BudgetPeriod, CapabilityPlan, TierBudgetStatus, plan_named, tokens_in};
pub use daily_budget::{AgentBudgetStatus, usd_spent_by_agent, utc_day_start_millis};
pub use finances::{category_label, finances_from};
pub use inference::{
    INFERENCE_SPEND_KIND, MEDULLA_PROVIDER, UNATTRIBUTED_AGENT, inference_ledger_entry,
    inference_sample, record_inference_usage,
};
pub use model::ModelSlug;
pub use oauth::{
    MCP_PROVIDER_PREFIX, UNKNOWN_PROVIDER, mcp_provider, oauth_call_sample, record_oauth_call,
};
pub use planning::{planning_sample, record_planning_usage};
pub use profile_draft::{
    DraftBudget, profile_draft_sample, record_profile_draft_usage, reserve_draft,
};
pub use search::{
    FALLBACK_SEARCH_COST_USD, MANAGED_SEARCH_PROVIDER, record_search_call, search_call_sample,
};
pub use selector::{record_selector_usage, selector_sample};
pub use triage::{record_triage_usage, triage_sample};
pub use types::{
    AgentTokens, CategorySpend, Direction, Finances, ProviderCalls, Transaction, Usage, UsagePoint,
    UsageRange, UsageTotals,
};
pub use usage::bucket_usage;
pub use workflow_build::{record_workflow_build_usage, workflow_build_sample};

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
        // An operator-set name wins over the job title when one exists — a
        // manifest teammate carries `name` only once somebody has renamed it
        // from the console.
        map.insert(
            agent.id.clone(),
            agent.name.clone().unwrap_or_else(|| agent.role.clone()),
        );
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
                global: false,
                id: "strategy".into(),
                role: "Strategy desk".into(),
                name: None,
                description: None,
                tier: None,
                harness: None,
                tools: None,
                delegates_to: vec![],
                context: None,
                budget_usd_daily: None,
                prompt: None,
                prompt_files: Vec::new(),
                prompt_files_resolved: Vec::new(),
                classes: Vec::new(),
                ledgers: None,
                can_declare_ledgers: true,
                model: None,
            },
            Agent {
                global: false,
                id: "creative".into(),
                role: "Creative studio".into(),
                name: None,
                description: None,
                tier: None,
                harness: None,
                tools: None,
                delegates_to: vec![],
                context: None,
                budget_usd_daily: None,
                prompt: None,
                prompt_files: Vec::new(),
                prompt_files_resolved: Vec::new(),
                classes: Vec::new(),
                ledgers: None,
                can_declare_ledgers: true,
                model: None,
            },
        ];
        let overlay = vec![OverlayAgent {
            id: "creative".into(),
            name: "Creative studio (renamed)".into(),
            role: "Creative".into(),
            description: None,
            tools: None,
            model: None,
            harness: None,
        }];
        let map = roster_display_names(&agents, &overlay);
        assert_eq!(map.get("strategy").unwrap(), "Strategy desk");
        // Overlay name overrides the manifest role for the same id.
        assert_eq!(map.get("creative").unwrap(), "Creative studio (renamed)");
    }
}
