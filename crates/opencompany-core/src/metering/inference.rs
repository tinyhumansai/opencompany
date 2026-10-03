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
/// `metering::workflow_build`), rather than at the reader — inverting
/// `finances_from` would have meant changing both server fixtures to match a
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
#[path = "inference_tests.rs"]
mod tests;
