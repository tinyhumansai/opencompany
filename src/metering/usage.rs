//! Pure usage aggregation: raw [`UsageSample`]s → the console's [`Usage`] shape.
//!
//! No I/O. WS2's `graphql/usage.rs` resolver queries the
//! [`UsageMeter`](crate::ports::UsageMeter) for the window, resolves the roster
//! display-name map, and calls [`bucket_usage`].

use std::collections::HashMap;

use crate::ports::usage::{SampleKind, UsageSample};

use super::calendar::{epoch_day, iso_day};
use super::types::{AgentTokens, ProviderCalls, Usage, UsagePoint, UsageRange, UsageTotals};

/// Aggregates a company's usage samples into the [`Usage`] read surface.
///
/// - `samples`: the window's samples (the caller queries the meter with
///   `since = now - range.days()`; extra samples outside the day window still
///   feed totals but never widen the series).
/// - `range`: the number of daily buckets (7 / 30 / 90), all ending on the UTC
///   day of `now_millis`.
/// - `roster`: teammate id → display name (prosumer language). Ids missing from
///   the map fall back to the raw id.
pub fn bucket_usage(
    samples: &[UsageSample],
    range: UsageRange,
    now_millis: u64,
    roster: &HashMap<String, String>,
) -> Usage {
    let days = range.days();
    let today = epoch_day(now_millis);
    // The oldest bucket day inclusive: `days` buckets ending today.
    let first_day = today - (days as i64 - 1);

    // Series: zero-filled per-day input/output token sums.
    let mut per_day: HashMap<i64, (u64, u64)> = HashMap::new();
    // Per-agent token sums (keyed by raw agent id).
    let mut per_agent: HashMap<String, u64> = HashMap::new();
    // Per-provider OAuth-call counts.
    let mut per_provider: HashMap<String, u64> = HashMap::new();

    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_cost: f64 = 0.0;
    let mut oauth_calls: u64 = 0;

    for s in samples {
        total_input += s.input_tokens;
        total_output += s.output_tokens;
        total_cost += s.cost_usd;

        *per_agent.entry(s.agent.clone()).or_default() += s.input_tokens + s.output_tokens;

        if s.kind == SampleKind::OauthCall {
            oauth_calls += 1;
            *per_provider.entry(s.provider.clone()).or_default() += 1;
        }

        let day = epoch_day(s.at_millis);
        if day >= first_day && day <= today {
            let slot = per_day.entry(day).or_default();
            slot.0 += s.input_tokens;
            slot.1 += s.output_tokens;
        }
    }

    let series = (0..days)
        .map(|i| {
            let day = first_day + i as i64;
            let (input_tokens, output_tokens) = per_day.get(&day).copied().unwrap_or_default();
            UsagePoint {
                date: iso_day(day),
                input_tokens,
                output_tokens,
            }
        })
        .collect();

    let mut by_agent: Vec<AgentTokens> = per_agent
        .into_iter()
        .map(|(id, tokens)| AgentTokens {
            name: roster.get(&id).cloned().unwrap_or(id),
            tokens,
        })
        .collect();
    // Highest tokens first; name as a stable tie-breaker.
    by_agent.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.name.cmp(&b.name)));

    let connections = per_provider.len() as u64;
    let mut by_provider: Vec<ProviderCalls> = per_provider
        .into_iter()
        .map(|(provider, calls)| ProviderCalls { provider, calls })
        .collect();
    by_provider.sort_by(|a, b| {
        b.calls
            .cmp(&a.calls)
            .then_with(|| a.provider.cmp(&b.provider))
    });

    Usage {
        series,
        by_agent,
        by_provider,
        totals: UsageTotals {
            input_tokens: total_input,
            output_tokens: total_output,
            tokens: total_input + total_output,
            cost_usd: total_cost,
            oauth_calls,
            connections,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::calendar::{MILLIS_PER_DAY, days_from_civil};

    fn at(y: i64, m: u32, d: u32) -> u64 {
        (days_from_civil(y, m, d) as u64) * MILLIS_PER_DAY + 12 * 3_600_000
    }

    fn inference(at_millis: u64, agent: &str, input: u64, output: u64, cost: f64) -> UsageSample {
        UsageSample {
            at_millis,
            agent: agent.to_string(),
            provider: "managed".to_string(),
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
            cost_usd: cost,
            kind: SampleKind::Inference,
        }
    }

    fn oauth(at_millis: u64, provider: &str) -> UsageSample {
        UsageSample {
            at_millis,
            agent: "ceo".to_string(),
            provider: provider.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cost_usd: 0.0,
            kind: SampleKind::OauthCall,
        }
    }

    #[test]
    fn empty_samples_zero_fill_the_series() {
        let now = at(2026, 7, 16);
        let u = bucket_usage(&[], UsageRange::D7, now, &HashMap::new());
        assert_eq!(u.series.len(), 7);
        assert_eq!(u.series[0].date, "2026-07-10");
        assert_eq!(u.series[6].date, "2026-07-16");
        assert!(
            u.series
                .iter()
                .all(|p| p.input_tokens == 0 && p.output_tokens == 0)
        );
        assert_eq!(u.totals.tokens, 0);
        assert_eq!(u.totals.connections, 0);
        assert!(u.by_agent.is_empty());
        assert!(u.by_provider.is_empty());
    }

    #[test]
    fn series_lengths_track_the_range() {
        let now = at(2026, 7, 16);
        assert_eq!(
            bucket_usage(&[], UsageRange::D7, now, &HashMap::new())
                .series
                .len(),
            7
        );
        assert_eq!(
            bucket_usage(&[], UsageRange::D30, now, &HashMap::new())
                .series
                .len(),
            30
        );
        assert_eq!(
            bucket_usage(&[], UsageRange::D90, now, &HashMap::new())
                .series
                .len(),
            90
        );
    }

    #[test]
    fn tokens_land_in_the_right_day_bucket() {
        let now = at(2026, 7, 16);
        let samples = vec![
            inference(at(2026, 7, 16), "ceo", 100, 40, 0.5),
            inference(at(2026, 7, 15), "ceo", 10, 5, 0.1),
            inference(at(2026, 7, 15), "ceo", 20, 5, 0.1),
        ];
        let u = bucket_usage(&samples, UsageRange::D7, now, &HashMap::new());
        let today = u.series.last().unwrap();
        assert_eq!(today.date, "2026-07-16");
        assert_eq!((today.input_tokens, today.output_tokens), (100, 40));
        let yesterday = &u.series[5];
        assert_eq!(yesterday.date, "2026-07-15");
        assert_eq!((yesterday.input_tokens, yesterday.output_tokens), (30, 10));
    }

    #[test]
    fn samples_outside_the_window_still_feed_totals_not_series() {
        let now = at(2026, 7, 16);
        let samples = vec![
            inference(at(2026, 7, 16), "ceo", 100, 40, 0.5),
            // 60 days ago: outside the 7-day window.
            inference(at(2026, 5, 17), "ceo", 999, 999, 9.0),
        ];
        let u = bucket_usage(&samples, UsageRange::D7, now, &HashMap::new());
        // Series only covers the 7-day window.
        assert_eq!(u.series.iter().map(|p| p.input_tokens).sum::<u64>(), 100);
        // Totals include the out-of-window sample.
        assert_eq!(u.totals.input_tokens, 1099);
        assert_eq!(u.totals.output_tokens, 1039);
        assert!((u.totals.cost_usd - 9.5).abs() < 1e-9);
    }

    #[test]
    fn by_agent_resolves_display_names_and_sorts_desc() {
        let now = at(2026, 7, 16);
        let samples = vec![
            inference(at(2026, 7, 16), "strategy", 100, 50, 0.1),
            inference(at(2026, 7, 16), "creative", 300, 100, 0.2),
            inference(at(2026, 7, 16), "unknown", 10, 0, 0.0),
        ];
        let mut roster = HashMap::new();
        roster.insert("strategy".to_string(), "Strategy desk".to_string());
        roster.insert("creative".to_string(), "Creative studio".to_string());
        let u = bucket_usage(&samples, UsageRange::D7, now, &roster);
        assert_eq!(u.by_agent.len(), 3);
        assert_eq!(u.by_agent[0].name, "Creative studio");
        assert_eq!(u.by_agent[0].tokens, 400);
        assert_eq!(u.by_agent[1].name, "Strategy desk");
        assert_eq!(u.by_agent[1].tokens, 150);
        // Unknown id falls back to the raw id.
        assert_eq!(u.by_agent[2].name, "unknown");
    }

    #[test]
    fn by_provider_counts_only_oauth_calls() {
        let now = at(2026, 7, 16);
        let samples = vec![
            inference(at(2026, 7, 16), "ceo", 100, 50, 0.1),
            oauth(at(2026, 7, 16), "github"),
            oauth(at(2026, 7, 15), "github"),
            oauth(at(2026, 7, 15), "gmail"),
        ];
        let u = bucket_usage(&samples, UsageRange::D7, now, &HashMap::new());
        assert_eq!(u.by_provider.len(), 2);
        assert_eq!(u.by_provider[0].provider, "github");
        assert_eq!(u.by_provider[0].calls, 2);
        assert_eq!(u.by_provider[1].provider, "gmail");
        assert_eq!(u.by_provider[1].calls, 1);
        assert_eq!(u.totals.oauth_calls, 3);
        assert_eq!(u.totals.connections, 2);
        // OAuth calls carry no tokens.
        assert_eq!(u.totals.tokens, 150);
    }
}
