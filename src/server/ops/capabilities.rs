//! Capability-budget read surface (issue #108): the company's effective tier
//! plan and how much of each tier's token budget the current period has spent.
//!
//! A read-only companion to the harness gate — the console renders one row per
//! configured tier (budget, spend, remaining, whether its tools are disabled).
//! With no `[plan]` configured the response is `{ configured: false }` and the
//! console shows a "no token plan configured" note. The heavy lifting is the
//! pure math in [`crate::metering::capability`]; this handler only queries the
//! [`UsageMeter`](crate::ports::UsageMeter) for the period and projects it.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::metering::capability::{CapabilityPlan, tokens_in};
use crate::ports::now_millis;
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the capability-budget route fragment.
pub fn router() -> Router<AppState> {
    scoped("/capabilities", get(get_status))
}

/// The company's capability-budget status as the console renders it.
///
/// When no `[plan]` is configured only `configured: false` is sent; the extra
/// fields are omitted so the console can branch on presence.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CapabilityStatusDto {
    /// Whether the company has a capability plan at all.
    configured: bool,
    /// The configured built-in tier name, if any (`null` for a bare
    /// `token_budgets` plan).
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    /// The budget window (`daily` / `monthly`).
    #[serde(skip_serializing_if = "Option::is_none")]
    period: Option<String>,
    /// Epoch-millis start of the current budget period (the spend window).
    #[serde(skip_serializing_if = "Option::is_none")]
    period_start_millis: Option<u64>,
    /// Total inference tokens spent by the company this period (the figure every
    /// tier threshold is compared against).
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_tokens: Option<u64>,
    /// One row per configured tier, namespace-sorted. Omitted when unconfigured.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tiers: Vec<TierDto>,
    /// Media generation (issue #109): whether this company **explicitly** grants
    /// the real-money `media` namespace (a `*` wildcard does NOT count). Sent
    /// regardless of whether a `[plan]` is configured, since media is opt-in per
    /// tool grant, not per plan.
    media_granted: bool,
    /// Whether the `media` feature is compiled into this build at all (the tools
    /// only exist under it). `false` lets the console show a "not in this build"
    /// state rather than implying a missing credential.
    media_in_build: bool,
    /// Whether a MANAGED media credential is resolvable from the environment on
    /// this build (feature on + env present). Never reflects a tenant secret.
    media_credential_configured: bool,
    /// Per-tenant Composio (issue #110): whether this company **explicitly**
    /// grants the `composio` namespace (a `*` wildcard does NOT count). Opt-in
    /// per tool grant, independent of a `[plan]`.
    composio_granted: bool,
    /// Whether the `composio` feature is compiled into this build at all.
    composio_in_build: bool,
    /// Whether a non-empty per-tenant Composio token is stored — never the token
    /// itself. Unlike media's env credential, this is a tenant secret.
    composio_token_configured: bool,
}

/// One tier's budget row.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TierDto {
    /// The exec tool namespace this tier gates.
    namespace: String,
    /// Tokens allowed this period.
    budget_tokens: u64,
    /// Tokens spent this period (company-wide — spend has no per-tier
    /// attribution, so this is the same across rows).
    spent_tokens: u64,
    /// `budget - spent`, saturating at zero.
    remaining_tokens: u64,
    /// Whether spend has reached the threshold — the tier's tools are disabled.
    exhausted: bool,
}

/// The opt-in-capability status flags carried on every response (media +
/// composio), independent of whether a `[plan]` is configured.
struct OptInFlags {
    media_granted: bool,
    composio_granted: bool,
    composio_token_configured: bool,
}

impl OptInFlags {
    /// All-false — used when no company record is present.
    fn none() -> Self {
        Self {
            media_granted: false,
            composio_granted: false,
            composio_token_configured: false,
        }
    }
}

/// The unconfigured response: `{ configured: false }` plus the opt-in-capability
/// flags (media + composio are opt-in per tool grant, independent of a `[plan]`).
fn unconfigured(flags: OptInFlags) -> CapabilityStatusDto {
    CapabilityStatusDto {
        configured: false,
        plan: None,
        period: None,
        period_start_millis: None,
        spent_tokens: None,
        tiers: Vec::new(),
        media_granted: flags.media_granted,
        media_in_build: cfg!(feature = "media"),
        media_credential_configured: media_credential_configured(),
        composio_granted: flags.composio_granted,
        composio_in_build: cfg!(feature = "composio"),
        composio_token_configured: flags.composio_token_configured,
    }
}

/// Whether a MANAGED media credential (issue #109) is resolvable from the
/// environment on this build. `true` only under the `media` feature with a
/// credential present — env-only, never a tenant secret, matching the harness's
/// fail-closed resolution. Off the feature this is always `false`.
fn media_credential_configured() -> bool {
    #[cfg(feature = "media")]
    {
        use crate::app::config::ProcessEnv;
        crate::harness::provider::media_backend_from_env(&ProcessEnv).is_some()
    }
    #[cfg(not(feature = "media"))]
    {
        false
    }
}

/// Resolves the capability-budget status DTO for a company.
async fn effective_status(runtime: &CompanyRuntime) -> Result<CapabilityStatusDto, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let Some(record) = record else {
        return Ok(unconfigured(OptInFlags::none()));
    };
    // Media + composio are opt-in per tool grant (explicit namespace, never `*`)
    // and live on the manifest regardless of whether a `[plan]` is configured.
    let flags = OptInFlags {
        media_granted: crate::company::grants_media_explicit(&record.manifest.tools.allow),
        composio_granted: crate::company::grants_composio_explicit(&record.manifest.tools.allow),
        // Degrade to "unconfigured" on a transient secret-store error rather
        // than failing the whole /capabilities response (budget/tier data is
        // unrelated to Composio). Mirrors other opt-in-credential probes.
        composio_token_configured: crate::company::composio::token_configured(
            runtime.id(),
            runtime.secrets().as_ref(),
        )
        .await
        .unwrap_or(false),
    };
    let manifest_plan = &record.manifest.plan;
    let Some(plan) = CapabilityPlan::from_manifest(manifest_plan) else {
        return Ok(unconfigured(flags));
    };

    let now = now_millis();
    let since = plan.period.period_start_millis(now);
    let samples = runtime
        .usage()
        .query(runtime.id(), since)
        .await
        .map_err(ApiError)?;
    let spent = tokens_in(&samples);

    let tiers = plan
        .status(spent)
        .into_iter()
        .map(|tier| TierDto {
            namespace: tier.namespace,
            budget_tokens: tier.budget,
            spent_tokens: tier.spent,
            remaining_tokens: tier.remaining,
            exhausted: tier.exhausted,
        })
        .collect();

    Ok(CapabilityStatusDto {
        configured: true,
        plan: manifest_plan.name.clone(),
        period: Some(plan.period.as_str().to_string()),
        period_start_millis: Some(since),
        spent_tokens: Some(spent),
        tiers,
        media_granted: flags.media_granted,
        media_in_build: cfg!(feature = "media"),
        media_credential_configured: media_credential_configured(),
        composio_granted: flags.composio_granted,
        composio_in_build: cfg!(feature = "composio"),
        composio_token_configured: flags.composio_token_configured,
    })
}

/// `GET …/capabilities` — the company's capability-budget status.
async fn get_status(company: ScopedCompany) -> Result<Json<CapabilityStatusDto>, ApiError> {
    Ok(Json(effective_status(company.runtime.as_ref()).await?))
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
        std::env::temp_dir().join(format!("oc-capabilities-{}", crate::ports::generate_id()))
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

    async fn get_capabilities(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/capabilities")
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

    #[tokio::test]
    async fn reports_unconfigured_without_a_plan() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n",
        )
        .await;

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false);
        assert!(
            dto.get("tiers").is_none(),
            "no tiers when unconfigured: {dto}"
        );
        assert!(dto.get("plan").is_none());

        std::fs::remove_dir_all(&home).ok();
    }

    /// Media generation (issue #109): the route surfaces `mediaGranted` from the
    /// manifest tool grants (explicit `media`, never `*`), even with no `[plan]`.
    #[tokio::test]
    async fn reports_media_granted_from_explicit_grant_only() {
        // Explicit `media` grant, no `[plan]` → unconfigured but mediaGranted.
        let home_a = home();
        let state = state_with_manifest(
            &home_a,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"media\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], false);
        assert_eq!(dto["mediaGranted"], true, "{dto}");
        // The flags are always present so the console can render every state.
        assert!(dto.get("mediaInBuild").is_some(), "{dto}");
        assert!(dto.get("mediaCredentialConfigured").is_some(), "{dto}");
        std::fs::remove_dir_all(&home_a).ok();

        // A `*` wildcard grant must NOT count as a media grant.
        let home_b = home();
        let state2 = state_with_manifest(
            &home_b,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(
            dto2["mediaGranted"], false,
            "the `*` wildcard must not grant the real-money media family: {dto2}"
        );
        std::fs::remove_dir_all(&home_b).ok();
    }

    /// Per-tenant Composio (issue #110): the route surfaces `composioGranted`
    /// from the explicit grant (never `*`) and the trio flags, even with no
    /// `[plan]`.
    #[tokio::test]
    async fn reports_composio_flags_from_explicit_grant_only() {
        let home_a = home();
        let state = state_with_manifest(
            &home_a,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n",
        )
        .await;
        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["composioGranted"], true, "{dto}");
        assert_eq!(dto["composioTokenConfigured"], false, "no token yet: {dto}");
        assert!(dto.get("composioInBuild").is_some(), "{dto}");
        std::fs::remove_dir_all(&home_a).ok();

        // A `*` wildcard grant must NOT count as a composio grant.
        let home_b = home();
        let state2 = state_with_manifest(
            &home_b,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto2) = get_capabilities(&state2).await;
        assert_eq!(
            dto2["composioGranted"], false,
            "the `*` wildcard must not grant composio: {dto2}"
        );
        std::fs::remove_dir_all(&home_b).ok();
    }

    #[tokio::test]
    async fn reports_tiers_and_exhaustion_for_a_configured_plan() {
        let home = home();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[plan]\nname = \"starter\"\n",
        )
        .await;

        // Seed 250k inference tokens into the company meter — past starter's 200k.
        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: "ceo".into(),
                    provider: "managed".into(),
                    input_tokens: 200_000,
                    output_tokens: 50_000,
                    cached_input_tokens: 0,
                    cost_usd: 0.0,
                    kind: SampleKind::Inference,
                },
            )
            .await
            .unwrap();

        let (status, dto) = get_capabilities(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["configured"], true);
        assert_eq!(dto["plan"], "starter");
        assert_eq!(dto["period"], "daily");
        assert_eq!(dto["spentTokens"], 250_000);

        let tiers = dto["tiers"].as_array().expect("tiers present");
        assert_eq!(tiers.len(), 2, "starter budgets shell + code: {dto}");
        for tier in tiers {
            assert_eq!(tier["budgetTokens"], 200_000);
            assert_eq!(tier["spentTokens"], 250_000);
            assert_eq!(tier["remainingTokens"], 0);
            assert_eq!(tier["exhausted"], true);
        }

        std::fs::remove_dir_all(&home).ok();
    }
}
