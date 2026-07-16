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
//! ## WS5 seam
//!
//! opencompany does not yet ship a `UsageMeter` port (it lands with WS3/WS5), so
//! this module defines a **minimal local [`UsageMeter`] trait + [`UsageSample`]**
//! to keep the cost hook whole and testable. When WS5 lands its real port, this
//! seam should be replaced by (or re-exported from) `crate::ports`. The ledger
//! half already writes through the real [`CompanyStore`] port.

use async_trait::async_trait;

use crate::ports::CompanyStore;
use crate::ports::now_millis;
use crate::ports::types::{CompanyId, LedgerEntry};

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
    /// Number of model calls made during the turn.
    pub call_count: u32,
}

/// What produced a usage sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    /// Tokens consumed by a model inference call.
    Inference,
    /// An OAuth-connected tool invocation (populates the calls-by-provider
    /// chart). Wired by the runtime when a connected tool runs.
    OauthCall,
}

/// One metered usage event. The WS5 seam type — kept minimal until the real
/// `UsageMeter` port lands.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSample {
    /// The agent that produced the usage.
    pub agent: String,
    /// The inference/tool provider slug (e.g. `managed`, `github`).
    pub provider: String,
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens produced.
    pub output_tokens: u64,
    /// Input tokens served from the KV cache.
    pub cached_input_tokens: u64,
    /// USD cost attributed to the sample.
    pub cost_usd: f64,
    /// What produced the sample.
    pub kind: SampleKind,
}

/// Minimal usage sink — the WS5 `UsageMeter` seam.
#[async_trait]
pub trait UsageMeter: Send + Sync {
    /// Records a single usage sample.
    async fn record(&self, sample: UsageSample) -> crate::Result<()>;
}

/// Build the `inference.spend` [`LedgerEntry`] for a turn, or `None` when the
/// turn consumed nothing (no model calls / no cost).
pub fn ledger_entry_for(turn: &TurnUsage, agent_id: &str) -> Option<LedgerEntry> {
    if is_zero_usage(turn) {
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
        agent: agent_id.to_string(),
        provider: provider.to_string(),
        input_tokens: turn.input_tokens,
        output_tokens: turn.output_tokens,
        cached_input_tokens: turn.cached_input_tokens,
        cost_usd: turn.cost_usd,
        kind: SampleKind::Inference,
    })
}

/// A turn is zero-usage when it made no calls and moved no tokens or cost.
fn is_zero_usage(turn: &TurnUsage) -> bool {
    turn.call_count == 0
        && turn.input_tokens == 0
        && turn.output_tokens == 0
        && turn.cost_usd == 0.0
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
        meter.record(sample).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
        async fn record(&self, sample: UsageSample) -> crate::Result<()> {
            self.samples.lock().unwrap().push(sample);
            Ok(())
        }
    }

    fn turn_with(cost: f64, calls: u32) -> TurnUsage {
        TurnUsage {
            input_tokens: 100,
            output_tokens: 50,
            cached_input_tokens: 10,
            cost_usd: cost,
            call_count: calls,
        }
    }

    #[test]
    fn zero_usage_turn_produces_no_entry_or_sample() {
        let turn = TurnUsage::default();
        assert!(ledger_entry_for(&turn, "ceo").is_none());
        assert!(usage_sample_for(&turn, "ceo", "managed").is_none());
    }

    #[test]
    fn spend_maps_to_inference_ledger_entry() {
        let turn = turn_with(0.42, 1);
        let entry = ledger_entry_for(&turn, "ceo").unwrap();
        assert_eq!(entry.kind, "inference.spend");
        assert_eq!(entry.amount_usd, 0.42);
        assert_eq!(entry.memo, "ceo");
    }

    #[tokio::test]
    async fn record_turn_cost_writes_ledger_and_sample() {
        let store = RecordingStore::default();
        let meter = RecordingMeter::default();
        let turn = turn_with(1.5, 2);
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
