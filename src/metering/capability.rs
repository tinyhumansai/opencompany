//! Pure capability-budget math (issue #108): resolve a manifest `[plan]` into a
//! per-namespace token budget, sum a tenant's period spend, and decide which
//! exec tool families that spend has exhausted.
//!
//! This is the I/O-free half of the capability-tier gate — it lives in
//! `metering` (always compiled, beside [`usage`](super::usage) /
//! [`finances`](super::finances)) so both the console read surface and the
//! feature-gated harness can share it. The harness half
//! ([`capability_budget`](crate::harness::capability_budget)) adds the
//! `CapabilityFilter` wiring on top and re-exports these types.
//!
//! ## Threshold semantics (important)
//!
//! [`UsageSample`]s are **per-turn totals** — the meter records the tokens a turn
//! burned, with **no per-tool-namespace attribution**. So each tier's budget is a
//! **threshold over the tenant's total token spend this period**: when cumulative
//! spend reaches a tier's budget, that tier's tools are disabled. Different
//! budgets per tier give **graduated degradation** (the cheapest tier drops
//! first). Per-tier *attribution* is out of scope; if it ever lands, the sample
//! shape — not this module — is what must change first.

use std::collections::{BTreeMap, HashSet};

use crate::company::{GATEABLE_NAMESPACES, Plan};
use crate::ports::{SampleKind, UsageSample};

use super::calendar;

/// The window a [`CapabilityPlan`]'s budgets are measured over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetPeriod {
    /// The current UTC calendar day (`00:00Z`-aligned).
    Daily,
    /// The current UTC calendar month (the 1st, `00:00Z`).
    Monthly,
}

impl BudgetPeriod {
    /// Parses the manifest `[plan].period` string; `None` for an unknown value
    /// (the manifest validator rejects those before a company boots).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "daily" | "day" => Some(BudgetPeriod::Daily),
            "monthly" | "month" => Some(BudgetPeriod::Monthly),
            _ => None,
        }
    }

    /// The console-facing label.
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetPeriod::Daily => "daily",
            BudgetPeriod::Monthly => "monthly",
        }
    }

    /// The epoch-millis start of the period `now` falls in — the `since` a spend
    /// query is anchored to.
    pub fn period_start_millis(self, now: u64) -> u64 {
        match self {
            BudgetPeriod::Daily => {
                (calendar::epoch_day(now).max(0) as u64) * calendar::MILLIS_PER_DAY
            }
            BudgetPeriod::Monthly => calendar::month_start_millis(now),
        }
    }
}

/// A tenant's capability plan: a budget window plus a per-namespace token
/// threshold map.
///
/// The `budgets` **key set is the capability set** — a gateable namespace
/// (`shell` / `code` / `web` / `subagent`) present in the map is granted until
/// the tenant's period spend reaches its value; a gateable namespace *absent*
/// from the map is always denied. The value is a token count per
/// [`BudgetPeriod`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityPlan {
    /// The window `budgets` are measured over.
    pub period: BudgetPeriod,
    /// Gateable namespace → tokens allowed per period. `u64::MAX` is effectively
    /// unlimited.
    pub budgets: BTreeMap<String, u64>,
    /// Plan-level **total token ceiling** for the period (issue #188), or `None`
    /// when no ceiling is set.
    ///
    /// The `budgets` map is a **soft** gate — it only trims *which* exec tool
    /// families a turn may reach; an exhausted namespace's tools drop off the
    /// roster but the turn still runs on intrinsic tools and burns model tokens.
    /// This is the **hard** gate: once the tenant's total period spend reaches
    /// this value, dispatch is refused outright (no model call) until the period
    /// resets. Carried from the manifest `[plan].total_tokens`; the built-in
    /// named tiers leave it `None` (a manifest overlays it explicitly).
    pub total_budget: Option<u64>,
}

impl CapabilityPlan {
    /// Resolves the manifest `[plan]` section into a runtime plan, or `None` when
    /// no plan is configured (gating stays off — byte-identical to Cell A).
    ///
    /// A built-in tier name (`plan.name`) supplies the base budget map; the
    /// explicit `token_budgets` table then overrides/extends it, and the `period`
    /// field selects the window. The manifest validator has already rejected an
    /// unknown name / period / non-gateable budget key, so this is infallible in
    /// a booted company; an unknown name still degrades to `None` (gating off)
    /// rather than panicking.
    pub fn from_manifest(plan: &Plan) -> Option<Self> {
        if !plan.is_set() {
            return None;
        }
        let mut resolved = match plan.name.as_deref() {
            Some(name) => plan_named(name)?,
            None => CapabilityPlan {
                period: BudgetPeriod::Daily,
                budgets: BTreeMap::new(),
                total_budget: None,
            },
        };
        // The manifest `period` field is authoritative for the window (defaults
        // to daily, which is also every built-in tier's window).
        resolved.period = BudgetPeriod::parse(&plan.period).unwrap_or(BudgetPeriod::Daily);
        // Explicit budgets override a named tier's entry and extend it with new
        // namespaces.
        for (namespace, tokens) in &plan.token_budgets {
            resolved.budgets.insert(namespace.clone(), *tokens);
        }
        // The plan-level total ceiling (issue #188) is a manifest-only overlay —
        // the built-in tiers carry none, so `total_tokens` is authoritative when
        // present and leaves the hard gate off when absent.
        resolved.total_budget = plan.total_tokens;
        Some(resolved)
    }

    /// Whether the tenant's total period `spent` has reached the plan-level token
    /// ceiling (issue #188). `false` when no ceiling is configured (the hard gate
    /// is off) or when spend is still under it. The boundary is `>=`, matching the
    /// per-namespace [`denied_namespaces`](Self::denied_namespaces) exhaustion so
    /// the total gate trips exactly when a namespace budget equal to it would.
    pub fn total_exhausted(&self, spent: u64) -> bool {
        self.total_budget.is_some_and(|cap| spent >= cap)
    }

    /// The plan-level total-budget status row for the console read surface (issue
    /// #188), or `None` when no total ceiling is configured. Mirrors
    /// [`TierBudgetStatus`] but describes the whole-period ceiling, not one
    /// namespace.
    pub fn total_status(&self, spent: u64) -> Option<TotalBudgetStatus> {
        self.total_budget.map(|budget| TotalBudgetStatus {
            budget,
            spent,
            remaining: budget.saturating_sub(spent),
            exhausted: spent >= budget,
        })
    }

    /// The gateable namespaces this plan denies at `spent` tokens: every gateable
    /// namespace absent from the budget map, plus every mapped namespace whose
    /// budget the spend has **reached or passed** (`spent >= budget`).
    pub fn denied_namespaces(&self, spent: u64) -> HashSet<&'static str> {
        GATEABLE_NAMESPACES
            .iter()
            .copied()
            .filter(|namespace| match self.budgets.get(*namespace) {
                Some(budget) => spent >= *budget,
                None => true,
            })
            .collect()
    }

    /// The per-tier status rows the console renders: one per configured budget,
    /// in stable (namespace-sorted, via [`BTreeMap`]) order. Because spend is a
    /// per-turn total with no per-namespace attribution, every row's `spent` is
    /// the same tenant-wide figure compared against that tier's own threshold.
    pub fn status(&self, spent: u64) -> Vec<TierBudgetStatus> {
        self.budgets
            .iter()
            .map(|(namespace, &budget)| TierBudgetStatus {
                namespace: namespace.clone(),
                budget,
                spent,
                remaining: budget.saturating_sub(spent),
                exhausted: spent >= budget,
            })
            .collect()
    }
}

/// A built-in plan tier, or `None` for an unknown name.
///
/// * `free` — no exec tiers (every gateable namespace denied).
/// * `starter` — `shell` + `code` at 200k tokens/day.
/// * `pro` — `shell` + `code` + `web` at 1M tokens/day.
/// * `unlimited` — every gateable namespace at `u64::MAX` (effectively
///   uncapped), including the real-money `media` tier (issue #109), the
///   per-tenant `composio` tier (issue #110), the metered `search` tier
///   (issue #238) and the bound-repository `repo` tier (issue #245). Those four
///   are absent from `free` / `starter` / `pro`, so those tiers deny them
///   outright unless the manifest opts in with an explicit
///   `token_budgets = { media = N }` / `{ composio = N }` / `{ search = N }` /
///   `{ repo = N }`.
///
/// A `repo` token budget behaves like the `search` one below, and for a related
/// reason: a checkout costs disk and network rather than tokens, so shedding it
/// on token spend is a blunt cross-subsidy gate. The real ceiling on a checkout
/// is the company's `[workspace].tree_quota_gb`, enforced as a refusal before
/// the clone.
///
/// Note what a `search` *token* budget does and does not do: it sheds the tool
/// once the company's period **token** spend crosses the threshold, which is a
/// blunt cross-subsidy gate. The real ceiling on search spend is the per-company
/// **daily call cap** (`[tools].search_daily_calls`), because a search costs
/// money per call and no tokens at all.
///
/// Every built-in is daily; the manifest `[plan].period` can widen the window.
pub fn plan_named(name: &str) -> Option<CapabilityPlan> {
    let budgets: &[(&str, u64)] = match name.trim().to_ascii_lowercase().as_str() {
        "free" => &[],
        "starter" => &[("shell", 200_000), ("code", 200_000)],
        "pro" => &[
            ("shell", 1_000_000),
            ("code", 1_000_000),
            ("web", 1_000_000),
        ],
        "unlimited" => &[
            ("shell", u64::MAX),
            ("code", u64::MAX),
            ("web", u64::MAX),
            ("subagent", u64::MAX),
            ("media", u64::MAX),
            ("composio", u64::MAX),
            ("search", u64::MAX),
            ("repo", u64::MAX),
        ],
        _ => return None,
    };
    Some(CapabilityPlan {
        period: BudgetPeriod::Daily,
        budgets: budgets
            .iter()
            .map(|(ns, tokens)| ((*ns).to_string(), *tokens))
            .collect(),
        // The plan-level total ceiling (issue #188) is opt-in via the manifest
        // `[plan].total_tokens` overlay only — a named tier never sets it.
        total_budget: None,
    })
}

/// Total model tokens (input + output) across a sample window — every kind that
/// actually moves tokens.
///
/// [`SampleKind::OauthCall`] carries no token spend and
/// [`SampleKind::SearchCall`] is a priced request rather than a completion, so
/// neither can inflate the token budget; a tool-heavy turn is not a token-heavy
/// one.
///
/// [`SampleKind::PlanningCall`] **is** counted (issue #337). A planning pass is
/// a real completion against the tenant's own inference budget; it is only
/// filed under a different kind because it belongs to the company rather than
/// to a teammate. Excluding it would leave a company able to plan indefinitely
/// after crossing the tier ceiling that is supposed to have stopped it.
///
/// [`SampleKind::TriageCall`] is counted for the identical reason (issue #678),
/// and the leak it closes is larger: triage escalations are driven by *chat
/// volume*, so a company left able to classify indefinitely past its ceiling
/// would keep paying per operator message with nothing to stop it.
///
/// [`SampleKind::SelectorCall`] is counted on `TriageCall`'s exact terms
/// (issue #1835): per-message company-driven spend, uncappable by any
/// teammate's budget, that must stop when the tier ceiling does.
///
/// [`SampleKind::SetupCall`] is counted too, and its exposure is the smallest of
/// the three: a company runs first-run setup once. It is included so the ceiling
/// covers every completion billed to the tenant rather than only the ones that
/// happen to belong to a teammate.
///
/// [`SampleKind::AuthoringCall`] is counted on the same principle (issue #1776).
/// Drafting a teammate's mandate or persona is a real completion the tenant
/// pays for, and it is operator-driven and repeatable — an excluded kind would
/// let someone keep pressing Draft after the ceiling that is supposed to have
/// stopped them.
pub fn tokens_in(samples: &[UsageSample]) -> u64 {
    samples
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SampleKind::Inference
                    | SampleKind::PlanningCall
                    | SampleKind::TriageCall
                    | SampleKind::SelectorCall
                    | SampleKind::SetupCall
                    | SampleKind::AuthoringCall
            )
        })
        .map(|s| s.input_tokens.saturating_add(s.output_tokens))
        .fold(0u64, |acc, t| acc.saturating_add(t))
}

/// One tier's budget status for the console read surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierBudgetStatus {
    /// The gateable namespace this tier gates (`shell` / `code` / `web` /
    /// `subagent`).
    pub namespace: String,
    /// The token threshold for the period.
    pub budget: u64,
    /// The tenant's total period spend (same across rows — no per-tier
    /// attribution).
    pub spent: u64,
    /// `budget - spent`, saturating at zero.
    pub remaining: u64,
    /// Whether spend has reached the threshold (`spent >= budget`) — the tier's
    /// tools are disabled.
    pub exhausted: bool,
}

/// The plan-level total-budget status for the console read surface (issue #188).
///
/// Unlike [`TierBudgetStatus`] — a per-namespace **soft** gate that only trims a
/// tool family — `exhausted` here means the tenant has crossed the **hard**
/// ceiling and the harness will refuse to dispatch further turns this period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TotalBudgetStatus {
    /// The total token ceiling for the period.
    pub budget: u64,
    /// The tenant's total period spend.
    pub spent: u64,
    /// `budget - spent`, saturating at zero.
    pub remaining: u64,
    /// Whether spend has reached the ceiling (`spent >= budget`) — dispatch is
    /// refused until the period resets.
    pub exhausted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::calendar::{MILLIS_PER_DAY, days_from_civil};

    fn inference_sample(input: u64, output: u64) -> UsageSample {
        UsageSample {
            at_millis: 0,
            agent: "ceo".into(),
            provider: "managed".into(),
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
            cost_usd: 0.0,
            kind: SampleKind::Inference,
            run_id: None,
            model: None,
        }
    }

    fn oauth_sample() -> UsageSample {
        UsageSample {
            at_millis: 0,
            agent: "ceo".into(),
            provider: "github".into(),
            input_tokens: 999,
            output_tokens: 999,
            cached_input_tokens: 0,
            cost_usd: 0.0,
            kind: SampleKind::OauthCall,
            run_id: None,
            model: None,
        }
    }

    // --- period boundaries --------------------------------------------------

    #[test]
    fn daily_period_start_snaps_to_midnight_utc() {
        let day = days_from_civil(2026, 7, 16) as u64;
        let noon = day * MILLIS_PER_DAY + 12 * 3_600_000;
        assert_eq!(
            BudgetPeriod::Daily.period_start_millis(noon),
            day * MILLIS_PER_DAY
        );
        assert_eq!(
            BudgetPeriod::Daily.period_start_millis(day * MILLIS_PER_DAY),
            day * MILLIS_PER_DAY
        );
    }

    #[test]
    fn monthly_period_start_snaps_to_the_first() {
        let mid = (days_from_civil(2026, 7, 16) as u64) * MILLIS_PER_DAY + 12 * 3_600_000;
        let first = (days_from_civil(2026, 7, 1) as u64) * MILLIS_PER_DAY;
        assert_eq!(BudgetPeriod::Monthly.period_start_millis(mid), first);
    }

    #[test]
    fn period_parse_round_trips_and_rejects_unknown() {
        assert_eq!(BudgetPeriod::parse("daily"), Some(BudgetPeriod::Daily));
        assert_eq!(BudgetPeriod::parse("Monthly"), Some(BudgetPeriod::Monthly));
        assert_eq!(BudgetPeriod::parse("hourly"), None);
    }

    // --- tokens_in ----------------------------------------------------------

    #[test]
    fn tokens_in_sums_inference_and_ignores_oauth() {
        let samples = vec![
            inference_sample(100, 50),
            oauth_sample(),
            inference_sample(20, 5),
        ];
        assert_eq!(tokens_in(&samples), 175);
    }

    #[test]
    fn tokens_in_empty_is_zero() {
        assert_eq!(tokens_in(&[]), 0);
    }

    /// Issue #337: a planning pass spends the tenant's inference budget just as
    /// a teammate's turn does, so it counts toward the tier ceiling. Without
    /// this a company could keep planning after the budget that was supposed to
    /// stop it had been exhausted — the tokens are spent either way, and a
    /// ceiling that only some completions respect is not a ceiling.
    #[test]
    fn tokens_in_counts_planning_passes() {
        let planning = UsageSample {
            kind: SampleKind::PlanningCall,
            agent: crate::metering::UNATTRIBUTED_AGENT.into(),
            ..inference_sample(300, 100)
        };
        assert_eq!(tokens_in(std::slice::from_ref(&planning)), 400);
        // And it adds to a teammate's, rather than replacing or shadowing it.
        assert_eq!(
            tokens_in(&[inference_sample(100, 50), planning, oauth_sample()]),
            550
        );
    }

    // --- exhaustion / denial ------------------------------------------------

    #[test]
    fn ge_boundary_exhausts_the_tier() {
        let plan = plan_named("starter").unwrap();
        // Under budget, the *mapped* tiers (shell/code) are granted. web/subagent
        // are absent from starter's map, so they are always denied — assert on the
        // mapped tiers specifically, not on emptiness.
        let under = plan.denied_namespaces(199_999);
        assert!(!under.contains("shell") && !under.contains("code"));
        // Exactly at budget: both mapped tiers exhausted (>= boundary).
        let at = plan.denied_namespaces(200_000);
        assert!(at.contains("shell") && at.contains("code"));
        // Over budget: still denied.
        assert!(plan.denied_namespaces(200_001).contains("shell"));
    }

    #[test]
    fn absent_namespace_is_always_denied() {
        let plan = plan_named("starter").unwrap();
        let denied = plan.denied_namespaces(0);
        assert!(denied.contains("web"), "web absent from map → denied");
        assert!(denied.contains("subagent"), "subagent absent → denied");
        assert!(!denied.contains("shell"), "shell granted under budget");
    }

    #[test]
    fn zero_budget_namespace_is_denied_from_the_first_token() {
        let mut plan = plan_named("free").unwrap();
        plan.budgets.insert("shell".into(), 0);
        assert!(plan.denied_namespaces(0).contains("shell"));
    }

    #[test]
    fn free_plan_denies_every_gateable_namespace() {
        let plan = plan_named("free").unwrap();
        let denied = plan.denied_namespaces(0);
        for ns in GATEABLE_NAMESPACES {
            assert!(denied.contains(ns), "free must deny {ns}");
        }
    }

    #[test]
    fn unlimited_plan_grants_everything() {
        let plan = plan_named("unlimited").unwrap();
        assert!(plan.denied_namespaces(u64::MAX - 1).is_empty());
    }

    /// The real-money `media` tier (issue #109) is uncapped under `unlimited`
    /// but denied by every other built-in tier — a company must opt into it via
    /// an explicit `token_budgets = { media = N }`, never a wildcard.
    #[test]
    fn media_tier_is_unlimited_only_and_denied_elsewhere() {
        assert!(
            !plan_named("unlimited")
                .unwrap()
                .denied_namespaces(0)
                .contains("media"),
            "unlimited grants media"
        );
        for tier in ["free", "starter", "pro"] {
            assert!(
                plan_named(tier)
                    .unwrap()
                    .denied_namespaces(0)
                    .contains("media"),
                "{tier} must deny the real-money media tier by default"
            );
        }
    }

    /// The per-tenant `composio` tier (issue #110) is uncapped under `unlimited`
    /// but denied by every other built-in tier — a company opts into it via an
    /// explicit `token_budgets = { composio = N }`, never a wildcard.
    #[test]
    fn composio_tier_is_unlimited_only_and_denied_elsewhere() {
        assert!(
            !plan_named("unlimited")
                .unwrap()
                .denied_namespaces(0)
                .contains("composio"),
            "unlimited grants composio"
        );
        for tier in ["free", "starter", "pro"] {
            assert!(
                plan_named(tier)
                    .unwrap()
                    .denied_namespaces(0)
                    .contains("composio"),
                "{tier} must deny the composio tier by default"
            );
        }
    }

    /// The metered `search` tier (issue #238) follows media/composio: uncapped
    /// under `unlimited`, denied by every other built-in tier unless the
    /// manifest opts in with `token_budgets = { search = N }`. Every search is
    /// a priced request, so a company should not inherit one from a tier it
    /// chose for its token allowance.
    #[test]
    fn search_tier_is_unlimited_only_and_denied_elsewhere() {
        assert!(
            !plan_named("unlimited")
                .unwrap()
                .denied_namespaces(0)
                .contains("search"),
            "unlimited grants search"
        );
        for tier in ["free", "starter", "pro"] {
            assert!(
                plan_named(tier)
                    .unwrap()
                    .denied_namespaces(0)
                    .contains("search"),
                "{tier} must deny the metered search tier by default"
            );
        }
    }

    /// A manifest can opt a non-`unlimited` plan into `composio` with an explicit
    /// token budget; exhausting that budget drops exactly `composio`.
    #[test]
    fn explicit_composio_budget_grants_then_exhausts_only_composio() {
        let mut token_budgets = BTreeMap::new();
        token_budgets.insert("composio".to_string(), 100_000);
        let plan = CapabilityPlan::from_manifest(&Plan {
            name: Some("starter".into()),
            period: "daily".into(),
            token_budgets,
            total_tokens: None,
        })
        .unwrap();
        // Under budget: composio granted, shell/code still granted.
        let under = plan.denied_namespaces(50_000);
        assert!(!under.contains("composio"));
        assert!(!under.contains("shell"));
        // At the composio budget but under starter's 200k shell/code: only
        // composio drops.
        let at = plan.denied_namespaces(100_000);
        assert!(at.contains("composio"), "composio exhausted at its budget");
        assert!(!at.contains("shell"), "shell still under its 200k budget");
    }

    #[test]
    fn unknown_plan_name_is_none() {
        assert!(plan_named("enterprise").is_none());
    }

    // --- status -------------------------------------------------------------

    #[test]
    fn status_reports_per_tier_rows_against_shared_spend() {
        let plan = plan_named("starter").unwrap();
        let rows = plan.status(200_000);
        assert_eq!(rows.len(), 2, "one row per configured budget");
        for row in &rows {
            assert_eq!(row.spent, 200_000);
            assert_eq!(row.budget, 200_000);
            assert_eq!(row.remaining, 0);
            assert!(row.exhausted);
        }
        // Sorted namespace order (BTreeMap): code before shell.
        assert_eq!(rows[0].namespace, "code");
        assert_eq!(rows[1].namespace, "shell");
    }

    #[test]
    fn status_remaining_saturates_and_tracks_exhaustion() {
        let plan = plan_named("pro").unwrap();
        let rows = plan.status(600_000);
        for row in rows {
            assert_eq!(row.remaining, 400_000);
            assert!(!row.exhausted);
        }
    }

    // --- from_manifest ------------------------------------------------------

    #[test]
    fn from_manifest_none_when_unset() {
        let plan = Plan::default();
        assert!(CapabilityPlan::from_manifest(&plan).is_none());
    }

    #[test]
    fn from_manifest_named_tier_resolves_budgets() {
        let plan = Plan {
            name: Some("starter".into()),
            period: "daily".into(),
            token_budgets: BTreeMap::new(),
            total_tokens: None,
        };
        let resolved = CapabilityPlan::from_manifest(&plan).unwrap();
        assert_eq!(resolved.period, BudgetPeriod::Daily);
        assert_eq!(resolved.budgets.get("shell"), Some(&200_000));
        assert_eq!(resolved.budgets.get("code"), Some(&200_000));
        assert!(!resolved.budgets.contains_key("web"));
    }

    #[test]
    fn from_manifest_token_budgets_override_and_extend_named() {
        let mut token_budgets = BTreeMap::new();
        token_budgets.insert("shell".to_string(), 42);
        token_budgets.insert("web".to_string(), 7);
        let plan = Plan {
            name: Some("starter".into()),
            period: "monthly".into(),
            token_budgets,
            total_tokens: None,
        };
        let resolved = CapabilityPlan::from_manifest(&plan).unwrap();
        assert_eq!(resolved.period, BudgetPeriod::Monthly, "period field wins");
        assert_eq!(resolved.budgets.get("shell"), Some(&42), "override");
        assert_eq!(resolved.budgets.get("code"), Some(&200_000), "kept");
        assert_eq!(resolved.budgets.get("web"), Some(&7), "extended");
    }

    #[test]
    fn from_manifest_bare_token_budgets_without_name() {
        let mut token_budgets = BTreeMap::new();
        token_budgets.insert("shell".to_string(), 500);
        let plan = Plan {
            name: None,
            period: "daily".into(),
            token_budgets,
            total_tokens: None,
        };
        let resolved = CapabilityPlan::from_manifest(&plan).unwrap();
        assert_eq!(resolved.budgets.len(), 1);
        assert_eq!(resolved.budgets.get("shell"), Some(&500));
    }

    // --- total budget (issue #188) ------------------------------------------

    #[test]
    fn total_exhausted_none_budget_is_never_exhausted() {
        let plan = plan_named("starter").unwrap();
        assert!(plan.total_budget.is_none(), "named tiers carry no total");
        assert!(!plan.total_exhausted(0));
        assert!(!plan.total_exhausted(u64::MAX));
    }

    #[test]
    fn total_exhausted_trips_at_the_ge_boundary() {
        let mut plan = plan_named("free").unwrap();
        plan.total_budget = Some(1_000);
        assert!(!plan.total_exhausted(999), "under budget runs");
        assert!(plan.total_exhausted(1_000), ">= boundary refuses");
        assert!(plan.total_exhausted(1_001), "over budget refuses");
    }

    #[test]
    fn total_exhausted_zero_budget_refuses_from_the_first_token() {
        let mut plan = plan_named("free").unwrap();
        plan.total_budget = Some(0);
        assert!(
            plan.total_exhausted(0),
            "a zero ceiling refuses immediately"
        );
    }

    #[test]
    fn total_status_none_when_no_ceiling() {
        let plan = plan_named("pro").unwrap();
        assert!(plan.total_status(0).is_none());
    }

    #[test]
    fn total_status_reports_budget_spend_remaining_and_exhaustion() {
        let mut plan = plan_named("free").unwrap();
        plan.total_budget = Some(1_000);
        let under = plan.total_status(400).unwrap();
        assert_eq!(under.budget, 1_000);
        assert_eq!(under.spent, 400);
        assert_eq!(under.remaining, 600);
        assert!(!under.exhausted);
        // At/over the ceiling: remaining saturates at zero and exhausted flips.
        let over = plan.total_status(1_500).unwrap();
        assert_eq!(over.remaining, 0);
        assert!(over.exhausted);
    }

    #[test]
    fn from_manifest_carries_total_tokens_and_named_tier_leaves_it_none() {
        // A bare total-only plan: no name, no per-namespace budgets.
        let plan = Plan {
            name: None,
            period: "daily".into(),
            token_budgets: BTreeMap::new(),
            total_tokens: Some(5_000),
        };
        let resolved = CapabilityPlan::from_manifest(&plan).unwrap();
        assert_eq!(resolved.total_budget, Some(5_000));
        assert!(resolved.budgets.is_empty(), "no per-namespace gate");

        // A named tier with an explicit total overlay carries the ceiling too.
        let overlaid = CapabilityPlan::from_manifest(&Plan {
            name: Some("starter".into()),
            period: "daily".into(),
            token_budgets: BTreeMap::new(),
            total_tokens: Some(9_000),
        })
        .unwrap();
        assert_eq!(overlaid.total_budget, Some(9_000));
        assert_eq!(overlaid.budgets.get("shell"), Some(&200_000));

        // A named tier with no overlay leaves the total gate off.
        let plain = plan_named("starter").unwrap();
        assert!(plain.total_budget.is_none());
    }
}
