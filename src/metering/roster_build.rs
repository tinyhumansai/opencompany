//! Emitting [`SampleKind::SetupCall`] usage samples — what a first-run setup
//! pass costs, and who it is charged to (`docs/spec/runtime/company-setup.md`).
//!
//! First-run setup makes exactly one tool-less model call: it takes the curated
//! template [`match_template`](crate::company::setup::match_template) chose and
//! rewrites each agent's mandate in the operator's own terms. Those tokens are
//! real, so they must reach the meter.
//!
//! ## The company pays, because there is nobody else yet
//!
//! [`planning`](super::planning) charges the whole-company bucket because
//! planning is often what *picks* the assignee. Setup's version of that argument
//! is stronger still: the pass runs **before the roster exists**. There is no
//! teammate to attribute it to, because creating the teammates is what the pass
//! is for. [`UNATTRIBUTED_AGENT`] is the only honest answer.
//!
//! This also means the per-teammate daily cap (issue #304) cannot bite here,
//! which is the behaviour we want — a company whose first act is setup must not
//! be able to fail it on a spend limit belonging to an agent that does not exist.
//! The tokens still count toward the capability-tier ceiling (issue #108)
//! through [`tokens_in`](super::capability::tokens_in).
//!
//! ## No run, so no `run_id`
//!
//! Like a planning pass and unlike a builder pass, setup mints no
//! [`RunRecord`](crate::ports::runs::RunRecord): no agent turn, no tool loop, no
//! trace, nothing for an operator to steer or cancel. `run_id: None` is the
//! truth rather than a gap.
//!
//! ## Why it lives here (always compiled) and not in the harness
//!
//! The argument [`planning`](super::planning) and [`workflow_build`](super::workflow_build)
//! both make: the pass itself is behind the non-default `openhuman` feature,
//! which CI's default lane never compiles. Keeping the sample shape, the
//! attribution rule and the zero-usage guard here — beside the aggregation that
//! reads them — means this contract is unit-tested on every CI run, and the pass
//! is a thin delegation over it.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_roster_build_usage`] logs and swallows both writes. The tokens were
//! spent before it was called, and a full disk must not turn a roster the
//! operator is about to see into a failed setup.

use crate::ports::types::{CompanyId, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

use super::inference::{UNATTRIBUTED_AGENT, inference_ledger_entry};

/// Builds the [`SampleKind::SetupCall`] sample for one completed setup pass, or
/// `None` when the pass moved no tokens and cost nothing.
///
/// The `None` case is the offline path, and it is the common one here: a
/// company with no inference credential ships the template unpolished, having
/// made no call at all. Writing a zero row for that would put a setup cost in
/// the Usage view for a company that never spent anything.
///
/// `agent` is not a parameter. Attribution to [`UNATTRIBUTED_AGENT`] is the rule
/// this module exists to hold — see the module docs — so no caller can bill
/// setup to a teammate, least of all one the pass itself invented.
///
/// `model` is the classified [`ModelSlug`](crate::metering::ModelSlug) the pass
/// ran against, or `None` when the caller cannot name one (issue #1749).
pub fn roster_build_sample(
    usage: &TokenUsage,
    provider: &str,
    model: Option<crate::metering::ModelSlug>,
) -> Option<UsageSample> {
    if usage.is_zero() {
        return None;
    }
    Some(UsageSample {
        at_millis: now_millis(),
        agent: UNATTRIBUTED_AGENT.to_string(),
        provider: super::oauth::normalize_provider(provider),
        input_tokens: usage.input,
        output_tokens: usage.output,
        cached_input_tokens: usage.cached_input,
        cost_usd: usage.cost_usd,
        kind: SampleKind::SetupCall,
        run_id: None,
        model,
    })
}

/// Records one completed setup pass: the Finances ledger entry (when it cost
/// USD) and the usage sample (when it moved tokens or money).
///
/// The ledger entry goes through the same [`inference_ledger_entry`] every other
/// model call uses, under the same `inference.spend` kind — setup spend is
/// inference spend as far as the money is concerned, and a separate Finances
/// category would split one line item for a distinction only the *usage*
/// breakdown cares about. The memo carries `"company"`, so the transaction list
/// still says who.
///
/// Both writes are logged-and-swallowed: see the module docs.
pub async fn record_roster_build_usage(
    usage: &TokenUsage,
    provider: &str,
    model: Option<crate::metering::ModelSlug>,
    company: &CompanyId,
    store: &dyn CompanyStore,
    meter: &dyn UsageMeter,
) {
    if usage.is_zero() {
        return;
    }
    tracing::debug!(
        company = %company,
        provider = %provider,
        input = usage.input,
        output = usage.output,
        cached_input = usage.cached_input,
        cost_usd = usage.cost_usd,
        "[usage] recording a first-run setup pass"
    );
    if let Some(entry) = inference_ledger_entry(usage, UNATTRIBUTED_AGENT)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to append the setup spend entry; the roster itself was proposed"
        );
    }
    if let Some(sample) = roster_build_sample(usage, provider, model)
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record a setup sample; the roster itself was proposed"
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn usage_with(cost: f64) -> TokenUsage {
        TokenUsage {
            input: 700,
            output: 250,
            cached_input: 50,
            cost_usd: cost,
        }
    }

    /// The attribution rule, pinned. Setup runs before the roster exists, so
    /// there is no teammate it could be charged to and no attempt row to point
    /// at — and a per-agent cap must never be able to fail a company's first
    /// action.
    #[test]
    fn a_setup_sample_is_charged_to_the_company_with_no_run() {
        let sample =
            roster_build_sample(&usage_with(0.2), "managed", None).expect("a real pass meters");
        assert_eq!(sample.agent, UNATTRIBUTED_AGENT);
        assert_eq!(sample.agent, "company");
        assert_eq!(sample.kind, SampleKind::SetupCall);
        assert!(
            sample.run_id.is_none(),
            "a setup pass mints no attempt row, so it has no run to point at"
        );
        assert_eq!(sample.input_tokens, 700);
        assert_eq!(sample.output_tokens, 250);
        assert_eq!(sample.cached_input_tokens, 50);
        assert_eq!(sample.cost_usd, 0.2);
    }

    /// Its own kind, distinct from planning's. Folding the two together would
    /// make "what does onboarding a company cost?" unanswerable, which is the
    /// question this feature is being measured on.
    #[test]
    fn a_setup_sample_is_not_a_planning_sample() {
        let setup = roster_build_sample(&usage_with(0.2), "managed", None).expect("sample");
        let planning = super::super::planning::planning_sample(&usage_with(0.2), "managed", None)
            .expect("sample");
        assert_ne!(setup.kind, planning.kind);
        // But both belong to the company rather than to a teammate.
        assert_eq!(setup.agent, planning.agent);
    }

    /// The offline path, and the common one: no credential means no call, so
    /// there is nothing to meter. A zero row would claim a company spent
    /// something on setup when it never made the request.
    #[test]
    fn a_pass_that_never_called_writes_no_sample() {
        assert!(roster_build_sample(&TokenUsage::default(), "managed", None).is_none());
    }

    /// Cost alone and tokens alone are each enough to be worth recording — a
    /// provider that reports one but not the other must not fall through the
    /// zero guard.
    #[test]
    fn either_tokens_or_cost_is_enough_to_record() {
        assert!(
            roster_build_sample(
                &TokenUsage {
                    cost_usd: 0.01,
                    ..TokenUsage::default()
                },
                "managed",
                None
            )
            .is_some()
        );
        assert!(
            roster_build_sample(
                &TokenUsage {
                    input: 10,
                    ..TokenUsage::default()
                },
                "managed",
                None
            )
            .is_some()
        );
    }

    /// Normalised like every other sample's, so the Usage view's provider axis
    /// does not grow a second spelling of one backend.
    #[test]
    fn the_provider_slug_is_normalised() {
        let sample = roster_build_sample(&usage_with(0.1), "  MANAGED ", None).expect("sample");
        assert_eq!(sample.provider, "managed");
        let blank = roster_build_sample(&usage_with(0.1), "", None).expect("sample");
        assert_eq!(blank.provider, crate::metering::UNKNOWN_PROVIDER);
    }

    /// Setup tokens count toward the capability-tier ceiling. Excluding them
    /// would leave a tenant able to run setup on an exhausted plan.
    #[test]
    fn setup_tokens_count_toward_the_tier_ceiling() {
        let sample = roster_build_sample(&usage_with(0.2), "managed", None).expect("sample");
        assert_eq!(
            crate::metering::capability::tokens_in(std::slice::from_ref(&sample)),
            950
        );
    }
}
