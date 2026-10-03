use super::*;

/// **Issue #1047, the test whose absence let this stand.** The writer and
/// the reader, together — because each was self-consistent and only their
/// pairing was wrong.
///
/// A company that has only ever spent money must report that spending as
/// `spent_usd`. Before this, `inference_ledger_entry` posted the cost
/// unnegated and `finances_from` read a positive amount as income, so the
/// Finances surface showed growing **revenue** and a growing **balance**
/// while `spent_usd` and `by_category` stayed at zero.
///
/// Asserts the **direction**, not the magnitude: `revenue_usd` must be
/// exactly zero and `balance_usd` must be negative. A test that only
/// checked `spent_usd.abs()` would pass with the sign flipped either way,
/// which is the failure mode that produced this bug.
#[test]
fn a_costed_cycle_is_spending_not_revenue() {
    let usage = TokenUsage {
        input: 1_000,
        output: 200,
        cached_input: 0,
        cost_usd: 0.42,
    };
    let entry = inference_ledger_entry(&usage, "ceo").expect("a costed cycle posts");

    // The writer's half: an outflow is negative.
    assert!(
        entry.amount_usd < 0.0,
        "inference is an outflow, so the ledger amount is negative: {}",
        entry.amount_usd
    );
    assert!((entry.amount_usd - -0.42).abs() < 1e-9);

    // The reader's half, over the entry the writer actually produced.
    let at_millis = entry.at_millis;
    let finances = crate::metering::finances::finances_from(
        std::slice::from_ref(&entry),
        &crate::company::Budget { monthly_usd: None },
        at_millis,
    );

    assert!(
        (finances.spent_usd - 0.42).abs() < 1e-9,
        "the charge lands in spent_usd: {finances:?}"
    );
    assert_eq!(
        finances.revenue_usd, 0.0,
        "a company that only spent money has NO revenue: {finances:?}"
    );
    assert!(
        finances.balance_usd < 0.0,
        "and spending moves the balance down, not up: {}",
        finances.balance_usd
    );
    assert_eq!(
        finances.by_category.len(),
        1,
        "the spend is categorised rather than invisible: {finances:?}"
    );
    assert_eq!(finances.by_category[0].category, "Inference");
    assert!((finances.by_category[0].amount - 0.42).abs() < 1e-9);
}

/// The operator-facing rendering, pinned because it is the half a sign
/// change could plausibly break: `Transaction` carries an **absolute**
/// amount plus a `Direction`, so negating the writer must flip the
/// direction to `Out` and must NOT start rendering a negative figure.
#[test]
fn a_charge_renders_as_an_outgoing_transaction_with_a_positive_amount() {
    let usage = TokenUsage {
        input: 10,
        output: 5,
        cached_input: 0,
        cost_usd: 0.42,
    };
    let entry = inference_ledger_entry(&usage, "ceo").expect("a costed cycle posts");
    let at_millis = entry.at_millis;
    let finances = crate::metering::finances::finances_from(
        std::slice::from_ref(&entry),
        &crate::company::Budget { monthly_usd: None },
        at_millis,
    );

    assert_eq!(finances.transactions.len(), 1);
    let tx = &finances.transactions[0];
    assert!(
        matches!(tx.direction, crate::metering::types::Direction::Out),
        "a charge is money going out"
    );
    assert!(
        tx.amount_usd > 0.0,
        "the rendered amount stays a positive magnitude — the sign is the \
         `direction` field's job, not the number's: {}",
        tx.amount_usd
    );
    assert!((tx.amount_usd - 0.42).abs() < 1e-9);
    assert_eq!(tx.category, "Inference");
}

use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::OpenCompanyError;
use crate::ports::types::{CompanyRecord, CompanySummary};

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

/// A store whose ledger append always fails — proves accounting cannot fail
/// the cycle it accounts for.
struct FailingStore;

#[async_trait]
impl CompanyStore for FailingStore {
    async fn load(&self, _id: &CompanyId) -> crate::Result<Option<CompanyRecord>> {
        Ok(None)
    }
    async fn save(&self, _record: &CompanyRecord) -> crate::Result<()> {
        Ok(())
    }
    async fn list(&self) -> crate::Result<Vec<CompanySummary>> {
        Ok(Vec::new())
    }
    async fn append_ledger(&self, _id: &CompanyId, _entry: LedgerEntry) -> crate::Result<()> {
        Err(OpenCompanyError::Store("ledger on fire".to_string()))
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
    async fn query(&self, _company: &CompanyId, _since: u64) -> crate::Result<Vec<UsageSample>> {
        Ok(self.samples.lock().unwrap().clone())
    }
}

struct FailingMeter;

#[async_trait]
impl UsageMeter for FailingMeter {
    async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> crate::Result<()> {
        Err(OpenCompanyError::Store("disk on fire".to_string()))
    }
    async fn query(&self, _company: &CompanyId, _since: u64) -> crate::Result<Vec<UsageSample>> {
        Ok(Vec::new())
    }
}

fn usage_with(cost: f64) -> TokenUsage {
    TokenUsage {
        input: 100,
        output: 50,
        cached_input: 10,
        cost_usd: cost,
    }
}

#[test]
fn zero_usage_produces_no_entry_or_sample() {
    let usage = TokenUsage::default();
    assert!(inference_ledger_entry(&usage, "ceo").is_none());
    assert!(inference_sample(&usage, "ceo", MEDULLA_PROVIDER, None).is_none());
}

/// The managed passthrough reports tokens but no USD (billing happens
/// backend-side, off the wire). A token-bearing, zero-cost cycle is still
/// real usage — it must produce a sample so the Usage surface is not blind.
#[test]
fn token_only_zero_cost_usage_still_produces_a_sample() {
    let usage = TokenUsage {
        input: 22,
        output: 2,
        cached_input: 0,
        cost_usd: 0.0,
    };
    assert!(inference_sample(&usage, "ceo", "managed", None).is_some());
    // No USD ⇒ no ledger entry, but the token sample still lands.
    assert!(inference_ledger_entry(&usage, "ceo").is_none());
}

#[test]
fn sample_carries_every_token_field_and_the_inference_kind() {
    let sample = inference_sample(&usage_with(0.25), "ceo", MEDULLA_PROVIDER, None).unwrap();
    assert_eq!(sample.kind, SampleKind::Inference);
    assert_eq!(sample.agent, "ceo");
    assert_eq!(sample.provider, MEDULLA_PROVIDER);
    assert_eq!(sample.input_tokens, 100);
    assert_eq!(sample.output_tokens, 50);
    assert_eq!(sample.cached_input_tokens, 10);
    assert_eq!(sample.cost_usd, 0.25);
}

/// `byProvider` groups on the raw slug, so an inference sample normalizes its
/// provider exactly like an OAuth one — `Medulla` and `medulla` are one row.
#[test]
fn provider_is_normalized_like_an_oauth_sample() {
    let sample = inference_sample(&usage_with(0.1), "ceo", "  MEDULLA ", None).unwrap();
    assert_eq!(sample.provider, MEDULLA_PROVIDER);
    let blank = inference_sample(&usage_with(0.1), "ceo", "", None).unwrap();
    assert_eq!(blank.provider, crate::metering::UNKNOWN_PROVIDER);
}

#[test]
fn spend_maps_to_the_inference_ledger_kind() {
    let entry = inference_ledger_entry(&usage_with(0.42), "ceo").unwrap();
    assert_eq!(entry.kind, INFERENCE_SPEND_KIND);
    // Negative: an outflow, per the ledger convention (issue #1047).
    assert_eq!(entry.amount_usd, -0.42);
    assert_eq!(entry.memo, "ceo");
}

#[tokio::test]
async fn record_writes_both_the_ledger_entry_and_the_sample() {
    let store = RecordingStore::default();
    let meter = RecordingMeter::default();
    record_inference_usage(
        &usage_with(1.5),
        UNATTRIBUTED_AGENT,
        MEDULLA_PROVIDER,
        None,
        &CompanyId::new("acme"),
        &store,
        &meter,
    )
    .await;

    let ledger = store.ledger.lock().unwrap();
    assert_eq!(ledger.len(), 1);
    // Negative: an outflow, per the ledger convention (issue #1047).
    assert_eq!(ledger[0].amount_usd, -1.5);
    let samples = meter.samples.lock().unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].kind, SampleKind::Inference);
    assert_eq!(samples[0].agent, UNATTRIBUTED_AGENT);
    assert_eq!(samples[0].output_tokens, 50);
}

#[tokio::test]
async fn record_is_a_noop_for_zero_usage() {
    let store = RecordingStore::default();
    let meter = RecordingMeter::default();
    record_inference_usage(
        &TokenUsage::default(),
        UNATTRIBUTED_AGENT,
        MEDULLA_PROVIDER,
        None,
        &CompanyId::new("acme"),
        &store,
        &meter,
    )
    .await;
    assert!(store.ledger.lock().unwrap().is_empty());
    assert!(meter.samples.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_write_failure_never_surfaces_to_the_cycle() {
    // The tokens were already spent and the reply is already on the
    // operator's screen; failing here would fail a cycle that succeeded.
    record_inference_usage(
        &usage_with(1.0),
        UNATTRIBUTED_AGENT,
        MEDULLA_PROVIDER,
        None,
        &CompanyId::new("acme"),
        &FailingStore,
        &FailingMeter,
    )
    .await;
}

/// The emitted sample must survive the aggregation that reads it — the whole
/// point of issue #174 is that these totals stop being zero.
#[test]
fn emitted_samples_reach_the_console_totals() {
    use std::collections::HashMap;

    use crate::metering::{UsageRange, bucket_usage};

    let now = 1_700_000_000_000u64;
    let mut sample = inference_sample(
        &usage_with(0.75),
        UNATTRIBUTED_AGENT,
        MEDULLA_PROVIDER,
        None,
    )
    .expect("non-zero usage samples");
    sample.at_millis = now;

    let usage = bucket_usage(&[sample], UsageRange::D7, now, &HashMap::new());
    assert_eq!(usage.totals.input_tokens, 100);
    assert_eq!(usage.totals.output_tokens, 50);
    assert_eq!(usage.totals.tokens, 150);
    assert_eq!(usage.totals.cost_usd, 0.75);
    // Charged to the whole-company bucket, and it lands on today's point.
    assert_eq!(usage.by_agent.len(), 1);
    assert_eq!(usage.by_agent[0].name, UNATTRIBUTED_AGENT);
    assert_eq!(usage.series.last().unwrap().input_tokens, 100);
    // An inference sample is not an OAuth call, so it never inflates those.
    assert_eq!(usage.totals.oauth_calls, 0);
}

/// Issue #1749: the shared mapping every cognition path goes through
/// carries the model, and carries it as a vocabulary member.
#[test]
fn a_cycles_sample_names_the_model_the_brain_reported() {
    let sample = inference_sample(
        &usage_with(0.2),
        "ceo",
        MEDULLA_PROVIDER,
        Some(crate::metering::ModelSlug::classify(
            "deepseek/deepseek-v4-pro",
        )),
    )
    .expect("a real cycle meters");
    assert_eq!(sample.model.map(|m| m.as_str()), Some("deepseek-v4-pro"));

    // A path that cannot name one says so, rather than guessing.
    let blind = inference_sample(&usage_with(0.2), "ceo", MEDULLA_PROVIDER, None)
        .expect("a real cycle meters");
    assert_eq!(blind.model, None);
}
