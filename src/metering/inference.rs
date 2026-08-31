//! Emitting [`SampleKind::Inference`] usage samples — the write half of the
//! Usage view's token series, `byAgent` breakdown, and `costUsd` total.
//!
//! [`bucket_usage`](super::bucket_usage) has always summed inference samples
//! into the daily series and the token/cost totals, but until now the **only**
//! writer was the `openhuman` harness's per-turn cost hook (`harness::cost`).
//! Every other cognition path — hosted Medulla, the sidecar, an injected brain
//! — reports its usage as
//! [`CycleResult::token_usage`](crate::ports::types::CycleResult::token_usage),
//! which nothing read: real inference happened and the meter stayed empty, so
//! `tokens`/`costUsd` read a blind zero (issue #174).
//!
//! This module is the shared write side. The runtime's cycle loop calls
//! [`record_inference_usage`] with whatever the brain reported, so *any* brain
//! that fills `token_usage` is metered without touching the Usage read layer.
//!
//! ## Why it lives here (always compiled) and not at the harness call site
//!
//! Same reason as [`oauth`](super::oauth): the harness is behind the
//! non-default `openhuman` feature and CI builds only the default feature set,
//! so a mapping written *inside* the harness is neither compiled nor tested by
//! CI. Keeping the sample/ledger shape and the zero-usage guard here — beside
//! the aggregation that reads them — means the contract is unit-tested on every
//! CI run and `harness::cost` is a thin delegation over the same guard.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_inference_usage`] logs and swallows a meter error rather than
//! propagating it. The tokens were already spent: a full disk must not turn a
//! model reply the operator can see into a failed cycle.
//!
//! ## Known gap — per-teammate attribution on the hosted path
//!
//! A hosted Medulla session is one session for the whole company, and the
//! `orch:usage` frame carries no teammate id, so cycle-level usage is charged to
//! [`UNATTRIBUTED_AGENT`] rather than to the teammate that answered. Totals,
//! cost, and the daily series are exact; only the "tokens by teammate"
//! breakdown lumps hosted usage into one row. Fixing that needs an agent field
//! on the wire frame *and* a per-agent breakdown on `CycleResult` — deliberately
//! out of scope here.

use crate::metering::ModelSlug;
use crate::ports::types::{CompanyId, LedgerEntry, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

/// The agent a cycle's usage is charged to when the cognition path cannot name
/// one teammate — the whole-company bucket.
///
/// Better than dropping the sample: the tokens were genuinely spent, so the
/// totals must include them even when the breakdown cannot attribute them.
pub const UNATTRIBUTED_AGENT: &str = "company";

/// The provider slug hosted Medulla cognition is metered under.
pub const MEDULLA_PROVIDER: &str = "medulla";

/// The `LedgerEntry::kind` an inference charge posts to Finances. Shared with
/// `harness::cost` so the two writers cannot drift into two spend categories for
/// one kind of spend.
pub const INFERENCE_SPEND_KIND: &str = "inference.spend";

/// Builds the `inference.spend` [`LedgerEntry`] for a cycle, or `None` when the
/// cycle cost nothing.
///
/// Gated on **cost**, not tokens: a managed `/openai/v1` passthrough reports
/// tokens but bills backend-side and echoes no USD, so a token-bearing
/// zero-cost cycle must not post a meaningless `$0.00` spend line to Finances.
/// Its tokens still land on the Usage surface through [`inference_sample`].
///
/// # Sign (issue #1047)
///
/// The amount is posted **negative**. Inference is an outflow, and the ledger's
/// convention — stated in [`finances`](super::finances) and implemented by
/// `finances_from` — is that outflows are negative and inflows positive. This
/// posted `usage.cost_usd` unnegated, so the projection read every model charge
/// as income: `revenueUsd` and `balanceUsd` grew with spending while `spentUsd`
/// and the `byCategory` breakdown stayed empty.
///
/// Negated **here**, at the one constructor every writer goes through
/// (`harness::cost`, `metering::planning`, `metering::triage`,
/// `metering::workflow_build`), rather than at the reader — the x402 outflow in
/// `economy::adapter` already posts negative, so inverting `finances_from`
/// would have meant changing that and both server fixtures to match a
/// convention the docs already state correctly.
///
/// `usage.cost_usd` is a non-zero cost, so the result is strictly negative; the
/// zero case returned above and never reaches here.
pub fn inference_ledger_entry(usage: &TokenUsage, agent: &str) -> Option<LedgerEntry> {
    if usage.cost_usd == 0.0 {
        return None;
    }
    Some(LedgerEntry {
        at_millis: now_millis(),
        kind: INFERENCE_SPEND_KIND.to_string(),
        amount_usd: -usage.cost_usd,
        memo: agent.to_string(),
    })
}

/// Builds the [`UsageSample`] for a metered inference unit (a harness turn or a
/// brain cycle), or `None` for a zero-usage one.
///
/// `model` is the classified [`ModelSlug`] this unit's spend went to, or `None`
/// when the cognition path cannot name one (issue #1749). It is a parameter of
/// the *shared* mapping rather than something the harness stamps afterwards —
/// the way [`run_id`](UsageSample::run_id) is — because every path that reaches
/// here ran a model: the cycle paths read it off
/// [`Cognition::model`](crate::ports::brain::Cognition::model) and the harness
/// off the live provider. A `None` here is a path that genuinely could not
/// identify one, which is a fact worth carrying rather than a gap to fill in.
pub fn inference_sample(
    usage: &TokenUsage,
    agent: &str,
    provider: &str,
    model: Option<ModelSlug>,
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
        run_id: None,
        model,
    })
}

/// Records one cycle's inference usage: the `inference.spend` ledger entry (when
/// the cycle cost USD) and the usage sample (when it moved tokens or money).
///
/// A zero-usage cycle is a no-op — an idle cycle, the offline echo brain, or the
/// harness brain (which meters itself per turn and deliberately reports zero
/// here so the spend is never counted twice).
///
/// Both writes are logged-and-swallowed: see the module docs.
pub async fn record_inference_usage(
    usage: &TokenUsage,
    agent: &str,
    provider: &str,
    model: Option<ModelSlug>,
    company: &CompanyId,
    store: &dyn CompanyStore,
    meter: &dyn UsageMeter,
) {
    if usage.is_zero() {
        return;
    }
    tracing::debug!(
        company = %company,
        agent = %agent,
        provider = %provider,
        input = usage.input,
        output = usage.output,
        cached_input = usage.cached_input,
        cost_usd = usage.cost_usd,
        "[usage] recording cycle inference usage"
    );
    if let Some(entry) = inference_ledger_entry(usage, agent)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            agent = %agent,
            error = %err,
            "[usage] failed to append the inference spend entry; the cycle itself succeeded"
        );
    }
    if let Some(sample) = inference_sample(usage, agent, provider, model)
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            agent = %agent,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record an inference sample; the cycle itself succeeded"
        );
    }
}

#[cfg(test)]
mod tests {
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
            None,
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
            None,
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
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
            Ok(self.samples.lock().unwrap().clone())
        }
    }

    struct FailingMeter;

    #[async_trait]
    impl UsageMeter for FailingMeter {
        async fn record(&self, _company: &CompanyId, _sample: &UsageSample) -> crate::Result<()> {
            Err(OpenCompanyError::Store("disk on fire".to_string()))
        }
        async fn query(
            &self,
            _company: &CompanyId,
            _since: u64,
        ) -> crate::Result<Vec<UsageSample>> {
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
}
