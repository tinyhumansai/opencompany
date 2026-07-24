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
/// `resolve_team` merge (minus its `inbox_enabled` field, which has no REST
/// consumer yet). Hosts with no persisted record yet return an empty roster,
/// the same soft-fail the sibling `/desks` route uses, rather than 404ing.
async fn list_team(company: ScopedCompany) -> Result<Json<Vec<TeamMemberDto>>, ApiError> {
    let record = company.runtime.store().load(company.id()).await?;
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
                })
                .collect();
            members.extend(record.overlay_agents.iter().map(|agent| TeamMemberDto {
                id: agent.id.clone(),
                name: Some(agent.name.clone()),
                role: agent.role.clone(),
                description: agent.description.clone(),
            }));
            members
        })
        .unwrap_or_default();
    Ok(Json(members))
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
