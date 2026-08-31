//! Emitting [`SampleKind::AuthoringCall`] usage samples — what one drafted
//! teammate mandate or persona costs, and who it is charged to (issue #1776).
//!
//! A draft is one tool-less model call ([`crate::harness::profile_draft`]) that
//! writes nothing: it hands the operator a suggestion they keep or throw away.
//! The tokens are real either way, so they must reach the meter — including on
//! a draft that was discarded, because the provider billed it the same.
//!
//! ## The company pays, not the teammate the draft is about
//!
//! This pass names exactly one teammate, so attributing it to that teammate is
//! the obvious move and it is wrong twice over.
//!
//! * **The teammate did not run.** No turn, no tools, no output of its own —
//!   the operator was describing it, not asking it for anything. A sample on its
//!   column would make "how much did Maya spend?" answer with work Maya never
//!   did.
//! * **It would eat that teammate's daily cap** (issue #304). A cap exists to
//!   bound what a teammate *does*; letting an operator exhaust it by pressing
//!   Draft would mean the better you describe a teammate, the less it can work.
//!
//! [`UNATTRIBUTED_AGENT`] is the honest answer, exactly as it is for
//! [`planning`](super::planning) and [`roster_build`](super::roster_build).
//! The tokens still count toward the capability-tier ceiling (issue #108)
//! through [`tokens_in`](super::capability::tokens_in): drafting is
//! company-driven model spend, and a kind excluded there would let an operator
//! keep drafting past the ceiling that was supposed to stop them.
//!
//! ## No run, so no `run_id`
//!
//! Like a planning pass and unlike a builder pass, a draft mints no
//! [`RunRecord`](crate::ports::runs::RunRecord): nothing for an operator to
//! steer, cancel or trace. `run_id: None` is the truth rather than a gap.
//!
//! ## Why it lives here (always compiled) and not in the harness
//!
//! The argument [`planning`](super::planning) and
//! [`roster_build`](super::roster_build) both make: the pass itself is behind
//! the non-default `openhuman` feature, which CI's default lane never compiles.
//! Keeping the sample shape, the attribution rule and the zero-usage guard here
//! — beside the aggregation that reads them — means this contract is unit-tested
//! on every CI run, and the pass is a thin delegation over it.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_profile_draft_usage`] logs and swallows both writes. The tokens were
//! spent before it was called, and a full disk must not turn a suggestion the
//! operator is about to read into a failed request.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::ports::types::{CompanyId, TokenUsage};
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};
use crate::ports::{CompanyStore, now_millis};

use super::inference::{UNATTRIBUTED_AGENT, inference_ledger_entry};

/// Tokens promised to drafts that are running but have not yet been metered.
///
/// The ceiling check reads the meter, which can only report what has already
/// *finished*. Two drafts started together — the mandate copilot and the
/// persona copilot are separately openable, so this is a click apart, not a
/// stress test — both read the same pre-call total, both find room, and both
/// spend. The tenant lands past a hard ceiling that refused everything else.
///
/// The harness has the same check-then-spend shape in `total_ceiling_refusal`
/// and does not need this: a dispatch is serialised by the delegation queue's
/// claim, so its checks cannot interleave. Drafting has no queue, which is
/// precisely why it needs a promise instead of one.
///
/// **Process-global, and that is the limit worth stating.** It bounds spend
/// within one host, which is the whole population for a desktop or a
/// single-container tenant. Replicas of one company would each hold their own
/// map, so this narrows the window rather than closing it everywhere — closing
/// it across replicas needs a reservation the *meter* can see, which is a
/// larger change than the leak currently justifies.
static IN_FLIGHT: LazyLock<Mutex<HashMap<CompanyId, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A promise to spend at most this many tokens, released when it is dropped.
///
/// Held across the model call and dropped when the draft finishes — including
/// on the paths that never reached a provider, because `Drop` does not care why
/// the call ended. A leaked promise would refuse a company its own budget until
/// the process restarted, so nothing here returns a raw number to release by
/// hand.
#[derive(Debug)]
pub struct DraftBudget {
    company: CompanyId,
    tokens: u64,
}

impl Drop for DraftBudget {
    fn drop(&mut self) {
        let mut in_flight = match IN_FLIGHT.lock() {
            Ok(guard) => guard,
            // A poisoned map means another thread panicked mid-update. Taking
            // the inner value is right for a counter of *in-flight* work: the
            // alternative is to leak this reservation forever and refuse the
            // company its budget until restart.
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(total) = in_flight.get_mut(&self.company) {
            *total = total.saturating_sub(self.tokens);
            if *total == 0 {
                in_flight.remove(&self.company);
            }
        }
    }
}

/// Promises `tokens` against the ceiling, or `None` when there is no room.
///
/// `spent` is what the meter reported, which counts only finished work; the
/// promises other drafts are holding are added to it here. The check and the
/// promise happen under one lock, which is the whole point — two callers that
/// both read the same `spent` cannot both find room, because the first has
/// recorded its claim before the second looks.
///
/// `tokens` is the field's *output ceiling*, not an estimate: reserving the
/// most a draft could spend is what makes the promise an upper bound, so real
/// usage can only ever come in under it.
pub fn reserve_draft(
    company: &CompanyId,
    tokens: u64,
    spent: u64,
    plan: &super::CapabilityPlan,
) -> Option<DraftBudget> {
    let mut in_flight = match IN_FLIGHT.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let promised = in_flight.get(company).copied().unwrap_or(0);
    if plan.total_exhausted(spent.saturating_add(promised)) {
        return None;
    }
    *in_flight.entry(company.clone()).or_insert(0) += tokens;
    Some(DraftBudget {
        company: company.clone(),
        tokens,
    })
}

/// Builds the [`SampleKind::AuthoringCall`] sample for one completed draft, or
/// `None` when the pass moved no tokens and cost nothing.
///
/// The `None` case is every refusal that never reached a provider — a company
/// with no model wired, a call that timed out before it landed. Writing a zero
/// row for those would put a drafting cost in the Usage view for a company that
/// never spent anything.
///
/// `agent` is not a parameter, and the teammate the draft is *about* is
/// deliberately not reachable from here — see the module docs. No caller can
/// bill a draft to the teammate it describes.
///
/// `model` is the classified [`ModelSlug`](crate::metering::ModelSlug) the turn
/// ran against, or `None` when the caller cannot name one (issue #1749).
pub fn profile_draft_sample(
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
        kind: SampleKind::AuthoringCall,
        run_id: None,
        model,
    })
}

/// Records one completed draft: the Finances ledger entry (when it cost USD)
/// and the usage sample (when it moved tokens or money).
///
/// The ledger entry goes through the same [`inference_ledger_entry`] every other
/// model call uses, under the same `inference.spend` kind — drafting spend is
/// inference spend as far as the money is concerned, and a separate Finances
/// category would split one line item for a distinction only the *usage*
/// breakdown cares about.
///
/// Both writes are logged-and-swallowed: see the module docs.
pub async fn record_profile_draft_usage(
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
        "[usage] recording an authoring draft"
    );
    if let Some(entry) = inference_ledger_entry(usage, UNATTRIBUTED_AGENT)
        && let Err(err) = store.append_ledger(company, entry).await
    {
        tracing::warn!(
            company = %company,
            error = %err,
            "[usage] failed to append the draft spend entry; the draft itself was returned"
        );
    }
    if let Some(sample) = profile_draft_sample(usage, provider, model)
        && let Err(err) = meter.record(company, &sample).await
    {
        tracing::warn!(
            company = %company,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record a draft sample; the draft itself was returned"
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn usage_with(cost: f64) -> TokenUsage {
        TokenUsage {
            input: 400,
            output: 120,
            cached_input: 20,
            cost_usd: cost,
        }
    }

    /// The company pays and no teammate does — the rule this module exists to
    /// hold, asserted rather than described.
    #[test]
    fn a_draft_is_charged_to_the_company_and_to_no_teammate() {
        let sample =
            profile_draft_sample(&usage_with(0.02), "managed", None).expect("a real draft meters");
        assert_eq!(sample.agent, UNATTRIBUTED_AGENT);
        assert_eq!(sample.kind, SampleKind::AuthoringCall);
        assert_eq!(sample.run_id, None, "a draft mints no run");
        assert_eq!(sample.input_tokens, 400);
        assert_eq!(sample.output_tokens, 120);
    }

    /// Its own kind, so "what does onboarding cost?" stays answerable — the
    /// question [`SampleKind::SetupCall`] was split out to keep answerable.
    #[test]
    fn a_draft_is_not_filed_as_a_setup_pass() {
        let sample = profile_draft_sample(&usage_with(0.02), "managed", None).expect("sample");
        assert_ne!(sample.kind, SampleKind::SetupCall);
        assert_ne!(sample.kind, SampleKind::Inference);
    }

    /// A pass that never reached a provider cost nothing, and a zero row would
    /// report drafting spend for a company that has none.
    #[test]
    fn a_pass_that_spent_nothing_writes_no_row() {
        assert!(profile_draft_sample(&TokenUsage::default(), "managed", None).is_none());
    }

    /// Drafting counts toward the tier ceiling: an excluded kind would let an
    /// operator keep pressing Draft past the budget that stopped everything
    /// else.
    #[test]
    fn a_draft_counts_toward_the_tier_ceiling() {
        let sample = profile_draft_sample(&usage_with(0.02), "managed", None).expect("sample");
        assert_eq!(super::super::capability::tokens_in(&[sample]), 520);
    }
}
