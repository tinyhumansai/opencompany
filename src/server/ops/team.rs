//! Team writes: add an overlay teammate, remove one, and toggle a teammate's
//! inbox — under both scope forms.
//!
//! Adds use the **operator-overlay** model: a new teammate is persisted as an
//! [`OverlayAgent`](crate::ports::types::OverlayAgent) on the `CompanyRecord`
//! through [`CompanyStore`](crate::ports::CompanyStore) and merged into the
//! roster at read time; the version-controlled `company.toml` is never
//! rewritten. Overlay teammates are roster-only in v1. A teammate defined in the
//! manifest cannot be removed here (409).

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::dns::DomainStatus;
use crate::error::OpenCompanyError;
use crate::ports::generate_id;
use crate::ports::inbox::InboxMeta;
use crate::ports::store::company_write_lock;
use crate::ports::types::OverlayAgent;
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{DOMAIN_KEY, ScopedCompany, scoped};

/// Builds the team route fragment.
pub fn router() -> Router<AppState> {
    scoped("/team", get(list_team).post(add_member))
        .merge(scoped("/team/{agent_id}", delete(remove_member)))
        .merge(scoped("/team/{agent_id}/inbox", put(toggle_inbox)))
}

/// One teammate as the console renders it (mirrors `TeamMemberDto`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TeamMemberDto {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Whether this teammate has an enabled inbox, so the Team page's toggle
    /// renders the host's real state instead of a client-side guess.
    inbox_enabled: bool,
    /// This teammate's manifest `budget_usd_daily` cap (issue #304), or absent
    /// when it has none.
    ///
    /// Absent-vs-present **is** the capped/uncapped distinction, which is why
    /// this is skipped rather than zeroed: `0` would read as "capped at nothing"
    /// and render a permanently exhausted teammate. Overlay teammates carry no
    /// cap in v1, so they always omit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_usd_daily: Option<f64>,
    /// What this teammate has spent since 00:00 UTC, present only alongside a
    /// cap — an uncapped teammate's spend belongs on the Usage page, not here.
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_today_usd: Option<f64>,
}

/// The add-teammate body.
#[derive(Debug, Deserialize)]
struct AddMember {
    name: String,
    role: String,
    #[serde(default)]
    description: Option<String>,
}

/// The inbox-toggle body.
#[derive(Debug, Deserialize)]
struct ToggleInbox {
    enabled: bool,
}

/// The inbox-toggle response.
#[derive(Debug, Serialize)]
struct InboxAck {
    key: String,
    address: String,
}

/// The sub-resource path (`agent_id`).
#[derive(Debug, Deserialize)]
struct AgentPath {
    agent_id: String,
}

/// `GET {scope}/team` — the merged roster: manifest teammates (versioned in
/// `company.toml`, `name: null` — the console falls back to the role) plus
/// operator-added overlay teammates (`name` always set). Mirrors the GraphQL
/// `resolve_team` merge, `inbox_enabled` included — the console's Team page is
/// its REST consumer, so the inbox toggle reflects the [`InboxStore`] rather
/// than a client-side guess (issue #173). Hosts with no persisted record yet
/// return an empty roster, the same soft-fail the sibling `/desks` route uses,
/// rather than 404ing.
///
/// [`InboxStore`]: crate::ports::InboxStore
async fn list_team(company: ScopedCompany) -> Result<Json<Vec<TeamMemberDto>>, ApiError> {
    let record = company.runtime.store().load(company.id()).await?;
    // Inbox metadata is keyed by agent id, so the roster can be tagged without
    // a per-teammate read. An inbox that was never toggled is simply absent.
    let enabled_inboxes: std::collections::HashMap<String, bool> = company
        .runtime
        .inbox()
        .inboxes(company.id())
        .await?
        .into_iter()
        .map(|meta| (meta.key, meta.enabled))
        .collect();
    let enabled = |id: &str| enabled_inboxes.get(id).copied().unwrap_or(false);
    // Issue #304: today's spend, for capped teammates only. One meter read for
    // the whole roster, and only when the manifest actually caps somebody —
    // a company with no caps pays nothing for a column it will not render.
    let spend_today = daily_spend_samples(&company, record.as_ref()).await?;
    let spent = |id: &str| {
        spend_today
            .as_ref()
            .map(|samples| crate::metering::usd_spent_by_agent(samples, id))
    };
    let members = record
        .map(|record| {
            let mut members: Vec<TeamMemberDto> = record
                .manifest
                .agents
                .iter()
                .map(|agent| TeamMemberDto {
                    id: agent.id.clone(),
                    name: None,
                    role: agent.role.clone(),
                    description: agent.description.clone(),
                    inbox_enabled: enabled(&agent.id),
                    budget_usd_daily: agent.budget_usd_daily,
                    // Paired with the cap: no cap, no spend row.
                    spent_today_usd: agent.budget_usd_daily.and_then(|_| spent(&agent.id)),
                })
                .collect();
            members.extend(record.overlay_agents.iter().map(|agent| TeamMemberDto {
                id: agent.id.clone(),
                name: Some(agent.name.clone()),
                role: agent.role.clone(),
                description: agent.description.clone(),
                inbox_enabled: enabled(&agent.id),
                // Overlay teammates are uncapped in v1.
                budget_usd_daily: None,
                spent_today_usd: None,
            }));
            members
        })
        .unwrap_or_default();
    Ok(Json(members))
}

/// Today's usage samples (since 00:00 UTC), or `None` when no teammate on this
/// company carries a `budget_usd_daily` cap (issue #304).
///
/// Returning `None` rather than an empty vec keeps "nobody is capped" distinct
/// from "everybody is capped and has spent nothing", and is what lets the
/// caller skip the meter round-trip entirely for the common uncapped company.
async fn daily_spend_samples(
    company: &ScopedCompany,
    record: Option<&crate::ports::types::CompanyRecord>,
) -> Result<Option<Vec<crate::ports::usage::UsageSample>>, ApiError> {
    let any_capped = record.is_some_and(|record| {
        record
            .manifest
            .agents
            .iter()
            .any(|agent| agent.budget_usd_daily.is_some())
    });
    if !any_capped {
        return Ok(None);
    }
    let since = crate::metering::utc_day_start_millis(crate::ports::now_millis());
    let samples = company
        .runtime
        .usage()
        .query(company.id(), since)
        .await
        .map_err(ApiError)?;
    Ok(Some(samples))
}

async fn add_member(
    company: ScopedCompany,
    Json(body): Json<AddMember>,
) -> Result<Json<TeamMemberDto>, ApiError> {
    // Serialize per-company writes so concurrent console POST /team and
    // orchestrator add_agent calls can't clobber each other's overlay_agents.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    let agent = OverlayAgent {
        id: generate_id(),
        name: body.name,
        role: body.role,
        description: body.description,
    };
    record.overlay_agents.push(agent.clone());
    company.runtime.store().save(&record).await?;
    Ok(Json(TeamMemberDto {
        id: agent.id,
        name: Some(agent.name),
        role: agent.role,
        description: agent.description,
        // A brand-new teammate has no inbox until the toggle writes one.
        inbox_enabled: false,
        // Overlay teammates carry no per-agent cap in v1 (issue #304).
        budget_usd_daily: None,
        spent_today_usd: None,
    }))
}

async fn remove_member(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Result<StatusCode, ApiError> {
    // Serialize so a concurrent add_agent / add_member doesn't clobber.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    // A manifest teammate is part of the version-controlled blueprint.
    if record.manifest.agents.iter().any(|a| a.id == agent_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::MANIFEST_TEAMMATE_DELETE.to_string(),
        )));
    }
    let before = record.overlay_agents.len();
    record.overlay_agents.retain(|a| a.id != agent_id);
    if record.overlay_agents.len() == before {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        ))));
    }
    company.runtime.store().save(&record).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn toggle_inbox(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<ToggleInbox>,
) -> Result<Json<InboxAck>, ApiError> {
    // Resolve a display name and address for the inbox metadata.
    let record = company.runtime.store().load(company.id()).await?;
    let name = record
        .as_ref()
        .and_then(|r| {
            r.manifest
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .map(|a| a.role.clone())
                .or_else(|| {
                    r.overlay_agents
                        .iter()
                        .find(|a| a.id == agent_id)
                        .map(|a| a.name.clone())
                })
        })
        .unwrap_or_else(|| agent_id.clone());
    let address = match load_domain(&company).await? {
        Some(domain) => format!("{agent_id}@{domain}"),
        None => agent_id.clone(),
    };
    let meta = InboxMeta {
        key: agent_id.clone(),
        name,
        address: address.clone(),
        enabled: body.enabled,
    };
    company
        .runtime
        .inbox()
        .set_enabled(company.id(), &agent_id, &meta)
        .await?;
    Ok(Json(InboxAck {
        key: agent_id,
        address,
    }))
}

/// Loads the configured custom domain, if any.
async fn load_domain(company: &ScopedCompany) -> Result<Option<String>, ApiError> {
    let Some(value) = company
        .runtime
        .secrets()
        .get(company.id(), DOMAIN_KEY)
        .await?
    else {
        return Ok(None);
    };
    let status: DomainStatus = serde_json::from_str(value.expose())?;
    Ok(Some(status.domain))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::ports::usage::{SampleKind, UsageSample};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-team-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
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
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                template_provenance: None,
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

    async fn get_team(state: &AppState) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri("/api/v1/company/team")
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

    /// Two teammates on one manifest: `analyst` is capped, `writer` is not.
    const ROSTER: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\nbudget_usd_daily = 5.0\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n";

    /// Issue #304 — the cap was never on the wire at all, so the issue's "and
    /// displayed in the console" was stale against main. A capped teammate now
    /// carries both its cap and its spend since UTC midnight, summed from the
    /// meter; an uncapped one carries neither key.
    ///
    /// The omission is the contract, not an optimisation: the console tells
    /// "spends freely" from "capped and has spent nothing" by presence alone.
    #[tokio::test]
    async fn a_capped_teammate_carries_its_cap_and_todays_spend() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let now = crate::ports::now_millis();
        for cost in [1.25f64, 0.50] {
            runtime
                .usage()
                .record(
                    &id,
                    &UsageSample {
                        at_millis: now,
                        agent: "analyst".into(),
                        provider: "managed".into(),
                        input_tokens: 10,
                        output_tokens: 5,
                        cached_input_tokens: 0,
                        cost_usd: cost,
                        kind: SampleKind::Inference,
                        run_id: None,
                    },
                )
                .await
                .unwrap();
        }
        // The uncapped teammate's spend must not leak onto the capped one.
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: now,
                    agent: "writer".into(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: 9.00,
                    kind: SampleKind::Inference,
                    run_id: None,
                },
            )
            .await
            .unwrap();

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body.as_array().unwrap();

        let analyst = rows.iter().find(|m| m["id"] == "analyst").unwrap();
        assert_eq!(analyst["budgetUsdDaily"], 5.0, "{analyst}");
        assert!(
            (analyst["spentTodayUsd"].as_f64().unwrap() - 1.75).abs() < 1e-9,
            "spend is summed per agent since UTC midnight: {analyst}"
        );

        let writer = rows.iter().find(|m| m["id"] == "writer").unwrap();
        assert!(
            writer.get("budgetUsdDaily").is_none() && writer.get("spentTodayUsd").is_none(),
            "an uncapped teammate omits both keys: {writer}"
        );
    }

    /// Yesterday's spend is not today's: the read is anchored at 00:00 UTC, the
    /// same boundary the harness gate and the policy arm enforce against.
    #[tokio::test]
    async fn spend_today_excludes_yesterday() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        let yesterday =
            crate::metering::utc_day_start_millis(crate::ports::now_millis()).saturating_sub(1);
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: yesterday,
                    agent: "analyst".into(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: 9.00,
                    kind: SampleKind::Inference,
                    run_id: None,
                },
            )
            .await
            .unwrap();

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK);
        let analyst = body
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "analyst")
            .unwrap()
            .clone();
        assert_eq!(analyst["budgetUsdDaily"], 5.0, "{analyst}");
        assert_eq!(
            analyst["spentTodayUsd"], 0.0,
            "a capped teammate with no spend today reads $0, not yesterday's $9: {analyst}"
        );
    }

    /// A company that caps nobody renders exactly as it did before #304 — and
    /// the meter is never consulted for it.
    #[tokio::test]
    async fn an_uncapped_company_is_unchanged() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n",
        )
        .await;

        let (status, body) = get_team(&state).await;
        assert_eq!(status, StatusCode::OK);
        let writer = &body.as_array().unwrap()[0];
        assert_eq!(writer["id"], "writer");
        assert!(
            writer.get("budgetUsdDaily").is_none() && writer.get("spentTodayUsd").is_none(),
            "{writer}"
        );
    }
}
