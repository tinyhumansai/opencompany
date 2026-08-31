//! Emitting [`SampleKind::OauthCall`] usage samples — the write half of the
//! Usage view's calls-by-provider chart.
//!
//! [`bucket_usage`](super::bucket_usage) has always summed `OauthCall` samples
//! into `oauthCalls`, `byProvider`, and `connections`, but nothing ever emitted
//! one, so those three counters were structurally zero however much
//! connected-tool activity a company had. This module is the emit side.
//!
//! ## Why it lives here (always compiled) and not at the call site
//!
//! The connected-tool execution sites are behind non-default features
//! (`composio`, `mcp`), and CI builds only the default feature set — so code
//! written *at* those sites is neither compiled nor tested by CI. Keeping the
//! sample shape and the record-and-swallow behaviour here, beside the
//! aggregation that reads it, means the contract is unit-tested on every CI run
//! and the gated call site stays a single line.
//!
//! ## Metering never fails the work it meters
//!
//! [`record_oauth_call`] logs and swallows a meter error rather than
//! propagating it. A usage sample is accounting, not the operator's tool call:
//! a full disk or a transient store fault must not turn a Gmail send that
//! already happened into a tool-call failure the agent then retries.

use crate::ports::types::CompanyId;
use crate::ports::usage::{SampleKind, UsageMeter, UsageSample};

/// The provider slug recorded when a call site cannot name its provider.
///
/// Better than dropping the sample: the call genuinely happened, so
/// `oauthCalls` should count it even when the breakdown cannot attribute it.
pub const UNKNOWN_PROVIDER: &str = "unknown";

/// Normalises a provider slug for grouping: trimmed and lowercased, falling
/// back to [`UNKNOWN_PROVIDER`] when empty.
///
/// `byProvider` groups on the raw string, so `GitHub` and `github` would
/// otherwise land as two rows — and, because `connections` is the *count* of
/// those rows, inflate the connection count for a single connected account.
pub fn normalize_provider(provider: &str) -> String {
    let trimmed = provider.trim();
    if trimmed.is_empty() {
        return UNKNOWN_PROVIDER.to_string();
    }
    trimmed.to_ascii_lowercase()
}

/// The namespace prefix for a remote MCP server's provider slug.
///
/// Composio toolkit slugs and MCP server names are two different namespaces
/// that `by_provider` would otherwise merge, because it groups on one flat
/// string. A company with a Composio `gmail` toolkit and an MCP server its
/// operator also called `gmail` would see one row carrying the sum of both —
/// and, since the `connections` KPI is that map's *row count*, would be told it
/// has one connection where it has two (issue #698).
pub const MCP_PROVIDER_PREFIX: &str = "mcp:";

/// The provider slug recorded for a call through a remote MCP server.
///
/// Namespaced rather than bare, so the row cannot collide with a Composio
/// toolkit of the same name. The prefix is part of the grouping key, which
/// means it is also what an operator reads in the calls-by-provider chart —
/// deliberately, because "which of my two `gmail` connections was this?" is a
/// question the chart could not otherwise answer.
///
/// An empty or whitespace-only server name yields a bare [`UNKNOWN_PROVIDER`]
/// rather than a prefixed one. A sample that cannot name its server is not
/// evidence about MCP specifically, and minting `mcp:unknown` would invent a
/// connection row for a server that was never identified.
pub fn mcp_provider(server: &str) -> String {
    let normalized = normalize_provider(server);
    if normalized == UNKNOWN_PROVIDER {
        return normalized;
    }
    format!("{MCP_PROVIDER_PREFIX}{normalized}")
}

/// Builds the [`UsageSample`] for one connected-tool invocation.
///
/// Carries no tokens and no cost: an OAuth call is counted, not billed by token
/// — the money for a connected tool moves at the provider, not through our
/// inference meter. Keeping the token fields at zero is what lets the sample
/// share a stream with inference samples without polluting the token series.
pub fn oauth_call_sample(agent: &str, provider: &str, at_millis: u64) -> UsageSample {
    UsageSample {
        at_millis,
        agent: agent.to_string(),
        provider: normalize_provider(provider),
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cost_usd: 0.0,
        kind: SampleKind::OauthCall,
        run_id: None,
        model: None,
    }
}

/// Records one connected-tool invocation against the company's usage meter.
///
/// Call this **after** a connected tool has actually reached its provider, so
/// the counters reflect calls that happened rather than calls that were
/// attempted. A meter failure is logged and swallowed — see the module docs.
pub async fn record_oauth_call(
    meter: &dyn UsageMeter,
    company: &CompanyId,
    agent: &str,
    provider: &str,
    at_millis: u64,
) {
    let sample = oauth_call_sample(agent, provider, at_millis);
    if let Err(err) = meter.record(company, &sample).await {
        tracing::warn!(
            company = %company,
            agent = %agent,
            provider = %sample.provider,
            error = %err,
            "[usage] failed to record an OAuth-call sample; the tool call itself succeeded"
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

    /// A meter whose writes always fail — proves metering cannot fail the work.
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
    fn sample_is_a_zero_token_zero_cost_oauth_call() {
        let s = oauth_call_sample("ceo", "gmail", 1_700);
        assert_eq!(s.kind, SampleKind::OauthCall);
        assert_eq!(s.agent, "ceo");
        assert_eq!(s.provider, "gmail");
        assert_eq!(s.at_millis, 1_700);
        // An OAuth call is counted, never token-billed — zeros here keep it out
        // of the token series it shares a stream with.
        assert_eq!(s.input_tokens, 0);
        assert_eq!(s.output_tokens, 0);
        assert_eq!(s.cached_input_tokens, 0);
        assert_eq!(s.cost_usd, 0.0);
    }

    #[test]
    fn provider_is_normalized_so_one_account_is_one_connection() {
        assert_eq!(normalize_provider("GitHub"), "github");
        assert_eq!(normalize_provider("  gmail  "), "gmail");
        assert_eq!(normalize_provider("SLACK"), "slack");
    }

    #[test]
    fn blank_provider_falls_back_rather_than_dropping_the_call() {
        assert_eq!(normalize_provider(""), UNKNOWN_PROVIDER);
        assert_eq!(normalize_provider("   "), UNKNOWN_PROVIDER);
        assert_eq!(oauth_call_sample("ceo", "", 1).provider, UNKNOWN_PROVIDER);
    }

    #[tokio::test]
    async fn record_writes_one_sample_per_call() {
        let meter = RecordingMeter::default();
        let company = CompanyId::new("acme");
        record_oauth_call(&meter, &company, "ceo", "GMAIL", 1_000).await;
        record_oauth_call(&meter, &company, "ceo", "gmail", 2_000).await;

        let samples = meter.samples.lock().unwrap();
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|s| s.kind == SampleKind::OauthCall));
        // Both spellings collapse onto one provider, so this reads as one
        // connection with two calls rather than two connections with one each.
        assert!(samples.iter().all(|s| s.provider == "gmail"));
    }

    #[tokio::test]
    async fn a_meter_failure_never_surfaces_to_the_caller() {
        // The tool call already reached the provider; failing here would make
        // the agent believe a completed action failed, and retry it.
        record_oauth_call(&FailingMeter, &CompanyId::new("acme"), "ceo", "gmail", 1).await;
    }

    /// The emitted sample must survive the aggregation that reads it — the
    /// whole point of the issue is that these three counters stop being zero.
    #[test]
    fn emitted_samples_reach_the_console_counters() {
        use std::collections::HashMap;

        use crate::metering::{UsageRange, bucket_usage};

        let now = 1_700_000_000_000u64;
        let samples = vec![
            oauth_call_sample("ceo", "gmail", now),
            oauth_call_sample("ceo", "gmail", now),
            oauth_call_sample("ops", "github", now),
        ];
        let usage = bucket_usage(&samples, UsageRange::D7, now, &HashMap::new());

        assert_eq!(usage.totals.oauth_calls, 3);
        assert_eq!(usage.totals.connections, 2);
        assert_eq!(usage.by_provider.len(), 2);
        assert_eq!(usage.by_provider[0].provider, "gmail");
        assert_eq!(usage.by_provider[0].calls, 2);
        assert_eq!(usage.by_provider[1].provider, "github");
        assert_eq!(usage.by_provider[1].calls, 1);
        // OAuth calls carry no tokens, so they never distort the token series.
        assert_eq!(usage.totals.tokens, 0);
    }

    #[test]
    fn an_mcp_server_is_namespaced_away_from_composio_toolkits() {
        assert_eq!(mcp_provider("linear"), "mcp:linear");
        // Same normalisation as every other provider: the prefix does not
        // exempt a server name from the case folding that stops `Linear` and
        // `linear` becoming two connections.
        assert_eq!(mcp_provider("  LINEAR "), "mcp:linear");
    }

    #[test]
    fn an_unnamed_server_does_not_mint_a_phantom_mcp_connection() {
        // `mcp:unknown` would be a connection row for a server nobody can
        // point at. Falling back to the bare slug keeps the call counted in
        // `oauthCalls` without inventing an MCP connection in `byProvider`.
        assert_eq!(mcp_provider(""), UNKNOWN_PROVIDER);
        assert_eq!(mcp_provider("   "), UNKNOWN_PROVIDER);
    }

    /// The collision issue #698 asks to prevent, asserted through the
    /// aggregation rather than on the string, because the string is not the
    /// claim — "two connections, counted separately" is.
    #[test]
    fn a_composio_toolkit_and_an_mcp_server_of_one_name_stay_two_connections() {
        use std::collections::HashMap;

        use crate::metering::{UsageRange, bucket_usage};

        let now = 1_700_000_000_000u64;
        let samples = vec![
            // The Composio toolkit: bare slug, as `composio_execute` emits it.
            oauth_call_sample("ceo", "gmail", now),
            // An MCP server the operator also named `gmail`.
            oauth_call_sample("ceo", &mcp_provider("gmail"), now),
        ];
        let usage = bucket_usage(&samples, UsageRange::D7, now, &HashMap::new());

        assert_eq!(usage.totals.oauth_calls, 2);
        assert_eq!(
            usage.totals.connections, 2,
            "an MCP server and a Composio toolkit sharing a name are two \
             connections; merging them under-reports the KPI"
        );
        let providers: Vec<&str> = usage
            .by_provider
            .iter()
            .map(|p| p.provider.as_str())
            .collect();
        assert!(providers.contains(&"gmail"), "{providers:?}");
        assert!(providers.contains(&"mcp:gmail"), "{providers:?}");
    }
}
