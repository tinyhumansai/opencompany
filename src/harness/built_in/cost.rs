//! Post-turn cost accounting: a turn's [`TurnUsage`] → ledger + usage meter.
//!
//! After each agent turn the harness maps the turn's token/cost totals
//! ([`TurnUsage`]) and writes two things:
//!
//! 1. a [`LedgerEntry`] with kind `inference.spend` through
//!    [`CompanyStore::append_ledger`] — this feeds the Finances surface;
//! 2. a [`UsageSample`] through the [`UsageMeter`] seam — this feeds the Usage
//!    surface (WS5).
//!
//! A **zero-usage turn writes nothing** — no ledger entry and no sample.
//!
//! ## Why a local [`TurnUsage`] (flagged openhuman seam)
//!
//! openhuman accumulates a turn's real cost in its own `TurnCost`, but both that
//! type (`agent::cost` is `pub(crate)`) and the accessor for a completed turn's
//! totals (`Agent::take_last_turn_usage_totals`, also `pub(crate)`) are
//! crate-private — a host crate can neither name the type nor read the numbers.
//! So this module carries its own [`TurnUsage`] mirror. When openhuman exposes a
//! public accessor, [`HarnessPool::run`](crate::harness::HarnessPool::run) fills
//! [`TurnUsage`] from it; the mapping below is unchanged.
//!
//! ## Usage port (WS3)
//!
//! The usage half writes through the canonical
//! [`UsageMeter`](crate::ports::UsageMeter) port shipped by WS3, mapping the
//! turn onto a [`UsageSample`](crate::ports::UsageSample); WS5 reads the meter
//! back for the Usage/Finances surfaces. The ledger half writes through the
//! real [`CompanyStore`] port.
//!
//! ## One mapping, two writers (issue #174)
//!
//! The sample/ledger shapes and the zero-usage guard live in
//! [`metering::inference`](crate::metering::inference), which is compiled and
//! tested by the default CI build; this module only converts [`TurnUsage`] into
//! the crate-wide [`TokenUsage`] and delegates. That is what keeps the harness's
//! per-turn metering and the runtime's per-cycle metering (every other cognition
//! path) from drifting into two different definitions of "usage worth recording".

use crate::metering::{ModelSlug, inference};
use crate::ports::CompanyStore;
use crate::ports::types::{CompanyId, LedgerEntry, TokenUsage};
use crate::ports::usage::{UsageMeter, UsageSample};

/// A turn's token/cost totals — a host-side mirror of openhuman's crate-private
/// `TurnCost` (see module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TurnUsage {
    /// Input/prompt tokens consumed across the turn's model calls.
    pub input_tokens: u64,
    /// Output/completion tokens produced.
    pub output_tokens: u64,
    /// Input tokens served from the KV cache.
    pub cached_input_tokens: u64,
    /// Best-available USD cost for the turn (charged + estimated).
    pub cost_usd: f64,
}

impl TurnUsage {
    /// This turn as the crate-wide [`TokenUsage`] the metering seam maps from.
    fn to_token_usage(self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens,
            output: self.output_tokens,
            cached_input: self.cached_input_tokens,
            cost_usd: self.cost_usd,
        }
    }
}

/// Build the `inference.spend` [`LedgerEntry`] for a turn, or `None` when the
/// turn cost nothing.
///
/// Gated on **cost**, not tokens: the `/openai/v1` passthrough reports tokens
/// but bills backend-side and echoes no USD, so a token-bearing zero-cost turn
/// must not post a meaningless `$0.00` spend line to Finances. Its tokens are
/// still recorded through [`usage_sample_for`] on the Usage surface.
pub fn ledger_entry_for(turn: &TurnUsage, agent_id: &str) -> Option<LedgerEntry> {
    inference::inference_ledger_entry(&turn.to_token_usage(), agent_id)
}

/// Build the [`UsageSample`] for a turn, or `None` for a zero-usage turn — e.g.
/// one served by the offline [`MockProvider`](super::provider::MockProvider),
/// whose replies carry no usage.
///
/// `model` is the classified [`ModelSlug`] the turn resolved to, read live off
/// the provider (issue #1749) — a vocabulary member, never the raw model name,
/// which stays inside
/// [`HarnessModel`](super::provider::HarnessModel). `None` when the provider
/// cannot name one.
///
/// `run_id` attributes the sample to the task attempt the turn ran under, when
/// it ran under one (issue #242). Stamped here rather than inside
/// [`inference::inference_sample`] because that function is the *shared* mapping
/// every cognition path uses and only the harness's per-turn path has a run to
/// name — widening it would push a `None` onto four call sites that can never
/// mean anything else.
pub fn usage_sample_for(
    turn: &TurnUsage,
    agent_id: &str,
    provider: &str,
    model: Option<ModelSlug>,
    run_id: Option<&str>,
) -> Option<UsageSample> {
    let mut sample =
        inference::inference_sample(&turn.to_token_usage(), agent_id, provider, model)?;
    sample.run_id = run_id.map(str::to_string);
    Some(sample)
}

/// Record a completed turn's cost: append the ledger entry (always available)
/// and, when a [`UsageMeter`] is wired, record the usage sample. A zero-usage
/// turn is a no-op.
#[allow(clippy::too_many_arguments)]
pub async fn record_turn_cost(
    turn: &TurnUsage,
    agent_id: &str,
    provider: &str,
    model: Option<ModelSlug>,
    company: &CompanyId,
    store: &dyn CompanyStore,
    meter: Option<&dyn UsageMeter>,
    run_id: Option<&str>,
) -> crate::Result<()> {
    if let Some(entry) = ledger_entry_for(turn, agent_id) {
        store.append_ledger(company, entry).await?;
    }
    if let (Some(meter), Some(sample)) = (
        meter,
        usage_sample_for(turn, agent_id, provider, model, run_id),
    ) {
        // The usage sample is telemetry, not the turn's record of itself: the
        // ledger write above has already happened, so a meter failure must not
        // fail a completed turn. Log it and let the turn stand.
        if let Err(error) = meter.record(company, &sample).await {
            tracing::warn!(
                company = %company,
                agent = %agent_id,
                provider = %provider,
                error = %error,
                "[cost] failed to record the usage sample; the turn still stands"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::ports::types::{CompanyRecord, CompanySummary};
    use crate::ports::usage::SampleKind;

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

    fn turn_with(cost: f64) -> TurnUsage {
        TurnUsage {
            input_tokens: 100,
            output_tokens: 50,
            cached_input_tokens: 10,
            cost_usd: cost,
        }
    }

    #[test]
    fn zero_usage_turn_produces_no_entry_or_sample() {
        let turn = TurnUsage::default();
        assert!(ledger_entry_for(&turn, "ceo").is_none());
        assert!(usage_sample_for(&turn, "ceo", "managed", None, None).is_none());
    }

    /// The `/openai/v1` passthrough reports tokens but no USD (billing happens
    /// backend-side, off the wire). A token-bearing, zero-cost turn is still
    /// real usage — it must produce a sample so the Usage surface is not blind.
    #[test]
    fn token_only_zero_cost_turn_is_not_zero_usage() {
        let turn = TurnUsage {
            input_tokens: 22,
            output_tokens: 2,
            cached_input_tokens: 0,
            cost_usd: 0.0,
        };
        assert!(usage_sample_for(&turn, "ceo", "managed", None, None).is_some());
        // No USD ⇒ no ledger entry, but the token sample still lands.
        assert!(ledger_entry_for(&turn, "ceo").is_none());
    }

    #[test]
    fn spend_maps_to_inference_ledger_entry() {
        let turn = turn_with(0.42);
        let entry = ledger_entry_for(&turn, "ceo").unwrap();
        assert_eq!(entry.kind, "inference.spend");
        // Negative: an outflow, per the ledger convention (issue #1047).
        assert_eq!(entry.amount_usd, -0.42);
        assert_eq!(entry.memo, "ceo");
    }

    #[tokio::test]
    async fn record_turn_cost_writes_ledger_and_sample() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let turn = turn_with(1.5);
        record_turn_cost(
            &turn,
            "ceo",
            "managed",
            None,
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
            None,
        )
        .await
        .unwrap();

        let ledger = store.ledger.lock().unwrap();
        assert_eq!(ledger.len(), 1);
        // Negative: an outflow, per the ledger convention (issue #1047).
        assert_eq!(ledger[0].amount_usd, -1.5);
        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].kind, SampleKind::Inference);
        assert_eq!(samples[0].output_tokens, 50);
    }

    /// Issue #242: a turn that ran under a dispatched attempt attributes its
    /// sample to that attempt, and one that did not stays unattributed. The
    /// ledger entry is untouched either way — money still moves through the same
    /// `inference.spend` line, so no accounting semantics change here.
    #[tokio::test]
    async fn a_sample_carries_the_attempt_its_turn_ran_under() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let turn = turn_with(0.5);

        record_turn_cost(
            &turn,
            "ceo",
            "managed",
            None,
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
            Some("run-7"),
        )
        .await
        .unwrap();
        // …and a chat turn, which belongs to no attempt.
        record_turn_cost(
            &turn,
            "ceo",
            "managed",
            None,
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
            None,
        )
        .await
        .unwrap();

        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples[0].run_id.as_deref(), Some("run-7"));
        assert_eq!(samples[1].run_id, None);
        // Attribution only — the spend itself is unchanged.
        let ledger = store.ledger.lock().unwrap();
        assert_eq!(ledger.len(), 2);
        assert!(ledger.iter().all(|e| e.amount_usd == -0.5));

        // The field is additive on the wire: an unattributed sample serializes
        // exactly as it did before #242.
        let plain = serde_json::to_string(&samples[1]).expect("serialize");
        assert!(!plain.contains("runId"), "{plain}");
        let tagged = serde_json::to_string(&samples[0]).expect("serialize");
        assert!(tagged.contains(r#""runId":"run-7""#), "{tagged}");
    }

    /// Issue #1749: the turn's sample names the model it ran on.
    ///
    /// This is the seam the issue is about — the richest one in the tree, with
    /// agent, provider, run id and totals already in scope — so a model that
    /// does not arrive here arrives nowhere.
    #[tokio::test]
    async fn a_turns_sample_names_the_model_it_ran_on() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let turn = turn_with(0.25);
        record_turn_cost(
            &turn,
            "ceo",
            "byok",
            Some(ModelSlug::classify("anthropic/claude-sonnet-4-6")),
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
            None,
        )
        .await
        .unwrap();

        let samples = meter.samples.lock().unwrap();
        assert_eq!(
            samples[0].model.map(|m| m.as_str()),
            Some("anthropic-sonnet"),
            "`provider` says who served the tokens; only `model` says what ran"
        );
    }

    /// The BYOK containment, end to end at this seam: an operator-named model
    /// is folded onto the fallback, and its raw name is nowhere in the sample
    /// the meter is handed.
    #[tokio::test]
    async fn an_operator_named_model_never_reaches_the_meter() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let turn = turn_with(0.25);
        record_turn_cost(
            &turn,
            "ceo",
            "byok",
            Some(ModelSlug::classify("northwind-legal-review-v2")),
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
            None,
        )
        .await
        .unwrap();

        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples[0].model, Some(ModelSlug::OTHER));
        let persisted = serde_json::to_string(&samples[0]).expect("serialize");
        assert!(
            !persisted.to_ascii_lowercase().contains("northwind"),
            "the operator's model name reached the meter: {persisted}"
        );
    }

    #[tokio::test]
    async fn record_turn_cost_is_a_noop_for_zero_usage() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        record_turn_cost(
            &TurnUsage::default(),
            "ceo",
            "managed",
            None,
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
            None,
        )
        .await
        .unwrap();
        assert!(store.ledger.lock().unwrap().is_empty());
        assert!(meter.samples.lock().unwrap().is_empty());
    }
}
