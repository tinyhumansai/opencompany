//! Team writes: add an overlay teammate, remove one, set a teammate's daily
//! spend cap, and toggle its inbox — under both scope forms.
//!
//! Adds use the **operator-overlay** model: a new teammate is persisted as an
//! [`OverlayAgent`](crate::ports::types::OverlayAgent) on the `CompanyRecord`
//! through [`CompanyStore`](crate::ports::CompanyStore) and merged into the
//! roster at read time; the version-controlled `company.toml` is never
//! rewritten. A teammate defined in the manifest cannot be removed here (409).
//!
//! ## Daily budgets (issue #343)
//!
//! `budget_usd_daily` is enforced (issue #304) but was readable only from the
//! manifest, which on a hosted tenant is baked into the container image — so an
//! operator whose teammate hit its cap had no remedy short of a redeploy.
//! `PUT`/`DELETE …/team/{agent_id}/budget` write a
//! [`BudgetOverride`](crate::ports::types::BudgetOverride) onto the record, and
//! [`CompanyRecord::effective_budget`](crate::ports::types::CompanyRecord::effective_budget)
//! resolves it ahead of the manifest everywhere the cap is read. The harness
//! fingerprints the override set, so the new value is enforced on the company's
//! next dispatch with no restart.
//!
//! Three rules the surface exists to keep:
//!
//! - **Admin-only, and attributed.** Raising your own spend limit is a privilege
//!   boundary, so both writes go through
//!   [`require_admin`](crate::server::users::admin::require_admin) and stamp who
//!   did it and when.
//! - **Clearing is not zeroing.** `{"budgetUsdDaily": null}` removes the cap;
//!   `{"budgetUsdDaily": 0}` caps at nothing. They are different stored states
//!   and different behaviours. An **omitted** key is neither, so it is a 422
//!   rather than a silent uncap — see [`SetBudget`].
//! - **Reset is its own verb.** `DELETE` drops the override so the manifest
//!   default applies again, which no `PUT` body can express.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::dns::DomainStatus;
use crate::error::OpenCompanyError;
use crate::ports::inbox::InboxMeta;
use crate::ports::store::company_write_lock;
use crate::ports::types::{Actor, ActorKind, BudgetOverride, CompanyRecord, OverlayAgent};
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{DOMAIN_KEY, ScopedCompany, scoped};
use crate::server::users::admin::require_admin;

/// Builds the team route fragment.
pub fn router() -> Router<AppState> {
    scoped("/team", get(list_team).post(add_member))
        .merge(scoped("/team/{agent_id}", delete(remove_member)))
        .merge(scoped("/team/{agent_id}/inbox", put(toggle_inbox)))
        .merge(scoped(
            "/team/{agent_id}/budget",
            put(set_budget).delete(clear_budget),
        ))
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
    /// This teammate's daily spend cap in force (issue #304), or absent when it
    /// has none.
    ///
    /// Absent-vs-present **is** the capped/uncapped distinction, which is why
    /// this is skipped rather than zeroed: `0` would read as "capped at nothing"
    /// and render a permanently exhausted teammate.
    ///
    /// Since #343 this is the **effective** cap — an operator override when one
    /// is stored, the manifest value otherwise — so the card shows what the
    /// dispatch gate will actually enforce. An overlay teammate can carry one
    /// too; it is no longer unconditionally uncapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_usd_daily: Option<f64>,
    /// What this teammate has spent since 00:00 UTC, present only alongside a
    /// cap — an uncapped teammate's spend belongs on the Usage page, not here.
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_today_usd: Option<f64>,
    /// The user id of the admin who last set this teammate's cap from the
    /// console (issue #343), absent when no override is stored.
    ///
    /// Present **whenever an override exists**, including one that removed the
    /// cap — which is why it is not paired with `budgetUsdDaily`. "Nobody has
    /// touched this" and "an admin deliberately uncapped this" look identical
    /// on the cap alone, and the second is exactly what an operator asking
    /// "why is this teammate spending freely?" needs to see.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_by: Option<String>,
    /// When that cap was set (epoch millis). Paired with `budgetSetBy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_at_millis: Option<u64>,
}

/// The add-teammate body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddMember {
    name: String,
    role: String,
    #[serde(default)]
    description: Option<String>,
    /// An optional daily spend cap for the new teammate (issue #343): "a
    /// teammate created through the Console can be given a cap at creation".
    ///
    /// Only `Some` requires an admin — a budget-less add keeps working for any
    /// member exactly as before, so adding the field takes no permission away.
    #[serde(default)]
    budget_usd_daily: Option<f64>,
}

/// The set-budget body.
///
/// `budget_usd_daily` is a **double option** so the three cases stay apart on
/// the wire, which is the whole point of the route:
///
/// | body | parses as | means |
/// |---|---|---|
/// | `{"budgetUsdDaily": 5}` | `Some(Some(5.0))` | cap at $5/day |
/// | `{"budgetUsdDaily": 0}` | `Some(Some(0.0))` | cap at nothing |
/// | `{"budgetUsdDaily": null}` | `Some(None)` | remove the cap |
/// | `{}` | *rejected* | — |
///
/// The last row is deliberate. There is **no `#[serde(default)]`**, so an
/// omitted key is a deserialization failure and axum answers `422` — an empty
/// body can never be read as "uncap this teammate". A client that means to
/// remove a cap has to say `null` and mean it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetBudget {
    #[serde(deserialize_with = "double_option")]
    budget_usd_daily: Option<Option<f64>>,
}

/// Deserializes into `Some(inner)` when the field is present (so an explicit
/// `null` becomes `Some(None)`). Without a companion `#[serde(default)]` an
/// omitted field stays an error — which is what [`SetBudget`] wants.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer).map(Some)
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
                .map(|agent| {
                    member_row(
                        &record,
                        &agent.id,
                        None,
                        agent.role.clone(),
                        agent.description.clone(),
                        enabled(&agent.id),
                        &spent,
                    )
                })
                .collect();
            members.extend(record.overlay_agents.iter().map(|agent| {
                member_row(
                    &record,
                    &agent.id,
                    Some(agent.name.clone()),
                    agent.role.clone(),
                    agent.description.clone(),
                    enabled(&agent.id),
                    &spent,
                )
            }));
            members
        })
        .unwrap_or_default();
    Ok(Json(members))
}

/// Builds one roster row, resolving the cap and its attribution through the
/// record so the manifest arm and the overlay arm cannot drift (issue #343).
///
/// `spent` is the shared per-agent spend lookup — `None` for a company where
/// nobody is capped, in which case the meter was never read.
fn member_row(
    record: &CompanyRecord,
    agent_id: &str,
    name: Option<String>,
    role: String,
    description: Option<String>,
    inbox_enabled: bool,
    spent: &dyn Fn(&str) -> Option<f64>,
) -> TeamMemberDto {
    let cap = record.effective_budget(agent_id);
    let attribution = record.budget_override(agent_id);
    TeamMemberDto {
        id: agent_id.to_string(),
        name,
        role,
        description,
        inbox_enabled,
        budget_usd_daily: cap,
        // Paired with the cap: no cap, no spend row.
        spent_today_usd: cap.and_then(|_| spent(agent_id)),
        budget_set_by: attribution.map(|entry| entry.set_by.id.clone()),
        budget_set_at_millis: attribution.map(|entry| entry.at_millis),
    }
}

/// Today's usage samples (since 00:00 UTC), or `None` when no teammate on this
/// company carries a daily cap (issue #304).
///
/// Returning `None` rather than an empty vec keeps "nobody is capped" distinct
/// from "everybody is capped and has spent nothing", and is what lets the
/// caller skip the meter round-trip entirely for the common uncapped company.
///
/// The scan runs over **effective** caps across the **whole** roster (issue
/// #343). Both halves matter: an override that caps a previously-uncapped
/// teammate has to start the meter read, or its card would render a cap with no
/// spend beside it; and overlay teammates are now cappable, so restricting the
/// scan to manifest agents would miss the only capped teammate on a company
/// whose roster was built entirely from the console.
async fn daily_spend_samples(
    company: &ScopedCompany,
    record: Option<&CompanyRecord>,
) -> Result<Option<Vec<crate::ports::usage::UsageSample>>, ApiError> {
    let any_capped = record
        .is_some_and(|record| roster_ids(record).any(|id| record.effective_budget(id).is_some()));
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

/// Every roster teammate's id — manifest agents first, then overlay teammates.
/// The same union `CompanyRecord::is_roster_agent` accepts.
fn roster_ids(record: &CompanyRecord) -> impl Iterator<Item = &String> {
    record
        .manifest
        .agents
        .iter()
        .map(|agent| &agent.id)
        .chain(record.overlay_agents.iter().map(|agent| &agent.id))
}

/// `POST {scope}/team` — add an operator-defined teammate, optionally with a
/// daily spend cap (issue #343).
///
/// The cap and the teammate land in **one** record save, so a company can never
/// end up with a teammate whose intended cap silently failed to apply.
async fn add_member(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AddMember>,
) -> Result<Json<TeamMemberDto>, Response> {
    // Setting a cap is admin-only, so an add that carries one is too — but an
    // add that does not keeps working for any member, exactly as before. The
    // check is deliberately conditional: adding this field must not quietly
    // take the existing capability away from members.
    let author = match body.budget_usd_daily {
        Some(cap) => {
            if let Some(refusal) = validate_cap(cap) {
                return Err(refusal);
            }
            Some(require_admin(&headers, &state, &company.runtime).await?)
        }
        None => None,
    };

    // Serialize per-company writes so concurrent console POST /team and
    // orchestrator add_agent calls can't clobber each other's overlay_agents.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    let agent = OverlayAgent {
        id: generate_id(),
        name: body.name,
        role: body.role,
        description: body.description,
    };
    record.overlay_agents.push(agent.clone());
    let attribution = author.map(|admin| BudgetOverride {
        agent_id: agent.id.clone(),
        budget_usd_daily: body.budget_usd_daily,
        set_by: Actor {
            kind: ActorKind::User,
            id: admin.user_id,
        },
        at_millis: now_millis(),
    });
    if let Some(entry) = attribution.clone() {
        // Through the upsert even though `agent.id` is freshly generated and so
        // cannot already hold a row: the "one override per teammate" invariant
        // belongs to the record, not to each call site's reasoning about id
        // uniqueness.
        record.upsert_budget_override(entry);
    }
    company
        .runtime
        .store()
        .save(&record)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    Ok(Json(TeamMemberDto {
        id: agent.id,
        name: Some(agent.name),
        role: agent.role,
        description: agent.description,
        // A brand-new teammate has no inbox until the toggle writes one.
        inbox_enabled: false,
        budget_usd_daily: body.budget_usd_daily,
        // Brand new, so nothing has been spent against the cap yet. Sent as
        // `0.0` rather than omitted so the card renders "$0.00 spent today"
        // beside a cap it was just given, instead of a cap with nothing next
        // to it.
        spent_today_usd: body.budget_usd_daily.map(|_| 0.0),
        budget_set_by: attribution.as_ref().map(|entry| entry.set_by.id.clone()),
        budget_set_at_millis: attribution.as_ref().map(|entry| entry.at_millis),
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
    // Drop the teammate's budget override with it (issue #343). Overlay ids are
    // generated, so a future teammate will not collide with this one — but a
    // record that accumulated dead override rows would grow without bound and
    // make the roster read scan entries for teammates that no longer exist.
    record.overlay_budgets.retain(|b| b.agent_id != agent_id);
    company.runtime.store().save(&record).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT {scope}/team/{agent_id}/budget` — set, change, or remove a teammate's
/// daily spend cap. Admin-only, attributed, and in force on the next dispatch.
///
/// See [`SetBudget`] for why `{}` is a `422` rather than an uncap.
async fn set_budget(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<SetBudget>,
) -> Result<Json<TeamMemberDto>, Response> {
    let admin = require_admin(&headers, &state, &company.runtime).await?;
    // `Some(_)` is guaranteed by `SetBudget`'s missing-key rejection; the inner
    // option is the cap-or-uncap the operator asked for.
    let cap = body.budget_usd_daily.flatten();
    if let Some(refusal) = cap.and_then(validate_cap) {
        return Err(refusal);
    }

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    if let Some(refusal) = require_roster_teammate(&record, &agent_id) {
        return Err(refusal);
    }

    let entry = BudgetOverride {
        agent_id: agent_id.clone(),
        budget_usd_daily: cap,
        set_by: Actor {
            kind: ActorKind::User,
            id: admin.user_id,
        },
        at_millis: now_millis(),
    };
    // One override per teammate: replace in place rather than accumulating, so
    // `effective_budget`'s first-match read can never see a stale row.
    record.upsert_budget_override(entry);
    company
        .runtime
        .store()
        .save(&record)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    updated_row(&company, &record, &agent_id).await
}

/// `DELETE {scope}/team/{agent_id}/budget` — drop the override so the manifest
/// default applies again.
///
/// Distinct from `PUT null`, and not expressible by it: `PUT null` stores "no
/// cap, decided by an admin", while this restores whatever `company.toml`
/// declares — which for a manifest-capped teammate means the cap comes **back**.
/// Deleting when nothing is stored is a no-op rather than a 404: the caller's
/// intent ("this teammate should follow the manifest") is already satisfied.
async fn clear_budget(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Result<Json<TeamMemberDto>, Response> {
    require_admin(&headers, &state, &company.runtime).await?;

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    if let Some(refusal) = require_roster_teammate(&record, &agent_id) {
        return Err(refusal);
    }

    record.overlay_budgets.retain(|b| b.agent_id != agent_id);
    company
        .runtime
        .store()
        .save(&record)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    updated_row(&company, &record, &agent_id).await
}

/// Rejects a cap that is not a spendable amount of money, mirroring the
/// manifest validation in `crate::company::manifest` so a value the console
/// accepts is one `company.toml` would have accepted too.
///
/// NaN and the infinities are refused as well as negatives. They parse as JSON
/// numbers in some encoders and would poison every comparison downstream: the
/// dispatch gate's `spent >= cap` is false for NaN, so a NaN cap would read as
/// "capped" everywhere in the console while enforcing nothing at all.
fn validate_cap(cap: f64) -> Option<Response> {
    if !cap.is_finite() {
        return Some(
            ApiError(OpenCompanyError::InvalidRequest(
                "a daily budget has to be a real number of dollars.".to_string(),
            ))
            .into_response(),
        );
    }
    if cap < 0.0 {
        return Some(
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "a daily budget cannot be negative — you sent `{cap}`."
            )))
            .into_response(),
        );
    }
    None
}

/// Loads the addressed company's record, or 404s.
async fn load_record(company: &ScopedCompany) -> Result<CompanyRecord, Response> {
    company
        .runtime
        .store()
        .load(company.id())
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(company.id().to_string())).into_response()
        })
}

/// 404s unless `agent_id` names a real roster teammate.
///
/// Without this an unknown id would store an override nothing ever reads —
/// a write that reports success and changes nothing, which is worse than a
/// refusal because the operator believes the cap is in place.
fn require_roster_teammate(record: &CompanyRecord, agent_id: &str) -> Option<Response> {
    if record.is_roster_agent(agent_id) {
        return None;
    }
    Some(
        ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        )))
        .into_response(),
    )
}

/// The teammate's roster row after a budget write, so the console can update the
/// card from the response instead of refetching the whole team.
async fn updated_row(
    company: &ScopedCompany,
    record: &CompanyRecord,
    agent_id: &str,
) -> Result<Json<TeamMemberDto>, Response> {
    let spend_today = daily_spend_samples(company, Some(record))
        .await
        .map_err(|e| e.into_response())?;
    let spent = |id: &str| {
        spend_today
            .as_ref()
            .map(|samples| crate::metering::usd_spent_by_agent(samples, id))
    };
    let inbox_enabled = company
        .runtime
        .inbox()
        .inboxes(company.id())
        .await
        .map_err(|e| ApiError(e).into_response())?
        .into_iter()
        .any(|meta| meta.key == agent_id && meta.enabled);

    // A manifest teammate is named by its role and carries no display name; an
    // overlay teammate always has one. Same rule as `list_team`.
    let overlay = record.overlay_agents.iter().find(|a| a.id == agent_id);
    let (name, role, description) = match overlay {
        Some(agent) => (
            Some(agent.name.clone()),
            agent.role.clone(),
            agent.description.clone(),
        ),
        None => {
            let agent = record
                .manifest
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .expect("roster membership was checked before the write");
            (None, agent.role.clone(), agent.description.clone())
        }
    };
    Ok(Json(member_row(
        record,
        agent_id,
        name,
        role,
        description,
        inbox_enabled,
        &spent,
    )))
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
    use serde_json::{Value, json};
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
                overlay_budgets: Vec::new(),
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

    /// Drives any team route with an explicit cookie, so the auth boundary can
    /// be exercised with an admin session, a member session, or none at all.
    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
        cookie: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let request = match &body {
            Some(value) => builder
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(value).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
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

    fn admin_cookie() -> String {
        crate::server::test_support::fixed_cookie("acme")
    }

    /// `PUT …/team/{id}/budget` as the seeded admin.
    async fn put_budget(state: &AppState, agent: &str, body: Value) -> (StatusCode, Value) {
        send(
            state,
            "PUT",
            &format!("/api/v1/company/team/{agent}/budget"),
            Some(body),
            Some(&admin_cookie()),
        )
        .await
    }

    /// One roster row from `GET …/team`.
    async fn team_row(state: &AppState, agent: &str) -> Value {
        let (status, body) = get_team(state).await;
        assert_eq!(status, StatusCode::OK);
        body.as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == agent)
            .unwrap_or_else(|| panic!("no {agent} row in {body}"))
            .clone()
    }

    // --- Console budget writes (issue #343) ---------------------------------

    /// The acceptance criterion, on the wire: an admin sets, changes and clears
    /// a cap, and every state is visible on the next read.
    ///
    /// The `writer` starts **uncapped in the manifest**, which is the case the
    /// pre-#343 code could not express at all — there was no field to write.
    #[tokio::test]
    async fn an_admin_can_set_change_and_clear_a_cap() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        // Uncapped to begin with — no cap key, no attribution.
        let before = team_row(&state, "writer").await;
        assert!(before.get("budgetUsdDaily").is_none(), "{before}");
        assert!(before.get("budgetSetBy").is_none(), "{before}");

        // Set.
        let (status, row) = put_budget(&state, "writer", json!({"budgetUsdDaily": 12.5})).await;
        assert_eq!(status, StatusCode::OK, "{row}");
        assert_eq!(row["budgetUsdDaily"], 12.5, "{row}");
        let after_set = team_row(&state, "writer").await;
        assert_eq!(after_set["budgetUsdDaily"], 12.5, "{after_set}");
        assert!(
            after_set["budgetSetBy"].is_string(),
            "a set cap is attributable to the admin who set it: {after_set}"
        );
        assert!(
            after_set["budgetSetAtMillis"].as_u64().unwrap() > 0,
            "{after_set}"
        );

        // Change.
        let (status, _) = put_budget(&state, "writer", json!({"budgetUsdDaily": 3.0})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(team_row(&state, "writer").await["budgetUsdDaily"], 3.0);

        // Remove the cap (explicit null).
        let (status, row) = put_budget(&state, "writer", json!({"budgetUsdDaily": null})).await;
        assert_eq!(status, StatusCode::OK, "{row}");
        let uncapped = team_row(&state, "writer").await;
        assert!(
            uncapped.get("budgetUsdDaily").is_none(),
            "an uncapped teammate omits the cap key entirely: {uncapped}"
        );
        assert!(
            uncapped["budgetSetBy"].is_string(),
            "…but the attribution stays, so an operator can see that a human \
             uncapped this teammate rather than that nobody ever capped it: {uncapped}"
        );
    }

    /// A cap set from the console **wins over the manifest**, and `DELETE`
    /// puts the manifest back. This is the pair that makes the override a
    /// remedy rather than a second opinion.
    #[tokio::test]
    async fn an_override_beats_the_manifest_and_delete_restores_it() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        assert_eq!(team_row(&state, "analyst").await["budgetUsdDaily"], 5.0);

        let (status, _) = put_budget(&state, "analyst", json!({"budgetUsdDaily": 50.0})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            50.0,
            "the stored cap wins over the manifest's $5"
        );

        let (status, row) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/analyst/budget",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{row}");
        let reset = team_row(&state, "analyst").await;
        assert_eq!(
            reset["budgetUsdDaily"], 5.0,
            "DELETE drops the override, so the manifest default applies again: {reset}"
        );
        assert!(
            reset.get("budgetSetBy").is_none(),
            "with no override there is nothing to attribute: {reset}"
        );
    }

    /// The issue's third rule, pinned **on the wire** rather than in Rust: `0`
    /// and `null` are different bodies with different stored outcomes.
    ///
    /// `0` caps the teammate at nothing (the cap key comes back as `0.0`);
    /// `null` removes the cap (the key is absent). If these ever collapsed, an
    /// operator lifting a cap would instead have silenced the teammate.
    #[tokio::test]
    async fn zero_and_null_are_different_states() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = put_budget(&state, "analyst", json!({"budgetUsdDaily": 0})).await;
        assert_eq!(status, StatusCode::OK);
        let zeroed = team_row(&state, "analyst").await;
        assert_eq!(
            zeroed["budgetUsdDaily"], 0.0,
            "a zero cap is sent as 0, not omitted: {zeroed}"
        );

        let (status, _) = put_budget(&state, "analyst", json!({"budgetUsdDaily": null})).await;
        assert_eq!(status, StatusCode::OK);
        let cleared = team_row(&state, "analyst").await;
        assert!(
            cleared.get("budgetUsdDaily").is_none(),
            "a cleared cap omits the key — and beats the manifest's $5: {cleared}"
        );
    }

    /// An omitted key is **not** an uncap. `{}` cannot be mistaken for
    /// `{"budgetUsdDaily": null}`, so a client bug or a truncated body can never
    /// silently lift a cap; axum rejects the body before the handler runs.
    #[tokio::test]
    async fn an_absent_key_is_rejected_rather_than_read_as_uncapped() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = put_budget(&state, "analyst", json!({})).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an empty body must never be read as 'remove the cap'"
        );
        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            5.0,
            "and nothing was written"
        );
    }

    /// A cap has to be a real, non-negative number of dollars — the same rule
    /// the manifest validator applies, so the console cannot store a value
    /// `company.toml` would have rejected.
    #[tokio::test]
    async fn a_nonsensical_cap_is_refused() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, body) = put_budget(&state, "analyst", json!({"budgetUsdDaily": -1.0})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // NaN and ∞ have no JSON literal, so they arrive as raw tokens. Either
        // outcome is a refusal; what must never happen is one being stored,
        // because `spent >= NaN` is false and the cap would enforce nothing
        // while the console rendered it as set.
        for raw in ["{\"budgetUsdDaily\": NaN}", "{\"budgetUsdDaily\": 1e400}"] {
            let request = Request::builder()
                .method("PUT")
                .uri("/api/v1/company/team/analyst/budget")
                .header("cookie", admin_cookie())
                .header("content-type", "application/json")
                .body(Body::from(raw))
                .unwrap();
            let status = router(state.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status();
            assert!(status.is_client_error(), "{raw} → {status}");
        }

        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            5.0,
            "no refused write left anything behind"
        );
    }

    /// An unknown teammate 404s rather than storing an override nothing reads.
    #[tokio::test]
    async fn an_unknown_teammate_is_not_found() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = put_budget(&state, "nobody", json!({"budgetUsdDaily": 1.0})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(
            &state,
            "DELETE",
            "/api/v1/company/team/nobody/budget",
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The privilege boundary: a signed-in **member** cannot change a cap, and
    /// an unauthenticated caller cannot reach the route at all.
    ///
    /// "A cap that can be raised silently is not much of a cap" — so this is the
    /// assertion that makes the enforcement worth having. It is checked on the
    /// backend, never on the console's hidden buttons.
    #[tokio::test]
    async fn a_non_admin_cannot_change_a_cap() {
        use crate::ports::UserRole;

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        for (method, body) in [
            ("PUT", Some(json!({"budgetUsdDaily": 999.0}))),
            ("DELETE", None),
        ] {
            let (status, _) = send(
                &state,
                method,
                "/api/v1/company/team/analyst/budget",
                body.clone(),
                Some(&member),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} as a member must be refused"
            );

            let (status, _) = send(
                &state,
                method,
                "/api/v1/company/team/analyst/budget",
                body,
                None,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} with no session must be refused"
            );
        }

        assert_eq!(
            team_row(&state, "analyst").await["budgetUsdDaily"],
            5.0,
            "the manifest cap is untouched"
        );
    }

    /// A teammate created through the console can be given a cap at creation —
    /// and that is admin-only, while a budget-less add stays open to any member
    /// exactly as it was before #343.
    #[tokio::test]
    async fn a_new_teammate_can_be_created_with_a_cap() {
        use crate::ports::UserRole;

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let member =
            crate::server::test_support::seed_session(&state, "acme", UserRole::Member).await;

        // A member may still add a teammate — no permission was taken away.
        let (status, plain) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Growth"})),
            Some(&member),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plain}");
        assert!(plain.get("budgetUsdDaily").is_none(), "{plain}");

        // …but not with a budget attached.
        let (status, _) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Sam", "role": "Ops", "budgetUsdDaily": 4.0})),
            Some(&member),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "setting a cap is admin-only wherever it happens"
        );

        // An admin can.
        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Sam", "role": "Ops", "budgetUsdDaily": 4.0})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["budgetUsdDaily"], 4.0, "{created}");
        let sam = created["id"].as_str().unwrap().to_string();
        let row = team_row(&state, &sam).await;
        assert_eq!(
            row["budgetUsdDaily"], 4.0,
            "the cap and the teammate landed in one save: {row}"
        );
        assert!(row["budgetSetBy"].is_string(), "{row}");
    }

    /// An **overlay** teammate can be capped after the fact too — the case the
    /// pre-#343 read path hardcoded to `None` ("uncapped in v1").
    ///
    /// Capping it also has to start the spend read for the whole roster, which
    /// the old `any_capped` scan (manifest agents only) would have missed on a
    /// company whose only capped teammate came from the console.
    #[tokio::test]
    async fn an_overlay_teammate_can_be_capped_and_reports_its_spend() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n",
        )
        .await;

        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Growth"})),
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let jamie = created["id"].as_str().unwrap().to_string();

        let id = CompanyId::new("acme");
        let runtime = state.registry().get(&id).unwrap();
        runtime
            .usage()
            .record(
                &id,
                &UsageSample {
                    at_millis: crate::ports::now_millis(),
                    agent: jamie.clone(),
                    provider: "managed".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cost_usd: 0.75,
                    kind: SampleKind::Inference,
                    run_id: None,
                },
            )
            .await
            .unwrap();

        let (status, _) = put_budget(&state, &jamie, json!({"budgetUsdDaily": 2.0})).await;
        assert_eq!(status, StatusCode::OK);

        let row = team_row(&state, &jamie).await;
        assert_eq!(row["budgetUsdDaily"], 2.0, "{row}");
        assert!(
            (row["spentTodayUsd"].as_f64().unwrap() - 0.75).abs() < 1e-9,
            "capping the only console-added teammate must start the meter read \
             for the roster: {row}"
        );
    }

    /// Removing a teammate takes its override with it, so the record does not
    /// accumulate rows for teammates that no longer exist.
    #[tokio::test]
    async fn removing_a_teammate_drops_its_budget_override() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (_, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Jamie", "role": "Growth", "budgetUsdDaily": 2.0})),
            Some(&admin_cookie()),
        )
        .await;
        let jamie = created["id"].as_str().unwrap().to_string();

        let (status, _) = send(
            &state,
            "DELETE",
            &format!("/api/v1/company/team/{jamie}"),
            None,
            Some(&admin_cookie()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let record = state
            .registry()
            .get(&CompanyId::new("acme"))
            .unwrap()
            .store()
            .load(&CompanyId::new("acme"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            record.overlay_budgets.is_empty(),
            "the removed teammate's override went with it: {:?}",
            record.overlay_budgets
        );
    }

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
