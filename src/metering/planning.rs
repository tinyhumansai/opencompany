//! Emitting [`SampleKind::PlanningCall`] usage samples — what a planning pass
//! costs, and who it is charged to (issue #337).
//!
//! A card dragged into `planning` makes exactly one tool-less model call. Those
//! tokens are real, so they must reach the meter; the only question this module
//! answers is *whose* they are.
//!
//! ## The company pays, not the assignee
//!
//! The sample is written against [`UNATTRIBUTED_AGENT`] — the whole-company
//! bucket — rather than the card's assignee, and charging the assignee would be
//! wrong twice over:
//!
//!  * **Planning is often what picks the assignee.** A card dragged into
//!    Planning with a blank assignee has nobody to charge, and the pass's own
//!    output is what fills the field in. Attributing the cost to the teammate
//!    the pass *chose* would bill a decision to its own outcome.
//!  * **Per-agent caps are enforced (issue #304).** A teammate close to its
//!    `budget_usd_daily` would make its own cards unplannable — the operator
//!    would drag a card into Planning and get a refusal about a spend limit
//!    they were not trying to spend against. Worse, the refusal would arrive
//!    for the *planning* of work whose whole purpose might have been to hand it
//!    to somebody else.
//!
//! `"company"` is not a roster agent, so it is uncapped by #304 — but it is
//! **not** uncapped generally: the tokens count toward the capability-tier
//! ceiling (issue #108) through
//! [`tokens_in`](super::capability::tokens_in), which is what stops a company
//! planning forever on an exhausted plan.
//!
//! ## No run, so no `run_id`
//!
//! A planning pass mints no [`RunRecord`](crate::ports::runs::RunRecord): there
//! is no agent turn, no tool loop, no trace, and nothing for an operator to
//! steer or cancel. `run_id: None` is therefore the truth, not a gap — see
//! `docs/spec/runtime/planning.md` for why the pass deliberately runs outside
//! the run machinery.
//!
//! ## Why it lives here (always compiled) and not in the harness
//!
//! Exactly the argument [`inference`](super::inference) and [`oauth`](super::oauth)
//! make: the planner itself is behind the non-default `openhuman` feature, and
//! CI's default lane never compiles that. Keeping the sample shape, the
//! attribution rule and the zero-usage guard here — beside the aggregation that
//! reads them — means this contract is unit-tested on every CI run, and the
//! planner is a thin delegation over it.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_planning_usage`] logs and swallows both writes. The tokens were
//! spent before this function was called; a full disk must not turn a plan the
//! operator can read into a failed pass.

use crate::ports::types::{CompanyId, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

use super::inference::{UNATTRIBUTED_AGENT, inference_ledger_entry};

/// Builds the [`SampleKind::PlanningCall`] sample for one completed planning
/// pass, or `None` when the pass moved no tokens and cost nothing.
///
/// The `None` case is the offline/mock path: a provider that reports no usage
/// yields a zero [`TokenUsage`], and writing a sample for it would put a row in
/// the Usage view claiming a call that cost nothing happened — indistinguishable
/// from a real free call, and noise in a chart whose whole job is showing spend.
///
/// `agent` is not a parameter. Attribution to [`UNATTRIBUTED_AGENT`] is the
/// rule this module exists to hold (see the module docs), so a caller cannot
/// pass a teammate id in and quietly bill planning to a desk.
///
/// `model` is the classified [`ModelSlug`](crate::metering::ModelSlug) the pass
/// ran against, or `None` when the caller cannot name one (issue #1749).
pub fn planning_sample(
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
        kind: SampleKind::PlanningCall,
        run_id: None,
        model,
    })
}

/// Records one completed planning pass: the Finances ledger entry (when the
/// pass cost USD) and the usage sample (when it moved tokens or money).
///
/// The ledger entry goes through the **same** [`inference_ledger_entry`] the
/// cycle's inference spend uses, under the same `inference.spend` kind. That is
/// deliberate: planning spend is inference spend as far as the money is
/// concerned, and a separate Finances category would split one line item into
/// two for a distinction that only matters to the *usage* breakdown. The memo
/// carries `"company"`, so the Finances transaction list still says who.
///
/// Both writes are logged-and-swallowed: see the module docs.
pub async fn record_planning_usage(
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
        "[usage] recording a planning pass"
    );
    if let Some(entry) = inference_ledger_entry(usage, UNATTRIBUTED_AGENT)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to append the planning spend entry; the plan itself was written"
        );
    }
    if let Some(sample) = planning_sample(usage, provider, model)
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record a planning sample; the plan itself was written"
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn usage_with(cost: f64) -> TokenUsage {
        TokenUsage {
            input: 900,
            output: 300,
            cached_input: 100,
            cost_usd: cost,
        }
    }

    /// The attribution rule, pinned. A planning sample is charged to the
    /// company bucket with no run — never to a teammate — because planning is
    /// frequently what chooses the teammate, and because a teammate at its
    /// daily cap must not be unable to have work planned for it.
    #[test]
    fn a_planning_sample_is_charged_to_the_company_with_no_run() {
        let sample =
            planning_sample(&usage_with(0.4), "managed", None).expect("a real pass meters");
        assert_eq!(sample.agent, UNATTRIBUTED_AGENT);
        assert_eq!(sample.agent, "company");
        assert_eq!(sample.kind, SampleKind::PlanningCall);
        assert!(
            sample.run_id.is_none(),
            "a planning pass mints no attempt row, so it has no run to point at"
        );
        assert_eq!(sample.input_tokens, 900);
        assert_eq!(sample.output_tokens, 300);
        assert_eq!(sample.cached_input_tokens, 100);
        assert_eq!(sample.cost_usd, 0.4);
        assert_eq!(sample.provider, "managed");
    }

    /// A provider slug is normalised the same way every other sample's is, so
    /// the Usage view's provider axis does not grow a second spelling of one
    /// backend.
    #[test]
    fn the_provider_slug_is_normalised() {
        let sample = planning_sample(&usage_with(0.1), "  MANAGED ", None).expect("sample");
        assert_eq!(sample.provider, "managed");
        let blank = planning_sample(&usage_with(0.1), "", None).expect("sample");
        assert_eq!(blank.provider, crate::metering::UNKNOWN_PROVIDER);
    }

    /// A pass that moved nothing writes nothing. The offline provider reports
    /// no usage, and a zero row in the Usage view is indistinguishable from a
    /// real free call.
    #[test]
    fn a_zero_pass_writes_no_sample() {
        assert!(planning_sample(&TokenUsage::default(), "managed", None).is_none());
        // Cost alone is enough to be worth recording, and so are tokens alone.
        assert!(
            planning_sample(
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
            planning_sample(
                &TokenUsage {
                    input: 1,
                    ..TokenUsage::default()
                },
                "managed",
                None
            )
            .is_some()
        );
    }

    /// Planning spend posts to the same Finances category as any other
    /// inference spend, memo'd to the company. A separate category would split
    /// one line item for a distinction that only the usage breakdown cares
    /// about.
    #[test]
    fn planning_spend_posts_to_the_inference_ledger_under_the_company() {
        let entry = inference_ledger_entry(&usage_with(0.25), UNATTRIBUTED_AGENT)
            .expect("a costed pass posts");
        assert_eq!(entry.kind, crate::metering::INFERENCE_SPEND_KIND);
        assert_eq!(
            entry.amount_usd, -0.25,
            "an outflow is negative (issue #1047)"
        );
        assert_eq!(entry.memo, "company");
        // A token-bearing but zero-cost pass (the managed passthrough) posts no
        // money — the sample still lands, the ledger stays honest.
        assert!(inference_ledger_entry(&usage_with(0.0), UNATTRIBUTED_AGENT).is_none());
    }
}
