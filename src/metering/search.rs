//! Emitting [`SampleKind::SearchCall`] usage samples — the write half of the
//! Usage view's `searchCalls` counter and the cost the managed search backend
//! charged (issue #238).
//!
//! ## Why this is not an `OauthCall`
//!
//! [`oauth_call_sample`](super::oauth::oauth_call_sample) hardcodes
//! `cost_usd: 0.0` **by design** — a connected-tool call is counted, not billed
//! through our meter, because the money moves at the provider. A managed web
//! search is the opposite shape: the tinyhumans backend charges the platform
//! per request and reports the amount back on the response, exactly the way
//! managed inference does. Recording it as an `OauthCall` would therefore throw
//! away the only cost figure we have *and* mint a phantom row in the
//! calls-by-provider chart (whose row count **is** the connections KPI), so a
//! company that has connected no account would read as having one. Hence a
//! distinct [`SampleKind`] with its own counter, and a `by_provider` chart that
//! stays OAuth-only.
//!
//! ## Why it lives here (always compiled)
//!
//! Same reason as [`oauth`](super::oauth): the emit site is inside the
//! feature-gated harness, so keeping the sample shape here — beside the
//! aggregation that reads it — means the contract is unit-tested on the default
//! CI build and the gated call site stays one line.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_search_call`] logs and swallows a meter error. The search already
//! happened and the backend already charged for it; a full disk must not turn a
//! completed search into a tool-call failure the agent then retries (and pays
//! for twice).

use crate::ports::types::CompanyId;
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};

use super::oauth::normalize_provider;

/// The provider slug recorded when the backend does not name the upstream
/// engine it resolved a search to.
///
/// `managed` matches the inference samples' provider slug, so the two priced
/// managed surfaces read as one platform rather than as an unattributed
/// `unknown`.
pub const MANAGED_SEARCH_PROVIDER: &str = "managed";

/// The cost attributed to a completed search when the backend reports none.
///
/// The managed search path is priced per request (OpenHuman's Parallel
/// integration documents ~$0.01/request), and every response carries a
/// `costUsd`. An older or degraded backend can still answer with `0`, and a
/// completed *paid* call recorded at zero cost is worse than a slightly wrong
/// number: it makes the Usage view claim searches are free, which is the exact
/// failure mode the issue set out to end ("a paid call is never free"). So a
/// non-positive reported cost floors to this documented list price rather than
/// to zero.
pub const FALLBACK_SEARCH_COST_USD: f64 = 0.01;

/// The USD cost to attribute to one completed search, given what the backend
/// reported. See [`FALLBACK_SEARCH_COST_USD`] for why zero is never recorded.
pub fn attributed_cost_usd(reported: f64) -> f64 {
    if reported.is_finite() && reported > 0.0 {
        reported
    } else {
        FALLBACK_SEARCH_COST_USD
    }
}

/// Builds the [`UsageSample`] for one **completed** web search.
///
/// Carries no tokens (a search consumes none) but a real `cost_usd`, so it
/// rolls into the window's cost total the way an inference sample does while
/// staying out of the token series and the tokens-by-teammate chart.
pub fn search_call_sample(
    agent: &str,
    provider: &str,
    reported_cost_usd: f64,
    at_millis: u64,
) -> UsageSample {
    let provider = normalize_provider(provider);
    let provider = if provider == super::oauth::UNKNOWN_PROVIDER {
        MANAGED_SEARCH_PROVIDER.to_string()
    } else {
        provider
    };
    UsageSample {
        at_millis,
        agent: agent.to_string(),
        provider,
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cost_usd: attributed_cost_usd(reported_cost_usd),
        kind: SampleKind::SearchCall,
        run_id: None,
        model: None,
    }
}

/// Records one completed web search against the company's usage meter.
///
/// Call this **only after** the backend has answered — a search that failed
/// (transport error, non-2xx, budget refusal) records nothing, because nothing
/// was charged. A meter failure is logged and swallowed; see the module docs.
pub async fn record_search_call(
    meter: &dyn UsageMeter,
    company: &CompanyId,
    agent: &str,
    provider: &str,
    reported_cost_usd: f64,
    at_millis: u64,
) {
    let sample = search_call_sample(agent, provider, reported_cost_usd, at_millis);
    if let Err(err) = meter.record(company, &sample).await {
        tracing::warn!(
            company = %company,
            agent = %agent,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record a search-call sample; the search itself succeeded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::error::OpenCompanyError;

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

    #[test]
    fn a_search_sample_is_token_less_but_never_cost_less() {
        let s = search_call_sample("ceo", "Exa", 0.013, 1_700);
        assert_eq!(s.kind, SampleKind::SearchCall);
        assert_eq!(s.agent, "ceo");
        // Provider is normalised the same way OAuth providers are, so `Exa` and
        // `exa` are one row rather than two.
        assert_eq!(s.provider, "exa");
        assert_eq!(s.at_millis, 1_700);
        assert_eq!(s.input_tokens, 0);
        assert_eq!(s.output_tokens, 0);
        assert!((s.cost_usd - 0.013).abs() < 1e-9);
    }

    /// The headline invariant: a completed search can never be recorded free.
    #[test]
    fn a_backend_that_reports_no_cost_still_yields_a_priced_sample() {
        for reported in [0.0, -1.0, f64::NAN] {
            let s = search_call_sample("ceo", "", reported, 1);
            assert!(
                s.cost_usd > 0.0,
                "a paid call recorded at {reported} cost must still be priced"
            );
            assert!((s.cost_usd - FALLBACK_SEARCH_COST_USD).abs() < 1e-9);
        }
    }

    /// An unnamed provider attributes to the managed platform, not `unknown` —
    /// the search *did* run on the managed surface, and `managed` is the slug
    /// the inference samples already use.
    #[test]
    fn an_unnamed_provider_attributes_to_the_managed_platform() {
        assert_eq!(
            search_call_sample("ceo", "   ", 0.01, 1).provider,
            MANAGED_SEARCH_PROVIDER
        );
    }

    #[tokio::test]
    async fn record_writes_one_sample_per_completed_search() {
        let meter = RecordingMeter::default();
        let company = CompanyId::new("acme");
        record_search_call(&meter, &company, "ceo", "Exa", 0.01, 1_000).await;
        record_search_call(&meter, &company, "ceo", "Exa", 0.01, 2_000).await;

        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|s| s.kind == SampleKind::SearchCall));
    }

    #[tokio::test]
    async fn a_meter_failure_never_surfaces_to_the_caller() {
        // The backend already charged for this search; failing here would make
        // the agent believe a completed search failed, and pay for a retry.
        record_search_call(
            &FailingMeter,
            &CompanyId::new("acme"),
            "ceo",
            "exa",
            0.01,
            1,
        )
        .await;
    }

    /// The emitted sample must survive the aggregation that reads it: it counts
    /// in `searchCalls`, its cost rolls into the window total, and it neither
    /// mints a connection nor distorts the token charts.
    #[test]
    fn emitted_samples_reach_the_console_counters_without_faking_a_connection() {
        use std::collections::HashMap;

        use crate::metering::{UsageRange, bucket_usage};

        let now = 1_700_000_000_000u64;
        let samples = vec![
            search_call_sample("ceo", "Exa", 0.01, now),
            search_call_sample("ceo", "Exa", 0.02, now),
        ];
        let usage = bucket_usage(&samples, UsageRange::D7, now, &HashMap::new());

        assert_eq!(usage.totals.search_calls, 2);
        assert!((usage.totals.cost_usd - 0.03).abs() < 1e-9);
        // Searches are not connected accounts: the connections KPI and the
        // calls-by-provider chart must stay untouched.
        assert_eq!(usage.totals.oauth_calls, 0);
        assert_eq!(usage.totals.connections, 0);
        assert!(usage.by_provider.is_empty());
        // And they carry no tokens, so no teammate appears as a zero-token bar.
        assert_eq!(usage.totals.tokens, 0);
        assert!(usage.by_agent.is_empty());
    }
}
