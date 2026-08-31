//! Emitting [`SampleKind::SelectorCall`] usage samples — what a responder
//! selection costs, and who it is charged to (issue #1835).
//!
//! # Why this is not a teammate's inference
//!
//! A responder selection is the tool-less model call an unmentioned message in
//! an `auto` channel makes to pick its best-fit answerer. It happens **before**
//! any teammate is chosen — selection is what picks the teammate — so there is
//! no agent to attribute it to. Like a triage escalation, it is charged to the
//! whole-company bucket ([`UNATTRIBUTED_AGENT`]) with no `run_id`.
//!
//! It is deliberately not folded into
//! [`SampleKind::TriageCall`](crate::ports::usage::SampleKind) either. The two
//! are driven by different things — triage by raw chat volume everywhere,
//! selection only by unmentioned messages in `auto` channels — so sharing a
//! kind would make the triage line item move whenever an operator created a
//! channel, and neither number could be tuned against the other.
//!
//! # Both writes are logged and swallowed
//!
//! Same rule as the triage path: the selection has already been made and the
//! operator's turn is already running by the time this is called. A ledger or
//! meter hiccup must cost the accounting row and never the reply. The tokens
//! were genuinely spent either way, which is why the failure is logged rather
//! than silently dropped.

use crate::ports::types::{CompanyId, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

use super::inference::{UNATTRIBUTED_AGENT, inference_ledger_entry};

/// Builds the [`SampleKind::SelectorCall`] sample for one completed selection,
/// or `None` when it moved no tokens and cost nothing.
///
/// The `None` case is the offline/mock path, exactly as in
/// [`triage_sample`](super::triage_sample): a provider reporting no usage
/// yields a zero [`TokenUsage`], and a row for it would claim a call happened
/// that is indistinguishable from a real free one.
///
/// `agent` is not a parameter — attribution to [`UNATTRIBUTED_AGENT`] is the
/// rule this module holds, so no caller can bill a selection to the teammate it
/// happened to pick.
///
/// `model` is the classified [`ModelSlug`](crate::metering::ModelSlug) the pass
/// ran against, or `None` when the caller cannot name one (issue #1749).
pub fn selector_sample(
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
        kind: SampleKind::SelectorCall,
        run_id: None,
        model,
    })
}

/// Records one completed responder selection: the Finances ledger entry (when
/// it cost USD) and, when a usage meter is wired, the usage sample.
///
/// The ledger entry goes through the same [`inference_ledger_entry`] the
/// cycle's inference spend uses, under the same `inference.spend` kind —
/// selection spend is inference spend as far as the money is concerned, and
/// only the *usage* breakdown cares about the distinction. The meter is
/// deliberately optional: a host with no usage meter still records the spend
/// it can prove, exactly as
/// [`record_triage_usage`](super::record_triage_usage) preserves its ledger
/// write without a meter.
pub async fn record_selector_usage(
    usage: &TokenUsage,
    provider: &str,
    model: Option<crate::metering::ModelSlug>,
    company: &CompanyId,
    store: &dyn CompanyStore,
    meter: Option<&dyn UsageMeter>,
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
        "[usage] recording a responder selection"
    );
    if let Some(entry) = inference_ledger_entry(usage, UNATTRIBUTED_AGENT)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to append the selection spend entry; the pick still stands"
        );
    }
    if let Some(sample) = selector_sample(usage, provider, model)
        && let Some(meter) = meter
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to record the selection usage sample; the pick still stands"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> TokenUsage {
        TokenUsage {
            input: 120,
            output: 4,
            cached_input: 0,
            cost_usd: 0.0002,
        }
    }

    /// A selection that moved tokens mints a [`SampleKind::SelectorCall`] row
    /// charged to the whole-company bucket — never to the teammate it picked.
    #[test]
    fn a_selection_samples_under_its_own_kind_and_no_teammate() {
        let sample = selector_sample(&usage(), "openrouter", None).expect("a real spend samples");
        assert_eq!(sample.kind, SampleKind::SelectorCall);
        assert_eq!(sample.agent, UNATTRIBUTED_AGENT);
        assert_eq!(sample.run_id, None);
        assert_eq!(sample.input_tokens, 120);
    }

    /// The offline/mock path — zero usage — mints no row: a free fake call is
    /// indistinguishable from a real free one, so no row is the honest record.
    #[test]
    fn zero_usage_mints_no_sample() {
        assert!(selector_sample(&TokenUsage::default(), "openrouter", None).is_none());
    }
}
