//! Emitting the usage sample one workflow-builder pass costs, and attributing it
//! to the attempt that spent it (issue #580).
//!
//! A `workflow`-deliverable card entering In Progress makes exactly one tool-less
//! model call — the builder pass (`crate::harness::workflow_build`) — which turns
//! the card's plan into a proposed graph. Those tokens are real, so they must
//! reach the meter; this module answers the same two questions
//! [`planning`](super::planning) does, and gives the **opposite** answers, on
//! purpose.
//!
//! ## The assignee pays, and the spend links to a run
//!
//! Planning is charged to the whole-company bucket with no run, because planning
//! is frequently what *picks* the assignee and mints no attempt row. A builder
//! pass is different on both counts:
//!
//!  * **Building the workflow IS the card's In-Progress work.** It is dispatched
//!    to an assignee the operator (or a prior plan) already chose, and it mints a
//!    real [`RunRecord`](crate::ports::runs::RunRecord) — the attempt whose id the
//!    resulting proposal, and later the applied card's
//!    [`TaskOutput`](crate::ports::tasks::TaskOutput), point at. So the sample is
//!    attributed to that **assignee** and carries that **`run_id`**, exactly like
//!    an ordinary dispatch turn's inference sample
//!    ([`cost::usage_sample_for`](crate::harness::cost::usage_sample_for)).
//!  * **It is inference spend.** The sample's kind is
//!    [`SampleKind::Inference`](crate::ports::usage::SampleKind::Inference), not a
//!    bespoke kind — the call is a normal model call on the card's attempt, it
//!    should read as one in the Usage view, and it must count toward the
//!    capability-tier ceiling through
//!    [`tokens_in`](super::capability::tokens_in) the same way every other
//!    inference token does. A new `SampleKind` would have to be threaded through
//!    every exhaustive match that reads one, for a distinction the spend view does
//!    not need.
//!
//! ## Why it lives here (always compiled) and not in the harness
//!
//! The same argument [`planning`](super::planning) and [`inference`](super::inference)
//! make: the builder itself is behind the non-default `openhuman` feature, which
//! CI's default lane never compiles. Keeping the sample shape, the attribution
//! rule and the zero-usage guard here — beside the aggregation that reads them —
//! means this contract is unit-tested on every CI run, and the builder is a thin
//! delegation over it.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_workflow_build_usage`] logs and swallows both writes. The tokens were
//! spent before this function was called; a full disk must not turn a proposal
//! the operator can review into a failed pass.

use crate::ports::types::{CompanyId, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

use super::inference::inference_ledger_entry;

/// Builds the [`SampleKind::Inference`] sample for one completed builder pass, or
/// `None` when the pass moved no tokens and cost nothing.
///
/// The `None` case is the offline/mock path: a provider that reports no usage
/// yields a zero [`TokenUsage`], and a zero row in the Usage view is
/// indistinguishable from a real free call.
///
/// Unlike [`planning_sample`](super::planning::planning_sample), `agent` and
/// `run_id` are parameters and both are carried: the builder pass runs on a
/// dispatched card, so its spend belongs to that card's assignee and its attempt,
/// exactly like any other dispatch turn.
///
/// `model` is the classified [`ModelSlug`](crate::metering::ModelSlug) the pass
/// ran against, or `None` when the caller cannot name one (issue #1749).
pub fn workflow_build_sample(
    usage: &TokenUsage,
    provider: &str,
    agent: &str,
    run_id: &str,
    model: Option<crate::metering::ModelSlug>,
) -> Option<UsageSample> {
    if usage.is_zero() {
        return None;
    }
    Some(UsageSample {
        at_millis: now_millis(),
        agent: agent.to_string(),
        provider: super::oauth::normalize_provider(provider),
        input_tokens: usage.input,
        output_tokens: usage.output,
        cached_input_tokens: usage.cached_input,
        cost_usd: usage.cost_usd,
        kind: SampleKind::Inference,
        run_id: Some(run_id.to_string()),
        model,
    })
}

/// Records one completed builder pass: the Finances ledger entry (when the pass
/// cost USD) and the usage sample (when it moved tokens or money), both
/// attributed to `agent`.
///
/// The ledger entry goes through the **same** [`inference_ledger_entry`] every
/// other inference spend uses, under the same `inference.spend` kind and memo'd
/// to the assignee — a builder pass's spend is inference spend as far as the
/// money is concerned. Both writes are logged-and-swallowed: see the module docs.
#[allow(clippy::too_many_arguments)]
pub async fn record_workflow_build_usage(
    usage: &TokenUsage,
    provider: &str,
    company: &CompanyId,
    agent: &str,
    run_id: &str,
    model: Option<crate::metering::ModelSlug>,
    store: &dyn CompanyStore,
    meter: &dyn UsageMeter,
) {
    if usage.is_zero() {
        return;
    }
    tracing::debug!(
        company = %company,
        provider = %provider,
        agent = %agent,
        run = %run_id,
        input = usage.input,
        output = usage.output,
        cached_input = usage.cached_input,
        cost_usd = usage.cost_usd,
        "[usage] recording a workflow-builder pass"
    );
    if let Some(entry) = inference_ledger_entry(usage, agent)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to append the builder-pass spend entry; the proposal itself was written"
        );
    }
    if let Some(sample) = workflow_build_sample(usage, provider, agent, run_id, model)
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record a builder-pass sample; the proposal itself was written"
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn usage_with(cost: f64) -> TokenUsage {
        TokenUsage {
            input: 1_200,
            output: 400,
            cached_input: 150,
            cost_usd: cost,
        }
    }

    /// The attribution rule, pinned — and it is the mirror image of planning's.
    /// A builder pass is charged to the card's **assignee**, carries the
    /// **attempt's run id**, and reads as ordinary [`SampleKind::Inference`]
    /// spend, because building the workflow is the card's In-Progress work rather
    /// than the run-less, company-bucket planning pass.
    #[test]
    fn a_builder_sample_is_charged_to_the_assignee_with_a_run() {
        let sample = workflow_build_sample(&usage_with(0.6), "managed", "maya", "run-7", None)
            .expect("a real pass meters");
        assert_eq!(sample.agent, "maya");
        assert_eq!(sample.kind, SampleKind::Inference);
        assert_eq!(
            sample.run_id.as_deref(),
            Some("run-7"),
            "a builder pass mints an attempt row, so its spend links to that run"
        );
        assert_eq!(sample.input_tokens, 1_200);
        assert_eq!(sample.output_tokens, 400);
        assert_eq!(sample.cached_input_tokens, 150);
        assert_eq!(sample.cost_usd, 0.6);
        assert_eq!(sample.provider, "managed");
    }

    /// The token counter the capability-tier ceiling reads
    /// ([`tokens_in`](crate::metering::capability::tokens_in)) sums
    /// `Inference` samples, so a builder pass automatically counts against the
    /// tier — the exact guard the module docs promise. Pinned here so a future
    /// switch of the sample's kind that would silently exempt builder spend from
    /// the ceiling fails a test instead.
    #[test]
    fn builder_spend_counts_toward_the_capability_ceiling() {
        let sample = workflow_build_sample(&usage_with(0.6), "managed", "maya", "run-7", None)
            .expect("sample");
        assert_eq!(
            crate::metering::tokens_in(std::slice::from_ref(&sample)),
            1_600
        );
    }

    /// A provider slug is normalised the same way every other sample's is, so the
    /// Usage view's provider axis does not grow a second spelling of one backend.
    #[test]
    fn the_provider_slug_is_normalised() {
        let sample = workflow_build_sample(&usage_with(0.1), "  MANAGED ", "maya", "run-7", None)
            .expect("sample");
        assert_eq!(sample.provider, "managed");
        let blank =
            workflow_build_sample(&usage_with(0.1), "", "maya", "run-7", None).expect("sample");
        assert_eq!(blank.provider, crate::metering::UNKNOWN_PROVIDER);
    }

    /// A pass that moved nothing writes nothing — the offline provider reports no
    /// usage, and a zero row is indistinguishable from a real free call. Cost
    /// alone, or tokens alone, is enough to be worth recording.
    #[test]
    fn a_zero_pass_writes_no_sample() {
        assert!(
            workflow_build_sample(&TokenUsage::default(), "managed", "maya", "run-7", None)
                .is_none()
        );
        assert!(
            workflow_build_sample(
                &TokenUsage {
                    cost_usd: 0.01,
                    ..TokenUsage::default()
                },
                "managed",
                "maya",
                "run-7",
                None
            )
            .is_some()
        );
        assert!(
            workflow_build_sample(
                &TokenUsage {
                    input: 1,
                    ..TokenUsage::default()
                },
                "managed",
                "maya",
                "run-7",
                None
            )
            .is_some()
        );
    }

    /// Builder spend posts to the same Finances category as any other inference
    /// spend, memo'd to the assignee — not to the whole-company bucket planning
    /// uses. A token-bearing but zero-cost pass posts no money; the sample still
    /// lands.
    #[test]
    fn builder_spend_posts_to_the_inference_ledger_under_the_assignee() {
        let entry = inference_ledger_entry(&usage_with(0.25), "maya").expect("a costed pass posts");
        assert_eq!(entry.kind, crate::metering::INFERENCE_SPEND_KIND);
        assert_eq!(
            entry.amount_usd, -0.25,
            "an outflow is negative (issue #1047)"
        );
        assert_eq!(entry.memo, "maya");
        assert!(inference_ledger_entry(&usage_with(0.0), "maya").is_none());
    }
}
