//! Pure per-agent daily-spend math (issue #304): sum one teammate's metered
//! spend over a window, and find the UTC-midnight boundary that window starts
//! at.
//!
//! This is the I/O-free half of the per-agent `budget_usd_daily` cap — it lives
//! in `metering` (always compiled, beside [`capability`](super::capability)) so
//! the console read surface and the feature-gated harness share one definition
//! of "spent today". The harness halves add the policy arm
//! ([`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy)) and the dispatch
//! gate ([`HarnessPool::run`](crate::harness::HarnessPool::run)) on top.
//!
//! ## "Daily" means the UTC calendar day
//!
//! The window resets at `00:00Z`, delegating to
//! [`BudgetPeriod::Daily`](super::BudgetPeriod) rather than re-deriving the
//! boundary — the capability plan's daily budget, the search daily-call cap and
//! this cap must all roll over at the same instant, and two clocks that agree
//! today drift the moment one of them grows a special case. A test pins the two
//! to the same value for exactly that reason.
//!
//! A rolling 24-hour window was the alternative and is deliberately not what
//! this is: an operator reading "$5/day" against a console that renders calendar
//! days has no way to reason about a cap that resets at a different time for
//! every agent.
//!
//! ## What the sum covers, and what it misses
//!
//! [`usd_spent_by_agent`] sums `cost_usd` over **every** [`SampleKind`] — a
//! model turn ([`Inference`](crate::ports::SampleKind::Inference)), a metered
//! web search ([`SearchCall`](crate::ports::SampleKind::SearchCall)), a
//! connected-tool call ([`OauthCall`](crate::ports::SampleKind::OauthCall),
//! which is zero-cost by definition and so contributes nothing). Filtering to
//! one kind would make the cap mean "part of what this teammate spent", which
//! is not a cap anyone can reason about.
//!
//! It does **not** see executed x402 payments. Those land on the ledger, and
//! [`LedgerEntry`](crate::ports::types::LedgerEntry) carries no agent — there is
//! no attribution to sum. The gate covers the *pre-flight* case instead (a tool
//! call that declares an `amount_usd` which would breach the remaining budget
//! parks for approval before the money moves), and the executed-payment gap is
//! stated plainly in `docs/spec/runtime/manifest.md`. Closing it is a
//! store-shape change across three backends, not something this module can
//! paper over.

use crate::ports::UsageSample;

use super::capability::BudgetPeriod;

/// Total USD a single teammate is metered as having spent across `samples`.
///
/// Sums `cost_usd` for every sample whose [`agent`](UsageSample::agent) matches,
/// across all [`SampleKind`](crate::ports::SampleKind)s. Callers scope the
/// window by what they pass — typically a
/// [`UsageMeter::query`](crate::ports::UsageMeter::query) anchored at
/// [`utc_day_start_millis`].
///
/// An empty window, or an agent with no samples in it, is **positive** `0.0` —
/// never an error, and never `-0.0`. "I have spent nothing today" and "I have
/// no record of spending today" are the same statement to a cap.
///
/// The fold seeds at `0.0` rather than using `Iterator::sum`, which is not
/// stylistic: `<f64 as Sum>` seeds at `-0.0` (the additive identity that
/// survives `-0.0 + -0.0`), so summing an empty window through it yields
/// `-0.0`. That value compares equal to zero in Rust and so passes every gate
/// here unnoticed — but it serialises as `-0.0` onto the wire, and the console
/// renders `(-0).toFixed(2)` as `"-0.00"`. An operator would read "$-0.00 spent
/// today" on every capped teammate that had not yet spent anything. Caught by
/// clicking the Team page, not by the unit tests, which is why one is pinned
/// below.
pub fn usd_spent_by_agent(samples: &[UsageSample], agent: &str) -> f64 {
    samples
        .iter()
        .filter(|sample| sample.agent == agent)
        .map(|sample| sample.cost_usd)
        .fold(0.0, |total, cost| total + cost)
}

/// The epoch-millis start (`00:00Z`) of the UTC calendar day `now` falls in —
/// the `since` a daily-spend query is anchored to.
///
/// Delegates to [`BudgetPeriod::Daily`] so this cap and the capability plan's
/// daily budget can never roll over at different instants.
pub fn utc_day_start_millis(now: u64) -> u64 {
    BudgetPeriod::Daily.period_start_millis(now)
}

/// One teammate's daily-budget status for the console read surface, mirroring
/// [`TierBudgetStatus`](super::TierBudgetStatus) in shape.
///
/// Only produced for a teammate that actually carries a cap: an uncapped
/// teammate has no row, which is what lets the console tell "spends freely"
/// apart from "capped and has spent nothing".
#[derive(Clone, Debug, PartialEq)]
pub struct AgentBudgetStatus {
    /// The teammate id the cap belongs to.
    pub agent: String,
    /// The manifest `budget_usd_daily` cap.
    pub budget_usd: f64,
    /// What this teammate has spent since UTC midnight.
    pub spent_usd: f64,
    /// `budget_usd - spent_usd`, floored at zero.
    pub remaining_usd: f64,
    /// Whether spend has reached the cap (`spent >= budget`) — the boundary the
    /// harness gate and the policy arm both trip on.
    pub exhausted: bool,
}

impl AgentBudgetStatus {
    /// Builds a status row from a teammate's cap and its spend since UTC
    /// midnight.
    pub fn new(agent: impl Into<String>, budget_usd: f64, spent_usd: f64) -> Self {
        Self {
            agent: agent.into(),
            budget_usd,
            spent_usd,
            remaining_usd: (budget_usd - spent_usd).max(0.0),
            exhausted: spent_usd >= budget_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::calendar::{MILLIS_PER_DAY, days_from_civil};
    use crate::ports::{SampleKind, UsageSample};

    fn sample(agent: &str, cost: f64, kind: SampleKind) -> UsageSample {
        UsageSample {
            at_millis: 0,
            agent: agent.into(),
            provider: "managed".into(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cost_usd: cost,
            kind,
            run_id: None,
            model: None,
        }
    }

    /// The sum is per-agent: another teammate's spend never counts against this
    /// one's cap. Per-agent caps that leaked into each other would make a busy
    /// desk silently starve a quiet one.
    #[test]
    fn spend_is_attributed_to_one_agent() {
        let samples = vec![
            sample("analyst", 1.50, SampleKind::Inference),
            sample("writer", 9.00, SampleKind::Inference),
            sample("analyst", 0.25, SampleKind::Inference),
        ];
        assert!((usd_spent_by_agent(&samples, "analyst") - 1.75).abs() < f64::EPSILON);
        assert!((usd_spent_by_agent(&samples, "writer") - 9.00).abs() < f64::EPSILON);
    }

    /// Every metered kind counts. A cap that only saw inference would let a
    /// teammate spend its whole day on searches and report `$0.00 spent`.
    #[test]
    fn spend_sums_across_every_sample_kind() {
        let samples = vec![
            sample("analyst", 2.00, SampleKind::Inference),
            sample("analyst", 0.50, SampleKind::SearchCall),
            // An OAuth call is zero-cost by definition — included, contributes
            // nothing, and must not be *excluded* on the assumption it always
            // will be.
            sample("analyst", 0.00, SampleKind::OauthCall),
        ];
        assert!((usd_spent_by_agent(&samples, "analyst") - 2.50).abs() < f64::EPSILON);
    }

    /// No samples, or none for this agent, is zero — never an error.
    ///
    /// And specifically **positive** zero. `assert_eq!(x, 0.0)` passes for
    /// `-0.0` too, so the sign is checked through `is_sign_positive`: the whole
    /// bug this pins is invisible to an equality assertion and only shows up
    /// once the value is serialised and formatted by the console.
    #[test]
    fn an_empty_window_is_positive_zero() {
        for spent in [
            usd_spent_by_agent(&[], "analyst"),
            usd_spent_by_agent(&[sample("writer", 4.0, SampleKind::Inference)], "analyst"),
        ] {
            assert_eq!(spent, 0.0);
            assert!(
                spent.is_sign_positive(),
                "an unspent budget must serialise as 0.0, not -0.0 — the console \
                 renders the latter as \"$-0.00 spent today\""
            );
        }
    }

    /// The load-bearing pin: this cap's day boundary IS
    /// [`BudgetPeriod::Daily`]'s. If these two ever diverge, a company's
    /// capability budget and its per-agent spend caps reset at different
    /// instants and no operator can reason about either.
    #[test]
    fn the_day_boundary_is_the_capability_plans_day_boundary() {
        let noon = (days_from_civil(2026, 8, 4) as u64) * MILLIS_PER_DAY + 12 * 3_600_000;
        assert_eq!(
            utc_day_start_millis(noon),
            BudgetPeriod::Daily.period_start_millis(noon),
        );
        // ...and it is midnight UTC of that same day.
        assert_eq!(
            utc_day_start_millis(noon),
            (days_from_civil(2026, 8, 4) as u64) * MILLIS_PER_DAY
        );
        // Midnight maps to itself.
        let midnight = utc_day_start_millis(noon);
        assert_eq!(utc_day_start_millis(midnight), midnight);
    }

    /// The console row: remaining floors at zero and `exhausted` trips on `>=`,
    /// matching the boundary the harness gate and the policy arm use.
    #[test]
    fn status_floors_remaining_and_trips_on_reaching_the_cap() {
        let under = AgentBudgetStatus::new("analyst", 5.0, 1.25);
        assert!((under.remaining_usd - 3.75).abs() < f64::EPSILON);
        assert!(!under.exhausted);

        let exactly_at = AgentBudgetStatus::new("analyst", 5.0, 5.0);
        assert_eq!(exactly_at.remaining_usd, 0.0);
        assert!(exactly_at.exhausted, "the boundary is `>=`, not `>`");

        let over = AgentBudgetStatus::new("analyst", 5.0, 7.5);
        assert_eq!(over.remaining_usd, 0.0, "remaining never goes negative");
        assert!(over.exhausted);
    }
}
