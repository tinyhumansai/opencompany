//! REST usage read surface (Phase 1): `GET …/usage?range=…`.
//!
//! A REST twin of the `Company.usage(range)` GraphQL resolver
//! (`graphql/usage.rs`). The operator console is REST-only and ships no GraphQL
//! client, so the Usage view could only render sample data; this exposes the
//! same pure projection ([`bucket_usage`](crate::metering::bucket_usage)) over
//! the write-plane's dual-scope router. No new business logic — it queries the
//! [`UsageMeter`](crate::ports::UsageMeter) for the window, resolves the roster
//! display-name map, and buckets.
//!
//! Data maturity: token/cost samples only populate on a harness
//! (`openhuman`-feature) build via `src/harness/cost.rs`. The offline build has
//! no cost hook, so the meter is empty and the series is (correctly) zero-filled
//! — that is the true value, not a stub.
//!
//! The same holds for OAuth-call samples
//! ([`SampleKind::OauthCall`](crate::ports::usage::SampleKind)): they are now
//! emitted per completed connected-tool call (see
//! [`metering::oauth`](crate::metering::oauth)), but only a `composio`-feature
//! build wires a connected-tool surface at all — so on a default build
//! `byProvider` / `oauthCalls` still read zero, and that zero is now the honest
//! answer ("no connected tools in this build") rather than a missing hook.

use axum::Json;
use axum::Router;
use axum::extract::Query;
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::metering::{
    ProviderCalls, Usage, UsagePoint, UsageRange, bucket_usage, roster_display_names,
};
use crate::ports::now_millis;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Milliseconds in a UTC day — the meter is queried `range.days()` of these back.
const MILLIS_PER_DAY: u64 = 86_400_000;

/// Builds the usage read route fragment (both scope forms).
pub fn router() -> Router<AppState> {
    scoped("/usage", get(get_usage))
}

/// The `?range=` selector. Accepts the console's `7d` / `30d` / `90d` keys (and
/// the bare-number / `D7` forms); anything else falls back to the 30-day default,
/// matching the GraphQL resolver's default window.
#[derive(Debug, Deserialize)]
struct UsageQuery {
    range: Option<String>,
}

/// Parses a `?range=` value into a [`UsageRange`], defaulting to 30 days.
fn parse_range(raw: Option<&str>) -> UsageRange {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("7d" | "7" | "d7") => UsageRange::D7,
        Some("90d" | "90" | "d90") => UsageRange::D90,
        _ => UsageRange::D30,
    }
}

/// The Usage read as the console renders it (camelCase). Wraps the pure
/// [`Usage`] projection; the inner types already serialize camelCase, but
/// [`Usage`] itself does not rename its fields, so this DTO carries the
/// `byAgent` / `byProvider` casing the frontend keys off.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageDto {
    /// Zero-filled daily token series over the range, oldest first.
    series: Vec<UsagePoint>,
    /// Tokens per teammate (desk), highest first.
    by_agent: Vec<AgentUsageDto>,
    /// OAuth calls per provider, highest first. Empty on a build with no
    /// connected-tool surface compiled in — see the module docs.
    by_provider: Vec<ProviderCalls>,
    /// Window totals.
    totals: UsageTotalsDto,
    /// Money exists in this response but is withheld from this principal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cost_hidden: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentUsageDto {
    name: String,
    tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageTotalsDto {
    input_tokens: u64,
    output_tokens: u64,
    tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    oauth_calls: u64,
    connections: u64,
    search_calls: u64,
}

impl UsageDto {
    fn new(usage: Usage, may_read_cost: bool) -> Self {
        let cost_hidden = !may_read_cost && usage.totals.cost_usd > 0.0;
        Self {
            series: usage.series,
            by_agent: usage
                .by_agent
                .into_iter()
                .map(|agent| AgentUsageDto {
                    name: agent.name,
                    tokens: agent.tokens,
                    cost_usd: may_read_cost.then_some(agent.cost_usd),
                })
                .collect(),
            by_provider: usage.by_provider,
            totals: UsageTotalsDto {
                input_tokens: usage.totals.input_tokens,
                output_tokens: usage.totals.output_tokens,
                tokens: usage.totals.tokens,
                cost_usd: may_read_cost.then_some(usage.totals.cost_usd),
                oauth_calls: usage.totals.oauth_calls,
                connections: usage.totals.connections,
                search_calls: usage.totals.search_calls,
            },
            cost_hidden,
        }
    }
}

/// Queries the meter and projects a company's usage for the window.
async fn project_usage(
    runtime: &CompanyRuntime,
    range: UsageRange,
    may_read_cost: bool,
) -> Result<UsageDto, ApiError> {
    let now = now_millis();
    let since = now.saturating_sub(range.days().saturating_mul(MILLIS_PER_DAY));
    let samples = runtime
        .usage()
        .query(runtime.id(), since)
        .await
        .map_err(ApiError)?;

    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let roster = record
        .as_ref()
        .map(|record| roster_display_names(&record.effective_agents(), &record.overlay_agents))
        .unwrap_or_default();

    Ok(UsageDto::new(
        bucket_usage(&samples, range, now, &roster),
        may_read_cost,
    ))
}

/// `GET …/usage?range=` — the company's usage read for the window.
async fn get_usage(
    company: ScopedCompany,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageDto>, ApiError> {
    let range = parse_range(query.range.as_deref());
    Ok(Json(
        project_usage(company.runtime.as_ref(), range, company.may_read_contents).await?,
    ))
}

#[cfg(test)]
mod visibility_tests {
    use super::*;
    use crate::metering::{AgentTokens, UsageTotals};

    #[test]
    fn redacted_cost_is_explicit_and_never_serializes_as_zero() {
        let dto = UsageDto::new(
            Usage {
                series: Vec::new(),
                by_agent: vec![AgentTokens {
                    name: "Ops".to_string(),
                    tokens: 10,
                    cost_usd: 2.5,
                }],
                by_provider: Vec::new(),
                totals: UsageTotals {
                    input_tokens: 8,
                    output_tokens: 2,
                    tokens: 10,
                    cost_usd: 2.5,
                    oauth_calls: 0,
                    connections: 0,
                    search_calls: 0,
                },
            },
            false,
        );
        let value = serde_json::to_value(dto).expect("serialize usage");
        assert_eq!(value["costHidden"], true);
        assert!(value["totals"].get("costUsd").is_none(), "{value}");
        assert!(value["byAgent"][0].get("costUsd").is_none(), "{value}");
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::ports::usage::{SampleKind, UsageSample};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-usage-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn get_usage(state: &AppState, query: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/company/usage{query}"))
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// An empty meter zero-fills the series to the range length and totals zero.
    #[tokio::test]
    async fn empty_meter_projects_a_zero_filled_series() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, dto) = get_usage(&state, "?range=7d").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["series"].as_array().unwrap().len(), 7, "{dto}");
        assert_eq!(dto["totals"]["tokens"], 0.0);
        assert_eq!(dto["totals"]["connections"], 0);
        assert!(dto["byAgent"].as_array().unwrap().is_empty(), "{dto}");
        assert!(dto["byProvider"].as_array().unwrap().is_empty(), "{dto}");
    }

    /// A couple of recorded samples project the expected DTO: totals sum, the
    /// series carries the day's tokens, and `byAgent` resolves the roster role.
    #[tokio::test]
    async fn recorded_samples_project_totals_series_and_by_desk() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\n",
        )
        .await;

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let now = crate::ports::now_millis();
        for (input, output) in [(100u64, 40u64), (20, 5)] {
            runtime
                .usage()
                .record(
                    &id,
                    &UsageSample {
                        at_millis: now,
                        agent: "ceo".into(),
                        provider: "managed".into(),
                        input_tokens: input,
                        output_tokens: output,
                        cached_input_tokens: 0,
                        cost_usd: 0.25,
                        kind: SampleKind::Inference,
                        run_id: None,
                        model: None,
                    },
                )
                .await
                .unwrap();
        }

        let (status, dto) = get_usage(&state, "?range=30d").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["series"].as_array().unwrap().len(), 30, "{dto}");
        assert_eq!(dto["totals"]["inputTokens"], 120.0, "{dto}");
        assert_eq!(dto["totals"]["outputTokens"], 45.0, "{dto}");
        assert_eq!(dto["totals"]["tokens"], 165.0, "{dto}");
        assert!(
            (dto["totals"]["costUsd"].as_f64().unwrap() - 0.5).abs() < 1e-9,
            "{dto}"
        );

        // The most recent bucket (today) carries the whole burn.
        let series = dto["series"].as_array().unwrap();
        let today = series.last().unwrap();
        assert_eq!(today["inputTokens"], 120.0, "{today}");
        assert_eq!(today["outputTokens"], 45.0, "{today}");

        // byAgent resolves the manifest role as the display name.
        let by_agent = dto["byAgent"].as_array().unwrap();
        assert_eq!(by_agent.len(), 1, "{dto}");
        assert_eq!(by_agent[0]["name"], "Chief Executive");
        assert_eq!(by_agent[0]["tokens"], 165.0);
    }

    /// Metered web searches (issue #238) reach the console read: they count in
    /// their own `searchCalls` field and their cost rolls into the window
    /// total, while leaving the connected-accounts numbers alone.
    ///
    /// That last clause is the point. A search is a priced call on the managed
    /// platform, not a third-party account the company connected, so folding it
    /// into `oauthCalls` / `byProvider` (whose row count **is** `connections`)
    /// would report an integration the operator never set up.
    #[tokio::test]
    async fn search_calls_surface_separately_from_connected_accounts() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"Chief Executive\"\n",
        )
        .await;

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let now = crate::ports::now_millis();
        for _ in 0..3 {
            runtime
                .usage()
                .record(
                    &id,
                    &crate::metering::search_call_sample("ceo", "Exa", 0.01, now),
                )
                .await
                .unwrap();
        }

        let (status, dto) = get_usage(&state, "?range=7d").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["totals"]["searchCalls"], 3, "{dto}");
        assert!(
            (dto["totals"]["costUsd"].as_f64().unwrap() - 0.03).abs() < 1e-9,
            "search cost must roll into the window total: {dto}"
        );
        assert_eq!(
            dto["totals"]["connections"], 0,
            "a search is not a connected account: {dto}"
        );
        assert_eq!(dto["totals"]["oauthCalls"], 0, "{dto}");
        assert!(dto["byProvider"].as_array().unwrap().is_empty(), "{dto}");
        // And no tokens, so the teammate chart stays a token chart.
        assert_eq!(dto["totals"]["tokens"], 0.0, "{dto}");
        assert!(dto["byAgent"].as_array().unwrap().is_empty(), "{dto}");
    }

    /// An absent / unknown `?range=` defaults to the 30-day window.
    #[tokio::test]
    async fn range_defaults_to_thirty_days() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (_, dto) = get_usage(&state, "").await;
        assert_eq!(dto["series"].as_array().unwrap().len(), 30, "{dto}");
        let (_, dto_bad) = get_usage(&state, "?range=nonsense").await;
        assert_eq!(dto_bad["series"].as_array().unwrap().len(), 30, "{dto_bad}");
    }
}
