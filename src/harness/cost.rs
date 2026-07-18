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

use crate::ports::CompanyStore;
use crate::ports::now_millis;
use crate::ports::types::{CompanyId, LedgerEntry};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};

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

/// Build the `inference.spend` [`LedgerEntry`] for a turn, or `None` when the
/// turn cost nothing.
///
/// Gated on **cost**, not tokens: the `/openai/v1` passthrough reports tokens
/// but bills backend-side and echoes no USD, so a token-bearing zero-cost turn
/// must not post a meaningless `$0.00` spend line to Finances. Its tokens are
/// still recorded through [`usage_sample_for`] on the Usage surface.
pub fn ledger_entry_for(turn: &TurnUsage, agent_id: &str) -> Option<LedgerEntry> {
    if turn.cost_usd == 0.0 {
        return None;
    }
    Some(LedgerEntry {
        at_millis: now_millis(),
        kind: "inference.spend".to_string(),
        amount_usd: turn.cost_usd,
        memo: agent_id.to_string(),
    })
}

/// Build the [`UsageSample`] for a turn, or `None` for a zero-usage turn.
pub fn usage_sample_for(turn: &TurnUsage, agent_id: &str, provider: &str) -> Option<UsageSample> {
    if is_zero_usage(turn) {
        return None;
    }
    Some(UsageSample {
        at_millis: now_millis(),
        agent: agent_id.to_string(),
        provider: provider.to_string(),
        input_tokens: turn.input_tokens,
        output_tokens: turn.output_tokens,
        cached_input_tokens: turn.cached_input_tokens,
        cost_usd: turn.cost_usd,
        kind: SampleKind::Inference,
    })
}

/// A turn is zero-usage when it moved no tokens and cost nothing — e.g. the
/// offline [`MockProvider`](super::provider::MockProvider), whose replies carry
/// no usage. Such a turn writes neither a ledger entry nor a usage sample.
fn is_zero_usage(turn: &TurnUsage) -> bool {
    turn.input_tokens == 0 && turn.output_tokens == 0 && turn.cost_usd == 0.0
}

/// Record a completed turn's cost: append the ledger entry (always available)
/// and, when a [`UsageMeter`] is wired, record the usage sample. A zero-usage
/// turn is a no-op.
pub async fn record_turn_cost(
    turn: &TurnUsage,
    agent_id: &str,
    provider: &str,
    company: &CompanyId,
    store: &dyn CompanyStore,
    meter: Option<&dyn UsageMeter>,
) -> crate::Result<()> {
    if let Some(entry) = ledger_entry_for(turn, agent_id) {
        store.append_ledger(company, entry).await?;
    }
    if let (Some(meter), Some(sample)) = (meter, usage_sample_for(turn, agent_id, provider)) {
        meter.record(company, &sample).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;

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
        assert!(usage_sample_for(&turn, "ceo", "managed").is_none());
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
        assert!(usage_sample_for(&turn, "ceo", "managed").is_some());
        // No USD ⇒ no ledger entry, but the token sample still lands.
        assert!(ledger_entry_for(&turn, "ceo").is_none());
    }

    #[test]
    fn spend_maps_to_inference_ledger_entry() {
        let turn = turn_with(0.42);
        let entry = ledger_entry_for(&turn, "ceo").unwrap();
        assert_eq!(entry.kind, "inference.spend");
        assert_eq!(entry.amount_usd, 0.42);
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
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
        )
        .await
        .unwrap();

        let ledger = store.ledger.lock().unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].amount_usd, 1.5);
        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].kind, SampleKind::Inference);
        assert_eq!(samples[0].output_tokens, 50);
    }

    #[tokio::test]
    async fn record_turn_cost_is_a_noop_for_zero_usage() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        record_turn_cost(
            &TurnUsage::default(),
            "ceo",
            "managed",
            &CompanyId::new("acme"),
            &store,
            Some(&meter),
        )
        .await
        .unwrap();
        assert!(store.ledger.lock().unwrap().is_empty());
        assert!(meter.samples.lock().unwrap().is_empty());
    }
}
