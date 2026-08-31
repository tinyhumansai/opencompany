//! Emitting [`SampleKind::TriageCall`] usage samples — what a triage
//! escalation costs, and who it is charged to (issue #678).
//!
//! # Why this is not a teammate's inference
//!
//! A triage escalation is the tool-less model call an operator message makes
//! when the lexical classifier in [`task_intent`](crate::company::task_intent)
//! abstained. It happens **before** any teammate is chosen — deciding whether
//! the orchestrator may write to the board at all is upstream of who answers —
//! so there is no agent to attribute it to. Like a planning pass, it is charged
//! to the whole-company bucket ([`UNATTRIBUTED_AGENT`]) with no `run_id`.
//!
//! It is deliberately not folded into
//! [`SampleKind::PlanningCall`](crate::ports::usage::SampleKind) either. The two
//! are driven by different things — planning by cards entering `planning`,
//! triage by raw chat volume — so sharing a kind would make the planning line
//! item move whenever chat got busier, and neither number could be tuned
//! against the other.
//!
//! # Both writes are logged and swallowed
//!
//! Same rule as the planning path: the classification has already been made and
//! the operator's turn is already running by the time this is called. A ledger
//! or meter hiccup must cost the accounting row and never the reply. The tokens
//! were genuinely spent either way, which is why the failure is logged rather
//! than silently dropped.

use crate::ports::types::{CompanyId, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

use super::inference::{UNATTRIBUTED_AGENT, inference_ledger_entry};

/// Builds the [`SampleKind::TriageCall`] sample for one completed escalation, or
/// `None` when it moved no tokens and cost nothing.
///
/// The `None` case is the offline/mock path, exactly as in
/// [`planning_sample`](super::planning_sample): a provider reporting no usage
/// yields a zero [`TokenUsage`], and a row for it would claim a call happened
/// that is indistinguishable from a real free one.
///
/// `agent` is not a parameter — attribution to [`UNATTRIBUTED_AGENT`] is the
/// rule this module holds, so no caller can bill a classification to a desk.
///
/// `model` is the classified [`ModelSlug`](crate::metering::ModelSlug) the pass
/// ran against, or `None` when the caller cannot name one (issue #1749).
pub fn triage_sample(
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
        kind: SampleKind::TriageCall,
        run_id: None,
        model,
    })
}

/// Records one completed triage escalation: the Finances ledger entry (when it
/// cost USD) and, when a usage meter is wired, the usage sample.
///
/// The ledger entry goes through the same [`inference_ledger_entry`] the cycle's
/// inference spend uses, under the same `inference.spend` kind — triage spend is
/// inference spend as far as the money is concerned, and only the *usage*
/// breakdown cares about the distinction. The meter is deliberately optional:
/// a host with no usage meter still records the spend it can prove, exactly as
/// [`record_turn_cost`](crate::harness::cost::record_turn_cost) preserves its
/// ledger write without a meter.
pub async fn record_triage_usage(
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
        "[usage] recording a triage escalation"
    );
    if let Some(entry) = inference_ledger_entry(usage, UNATTRIBUTED_AGENT)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to append the triage spend entry; the classification still stands"
        );
    }
    if let Some(sample) = triage_sample(usage, provider, model)
        && let Some(meter) = meter
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to record the triage usage sample; the classification still stands"
        );
    }
}

#[cfg(test)]
mod test {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::ports::types::{CompanyRecord, CompanySummary, LedgerEntry};

    fn usage() -> TokenUsage {
        TokenUsage {
            input: 120,
            output: 3,
            cached_input: 0,
            cost_usd: 0.0004,
        }
    }

    #[derive(Default)]
    struct RecordingStore {
        ledger: Mutex<Vec<LedgerEntry>>,
    }

    #[async_trait]
    impl CompanyStore for RecordingStore {
        async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
            Ok(None)
        }
        async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
            Ok(())
        }
        async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
            Ok(Vec::new())
        }
        async fn append_ledger(&self, _id: &CompanyId, entry: LedgerEntry) -> crate::Result<()> {
            self.ledger.lock().unwrap().push(entry);
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingMeter {
        samples: Mutex<Vec<UsageSample>>,
    }

    #[async_trait]
    impl UsageMeter for RecordingMeter {
        async fn record(&self, _company: &CompanyId, sample: &UsageSample) -> crate::Result<()> {
            self.samples.lock().unwrap().push(sample.clone());
            Ok(())
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Ok(self.samples.lock().unwrap().clone())
        }
    }

    /// The meter is optional, the ledger is not: a host with no usage meter must
    /// still record the spend it can prove (same contract as
    /// [`record_turn_cost`](crate::harness::cost::record_turn_cost)).
    #[tokio::test]
    async fn meter_none_still_records_the_ledger_row() {
        let store = RecordingStore::default();
        let company = CompanyId::new("acme");
        record_triage_usage(&usage(), "openrouter", None, &company, &store, None).await;

        let ledger = store.ledger.lock().unwrap();
        assert_eq!(
            ledger.len(),
            1,
            "the spend row must survive without a meter"
        );
        let entry = &ledger[0];
        assert_eq!(entry.kind, super::super::inference::INFERENCE_SPEND_KIND);
        assert_eq!(entry.memo, UNATTRIBUTED_AGENT);
        assert!(
            (entry.amount_usd - (-0.0004)).abs() < 1e-9,
            "an outflow posts negative (issue #1047)"
        );
    }

    /// A wired meter receives the same sample `triage_sample` builds — the
    /// `record` call and the shape the aggregation reads are one contract.
    #[tokio::test]
    async fn a_wired_meter_records_the_sample_and_the_ledger() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let company = CompanyId::new("acme");
        record_triage_usage(&usage(), "openrouter", None, &company, &store, Some(&meter)).await;

        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].kind, SampleKind::TriageCall);
        assert_eq!(samples[0].agent, UNATTRIBUTED_AGENT);
        assert_eq!(samples[0].provider, "openrouter");
        assert_eq!(samples[0].input_tokens, 120);
        assert_eq!(store.ledger.lock().unwrap().len(), 1);
    }

    /// The offline path is a no-op at the record level too — nothing to charge
    /// and nothing to meter.
    #[tokio::test]
    async fn a_zero_usage_escalation_records_nothing() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let company = CompanyId::new("acme");
        record_triage_usage(
            &TokenUsage::default(),
            "managed",
            None,
            &company,
            &store,
            Some(&meter),
        )
        .await;
        assert!(store.ledger.lock().unwrap().is_empty());
        assert!(meter.samples.lock().unwrap().is_empty());
    }

    #[test]
    fn a_completed_escalation_is_charged_to_the_company_not_a_teammate() {
        let sample = triage_sample(&usage(), "managed", None).expect("a sample for real spend");
        assert_eq!(sample.kind, SampleKind::TriageCall);
        assert_eq!(
            sample.agent, UNATTRIBUTED_AGENT,
            "triage runs before a teammate is chosen, so no teammate may be billed"
        );
        assert!(
            sample.run_id.is_none(),
            "an escalation belongs to no attempt"
        );
    }

    /// The offline path. A mock provider reports nothing, and a zero row would be
    /// indistinguishable from a real call that happened to be free.
    #[test]
    fn an_escalation_that_moved_nothing_writes_no_row() {
        assert!(triage_sample(&TokenUsage::default(), "managed", None).is_none());
    }

    /// Distinct from planning, on purpose: the two are driven by different
    /// things and tuned separately.
    #[test]
    fn triage_is_not_filed_as_planning() {
        let sample = triage_sample(&usage(), "managed", None).expect("a sample");
        assert_ne!(sample.kind, SampleKind::PlanningCall);
    }
}
