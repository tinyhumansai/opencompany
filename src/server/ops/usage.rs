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
//! — that is the true value, not a stub. OAuth-call samples
//! ([`SampleKind::OauthCall`](crate::ports::usage::SampleKind)) are not yet
//! emitted anywhere, so `byProvider` / `oauthCalls` read zero until the Phase 2
//! emit work lands.

use axum::Json;
use axum::Router;
use axum::extract::Query;
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::metering::{
    AgentTokens, ProviderCalls, Usage, UsagePoint, UsageRange, UsageTotals, bucket_usage,
    roster_display_names,
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
    by_agent: Vec<AgentTokens>,
    /// OAuth calls per provider, highest first (empty until Phase 2 emit).
    by_provider: Vec<ProviderCalls>,
    /// Window totals.
    totals: UsageTotals,
}

impl From<Usage> for UsageDto {
    fn from(usage: Usage) -> Self {
        Self {
            series: usage.series,
            by_agent: usage.by_agent,
            by_provider: usage.by_provider,
            totals: usage.totals,
        }
    }
}

/// Queries the meter and projects a company's usage for the window.
async fn project_usage(runtime: &CompanyRuntime, range: UsageRange) -> Result<UsageDto, ApiError> {
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
        .map(|record| roster_display_names(&record.manifest.agents, &record.overlay_agents))
        .unwrap_or_default();

    Ok(bucket_usage(&samples, range, now, &roster).into())
}

/// `GET …/usage?range=` — the company's usage read for the window.
async fn get_usage(
    company: ScopedCompany,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageDto>, ApiError> {
    let range = parse_range(query.range.as_deref());
    Ok(Json(project_usage(company.runtime.as_ref(), range).await?))
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

    fn home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oc-usage-{}", crate::ports::generate_id()))
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
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
        let home = home();
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

        std::fs::remove_dir_all(&home).ok();
    }

    /// A couple of recorded samples project the expected DTO: totals sum, the
    /// series carries the day's tokens, and `byAgent` resolves the roster role.
    #[tokio::test]
    async fn recorded_samples_project_totals_series_and_by_desk() {
        let home = home();
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

        std::fs::remove_dir_all(&home).ok();
    }

    /// An absent / unknown `?range=` defaults to the 30-day window.
    #[tokio::test]
    async fn range_defaults_to_thirty_days() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (_, dto) = get_usage(&state, "").await;
        assert_eq!(dto["series"].as_array().unwrap().len(), 30, "{dto}");
        let (_, dto_bad) = get_usage(&state, "?range=nonsense").await;
        assert_eq!(dto_bad["series"].as_array().unwrap().len(), 30, "{dto_bad}");

        std::fs::remove_dir_all(&home).ok();
    }
}
